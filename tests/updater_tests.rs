//! Adapter-Tests gegen das Fake-arch-update (keine Systemänderungen, kein Netzwerk).

mod common;

use cashy_updater_gui::error::ErrorKind;
use cashy_updater_gui::updater::{Backend, CancelToken, UpdateSource, UpgradeOptions};
use common::Env;

#[test]
fn tool_info_reads_version_and_owning_package() {
    let env = Env::new("success");
    let info = env.service().tool_info().unwrap();
    assert_eq!(info.version_line, "arch-update 9.9.9-test");
    assert_eq!(
        info.package,
        Some(("cachy-update".to_string(), "9.9.9-1".to_string()))
    );
}

#[test]
fn missing_tool_is_reported() {
    let mut env = Env::new("success");
    env.cfg.tool = None;
    let err = env.service().tool_info().unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);
    let err = env.service().check(&CancelToken::new()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);
}

#[test]
fn status_without_previous_check_invents_nothing() {
    let env = Env::new("success");
    let s = env.service().read_status().unwrap();
    assert!(s.check.pending.is_empty());
    assert!(s.check.last_check.is_none());
    assert!(s.check.last_update_run.is_none());
    assert!(s.last_transaction.is_none(), "pacman.log fehlt → unbekannt");
    assert_eq!(s.kernel.as_deref(), Some("6.0.0-test"));
    assert_eq!(s.reboot_pending, Some(false));
    assert_eq!(s.timer_enabled, Some(false));
    assert!(!s.instance_running);
}

#[test]
fn check_reads_state_files_written_by_arch_update() {
    let env = Env::new("success");
    let report = env.service().check(&CancelToken::new()).unwrap();
    assert!(!report.is_incomplete());
    let p = &report.state.pending;
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].name, "linux");
    assert_eq!(p[0].current_version.as_deref(), Some("6.17.1.arch1-1"));
    assert_eq!(p[0].new_version.as_deref(), Some("6.17.2.arch1-1"));
    assert_eq!(p[2].source, UpdateSource::Aur);
    assert!(report.state.last_check.is_some());
}

#[test]
fn check_with_source_error_is_marked_incomplete() {
    let env = Env::new("check_network");
    let report = env.service().check(&CancelToken::new()).unwrap();
    assert!(report.is_incomplete());
    let err = report.problem_error().unwrap();
    assert_eq!(err.kind, ErrorKind::SourceUnreachable);
    assert!(err.details.contains("Cannot fetch updates"));
}

#[test]
fn check_can_be_cancelled() {
    let env = Env::new("success").with_env("FAKE_DELAY", "10");
    let svc = env.service();
    let token = CancelToken::new();
    let t2 = token.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        t2.cancel();
    });
    let started = std::time::Instant::now();
    let err = svc.check(&token).unwrap_err();
    h.join().unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn news_are_parsed_from_arch_update_output() {
    let env = Env::new("success");
    let news = env.service().news(&CancelToken::new()).unwrap();
    assert_eq!(news.len(), 2);
    assert_eq!(news[1].title, "Valkey to replace Redis");
}

#[test]
fn timer_toggle_uses_existing_mechanisms() {
    let env = Env::new("success");
    let svc = env.service();
    svc.set_timer(true).unwrap();
    assert_eq!(svc.read_status().unwrap().timer_enabled, Some(true));
    svc.set_timer(false).unwrap();
    assert_eq!(svc.read_status().unwrap().timer_enabled, Some(false));
}

#[test]
fn reboot_pending_when_running_kernel_image_is_gone() {
    let env = Env::new("success");
    std::fs::remove_file(env.cfg.modules_root.join("6.0.0-test/vmlinuz")).unwrap();
    assert_eq!(
        env.service().read_status().unwrap().reboot_pending,
        Some(true)
    );
}

#[test]
fn upgrade_spec_uses_fixed_arguments_and_c_locale() {
    let env = Env::new("success");
    let svc = env.service();
    let spec = svc.upgrade_spec(UpgradeOptions { devel: true }).unwrap();
    assert_eq!(spec.args, vec!["--devel".to_string()]);
    assert!(spec.env.contains(&("LC_ALL".into(), "C.UTF-8".into())));
    assert!(spec.env_remove.contains(&"LANGUAGE".to_string()));
    assert!(
        svc.upgrade_spec(UpgradeOptions::default())
            .unwrap()
            .args
            .is_empty()
    );
}

#[test]
fn generate_config_uses_gen_config() {
    let env = Env::new("success");
    let svc = env.service();
    assert!(!svc.config_path().exists());
    svc.generate_config().unwrap();
    assert!(svc.config_path().exists());
}
