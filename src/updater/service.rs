//! Adapter zwischen GUI und dem installierten `cachy-update`/`arch-update`.
//!
//! Sämtliche Update-Aktionen werden an arch-update delegiert. Der Adapter
//! startet ausschließlich die dokumentierten Optionen (`-V`, `--check`,
//! `--check --enable`, `-n`, `--gen-config`, ohne Option bzw. `--devel`) mit
//! festen Argumenten und liest die von arch-update geschriebenen Statusdateien.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Local};
use regex::Regex;

use crate::error::{AppError, ErrorKind};
use crate::paths::{ArchUpdatePaths, XdgDirs, find_in_path};
use crate::updater::progress::strip_ansi;
use crate::updater::runner::{CancelToken, CommandOutput, CommandRunner, SystemRunner};
use crate::updater::session::SessionSpec;
use crate::updater::state_files::{CheckState, read_check_state};
use crate::updater::system;

/// Name der systemd-User-Unit aus dem cachy-update-Paket.
pub const TIMER_UNIT: &str = "arch-update.timer";
/// Quelle der News laut `lib/list_news.sh`.
pub const ARCH_NEWS_URL: &str = "https://archlinux.org/news/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub program: PathBuf,
    /// Ausgabe von `arch-update -V`, z. B. "arch-update 4.4.2".
    pub version_line: String,
    /// Besitzendes Paket laut pacman, z. B. ("cachy-update", "4.4.2-1").
    pub package: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSnapshot {
    pub check: CheckState,
    pub kernel: Option<String>,
    /// `None`: nicht ermittelbar oder (wie in arch-update) nicht anwendbar.
    pub reboot_pending: Option<bool>,
    pub last_transaction: Option<DateTime<Local>>,
    pub timer_enabled: Option<bool>,
    pub instance_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub state: CheckState,
    /// Fehlermeldungen (stderr) von arch-update/checkupdates/flatpak.
    pub problems: Vec<String>,
}

impl CheckReport {
    pub fn is_incomplete(&self) -> bool {
        !self.problems.is_empty()
    }

