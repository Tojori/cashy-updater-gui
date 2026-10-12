//! Verbindung zwischen Slint-Oberfläche und [`Controller`].
//!
//! * Callbacks der Oberfläche werden im GUI-Thread behandelt.
//! * Lang laufende Arbeit (Prozessaufrufe, PTY) läuft in eigenen Threads.
//! * Ergebnisse werden per `slint::invoke_from_event_loop` zurück in den
//!   GUI-Thread übertragen – der einzige Weg, auf dem Zustand geändert wird.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};

use crate::config::arch_update_conf::{ArchUpdateConf, VALUE_KEYS};
use crate::config::settings::{self, MAX_LOG_ENTRIES, MIN_LOG_ENTRIES};
use crate::config::{Settings, Theme as ThemeSetting};
use crate::error::{AppError, ErrorKind};
use crate::logging::{self, LogFilter, LogLevel, LogStore};
use crate::paths::{AppPaths, XdgDirs};
use crate::state::{Operation, UpdateState};
use crate::ui::controller::{CancelAction, ClosePolicy, Controller, Tone};
use crate::ui::{AppBridge, InfoRow, LogRow, MainWindow, NewsRow, PackageRow, StepRow, theme};
use crate::updater::service::ARCH_NEWS_URL;
use crate::updater::{
    Backend, OutcomeKind, Prompt, SessionEvent, StepStatus, UpdaterService, UpgradeOptions,
    UpgradeSession,
};

type Service = UpdaterService;

/// Anzahl Logzeilen, die initial bzw. pro „Ältere laden“ angezeigt werden.
const LOG_PAGE: usize = 500;

struct LogView {
    limit: usize,
    min_id: u64,
    selected: HashSet<u64>,
    visible_ids: Vec<u64>,
    seen_next_id: u64,
    dirty: bool,
    revision: i32,
}

struct Core {
    controller: Controller<Service>,
    session: Option<UpgradeSession>,
    settings: Settings,
    xdg: XdgDirs,
    paths: AppPaths,
    logs: LogStore,
    log_view: LogView,
    clipboard: Option<arboard::Clipboard>,
    workers: Vec<JoinHandle<()>>,
    refreshing: bool,
    quit_after_session: bool,
    terminal_revision: i32,
    packages: Rc<VecModel<PackageRow>>,
    log_rows: Rc<VecModel<LogRow>>,
    toast_timer: Timer,
    last_status_refresh: Instant,
}

pub struct Bridge {
    ui: slint::Weak<MainWindow>,
    core: RefCell<Core>,
    _timers: Vec<Timer>,
}

thread_local! {
    static BRIDGE: RefCell<Option<Rc<Bridge>>> = const { RefCell::new(None) };
}

fn with_bridge(f: impl FnOnce(&Rc<Bridge>)) {
    let b = BRIDGE.with(|b| b.borrow().clone());
    if let Some(b) = b {
        f(&b);
    }
}

/// Führt `f` im GUI-Thread aus (aus beliebigem Thread aufrufbar).
fn post(f: impl FnOnce(&Rc<Bridge>) + Send + 'static) {
    // Fehler nur, wenn die Ereignisschleife bereits beendet ist.
    let _ = slint::invoke_from_event_loop(move || with_bridge(f));
}

fn spawn_worker(name: &str, f: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    match thread::Builder::new().name(name.into()).spawn(f) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!("Hintergrund-Thread {name} konnte nicht gestartet werden: {e}");
            None
        }
    }
}

/// Startet ein Hilfsprogramm (z. B. xdg-open) ohne Shell und ohne zu blockieren.
fn spawn_detached(program: &str, args: &[&str]) -> Result<(), AppError> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            AppError::new(
                ErrorKind::ToolMissing,
                format!("{program} konnte nicht gestartet werden."),
            )
            .with_details(e.to_string())
        })?;
    // Prozess im Hintergrund einsammeln (keine Zombies).
    let _ = thread::Builder::new().name("reaper".into()).spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

/// Kurze, unkritische Antworten werden protokolliert, alles andere nicht.
fn loggable_input(text: &str) -> bool {
    text.len() <= 16
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || " yYnNaAjJ".contains(c))
}

