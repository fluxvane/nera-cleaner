//! Screen registry: maps each nav entry to its feature crate view.

use gpui_kit::*;
use nera_ui::nav::Screen;

/// Create the view for `screen` (called once per screen, lazily).
pub fn build<V: 'static>(screen: Screen, window: &mut Window, cx: &mut Context<V>) -> AnyView {
    match screen {
        Screen::Dashboard => cx
            .new(|cx| nera_cleanup::DashboardScreen::new(window, cx))
            .into(),
        Screen::Cleanup => cx
            .new(|cx| nera_cleanup::CleanupScreen::new(window, cx))
            .into(),
        Screen::DevScan => cx
            .new(|cx| nera_devscan::DevScanScreen::new(window, cx))
            .into(),
        Screen::Duplicates => cx
            .new(|cx| nera_duplicates::DuplicatesScreen::new(window, cx))
            .into(),
        Screen::Space => cx.new(|cx| nera_space::SpaceScreen::new(window, cx)).into(),
        Screen::Apps => cx.new(|cx| nera_apps::AppsScreen::new(window, cx)).into(),
        Screen::Processes => cx
            .new(|cx| nera_processes::ProcessesScreen::new(window, cx))
            .into(),
        Screen::Startup => cx
            .new(|cx| nera_startup::StartupScreen::new(window, cx))
            .into(),
        Screen::Performance => cx
            .new(|cx| nera_performance::PerformanceScreen::new(window, cx))
            .into(),
        Screen::Battery => cx
            .new(|cx| nera_battery::BatteryScreen::new(window, cx))
            .into(),
        Screen::Privacy => cx
            .new(|cx| nera_privacy::PrivacyScreen::new(window, cx))
            .into(),
        Screen::Settings => cx
            .new(|cx| nera_settings::SettingsScreen::new(window, cx))
            .into(),
    }
}
