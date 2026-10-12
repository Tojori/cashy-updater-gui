//! Fehlerkategorien der Anwendung.
//!
//! Die Kategorien orientieren sich an den tatsächlichen Abläufen von
//! `cachy-update`/`arch-update` (Bash) und pacman. Jede Kategorie besitzt eine
//! verständliche deutsche Zusammenfassung; technische Details werden separat
//! transportiert und in der GUI aufklappbar angezeigt.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// DNS-/Verbindungsfehler (z. B. "Could not resolve host").
    NetworkUnavailable,
    /// Die Paketquellen konnten nicht abgefragt werden (z. B. checkupdates: "Cannot fetch updates").
    SourceUnreachable,
    /// Die Update-Prüfung lieferte Fehlermeldungen; das Ergebnis ist ggf. unvollständig.
    CheckIncomplete,
    /// pacman konnte Pakete/Datenbanken nicht herunterladen.
    DownloadFailed,
    /// Signatur-, Schlüssel- oder Prüfsummenprüfung durch pacman fehlgeschlagen.
    IntegrityFailed,
    /// Die Paket-Transaktion ist fehlgeschlagen (Konflikte, Abhängigkeiten, ...).
    InstallationFailed,
    /// Rechteerhöhung (sudo/sudo-rs/doas/run0) fehlgeschlagen oder nicht vorhanden.
    PermissionDenied,
    /// Eine Konfigurationsdatei ist ungültig.
    ConfigInvalid,
    /// `cachy-update`/`arch-update` bzw. ein benötigtes Programm ist nicht installiert.
    ToolMissing,
    /// Eine andere Instanz von arch-update (oder pacman) läuft bereits.
    AlreadyRunning,
    /// In der GUI läuft bereits ein Vorgang.
    Busy,
    /// Der Vorgang wurde abgebrochen.
    Cancelled,
    /// Unerwarteter interner Fehler (I/O, Prozessstart, ...).
    Internal,
}

impl ErrorKind {
    /// Kurze, verständliche Zusammenfassung für die Oberfläche.
    pub fn title(self) -> &'static str {
        match self {
            ErrorKind::NetworkUnavailable => "Netzwerk nicht verfügbar",
            ErrorKind::SourceUnreachable => "Update-Quelle nicht erreichbar",
            ErrorKind::CheckIncomplete => "Update-Prüfung unvollständig",
            ErrorKind::DownloadFailed => "Download fehlgeschlagen",
            ErrorKind::IntegrityFailed => "Integritäts- oder Signaturprüfung fehlgeschlagen",
            ErrorKind::InstallationFailed => "Installation fehlgeschlagen",
            ErrorKind::PermissionDenied => "Berechtigungen unzureichend",
            ErrorKind::ConfigInvalid => "Konfiguration ungültig",
            ErrorKind::ToolMissing => "Benötigtes Programm nicht gefunden",
            ErrorKind::AlreadyRunning => "Eine andere Update-Instanz läuft bereits",
            ErrorKind::Busy => "Es läuft bereits ein Vorgang",
            ErrorKind::Cancelled => "Vorgang abgebrochen",
            ErrorKind::Internal => "Interner Fehler",
        }
    }

    /// Handlungsempfehlung für die Oberfläche.
    pub fn hint(self) -> &'static str {
        match self {
            ErrorKind::NetworkUnavailable => {
                "Bitte Netzwerkverbindung prüfen und den Vorgang erneut starten."
            }
            ErrorKind::SourceUnreachable => {
                "Die Paketspiegel sind nicht erreichbar. Später erneut versuchen oder die Mirrorlist prüfen."
            }
            ErrorKind::CheckIncomplete => {
                "Die angezeigte Liste ist möglicherweise unvollständig. Details im Protokoll prüfen."
            }
            ErrorKind::DownloadFailed => {
                "Netzwerk und Mirror prüfen und das Update erneut starten."
            }
            ErrorKind::IntegrityFailed => {
                "Pakete wurden NICHT installiert. Keyring aktualisieren (archlinux-keyring/cachyos-keyring) und erneut versuchen."
            }
            ErrorKind::InstallationFailed => {
                "Die Ausgabe von pacman in den technischen Details prüfen."
            }
            ErrorKind::PermissionDenied => {
                "Passwort prüfen bzw. sicherstellen, dass sudo, sudo-rs, doas oder run0 eingerichtet ist."
            }
            ErrorKind::ConfigInvalid => "Die betroffene Konfigurationsdatei prüfen.",
            ErrorKind::ToolMissing => "cachy-update installieren: sudo pacman -S cachy-update",
            ErrorKind::AlreadyRunning => {
                "Warten, bis der andere Update-Vorgang beendet ist, und dann erneut versuchen."
            }
            ErrorKind::Busy => "Bitte warten, bis der laufende Vorgang abgeschlossen ist.",
            ErrorKind::Cancelled => "Es wurden keine Änderungen durch diesen Vorgang bestätigt.",
            ErrorKind::Internal => "Technische Details prüfen und ggf. das Protokoll exportieren.",
        }
    }
}

