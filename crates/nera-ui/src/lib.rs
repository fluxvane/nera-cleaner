//! Nera design system, navigation and app-wide state, shared by every
//! feature crate. See docs/DESIGN.md.
//!
//! Feature screens: `use nera_ui::*;` brings in the kit (Page, Section, Card,
//! Row, sp, Tone, run_blocking, confirm, ...), plus `nav` and `state`.

pub mod nav;
pub mod state;
mod ui;

pub use ui::*;

/// Call once at startup (after `gpui_kit::component::init`).
pub fn init(cx: &mut gpui_kit::App) {
    state::init(cx);
    nav::init(cx);
}
