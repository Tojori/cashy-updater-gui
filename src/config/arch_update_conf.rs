//! Lesen und gezieltes Ändern der vorhandenen `arch-update.conf`.
//!
//! Die Datei gehört arch-update; ihr Format wird in `lib/config.sh` per
//! `grep -E` ausgewertet. Dieser Editor
//! * erkennt Optionen mit denselben regulären Ausdrücken wie `config.sh`,
//! * ändert nur die betroffenen Zeilen (Kommentare/Reihenfolge bleiben erhalten),
//! * akzeptiert ausschließlich Werte, die `config.sh` ebenfalls akzeptiert.
//!
//! `DiffProg` wird bewusst nicht angeboten: der Wert ist ein frei ausführbarer
//! Befehl und das pacnew-Modul ist in cachy-update deaktiviert.

use std::fs;
use std::path::Path;

use regex::Regex;

use crate::config::settings::write_atomic;
use crate::error::{AppError, ErrorKind};

/// Schalter-Optionen (Zeile besteht nur aus dem Namen).
pub const FLAG_KEYS: &[&str] = &[
    "NoColor",
    "NoVersion",
    "NoAUR",
    "NoFlatpak",
    "NoNotification",
    "NoALHPCheck",
    "ShowTrayWhenRelevant",
    "ColorblindMode",
];

/// Werte-Optionen mit dem in `config.sh` verwendeten Wertemuster und Standardwert.
pub const VALUE_KEYS: &[(&str, &str, &str)] = &[
    ("NewsNum", "[1-9][0-9]*", "5"),
    ("NewsTimeout", "[0-9][0-9]*", "10"),
    ("UpdateCheckTimeout", "[0-9][0-9]*", "30"),
    ("AURHelper", "(paru|yay|pikaur)", ""),
    ("PrivilegeElevationCommand", "(sudo|sudo-rs|doas|run0)", ""),
    ("KeepOldPackages", "[0-9]+", "3"),
    ("KeepUninstalledPackages", "[0-9]+", "0"),
    ("TrayUpdatesPerPage", "[0-9][0-9]*", "0"),
    ("TrayIconStyle", "(blue|light|dark)", "blue"),
];

fn value_spec(key: &str) -> Option<(&'static str, &'static str)> {
    VALUE_KEYS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, pat, def)| (*pat, *def))
}

fn flag_re(key: &str) -> Regex {
    Regex::new(&format!(
        r"^[[:space:]]*{}[[:space:]]*$",
        regex::escape(key)
    ))
    .unwrap()
}

fn commented_flag_re(key: &str) -> Regex {
    Regex::new(&format!(
        r"^[[:space:]]*#[[:space:]]*{}[[:space:]]*$",
        regex::escape(key)
    ))
    .unwrap()
}

fn any_value_re(key: &str) -> Regex {
    Regex::new(&format!(
        r"^[[:space:]]*{}[[:space:]]*=",
        regex::escape(key)
    ))
    .unwrap()
}

fn commented_value_re(key: &str) -> Regex {
    Regex::new(&format!(
        r"^[[:space:]]*#[[:space:]]*{}[[:space:]]*=",
        regex::escape(key)
    ))
    .unwrap()
}

/// Inhalt der arch-update.conf als Zeilenliste.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArchUpdateConf {
    lines: Vec<String>,
}

