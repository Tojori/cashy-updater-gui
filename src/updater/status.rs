//! Zustandsmodell eines Update-Laufs.
//!
//! [`UpgradeTracker`] wertet die (C-Locale-)Ausgabe von arch-update und pacman
//! aus und leitet daraus Phase, Schritte, Fortschritt, Eingabeaufforderungen und
//! das Endergebnis ab. Es werden nur Schritte als erledigt markiert, die in der
//! Ausgabe tatsächlich beobachtet wurden.

use std::collections::VecDeque;

use crate::error::{ErrorKind, classify_failure};
use crate::updater::progress::{parse_counter, parse_download_total, trailing_percent};

/// Feingranulare Phase des laufenden arch-update-Prozesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Starting,
    /// `list_packages.sh`: checkupdates / AUR-Helper / Flatpak werden abgefragt.
    Checking,
    /// „Proceed with update? [Y/n]“
    AwaitingConfirmation,
    /// `list_news.sh`
    News,
    /// `pacman -Syu`: Datenbanken synchronisieren, Abhängigkeiten auflösen.
    Synchronizing,
    /// „:: Retrieving packages...“
    Downloading,
    /// Schlüssel-, Integritäts-, Datei- und Speicherplatzprüfung durch pacman.
    Verifying,
    /// „:: Processing package changes...“
    Installing,
    /// „:: Running post-transaction hooks...“
    PostTransaction,
    /// AUR-Helper (paru/yay/pikaur) läuft.
    Aur,
    /// `flatpak update` läuft.
    Flatpak,
    /// Nacharbeiten: verwaiste Pakete, Paket-Cache, Kernel-Neustart, Dienste.
    Maintenance,
    Finished,
}

impl UpdatePhase {
    pub fn label(self) -> &'static str {
        match self {
            UpdatePhase::Starting => "Wird gestartet",
            UpdatePhase::Checking => "Suche nach Updates",
            UpdatePhase::AwaitingConfirmation => "Warte auf Bestätigung",
            UpdatePhase::News => "Arch-News",
            UpdatePhase::Synchronizing => "Paketdatenbanken werden synchronisiert",
            UpdatePhase::Downloading => "Pakete werden heruntergeladen",
            UpdatePhase::Verifying => "Signaturen und Integrität werden geprüft",
            UpdatePhase::Installing => "Pakete werden installiert",
            UpdatePhase::PostTransaction => "Nachbereitungs-Hooks laufen",
            UpdatePhase::Aur => "AUR-Pakete werden aktualisiert",
            UpdatePhase::Flatpak => "Flatpak-Pakete werden aktualisiert",
            UpdatePhase::Maintenance => "Nacharbeiten (Wartung)",
            UpdatePhase::Finished => "Beendet",
        }
    }

    /// Ein Abbruch ist nur sicher, solange noch keine Paketänderung begonnen hat.
    pub fn is_safely_cancellable(self) -> bool {
        matches!(
            self,
            UpdatePhase::Starting
                | UpdatePhase::Checking
                | UpdatePhase::AwaitingConfirmation
                | UpdatePhase::News
        )
    }
}

/// Die fünf in der GUI dargestellten Schritte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Check,
    Download,
    Verify,
    Install,
    Finish,
}

impl Step {
    pub const ALL: [Step; 5] = [
        Step::Check,
        Step::Download,
        Step::Verify,
        Step::Install,
        Step::Finish,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Step::Check => "Update-Prüfung",
            Step::Download => "Download",
            Step::Verify => "Integritätsprüfung",
            Step::Install => "Installation",
            Step::Finish => "Abschluss",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Active,
    Done,
    Failed,
    /// Schritt wurde im Ablauf nicht beobachtet (z. B. nur Flatpak-Updates).
    NotObserved,
}

/// Erkannte Eingabeaufforderung des Prozesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    YesNo { default_yes: bool, text: String },
    Password { text: String },
    PressEnter { text: String },
    Selection { text: String },
    Free { text: String },
}

