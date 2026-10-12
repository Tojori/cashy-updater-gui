//! Linux-Pfade nach XDG-Konvention.
//!
//! Die Pfade von arch-update werden exakt so nachgebildet, wie sie die Skripte
//! (`/usr/bin/arch-update`, `lib/common.sh`, `lib/config.sh`) berechnen:
//! `${XDG_STATE_HOME:-$HOME/.local/state}/arch-update` usw. Leere Variablen
//! zählen dabei – wie bei `${VAR:-default}` – als nicht gesetzt.

use std::env;
use std::path::{Path, PathBuf};

use crate::error::{AppError, ErrorKind};

/// Interner Name von arch-update (auch wenn es als `cachy-update` aufgerufen wird).
pub const ARCH_UPDATE_NAME: &str = "arch-update";
/// Verzeichnisname der GUI unter den XDG-Basisverzeichnissen.
pub const APP_DIR_NAME: &str = "cashy-updater-gui";

/// Basisverzeichnisse der aktuellen Sitzung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgDirs {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub data_home: Option<PathBuf>,
    pub data_dirs: Vec<PathBuf>,
}

fn non_empty_var(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl XdgDirs {
    /// Liest die XDG-Variablen aus der Umgebung.
    pub fn from_env() -> Result<Self, AppError> {
        let home = non_empty_var("HOME").ok_or_else(|| {
            AppError::new(
                ErrorKind::ConfigInvalid,
                "Die Umgebungsvariable HOME ist nicht gesetzt.",
            )
        })?;
        Ok(Self::from_parts(
            home,
            non_empty_var("XDG_CONFIG_HOME"),
            non_empty_var("XDG_STATE_HOME"),
            non_empty_var("XDG_DATA_HOME"),
            env::var("XDG_DATA_DIRS").ok(),
        ))
    }

    pub fn from_parts(
        home: PathBuf,
        config_home: Option<PathBuf>,
        state_home: Option<PathBuf>,
        data_home: Option<PathBuf>,
        data_dirs: Option<String>,
    ) -> Self {
        let config_home = config_home.unwrap_or_else(|| home.join(".config"));
        let state_home = state_home.unwrap_or_else(|| home.join(".local/state"));
        let data_dirs = data_dirs
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string())
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        Self {
            home,
            config_home,
            state_home,
            data_home,
            data_dirs,
        }
    }

    /// Pfade, die arch-update selbst verwendet.
    pub fn arch_update(&self) -> ArchUpdatePaths {
        ArchUpdatePaths {
            state_dir: self.state_home.join(ARCH_UPDATE_NAME),
            config_file: self
                .config_home
                .join(ARCH_UPDATE_NAME)
                .join(format!("{ARCH_UPDATE_NAME}.conf")),
            // full_upgrade.sh: exec {fd_upgrade}> "/tmp/${name}.lock"
            lock_file: PathBuf::from(format!("/tmp/{ARCH_UPDATE_NAME}.lock")),
        }
    }

    /// Eigene Pfade der GUI.
    pub fn app(&self) -> AppPaths {
        let state = self.state_home.join(APP_DIR_NAME);
        AppPaths {
            settings_file: self.config_home.join(APP_DIR_NAME).join("settings.toml"),
            log_dir: state.join("logs"),
            export_dir: state.join("exports"),
            last_result_file: state.join("last_result.toml"),
            state_dir: state,
        }
    }

    /// Sucht die Desktop-Datei von arch-update in derselben Reihenfolge wie
    /// das Tray-Applet bzw. `lib/tray.sh` (XDG_DATA_HOME, ~/.local/share, Datenverzeichnisse).
    pub fn find_arch_update_desktop_file(&self) -> Option<PathBuf> {
        let file = format!("applications/{ARCH_UPDATE_NAME}.desktop");
        let mut candidates = Vec::new();
        if let Some(d) = &self.data_home {
            candidates.push(d.join(&file));
        }
        candidates.push(self.home.join(".local/share").join(&file));
        candidates.extend(self.data_dirs.iter().map(|d| d.join(&file)));
        candidates.into_iter().find(|p| p.is_file())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchUpdatePaths {
    pub state_dir: PathBuf,
    pub config_file: PathBuf,
    pub lock_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub settings_file: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    pub export_dir: PathBuf,
    pub last_result_file: PathBuf,
}

/// Sucht ein ausführbares Programm im `PATH` (ohne Shell).
pub fn find_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return is_executable(&p).then_some(p);
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_arch_update_defaults() {
        let x = XdgDirs::from_parts(PathBuf::from("/home/u"), None, None, None, None);
        let p = x.arch_update();
        assert_eq!(
            p.state_dir,
            PathBuf::from("/home/u/.local/state/arch-update")
        );
        assert_eq!(
            p.config_file,
            PathBuf::from("/home/u/.config/arch-update/arch-update.conf")
        );
        assert_eq!(p.lock_file, PathBuf::from("/tmp/arch-update.lock"));
        assert_eq!(
            x.app().settings_file,
            PathBuf::from("/home/u/.config/cashy-updater-gui/settings.toml")
        );
    }

    #[test]
    fn honours_xdg_overrides() {
        let x = XdgDirs::from_parts(
            PathBuf::from("/home/u"),
            Some(PathBuf::from("/cfg")),
            Some(PathBuf::from("/st")),
            None,
            Some(String::new()),
        );
        assert_eq!(x.arch_update().state_dir, PathBuf::from("/st/arch-update"));
        assert_eq!(x.app().log_dir, PathBuf::from("/st/cashy-updater-gui/logs"));
        assert_eq!(
            x.data_dirs,
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
    }
}
