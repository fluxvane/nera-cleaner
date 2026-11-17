//! Content blocks: Card, StatTile, StatGrid, Meter, Badge, Banner,
//! EmptyState and ProgressPanel.

use super::{dim, icon, sp, Tone};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The one bordered surface. Padding `sp::LG`, children gap `sp::MD`.
/// Use `.flush()` when the children draw their own padding (e.g. rows).
#[derive(IntoElement)]
pub struct Card {
    flush: bool,
    children: Vec<AnyElement>,
}

impl Default for Card {
    fn default() -> Self {
        Self::new()
    }
}

impl Card {
    pub fn new() -> Self {
        Self {
            flush: false,
            children: Vec::new(),
        }
    }

    pub fn flush(mut self) -> Self {
        self.flush = true;
        self
    }
}

impl ParentElement for Card {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

/// Shared card chrome so other blocks match exactly.
pub fn card_surface(cx: &App) -> Div {
    let t = cx.theme();
    v_flex()
        .w_full()
        .min_w_0()
        .bg(t.background)
        .border_1()
        .border_color(t.border)
        .rounded(t.radius_lg)
        .overflow_hidden()
}

impl RenderOnce for Card {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        card_surface(cx)
            .when(!self.flush, |this| this.p(sp::LG).gap(sp::MD))
            .children(self.children)
    }
}

/// A single metric: label above a large value, optional caption below.
#[derive(IntoElement)]
pub struct StatTile {
    label: SharedString,
    value: SharedString,
    caption: Option<SharedString>,
    icon: Option<&'static str>,
    tone: Tone,
    meter: Option<f64>,
}

impl StatTile {
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            caption: None,
            icon: None,
            tone: Tone::Neutral,
            meter: None,
        }
    }

    pub fn caption(mut self, caption: impl Into<SharedString>) -> Self {
        self.caption = Some(caption.into());
        self
    }

    pub fn icon(mut self, name: &'static str) -> Self {
        self.icon = Some(name);
        self
    }

    /// Colours the icon and meter (value text stays foreground for contrast).
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    /// Show a usage bar under the value (0.0…1.0).
    pub fn meter(mut self, fraction: f64) -> Self {
        self.meter = Some(fraction);
        self
    }
}

impl RenderOnce for StatTile {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let tone_color = self.tone.color(cx);
        card_surface(cx)
            .p(sp::LG)
            .gap(sp::SM)
            .child(
                h_flex()
                    .gap(sp::SM)
                    .items_center()
                    .when_some(self.icon, |this, name| {
                        this.child(icon(name).size(dim::ICON_SM).text_color(tone_color))
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .truncate()
                            .child(self.label),
                    ),
            )
            // Long values ("Service recommended") step down a size and may
            // wrap instead of being cut off; captions get two lines.
            .child({
                let long = self.value.chars().count() > 12;
                div()
                    .when(long, |d| d.text_xl())
                    .when(!long, |d| d.text_2xl())
                    .font_semibold()
                    .line_clamp(2)
                    .text_ellipsis()
                    .child(self.value)
            })
            .when_some(self.meter, |this, f| {
                this.child(Meter::new(f).tone(self.tone))
            })
            .when_some(self.caption, |this, c| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .line_clamp(2)
                        .text_ellipsis()
                        .child(c),
                )
            })
    }
}

/// Equal-width columns of tiles that wrap to the next row.
#[derive(IntoElement)]
pub struct StatGrid {
    columns: u16,
    children: Vec<AnyElement>,
}

impl StatGrid {
    pub fn new(columns: u16) -> Self {
        Self {
            columns: columns.max(1),
            children: Vec::new(),
        }
    }
}

impl ParentElement for StatGrid {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for StatGrid {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .grid()
            .grid_cols(self.columns)
            .gap(sp::LG)
            .children(self.children)
    }
}

/// Thin horizontal usage bar.
#[derive(IntoElement)]
pub struct Meter {
    fraction: f64,
    tone: Tone,
}

impl Meter {
    pub fn new(fraction: f64) -> Self {
        Self {
            fraction: fraction.clamp(0.0, 1.0),
            tone: Tone::Accent,
        }
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

impl RenderOnce for Meter {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = match self.tone {
            Tone::Neutral => cx.theme().muted_foreground,
            t => t.color(cx),
        };
        div()
            .w_full()
            .h(px(6.))
            .rounded_full()
            .bg(cx.theme().muted)
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .rounded_full()
                    .bg(color)
                    .w(relative(self.fraction as f32)),
            )
    }
}

/// Small pill label for status ("Running", "Safe", "3 items").
#[derive(IntoElement)]
pub struct Badge {
    label: SharedString,
    tone: Tone,
}

impl Badge {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            tone: Tone::Neutral,
        }
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

impl RenderOnce for Badge {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let fg = match self.tone {
            Tone::Neutral => cx.theme().foreground,
            t => t.color(cx),
        };
        div()
            .flex_shrink_0()
            .px(sp::SM)
            .py(sp::XXS)
            .rounded_full()
            .bg(self.tone.subtle_bg(cx))
            .text_xs()
            .font_medium()
            .text_color(fg)
            .child(self.label)
    }
}

