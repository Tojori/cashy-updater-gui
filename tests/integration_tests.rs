//! Controller ↔ Update-Service ↔ echte PTY-Sitzung mit Fake-arch-update.

mod common;

use std::fs::File;
use std::os::fd::AsRawFd;

use cashy_updater_gui::error::ErrorKind;
use cashy_updater_gui::logging::{self, LogFilter, LogLevel, LogStore};
use cashy_updater_gui::state::UpdateState;
use cashy_updater_gui::ui::controller::{CancelAction, ClosePolicy, Tone};
use cashy_updater_gui::updater::{Backend, CancelToken, OutcomeKind, Prompt, Step, StepStatus};
use common::{Env, default_answers, drive_upgrade};

fn checked_controller(
    env: &Env,
) -> cashy_updater_gui::ui::controller::Controller<cashy_updater_gui::updater::UpdaterService> {
    let mut c = env.controller();
    c.on_tool_info(c.backend().tool_info());
    let token = c.begin_check().unwrap();
    let report = c.backend().check(&token);
    c.on_check_finished(report);
    c.on_status(c.backend().read_status());
    c
}

#[test]
fn check_flow_updates_state_and_dashboard() {
    let env = Env::new("success");
    let c = checked_controller(&env);
    assert_eq!(c.state.update_state, UpdateState::UpdateAvailable(3));
    assert!(c.can_install());
    assert_eq!(c.badge_count(), Some(3));
    let d = c.dashboard();
    assert_eq!(d.pending_total, 3);
    assert_eq!(d.pending_pacman, 2);
    assert_eq!(d.pending_aur, 1);
    assert_eq!(d.tool_version, "arch-update 9.9.9-test");
    assert_eq!(d.last_install, "Kein Lauf über diese App");
    assert_eq!(d.last_transaction, "Unbekannt");
}

#[test]
fn incomplete_check_never_claims_up_to_date() {
    let env = Env::new("check_network");
    let c = checked_controller(&env);
    assert!(matches!(
        c.state.update_state,
        UpdateState::CheckIncomplete(_)
    ));
    assert_eq!(c.error().unwrap().kind, ErrorKind::SourceUnreachable);
    assert_eq!(c.dashboard().state_tone, Tone::Error);
}

#[test]
fn successful_upgrade_through_pty() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let run = drive_upgrade(&mut c, default_answers);

    assert_eq!(
        run.outcome.kind,
        OutcomeKind::Completed,
        "{:#?}",
        run.outcome
    );
    assert_eq!(run.outcome.exit_code, Some(0));
    assert!(!run.outcome.reboot_required);
    for (step, status) in c.steps() {
        assert_eq!(status, StepStatus::Done, "Schritt {step:?}");
    }
    // Abfragen: Bestätigung, Passwort, pacman-Bestätigung (quit wird automatisch beantwortet)
    assert!(matches!(
        run.prompts[0],
        Prompt::YesNo {
            default_yes: true,
            ..
        }
    ));
    assert!(matches!(run.prompts[1], Prompt::Password { .. }));
    assert!(matches!(run.prompts[2], Prompt::YesNo { .. }));
    assert_eq!(run.prompts.len(), 3);
    // Das Passwort taucht nirgends in der protokollierten Ausgabe auf.
    assert!(run.log_lines.iter().all(|l| !l.contains("secret")));
    // Fortschrittsbalken unter 100 % werden nicht protokolliert.
    assert!(run.log_lines.iter().all(|l| !l.contains(" 30%")));
    assert!(
        run.log_lines
            .iter()
            .any(|l| l.contains("The update has been applied"))
    );
    assert!(!c.state.is_busy());
    assert_eq!(
        c.state.update_state,
        UpdateState::Finished(OutcomeKind::Completed)
    );
    assert!(c.last_run().unwrap().success);
    assert_eq!(c.badge_count(), Some(0), "nach Erfolg kein Badge");
}

#[test]
fn reboot_request_is_reported() {
    let env = Env::new("success").with_env("FAKE_REBOOT", "1");
    let mut c = checked_controller(&env);
    let run = drive_upgrade(&mut c, default_answers);
    assert_eq!(run.outcome.kind, OutcomeKind::Completed);
    assert!(run.outcome.reboot_required);
    assert!(
        run.inputs.contains(&"n".to_string()),
        "Neustart wurde abgelehnt"
    );
}

#[test]
fn integrity_failure_is_reported_as_failure() {
    let env = Env::new("integrity");
    let mut c = checked_controller(&env);
    let run = drive_upgrade(&mut c, default_answers);
    assert_eq!(
        run.outcome.kind,
        OutcomeKind::Failed(ErrorKind::IntegrityFailed)
    );
    assert_eq!(run.outcome.exit_code, Some(5));
    let steps = c.steps();
    assert_eq!(steps[Step::Verify as usize].1, StepStatus::Failed);
    assert_eq!(steps[Step::Install as usize].1, StepStatus::Pending);
    assert_eq!(c.error().unwrap().kind, ErrorKind::IntegrityFailed);
    assert!(!c.last_run().unwrap().success);
    assert_eq!(c.badge_count(), Some(3), "Updates bleiben ausstehend");
}

