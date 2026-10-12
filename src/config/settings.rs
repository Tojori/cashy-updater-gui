//! Eigene Einstellungen der GUI (`$XDG_CONFIG_HOME/cashy-updater-gui/settings.toml`).
//!
//! Enthält ausschließlich Optionen, die die GUI selbst umsetzt. Optionen von
//! arch-update werden in dessen eigener Datei verwaltet (siehe `arch_update_conf`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, ErrorKind};

pub const MIN_LOG_ENTRIES: usize = 200;
pub const MAX_LOG_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme: Theme::Dark }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Behavior {
    /// Beim Start `arch-update --check` ausführen (Netzwerkzugriff).
    pub check_on_startup: bool,
    /// Nach einem Update-Lauf den Status (lokale Dateien) neu einlesen.
    pub refresh_after_upgrade: bool,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            check_on_startup: false,
            refresh_after_upgrade: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Logging {
    /// Maximale Anzahl Einträge im Speicher der Log-Ansicht.
    pub max_entries: usize,
    /// Debug-Meldungen erfassen (wirkt nach Neustart).
    pub debug: bool,
    /// Protokoll zusätzlich in eine Datei schreiben.
    pub write_file: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            max_entries: 5_000,
            debug: false,
            write_file: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Upgrade {
    /// arch-update mit `--devel` starten (AUR-Entwicklungspakete einbeziehen).
    pub include_devel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub behavior: Behavior,
    pub logging: Logging,
    pub upgrade: Upgrade,
}

impl Settings {
    /// Korrigiert ungültige Werte und liefert Hinweise zu jeder Korrektur.
    pub fn validate(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let n = self.logging.max_entries;
        let clamped = n.clamp(MIN_LOG_ENTRIES, MAX_LOG_ENTRIES);
        if clamped != n {
            notes.push(format!(
                "logging.max_entries={n} liegt außerhalb von {MIN_LOG_ENTRIES}..{MAX_LOG_ENTRIES} und wurde auf {clamped} gesetzt."
            ));
            self.logging.max_entries = clamped;
        }
        notes
    }
}

/// Ergebnis des Ladens der Einstellungen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    Loaded {
        settings: Settings,
        notes: Vec<String>,
    },
    /// Datei existiert nicht; Standardwerte werden verwendet.
    Missing(Settings),
    /// Datei war nicht lesbar; sie wurde (falls möglich) gesichert, Standardwerte werden verwendet.
    Corrupt {
        settings: Settings,
        error: String,
        backup: Option<PathBuf>,
    },
}

impl LoadOutcome {
    pub fn settings(&self) -> &Settings {
        match self {
            LoadOutcome::Loaded { settings, .. }
            | LoadOutcome::Missing(settings)
            | LoadOutcome::Corrupt { settings, .. } => settings,
        }
    }
}

/// Lädt Einstellungen. Stürzt niemals ab: Fehler führen zu Standardwerten
/// und einer nachvollziehbaren Meldung; eine defekte Datei wird gesichert.
pub fn load(path: &Path) -> LoadOutcome {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LoadOutcome::Missing(Settings::default());
        }
        Err(e) => {
            return LoadOutcome::Corrupt {
                settings: Settings::default(),
                error: format!("Datei nicht lesbar: {e}"),
                backup: None,
            };
        }
    };
    match toml::from_str::<Settings>(&text) {
        Ok(mut settings) => {
            let notes = settings.validate();
            LoadOutcome::Loaded { settings, notes }
        }
        Err(e) => {
            let backup = path.with_extension(format!(
                "toml.defekt-{}",
                Local::now().format("%Y%m%d-%H%M%S")
            ));
            let backup = fs::rename(path, &backup).ok().map(|_| backup);
            LoadOutcome::Corrupt {
                settings: Settings::default(),
                error: e.to_string(),
                backup,
            }
        }
    }
}

/// Speichert Einstellungen atomar (temporäre Datei + rename).
pub fn save(path: &Path, settings: &Settings) -> Result<(), AppError> {
    let text = toml::to_string_pretty(settings)
        .map_err(|e| AppError::internal("Einstellungen konnten nicht serialisiert werden.", e))?;
    write_atomic(path, text.as_bytes()).map_err(|e| {
        AppError::new(
            ErrorKind::Internal,
            format!(
                "Einstellungen konnten nicht nach {} geschrieben werden.",
                path.display()
            ),
        )
        .with_details(e.to_string())
    })
}

/// Schreibt eine Datei atomar im selben Verzeichnis.
pub fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "datei".into());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    // Berechtigungen einer vorhandenen Datei übernehmen.
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), s);
        assert_eq!(s.appearance.theme, Theme::Dark);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let s: Settings = toml::from_str("[appearance]\ntheme = \"light\"\n").unwrap();
        assert_eq!(s.appearance.theme, Theme::Light);
        assert_eq!(s.logging, Logging::default());
    }

    #[test]
    fn validate_clamps_entries() {
        let mut s = Settings::default();
        s.logging.max_entries = 5;
        let notes = s.validate();
        assert_eq!(s.logging.max_entries, MIN_LOG_ENTRIES);
        assert_eq!(notes.len(), 1);
        s.logging.max_entries = usize::MAX;
        s.validate();
        assert_eq!(s.logging.max_entries, MAX_LOG_ENTRIES);
    }
}