impl Prompt {
    pub fn text(&self) -> &str {
        match self {
            Prompt::YesNo { text, .. }
            | Prompt::Password { text }
            | Prompt::PressEnter { text }
            | Prompt::Selection { text }
            | Prompt::Free { text } => text,
        }
    }
}

/// Erkennt Eingabeaufforderungen in der unvollständigen letzten Zeile.
pub fn detect_prompt(partial: &str) -> Option<Prompt> {
    let t = partial.trim_end();
    if t.is_empty() {
        return None;
    }
    let text = t.to_string();
    let lower = t.to_lowercase();
    if t.ends_with("[Y/n]") || t.ends_with("[Y/n]:") {
        return Some(Prompt::YesNo {
            default_yes: true,
            text,
        });
    }
    if t.ends_with("[y/N]") || t.ends_with("[y/N]:") {
        return Some(Prompt::YesNo {
            default_yes: false,
            text,
        });
    }
    if (lower.contains("password") || lower.contains("passwort") || lower.contains("passphrase"))
        && t.ends_with(':')
    {
        return Some(Prompt::Password { text });
    }
    if lower.contains("select the") {
        return Some(Prompt::Selection { text });
    }
    if lower.contains("press \"enter\"") || lower.contains("press enter") {
        return Some(Prompt::PressEnter { text });
    }
    if t == "==>" || t.ends_with('?') || t.ends_with(':') {
        return Some(Prompt::Free { text });
    }
    None
}

/// Ergebnis eines Update-Laufs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Completed,
    CompletedWithWarnings,
    UpToDate,
    Cancelled,
    Failed(ErrorKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub summary: String,
    /// Relevante Ausgabezeilen (Fehler/Warnungen bzw. letzte Ausgabe).
    pub details: String,
    pub exit_code: Option<i32>,
    pub reboot_required: bool,
}

impl Outcome {
    pub fn title(&self) -> &'static str {
        match self.kind {
            OutcomeKind::Completed => "Update erfolgreich abgeschlossen",
            OutcomeKind::CompletedWithWarnings => "Update mit Warnungen beendet",
            OutcomeKind::UpToDate => "System ist aktuell",
            OutcomeKind::Cancelled => "Update abgebrochen",
            OutcomeKind::Failed(kind) => kind.title(),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self.kind, OutcomeKind::Completed | OutcomeKind::UpToDate)
    }
}

const RECENT_LINES: usize = 300;

/// Verfolgt einen laufenden arch-update-Prozess anhand seiner Ausgabe.
#[derive(Debug)]
pub struct UpgradeTracker {
    phase: UpdatePhase,
    steps: [StepStatus; 5],
    progress: Option<(u32, u32)>,
    percent: Option<u8>,
    activity: String,
    prompt: Option<Prompt>,
    saw_update_applied: bool,
    saw_no_update: bool,
    saw_update_error_warning: bool,
    saw_fatal_error: bool,
    reboot_required: bool,
    user_cancelled: bool,
    notable: Vec<String>,
    recent: VecDeque<String>,
    outcome: Option<Outcome>,
}

impl Default for UpgradeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl UpgradeTracker {
    pub fn new() -> Self {
        Self {
            phase: UpdatePhase::Starting,
            steps: [StepStatus::Pending; 5],
            progress: None,
            percent: None,
            activity: String::new(),
            prompt: None,
            saw_update_applied: false,
            saw_no_update: false,
            saw_update_error_warning: false,
            saw_fatal_error: false,
            reboot_required: false,
            user_cancelled: false,
            notable: Vec::new(),
            recent: VecDeque::new(),
            outcome: None,
        }
    }

