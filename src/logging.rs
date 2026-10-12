//! Zentrales Logging.
//!
//! Alle Meldungen laufen über `tracing`. Ein eigener Layer schreibt sie in
//! einen begrenzten Ringpuffer ([`LogStore`]), aus dem die Log-Ansicht liest,
//! und optional in eine Datei unter `$XDG_STATE_HOME/cashy-updater-gui/logs/`.
//! Die Ausgabe von arch-update wird mit dem Target `arch-update` protokolliert.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Local};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// Target für Zeilen, die von arch-update/pacman stammen.
pub const ARCH_UPDATE_TARGET: &str = "arch-update";
/// Ab dieser Größe wird die Logdatei beim Start rotiert.
const MAX_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Debug => "Debug",
            LogLevel::Info => "Info",
            LogLevel::Warn => "Warnung",
            LogLevel::Error => "Fehler",
        }
    }

    fn file_tag(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO ",
            LogLevel::Warn => "WARN ",
            LogLevel::Error => "ERROR",
        }
    }

    fn from_tracing(level: &Level) -> Self {
        match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            _ => LogLevel::Debug,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp: DateTime<Local>,
    pub level: LogLevel,
    /// "app" für Meldungen der GUI, "arch-update" für Prozessausgaben.
    pub source: String,
    pub message: String,
}

impl LogEntry {
    pub fn time_text(&self) -> String {
        self.timestamp.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Einzeilige Textdarstellung (Export, Zwischenablage, Datei).
    pub fn format_line(&self) -> String {
        format!(
            "{} [{}] {}: {}",
            self.time_text(),
            self.level.file_tag(),
            self.source,
            self.message
        )
    }
}

struct Inner {
    entries: VecDeque<LogEntry>,
    capacity: usize,
    next_id: u64,
    file: Option<BufWriter<File>>,
    file_path: Option<PathBuf>,
}

/// Thread-sicherer, größenbegrenzter Log-Puffer.
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Mutex<Inner>>,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: VecDeque::new(),
                capacity: capacity.max(1),
                next_id: 1,
                file: None,
                file_path: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Ein Panic in einem anderen Thread darf das Logging nicht lahmlegen.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Öffnet (bzw. rotiert) die Logdatei im angegebenen Verzeichnis.
    pub fn attach_file(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join("cashy-updater-gui.log");
        if fs::metadata(&path)
            .map(|m| m.len() > MAX_LOG_FILE_BYTES)
            .unwrap_or(false)
        {
            fs::rename(&path, dir.join("cashy-updater-gui.log.1"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut inner = self.lock();
        inner.file = Some(BufWriter::new(file));
        inner.file_path = Some(path.clone());
        Ok(path)
    }

    pub fn detach_file(&self) {
        let mut inner = self.lock();
        if let Some(mut f) = inner.file.take() {
            let _ = f.flush();
        }
        inner.file_path = None;
    }

    pub fn file_path(&self) -> Option<PathBuf> {
        self.lock().file_path.clone()
    }

    pub fn push(&self, level: LogLevel, source: &str, message: &str) {
        let mut inner = self.lock();
        let entry = LogEntry {
            id: inner.next_id,
            timestamp: Local::now(),
            level,
            source: source.to_string(),
            message: message.to_string(),
        };
        inner.next_id += 1;
        if let Some(file) = inner.file.as_mut() {
            // Schreibfehler der Logdatei dürfen die Anwendung nicht stören.
            let ok = writeln!(file, "{}", entry.format_line()).and_then(|_| file.flush());
            if ok.is_err() {
                inner.file = None;
            }
        }
        inner.entries.push_back(entry);
        while inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
        }
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.lock();
        inner.capacity = capacity.max(1);
        while inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
        }
    }

    /// ID, die der nächste Eintrag erhalten wird (für Änderungserkennung).
    pub fn next_id(&self) -> u64 {
        self.lock().next_id
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.lock().entries.iter().cloned().collect()
    }

    /// Liefert die letzten `limit` passenden Einträge (ab `min_id`) und die
    /// Gesamtzahl passender Einträge. Klont nur die zurückgegebenen Einträge,
    /// damit große Puffer die GUI nicht ausbremsen.
    pub fn query(&self, filter: &LogFilter, min_id: u64, limit: usize) -> (Vec<LogEntry>, usize) {
        let inner = self.lock();
        let mut total = 0;
        let mut out = Vec::new();
        for e in inner.entries.iter().rev() {
            if e.id < min_id {
                break;
            }
            if filter.matches(e) {
                total += 1;
                if out.len() < limit {
                    out.push(e.clone());
                }
            }
        }
        out.reverse();
        (out, total)
    }

    /// Einträge mit den angegebenen IDs (in Originalreihenfolge).
    pub fn by_ids(&self, ids: &std::collections::HashSet<u64>) -> Vec<LogEntry> {
        self.lock()
            .entries
            .iter()
            .filter(|e| ids.contains(&e.id))
            .cloned()
            .collect()
    }
}

/// Filter der Log-Ansicht.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    pub show_debug: bool,
    pub show_info: bool,
    pub show_warn: bool,
    pub show_error: bool,
    /// Volltextsuche (Groß-/Kleinschreibung wird ignoriert).
    pub query: String,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            show_debug: false,
            show_info: true,
            show_warn: true,
            show_error: true,
            query: String::new(),
        }
    }
}

