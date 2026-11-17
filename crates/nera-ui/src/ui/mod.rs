//! Nera's design system. EVERY screen is built from these pieces so layout,
//! spacing, borders and typography stay identical across the app.
//!
//! Rules (see docs/DESIGN.md):
//! - A screen renders exactly one [`Page`]; its children are [`Section`]s.
//! - Content inside a section sits in a [`Card`] (or [`ListCard`] of [`Row`]s).
//! - Spacing comes only from [`sp`]; never write raw `px(...)` gaps in screens.
//! - Colors come only from `cx.theme()` or [`Tone`]; never hard-code colors.

mod blocks;
mod page;
mod rows;
mod tasks;

pub use blocks::*;
pub use page::*;
pub use rows::*;
pub use tasks::*;

use gpui_kit::component::{ActiveTheme as _, Icon};
use gpui_kit::*;

/// Spacing scale (px). Relationship → token:
/// parts of one control `XS`, related controls `SM`, inside a group `MD`,
/// separate groups / card padding `LG`, separate sections `XL`, page gutters `XXL`.
pub mod sp {
    use gpui_kit::{px, Pixels};
    pub const XXS: Pixels = px(2.);
    pub const XS: Pixels = px(4.);
    pub const SM: Pixels = px(8.);
    pub const MD: Pixels = px(12.);
    pub const LG: Pixels = px(16.);
    pub const XL: Pixels = px(24.);
    pub const XXL: Pixels = px(32.);
}

/// Fixed layout dimensions.
pub mod dim {
    use gpui_kit::{px, Pixels};
    /// Maximum width of page content; wider windows centre it.
    pub const CONTENT_MAX_W: Pixels = px(1080.);
    /// Height of the page header bar (aligned with the sidebar header).
    pub const HEADER_H: Pixels = px(64.);
    pub const ICON_SM: Pixels = px(14.);
    pub const ICON_MD: Pixels = px(16.);
    pub const ICON_LG: Pixels = px(20.);
    /// Square tile behind a row/stat icon.
    pub const ICON_TILE: Pixels = px(32.);
    pub const ROW_MIN_H: Pixels = px(52.);
}

/// A Lucide icon from the bundled catalogue, by file name without `.svg`
/// (e.g. `icon("trash")`). Browse names at https://lucide.dev/icons.
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// Semantic colour role for status text, badges, icons and banners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
    Info,
}

impl Tone {
    pub fn color(self, cx: &App) -> Hsla {
        let t = cx.theme();
        match self {
            Tone::Neutral => t.muted_foreground,
            Tone::Accent => t.primary,
            Tone::Success => t.success,
            Tone::Warning => t.warning,
            Tone::Danger => t.danger,
            Tone::Info => t.info,
        }
    }

    /// Low-contrast fill behind tone-coloured content.
    pub fn subtle_bg(self, cx: &App) -> Hsla {
        match self {
            Tone::Neutral => cx.theme().muted,
            other => other.color(cx).opacity(0.12),
        }
    }

    /// Tone for a fraction where higher is worse (disk/memory usage).
    pub fn for_usage(fraction: f64) -> Tone {
        if fraction >= 0.9 {
            Tone::Danger
        } else if fraction >= 0.75 {
            Tone::Warning
        } else {
            Tone::Success
        }
    }
}
