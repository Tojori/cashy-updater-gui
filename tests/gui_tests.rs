//! Ende-zu-Ende-Test der Oberfläche mit dem Slint-Test-Backend (ohne Display).
//!
//! Bedient die echte `MainWindow` über Accessibility-Aktionen und führt mit dem
//! Fake-arch-update eine Prüfung und einen vollständigen Update-Lauf (PTY) aus.
//! Das Test-Backend erlaubt nur eine Ereignisschleife pro Prozess, daher gibt
//! es hier genau einen Test.

mod common;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cashy_updater_gui::config::{Settings, Theme as ThemeSetting};
use cashy_updater_gui::logging::{self, LogStore};
use cashy_updater_gui::paths::XdgDirs;
use cashy_updater_gui::ui::bridge::Bridge;
use cashy_updater_gui::ui::{AppBridge, MainWindow, Theme};
use common::Env;
use i_slint_backend_testing::ElementHandle;
use slint::platform::WindowEvent;
use slint::{ComponentHandle, Model, Timer, TimerMode};

fn g(ui: &MainWindow) -> AppBridge<'_> {
    ui.global::<AppBridge>()
}

fn click(ui: &MainWindow, label: &str) -> bool {
    match ElementHandle::find_by_accessible_label(ui, label).next() {
        Some(e) => {
            e.invoke_accessible_default_action();
            true
        }
        None => false,
    }
}

/// Testschritt: liefert `true`, wenn erledigt (dann folgt der nächste Schritt).
type Step = Box<dyn FnMut(&MainWindow) -> bool>;

