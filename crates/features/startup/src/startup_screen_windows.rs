//! Startup apps on Windows: what Task Manager lists under Startup apps.
//!
//! Sections: your startup apps (can be turned on or off, like Task Manager) ·
//! startup apps for all users (read-only unless Nera Cleaner runs as an
//! administrator) · Microsoft Store startup tasks (read-only).

use crate::engine::{self, Scope, Snapshot, StartupItem, Write};
use crate::view::{self, BadgeTone};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, Disableable as _, Sizable as _};
use gpui_kit::*;
use nera_core::fs_util;
use nera_ui::nav::Screen;
use nera_ui::*;

const STARTUP_SETTINGS: &str = "ms-settings:startupapps";

pub struct StartupScreen {
    snapshot: Option<Snapshot>,
    scanning: bool,
    /// A change finished while a scan was running; scan once more after it.
    rescan_again: bool,
    /// Id of the item being changed; blocks other changes meanwhile.
    acting: Option<String>,
    search: Entity<InputState>,
    _scan_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _search_sub: Subscription,
}

impl StartupScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search by name, publisher or command")
        });
        let sub = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut this = Self {
            snapshot: None,
            scanning: false,
            rescan_again: false,
            acting: None,
            search,
            _scan_task: None,
            _action_task: None,
            _search_sub: sub,
        };
        this.rescan(cx);
        this
    }

    /// Re-read the startup apps (registry, Startup folders, Store tasks).
    fn rescan(&mut self, cx: &mut Context<Self>) {
        if self.scanning {
            // The scan in flight may have read the state before the change
            // that asked for this one: read again when it is done.
            self.rescan_again = true;
            return;
        }
        self.scanning = true;
        self.rescan_again = false;
        self._scan_task = Some(run_blocking(cx, engine::scan, |this, snapshot, cx| {
            this.snapshot = Some(snapshot);
            this.scanning = false;
            this._scan_task = None;
            if this.rescan_again {
                this.rescan(cx);
            }
        }));
        cx.notify();
    }

    // MARK: Changes

    fn toggle(
        &mut self,
        item: &StartupItem,
        enable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.acting.is_some() {
            return;
        }
        match engine::plan_toggle(item, enable, engine::filetime_now()) {
            Ok(write) => {
                let name = item.name.clone();
                let (title, message, failure) = if enable {
                    (
                        format!("Turned on {name}"),
                        "It starts when you sign in again.",
                        format!("Couldn't turn on {name}"),
                    )
                } else {
                    (
                        format!("Turned off {name}"),
                        "It won't start when you sign in.",
                        format!("Couldn't turn off {name}"),
                    )
                };
                self.run_write(
                    item.id.clone(),
                    write,
                    title,
                    message.into(),
                    failure,
                    window,
                    cx,
                );
            }
            Err(message) => notify(window, cx, Tone::Danger, "Not changed", message),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_write(
        &mut self,
        id: String,
        write: Write,
        success_title: String,
        success_message: String,
        failure_title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.acting = Some(id);
        let handle = window.window_handle();
        self._action_task = Some(run_blocking(
            cx,
            move || engine::execute(&write),
            move |this, result, cx| {
                this.acting = None;
                this._action_task = None;
                match result {
                    Ok(()) => {
                        toast_later(handle, cx, Tone::Success, success_title, success_message)
                    }
                    Err(message) => {
                        log::warn!("{failure_title}: {message}");
                        toast_later(handle, cx, Tone::Danger, failure_title, message);
                    }
                }
                this.rescan(cx);
            },
        ));
        cx.notify();
    }

    // MARK: Rendering

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    fn render_toolbar(&self) -> impl IntoElement {
        h_flex().w_full().items_center().gap(sp::MD).child(
            div().flex_1().min_w_0().child(
                Input::new(&self.search)
                    .prefix(icon("search").size(dim::ICON_SM))
                    .cleanable(true),
            ),
        )
    }

    fn render_banners(&self) -> Vec<AnyElement> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        snapshot
            .errors
            .iter()
            .map(|error| {
                Banner::new(Tone::Warning, "Couldn't read every startup location")
                    .message(format!("Some startup apps may be missing. {error}"))
                    .into_any_element()
            })
            .collect()
    }

    fn render_stats(&self) -> Option<AnyElement> {
        let snapshot = self.snapshot.as_ref()?;
        let [user, machine, store] = view::tiles(snapshot);
        Some(
            StatGrid::new(3)
                .child(
                    StatTile::new(user.label, user.value)
                        .icon("user")
                        .caption(user.caption),
                )
                .child(
                    StatTile::new(machine.label, machine.value)
                        .icon("users")
                        .caption(machine.caption),
                )
                .child(
                    StatTile::new(store.label, store.value)
                        .icon("store")
                        .caption(store.caption),
                )
                .into_any_element(),
        )
    }

    fn render_section(&self, scope: Scope, title: &'static str, cx: &mut Context<Self>) -> Section {
        let Some(snapshot) = &self.snapshot else {
            let section = Section::new(title);
            return if scope == Scope::User {
                section.child(ProgressPanel::new("scan-progress", "Reading startup apps"))
            } else {
                section
            };
        };
        let mut section =
            Section::new(title).description(view::section_description(scope, snapshot));
        if scope == Scope::User {
            section = section.trailing(
                Button::new("open-startup-settings")
                    .small()
                    .outline()
                    .icon(icon("external-link"))
                    .label("Open Settings")
                    .on_click(|_, _, _| fs_util::open(STARTUP_SETTINGS)),
            );
        }
        let query = self.query(cx);
        let items = engine::visible(snapshot.in_scope(scope), &query);
        if items.is_empty() {
            let (title, message) =
                view::empty_state(scope, snapshot.in_scope(scope).next().is_some());
            let empty_icon = match scope {
                Scope::User => "power",
                Scope::Machine => "users",
                Scope::Store => "store",
            };
            return section.child(EmptyState::new(empty_icon, title).message(message));
        }
        let rows: Vec<Row> = items.into_iter().map(|item| self.row(item, cx)).collect();
        section.child(ListCard::new().children(rows))
    }

    fn row(&self, item: &StartupItem, cx: &mut Context<Self>) -> Row {
        let icon_tone = if item.target_missing {
            Tone::Warning
        } else if item.running == Some(true) {
            Tone::Success
        } else {
            Tone::Neutral
        };
        let mut row = Row::new(SharedString::from(item.id.clone()), item.name.clone())
            .icon(if item.source == engine::Source::Store {
                "store"
            } else {
                "power"
            })
            .icon_tone(icon_tone)
            .subtitle(view::subtitle(item));
        for b in view::badges(item) {
            let tone = match b.tone {
                BadgeTone::Neutral => Tone::Neutral,
                BadgeTone::Accent => Tone::Accent,
                BadgeTone::Success => Tone::Success,
                BadgeTone::Warning => Tone::Warning,
            };
            row = row.badge(Badge::new(b.label).tone(tone));
        }
        if item.is_editable() {
            let toggled = item.clone();
            row = row.action(
                Switch::new(SharedString::from(format!("toggle:{}", item.id)))
                    .checked(item.enabled())
                    .disabled(self.acting.is_some())
                    .tooltip(if item.enabled() {
                        "Turn off"
                    } else {
                        "Turn on"
                    })
                    .on_change(cx.listener(move |this, checked: &bool, window, cx| {
                        this.toggle(&toggled, *checked, window, cx)
                    })),
            );
        }
        if let Some(path) = item.reveal_path() {
            row = row.action(
                Button::new(SharedString::from(format!("reveal:{}", item.id)))
                    .small()
                    .ghost()
                    .icon(icon("folder-search"))
                    .tooltip(nera_core::os::SHOW_IN_FILE_MANAGER)
                    .on_click(move |_, _, _| fs_util::reveal_in_finder(&path)),
            );
        }
        row
    }
}

impl Render for StartupScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.scanning || self.acting.is_some();
        let subtitle = view::page_subtitle(self.snapshot.as_ref());
        let mut page = Page::new("startup-page", Screen::Startup.title())
            .subtitle(subtitle)
            .toolbar(self.render_toolbar())
            .action(
                Button::new("refresh")
                    .primary()
                    .icon(icon("refresh-cw"))
                    .label("Refresh")
                    .loading(self.scanning)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
            );
        for banner in self.render_banners() {
            page = page.child(banner);
        }
        if let Some(stats) = self.render_stats() {
            page = page.child(stats);
        }
        page.child(self.render_section(Scope::User, "Your startup apps", cx))
            .child(self.render_section(Scope::Machine, "Startup apps for all users", cx))
            .child(self.render_section(Scope::Store, "Microsoft Store apps", cx))
    }
}

/// Show a toast once the current update finishes (async callbacks have no
/// `Window` of their own).
fn toast_later(window: AnyWindowHandle, cx: &mut App, tone: Tone, title: String, message: String) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
    });
}
