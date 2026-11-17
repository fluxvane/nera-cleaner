//! Page and Section: the fixed skeleton of every screen.

use super::{dim, sp};
use gpui_kit::base::InteractiveElementExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;

/// One screen: a fixed header (title, subtitle, primary actions), an optional
/// toolbar band (search/filters/summary), and a scrolling body of sections.
///
/// ```ignore
/// Page::new("cleanup", "System Junk")
///     .subtitle("Caches, logs and temporary files")
///     .action(Button::new("scan").primary().label("Scan"))
///     .child(Section::new("Results").child(...))
/// ```
#[derive(IntoElement)]
pub struct Page {
    id: ElementId,
    title: SharedString,
    subtitle: Option<SharedString>,
    actions: Vec<AnyElement>,
    toolbar: Option<AnyElement>,
    footer: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl Page {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            actions: Vec::new(),
            toolbar: None,
            footer: None,
            children: Vec::new(),
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Header action, trailing edge. Add the primary action LAST.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    /// Band under the header for search, filters or a selection summary.
    pub fn toolbar(mut self, toolbar: impl IntoElement) -> Self {
        self.toolbar = Some(toolbar.into_any_element());
        self
    }

    /// Sticky bottom bar, e.g. "3 selected · 1.2 GB  [Clean]".
    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }
}

impl ParentElement for Page {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

/// Scroll position of every page, by page id. Kept for the whole session so
/// a screen is exactly where the user left it after visiting another one
/// (state stored in the element tree is dropped while a screen is hidden).
#[derive(Default)]
struct PageScrolls(HashMap<ElementId, ScrollHandle>);

impl Global for PageScrolls {}

/// The scrolling body of a page: `gpui-component`'s scroll area (bounce,
/// axis lock, overlay scrollbar) around a scroll handle that outlives it.
fn scroll_body(id: ElementId, content: Div, cx: &mut App) -> impl IntoElement {
    let handle = cx
        .default_global::<PageScrolls>()
        .0
        .entry(id.clone())
        .or_default()
        .clone();
    let area = div()
        .id((id.clone(), "area"))
        .size_full()
        .flex()
        .flex_col()
        .track_scroll(&handle)
        .overflow_y_scroll()
        .lock_scroll_axis()
        .child(
            div()
                .id((id.clone(), "content"))
                .flex_none()
                .w_full()
                .min_h_full()
                .child(content),
        );
    div()
        .id(id.clone())
        .flex_1()
        .min_h_0()
        .relative()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(gpui_kit::base::ScrollBounce::new(
            (id, "bounce"),
            &handle,
            area,
        ))
        .vertical_scrollbar(&handle)
}

/// Centres page-width content with the standard gutters.
fn gutter() -> Div {
    div()
        .w_full()
        .max_w(dim::CONTENT_MAX_W)
        .mx_auto()
        .px(sp::XXL)
}

impl RenderOnce for Page {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let body = scroll_body(
            self.id,
            gutter()
                .py(sp::XL)
                .v_flex()
                .gap(sp::XL)
                .children(self.children),
            cx,
        );
        let theme = cx.theme();
        let header = h_flex()
            .w_full()
            .h(dim::HEADER_H)
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(
                gutter()
                    .h_full()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap(sp::LG)
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap(sp::XXS)
                            .child(div().text_lg().font_semibold().truncate().child(self.title))
                            .when_some(self.subtitle, |this, s| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(s),
                                )
                            }),
                    )
                    .child(h_flex().flex_shrink_0().gap(sp::SM).children(self.actions)),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(header)
            .when_some(self.toolbar, |this, toolbar| {
                this.child(
                    div()
                        .w_full()
                        .flex_shrink_0()
                        .py(sp::MD)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(gutter().child(toolbar)),
                )
            })
            .child(body)
            .when_some(self.footer, |this, footer| {
                this.child(
                    div()
                        .w_full()
                        .flex_shrink_0()
                        .py(sp::MD)
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.secondary)
                        .child(gutter().child(footer)),
                )
            })
    }
}

/// A titled group of content within a page. Sections are separated by
/// `sp::XL`; the title row sits `sp::MD` above its content.
#[derive(IntoElement)]
pub struct Section {
    title: Option<SharedString>,
    description: Option<SharedString>,
    trailing: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl Section {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: Some(title.into()),
            description: None,
            trailing: None,
            children: Vec::new(),
        }
    }

    /// A section without a heading (e.g. a hero summary at the top).
    pub fn untitled() -> Self {
        Self {
            title: None,
            description: None,
            trailing: None,
            children: Vec::new(),
        }
    }

    pub fn description(mut self, d: impl Into<SharedString>) -> Self {
        self.description = Some(d.into());
        self
    }

    /// Small control aligned with the title (link-style button, count, sort).
    pub fn trailing(mut self, el: impl IntoElement) -> Self {
        self.trailing = Some(el.into_any_element());
        self
    }
}

impl ParentElement for Section {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Section {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let has_head = self.title.is_some() || self.trailing.is_some();
        v_flex()
            .w_full()
            .gap(sp::MD)
            .when(has_head, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .items_end()
                        .justify_between()
                        .gap(sp::LG)
                        .child(
                            v_flex()
                                .min_w_0()
                                .gap(sp::XXS)
                                .when_some(self.title, |this, t| {
                                    this.child(div().text_base().font_semibold().child(t))
                                })
                                .when_some(self.description, |this, d| {
                                    this.child(div().text_sm().text_color(muted).child(d))
                                }),
                        )
                        .when_some(self.trailing, |this, t| this.child(t)),
                )
            })
            .children(self.children)
    }
}