#[test]
fn gui_end_to_end_with_fake_arch_update() {
    i_slint_backend_testing::init_integration_test_with_system_time();

    let env = Env::new("success").with_env("FAKE_DELAY", "0.05");
    let root = env.dir.path();
    let xdg = XdgDirs::from_parts(
        root.join("home"),
        Some(root.join("config")),
        Some(root.join("state")),
        None,
        None,
    );
    let settings_file = xdg.app().settings_file;
    let logs = LogStore::new(5000);
    logging::init(&logs, true).unwrap();

    let ui = MainWindow::new().unwrap();
    let bridge = Bridge::install(
        &ui,
        env.service(),
        Settings::default(),
        xdg,
        logs.clone(),
        None,
    );
    ui.show().unwrap();

    let steps: Vec<(&str, Step)> = vec![
        (
            "Status und Version geladen",
            Box::new(move |ui| {
                g(ui).get_tool_version() == "arch-update 9.9.9-test" && g(ui).get_can_check()
            }),
        ),
        (
            "Prüfung starten",
            Box::new(move |ui| click(ui, "Nach Updates suchen")),
        ),
        (
            "Prüfergebnis angezeigt",
            Box::new(move |ui| {
                g(ui).get_pending_total() == 3 && g(ui).get_can_install() && !g(ui).get_busy()
            }),
        ),
        (
            "Dashboard-Werte",
            Box::new(move |ui| {
                assert_eq!(g(ui).get_state_title(), "3 Update(s) verfügbar");
                assert_eq!(g(ui).get_tool_package(), "Paket cachy-update 9.9.9-1");
                assert_eq!(g(ui).get_reboot_text(), "Kein Neustart erforderlich");
                assert_eq!(g(ui).get_packages().row_count(), 3);
                true
            }),
        ),
        (
            "Installation starten",
            Box::new(move |ui| click(ui, "Update installieren")),
        ),
        (
            "Bestätigungsabfrage erscheint",
            Box::new(move |ui| {
                g(ui).get_page() == 1 && g(ui).get_session_active() && g(ui).get_prompt_kind() == 1
            }),
        ),
        (
            "Schließen während des Laufs wird abgefangen",
            Box::new(move |ui| {
                ui.window().dispatch_event(WindowEvent::CloseRequested);
                assert!(g(ui).get_close_dialog_visible());
                assert!(
                    g(ui).get_close_dialog_can_cancel(),
                    "vor Paketänderungen abbrechbar"
                );
                assert!(ui.window().is_visible());
                click(ui, "Weiter warten")
            }),
        ),
        (
            "Update bestätigen",
            Box::new(move |ui| {
                assert!(!g(ui).get_close_dialog_visible());
                click(ui, "Ja")
            }),
        ),
        (
            "Passwortabfrage",
            Box::new(move |ui| {
                if g(ui).get_prompt_kind() != 2 {
                    return false;
                }
                g(ui).invoke_send_input("secret".into());
                true
            }),
        ),
        (
            "pacman-Bestätigung",
            Box::new(move |ui| {
                g(ui).get_prompt_kind() == 1
                    && g(ui)
                        .get_prompt_text()
                        .contains("Proceed with installation")
                    && click(ui, "Ja")
            }),
        ),
        (
            "Während Paketänderungen kein Abbruch",
            Box::new(move |ui| {
                if !g(ui).get_session_active() {
                    return true; // Fake war schneller – Prüfung entfällt.
                }
                if g(ui).get_can_cancel() {
                    return false;
                }
                ui.window().dispatch_event(WindowEvent::CloseRequested);
                let blocked =
                    g(ui).get_close_dialog_visible() && !g(ui).get_close_dialog_can_cancel();
                g(ui).set_close_dialog_visible(false);
                blocked || !g(ui).get_session_active()
            }),
        ),
        (
            "Lauf abgeschlossen",
            Box::new(move |ui| !g(ui).get_session_active() && g(ui).get_result_visible()),
        ),
        (
            "Ergebnis und Schritte",
            Box::new(move |ui| {
                assert_eq!(g(ui).get_result_title(), "Update erfolgreich abgeschlossen");
                assert!(!g(ui).get_error_visible());
                let steps = g(ui).get_steps();
                assert_eq!(steps.row_count(), 5);
                assert!(
                    steps.iter().all(|s| s.status == 2),
                    "alle Schritte erledigt"
                );
                assert!(
                    g(ui)
                        .get_terminal_text()
                        .contains("The update has been applied")
                );
                assert!(!g(ui).get_terminal_text().contains("secret"));
                true
            }),
        ),
        (
            "Protokollseite",
            Box::new(move |ui| {
                g(ui).set_page(2);
                let rows = g(ui).get_log_rows();
                rows.row_count() > 0 && rows.iter().any(|r| r.message.contains("upgrading"))
            }),
        ),
        (
            "Protokoll-Filter",
            Box::new(move |ui| {
                let rows = g(ui).get_log_rows();
                assert!(
                    rows.iter().all(|r| !r.message.contains("secret")),
                    "Passwort nie im Protokoll"
                );
                g(ui).set_log_show_info(false);
                g(ui).set_log_show_debug(false);
                g(ui).invoke_log_filter_changed();
                true
            }),
        ),
        (
            "Nur Warnungen/Fehler",
            Box::new(move |ui| g(ui).get_log_rows().iter().all(|r| r.level >= 2)),
        ),
        (
            "Theme umschalten und speichern",
            Box::new(move |ui| {
                g(ui).set_dark_theme(false);
                g(ui).invoke_settings_changed();
                !ui.global::<Theme>().get_dark()
            }),
        ),
        (
            "Schließen im Leerlauf erlaubt",
            Box::new(move |ui| {
                ui.window().dispatch_event(WindowEvent::CloseRequested);
                !g(ui).get_close_dialog_visible()
            }),
        ),
    ];

    let steps = Rc::new(RefCell::new((
        steps,
        0usize,
        Instant::now(),
        None::<String>,
    )));
    let timer = Timer::default();
    let weak = ui.as_weak();
    let state = steps.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(25), move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut s = state.borrow_mut();
        let (list, idx, started, failure) = &mut *s;
        if *idx >= list.len() {
            let _ = slint::quit_event_loop();
            return;
        }
        if started.elapsed() > Duration::from_secs(30) {
            *failure = Some(format!("Zeitüberschreitung bei Schritt „{}“", list[*idx].0));
            let _ = slint::quit_event_loop();
            return;
        }
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (list[*idx].1)(&ui)));
        match ok {
            Ok(true) => {
                *idx += 1;
                *started = Instant::now();
            }
            Ok(false) => {}
            Err(_) => {
                *failure = Some(format!("Assertion in Schritt „{}“", list[*idx].0));
                let _ = slint::quit_event_loop();
            }
        }
    });

    slint::run_event_loop().unwrap();
    timer.stop();
    bridge.shutdown();

    let (_, idx, _, failure) = &*steps.borrow();
    assert!(failure.is_none(), "{}", failure.as_ref().unwrap());
    assert!(*idx > 0);

    // Einstellungen wurden in die isolierte XDG-Konfiguration geschrieben.
    let saved = std::fs::read_to_string(&settings_file).unwrap();
    let saved: Settings = toml::from_str(&saved).unwrap();
    assert_eq!(saved.appearance.theme, ThemeSetting::Light);
    // Ergebnis des Laufs wurde persistiert.
    assert!(Path::new(&root.join("state/cashy-updater-gui/last_result.toml")).exists());
}