    /// Fehlerobjekt für unvollständige Prüfungen (sonst `None`).
    pub fn problem_error(&self) -> Option<AppError> {
        if self.problems.is_empty() {
            return None;
        }
        let joined = self.problems.join("\n");
        let lower = joined.to_lowercase();
        let kind = if lower.contains("cannot fetch updates") {
            ErrorKind::SourceUnreachable
        } else if lower.contains("could not resolve") || lower.contains("network") {
            ErrorKind::NetworkUnavailable
        } else {
            ErrorKind::CheckIncomplete
        };
        Some(
            AppError::new(
                kind,
                "Die Update-Prüfung hat Fehler gemeldet; das Ergebnis ist möglicherweise unvollständig.",
            )
            .with_details(joined),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    pub index: u32,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpgradeOptions {
    /// `arch-update --devel`
    pub devel: bool,
}

/// Schnittstelle der GUI zum Updater (Test-Doubles implementieren sie ebenfalls).
pub trait Backend: Send + Sync + 'static {
    fn tool_info(&self) -> Result<ToolInfo, AppError>;
    fn read_status(&self) -> Result<StatusSnapshot, AppError>;
    fn check(&self, cancel: &CancelToken) -> Result<CheckReport, AppError>;
    fn news(&self, cancel: &CancelToken) -> Result<Vec<NewsItem>, AppError>;
    fn set_timer(&self, enabled: bool) -> Result<(), AppError>;
    fn generate_config(&self) -> Result<(), AppError>;
    fn config_path(&self) -> PathBuf;
    fn instance_running(&self) -> bool;
    fn upgrade_spec(&self, opts: UpgradeOptions) -> Result<SessionSpec, AppError>;
    fn launch_in_terminal(&self) -> Result<(), AppError>;
}

/// Pfade und Programme, die der Service verwendet (in Tests ersetzbar).
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// `cachy-update` bzw. `arch-update`; `None`, wenn nicht installiert.
    pub tool: Option<PathBuf>,
    pub pacman: PathBuf,
    pub systemctl: PathBuf,
    pub detect_virt: PathBuf,
    pub gio: PathBuf,
    pub arch_paths: ArchUpdatePaths,
    pub desktop_file: Option<PathBuf>,
    pub pacman_log: PathBuf,
    pub modules_root: PathBuf,
    pub osrelease: PathBuf,
    pub home: PathBuf,
    /// Zusätzliche Umgebung für alle Aufrufe (Tests: XDG-Verzeichnisse).
    pub extra_env: Vec<(String, String)>,
}

impl ServiceConfig {
    pub fn detect(xdg: &XdgDirs) -> Self {
        let prog = |name: &str, fallback: &str| {
            find_in_path(name).unwrap_or_else(|| PathBuf::from(fallback))
        };
        Self {
            tool: find_in_path("cachy-update").or_else(|| find_in_path("arch-update")),
            pacman: prog("pacman", "/usr/bin/pacman"),
            systemctl: prog("systemctl", "/usr/bin/systemctl"),
            detect_virt: prog("systemd-detect-virt", "/usr/bin/systemd-detect-virt"),
            gio: prog("gio", "/usr/bin/gio"),
            arch_paths: xdg.arch_update(),
            desktop_file: xdg.find_arch_update_desktop_file(),
            pacman_log: PathBuf::from("/var/log/pacman.log"),
            modules_root: PathBuf::from("/usr/lib/modules"),
            osrelease: PathBuf::from("/proc/sys/kernel/osrelease"),
            home: xdg.home.clone(),
            extra_env: Vec::new(),
        }
    }
}

/// Feste Umgebung für maschinenlesbare Ausgaben.
fn c_locale_env() -> Vec<(String, String)> {
    vec![
        ("LC_ALL".into(), "C.UTF-8".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ]
}

pub struct UpdaterService<R: CommandRunner = SystemRunner> {
    cfg: ServiceConfig,
    runner: R,
}

impl<R: CommandRunner> UpdaterService<R> {
    pub fn new(cfg: ServiceConfig, runner: R) -> Self {
        Self { cfg, runner }
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.cfg
    }

    fn tool(&self) -> Result<&Path, AppError> {
        self.cfg.tool.as_deref().ok_or_else(|| {
            AppError::new(
                ErrorKind::ToolMissing,
                "Weder „cachy-update“ noch „arch-update“ wurde im PATH gefunden.",
            )
        })
    }

    fn run(
        &self,
        program: &Path,
        args: &[&str],
        c_locale: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<CommandOutput, AppError> {
        let mut env = self.cfg.extra_env.clone();
        if c_locale {
            env.extend(c_locale_env());
        }
        tracing::debug!("Starte {} {:?}", program.display(), args);
        let out = self
            .runner
            .run(program, args, &env, true, cancel)
            .map_err(|e| {
                let kind = if e.kind() == std::io::ErrorKind::NotFound {
                    ErrorKind::ToolMissing
                } else {
                    ErrorKind::Internal
                };
                AppError::new(
                    kind,
                    format!("{} konnte nicht gestartet werden.", program.display()),
                )
                .with_details(e.to_string())
            })?;
        if out.cancelled {
            return Err(AppError::new(ErrorKind::Cancelled, ""));
        }
        Ok(out)
    }

    fn package_of(&self, path: &Path) -> Option<(String, String)> {
        static OWNED: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"is owned by (\S+) (\S+)").unwrap());
        let p = path.to_string_lossy();
        let out = self.run(&self.cfg.pacman, &["-Qo", &p], true, None).ok()?;
        let caps = OWNED.captures(&out.stdout)?;
        Some((caps[1].to_string(), caps[2].to_string()))
    }

    fn reboot_pending(&self, release: &str) -> Option<bool> {
        // Ausnahmen exakt wie in full_upgrade.sh.
        for pkg in ["kernel-modules-hook", "mkmm"] {
            if self
                .run(&self.cfg.pacman, &["-Q", pkg], true, None)
                .is_ok_and(|o| o.success())
            {
                return None;
            }
        }
        if self
            .run(
                &self.cfg.detect_virt,
                &["--container", "--quiet"],
                true,
                None,
            )
            .is_ok_and(|o| o.success())
        {
            return None;
        }
        Some(system::kernel_reboot_pending(
            &self.cfg.modules_root,
            release,
        ))
    }

    fn timer_enabled(&self) -> Option<bool> {
        let out = self
            .run(
                &self.cfg.systemctl,
                &["--user", "is-enabled", TIMER_UNIT],
                true,
                None,
            )
            .ok()?;
        match out.stdout.trim() {
            "enabled" | "enabled-runtime" => Some(true),
            "disabled" => Some(false),
            _ => None,
        }
    }

    fn read_state(&self) -> Result<CheckState, AppError> {
        read_check_state(&self.cfg.arch_paths.state_dir).map_err(|e| {
            AppError::new(
                ErrorKind::Internal,
                format!(
                    "Statusdateien in {} konnten nicht gelesen werden.",
                    self.cfg.arch_paths.state_dir.display()
                ),
            )
            .with_details(e.to_string())
        })
    }
}

fn non_empty_lines(text: &str) -> Vec<String> {
    strip_ansi(text)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parst die Ausgabe von `arch-update -n` (`<n> - <Titel>`).
pub fn parse_news(output: &str) -> Vec<NewsItem> {
    static NEWS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+) - (.+)$").unwrap());
    strip_ansi(output)
        .lines()
        .filter_map(|l| {
            let c = NEWS.captures(l.trim())?;
            Some(NewsItem {
                index: c[1].parse().ok()?,
                title: c[2].trim().to_string(),
            })
        })
        .collect()
}

impl<R: CommandRunner + 'static> Backend for UpdaterService<R> {
    fn tool_info(&self) -> Result<ToolInfo, AppError> {
        let tool = self.tool()?;
        let out = self.run(tool, &["-V"], true, None)?;
        if !out.success() {
            return Err(AppError::new(
                ErrorKind::Internal,
                "Version konnte nicht ermittelt werden.",
            )
            .with_details(format!("Exit-Code {:?}\n{}", out.code, out.stderr)));
        }
        Ok(ToolInfo {
            program: tool.to_path_buf(),
            version_line: strip_ansi(out.stdout.trim()),
            package: self.package_of(tool),
        })
    }