impl Bridge {
    /// Installiert die Anbindung und startet die initialen Hintergrundabfragen.
    pub fn install(
        ui: &MainWindow,
        backend: Service,
        settings: Settings,
        xdg: XdgDirs,
        logs: LogStore,
        last_run: Option<crate::state::LastRunRecord>,
    ) -> Rc<Bridge> {
        let paths = xdg.app();
        let backend = std::sync::Arc::new(backend);
        let packages = Rc::new(VecModel::default());
        let log_rows = Rc::new(VecModel::default());
        let core = Core {
            controller: Controller::new(backend, last_run),
            session: None,
            settings,
            xdg,
            paths,
            logs,
            log_view: LogView {
                limit: LOG_PAGE,
                min_id: 0,
                selected: HashSet::new(),
                visible_ids: Vec::new(),
                seen_next_id: 0,
                dirty: true,
                revision: 0,
            },
            clipboard: None,
            workers: Vec::new(),
            refreshing: false,
            quit_after_session: false,
            terminal_revision: 0,
            packages: packages.clone(),
            log_rows: log_rows.clone(),
            toast_timer: Timer::default(),
            last_status_refresh: Instant::now(),
        };

        let g = ui.global::<AppBridge>();
        g.set_packages(ModelRc::from(packages));
        g.set_log_rows(ModelRc::from(log_rows));

        // Regelmäßige Aktualisierung der Log-Ansicht (entkoppelt von der Log-Rate).
        let log_timer = Timer::default();
        log_timer.start(TimerMode::Repeated, Duration::from_millis(250), || {
            with_bridge(|b| b.act(|core, ui| core.update_logs(ui)));
        });
        // Status regelmäßig neu einlesen (lokale Dateien; erkennt externe Läufe).
        let status_timer = Timer::default();
        status_timer.start(TimerMode::Repeated, Duration::from_secs(5), || {
            with_bridge(|b| {
                let due = {
                    let core = b.core.borrow();
                    let interval = if core
                        .controller
                        .snapshot()
                        .is_some_and(|s| s.instance_running)
                    {
                        Duration::from_secs(5)
                    } else {
                        Duration::from_secs(60)
                    };
                    !core.controller.state.is_busy()
                        && core.last_status_refresh.elapsed() >= interval
                };
                if due {
                    b.refresh_status();
                }
            });
        });

        let bridge = Rc::new(Bridge {
            ui: ui.as_weak(),
            core: RefCell::new(core),
            _timers: vec![log_timer, status_timer],
        });
        BRIDGE.with(|b| *b.borrow_mut() = Some(bridge.clone()));
        bridge.connect_callbacks(ui);
        bridge.act(|core, ui| {
            core.load_settings_into_ui(ui);
            core.load_conf_into_ui(ui, None);
            core.update_diagnostics(ui);
        });
        bridge.load_tool_info();
        bridge.refresh_status();
        bridge
    }