    pub fn phase(&self) -> UpdatePhase {
        self.phase
    }
    pub fn steps(&self) -> [(Step, StepStatus); 5] {
        let mut out = [(Step::Check, StepStatus::Pending); 5];
        for (i, s) in Step::ALL.iter().enumerate() {
            out[i] = (*s, self.steps[i]);
        }
        out
    }
    pub fn step_status(&self, step: Step) -> StepStatus {
        self.steps[step.index()]
    }
    pub fn activity(&self) -> &str {
        &self.activity
    }
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }
    pub fn reboot_required(&self) -> bool {
        self.reboot_required
    }
    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// Fortschritt 0.0–1.0, nur wenn belastbare Zählerdaten vorliegen.
    pub fn fraction(&self) -> Option<f32> {
        if let Some((cur, total)) = self.progress
            && total > 0
        {
            // Innerhalb des aktuellen Elements den Prozentwert berücksichtigen.
            let within = self.percent.map(|p| f32::from(p) / 100.0).unwrap_or(0.0);
            let done = (cur.saturating_sub(1) as f32 + within) / total as f32;
            return Some(done.clamp(0.0, 1.0));
        }
        None
    }

    pub fn is_cancellable(&self) -> bool {
        self.outcome.is_none() && self.phase.is_safely_cancellable()
    }

    /// Markiert, dass der Benutzer den Lauf abgebrochen hat.
    pub fn mark_user_cancelled(&mut self) {
        self.user_cancelled = true;
    }

    /// Nach dem Senden einer Eingabe ist die Aufforderung beantwortet.
    pub fn input_sent(&mut self) {
        self.prompt = None;
    }

    fn set_phase(&mut self, phase: UpdatePhase) {
        if self.phase == phase {
            return;
        }
        self.phase = phase;
        self.progress = None;
        self.percent = None;
        let active = match phase {
            UpdatePhase::Starting => None,
            UpdatePhase::Checking => Some(Step::Check),
            UpdatePhase::AwaitingConfirmation | UpdatePhase::News => {
                self.complete_through(Step::Check);
                None
            }
            UpdatePhase::Synchronizing | UpdatePhase::Downloading => Some(Step::Download),
            UpdatePhase::Verifying => Some(Step::Verify),
            UpdatePhase::Installing
            | UpdatePhase::PostTransaction
            | UpdatePhase::Aur
            | UpdatePhase::Flatpak => Some(Step::Install),
            UpdatePhase::Maintenance | UpdatePhase::Finished => Some(Step::Finish),
        };
        if let Some(step) = active {
            self.activate(step);
        }
    }

    /// Setzt `step` aktiv; vorher aktive Schritte gelten als erledigt.
    fn activate(&mut self, step: Step) {
        for i in 0..step.index() {
            if self.steps[i] == StepStatus::Active {
                self.steps[i] = StepStatus::Done;
            }
        }
        if self.steps[step.index()] != StepStatus::Done {
            self.steps[step.index()] = StepStatus::Active;
        }
    }

    /// Schließt alle aktiven Schritte bis einschließlich `step` ab.
    fn complete_through(&mut self, step: Step) {
        for i in 0..=step.index() {
            if self.steps[i] == StepStatus::Active
                || (i == step.index() && self.steps[i] == StepStatus::Pending)
            {
                self.steps[i] = StepStatus::Done;
            }
        }
    }

    /// Verarbeitet eine vollständige Ausgabezeile.
    pub fn on_line(&mut self, line: &str) {
        self.prompt = None;
        let t = line.trim();
        if t.is_empty() {
            return;
        }
        self.recent.push_back(t.to_string());
        while self.recent.len() > RECENT_LINES {
            self.recent.pop_front();
        }
        let lower = t.to_lowercase();
        if lower.starts_with("==> error")
            || lower.starts_with("error:")
            || lower.starts_with("==> warning")
            || lower.starts_with("warning:")
            || lower.starts_with("sudo:")
        {
            self.notable.push(t.to_string());
        }

        // arch-update-Meldungen (lib/*.sh, C-Locale)
        if lower.contains("looking for updates") {
            self.set_phase(UpdatePhase::Checking);
        } else if lower.contains("looking for recent arch news") {
            self.set_phase(UpdatePhase::News);
        } else if t.starts_with("==> Updating Packages") {
            self.set_phase(UpdatePhase::Synchronizing);
        } else if t.starts_with("==> Updating AUR Packages") {
            self.set_phase(UpdatePhase::Aur);
        } else if t.starts_with("==> Updating Flatpak Packages") {
            self.set_phase(UpdatePhase::Flatpak);
        } else if lower.contains("the update has been applied") {
            self.saw_update_applied = true;
            self.set_phase(UpdatePhase::Maintenance);
        } else if lower.contains("no update available") {
            self.saw_no_update = true;
            self.complete_through(Step::Check);
        } else if lower.contains("an error has occurred during the update process") {
            if lower.starts_with("==> error") {
                self.saw_fatal_error = true;
            } else {
                self.saw_update_error_warning = true;
            }
        } else if lower.starts_with("==> reboot required")
            || (lower.contains("pending kernel update") && !lower.contains("no pending"))
        {
            self.reboot_required = true;
        }
        // pacman (auch innerhalb von AUR-Helpern)
        else if t.starts_with(":: Synchronizing package databases") {
            if self.phase != UpdatePhase::Aur {
                self.set_phase(UpdatePhase::Synchronizing);
            }
        } else if t.starts_with(":: Retrieving packages") {
            if self.phase != UpdatePhase::Aur {
                self.set_phase(UpdatePhase::Downloading);
            }
        } else if t.starts_with(":: Processing package changes") {
            if self.phase != UpdatePhase::Aur {
                self.set_phase(UpdatePhase::Installing);
            }
        } else if t.starts_with(":: Running post-transaction hooks")
            && self.phase != UpdatePhase::Aur
        {
            self.set_phase(UpdatePhase::PostTransaction);
        }

        self.observe_progress(t);
        if !crate::updater::progress::is_transient_progress_line(t) {
            self.activity = t.to_string();
        }
    }

    /// Verarbeitet die unvollständige letzte Zeile (Fortschritt/Aufforderung).
    pub fn on_partial(&mut self, partial: &str) {
        let t = partial.trim();
        if t.is_empty() {
            return;
        }
        self.observe_progress(t);
        self.prompt = detect_prompt(partial);
        if let Some(Prompt::YesNo { text, .. }) = &self.prompt
            && text.contains("Proceed with update?")
        {
            self.set_phase(UpdatePhase::AwaitingConfirmation);
        }
        self.activity = t.to_string();
    }

    fn observe_progress(&mut self, t: &str) {
        if let Some(c) = parse_counter(t) {
            let action = c.action.to_lowercase();
            let verify = [
                "checking keyring",
                "checking keys in keyring",
                "checking package integrity",
                "loading package files",
                "checking for file conflicts",
                "checking available disk space",
            ];
            let install = [
                "upgrading",
                "installing",
                "reinstalling",
                "downgrading",
                "removing",
            ];
            if self.phase != UpdatePhase::Aur && self.phase != UpdatePhase::Flatpak {
                if verify.iter().any(|v| action.starts_with(v)) {
                    self.set_phase(UpdatePhase::Verifying);
                } else if install.iter().any(|v| action.starts_with(v))
                    && matches!(
                        self.phase,
                        UpdatePhase::Verifying
                            | UpdatePhase::Synchronizing
                            | UpdatePhase::Downloading
                    )
                {
                    self.set_phase(UpdatePhase::Installing);
                }
            }
            self.progress = Some((c.current.min(c.total), c.total));
            self.percent = trailing_percent(t);
        } else if let Some((cur, total)) = parse_download_total(t) {
            if self.phase == UpdatePhase::Synchronizing {
                self.set_phase(UpdatePhase::Downloading);
            }
            self.progress = Some((cur.min(total), total));
            self.percent = trailing_percent(t);
        }
    }

    /// Prozessende: bestimmt das Ergebnis aus Exit-Code und beobachteter Ausgabe.
    pub fn on_exit(&mut self, exit_code: Option<i32>) -> Outcome {
        self.prompt = None;
        let failing_step = Step::ALL
            .iter()
            .copied()
            .find(|s| self.steps[s.index()] == StepStatus::Active)
            .unwrap_or(Step::Finish);

        let recent: Vec<&str> = self.recent.iter().map(String::as_str).collect();
        let details = if self.notable.is_empty() {
            recent[recent.len().saturating_sub(25)..].join("\n")
        } else {
            self.notable.join("\n")
        };

        let (kind, summary) = if self.user_cancelled && !self.saw_update_applied {
            (
                OutcomeKind::Cancelled,
                "Der Vorgang wurde vor Beginn der Paketänderungen abgebrochen.".to_string(),
            )
        } else {
            match exit_code {
                Some(0) if self.saw_fatal_error => (
                    OutcomeKind::Failed(classify_failure(recent.iter().copied(), ErrorKind::InstallationFailed)),
                    "arch-update hat einen Fehler gemeldet.".to_string(),
                ),
                Some(0) if self.saw_update_error_warning => (
                    OutcomeKind::CompletedWithWarnings,
                    "Mindestens ein Teil-Update (AUR oder Flatpak) ist fehlgeschlagen.".to_string(),
                ),
                Some(0) if self.saw_update_applied => (
                    OutcomeKind::Completed,
                    "arch-update hat das Update angewendet.".to_string(),
                ),
                Some(0) if self.saw_no_update => (
                    OutcomeKind::UpToDate,
                    "arch-update hat keine verfügbaren Updates gefunden.".to_string(),
                ),
                Some(0) => (
                    OutcomeKind::CompletedWithWarnings,
                    "arch-update wurde beendet, eine Anwendung von Updates wurde jedoch nicht bestätigt. Bitte Protokoll prüfen."
                        .to_string(),
                ),
                Some(4) => (
                    OutcomeKind::Cancelled,
                    "Das Update wurde an der Bestätigungsabfrage abgelehnt.".to_string(),
                ),
                Some(17) => (
                    OutcomeKind::Failed(ErrorKind::AlreadyRunning),
                    "arch-update läuft bereits in einer anderen Instanz.".to_string(),
                ),
                Some(2) => (
                    OutcomeKind::Failed(ErrorKind::PermissionDenied),
                    "Kein Programm zur Rechteerhöhung gefunden (sudo, sudo-rs, doas oder run0).".to_string(),
                ),
                Some(5) => (
                    OutcomeKind::Failed(classify_failure(recent.iter().copied(), ErrorKind::InstallationFailed)),
                    "Das Paket-Update durch pacman ist fehlgeschlagen und wurde abgebrochen.".to_string(),
                ),
                Some(6) => (
                    OutcomeKind::CompletedWithWarnings,
                    "Der angeforderte Neustart ist fehlgeschlagen.".to_string(),
                ),
                Some(11) if self.saw_update_applied => (
                    OutcomeKind::CompletedWithWarnings,
                    "Das Update wurde angewendet, aber mindestens ein Dienst konnte nicht neu gestartet werden.".to_string(),
                ),
                Some(code) => (
                    OutcomeKind::Failed(classify_failure(recent.iter().copied(), ErrorKind::Internal)),
                    format!("arch-update wurde mit Exit-Code {code} beendet."),
                ),
                None => (
                    OutcomeKind::Failed(ErrorKind::Internal),
                    "arch-update wurde durch ein Signal beendet.".to_string(),
                ),
            }
        };

        match kind {
            OutcomeKind::Completed | OutcomeKind::CompletedWithWarnings | OutcomeKind::UpToDate => {
                for s in &mut self.steps {
                    *s = match *s {
                        StepStatus::Active | StepStatus::Done => StepStatus::Done,
                        _ => StepStatus::NotObserved,
                    };
                }
                self.steps[Step::Finish.index()] = StepStatus::Done;
            }
            OutcomeKind::Failed(_) => {
                for s in &mut self.steps {
                    if *s == StepStatus::Active {
                        *s = StepStatus::Done;
                    }
                }
                self.steps[failing_step.index()] = StepStatus::Failed;
            }
            OutcomeKind::Cancelled => {
                for s in &mut self.steps {
                    if *s == StepStatus::Active {
                        *s = StepStatus::Pending;
                    }
                }
            }
        }

        self.phase = UpdatePhase::Finished;
        self.progress = None;
        let outcome = Outcome {
            kind,
            summary,
            details,
            exit_code,
            reboot_required: self.reboot_required,
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut UpgradeTracker, lines: &[&str]) {
        for l in lines {
            t.on_line(l);
        }
    }

    #[test]
    fn prompt_detection() {
        assert_eq!(
            detect_prompt("-> Proceed with update? [Y/n] "),
            Some(Prompt::YesNo {
                default_yes: true,
                text: "-> Proceed with update? [Y/n]".into()
            })
        );
        assert!(matches!(
            detect_prompt("-> Would you like to reboot now? [y/N]"),
            Some(Prompt::YesNo {
                default_yes: false,
                ..
            })
        ));
        assert!(matches!(
            detect_prompt("[sudo] password for user: "),
            Some(Prompt::Password { .. })
        ));
        assert!(matches!(
            detect_prompt("==> Press \"enter\" to quit "),
            Some(Prompt::PressEnter { .. })
        ));
        assert!(matches!(
            detect_prompt(
                "-> Select the news to read (e.g. 1 3 5), select 0 to read them all or press \"enter\" to proceed with update:"
            ),
            Some(Prompt::Selection { .. })
        ));
        assert!(detect_prompt("( 1/2) upgrading a  [###---]  50%").is_none());
        assert!(detect_prompt("").is_none());
    }

    #[test]
    fn successful_flow_marks_all_observed_steps() {
        let mut t = UpgradeTracker::new();
        feed(
            &mut t,
            &[
                "==> Looking for updates...",
                "==> Packages:",
                "linux 1 -> 2",
            ],
        );
        assert_eq!(t.step_status(Step::Check), StepStatus::Active);
        t.on_partial("-> Proceed with update? [Y/n] ");
        assert_eq!(t.phase(), UpdatePhase::AwaitingConfirmation);
        assert!(t.is_cancellable());
        assert_eq!(t.step_status(Step::Check), StepStatus::Done);
        t.input_sent();
        feed(
            &mut t,
            &[
                "==> Looking for recent Arch News...",
                "==> Updating Packages...",
                ":: Synchronizing package databases...",
                ":: Starting full system upgrade...",
                ":: Retrieving packages...",
            ],
        );
        assert_eq!(t.phase(), UpdatePhase::Downloading);
        assert!(!t.is_cancellable(), "nach Beginn von pacman kein Abbruch");
        t.on_partial("(1/1) checking keys in keyring     [####----]  50%");
        assert_eq!(t.phase(), UpdatePhase::Verifying);
        assert_eq!(t.step_status(Step::Download), StepStatus::Done);
        feed(
            &mut t,
            &[
                ":: Processing package changes...",
                "(1/2) upgrading linux  [######] 100%",
            ],
        );
        t.on_partial("(2/2) upgrading mesa  [###---]  50%");
        let f = t.fraction().unwrap();
        assert!((f - 0.75).abs() < 0.01, "{f}");
        feed(
            &mut t,
            &[
                ":: Running post-transaction hooks...",
                "==> The update has been applied",
            ],
        );
        let o = t.on_exit(Some(0));
        assert_eq!(o.kind, OutcomeKind::Completed);
        assert!(t.steps().iter().all(|(_, s)| *s == StepStatus::Done));
    }

    #[test]
    fn steps_not_seen_are_reported_as_not_observed() {
        let mut t = UpgradeTracker::new();
        feed(
            &mut t,
            &[
                "==> Looking for updates...",
                "==> Updating Flatpak Packages...",
                "==> The update has been applied",
            ],
        );
        let o = t.on_exit(Some(0));
        assert_eq!(o.kind, OutcomeKind::Completed);
        assert_eq!(t.step_status(Step::Download), StepStatus::NotObserved);
        assert_eq!(t.step_status(Step::Verify), StepStatus::NotObserved);
        assert_eq!(t.step_status(Step::Install), StepStatus::Done);
    }

    #[test]
    fn pacman_failure_is_never_success() {
        let mut t = UpgradeTracker::new();
        feed(
            &mut t,
            &[
                "==> Updating Packages...",
                ":: Retrieving packages...",
                "(1/1) checking package integrity",
                "error: linux: signature from \"Foo\" is unknown trust",
                "==> ERROR: An error has occurred during the update process",
            ],
        );
        let o = t.on_exit(Some(5));
        assert_eq!(o.kind, OutcomeKind::Failed(ErrorKind::IntegrityFailed));
        assert_eq!(t.step_status(Step::Verify), StepStatus::Failed);
        assert!(o.details.contains("unknown trust"));
        assert!(!o.is_success());
    }

    #[test]
    fn exit_codes_are_mapped() {
        let o = |code: Option<i32>, lines: &[&str]| {
            let mut t = UpgradeTracker::new();
            feed(&mut t, lines);
            t.on_exit(code).kind
        };
        assert_eq!(o(Some(4), &[]), OutcomeKind::Cancelled);
        assert_eq!(
            o(Some(17), &[]),
            OutcomeKind::Failed(ErrorKind::AlreadyRunning)
        );
        assert_eq!(
            o(Some(2), &[]),
            OutcomeKind::Failed(ErrorKind::PermissionDenied)
        );
        assert_eq!(
            o(Some(0), &["==> No update available"]),
            OutcomeKind::UpToDate
        );
        assert_eq!(o(Some(0), &[]), OutcomeKind::CompletedWithWarnings);
        assert_eq!(
            o(
                Some(0),
                &[
                    "==> WARNING: An error has occurred during the update process",
                    "==> The update has been applied"
                ]
            ),
            OutcomeKind::CompletedWithWarnings
        );
        assert_eq!(o(None, &[]), OutcomeKind::Failed(ErrorKind::Internal));
        assert_eq!(
            o(Some(5), &["sudo: 3 incorrect password attempts"]),
            OutcomeKind::Failed(ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn user_cancel_before_changes() {
        let mut t = UpgradeTracker::new();
        t.on_line("==> Looking for updates...");
        t.mark_user_cancelled();
        assert_eq!(t.on_exit(None).kind, OutcomeKind::Cancelled);
    }

    #[test]
    fn reboot_required_is_detected() {
        let mut t = UpgradeTracker::new();
        feed(
            &mut t,
            &[
                "==> The update has been applied",
                "==> Reboot required:",
                "There's a pending kernel update on your system requiring a reboot to be applied",
            ],
        );
        assert!(t.on_exit(Some(0)).reboot_required);
        let mut t = UpgradeTracker::new();
        t.on_line("==> No pending kernel update found");
        assert!(!t.reboot_required());
    }

    #[test]
    fn fraction_bounds() {
        let mut t = UpgradeTracker::new();
        assert_eq!(t.fraction(), None);
        t.on_partial("(5/3) upgrading x [######] 100%");
        assert_eq!(t.fraction(), Some(1.0));
        t.on_partial("(0/0) upgrading x");
        assert_eq!(t.fraction(), None);
    }
}