    fn read_status(&self) -> Result<StatusSnapshot, AppError> {
        let kernel = system::running_kernel(&self.cfg.osrelease);
        Ok(StatusSnapshot {
            check: self.read_state()?,
            reboot_pending: kernel.as_deref().and_then(|k| self.reboot_pending(k)),
            kernel,
            last_transaction: system::last_completed_transaction(&self.cfg.pacman_log),
            timer_enabled: self.timer_enabled(),
            instance_running: self.instance_running(),
        })
    }

    fn check(&self, cancel: &CancelToken) -> Result<CheckReport, AppError> {
        let tool = self.tool()?;
        // Benutzer-Locale beibehalten: arch-update sendet ggf. eine Desktop-Benachrichtigung.
        let out = self.run(tool, &["--check"], false, Some(cancel))?;
        if out.code != Some(0) {
            return Err(AppError::new(
                ErrorKind::Internal,
                format!(
                    "arch-update --check wurde mit Exit-Code {:?} beendet.",
                    out.code
                ),
            )
            .with_details(format!("{}\n{}", out.stdout, out.stderr).trim().to_string()));
        }
        // checkupdates meldet Fehler nur auf stderr; der Exit-Code von --check bleibt 0.
        Ok(CheckReport {
            state: self.read_state()?,
            problems: non_empty_lines(&out.stderr),
        })
    }

