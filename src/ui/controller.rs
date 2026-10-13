//! UI-unabhängige Steuerlogik.
//!
//! Der Controller läuft ausschließlich im GUI-Thread und führt selbst keine
//! blockierenden Aufrufe aus: lang laufende Arbeit wird über die zurück-
//! gegebenen Aufträge (z. B. [`CancelToken`], [`SessionSpec`]) von der
//! Slint-Anbindung in Hintergrund-Threads ausgeführt; deren Ergebnisse kommen
//! über die `on_*`-Methoden zurück. Dadurch ist die Logik ohne GUI testbar.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Local};

use crate::error::{AppError, ErrorKind};
use crate::state::{AppState, LastRunRecord, Operation, UpdateState};
use crate::updater::progress::{OutputParser, is_transient_progress_line};
use crate::updater::{
    Backend, CancelToken, CheckReport, NewsItem, Outcome, OutcomeKind, PendingUpdate, Prompt,
    SessionSpec, StatusSnapshot, Step, StepStatus, ToolInfo, UpdateSource, UpgradeOptions,
    UpgradeTracker,
};

/// Maximale Anzahl Zeilen in der Terminal-Ansicht der Update-Seite.
pub const TERMINAL_LINES: usize = 400;

/// Was die Anbindung bei „Abbrechen“ tun soll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelAction {
    /// Laufende Prüfung über ihr Token abbrechen (bereits ausgelöst).
    CheckCancelled,
    /// Antwort an die Bestätigungsabfrage senden (arch-update bricht selbst ab).
    SendLine(String),
    /// Prozessgruppe beenden (nur vor Paketänderungen).
    Terminate,
    /// Abbruch derzeit nicht sicher möglich.
    NotAllowed(String),
}

/// Ob das Fenster geschlossen werden darf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePolicy {
    Allow,
    /// Prüfung läuft; sie wird beim Schließen abgebrochen.
    AllowCancellingCheck,
    /// Update läuft; Schließen würde pacman unterbrechen.
    Block {
        cancellable: bool,
    },
}

/// Ergebnis der Verarbeitung eines Ausgabeblocks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputEffects {
    /// Neue vollständige Zeilen (für das Protokoll).
    pub lines: Vec<String>,
    /// Automatische Antwort (nur für „Press "enter" to quit“ am Ende).
    pub auto_reply: Option<String>,
}

pub struct Controller<B: Backend> {
    backend: Arc<B>,
    pub state: AppState,
    tool: Option<Result<ToolInfo, AppError>>,
    snapshot: Option<StatusSnapshot>,
    check_problem: Option<AppError>,
    check_cancel: Option<CancelToken>,
    tracker: Option<UpgradeTracker>,
    parser: OutputParser,
    terminal: VecDeque<String>,
    terminal_dirty: bool,
    error: Option<AppError>,
    last_outcome: Option<Outcome>,
    last_run: Option<LastRunRecord>,
    news: Vec<NewsItem>,
    news_status: String,
    news_loading: bool,
    auto_quit_sent: bool,
}

impl<B: Backend> Controller<B> {
    pub fn new(backend: Arc<B>, last_run: Option<LastRunRecord>) -> Self {
        Self {
            backend,
            state: AppState::default(),
            tool: None,
            snapshot: None,
            check_problem: None,
            check_cancel: None,
            tracker: None,
            parser: OutputParser::new(),
            terminal: VecDeque::new(),
            terminal_dirty: false,
            error: None,
            last_outcome: None,
            last_run,
            news: Vec::new(),
            news_status: "Noch nicht geladen.".into(),
            news_loading: false,
            auto_quit_sent: false,
        }
    }

    pub fn backend(&self) -> Arc<B> {
        Arc::clone(&self.backend)
    }

    // ---------------------------------------------------------------- Status

    pub fn on_tool_info(&mut self, info: Result<ToolInfo, AppError>) {
        if let Err(e) = &info
            && e.kind == ErrorKind::ToolMissing
        {
            self.error = Some(e.clone());
        }
        self.tool = Some(info);
    }

    pub fn tool_available(&self) -> bool {
        !matches!(&self.tool, Some(Err(e)) if e.kind == ErrorKind::ToolMissing)
    }

