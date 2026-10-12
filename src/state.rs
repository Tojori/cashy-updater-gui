//! Gemeinsamer Anwendungszustand mit klar definierten Übergängen.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::config::settings::write_atomic;
use crate::error::{AppError, ErrorKind};
use crate::updater::{OutcomeKind, UpdatePhase};

/// Exklusive, lang laufende Vorgänge. Es kann immer nur einer aktiv sein.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Checking,
    Upgrading,
}

/// Zusammengefasster Update-Status für Dashboard und Statusleiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// Noch keine Prüfung bekannt (keine Statusdateien von arch-update).
    Unknown,
    Checking,
    UpToDate,
    UpdateAvailable(usize),
    /// Prüfung hat Fehler gemeldet; Liste ggf. unvollständig.
    CheckIncomplete(usize),
    CheckFailed(ErrorKind),
    Upgrading(UpdatePhase),
    Finished(OutcomeKind),
}

impl UpdateState {
    /// Zustand aus den von arch-update gespeicherten Daten.
    pub fn from_check(has_check_data: bool, pending: usize, incomplete: bool) -> Self {
        match (has_check_data, pending, incomplete) {
            (_, n, true) => UpdateState::CheckIncomplete(n),
            (_, n, false) if n > 0 => UpdateState::UpdateAvailable(n),
            (true, _, false) => UpdateState::UpToDate,
            (false, _, false) => UpdateState::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    operation: Option<Operation>,
    pub update_state: UpdateState,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            operation: None,
            update_state: UpdateState::Unknown,
        }
    }
}

impl AppState {
    pub fn operation(&self) -> Option<Operation> {
        self.operation
    }

    pub fn is_busy(&self) -> bool {
        self.operation.is_some()
    }

    /// Startet einen exklusiven Vorgang oder meldet `Busy`.
    pub fn begin(&mut self, op: Operation) -> Result<(), AppError> {
        if let Some(running) = self.operation {
            let what = match running {
                Operation::Checking => "Die Update-Prüfung läuft noch.",
                Operation::Upgrading => "Ein Update-Lauf ist aktiv.",
            };
            return Err(AppError::new(ErrorKind::Busy, what));
        }
        self.operation = Some(op);
        self.update_state = match op {
            Operation::Checking => UpdateState::Checking,
            Operation::Upgrading => UpdateState::Upgrading(UpdatePhase::Starting),
        };
        Ok(())
    }

    /// Beendet den Vorgang `op`. Ein anderer laufender Vorgang bleibt unberührt.
    pub fn end(&mut self, op: Operation, next: UpdateState) {
        if self.operation == Some(op) {
            self.operation = None;
            self.update_state = next;
        }
    }

    pub fn set_phase(&mut self, phase: UpdatePhase) {
        if self.operation == Some(Operation::Upgrading) {
            self.update_state = UpdateState::Upgrading(phase);
        }
    }
}

/// Ergebnis des letzten über die GUI gestarteten Update-Laufs (persistiert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRunRecord {
    pub finished_at: String,
    pub title: String,
    pub summary: String,
    pub success: bool,
}

impl LastRunRecord {
    pub fn new(at: DateTime<Local>, title: &str, summary: &str, success: bool) -> Self {
        Self {
            finished_at: at.to_rfc3339(),
            title: title.to_string(),
            summary: summary.to_string(),
            success,
        }
    }

    pub fn finished_at_local(&self) -> Option<DateTime<Local>> {
        DateTime::parse_from_rfc3339(&self.finished_at)
            .ok()
            .map(|d| d.with_timezone(&Local))
    }

    /// Lädt den Eintrag; defekte oder fehlende Dateien ergeben `None`.
    pub fn load(path: &Path) -> Option<Self> {
        toml::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    pub fn save(&self, path: &Path) -> Result<(), AppError> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| AppError::internal("Ergebnis konnte nicht serialisiert werden.", e))?;
        write_atomic(path, text.as_bytes())
            .map_err(|e| AppError::internal("Ergebnis konnte nicht gespeichert werden.", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_operations() {
        let mut s = AppState::default();
        s.begin(Operation::Checking).unwrap();
        assert_eq!(s.update_state, UpdateState::Checking);
        let err = s.begin(Operation::Upgrading).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Busy);
        let err = s.begin(Operation::Checking).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Busy);
        // Falscher Vorgang beendet nichts.
        s.end(Operation::Upgrading, UpdateState::UpToDate);
        assert!(s.is_busy());
        s.end(Operation::Checking, UpdateState::UpToDate);
        assert!(!s.is_busy());
        s.begin(Operation::Upgrading).unwrap();
        s.set_phase(UpdatePhase::Installing);
        assert_eq!(
            s.update_state,
            UpdateState::Upgrading(UpdatePhase::Installing)
        );
    }

    #[test]
    fn state_from_check_data() {
        assert_eq!(
            UpdateState::from_check(false, 0, false),
            UpdateState::Unknown
        );
        assert_eq!(
            UpdateState::from_check(true, 0, false),
            UpdateState::UpToDate
        );
        assert_eq!(
            UpdateState::from_check(true, 3, false),
            UpdateState::UpdateAvailable(3)
        );
        assert_eq!(
            UpdateState::from_check(true, 0, true),
            UpdateState::CheckIncomplete(0)
        );
    }

    #[test]
    fn last_run_roundtrip_and_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("last_result.toml");
        assert!(LastRunRecord::load(&p).is_none());
        let r = LastRunRecord::new(Local::now(), "Update erfolgreich", "ok", true);
        r.save(&p).unwrap();
        assert_eq!(LastRunRecord::load(&p).unwrap(), r);
        fs::write(&p, "kaputt = [").unwrap();
        assert!(LastRunRecord::load(&p).is_none());
    }
}