impl ArchUpdateConf {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
        }
    }

    /// Liest die Datei. `Ok(None)`, wenn sie nicht existiert.
    pub fn load(path: &Path) -> Result<Option<Self>, AppError> {
        match fs::read_to_string(path) {
            Ok(t) => Ok(Some(Self::parse(&t))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AppError::new(
                ErrorKind::ConfigInvalid,
                format!("{} ist nicht lesbar.", path.display()),
            )
            .with_details(e.to_string())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), AppError> {
        write_atomic(path, self.to_text().as_bytes()).map_err(|e| {
            AppError::new(
                ErrorKind::Internal,
                format!("{} konnte nicht geschrieben werden.", path.display()),
            )
            .with_details(e.to_string())
        })
    }

    pub fn to_text(&self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }

    /// Wie `config.sh`: aktiv, wenn eine Zeile exakt (bis auf Leerraum) dem Namen entspricht.
    pub fn flag(&self, key: &str) -> bool {
        let re = flag_re(key);
        self.lines.iter().any(|l| re.is_match(l))
    }

    /// Wie `config.sh`: erster gültiger Wert (ungültige Werte werden ignoriert → Standardwert).
    pub fn value(&self, key: &str) -> Option<String> {
        let (pat, _) = value_spec(key)?;
        let re = Regex::new(&format!(
            r"^[[:space:]]*{}[[:space:]]*=[[:space:]]*{pat}[[:space:]]*$",
            regex::escape(key)
        ))
        .unwrap();
        // config.sh nutzt `grep | awk -F= '{print $2}'`; bei mehreren gültigen
        // Zeilen entstünde ein mehrzeiliger Wert – wir verwenden die erste.
        self.lines
            .iter()
            .find(|l| re.is_match(l))
            .and_then(|l| l.split('=').nth(1))
            .map(|v| v.trim().to_string())
    }

    /// Effektiver Wert inklusive Standardwert aus `config.sh`.
    pub fn effective_value(&self, key: &str) -> String {
        self.value(key)
            .or_else(|| value_spec(key).map(|(_, d)| d.to_string()))
            .unwrap_or_default()
    }

    pub fn set_flag(&mut self, key: &str, enabled: bool) -> Result<(), AppError> {
        if !FLAG_KEYS.contains(&key) {
            return Err(unknown_key(key));
        }
        let active = flag_re(key);
        if enabled {
            if self.flag(key) {
                return Ok(());
            }
            let commented = commented_flag_re(key);
            if let Some(line) = self.lines.iter_mut().find(|l| commented.is_match(l)) {
                *line = key.to_string();
            } else {
                self.lines.push(key.to_string());
            }
        } else {
            for line in self.lines.iter_mut().filter(|l| active.is_match(l)) {
                *line = format!("#{key}");
            }
        }
        Ok(())
    }

    /// Setzt einen Wert (`Some`) oder kommentiert die Option aus (`None` → Standardwert).
    pub fn set_value(&mut self, key: &str, value: Option<&str>) -> Result<(), AppError> {
        let (pat, _) = value_spec(key).ok_or_else(|| unknown_key(key))?;
        let any_active = any_value_re(key);
        match value {
            Some(v) => {
                let v = v.trim();
                let full = Regex::new(&format!("^{pat}$")).unwrap();
                if !full.is_match(v) {
                    return Err(AppError::new(
                        ErrorKind::ConfigInvalid,
                        format!("Ungültiger Wert „{v}“ für {key}."),
                    ));
                }
                let new_line = format!("{key}={v}");
                let mut replaced = false;
                for line in self.lines.iter_mut() {
                    if any_active.is_match(line) {
                        if replaced {
                            *line = format!("#{}", line.trim_start());
                        } else {
                            *line = new_line.clone();
                            replaced = true;
                        }
                    }
                }
                if !replaced {
                    let commented = commented_value_re(key);
                    if let Some(line) = self.lines.iter_mut().find(|l| commented.is_match(l)) {
                        *line = new_line;
                    } else {
                        self.lines.push(new_line);
                    }
                }
            }
            None => {
                for line in self.lines.iter_mut().filter(|l| any_active.is_match(l)) {
                    *line = format!("#{}", line.trim_start());
                }
            }
        }
        Ok(())
    }
}

fn unknown_key(key: &str) -> AppError {
    AppError::new(
        ErrorKind::ConfigInvalid,
        format!("Die Option {key} wird von arch-update nicht unterstützt."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str =
        "# comment\n#NoColor\n#NoAUR\n#NewsNum=5\n#AURHelper=paru\n#TrayIconStyle=blue\n";

    #[test]
    fn reads_like_config_sh() {
        let c = ArchUpdateConf::parse("  NoAUR  \nNewsNum = 7\nAURHelper=foo\nKeepOldPackages=x\n");
        assert!(c.flag("NoAUR"));
        assert!(!c.flag("NoColor"));
        assert_eq!(c.value("NewsNum").as_deref(), Some("7"));
        // Ungültige Werte ignoriert config.sh → Standardwert.
        assert_eq!(c.value("AURHelper"), None);
        assert_eq!(c.effective_value("KeepOldPackages"), "3");
        assert_eq!(c.effective_value("NewsTimeout"), "10");
    }

    #[test]
    fn enabling_flag_uncomments_example_line() {
        let mut c = ArchUpdateConf::parse(EXAMPLE);
        c.set_flag("NoAUR", true).unwrap();
        assert!(c.flag("NoAUR"));
        assert_eq!(c.to_text(), EXAMPLE.replace("#NoAUR", "NoAUR"));
        c.set_flag("NoAUR", false).unwrap();
        assert_eq!(c.to_text(), EXAMPLE);
        c.set_flag("NoNotification", true).unwrap();
        assert!(c.to_text().ends_with("NoNotification\n"));
    }

    #[test]
    fn setting_values_preserves_layout() {
        let mut c = ArchUpdateConf::parse(EXAMPLE);
        c.set_value("NewsNum", Some("10")).unwrap();
        assert!(c.to_text().contains("\nNewsNum=10\n"));
        assert_eq!(c.value("NewsNum").as_deref(), Some("10"));
        c.set_value("NewsNum", Some("3")).unwrap();
        assert_eq!(c.to_text().matches("NewsNum").count(), 1);
        c.set_value("NewsNum", None).unwrap();
        assert_eq!(c.value("NewsNum"), None);
        assert!(c.to_text().contains("#NewsNum=3"));
    }

    #[test]
    fn rejects_values_config_sh_would_ignore() {
        let mut c = ArchUpdateConf::parse(EXAMPLE);
        assert!(c.set_value("NewsNum", Some("0")).is_err());
        assert!(c.set_value("AURHelper", Some("trizen")).is_err());
        assert!(
            c.set_value("PrivilegeElevationCommand", Some("sudo; rm -rf /"))
                .is_err()
        );
        assert!(c.set_value("DiffProg", Some("vim")).is_err());
        assert!(c.set_flag("Unknown", true).is_err());
        assert_eq!(c.to_text(), EXAMPLE);
    }

    #[test]
    fn duplicate_active_values_are_reduced_to_one() {
        let mut c = ArchUpdateConf::parse("NewsNum=2\nNewsNum=4\n");
        c.set_value("NewsNum", Some("6")).unwrap();
        assert_eq!(c.to_text(), "NewsNum=6\n#NewsNum=4\n");
    }
}
