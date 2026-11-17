//! Screen catalogue and navigation.
//!
//! Any view can switch screens with [`navigate`]; the root view observes the
//! [`Nav`] global and re-renders.

use gpui_kit::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Screen {
    Dashboard,
    Cleanup,
    DevScan,
    Duplicates,
    Space,
    Apps,
    Processes,
    Startup,
    Performance,
    Battery,
    Privacy,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavGroup {
    Overview,
    Clean,
    Storage,
    System,
    Protect,
}

impl NavGroup {
    pub const ALL: [NavGroup; 5] = [
        NavGroup::Overview,
        NavGroup::Clean,
        NavGroup::Storage,
        NavGroup::System,
        NavGroup::Protect,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NavGroup::Overview => "Overview",
            NavGroup::Clean => "Clean",
            NavGroup::Storage => "Storage",
            NavGroup::System => "System",
            NavGroup::Protect => "Protect",
        }
    }
}

impl Screen {
    /// Sidebar order. Settings lives in the sidebar footer.
    pub const SIDEBAR: [Screen; 11] = [
        Screen::Dashboard,
        Screen::Cleanup,
        Screen::DevScan,
        Screen::Duplicates,
        Screen::Space,
        Screen::Apps,
        Screen::Processes,
        Screen::Startup,
        Screen::Performance,
        Screen::Battery,
        Screen::Privacy,
    ];

    /// Whether this screen exists on the platform the app was built for.
    /// Every screen is ported to macOS and Windows; a feature added for one
    /// platform first returns false here for the other.
    pub fn is_available(self) -> bool {
        true
    }

    /// The screen the app opens on.
    pub fn home() -> Screen {
        if Screen::Dashboard.is_available() {
            Screen::Dashboard
        } else {
            Screen::Settings
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Cleanup => "System Junk",
            Screen::DevScan => "Developer Junk",
            Screen::Duplicates => "Duplicates",
            Screen::Space => "Space Lens",
            Screen::Apps => "Applications",
            Screen::Processes => "Processes",
            Screen::Startup => {
                if cfg!(windows) {
                    "Startup Apps"
                } else {
                    "Login Items"
                }
            }
            Screen::Performance => "Performance",
            // Windows offers no supported way to read sensors or fans.
            Screen::Battery => {
                if cfg!(windows) {
                    "Battery"
                } else {
                    "Battery & Sensors"
                }
            }
            Screen::Privacy => "Privacy",
            Screen::Settings => "Settings",
        }
    }

    /// Lucide icon file name (see `ui::icon`).
    pub fn icon(self) -> &'static str {
        match self {
            Screen::Dashboard => "layout-dashboard",
            Screen::Cleanup => "brush-cleaning",
            Screen::DevScan => "code-xml",
            Screen::Duplicates => "copy",
            Screen::Space => "chart-pie",
            Screen::Apps => "app-window",
            Screen::Processes => "cpu",
            Screen::Startup => "power",
            Screen::Performance => "gauge",
            Screen::Battery => "battery-medium",
            Screen::Privacy => "shield-check",
            Screen::Settings => "settings",
        }
    }

    pub fn group(self) -> NavGroup {
        match self {
            Screen::Dashboard => NavGroup::Overview,
            Screen::Cleanup | Screen::DevScan | Screen::Duplicates => NavGroup::Clean,
            Screen::Space | Screen::Apps => NavGroup::Storage,
            Screen::Processes | Screen::Startup | Screen::Performance | Screen::Battery => {
                NavGroup::System
            }
            Screen::Privacy | Screen::Settings => NavGroup::Protect,
        }
    }
}

pub struct Nav {
    pub current: Screen,
}

impl Global for Nav {}

pub fn init(cx: &mut App) {
    let current = std::env::args()
        .skip_while(|a| a != "--screen")
        .nth(1)
        .and_then(|name| {
            Screen::SIDEBAR
                .iter()
                .chain(std::iter::once(&Screen::Settings))
                .find(|s| s.is_available() && format!("{s:?}").eq_ignore_ascii_case(&name))
                .copied()
        })
        .unwrap_or_else(Screen::home);
    cx.set_global(Nav { current });
}

pub fn current(cx: &App) -> Screen {
    cx.global::<Nav>().current
}

pub fn navigate(screen: Screen, cx: &mut App) {
    if screen.is_available() && current(cx) != screen {
        cx.global_mut::<Nav>().current = screen;
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` exports its own `test` attribute; name the standard one.
    use super::*;

    #[core::prelude::v1::test]
    fn home_and_settings_are_always_available() {
        assert!(Screen::home().is_available());
        assert!(Screen::Settings.is_available());
    }

    #[core::prelude::v1::test]
    fn every_screen_is_available() {
        assert!(Screen::SIDEBAR.iter().all(|s| s.is_available()));
        assert_eq!(Screen::home(), Screen::Dashboard);
    }
}
