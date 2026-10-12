//! Auswertung der Statusdateien von arch-update (`$XDG_STATE_HOME/arch-update/`).
//!
//! Formate laut `lib/check.sh` und `lib/list_packages.sh`:
//! * `last_updates_check_packages` / `_aur`: `name alt -> neu` je Zeile
//!   (bei `NoVersion` nur `name`), ANSI-Farben bereits entfernt.
//! * `last_updates_check_flatpak`: `Anzeigename Version` bzw. nur `Anzeigename`.
//!   Da Anzeigenamen Leerzeichen enthalten können, wird die Zeile unverändert übernommen.
//! * `last_updates_check_time`: Zeile 1 Unix-Zeit, Zeile 2 `%F %T`.
//! * `last_update_run`: `%Y-%m-%d` des letzten bestätigten Update-Laufs.

use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Local, NaiveDate, TimeZone};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdateSource {
    Pacman,
    Aur,
    Flatpak,
}

impl UpdateSource {
    pub fn label(self) -> &'static str {
        match self {
            UpdateSource::Pacman => "Paket",
            UpdateSource::Aur => "AUR",
            UpdateSource::Flatpak => "Flatpak",
        }
    }
}

/// Ein ausstehendes Update, wie es arch-update ermittelt hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    pub source: UpdateSource,
    pub name: String,
    pub current_version: Option<String>,
    pub new_version: Option<String>,
}

/// Parst Zeilen im Format von `checkupdates` bzw. `<aur-helper> -Qua`.
pub fn parse_package_lines(text: &str, source: UpdateSource) -> Vec<PendingUpdate> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.as_slice() {
                [name, old, "->", new, ..] => PendingUpdate {
                    source,
                    name: (*name).to_string(),
                    current_version: Some((*old).to_string()),
                    new_version: Some((*new).to_string()),
                },
                [name] => PendingUpdate {
                    source,
                    name: (*name).to_string(),
                    current_version: None,
                    new_version: None,
                },
                // Unbekanntes Format: Zeile unverändert anzeigen, nichts erfinden.
                _ => PendingUpdate {
                    source,
                    name: line.to_string(),
                    current_version: None,
                    new_version: None,
                },
            }
        })
        .collect()
}

pub fn parse_flatpak_lines(text: &str) -> Vec<PendingUpdate> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| PendingUpdate {
            source: UpdateSource::Flatpak,
            name: line.to_string(),
            current_version: None,
            new_version: None,
        })
        .collect()
}

/// Liest den Zeitpunkt der letzten Prüfung (`last_updates_check_time`).
pub fn parse_check_time(text: &str) -> Option<DateTime<Local>> {
    let epoch: i64 = text.lines().next()?.trim().parse().ok()?;
    Local.timestamp_opt(epoch, 0).single()
}

pub fn parse_update_run(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

/// Inhalt des State-Verzeichnisses. Fehlende Dateien bleiben `None`/leer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckState {
    pub pending: Vec<PendingUpdate>,
    /// Zeitpunkt der letzten Prüfung durch arch-update (Tray, Timer, GUI oder CLI).
    pub last_check: Option<DateTime<Local>>,
    /// Datum des letzten bestätigten Update-Laufs (`full_upgrade.sh`).
    pub last_update_run: Option<NaiveDate>,
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn read_check_state(state_dir: &Path) -> io::Result<CheckState> {
    let read = |name: &str| read_optional(&state_dir.join(name));
    let mut pending = Vec::new();
    if let Some(t) = read("last_updates_check_packages")? {
        pending.extend(parse_package_lines(&t, UpdateSource::Pacman));
    }
    if let Some(t) = read("last_updates_check_aur")? {
        pending.extend(parse_package_lines(&t, UpdateSource::Aur));
    }
    if let Some(t) = read("last_updates_check_flatpak")? {
        pending.extend(parse_flatpak_lines(&t));
    }
    Ok(CheckState {
        pending,
        last_check: read("last_updates_check_time")?.and_then(|t| parse_check_time(&t)),
        last_update_run: read("last_update_run")?.and_then(|t| parse_update_run(&t)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checkupdates_lines() {
        let v = parse_package_lines(
            "linux 6.17.1-1 -> 6.17.2-1\n\nmesa 1:25.1-1 -> 1:25.2-1\nfoo\n",
            UpdateSource::Pacman,
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "linux");
        assert_eq!(v[0].current_version.as_deref(), Some("6.17.1-1"));
        assert_eq!(v[1].new_version.as_deref(), Some("1:25.2-1"));
        assert_eq!(v[2].name, "foo");
        assert_eq!(v[2].new_version, None);
    }

    #[test]
    fn flatpak_lines_are_kept_verbatim() {
        let v = parse_flatpak_lines("GNU Image Manipulation Program 3.0.4\n");
        assert_eq!(v[0].name, "GNU Image Manipulation Program 3.0.4");
        assert_eq!(v[0].source, UpdateSource::Flatpak);
    }

    #[test]
    fn parses_time_files() {
        let t = parse_check_time("1791795433\n2026-10-12 10:57:13\n").unwrap();
        assert_eq!(t.timestamp(), 1791795433);
        assert!(parse_check_time("").is_none());
        assert!(parse_check_time("abc").is_none());
        assert_eq!(
            parse_update_run("2026-10-12\n"),
            NaiveDate::from_ymd_opt(2026, 10, 12)
        );
    }

    #[test]
    fn missing_state_dir_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = read_check_state(&dir.path().join("nope")).unwrap();
        assert_eq!(s, CheckState::default());
    }
}