#[test]
fn wrong_password_is_permission_error() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let run = drive_upgrade(&mut c, |p, c| match p {
        Prompt::Password { .. } => Some("falsch".into()),
        other => default_answers(other, c),
    });
    assert_eq!(
        run.outcome.kind,
        OutcomeKind::Failed(ErrorKind::PermissionDenied)
    );
}

#[test]
fn cancel_at_confirmation_sends_no_and_reports_cancelled() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let mut action = None;
    let run = drive_upgrade(&mut c, |p, c| {
        if let Prompt::YesNo { text, .. } = p
            && text.contains("Proceed with update?")
        {
            assert!(c.is_cancellable());
            let a = c.request_cancel();
            action = Some(a.clone());
            if let CancelAction::SendLine(s) = a {
                return Some(s);
            }
        }
        None
    });
    assert_eq!(action, Some(CancelAction::SendLine("n".into())));
    assert_eq!(run.outcome.kind, OutcomeKind::Cancelled);
    assert_eq!(run.outcome.exit_code, Some(4));
}

#[test]
fn cancel_is_refused_once_packages_change() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let mut refused = false;
    let run = drive_upgrade(&mut c, |p, c| {
        if let Prompt::YesNo { text, .. } = p
            && text.contains("Proceed with installation")
        {
            // pacman läuft bereits (Datenbanken synchronisiert) → kein Abbruch mehr.
            refused = matches!(c.request_cancel(), CancelAction::NotAllowed(_));
            assert!(matches!(
                c.close_policy(),
                ClosePolicy::Block { cancellable: false }
            ));
        }
        default_answers(p, c)
    });
    assert!(refused);
    assert_eq!(run.outcome.kind, OutcomeKind::Completed);
}

#[test]
fn concurrent_operations_are_rejected() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let _token = c.begin_check().unwrap();
    assert_eq!(
        c.badge_count(),
        None,
        "während der Prüfung bleibt das Badge stehen"
    );
    assert_eq!(c.begin_check().unwrap_err().kind, ErrorKind::Busy);
    assert_eq!(
        c.begin_upgrade(Default::default()).unwrap_err().kind,
        ErrorKind::Busy
    );
    assert!(!c.can_install());
    assert_eq!(c.close_policy(), ClosePolicy::AllowCancellingCheck);
    c.on_check_finished(Err(cashy_updater_gui::error::AppError::new(
        ErrorKind::Cancelled,
        "",
    )));
    assert!(!c.state.is_busy());
    assert_eq!(c.state.update_state, UpdateState::UpdateAvailable(3));
}

#[test]
fn foreign_arch_update_lock_blocks_installation() {
    let env = Env::new("success");
    let mut c = checked_controller(&env);
    let lock = File::create(&env.cfg.arch_paths.lock_file).unwrap();
    // SAFETY: Testdatei
    unsafe {
        assert_eq!(
            libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB),
            0
        );
    }
    let err = c.begin_upgrade(Default::default()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::AlreadyRunning);
    assert!(!c.state.is_busy());
    c.on_status(c.backend().read_status());
    assert!(c.dashboard().external_running);
    assert!(!c.can_install());
}

#[test]
fn up_to_date_system_does_not_offer_installation() {
    let env = Env::new("none");
    let mut c = checked_controller(&env);
    assert_eq!(c.state.update_state, UpdateState::UpToDate);
    assert_eq!(c.badge_count(), Some(0));
    assert!(!c.can_install());
    assert!(c.begin_upgrade(Default::default()).is_err());
}

#[test]
fn news_flow() {
    let env = Env::new("success");
    let mut c = env.controller();
    c.on_tool_info(c.backend().tool_info());
    let token = c.begin_news().unwrap();
    assert!(c.begin_news().is_none(), "kein doppelter Abruf");
    c.on_news(c.backend().news(&token));
    assert_eq!(c.news().len(), 2);
    assert!(!c.news_loading());
    let _ = CancelToken::new();
}

#[test]
fn logging_store_and_export() {
    let store = LogStore::new(100);
    let dir = tempfile::tempdir().unwrap();
    let file = store.attach_file(&dir.path().join("logs")).unwrap();
    store.push(LogLevel::Info, "arch-update", "(1/2) upgrading linux");
    store.push(
        LogLevel::Error,
        "arch-update",
        "error: failed to commit transaction",
    );
    store.detach_file();
    let written = std::fs::read_to_string(&file).unwrap();
    assert_eq!(written.lines().count(), 2);
    assert!(written.contains("[ERROR] arch-update: error: failed to commit transaction"));

    let errors = LogFilter {
        show_info: false,
        ..LogFilter::default()
    };
    let (entries, total) = store.query(&errors, 0, usize::MAX);
    assert_eq!(total, 1);
    let path = logging::export_entries(&entries, &dir.path().join("exports")).unwrap();
    let exported = std::fs::read_to_string(&path).unwrap();
    assert_eq!(exported.lines().count(), 1);
    assert!(exported.contains("failed to commit"));
    // Zweiter Export überschreibt nichts.
    let path2 = logging::export_entries(&entries, &dir.path().join("exports")).unwrap();
    assert_ne!(path, path2);
}
