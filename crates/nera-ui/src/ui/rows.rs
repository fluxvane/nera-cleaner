//! Lists: ListCard holds Rows separated by hairlines. Rows keep fixed lanes
//! (leading control · icon · text · badges · value · actions) so columns line
//! up across every list in the app.

use super::{card_surface, dim, icon, sp, Tone};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// A card containing rows, with an optional header row (e.g. select-all +
/// column captions). Rows are separated by 1px hairlines; no outer padding.
#[derive(IntoElement)]
pub struct ListCard {
    header: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl Default for ListCard {
    fn default() -> Self {
        Self::new()
    }
}

impl ListCard {
    pub fn new() -> Self {
        Self {
            header: None,
            children: Vec::new(),
        }
    }

    pub fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }
}

impl ParentElement for ListCard {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for ListCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let border = t.border;
        let head_bg = t.secondary;
        let count = self.children.len();
        card_surface(cx)
            .when_some(self.header, |this, h| {
                this.child(
                    div()
                        .w_full()
                        .px(sp::LG)
                        .py(sp::SM)
                        .bg(head_bg)
                        .border_b_1()
                        .border_color(border)
                        .text_xs()
                        .text_color(t.muted_foreground)
                        .child(h),
                )
            })
            .children(self.children.into_iter().enumerate().map(|(i, row)| {
                div()
                    .w_full()
                    .when(i + 1 < count, |d| d.border_b_1().border_color(border))
                    .child(row)
            }))
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub type CheckHandler = Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// One list row.
#[derive(IntoElement)]
pub struct Row {
    id: ElementId,
    title: SharedString,
    subtitle: Option<SharedString>,
    icon: Option<&'static str>,
    icon_tone: Tone,
    leading: Option<AnyElement>,
    checkbox: Option<(bool, CheckHandler)>,
    badges: Vec<AnyElement>,
    value: Option<SharedString>,
    value_tone: Tone,
    actions: Vec<AnyElement>,
    on_click: Option<ClickHandler>,
    selected: bool,
    disabled: bool,
}

impl Row {
    /// `id` must be domain-derived (path, bundle id, pid), never a list index.
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            icon: None,
            icon_tone: Tone::Neutral,
            leading: None,
            checkbox: None,
            badges: Vec::new(),
            value: None,
            value_tone: Tone::Neutral,
            actions: Vec::new(),
            on_click: None,
            selected: false,
            disabled: false,
        }
    }

    pub fn subtitle(mut self, s: impl Into<SharedString>) -> Self {
        self.subtitle = Some(s.into());
        self
    }

    /// Lucide icon in a tinted tile.
    pub fn icon(mut self, name: &'static str) -> Self {
        self.icon = Some(name);
        self
    }

    pub fn icon_tone(mut self, tone: Tone) -> Self {
        self.icon_tone = tone;
        self
    }

    /// Custom leading visual (e.g. an app icon image) instead of `icon`.
    pub fn leading(mut self, el: impl IntoElement) -> Self {
        self.leading = Some(el.into_any_element());
        self
    }

    /// Leading checkbox; handler receives the requested checked value.
    pub fn checkbox(
        mut self,
        checked: bool,
        f: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.checkbox = Some((checked, Box::new(f)));
        self
    }

    pub fn badge(mut self, b: impl IntoElement) -> Self {
        self.badges.push(b.into_any_element());
        self
    }

    /// Right-aligned value column (size, %, count). Tabular, fixed width.
    pub fn value(mut self, v: impl Into<SharedString>) -> Self {
        self.value = Some(v.into());
        self
    }

    pub fn value_tone(mut self, tone: Tone) -> Self {
        self.value_tone = tone;
        self
    }

    /// Trailing action (small ghost/outline buttons).
    pub fn action(mut self, a: impl IntoElement) -> Self {
        self.actions.push(a.into_any_element());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(f));
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for Row {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let muted = t.muted_foreground;
        let hover_bg = t.list_hover;
        let selected_bg = t.list_active;
        let clickable = self.on_click.is_some() && !self.disabled;
        let value_color = match self.value_tone {
            Tone::Neutral => t.foreground,
            tone => tone.color(cx),
        };
        let icon_color = match self.icon_tone {
            Tone::Neutral => t.foreground,
            tone => tone.color(cx),
        };
        let icon_bg = self.icon_tone.subtle_bg(cx);
        let check_id = SharedString::from(format!("{}-check", self.id));
        let a11y_label = self.title.clone();

        h_flex()
            .id(self.id)
            .w_full()
            .min_h(dim::ROW_MIN_H)
            .px(sp::LG)
            .py(sp::SM)
            .gap(sp::MD)
            .items_center()
            .when(self.selected, |d| d.bg(selected_bg))
            .when(self.disabled, |d| d.opacity(0.55))
            .when(clickable, |d| {
                d.cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .role(Role::Button)
                    .aria_label(a11y_label.clone())
            })
            .when_some(self.checkbox, |d, (checked, f)| {
                d.child(
                    Checkbox::new(check_id)
                        .checked(checked)
                        .disabled(self.disabled)
                        .on_change(f),
                )
            })
            .when_some(self.leading, |d, el| {
                d.child(div().flex_shrink_0().child(el))
            })
            .when_some(self.icon, |d, name| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .size(dim::ICON_TILE)
                        .rounded(t.radius)
                        .bg(icon_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(name).size(dim::ICON_MD).text_color(icon_color)),
                )
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(sp::XXS)
                    .child(div().text_sm().font_medium().truncate().child(self.title))
                    .when_some(self.subtitle, |d, s| {
                        d.child(div().text_xs().text_color(muted).truncate().child(s))
                    }),
            )
            .when(!self.badges.is_empty(), |d| {
                d.child(h_flex().flex_shrink_0().gap(sp::XS).children(self.badges))
            })
            .when_some(self.value, |d, v| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .min_w(px(88.))
                        .text_right()
                        .text_sm()
                        .font_medium()
                        .text_color(value_color)
                        .child(v),
                )
            })
            .when(!self.actions.is_empty(), |d| {
                d.child(h_flex().flex_shrink_0().gap(sp::XS).children(self.actions))
            })
            .when_some(self.on_click.filter(|_| clickable), |d, f| d.on_click(f))
    }
}

/// Header row for a [`ListCard`]: optional select-all checkbox, a label, and a
/// right-aligned caption aligned with the Row value column.
pub fn list_header(
    id: &str,
    label: impl Into<SharedString>,
    value_caption: Option<&'static str>,
    select_all: Option<(bool, CheckHandler)>,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap(sp::MD)
        .items_center()
        .when_some(select_all, |d, (checked, f)| {
            d.child(
                Checkbox::new(SharedString::from(format!("{id}-all")))
                    .checked(checked)
                    .on_change(f),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_medium()
                .child(label.into()),
        )
        .when_some(value_caption, |d, c| {
            d.child(div().min_w(px(88.)).text_right().font_medium().child(c))
        })
}

/// Label/value line for compact key-value details inside a Card.
pub fn kv_row(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .justify_between()
        .gap(sp::LG)
        .text_sm()
        .child(
            div()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(div().font_medium().text_right().child(value.into()))
}