impl LogFilter {
    pub fn matches(&self, entry: &LogEntry) -> bool {
        let level_ok = match entry.level {
            LogLevel::Debug => self.show_debug,
            LogLevel::Info => self.show_info,
            LogLevel::Warn => self.show_warn,
            LogLevel::Error => self.show_error,
        };
        if !level_ok {
            return false;
        }
        let q = self.query.trim();
        if q.is_empty() {
            return true;
        }
        let q = q.to_lowercase();
        entry.message.to_lowercase().contains(&q) || entry.source.to_lowercase().contains(&q)
    }

    pub fn apply<'a>(&self, entries: &'a [LogEntry]) -> Vec<&'a LogEntry> {
        entries.iter().filter(|e| self.matches(e)).collect()
    }
}

/// Formatiert Einträge als Text (eine Zeile pro Eintrag).
pub fn format_entries<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> String {
    let mut out = String::new();
    for e in entries {
        let _ = writeln!(out, "{}", e.format_line());
    }
    out
}

/// Exportiert Einträge in eine neue Datei im angegebenen Verzeichnis.
pub fn export_entries<'a>(
    entries: impl IntoIterator<Item = &'a LogEntry>,
    dir: &Path,
) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let name = format!(
        "cashy-updater-log-{}.txt",
        Local::now().format("%Y%m%d-%H%M%S")
    );
    let mut path = dir.join(&name);
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{}-{n}.txt", name.trim_end_matches(".txt")));
        n += 1;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(format_entries(entries).as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

/// Bestimmt die Log-Stufe einer Ausgabezeile von arch-update/pacman (C-Locale).
pub fn level_for_process_line(line: &str) -> LogLevel {
    let t = line.trim_start();
    let lower = t.to_lowercase();
    if lower.starts_with("==> error")
        || lower.starts_with("error:")
        || (lower.starts_with("sudo: ") && lower.contains("incorrect"))
    {
        LogLevel::Error
    } else if lower.starts_with("==> warning") || lower.starts_with("warning:") {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

/// `tracing`-Layer, der Ereignisse in den [`LogStore`] schreibt.
pub struct StoreLayer {
    store: LogStore,
    min_level: LogLevel,
}

impl StoreLayer {
    pub fn new(store: LogStore, min_level: LogLevel) -> Self {
        Self { store, min_level }
    }
}

impl<S: Subscriber> Layer<S> for StoreLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = LogLevel::from_tracing(meta.level());
        if level < self.min_level {
            return;
        }
        let target = meta.target();
        let own = target.starts_with("cashy_updater_gui");
        if !own && target != ARCH_UPDATE_TARGET && level < LogLevel::Warn {
            // Fremde Crates nur ab Warnung übernehmen.
            return;
        }
        let mut v = MessageVisitor {
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut v);
        let source = if target == ARCH_UPDATE_TARGET {
            ARCH_UPDATE_TARGET
        } else if own {
            "app"
        } else {
            target
        };
        self.store
            .push(level, source, &format!("{}{}", v.message, v.fields));
    }
}

/// Initialisiert den globalen Subscriber. Darf nur einmal aufgerufen werden.
pub fn init(store: &LogStore, debug: bool) -> Result<(), String> {
    let min = if debug {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let stderr_level = if debug { Level::DEBUG } else { Level::WARN };
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_target(true)
        .with_filter(tracing_subscriber::filter::LevelFilter::from_level(
            stderr_level,
        ));
    tracing_subscriber::registry()
        .with(StoreLayer::new(store.clone(), min))
        .with(stderr)
        .try_init()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_drops_oldest() {
        let s = LogStore::new(3);
        for i in 0..5 {
            s.push(LogLevel::Info, "app", &format!("m{i}"));
        }
        let snap = s.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "m2");
        assert_eq!(snap[2].id, 5);
        assert_eq!(s.next_id(), 6);
        s.set_capacity(1);
        assert_eq!(s.snapshot()[0].message, "m4");
    }

    #[test]
    fn filter_by_level_and_query() {
        let s = LogStore::new(10);
        s.push(LogLevel::Debug, "app", "debug detail");
        s.push(LogLevel::Info, "arch-update", "upgrading linux");
        s.push(LogLevel::Error, "arch-update", "error: failed");
        let snap = s.snapshot();
        let f = LogFilter::default();
        assert_eq!(f.apply(&snap).len(), 2);
        let f = LogFilter {
            query: "LINUX".into(),
            ..LogFilter::default()
        };
        assert_eq!(f.apply(&snap).len(), 1);
        let f = LogFilter {
            show_info: false,
            show_warn: false,
            ..LogFilter::default()
        };
        assert_eq!(f.apply(&snap)[0].level, LogLevel::Error);
        let f = LogFilter {
            query: "arch-update".into(),
            ..LogFilter::default()
        };
        assert_eq!(f.apply(&snap).len(), 2, "Suche berücksichtigt die Quelle");
    }

    #[test]
    fn query_returns_tail_and_total() {
        let s = LogStore::new(100);
        for i in 0..10 {
            let level = if i % 2 == 0 {
                LogLevel::Info
            } else {
                LogLevel::Error
            };
            s.push(level, "app", &format!("m{i}"));
        }
        let (tail, total) = s.query(&LogFilter::default(), 0, 3);
        assert_eq!(total, 10);
        assert_eq!(
            tail.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
            ["m7", "m8", "m9"]
        );
        let only_errors = LogFilter {
            show_info: false,
            ..LogFilter::default()
        };
        let (tail, total) = s.query(&only_errors, 0, 100);
        assert_eq!(total, 5);
        assert!(tail.iter().all(|e| e.level == LogLevel::Error));
        // min_id blendet ältere Einträge aus („Anzeige leeren“).
        let (_, total) = s.query(&LogFilter::default(), 9, 100);
        assert_eq!(total, 2);
        let ids = [2u64, 4].into_iter().collect();
        assert_eq!(s.by_ids(&ids).len(), 2);
    }

    #[test]
    fn process_line_levels() {
        assert_eq!(level_for_process_line("==> ERROR: boom"), LogLevel::Error);
        assert_eq!(
            level_for_process_line("error: failed to commit"),
            LogLevel::Error
        );
        assert_eq!(
            level_for_process_line("==> WARNING: careful"),
            LogLevel::Warn
        );
        assert_eq!(level_for_process_line("warning: x"), LogLevel::Warn);
        assert_eq!(level_for_process_line("(1/2) upgrading a"), LogLevel::Info);
    }
}
