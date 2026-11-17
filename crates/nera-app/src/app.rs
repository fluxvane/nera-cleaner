//! Root view: sidebar navigation + the current screen.

use crate::screens;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_ui::nav::{self, NavGroup, Screen};
use nera_ui::{dim, icon, sp};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const SIDEBAR_W: Pixels = px(232.);
/// App logo in the sidebar header.
const LOGO: &[u8] = include_bytes!("../resources/logo.png");
const LOGO_SIZE: Pixels = px(22.);
/// A screen fades and settles into place when it is switched to.
const SCREEN_ENTER: Duration = Duration::from_millis(180);
const SCREEN_ENTER_OFFSET: f32 = 8.;

/// macOS draws the window buttons (traffic lights) over our sidebar header;
/// Windows keeps its native title bar with the standard caption buttons.
pub const CUSTOM_TITLEBAR: bool = cfg!(target_os = "macos");

pub struct NeraApp {
    /// Screens are created on first visit and kept alive (state survives
    /// switching tabs, e.g. a scan keeps running).
    screens: HashMap<Screen, AnyView>,
    logo: Arc<Image>,
    /// The screen on show and how many switches led to it; each switch
    /// plays the enter animation once.
    shown: Option<Screen>,
    switches: usize,
    _subscriptions: Vec<Subscription>,
}

impl NeraApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe_global::<nav::Nav>(|_, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, window, cx| {
                nera_ui::state::apply_theme(Some(window), cx);
            }),
        ];
        Self {
            screens: HashMap::new(),
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, LOGO.to_vec())),
            shown: None,
            switches: 0,
            _subscriptions: subs,
        }
    }

    fn screen_view(
        &mut self,
        screen: Screen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyView {
        self.screens
            .entry(screen)
            .or_insert_with(|| screens::build(screen, window, cx))
            .clone()
    }

    fn render_sidebar(&self, current: Screen, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let muted = t.sidebar_foreground.opacity(0.6);

        let groups = NavGroup::ALL.into_iter().filter_map(|group| {
            let items: Vec<Screen> = Screen::SIDEBAR
                .iter()
                .copied()
                .filter(|s| s.is_available() && s.group() == group)
                .collect();
            if items.is_empty() {
                return None;
            }
            let column = v_flex()
                .gap(sp::XXS)
                .when(group != NavGroup::Overview, |d| {
                    d.child(
                        div()
                            .px(sp::SM)
                            .pt(sp::SM)
                            .pb(sp::XS)
                            .text_xs()
                            .font_medium()
                            .text_color(muted)
                            .child(group.label()),
                    )
                })
                .children(items.into_iter().map(|s| nav_item(s, s == current, cx)));
            Some(column)
        });

        v_flex()
            .w(SIDEBAR_W)
            .flex_shrink_0()
            .h_full()
            .bg(t.sidebar)
            .text_color(t.sidebar_foreground)
            .border_r_1()
            .border_color(t.sidebar_border)
            .child(
                // Header: leaves room for the traffic lights, doubles as drag area.
                h_flex()
                    .h(dim::HEADER_H)
                    .flex_shrink_0()
                    .pl(if CUSTOM_TITLEBAR { px(84.) } else { sp::LG })
                    .pr(sp::LG)
                    .items_center()
                    .gap(sp::SM)
                    .window_control_area(WindowControlArea::Drag)
                    .child(img(self.logo.clone()).size(LOGO_SIZE).flex_shrink_0())
                    .child(div().text_sm().font_semibold().child("Nera Cleaner")),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().px(sp::MD).pb(sp::LG).gap(sp::SM).children(groups)),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(sp::MD)
                    .py(sp::MD)
                    .gap(sp::XS)
                    .border_t_1()
                    .border_color(t.sidebar_border)
                    .child(nav_item(Screen::Settings, current == Screen::Settings, cx))
                    .child(
                        div()
                            .px(sp::SM)
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Version {}", nera_core::version())),
                    ),
            )
    }
}

fn nav_item(screen: Screen, active: bool, cx: &App) -> impl IntoElement {
    let t = cx.theme();
    let fg = if active {
        t.sidebar_accent_foreground
    } else {
        t.sidebar_foreground
    };
    let hover = t.sidebar_accent.opacity(0.6);
    h_flex()
        .id(SharedString::from(format!("nav-{screen:?}")))
        .role(Role::Button)
        .aria_label(screen.title())
        .accessibility_id(SharedString::from(format!("nav-{screen:?}")))
        .h(px(32.))
        .px(sp::SM)
        .gap(sp::SM)
        .items_center()
        .rounded(t.radius)
        .text_sm()
        .text_color(fg)
        .cursor_pointer()
        .when(active, |d| d.bg(t.sidebar_accent).font_medium())
        .when(!active, |d| d.hover(move |s| s.bg(hover)))
        .child(
            icon(screen.icon())
                .size(dim::ICON_MD)
                .text_color(if active { t.primary } else { fg }),
        )
        .child(div().truncate().child(screen.title()))
        .on_click(move |_, _, cx| nav::navigate(screen, cx))
}

impl Render for NeraApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current = nav::current(cx);
        let view = self.screen_view(current, window, cx);
        if self.shown != Some(current) {
            self.shown = Some(current);
            self.switches += 1;
        }
        let t = cx.theme();
        // Dialog, sheet and notification layers are rendered by the window's
        // Root (mounted by `gpui_kit::open_window`).
        h_flex()
            .size_full()
            .bg(t.background)
            .text_color(t.foreground)
            .child(self.render_sidebar(current, cx))
            .child(div().flex_1().min_w_0().h_full().overflow_hidden().child(
                div().relative().size_full().child(view).with_animation(
                    ("screen-enter", self.switches),
                    Animation::new(SCREEN_ENTER).with_easing(ease_out_quint()),
                    |screen, progress| {
                        screen
                            .opacity(progress)
                            .top(px(SCREEN_ENTER_OFFSET * (1. - progress)))
                    },
                ),
            ))
    }
}
