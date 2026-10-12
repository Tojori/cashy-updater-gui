//! Anwenden des Themes auf die Slint-Oberfläche.

use slint::ComponentHandle;

use crate::config::Theme as ThemeSetting;
use crate::ui::{AppBridge, MainWindow, Theme};

pub fn apply(ui: &MainWindow, theme: ThemeSetting) {
    let dark = theme == ThemeSetting::Dark;
    ui.global::<Theme>().set_dark(dark);
    ui.global::<AppBridge>().set_dark_theme(dark);
}
