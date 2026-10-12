//! Zerlegung der Terminal-Ausgabe von arch-update/pacman.
//!
//! Die Ausgabe kommt aus einem PTY und enthält ANSI-Sequenzen, `\r`-Updates
//! von Fortschrittsbalken und unvollständige Zeilen (Eingabeaufforderungen).
//! [`OutputParser`] arbeitet zustandsbehaftet, damit über Chunk-Grenzen
//! geteilte Escape-Sequenzen korrekt entfernt werden.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscState {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// Wandelt rohe PTY-Ausgabe in vollständige Zeilen um.
#[derive(Debug)]
pub struct OutputParser {
    state: EscState,
    current: String,
    carriage_return: bool,
}

impl Default for OutputParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputParser {
    pub fn new() -> Self {
        Self {
            state: EscState::Normal,
            current: String::new(),
            carriage_return: false,
        }
    }

    /// Verarbeitet einen Chunk und liefert alle darin abgeschlossenen Zeilen.
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for c in chunk.chars() {
            match self.state {
                EscState::Normal => self.normal(c, &mut lines),
                EscState::Esc => {
                    self.state = match c {
                        '[' => EscState::Csi,
                        ']' => EscState::Osc,
                        // Zwei-Zeichen-Sequenz (z. B. ESC 7, ESC =) ist damit beendet.
                        _ => EscState::Normal,
                    }
                }
                EscState::Csi => {
                    if ('@'..='~').contains(&c) {
                        self.state = EscState::Normal;
                    }
                }
                EscState::Osc => match c {
                    '\u{7}' => self.state = EscState::Normal,
                    '\u{1b}' => self.state = EscState::OscEsc,
                    _ => {}
                },
                EscState::OscEsc => self.state = EscState::Normal,
            }
        }
        lines
    }

    fn normal(&mut self, c: char, lines: &mut Vec<String>) {
        match c {
            '\u{1b}' => self.state = EscState::Esc,
            '\n' => {
                self.carriage_return = false;
                lines.push(std::mem::take(&mut self.current).trim_end().to_string());
            }
            '\r' => self.carriage_return = true,
            '\u{8}' => {
                self.current.pop();
            }
            '\t' => self.push_char(' '),
            c if c.is_control() => {}
            c => self.push_char(c),
        }
    }

    fn push_char(&mut self, c: char) {
        if self.carriage_return {
            // `\r` ohne `\n`: Zeile wird überschrieben (Fortschrittsbalken).
            self.current.clear();
            self.carriage_return = false;
        }
        self.current.push(c);
    }

    /// Aktuelle, noch nicht abgeschlossene Zeile (Fortschritt oder Eingabeaufforderung).
    pub fn partial(&self) -> &str {
        &self.current
    }
}

/// Entfernt ANSI-Sequenzen aus einem vollständigen Text (zustandslos).
pub fn strip_ansi(text: &str) -> String {
    let mut p = OutputParser::new();
    let mut lines = p.feed(text);
    if !p.partial().is_empty() {
        lines.push(p.partial().to_string());
    }
    lines.join("\n")
}

static COUNTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\(\s*(\d+)/(\d+)\)\s+(.*)$").unwrap());
static TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Total\s*\(\s*(\d+)/(\d+)\)").unwrap());
static BAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[#\-co ]*\]\s*\d{1,3}%\s*$").unwrap());
static PERCENT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,3})%\s*$").unwrap());

/// Zähler der Form `( 3/45) upgrading linux` aus pacman-Ausgaben.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    pub current: u32,
    pub total: u32,
    /// Text nach dem Zähler, ohne Fortschrittsbalken.
    pub action: String,
}

pub fn parse_counter(line: &str) -> Option<Counter> {
    let caps = COUNTER_RE.captures(line.trim_start())?;
    let current = caps[1].parse().ok()?;
    let total = caps[2].parse().ok()?;
    let rest = caps[3].to_string();
    let action = match rest.find('[') {
        Some(i) if BAR_RE.is_match(&rest) => rest[..i].trim_end().to_string(),
        _ => rest.trim_end().to_string(),
    };
    Some(Counter {
        current,
        total,
        action,
    })
}

/// `Total ( 3/45)` der pacman-Downloadanzeige.
pub fn parse_download_total(line: &str) -> Option<(u32, u32)> {
    let caps = TOTAL_RE.captures(line)?;
    Some((caps[1].parse().ok()?, caps[2].parse().ok()?))
}

/// Prozentwert am Zeilenende eines Fortschrittsbalkens.
pub fn trailing_percent(line: &str) -> Option<u8> {
    if !BAR_RE.is_match(line) && !line.contains("MiB") && !line.contains("KiB") {
        return None;
    }
    let p: u32 = PERCENT_RE.captures(line)?[1].parse().ok()?;
    Some(p.min(100) as u8)
}

/// `true` für reine Fortschrittsbalken-Zeilen, die nicht ins Protokoll sollen
/// (außer der Abschlusszeile mit 100 %).
pub fn is_transient_progress_line(line: &str) -> bool {
    BAR_RE.is_match(line) && trailing_percent(line).is_some_and(|p| p < 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors_and_splits_lines() {
        let mut p = OutputParser::new();
        let lines =
            p.feed("\x1b[1m\x1b[34m==>\x1b[0m\x1b[1m Looking for updates...\x1b[0m\n\nnext");
        assert_eq!(lines, vec!["==> Looking for updates...", ""]);
        assert_eq!(p.partial(), "next");
    }

    #[test]
    fn escape_sequences_split_across_chunks() {
        let mut p = OutputParser::new();
        assert!(p.feed("abc\x1b[").is_empty());
        assert!(p.feed("1;3").is_empty());
        let lines = p.feed("2mdef\n");
        assert_eq!(lines, vec!["abcdef"]);
    }

    #[test]
    fn carriage_return_overwrites_progress() {
        let mut p = OutputParser::new();
        p.feed("( 1/2) upgrading a  [###---]  50%\r( 1/2) upgrading a  [######] 100%");
        assert_eq!(p.partial(), "( 1/2) upgrading a  [######] 100%");
        let lines = p.feed("\r\n");
        assert_eq!(lines, vec!["( 1/2) upgrading a  [######] 100%"]);
    }

    #[test]
    fn osc_title_and_backspace() {
        let mut p = OutputParser::new();
        let lines = p.feed("\x1b]0;title\x07ab\x08c\n");
        assert_eq!(lines, vec!["ac"]);
        assert_eq!(strip_ansi("\x1b[31mrot\x1b[0m"), "rot");
    }

    #[test]
    fn counters_and_percent() {
        let c = parse_counter("( 3/45) upgrading linux            [##########] 100%").unwrap();
        assert_eq!((c.current, c.total), (3, 45));
        assert_eq!(c.action, "upgrading linux");
        let c = parse_counter("(12/12) checking keys in keyring").unwrap();
        assert_eq!(c.action, "checking keys in keyring");
        assert!(parse_counter("upgrading x").is_none());
        assert_eq!(
            parse_download_total(" Total ( 3/12)      120.5 MiB  8.00 MiB/s 00:11 [####---]  27%"),
            Some((3, 12))
        );
        assert_eq!(
            trailing_percent(" linux-6.17  120.5 MiB  8.00 MiB/s 00:11 [####---]  27%"),
            Some(27)
        );
        assert!(is_transient_progress_line("x [##--]  40%"));
        assert!(!is_transient_progress_line("x [####] 100%"));
        assert!(!is_transient_progress_line("Proceed with update? [Y/n]"));
    }
}