/// Fehler mit Kategorie, optionaler Zusatzinformation und technischen Details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    pub kind: ErrorKind,
    /// Ergänzende, verständliche Beschreibung (deutsch). Kann leer sein.
    pub message: String,
    /// Technische Details (Prozessausgabe, OS-Fehler, ...). Kann leer sein.
    pub details: String,
}

impl AppError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: String::new(),
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = details.into();
        self
    }

    pub fn internal(message: impl Into<String>, err: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Internal, message).with_details(err.to_string())
    }

    /// Text für die Zusammenfassung in der GUI.
    pub fn summary(&self) -> String {
        if self.message.is_empty() {
            self.kind.hint().to_string()
        } else {
            format!("{} {}", self.message, self.kind.hint())
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind.title())?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

/// Ordnet Ausgabezeilen eines fehlgeschlagenen Updates einer Fehlerkategorie zu.
///
/// Erwartet Ausgaben im C-Locale (die GUI startet arch-update mit `LC_ALL=C.UTF-8`).
/// Gibt `fallback` zurück, wenn kein bekanntes Muster gefunden wird.
pub fn classify_failure<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    fallback: ErrorKind,
) -> ErrorKind {
    const PERMISSION: &[&str] = &[
        "incorrect password attempt",
        "a password is required",
        "is not in the sudoers file",
        "authentication failure",
        "authentication failed",
        "not allowed to execute",
        "you cannot perform this operation unless you are root",
        "a privilege elevation command is required",
    ];
    const NETWORK: &[&str] = &[
        "could not resolve host",
        "couldn't resolve host",
        "network is unreachable",
        "failed to connect",
        "connection timed out",
        "operation timed out",
        "temporary failure in name resolution",
    ];
    const INTEGRITY: &[&str] = &[
        "invalid or corrupted package",
        "is unknown trust",
        "invalid pgp signature",
        "signature from",
        "could not be looked up remotely",
        "required key missing from keyring",
        "corrupted database",
        "checksum",
    ];
    const ALREADY_RUNNING: &[&str] = &["unable to lock database", "already a running instance"];
    const DOWNLOAD: &[&str] = &[
        "failed retrieving file",
        "failed to synchronize all databases",
        "failed to retrieve some files",
    ];
    const INSTALL: &[&str] = &[
        "failed to commit transaction",
        "failed to prepare transaction",
        "conflicting files",
        "exists in filesystem",
        "could not satisfy dependencies",
        "conflicting dependencies",
        "unresolvable package conflicts",
    ];

    let lowered: Vec<String> = lines.into_iter().map(|l| l.to_lowercase()).collect();
    let any = |patterns: &[&str]| {
        lowered
            .iter()
            .any(|line| patterns.iter().any(|p| line.contains(p)))
    };

    // Reihenfolge ist relevant: Ursache vor Folgefehler.
    if any(PERMISSION) {
        ErrorKind::PermissionDenied
    } else if any(INTEGRITY) {
        ErrorKind::IntegrityFailed
    } else if any(ALREADY_RUNNING) {
        ErrorKind::AlreadyRunning
    } else if any(NETWORK) {
        ErrorKind::NetworkUnavailable
    } else if any(DOWNLOAD) {
        ErrorKind::DownloadFailed
    } else if any(INSTALL) {
        ErrorKind::InstallationFailed
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_pacman_errors() {
        let c = |l: &str| classify_failure([l], ErrorKind::Internal);
        assert_eq!(
            c("sudo: 3 incorrect password attempts"),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            c("error: linux: signature from \"X\" is unknown trust"),
            ErrorKind::IntegrityFailed
        );
        assert_eq!(
            c(
                "error: failed retrieving file 'core.db' from mirror : Could not resolve host: mirror"
            ),
            ErrorKind::NetworkUnavailable
        );
        assert_eq!(
            c(
                "error: failed retrieving file 'x.pkg.tar.zst' from mirror : The requested URL returned error: 404"
            ),
            ErrorKind::DownloadFailed
        );
        assert_eq!(
            c("error: failed to commit transaction (conflicting files)"),
            ErrorKind::InstallationFailed
        );
        assert_eq!(
            c("error: failed to init transaction (unable to lock database)"),
            ErrorKind::AlreadyRunning
        );
        assert_eq!(c("something unrelated"), ErrorKind::Internal);
    }

    #[test]
    fn permission_wins_over_followup_errors() {
        let lines = [
            "sudo: 3 incorrect password attempts",
            "==> ERROR: An error has occurred during the update process",
        ];
        assert_eq!(
            classify_failure(lines, ErrorKind::InstallationFailed),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn summary_contains_hint() {
        let e = AppError::new(ErrorKind::DownloadFailed, "Mirror antwortet nicht.");
        assert!(e.summary().contains("Mirror antwortet nicht."));
        assert!(e.summary().contains(ErrorKind::DownloadFailed.hint()));
        assert_eq!(
            e.to_string(),
            "Download fehlgeschlagen: Mirror antwortet nicht."
        );
    }
}