    /// Einziger Zugriffspunkt auf den Zustand: leiht `Core` und die Oberfläche
    /// aus, führt `f` aus und synchronisiert anschließend die Anzeige.
    fn act(&self, f: impl FnOnce(&mut Core, &MainWindow)) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Ok(mut core) = self.core.try_borrow_mut() else {
            // Sollte nicht vorkommen; verhindert Panik bei Reentranz.
            tracing::warn!("Verschachtelter Zustandszugriff ignoriert");
            return;
        };
        f(&mut core, &ui);
        core.sync(&ui);
    }

    fn connect_callbacks(self: &Rc<Self>, ui: &MainWindow) {
        let g = ui.global::<AppBridge>();

        g.on_check_updates(|| with_bridge(|b| b.start_check()));
        g.on_start_upgrade(|| with_bridge(|b| b.start_upgrade()));
        g.on_cancel_operation(|| with_bridge(|b| b.act(|core, _| core.cancel())));
        g.on_open_terminal(|| with_bridge(|b| b.open_terminal()));
        g.on_refresh_status(|| with_bridge(|b| b.refresh_status()));
        g.on_send_input(|text| with_bridge(|b| b.act(|core, _| core.send_input(&text))));
        g.on_answer(|text| with_bridge(|b| b.act(|core, _| core.send_input(&text))));
        g.on_load_news(|| with_bridge(|b| b.load_news()));
        g.on_open_news_web(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    if let Err(e) = spawn_detached("xdg-open", &[ARCH_NEWS_URL]) {
                        core.toast(ui, &e.to_string(), Tone::Error);
                    }
                })
            })
        });
        g.on_dismiss_error(|| with_bridge(|b| b.act(|core, _| core.controller.dismiss_error())));
        g.on_copy_error(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    if let Some(e) = core.controller.error().cloned() {
                        let text = format!("{}\n{}\n\n{}", e.kind.title(), e.summary(), e.details);
                        core.copy_to_clipboard(ui, &text, "Fehlerdetails kopiert.");
                    }
                })
            })
        });

        g.on_log_filter_changed(|| {
            with_bridge(|b| {
                b.act(|core, _| {
                    core.log_view.limit = LOG_PAGE;
                    core.log_view.dirty = true;
                })
            })
        });
        g.on_log_toggle_select(|index| {
            with_bridge(|b| b.act(|core, ui| core.toggle_log_selection(ui, index)))
        });
        g.on_log_select_all(|| {
            with_bridge(|b| {
                b.act(|core, _| {
                    let ids: Vec<u64> = core.log_view.visible_ids.clone();
                    core.log_view.selected.extend(ids);
                    core.log_view.dirty = true;
                })
            })
        });
        g.on_log_copy_selected(|| with_bridge(|b| b.act(|core, ui| core.copy_selected_logs(ui))));
        g.on_log_export(|| with_bridge(|b| b.act(|core, ui| core.export_logs(ui))));
        g.on_log_clear_view(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    core.log_view.min_id = core.logs.next_id();
                    core.log_view.selected.clear();
                    core.log_view.dirty = true;
                    core.toast(
                        ui,
                        "Anzeige geleert. Die Logdatei bleibt unverändert.",
                        Tone::Neutral,
                    );
                })
            })
        });
        g.on_log_load_more(|| {
            with_bridge(|b| {
                b.act(|core, _| {
                    core.log_view.limit += LOG_PAGE;
                    core.log_view.dirty = true;
                })
            })
        });
        g.on_open_log_folder(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    let dir = core.paths.log_dir.clone();
                    let _ = std::fs::create_dir_all(&dir);
                    if let Err(e) = spawn_detached("xdg-open", &[&dir.to_string_lossy()]) {
                        core.toast(ui, &e.to_string(), Tone::Error);
                    }
                })
            })
        });

        g.on_settings_changed(|| {
            with_bridge(|b| b.act(|core, ui| core.apply_settings_from_ui(ui)))
        });
        g.on_set_timer(|enabled| with_bridge(|b| b.set_timer(enabled)));
        g.on_conf_save(|| with_bridge(|b| b.act(|core, ui| core.save_conf(ui))));
        g.on_conf_reload(|| {
            with_bridge(|b| {
                b.act(|core, ui| core.load_conf_into_ui(ui, Some("Konfiguration neu geladen.")))
            })
        });
        g.on_conf_generate(|| with_bridge(|b| b.generate_conf()));
        g.on_copy_diagnostics(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    let text = core
                        .diagnostics()
                        .into_iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    core.copy_to_clipboard(ui, &text, "Diagnose kopiert.");
                })
            })
        });
        g.on_close_cancel_and_quit(|| {
            with_bridge(|b| {
                b.act(|core, ui| {
                    ui.global::<AppBridge>().set_close_dialog_visible(false);
                    if core.controller.is_cancellable() {
                        core.quit_after_session = true;
                        core.cancel();
                    }
                })
            })
        });

        ui.window().on_close_requested(|| {
            let mut response = CloseRequestResponse::HideWindow;
            with_bridge(|b| {
                b.act(|core, ui| match core.controller.close_policy() {
                    ClosePolicy::Allow => {}
                    ClosePolicy::AllowCancellingCheck => {
                        core.controller.cancel_check_for_shutdown();
                    }
                    ClosePolicy::Block { cancellable } => {
                        let g = ui.global::<AppBridge>();
                        g.set_close_dialog_can_cancel(cancellable);
                        g.set_close_dialog_visible(true);
                        response = CloseRequestResponse::KeepWindowShown;
                    }
                })
            });
            response
        });
    }

    // ------------------------------------------------------ Hintergrundaufgaben

    fn load_tool_info(&self) {
        let backend = self.core.borrow().controller.backend();
        let h = spawn_worker("tool-info", move || {
            let info = backend.tool_info();
            post(move |b| {
                let mut check_now = false;
                b.act(|core, ui| {
                    match &info {
                        Ok(i) => {
                            tracing::info!("Backend: {} ({})", i.version_line, i.program.display())
                        }
                        Err(e) => tracing::error!("Backend nicht verfügbar: {e} {}", e.details),
                    }
                    check_now = info.is_ok() && core.settings.behavior.check_on_startup;
                    core.controller.on_tool_info(info);
                    core.update_diagnostics(ui);
                });
                if check_now {
                    tracing::info!("Update-Prüfung beim Start ist aktiviert");
                    b.start_check();
                }
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn refresh_status(&self) {
        let backend = {
            let mut core = self.core.borrow_mut();
            if core.refreshing {
                return;
            }
            core.refreshing = true;
            core.last_status_refresh = Instant::now();
            core.controller.backend()
        };
        let h = spawn_worker("status", move || {
            let status = backend.read_status();
            post(move |b| {
                b.act(|core, ui| {
                    core.refreshing = false;
                    core.last_status_refresh = Instant::now();
                    core.controller.on_status(status);
                    core.update_diagnostics(ui);
                })
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn start_check(&self) {
        let mut started = None;
        self.act(|core, ui| match core.controller.begin_check() {
            Ok(token) => started = Some((token, core.controller.backend())),
            Err(e) if e.kind == ErrorKind::Busy => core.toast(ui, &e.to_string(), Tone::Warning),
            Err(e) => core.controller.set_error(e),
        });
        let Some((token, backend)) = started else {
            return;
        };
        let h = spawn_worker("check", move || {
            let result = backend.check(&token);
            post(move |b| {
                b.act(|core, ui| {
                    core.controller.on_check_finished(result);
                    if let UpdateState::UpdateAvailable(n) = core.controller.state.update_state {
                        core.toast(ui, &format!("{n} Update(s) verfügbar."), Tone::Warning);
                    }
                });
                b.refresh_status();
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn start_upgrade(&self) {
        self.act(|core, ui| {
            let opts = UpgradeOptions {
                devel: core.settings.upgrade.include_devel,
            };
            let spec = match core.controller.begin_upgrade(opts) {
                Ok(spec) => spec,
                Err(e) => {
                    if e.kind == ErrorKind::Busy {
                        core.toast(ui, &e.to_string(), Tone::Warning);
                    } else {
                        core.controller.set_error(e);
                    }
                    return;
                }
            };
            ui.global::<AppBridge>().set_page(1);
            let result = UpgradeSession::start(&spec, |event| {
                post(move |b| b.on_session_event(event));
            });
            match result {
                Ok(session) => {
                    tracing::debug!("arch-update läuft mit PID {:?}", session.pid());
                    core.session = Some(session);
                }
                Err(e) => core.controller.on_upgrade_start_failed(
                    AppError::new(
                        ErrorKind::Internal,
                        "arch-update konnte nicht im PTY gestartet werden.",
                    )
                    .with_details(e.to_string()),
                ),
            }
        });
    }

    fn on_session_event(&self, event: SessionEvent) {
        let mut refresh = false;
        let mut quit = false;
        self.act(|core, ui| match event {
            SessionEvent::Output(chunk) => {
                let effects = core.controller.on_output(&chunk);
                for line in &effects.lines {
                    match logging::level_for_process_line(line) {
                        LogLevel::Error => tracing::error!(target: "arch-update", "{line}"),
                        LogLevel::Warn => tracing::warn!(target: "arch-update", "{line}"),
                        _ => tracing::info!(target: "arch-update", "{line}"),
                    }
                }
                if let Some(reply) = effects.auto_reply
                    && let Some(s) = core.session.as_mut()
                {
                    tracing::info!("Abschlussabfrage von arch-update automatisch bestätigt");
                    if let Err(e) = s.send_line(&reply) {
                        tracing::warn!("Eingabe konnte nicht gesendet werden: {e}");
                    }
                }
            }
            SessionEvent::Exited(code) => {
                core.session = None;
                let outcome = core.controller.on_session_exit(code);
                if let Some(record) = core.controller.last_run()
                    && let Err(e) = record.save(&core.paths.last_result_file)
                {
                    tracing::warn!("Ergebnis konnte nicht gespeichert werden: {e}");
                }
                let tone = match outcome.kind {
                    OutcomeKind::Completed | OutcomeKind::UpToDate => Tone::Success,
                    OutcomeKind::Failed(_) => Tone::Error,
                    _ => Tone::Warning,
                };
                let mut msg = outcome.title().to_string();
                if outcome.reboot_required {
                    msg.push_str(" – Neustart erforderlich.");
                }
                core.toast(ui, &msg, tone);
                refresh = core.settings.behavior.refresh_after_upgrade;
                quit = core.quit_after_session;
            }
        });
        if quit {
            if let Some(ui) = self.ui.upgrade() {
                let _ = ui.hide();
            }
            let _ = slint::quit_event_loop();
        } else if refresh {
            self.refresh_status();
        }
    }

    fn load_news(&self) {
        let mut job = None;
        self.act(|core, _| {
            job = core
                .controller
                .begin_news()
                .map(|token| (token, core.controller.backend()));
        });
        let Some((token, backend)) = job else {
            return;
        };
        let h = spawn_worker("news", move || {
            let result = backend.news(&token);
            post(move |b| b.act(|core, _| core.controller.on_news(result)));
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn open_terminal(&self) {
        let mut job = None;
        self.act(|core, ui| {
            if core.controller.state.is_busy() || core.controller.backend().instance_running() {
                core.toast(ui, "Es läuft bereits ein Update-Vorgang.", Tone::Warning);
                return;
            }
            job = Some(core.controller.backend());
        });
        let Some(backend) = job else {
            return;
        };
        let h = spawn_worker("terminal", move || {
            let result = backend.launch_in_terminal();
            post(move |b| {
                b.act(|core, ui| match result {
                    Ok(()) => {
                        tracing::info!("arch-update im Terminal gestartet (arch-update.desktop)");
                        core.toast(
                            ui,
                            "arch-update wurde im Terminal gestartet. Der Status wird automatisch aktualisiert.",
                            Tone::Neutral,
                        );
                    }
                    Err(e) => core.controller.set_error(e),
                });
                // Lock-Status zeitnah prüfen.
                b.core.borrow_mut().last_status_refresh = Instant::now() - Duration::from_secs(120);
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn set_timer(&self, enabled: bool) {
        let backend = self.core.borrow().controller.backend();
        let h = spawn_worker("timer", move || {
            let result = backend.set_timer(enabled);
            post(move |b| {
                b.act(|core, ui| match result {
                    Ok(()) => {
                        let msg = if enabled {
                            "Automatische Update-Prüfung aktiviert."
                        } else {
                            "Automatische Update-Prüfung deaktiviert."
                        };
                        tracing::info!("{msg}");
                        core.toast(ui, msg, Tone::Success);
                    }
                    Err(e) => {
                        tracing::error!("{e} {}", e.details);
                        core.controller.set_error(e);
                    }
                });
                b.refresh_status();
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    fn generate_conf(&self) {
        let backend = self.core.borrow().controller.backend();
        let h = spawn_worker("gen-config", move || {
            let result = backend.generate_config();
            post(move |b| {
                b.act(|core, ui| match result {
                    Ok(()) => {
                        tracing::info!("arch-update.conf mit --gen-config erzeugt");
                        core.load_conf_into_ui(ui, Some("Konfiguration erzeugt."));
                    }
                    Err(e) => core.controller.set_error(e),
                })
            });
        });
        self.core.borrow_mut().workers.extend(h);
    }

    /// Beim Beenden: laufende Prüfung abbrechen und Threads kurz abwarten.
    pub fn shutdown(&self) {
        let workers = {
            let mut core = self.core.borrow_mut();
            core.controller.cancel_check_for_shutdown();
            std::mem::take(&mut core.workers)
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        for h in workers {
            while !h.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        BRIDGE.with(|b| b.borrow_mut().take());
    }
}

impl Core {
    fn toast(&mut self, ui: &MainWindow, text: &str, tone: Tone) {
        let g = ui.global::<AppBridge>();
        g.set_toast_text(ss(text));
        g.set_toast_tone(tone as i32);
        let weak = ui.as_weak();
        self.toast_timer
            .start(TimerMode::SingleShot, Duration::from_secs(5), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppBridge>().set_toast_text(SharedString::new());
                }
            });
    }

    fn copy_to_clipboard(&mut self, ui: &MainWindow, text: &str, success: &str) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    self.toast(
                        ui,
                        &format!("Zwischenablage nicht verfügbar: {e}"),
                        Tone::Error,
                    );
                    return;
                }
            }
        }
        let result = self
            .clipboard
            .as_mut()
            .map(|c| c.set_text(text.to_string()));
        match result {
            Some(Ok(())) => self.toast(ui, success, Tone::Success),
            Some(Err(e)) => self.toast(ui, &format!("Kopieren fehlgeschlagen: {e}"), Tone::Error),
            None => {}
        }
    }

    fn cancel(&mut self) {
        match self.controller.request_cancel() {
            CancelAction::CheckCancelled => {}
            CancelAction::SendLine(answer) => {
                if let Some(s) = self.session.as_mut()
                    && let Err(e) = s.send_line(&answer)
                {
                    tracing::warn!("Abbruch-Antwort konnte nicht gesendet werden: {e}");
                }
            }
            CancelAction::Terminate => {
                if let Some(s) = self.session.as_mut()
                    && let Err(e) = s.terminate()
                {
                    tracing::warn!("arch-update konnte nicht beendet werden: {e}");
                }
            }
            CancelAction::NotAllowed(msg) => {
                tracing::warn!("Abbruch abgelehnt: {msg}");
                self.controller
                    .set_error(AppError::new(ErrorKind::Busy, msg));
            }
        }
    }

    fn send_input(&mut self, text: &str) {
        let is_password = matches!(self.controller.prompt(), Some(Prompt::Password { .. }));
        let Ok(input) = self.controller.take_input(text) else {
            return;
        };
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Err(e) = session.send_line(&input) {
            tracing::warn!("Eingabe konnte nicht gesendet werden: {e}");
            return;
        }
        if is_password {
            tracing::info!("Passwort an die Rechteerhöhung übergeben (nicht protokolliert)");
        } else if input.is_empty() {
            tracing::info!("Eingabe gesendet: <Enter>");
        } else if loggable_input(&input) {
            tracing::info!("Eingabe gesendet: {input}");
        } else {
            tracing::info!("Texteingabe gesendet (Inhalt nicht protokolliert)");
        }
    }

    // ------------------------------------------------------------ Protokoll

    fn log_filter(ui: &MainWindow) -> LogFilter {
        let g = ui.global::<AppBridge>();
        LogFilter {
            show_debug: g.get_log_show_debug(),
            show_info: g.get_log_show_info(),
            show_warn: g.get_log_show_warn(),
            show_error: g.get_log_show_error(),
            query: g.get_log_query().to_string(),
        }
    }

    fn update_logs(&mut self, ui: &MainWindow) {
        let next = self.logs.next_id();
        if !self.log_view.dirty && next == self.log_view.seen_next_id {
            return;
        }
        self.log_view.dirty = false;
        self.log_view.seen_next_id = next;
        let filter = Self::log_filter(ui);
        let (entries, total) = self
            .logs
            .query(&filter, self.log_view.min_id, self.log_view.limit);
        self.log_view.visible_ids = entries.iter().map(|e| e.id).collect();
        let rows: Vec<LogRow> = entries
            .iter()
            .map(|e| LogRow {
                id: e.id as i32,
                time: ss(e.time_text()),
                level: match e.level {
                    LogLevel::Debug => 0,
                    LogLevel::Info => 1,
                    LogLevel::Warn => 2,
                    LogLevel::Error => 3,
                },
                level_text: ss(e.level.label()),
                source: ss(&e.source),
                message: ss(&e.message),
                selected: self.log_view.selected.contains(&e.id),
            })
            .collect();
        self.log_rows.set_vec(rows);
        let g = ui.global::<AppBridge>();
        let selected = self.log_view.selected.len();
        g.set_log_info(ss(format!(
            "{} von {} Einträgen angezeigt{}",
            entries.len(),
            total,
            if selected > 0 {
                format!(" · {selected} ausgewählt")
            } else {
                String::new()
            }
        )));
        g.set_log_has_more(total > entries.len());
        self.log_view.revision = self.log_view.revision.wrapping_add(1);
        g.set_log_revision(self.log_view.revision);
    }

    fn toggle_log_selection(&mut self, ui: &MainWindow, index: i32) {
        let Some(&id) = usize::try_from(index)
            .ok()
            .and_then(|i| self.log_view.visible_ids.get(i))
        else {
            return;
        };
        if !self.log_view.selected.remove(&id) {
            self.log_view.selected.insert(id);
        }
        // Nur die betroffene Zeile aktualisieren (kein kompletter Neuaufbau).
        if let Ok(i) = usize::try_from(index)
            && let Some(mut row) = self.log_rows.row_data(i)
        {
            row.selected = self.log_view.selected.contains(&id);
            self.log_rows.set_row_data(i, row);
        }
        let _ = ui;
    }

    fn copy_selected_logs(&mut self, ui: &MainWindow) {
        if self.log_view.selected.is_empty() {
            self.toast(
                ui,
                "Keine Einträge ausgewählt (Zeile anklicken).",
                Tone::Warning,
            );
            return;
        }
        let entries = self.logs.by_ids(&self.log_view.selected);
        let text = logging::format_entries(&entries);
        self.copy_to_clipboard(ui, &text, &format!("{} Einträge kopiert.", entries.len()));
    }

    fn export_logs(&mut self, ui: &MainWindow) {
        let filter = Self::log_filter(ui);
        let (entries, _) = self.logs.query(&filter, 0, usize::MAX);
        match logging::export_entries(&entries, &self.paths.export_dir) {
            Ok(path) => {
                tracing::info!("Protokoll exportiert nach {}", path.display());
                self.toast(
                    ui,
                    &format!("{} Einträge exportiert: {}", entries.len(), path.display()),
                    Tone::Success,
                );
            }
            Err(e) => self.controller.set_error(
                AppError::new(ErrorKind::Internal, "Export fehlgeschlagen.")
                    .with_details(e.to_string()),
            ),
        }
    }

    // ---------------------------------------------------------- Einstellungen

    fn load_settings_into_ui(&mut self, ui: &MainWindow) {
        let g = ui.global::<AppBridge>();
        let s = &self.settings;
        theme::apply(ui, s.appearance.theme);
        g.set_set_check_on_startup(s.behavior.check_on_startup);
        g.set_set_refresh_after(s.behavior.refresh_after_upgrade);
        g.set_set_include_devel(s.upgrade.include_devel);
        g.set_set_debug_log(s.logging.debug);
        g.set_set_write_log(s.logging.write_file);
        g.set_set_max_entries(s.logging.max_entries.min(i32::MAX as usize) as i32);
        g.set_settings_path(ss(self.paths.settings_file.to_string_lossy()));
        g.set_log_file(ss(self
            .logs
            .file_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()));
    }

    fn apply_settings_from_ui(&mut self, ui: &MainWindow) {
        let g = ui.global::<AppBridge>();
        let mut s = self.settings.clone();
        s.appearance.theme = if g.get_dark_theme() {
            ThemeSetting::Dark
        } else {
            ThemeSetting::Light
        };
        s.behavior.check_on_startup = g.get_set_check_on_startup();
        s.behavior.refresh_after_upgrade = g.get_set_refresh_after();
        s.upgrade.include_devel = g.get_set_include_devel();
        s.logging.debug = g.get_set_debug_log();
        s.logging.write_file = g.get_set_write_log();
        s.logging.max_entries =
            (g.get_set_max_entries().max(0) as usize).clamp(MIN_LOG_ENTRIES, MAX_LOG_ENTRIES);
        let notes = s.validate();

        if s.logging.write_file != self.settings.logging.write_file {
            if s.logging.write_file {
                if let Err(e) = self.logs.attach_file(&self.paths.log_dir) {
                    tracing::warn!("Logdatei konnte nicht geöffnet werden: {e}");
                }
            } else {
                self.logs.detach_file();
            }
        }
        self.logs.set_capacity(s.logging.max_entries);
        theme::apply(ui, s.appearance.theme);

        match settings::save(&self.paths.settings_file, &s) {
            Ok(()) => {
                let mut notice = "Einstellungen gespeichert.".to_string();
                if s.logging.debug != self.settings.logging.debug {
                    notice.push_str(" Debug-Protokollierung wirkt nach einem Neustart.");
                }
                for n in notes {
                    notice.push(' ');
                    notice.push_str(&n);
                }
                g.set_settings_notice(ss(notice));
                tracing::debug!("Einstellungen gespeichert");
            }
            Err(e) => {
                g.set_settings_notice(ss(format!("Speichern fehlgeschlagen: {e}")));
                tracing::error!("{e} {}", e.details);
            }
        }
        self.settings = s;
        self.load_settings_into_ui(ui);
        self.log_view.dirty = true;
    }

    fn load_conf_into_ui(&mut self, ui: &MainWindow, notice: Option<&str>) {
        let g = ui.global::<AppBridge>();
        let path = self.controller.backend().config_path();
        g.set_conf_path(ss(path.to_string_lossy()));
        match ArchUpdateConf::load(&path) {
            Ok(None) => {
                g.set_conf_exists(false);
                g.set_conf_notice(ss(notice.unwrap_or_default()));
            }
            Ok(Some(conf)) => {
                g.set_conf_exists(true);
                g.set_conf_no_version(conf.flag("NoVersion"));
                g.set_conf_no_aur(conf.flag("NoAUR"));
                g.set_conf_no_flatpak(conf.flag("NoFlatpak"));
                g.set_conf_no_notification(conf.flag("NoNotification"));
                g.set_conf_no_alhp(conf.flag("NoALHPCheck"));
                g.set_conf_no_color(conf.flag("NoColor"));
                g.set_conf_show_tray_relevant(conf.flag("ShowTrayWhenRelevant"));
                g.set_conf_colorblind(conf.flag("ColorblindMode"));
                let num = |k: &str| conf.effective_value(k).parse::<i32>().unwrap_or(0);
                g.set_conf_news_num(num("NewsNum"));
                g.set_conf_news_timeout(num("NewsTimeout"));
                g.set_conf_check_timeout(num("UpdateCheckTimeout"));
                g.set_conf_keep_old(num("KeepOldPackages"));
                g.set_conf_keep_uninstalled(num("KeepUninstalledPackages"));
                g.set_conf_tray_per_page(num("TrayUpdatesPerPage"));
                let auto = |v: Option<String>| ss(v.unwrap_or_else(|| "Automatisch".into()));
                g.set_conf_aur_helper(auto(conf.value("AURHelper")));
                g.set_conf_su_cmd(auto(conf.value("PrivilegeElevationCommand")));
                g.set_conf_tray_style(ss(conf.effective_value("TrayIconStyle")));
                g.set_conf_notice(ss(notice.unwrap_or_default()));
            }
            Err(e) => {
                g.set_conf_exists(false);
                g.set_conf_notice(ss(format!("{e} {}", e.details)));
            }
        }
    }

    fn save_conf(&mut self, ui: &MainWindow) {
        let g = ui.global::<AppBridge>();
        let path = self.controller.backend().config_path();
        // Aktuellen Dateistand verwenden, um externe Änderungen nicht zu überschreiben.
        let mut conf = match ArchUpdateConf::load(&path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                g.set_conf_notice(ss("Die Datei existiert nicht mehr. Bitte neu erzeugen."));
                return;
            }
            Err(e) => {
                g.set_conf_notice(ss(format!("{e}")));
                return;
            }
        };
        let flags = [
            ("NoVersion", g.get_conf_no_version()),
            ("NoAUR", g.get_conf_no_aur()),
            ("NoFlatpak", g.get_conf_no_flatpak()),
            ("NoNotification", g.get_conf_no_notification()),
            ("NoALHPCheck", g.get_conf_no_alhp()),
            ("NoColor", g.get_conf_no_color()),
            ("ShowTrayWhenRelevant", g.get_conf_show_tray_relevant()),
            ("ColorblindMode", g.get_conf_colorblind()),
        ];
        let values = [
            ("NewsNum", g.get_conf_news_num().to_string()),
            ("NewsTimeout", g.get_conf_news_timeout().to_string()),
            ("UpdateCheckTimeout", g.get_conf_check_timeout().to_string()),
            ("KeepOldPackages", g.get_conf_keep_old().to_string()),
            (
                "KeepUninstalledPackages",
                g.get_conf_keep_uninstalled().to_string(),
            ),
            ("TrayUpdatesPerPage", g.get_conf_tray_per_page().to_string()),
            ("TrayIconStyle", g.get_conf_tray_style().to_string()),
        ];
        let choices = [
            ("AURHelper", g.get_conf_aur_helper().to_string()),
            ("PrivilegeElevationCommand", g.get_conf_su_cmd().to_string()),
        ];

        let apply = || -> Result<(), AppError> {
            for (k, v) in flags {
                conf.set_flag(k, v)?;
            }
            for (k, v) in values {
                let default = VALUE_KEYS
                    .iter()
                    .find(|(key, _, _)| *key == k)
                    .map(|(_, _, d)| *d)
                    .unwrap_or("");
                // Standardwerte nicht unnötig in die Datei schreiben.
                if conf.value(k).is_none() && v == default {
                    continue;
                }
                conf.set_value(k, Some(&v))?;
            }
            for (k, v) in choices {
                if v == "Automatisch" {
                    conf.set_value(k, None)?;
                } else {
                    conf.set_value(k, Some(&v))?;
                }
            }
            conf.save(&path)
        };
        match apply() {
            Ok(()) => {
                tracing::info!("arch-update.conf gespeichert: {}", path.display());
                self.load_conf_into_ui(
                    ui,
                    Some("Gespeichert. Änderungen gelten ab dem nächsten Aufruf von cachy-update."),
                );
            }
            Err(e) => {
                tracing::error!("arch-update.conf: {e} {}", e.details);
                g.set_conf_notice(ss(format!("Nicht gespeichert: {e}")));
            }
        }
    }

    // --------------------------------------------------------------- Diagnose

    fn diagnostics(&self) -> Vec<(String, String)> {
        let backend = self.controller.backend();
        let cfg = backend.config();
        let opt = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "nicht gefunden".into())
        };
        let disp = |p: &Path| p.display().to_string();
        let dash = self.controller.dashboard();
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unbekannt".into());
        vec![
            ("Cashy Updater".into(), env!("CARGO_PKG_VERSION").into()),
            ("Backend-Programm".into(), opt(&cfg.tool)),
            ("Backend-Version".into(), dash.tool_version.clone()),
            ("Paket".into(), dash.tool_package.clone()),
            (
                "arch-update Statusverzeichnis".into(),
                disp(&cfg.arch_paths.state_dir),
            ),
            (
                "arch-update Konfiguration".into(),
                disp(&cfg.arch_paths.config_file),
            ),
            (
                "arch-update Sperrdatei".into(),
                disp(&cfg.arch_paths.lock_file),
            ),
            (
                "Desktop-Datei (Terminal-Start)".into(),
                opt(&cfg.desktop_file),
            ),
            ("GUI-Einstellungen".into(), disp(&self.paths.settings_file)),
            (
                "GUI-Logdatei".into(),
                self.logs
                    .file_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "deaktiviert".into()),
            ),
            ("Exportverzeichnis".into(), disp(&self.paths.export_dir)),
            ("Kernel".into(), dash.kernel.clone()),
            ("Sitzungstyp".into(), session),
            ("Home".into(), disp(&self.xdg.home)),
        ]
    }

    fn update_diagnostics(&self, ui: &MainWindow) {
        let rows: Vec<InfoRow> = self
            .diagnostics()
            .into_iter()
            .map(|(label, value)| InfoRow {
                label: ss(label),
                value: ss(value),
            })
            .collect();
        ui.global::<AppBridge>()
            .set_diagnostics(ModelRc::new(VecModel::from(rows)));
    }

    // ------------------------------------------------------------- Anzeige

    /// Überträgt den Controller-Zustand in die Oberfläche.
    fn sync(&mut self, ui: &MainWindow) {
        let g = ui.global::<AppBridge>();
        let c = &mut self.controller;
        let d = c.dashboard();

        g.set_state_title(ss(&d.state_title));
        g.set_state_description(ss(&d.state_description));
        g.set_state_tone(d.state_tone as i32);
        g.set_tool_version(ss(&d.tool_version));
        g.set_tool_package(ss(&d.tool_package));
        g.set_kernel_version(ss(&d.kernel));
        g.set_self_update(ss(&d.self_update));
        g.set_pending_total(d.pending_total as i32);
        g.set_pending_pacman(d.pending_pacman as i32);
        g.set_pending_aur(d.pending_aur as i32);
        g.set_pending_flatpak(d.pending_flatpak as i32);
        g.set_last_check(ss(&d.last_check));
        g.set_last_check_note(ss(&d.last_check_note));
        g.set_last_install(ss(&d.last_install));
        g.set_last_install_note(ss(&d.last_install_note));
        g.set_last_install_tone(d.last_install_tone as i32);
        g.set_last_update_run(ss(&d.last_update_run));
        g.set_last_transaction(ss(&d.last_transaction));
        g.set_reboot_text(ss(&d.reboot));
        g.set_reboot_tone(d.reboot_tone as i32);
        g.set_timer_text(ss(&d.timer));
        g.set_external_running(d.external_running);

        let op = c.state.operation();
        g.set_busy(op.is_some());
        g.set_checking(op == Some(Operation::Checking));
        g.set_session_active(op == Some(Operation::Upgrading));
        g.set_can_check(c.can_check());
        g.set_can_install(c.can_install());
        g.set_can_cancel(c.is_cancellable());
        g.set_can_open_terminal(op.is_none() && c.tool_available() && !d.external_running);
        g.set_include_devel(self.settings.upgrade.include_devel);

        if let Some(snap) = c.snapshot() {
            g.set_timer_known(snap.timer_enabled.is_some());
            g.set_timer_enabled(snap.timer_enabled.unwrap_or(false));
        }

        // Paketliste nur bei Änderung ersetzen.
        let rows: Vec<PackageRow> = c
            .pending()
            .iter()
            .map(|p| PackageRow {
                source: ss(p.source.label()),
                name: ss(&p.name),
                current: ss(p.current_version.as_deref().unwrap_or("")),
                available: ss(p.new_version.as_deref().unwrap_or("")),
            })
            .collect();
        if self.packages.iter().ne(rows.iter().cloned()) {
            self.packages.set_vec(rows);
        }

        let steps: Vec<StepRow> = c
            .steps()
            .into_iter()
            .map(|(step, status)| StepRow {
                title: ss(step.title()),
                status: match status {
                    StepStatus::Pending => 0,
                    StepStatus::Active => 1,
                    StepStatus::Done => 2,
                    StepStatus::Failed => 3,
                    StepStatus::NotObserved => 4,
                },
            })
            .collect();
        g.set_steps(ModelRc::new(VecModel::from(steps)));

        // Fortschritt
        match op {
            Some(Operation::Checking) => {
                g.set_phase_text(ss("Suche nach Updates (arch-update --check)"));
                g.set_activity(ss(
                    "checkupdates, AUR-Helper und Flatpak werden abgefragt …",
                ));
                g.set_progress_visible(true);
                g.set_progress_known(false);
            }
            Some(Operation::Upgrading) => {
                let tracker = c.tracker();
                g.set_phase_text(ss(tracker
                    .map(|t| t.phase().label())
                    .unwrap_or("Wird gestartet")));
                g.set_activity(ss(tracker.map(|t| t.activity()).unwrap_or("")));
                let waiting_for_input = tracker.and_then(|t| t.prompt()).is_some();
                g.set_progress_visible(!waiting_for_input);
                match tracker.and_then(|t| t.fraction()) {
                    Some(f) => {
                        g.set_progress_known(true);
                        g.set_progress(f);
                        g.set_progress_text(ss(format!("{:.0} %", f * 100.0)));
                    }
                    None => g.set_progress_known(false),
                }
            }
            None => {
                g.set_progress_visible(false);
            }
        }

        let (kind, text, default_yes) = match c.prompt() {
            None => (0, String::new(), false),
            Some(Prompt::YesNo { default_yes, text }) => (1, text.clone(), *default_yes),
            Some(Prompt::Password { text }) => (2, text.clone(), false),
            Some(Prompt::PressEnter { text }) => (3, text.clone(), false),
            Some(Prompt::Selection { text }) => (4, text.clone(), false),
            Some(Prompt::Free { text }) => (5, text.clone(), false),
        };
        g.set_prompt_kind(kind);
        g.set_prompt_text(ss(text.trim_start_matches("->").trim()));
        g.set_prompt_default_yes(default_yes);

        if let Some(text) = c.take_terminal_text() {
            g.set_terminal_text(ss(text));
            self.terminal_revision = self.terminal_revision.wrapping_add(1);
            g.set_terminal_revision(self.terminal_revision);
        }

        // Ergebnis des letzten Laufs
        match (op, c.last_outcome()) {
            (None, Some(o)) => {
                g.set_result_visible(true);
                g.set_result_title(ss(o.title()));
                let mut text = o.summary.clone();
                if o.reboot_required {
                    text.push_str(
                        " Ein Neustart ist erforderlich, um den neuen Kernel zu verwenden.",
                    );
                }
                g.set_result_text(ss(text));
                g.set_result_tone(match o.kind {
                    OutcomeKind::Completed | OutcomeKind::UpToDate => Tone::Success,
                    OutcomeKind::Failed(_) => Tone::Error,
                    _ => Tone::Warning,
                } as i32);
            }
            _ => g.set_result_visible(false),
        }

        match c.error() {
            Some(e) => {
                g.set_error_visible(true);
                g.set_error_title(ss(e.kind.title()));
                g.set_error_summary(ss(e.summary()));
                g.set_error_details(ss(&e.details));
            }
            None => g.set_error_visible(false),
        }

        let news: Vec<NewsRow> = c
            .news()
            .iter()
            .map(|n| NewsRow {
                index: n.index as i32,
                title: ss(&n.title),
            })
            .collect();
        g.set_news(ModelRc::new(VecModel::from(news)));
        g.set_news_status(ss(c.news_status()));
        g.set_news_loading(c.news_loading());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_short_answers_are_logged() {
        assert!(loggable_input("y"));
        assert!(loggable_input("1 3 5"));
        assert!(loggable_input(""));
        assert!(!loggable_input("geheim123"));
        assert!(!loggable_input("hunter2"));
        assert!(!loggable_input("yyyyyyyyyyyyyyyyyyyy"));
    }
}
