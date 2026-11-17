//! App-wide preferences as a GPUI global, backed by `nera_core::settings`.

use gpui_kit::component::{Theme, ThemeMode as KitMode};
use gpui_kit::*;
use nera_core::settings::{Settings, ThemeMode};

pub struct AppSettings(pub Settings);

impl Global for AppSettings {}

pub fn init(cx: &mut App) {
    cx.set_global(AppSettings(Settings::load()));
}

/// Current preferences.
pub fn settings(cx: &App) -> &Settings {
    &cx.global::<AppSettings>().0
}

/// Change preferences, persist them, and refresh every window.
pub fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let before_theme = settings(cx).theme;
    f(&mut cx.global_mut::<AppSettings>().0);
    if let Err(err) = settings(cx).save() {
        log::error!("could not save settings: {err}");
    }
    if settings(cx).theme != before_theme {
        apply_theme(None, cx);
    }
    cx.refresh_windows();
}

/// Apply the preferred light/dark mode (System follows the OS appearance).
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    match settings(cx).theme {
        ThemeMode::System => Theme::sync_system_appearance(window, cx),
        ThemeMode::Light => Theme::change(KitMode::Light, window, cx),
        ThemeMode::Dark => Theme::change(KitMode::Dark, window, cx),
    }
}
