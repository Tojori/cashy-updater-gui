//! Konfiguration: eigene GUI-Einstellungen und die vorhandene arch-update.conf.

pub mod arch_update_conf;
pub mod settings;

pub use arch_update_conf::ArchUpdateConf;
pub use settings::{LoadOutcome, Settings, Theme};
