//! Gemeinsame Hilfen für Integrationstests mit dem Fake-arch-update.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use cashy_updater_gui::paths::XdgDirs;
use cashy_updater_gui::ui::controller::Controller;
use cashy_updater_gui::updater::{
    Outcome, Prompt, ServiceConfig, SessionEvent, SystemRunner, UpdaterService, UpgradeSession,
};
use tempfile::TempDir;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin")
}

pub struct Env {
    pub dir: TempDir,
    pub cfg: ServiceConfig,
}

impl Env {
    /// Isolierte XDG-Umgebung mit Fake-Programmen. `scenario` steuert das Fake.
    pub fn new(scenario: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let xdg = XdgDirs::from_parts(
            home.clone(),
            Some(dir.path().join("config")),
            Some(dir.path().join("state")),
            None,
            Some(dir.path().join("share").to_string_lossy().into_owned()),
        );
        let bin = fixtures();
        let mut arch_paths = xdg.arch_update();
        arch_paths.lock_file = dir.path().join("arch-update.lock");
        let modules = dir.path().join("modules");
        std::fs::create_dir_all(modules.join("6.0.0-test")).unwrap();
        std::fs::write(modules.join("6.0.0-test/vmlinuz"), b"").unwrap();
        let osrelease = dir.path().join("osrelease");
        std::fs::write(&osrelease, "6.0.0-test\n").unwrap();
        let cfg = ServiceConfig {
            tool: Some(bin.join("cachy-update")),
            pacman: bin.join("pacman"),
            systemctl: bin.join("systemctl"),
            detect_virt: bin.join("systemd-detect-virt"),
            gio: PathBuf::from("/bin/false"),
            arch_paths,
            desktop_file: None,
            pacman_log: dir.path().join("pacman.log"),
            modules_root: modules,
            osrelease,
            home: home.clone(),
            extra_env: vec![
                ("HOME".into(), home.to_string_lossy().into_owned()),
                (
                    "XDG_STATE_HOME".into(),
                    dir.path().join("state").to_string_lossy().into_owned(),
                ),
                (
                    "XDG_CONFIG_HOME".into(),
                    dir.path().join("config").to_string_lossy().into_owned(),
                ),
                (
                    "XDG_RUNTIME_DIR".into(),
                    dir.path().to_string_lossy().into_owned(),
                ),
                ("FAKE_SCENARIO".into(), scenario.into()),
                (
                    "FAKE_TIMER_STATE".into(),
                    dir.path().join("timer").to_string_lossy().into_owned(),
                ),
            ],
        };
        Self { dir, cfg }
    }

    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.cfg.extra_env.push((key.into(), value.into()));
        self
    }

    pub fn service(&self) -> UpdaterService {
        UpdaterService::new(
            self.cfg.clone(),
            SystemRunner {
                timeout: Some(Duration::from_secs(30)),
            },
        )
    }

    pub fn controller(&self) -> Controller<UpdaterService> {
        Controller::new(Arc::new(self.service()), None)
    }
}

/// Ergebnis eines simulierten GUI-Update-Laufs.
pub struct RunResult {
    pub outcome: Outcome,
    pub log_lines: Vec<String>,
    pub prompts: Vec<Prompt>,
    pub inputs: Vec<String>,
}

/// Führt einen Update-Lauf wie die GUI aus: PTY-Sitzung, Ausgabe → Controller,
/// Antworten auf Abfragen über `answer` (None = nicht antworten).
pub fn drive_upgrade(
    c: &mut Controller<UpdaterService>,
    mut answer: impl FnMut(&Prompt, &mut Controller<UpdaterService>) -> Option<String>,
) -> RunResult {
    let spec = c
        .begin_upgrade(Default::default())
        .expect("Upgrade muss startbar sein");
    let (tx, rx) = mpsc::channel();
    let mut session = UpgradeSession::start(&spec, move |ev| {
        let _ = tx.send(ev);
    })
    .expect("PTY-Sitzung");
    let mut log_lines = Vec::new();
    let mut prompts = Vec::new();
    let mut inputs = Vec::new();
    loop {
        let ev = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("Zeitüberschreitung: arch-update-Fake reagiert nicht");
        match ev {
            SessionEvent::Output(chunk) => {
                let fx = c.on_output(&chunk);
                log_lines.extend(fx.lines);
                if let Some(reply) = fx.auto_reply {
                    session.send_line(&reply).unwrap();
                }
                if let Some(p) = c.prompt().cloned()
                    && let Some(text) = answer(&p, c)
                {
                    prompts.push(p);
                    let input = c.take_input(&text).unwrap();
                    session.send_line(&input).unwrap();
                    inputs.push(text);
                }
            }
            SessionEvent::Exited(code) => {
                let outcome = c.on_session_exit(code);
                return RunResult {
                    outcome,
                    log_lines,
                    prompts,
                    inputs,
                };
            }
        }
    }
}

/// Standardantworten: Update bestätigen, Passwort `secret`, Installation bestätigen.
pub fn default_answers(p: &Prompt, _: &mut Controller<UpdaterService>) -> Option<String> {
    match p {
        Prompt::YesNo { text, .. } if text.contains("reboot") => Some("n".into()),
        Prompt::YesNo { .. } => Some("y".into()),
        Prompt::Password { .. } => Some("secret".into()),
        _ => None,
    }
}