    pub fn on_status(&mut self, status: Result<StatusSnapshot, AppError>) {
        match status {
            Ok(s) => {
                if !self.state.is_busy() {
                    self.state.update_state = UpdateState::from_check(
                        s.check.last_check.is_some(),
                        s.check.pending.len(),
                        self.check_problem.is_some(),
                    );
                }
                self.snapshot = Some(s);
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn snapshot(&self) -> Option<&StatusSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn pending(&self) -> &[PendingUpdate] {
        self.snapshot
            .as_ref()
            .map(|s| s.check.pending.as_slice())
            .unwrap_or(&[])
    }

    // ----------------------------------------------------------------- Check

    pub fn can_check(&self) -> bool {
        !self.state.is_busy() && self.tool_available()
    }

    /// Beginnt eine Prüfung; der Aufrufer führt `backend.check(&token)` im Hintergrund aus.
    pub fn begin_check(&mut self) -> Result<CancelToken, AppError> {
        if !self.tool_available() {
            return Err(AppError::new(ErrorKind::ToolMissing, ""));
        }
        self.state.begin(Operation::Checking)?;
        self.error = None;
        let token = CancelToken::new();
        self.check_cancel = Some(token.clone());
        tracing::info!("Update-Prüfung gestartet (arch-update --check)");
        Ok(token)
    }

    pub fn on_check_finished(&mut self, result: Result<CheckReport, AppError>) {
        self.check_cancel = None;
        let next = match result {
            Ok(report) => {
                self.check_problem = report.problem_error();
                let n = report.state.pending.len();
                let has = report.state.last_check.is_some();
                let snapshot = self.snapshot.get_or_insert_with(StatusSnapshot::default);
                snapshot.check = report.state;
                if let Some(problem) = &self.check_problem {
                    tracing::warn!("Update-Prüfung unvollständig: {}", problem.details);
                    self.error = Some(problem.clone());
                } else {
                    tracing::info!("Update-Prüfung abgeschlossen: {n} Update(s) verfügbar");
                }
                UpdateState::from_check(has, n, self.check_problem.is_some())
            }
            Err(e) if e.kind == ErrorKind::Cancelled => {
                tracing::info!("Update-Prüfung abgebrochen");
                self.previous_check_state()
            }
            Err(e) => {
                tracing::error!("Update-Prüfung fehlgeschlagen: {e} {}", e.details);
                let kind = e.kind;
                self.error = Some(e);
                UpdateState::CheckFailed(kind)
            }
        };
        self.state.end(Operation::Checking, next);
    }

    fn previous_check_state(&self) -> UpdateState {
        match &self.snapshot {
            Some(s) => UpdateState::from_check(
                s.check.last_check.is_some(),
                s.check.pending.len(),
                self.check_problem.is_some(),
            ),
            None => UpdateState::Unknown,
        }
    }

    // --------------------------------------------------------------- Upgrade

    /// Installation ist nur zulässig, wenn arch-update Updates gemeldet hat,
    /// nichts anderes läuft und keine andere arch-update-Instanz aktiv ist.
    pub fn can_install(&self) -> bool {
        !self.state.is_busy()
            && self.tool_available()
            && !self.pending().is_empty()
            && !self.snapshot.as_ref().is_some_and(|s| s.instance_running)
    }

    /// Zahl für das Taskleisten-Badge: bekannte ausstehende Updates, sonst 0.
    /// `None` während einer Prüfung oder Installation – der bisherige Wert bleibt
    /// stehen, statt zu flackern. Nach einem fehlgeschlagenen oder abgebrochenen
    /// Lauf gelten die gemeldeten Updates weiter als ausstehend.
    pub fn badge_count(&self) -> Option<usize> {
        match self.state.update_state {
            UpdateState::Checking | UpdateState::Upgrading(_) => None,
            UpdateState::UpdateAvailable(n) | UpdateState::CheckIncomplete(n) => Some(n),
            UpdateState::Finished(OutcomeKind::Completed) => Some(0),
            UpdateState::Finished(_) => Some(self.pending().len()),
            _ => Some(0),
        }
    }

    pub fn begin_upgrade(&mut self, opts: UpgradeOptions) -> Result<SessionSpec, AppError> {
        if self.state.is_busy() {
            return Err(AppError::new(ErrorKind::Busy, ""));
        }
        if self.pending().is_empty() {
            return Err(AppError::new(
                ErrorKind::Internal,
                "Es sind keine Updates bekannt. Bitte zuerst nach Updates suchen.",
            ));
        }
        if self.backend.instance_running() {
            return Err(AppError::new(
                ErrorKind::AlreadyRunning,
                "Die Sperrdatei von arch-update ist belegt.",
            ));
        }
        let spec = self.backend.upgrade_spec(opts)?;
        self.state.begin(Operation::Upgrading)?;
        self.tracker = Some(UpgradeTracker::new());
        self.parser = OutputParser::new();
        self.terminal.clear();
        self.terminal_dirty = true;
        self.error = None;
        self.last_outcome = None;
        self.auto_quit_sent = false;
        tracing::info!(
            "Update-Lauf gestartet: {} {}",
            spec.program.display(),
            spec.args.join(" ")
        );
        Ok(spec)
    }

    /// Der Prozess konnte nicht gestartet werden.
    pub fn on_upgrade_start_failed(&mut self, err: AppError) {
        tracing::error!(
            "Update-Lauf konnte nicht gestartet werden: {err} {}",
            err.details
        );
        self.tracker = None;
        let kind = err.kind;
        self.error = Some(err);
        self.state.end(
            Operation::Upgrading,
            UpdateState::Finished(OutcomeKind::Failed(kind)),
        );
    }

    pub fn session_active(&self) -> bool {
        self.state.operation() == Some(Operation::Upgrading)
    }

    pub fn on_output(&mut self, chunk: &str) -> OutputEffects {
        let mut effects = OutputEffects::default();
        let Some(tracker) = self.tracker.as_mut() else {
            return effects;
        };
        for line in self.parser.feed(chunk) {
            tracker.on_line(&line);
            if line.trim().is_empty() {
                continue;
            }
            self.auto_quit_sent = false;
            self.terminal.push_back(line.clone());
            if !is_transient_progress_line(&line) {
                effects.lines.push(line);
            }
        }
        while self.terminal.len() > TERMINAL_LINES {
            self.terminal.pop_front();
        }
        self.terminal_dirty = true;
        tracker.on_partial(self.parser.partial());
        self.state.set_phase(tracker.phase());

        // `quit_msg` am Ende hält nur ein Terminalfenster offen; in der GUI
        // wird es automatisch bestätigt. Andere Abfragen werden nie automatisch beantwortet.
        if let Some(Prompt::PressEnter { text }) = tracker.prompt()
            && text.contains("to quit")
            && !self.auto_quit_sent
        {
            self.auto_quit_sent = true;
            tracker.input_sent();
            effects.auto_reply = Some(String::new());
        }
        effects
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.tracker.as_ref().and_then(UpgradeTracker::prompt)
    }

    /// Bereitet eine Benutzereingabe vor. Gibt den zu sendenden Text zurück.
    pub fn take_input(&mut self, text: &str) -> Result<String, AppError> {
        let tracker = self
            .tracker
            .as_mut()
            .filter(|_| self.state.operation() == Some(Operation::Upgrading))
            .ok_or_else(|| AppError::new(ErrorKind::Internal, "Kein laufender Update-Vorgang."))?;
        tracker.input_sent();
        Ok(text.to_string())
    }

    pub fn is_cancellable(&self) -> bool {
        match self.state.operation() {
            Some(Operation::Checking) => true,
            Some(Operation::Upgrading) => self
                .tracker
                .as_ref()
                .is_some_and(UpgradeTracker::is_cancellable),
            None => false,
        }
    }

    pub fn request_cancel(&mut self) -> CancelAction {
        match self.state.operation() {
            Some(Operation::Checking) => {
                if let Some(t) = &self.check_cancel {
                    t.cancel();
                }
                CancelAction::CheckCancelled
            }
            Some(Operation::Upgrading) => {
                let Some(tracker) = self.tracker.as_mut() else {
                    return CancelAction::NotAllowed("Kein Update-Prozess aktiv.".into());
                };
                if !tracker.is_cancellable() {
                    return CancelAction::NotAllowed(
                        "Paketänderungen laufen bereits. Ein Abbruch könnte das System beschädigen."
                            .into(),
                    );
                }
                tracker.mark_user_cancelled();
                let at_confirmation = matches!(
                    tracker.prompt(),
                    Some(Prompt::YesNo { text, .. }) if text.contains("Proceed with update?")
                );
                tracker.input_sent();
                tracing::info!("Abbruch durch Benutzer angefordert");
                if at_confirmation {
                    CancelAction::SendLine("n".into())
                } else {
                    CancelAction::Terminate
                }
            }
            None => CancelAction::NotAllowed("Kein Vorgang aktiv.".into()),
        }
    }

    pub fn on_session_exit(&mut self, code: Option<i32>) -> Outcome {
        // Restliche unvollständige Zeile übernehmen.
        let rest = self.parser.partial().trim().to_string();
        let tracker = self.tracker.get_or_insert_with(UpgradeTracker::new);
        if !rest.is_empty() {
            tracker.on_line(&rest);
        }
        let outcome = tracker.on_exit(code);
        tracing::info!(
            "Update-Lauf beendet (Exit-Code {:?}): {} – {}",
            code,
            outcome.title(),
            outcome.summary
        );
        if let OutcomeKind::Failed(kind) = outcome.kind {
            self.error = Some(
                AppError::new(kind, outcome.summary.clone()).with_details(outcome.details.clone()),
            );
        }
        self.last_run = Some(LastRunRecord::new(
            Local::now(),
            outcome.title(),
            &outcome.summary,
            outcome.is_success(),
        ));
        self.state
            .end(Operation::Upgrading, UpdateState::Finished(outcome.kind));
        self.last_outcome = Some(outcome.clone());
        outcome
    }

    pub fn last_outcome(&self) -> Option<&Outcome> {
        self.last_outcome.as_ref()
    }

    pub fn last_run(&self) -> Option<&LastRunRecord> {
        self.last_run.as_ref()
    }

    pub fn tracker(&self) -> Option<&UpgradeTracker> {
        self.tracker.as_ref()
    }

    /// Terminaltext, falls seit dem letzten Aufruf geändert.
    pub fn take_terminal_text(&mut self) -> Option<String> {
        if !self.terminal_dirty {
            return None;
        }
        self.terminal_dirty = false;
        let mut text: String = self
            .terminal
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        let partial = self.parser.partial();
        if !partial.is_empty() {
            text.push('\n');
            text.push_str(partial);
        }
        Some(text)
    }

    // ------------------------------------------------------------------ News

    pub fn begin_news(&mut self) -> Option<CancelToken> {
        if self.news_loading || !self.tool_available() {
            return None;
        }
        self.news_loading = true;
        self.news_status = "News werden über arch-update abgerufen …".into();
        Some(CancelToken::new())
    }

    pub fn on_news(&mut self, result: Result<Vec<NewsItem>, AppError>) {
        self.news_loading = false;
        match result {
            Ok(items) => {
                self.news_status = if items.is_empty() {
                    "arch-update hat keine News geliefert.".into()
                } else {
                    format!("{} aktuelle Meldungen (Quelle: archlinux.org)", items.len())
                };
                self.news = items;
            }
            Err(e) => {
                tracing::warn!("Arch-News nicht abrufbar: {e}");
                self.news_status = format!("{} {}", e.kind.title(), e.message);
            }
        }
    }

    pub fn news(&self) -> &[NewsItem] {
        &self.news
    }
    pub fn news_status(&self) -> &str {
        &self.news_status
    }
    pub fn news_loading(&self) -> bool {
        self.news_loading
    }

    // ---------------------------------------------------------------- Fehler

    pub fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
    pub fn set_error(&mut self, e: AppError) {
        self.error = Some(e);
    }
    pub fn dismiss_error(&mut self) {
        self.error = None;
    }

    pub fn close_policy(&self) -> ClosePolicy {
        match self.state.operation() {
            None => ClosePolicy::Allow,
            Some(Operation::Checking) => ClosePolicy::AllowCancellingCheck,
            Some(Operation::Upgrading) => ClosePolicy::Block {
                cancellable: self.is_cancellable(),
            },
        }
    }

    pub fn cancel_check_for_shutdown(&self) {
        if let Some(t) = &self.check_cancel {
            t.cancel();
        }
    }

    // -------------------------------------------------------------- Ansicht

    pub fn dashboard(&self) -> DashboardView {
        DashboardView::build(self)
    }

    pub fn steps(&self) -> Vec<(Step, StepStatus)> {
        match &self.tracker {
            Some(t) => t.steps().to_vec(),
            None => Step::ALL
                .iter()
                .map(|s| (*s, StepStatus::Pending))
                .collect(),
        }
    }
}

/// Status-Ton für die Darstellung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral = 0,
    Success = 1,
    Warning = 2,
    Error = 3,
    Busy = 4,
}

/// Fertig formatierte Dashboard-Werte. Unbekanntes wird als solches angezeigt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardView {
    pub state_title: String,
    pub state_description: String,
    pub state_tone: Tone,
    pub tool_version: String,
    pub tool_package: String,
    pub kernel: String,
    pub pending_total: usize,
    pub pending_pacman: usize,
    pub pending_aur: usize,
    pub pending_flatpak: usize,
    pub self_update: String,
    pub last_check: String,
    pub last_check_note: String,
    pub last_install: String,
    pub last_install_note: String,
    pub last_install_tone: Tone,
    pub last_update_run: String,
    pub last_transaction: String,
    pub reboot: String,
    pub reboot_tone: Tone,
    pub timer: String,
    pub external_running: bool,
}

pub const UNKNOWN: &str = "Unbekannt";

pub fn format_datetime(dt: &DateTime<Local>) -> String {
    dt.format("%d.%m.%Y, %H:%M Uhr").to_string()
}

impl DashboardView {
    fn build<B: Backend>(c: &Controller<B>) -> Self {
        let snap = c.snapshot.as_ref();
        let pending = c.pending();
        let count = |src: UpdateSource| pending.iter().filter(|p| p.source == src).count();

        let (state_title, state_description, state_tone) = match &c.state.update_state {
            UpdateState::Unknown => (
                "Noch keine Prüfung".to_string(),
                "Es liegen keine Prüfergebnisse von arch-update vor.".to_string(),
                Tone::Neutral,
            ),
            UpdateState::Checking => (
                "Suche nach Updates …".to_string(),
                "arch-update --check fragt Paketquellen, AUR und Flatpak ab.".to_string(),
                Tone::Busy,
            ),
            UpdateState::UpToDate => (
                "System ist aktuell".to_string(),
                "arch-update hat bei der letzten Prüfung keine Updates gefunden.".to_string(),
                Tone::Success,
            ),
            UpdateState::UpdateAvailable(n) => (
                format!("{n} Update(s) verfügbar"),
                "Die Installation erfolgt über arch-update inklusive Prüfung durch pacman."
                    .to_string(),
                Tone::Warning,
            ),
            UpdateState::CheckIncomplete(n) => (
                "Prüfung unvollständig".to_string(),
                format!("{n} Update(s) gefunden, aber die Prüfung hat Fehler gemeldet."),
                Tone::Error,
            ),
            UpdateState::CheckFailed(kind) => (
                "Prüfung fehlgeschlagen".to_string(),
                kind.title().to_string(),
                Tone::Error,
            ),
            UpdateState::Upgrading(phase) => (
                "Update läuft …".to_string(),
                phase.label().to_string(),
                Tone::Busy,
            ),
            UpdateState::Finished(kind) => {
                let tone = match kind {
                    OutcomeKind::Completed | OutcomeKind::UpToDate => Tone::Success,
                    OutcomeKind::CompletedWithWarnings | OutcomeKind::Cancelled => Tone::Warning,
                    OutcomeKind::Failed(_) => Tone::Error,
                };
                let title = c
                    .last_outcome
                    .as_ref()
                    .map(|o| o.title().to_string())
                    .unwrap_or_else(|| "Update-Lauf beendet".into());
                let desc = c
                    .last_outcome
                    .as_ref()
                    .map(|o| o.summary.clone())
                    .unwrap_or_default();
                (title, desc, tone)
            }
        };

        let (tool_version, tool_package) = match &c.tool {
            None => ("Wird ermittelt …".to_string(), String::new()),
            Some(Ok(t)) => (
                t.version_line.clone(),
                t.package
                    .as_ref()
                    .map(|(n, v)| format!("Paket {n} {v}"))
                    .unwrap_or_else(|| "Paketzugehörigkeit unbekannt".into()),
            ),
            Some(Err(e)) => (e.kind.title().to_string(), e.message.clone()),
        };

        let own_pkg = match &c.tool {
            Some(Ok(t)) => t.package.as_ref().map(|(n, _)| n.clone()),
            _ => None,
        };
        let self_update = own_pkg
            .and_then(|name| pending.iter().find(|p| p.name == name))
            .map(|p| {
                format!(
                    "Neue Version verfügbar: {}",
                    p.new_version
                        .as_deref()
                        .unwrap_or("(Version nicht angezeigt)")
                )
            })
            .unwrap_or_default();

        let last_check = snap
            .and_then(|s| s.check.last_check.as_ref())
            .map(format_datetime)
            .unwrap_or_else(|| "Noch nie".into());
        let last_check_note = match (&c.state.update_state, &c.check_problem) {
            (UpdateState::Checking, _) => "Prüfung läuft".to_string(),
            (_, Some(p)) => format!("{} – Details im Protokoll", p.kind.title()),
            (UpdateState::CheckFailed(k), _) => k.title().to_string(),
            _ if snap.is_some_and(|s| s.check.last_check.is_some()) => {
                "Erfolgreich (laut arch-update)".to_string()
            }
            _ => String::new(),
        };

        let (last_install, last_install_note, last_install_tone) = match &c.last_run {
            Some(r) => (
                r.finished_at_local()
                    .as_ref()
                    .map(format_datetime)
                    .unwrap_or_else(|| UNKNOWN.into()),
                r.title.clone(),
                if r.success {
                    Tone::Success
                } else {
                    Tone::Warning
                },
            ),
            None => (
                "Kein Lauf über diese App".to_string(),
                String::new(),
                Tone::Neutral,
            ),
        };

        let (reboot, reboot_tone) = match snap.and_then(|s| s.reboot_pending) {
            Some(true) => (
                "Neustart erforderlich (Kernel aktualisiert)".to_string(),
                Tone::Warning,
            ),
            Some(false) => ("Kein Neustart erforderlich".to_string(), Tone::Success),
            None if snap.is_some() => ("Nicht ermittelbar".to_string(), Tone::Neutral),
            None => ("Wird ermittelt …".to_string(), Tone::Neutral),
        };

        let timer = match snap.and_then(|s| s.timer_enabled) {
            Some(true) => "Aktiv (täglich, arch-update.timer)".to_string(),
            Some(false) => "Deaktiviert".to_string(),
            None => UNKNOWN.to_string(),
        };

        DashboardView {
            state_title,
            state_description,
            state_tone,
            tool_version,
            tool_package,
            kernel: snap
                .and_then(|s| s.kernel.clone())
                .unwrap_or_else(|| UNKNOWN.into()),
            pending_total: pending.len(),
            pending_pacman: count(UpdateSource::Pacman),
            pending_aur: count(UpdateSource::Aur),
            pending_flatpak: count(UpdateSource::Flatpak),
            self_update,
            last_check,
            last_check_note,
            last_install,
            last_install_note,
            last_install_tone,
            last_update_run: snap
                .and_then(|s| s.check.last_update_run)
                .map(|d| d.format("%d.%m.%Y").to_string())
                .unwrap_or_else(|| UNKNOWN.into()),
            last_transaction: snap
                .and_then(|s| s.last_transaction.as_ref())
                .map(format_datetime)
                .unwrap_or_else(|| UNKNOWN.into()),
            reboot,
            reboot_tone,
            timer,
            external_running: snap.is_some_and(|s| s.instance_running),
        }
    }
}