/// Inline message strip (info, warning, error, success) with optional action.
#[derive(IntoElement)]
pub struct Banner {
    tone: Tone,
    title: SharedString,
    message: Option<SharedString>,
    action: Option<AnyElement>,
}

impl Banner {
    pub fn new(tone: Tone, title: impl Into<SharedString>) -> Self {
        Self {
            tone,
            title: title.into(),
            message: None,
            action: None,
        }
    }

    pub fn message(mut self, m: impl Into<SharedString>) -> Self {
        self.message = Some(m.into());
        self
    }

    pub fn action(mut self, a: impl IntoElement) -> Self {
        self.action = Some(a.into_any_element());
        self
    }
}

impl RenderOnce for Banner {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let color = self.tone.color(cx);
        let glyph = match self.tone {
            Tone::Success => "circle-check",
            Tone::Warning => "triangle-alert",
            Tone::Danger => "circle-x",
            _ => "info",
        };
        h_flex()
            .w_full()
            .items_start()
            .gap(sp::MD)
            .p(sp::MD)
            .rounded(t.radius_lg)
            .border_1()
            .border_color(color.opacity(0.35))
            .bg(self.tone.subtle_bg(cx))
            .child(icon(glyph).size(dim::ICON_MD).text_color(color).mt(sp::XXS))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(sp::XXS)
                    .child(div().text_sm().font_medium().child(self.title))
                    .when_some(self.message, |this, m| {
                        this.child(div().text_sm().text_color(t.muted_foreground).child(m))
                    }),
            )
            .when_some(self.action, |this, a| {
                this.child(div().flex_shrink_0().child(a))
            })
    }
}

/// Centred placeholder for "nothing yet" / "all clean" / errors.
#[derive(IntoElement)]
pub struct EmptyState {
    icon: &'static str,
    title: SharedString,
    message: Option<SharedString>,
    action: Option<AnyElement>,
    tone: Tone,
}

impl EmptyState {
    pub fn new(icon: &'static str, title: impl Into<SharedString>) -> Self {
        Self {
            icon,
            title: title.into(),
            message: None,
            action: None,
            tone: Tone::Neutral,
        }
    }

    pub fn message(mut self, m: impl Into<SharedString>) -> Self {
        self.message = Some(m.into());
        self
    }

    pub fn action(mut self, a: impl IntoElement) -> Self {
        self.action = Some(a.into_any_element());
        self
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let color = match self.tone {
            Tone::Neutral => muted,
            t => t.color(cx),
        };
        card_surface(cx)
            .items_center()
            .justify_center()
            .gap(sp::MD)
            .px(sp::XL)
            .py(sp::XXL)
            .child(
                div()
                    .size(px(48.))
                    .rounded_full()
                    .bg(self.tone.subtle_bg(cx))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(self.icon).size(dim::ICON_LG).text_color(color)),
            )
            .child(div().text_base().font_semibold().child(self.title))
            .when_some(self.message, |this, m| {
                this.child(
                    div()
                        .max_w(px(440.))
                        .text_sm()
                        .text_center()
                        .text_color(muted)
                        .child(m),
                )
            })
            .when_some(self.action, |this, a| this.child(div().mt(sp::XS).child(a)))
    }
}

type CancelHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Progress for a running task: label, detail line, bar, optional Cancel.
/// `fraction = None` shows an indeterminate spinner.
#[derive(IntoElement)]
pub struct ProgressPanel {
    id: ElementId,
    title: SharedString,
    detail: Option<SharedString>,
    fraction: Option<f64>,
    on_cancel: Option<CancelHandler>,
}

impl ProgressPanel {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: None,
            fraction: None,
            on_cancel: None,
        }
    }

    pub fn detail(mut self, d: impl Into<SharedString>) -> Self {
        self.detail = Some(d.into());
        self
    }

    pub fn fraction(mut self, f: Option<f64>) -> Self {
        self.fraction = f.map(|f| f.clamp(0.0, 1.0));
        self
    }

    pub fn on_cancel(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Box::new(f));
        self
    }
}

impl RenderOnce for ProgressPanel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let id = self.id.clone();
        // Keyed by the panel: two panels on one page must not share the
        // bar's animated value.
        let bar_id = SharedString::from(format!("{}-bar", self.id));
        card_surface(cx)
            .p(sp::LG)
            .gap(sp::MD)
            .child(
                h_flex()
                    .gap(sp::MD)
                    .items_center()
                    .child(Spinner::new().small())
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(sp::XXS)
                            .child(div().text_sm().font_medium().child(self.title))
                            .when_some(self.detail, |this, d| {
                                this.child(div().text_xs().text_color(muted).truncate().child(d))
                            }),
                    )
                    .when_some(self.fraction, |this, f| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(nera_core::format::percent(f)),
                        )
                    })
                    .when_some(self.on_cancel, |this, f| {
                        this.child(Button::new(id).small().ghost().label("Cancel").on_click(f))
                    }),
            )
            .when_some(self.fraction, |this, f| {
                this.child(Progress::new(bar_id).value((f * 100.0) as f32))
            })
    }
}