    fn news(&self, cancel: &CancelToken) -> Result<Vec<NewsItem>, AppError> {
        let tool = self.tool()?;
        // stdin=/dev/null: die Auswahlabfrage endet sofort ohne Eingabe.
        let out = self.run(tool, &["--news"], true, Some(cancel))?;
        if out.stdout.contains("Unable to retrieve recent Arch News") {
            return Err(AppError::new(
                ErrorKind::SourceUnreachable,
                "Die Arch-News konnten nicht abgerufen werden.",
            )
            .with_details(strip_ansi(&out.stdout)));
        }
        Ok(parse_news(&out.stdout))
    }

    fn set_timer(&self, enabled: bool) -> Result<(), AppError> {
        let out = if enabled {
            // Vorhandener Mechanismus: lib/check.sh mit --enable.
            self.run(self.tool()?, &["--check", "--enable"], true, None)?
        } else {
            self.run(
                &self.cfg.systemctl,
                &["--user", "disable", "--now", TIMER_UNIT],
                true,
                None,
            )?
        };
        if out.success() {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorKind::Internal,
                "Die automatische Update-Prüfung konnte nicht geändert werden.",
            )
            .with_details(strip_ansi(&format!("{}\n{}", out.stdout, out.stderr))))
        }
    }

    fn generate_config(&self) -> Result<(), AppError> {
        let out = self.run(self.tool()?, &["--gen-config"], true, None)?;
        if out.success() {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorKind::ConfigInvalid,
                "arch-update.conf konnte nicht erzeugt werden.",
            )
            .with_details(strip_ansi(&format!("{}\n{}", out.stdout, out.stderr))))
        }
    }

    fn config_path(&self) -> PathBuf {
        self.cfg.arch_paths.config_file.clone()
    }

    fn instance_running(&self) -> bool {
        system::lock_is_held(&self.cfg.arch_paths.lock_file)
    }

    fn upgrade_spec(&self, opts: UpgradeOptions) -> Result<SessionSpec, AppError> {
        let tool = self.tool()?;
        let mut env = self.cfg.extra_env.clone();
        env.extend(c_locale_env());
        env.push(("TERM".into(), "xterm-256color".into()));
        Ok(SessionSpec {
            program: tool.to_path_buf(),
            args: if opts.devel {
                vec!["--devel".into()]
            } else {
                Vec::new()
            },
            env,
            env_remove: vec!["LANGUAGE".into()],
            cwd: Some(self.cfg.home.clone()),
            cols: 120,
            rows: 40,
        })
    }

    fn launch_in_terminal(&self) -> Result<(), AppError> {
        let desktop = self.cfg.desktop_file.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorKind::ToolMissing,
                "arch-update.desktop wurde nicht gefunden.",
            )
        })?;
        let d = desktop.to_string_lossy();
        // Gleicher Weg wie das Tray-Applet (GDesktopAppInfo → Terminal=true).
        let out = self.run(&self.cfg.gio, &["launch", &d], false, None)?;
        if out.success() {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorKind::Internal,
                "Terminal konnte nicht gestartet werden.",
            )
            .with_details(out.stderr))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_news_list() {
        let out = "\x1b[1m==> Looking for recent Arch News...\x1b[0m\n\n==> Arch News:\n1 - Mkinitcpio >=42 requires manual intervention\n2 - Something [NEW]\n\n-> Select the news to read (e.g. 1 3 5)";
        let n = parse_news(out);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].index, 1);
        assert_eq!(n[1].title, "Something [NEW]");
    }

    #[test]
    fn check_problems_are_classified() {
        let r = CheckReport {
            state: CheckState::default(),
            problems: vec!["==> ERROR: Cannot fetch updates".into()],
        };
        assert!(r.is_incomplete());
        assert_eq!(
            r.problem_error().unwrap().kind,
            ErrorKind::SourceUnreachable
        );
        let ok = CheckReport {
            state: CheckState::default(),
            problems: vec![],
        };
        assert!(ok.problem_error().is_none());
    }
}
