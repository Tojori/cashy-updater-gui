//! Laden/Speichern der Einstellungen und Bearbeiten der arch-update.conf.

use std::fs;

use cashy_updater_gui::config::settings::{self, LoadOutcome, MAX_LOG_ENTRIES};
use cashy_updater_gui::config::{ArchUpdateConf, Settings, Theme};
use cashy_updater_gui::error::ErrorKind;

#[test]
fn missing_settings_file_uses_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = settings::load(&dir.path().join("settings.toml"));
    assert_eq!(outcome, LoadOutcome::Missing(Settings::default()));
}

#[test]
fn corrupt_settings_file_is_backed_up_and_defaults_used() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, "[appearance\ntheme = 42").unwrap();
    match settings::load(&path) {
        LoadOutcome::Corrupt {
            settings,
            error,
            backup,
        } => {
            assert_eq!(settings, Settings::default());
            assert!(!error.is_empty());
            let backup = backup.expect("Sicherung angelegt");
            assert!(backup.exists());
            assert!(!path.exists(), "defekte Datei wurde verschoben");
        }
        other => panic!("unerwartet: {other:?}"),
    }
}

#[test]
fn wrong_types_are_treated_as_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, "[appearance]\ntheme = \"neon\"\n").unwrap();
    assert!(matches!(settings::load(&path), LoadOutcome::Corrupt { .. }));
}

#[test]
fn out_of_range_values_are_corrected_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, "[logging]\nmax_entries = 99999999\n").unwrap();
    match settings::load(&path) {
        LoadOutcome::Loaded { settings, notes } => {
            assert_eq!(settings.logging.max_entries, MAX_LOG_ENTRIES);
            assert_eq!(notes.len(), 1);
        }
        other => panic!("unerwartet: {other:?}"),
    }
}

#[test]
fn settings_roundtrip_via_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/settings.toml");
    let mut s = Settings::default();
    s.appearance.theme = Theme::Light;
    s.behavior.check_on_startup = true;
    s.upgrade.include_devel = true;
    settings::save(&path, &s).unwrap();
    match settings::load(&path) {
        LoadOutcome::Loaded { settings, notes } => {
            assert_eq!(settings, s);
            assert!(notes.is_empty());
        }
        other => panic!("unerwartet: {other:?}"),
    }
    // Keine temporären Dateien zurückgelassen.
    let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn arch_update_conf_edit_roundtrip_preserves_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("arch-update.conf");
    let example = fs::read_to_string("/usr/share/arch-update/config/arch-update.conf.example")
        .unwrap_or_else(|_| "# Beispiel\n#NoAUR\n#NewsNum=5\n#AURHelper=paru\n".into());
    fs::write(&path, &example).unwrap();

    let mut conf = ArchUpdateConf::load(&path).unwrap().unwrap();
    conf.set_flag("NoAUR", true).unwrap();
    conf.set_value("NewsNum", Some("8")).unwrap();
    conf.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let reloaded = ArchUpdateConf::load(&path).unwrap().unwrap();
    assert!(reloaded.flag("NoAUR"));
    assert_eq!(reloaded.value("NewsNum").as_deref(), Some("8"));
    // Alle Kommentarzeilen außer den geänderten bleiben erhalten.
    let original_comments = example.lines().filter(|l| l.starts_with("# ")).count();
    assert_eq!(
        text.lines().filter(|l| l.starts_with("# ")).count(),
        original_comments
    );
    assert_eq!(text.lines().count(), example.lines().count());
}

#[test]
fn arch_update_conf_rejects_invalid_values() {
    let mut conf = ArchUpdateConf::parse("");
    let e = conf
        .set_value("UpdateCheckTimeout", Some("-1"))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::ConfigInvalid);
    assert!(
        ArchUpdateConf::load(&std::path::PathBuf::from("/nonexistent/x.conf"))
            .unwrap()
            .is_none()
    );
}
