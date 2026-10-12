//! Cashy Updater GUI – grafische Oberfläche für `cachy-update` (arch-update).
//!
//! Die Update-Logik bleibt vollständig in arch-update; diese Bibliothek
//! kapselt Aufruf, Statusauswertung, Zustand und Darstellung.

pub mod app;
pub mod config;
pub mod error;
pub mod logging;
pub mod paths;
pub mod state;
pub mod ui;
pub mod updater;
