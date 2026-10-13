//! Lebenszyklus der Anwendung: Konfiguration, Logging, GUI-Start und Beenden.

use std::process::ExitCode;

use slint::ComponentHandle;

use crate::config::{self, LoadOutcome};
use crate::launcher_badge::{self, LauncherBadge};
use crate::logging::{self, LogStore};
use crate::paths::XdgDirs;
use crate::state::LastRunRecord;
use crate::ui::MainWindow;
use crate::ui::bridge::Bridge;
use crate::updater::{ServiceConfig, SystemRunner, UpdaterService};

/// Sicherheitslimit für nicht-interaktive Aufrufe (z. B. `--check`).
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub fn run() -> ExitCode {
    let xdg = match XdgDirs::from_env() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cashy-updater-gui: {e}");
            return ExitCode::from(2);
        }
    };
    let paths = xdg.app();

    // Einstellungen laden – Fehler führen nie zum Absturz.
    let loaded = config::settings::load(&paths.settings_file);
    let settings = loaded.settings().clone();

    let logs = LogStore::new(settings.logging.max_entries);
    let log_file = if settings.logging.write_file {
        logs.attach_file(&paths.log_dir).map_err(|e| e.to_string())
    } else {
        Err("deaktiviert".into())
    };
    if let Err(e) = logging::init(&logs, settings.logging.debug) {
        eprintln!("cashy-updater-gui: Logging konnte nicht initialisiert werden: {e}");
    }

    tracing::info!("Cashy Updater {} gestartet", env!("CARGO_PKG_VERSION"));
    match &log_file {
        Ok(p) => tracing::info!("Logdatei: {}", p.display()),
        Err(e) => tracing::info!("Keine Logdatei ({e})"),
    }
    match &loaded {
        LoadOutcome::Loaded { notes, .. } => {
            for n in notes {
                tracing::warn!("Einstellungen korrigiert: {n}");
            }
        }
        LoadOutcome::Missing(_) => {
            tracing::info!("Keine Einstellungsdatei vorhanden – Standardwerte werden verwendet");
        }
        LoadOutcome::Corrupt { error, backup, .. } => {
            tracing::error!(
                "Einstellungsdatei {} ist ungültig ({error}); Standardwerte werden verwendet. Sicherung: {}",
                paths.settings_file.display(),
                backup
                    .as_ref()
                    .map(|b| b.display().to_string())
                    .unwrap_or_else(|| "nicht möglich".into())
            );
        }
    }

    let service_cfg = ServiceConfig::detect(&xdg);
    match &service_cfg.tool {
        Some(t) => tracing::info!("Backend gefunden: {}", t.display()),
        None => tracing::error!("Weder cachy-update noch arch-update im PATH gefunden"),
    }
    let service = UpdaterService::new(
        service_cfg,
        SystemRunner {
            timeout: Some(COMMAND_TIMEOUT),
        },
    );

    let ui = match MainWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            tracing::error!("GUI konnte nicht erstellt werden: {e}");
            eprintln!(
                "cashy-updater-gui: Die grafische Oberfläche konnte nicht gestartet werden: {e}\n\
                 Bitte prüfen, ob eine Wayland- oder X11-Sitzung läuft (WAYLAND_DISPLAY bzw. DISPLAY)\n\
                 und ob die Laufzeitbibliotheken installiert sind: fontconfig, freetype2, libxkbcommon,\n\
                 wayland bzw. libx11/libxcursor/libxrandr/libxi sowie mesa (EGL/OpenGL)."
            );
            return ExitCode::from(1);
        }
    };

    // Wayland-app_id bzw. X11-WM_CLASS = Name der Desktop-Datei: Taskleiste und
    // Badge ordnen das Fenster darüber `cashy-updater-gui.desktop` zu.
    if let Err(e) = slint::set_xdg_app_id(launcher_badge::APP_ID) {
        tracing::debug!("app_id konnte nicht gesetzt werden: {e}");
    }

    let last_run = LastRunRecord::load(&paths.last_result_file);
    let bridge = Bridge::install(
        &ui,
        service,
        settings,
        xdg,
        logs.clone(),
        last_run,
        LauncherBadge::session(),
    );

    let result = ui.run();
    bridge.shutdown();
    match result {
        Ok(()) => {
            tracing::info!("Cashy Updater beendet");
            logs.detach_file();
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("Ereignisschleife mit Fehler beendet: {e}");
            logs.detach_file();
            eprintln!("cashy-updater-gui: {e}");
            ExitCode::from(1)
        }
    }
}
