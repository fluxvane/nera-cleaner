//! Login Items: apps and launchd jobs that start automatically.
//!
//! Sections: login items (System Events, read-only) · user agents (can be
//! turned off or removed) · system agents and daemons (read-only; changing
//! them needs an administrator).

use crate::engine::{self, LaunchItem, LoginError, LoginItem, Snapshot, Status, Step};
use crate::view::{self, BadgeTone};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::*;
use nera_core::fs_util;
use nera_ui::nav::Screen;
use nera_ui::*;
use std::time::Duration;

const LOGIN_ITEMS_PANE: &str = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";
const AUTOMATION_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";
const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
/// How long to wait for System Events (the first run shows a permission prompt).
const LOGIN_ITEMS_TIMEOUT: Duration = Duration::from_secs(30);

enum Login {
    /// Not read yet: reading asks macOS for Automation access to System
    /// Events, so it only happens when the user asks for it.
    NotLoaded,
    Loading,
    Ready(Vec<LoginItem>),
    Failed(LoginError),
}

pub struct StartupScreen {
    snapshot: Option<Snapshot>,
    login: Login,
    scanning: bool,
    /// A change finished while a scan was running; scan once more after it.
    rescan_again: bool,
    /// Label of the agent being changed; blocks other changes meanwhile.
    acting: Option<String>,
    show_apple: bool,
    search: Entity<InputState>,
    _scan_task: Option<Task<()>>,
    _login_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _search_sub: Subscription,
}

impl StartupScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search by name, vendor or program"));
        let sub = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut this = Self {
            snapshot: None,
            login: Login::NotLoaded,
            scanning: false,
            rescan_again: false,
            acting: None,
            show_apple: false,
            search,
            _scan_task: None,
            _login_task: None,
            _action_task: None,
            _search_sub: sub,
        };
        this.rescan(cx);
        this
    }

    fn is_loading(&self) -> bool {
        self.scanning || self._login_task.is_some()
    }

    /// Re-read launch agents and daemons (cheap; no System Events prompt).
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

    /// Re-read agents and daemons, plus login items once the user has asked
    /// for them (so a refresh never triggers the Automation prompt by itself).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.rescan(cx);
        if !matches!(self.login, Login::NotLoaded) {
            self.load_login_items(cx);
        }
        cx.notify();
    }

    /// Read login items through System Events (may show macOS's Automation prompt).
    fn load_login_items(&mut self, cx: &mut Context<Self>) {
        if self._login_task.is_none() {
            // Keep showing the previous list while it reloads.
            if !matches!(self.login, Login::Ready(_)) {
                self.login = Login::Loading;
            }
            self._login_task = Some(run_blocking(
                cx,
                || engine::read_login_items(LOGIN_ITEMS_TIMEOUT),
                |this, result, _| {
                    this.login = match result {
                        Ok(items) => Login::Ready(items),
                        Err(e) => Login::Failed(e),
                    };
                    this._login_task = None;
                },
            ));
        }
        cx.notify();
    }

    // MARK: Changes

    fn toggle(
        &mut self,
        item: &LaunchItem,
        enable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.acting.is_some() {
            return;
        }
        match engine::plan_toggle(item, enable, engine::uid()) {
            Ok(steps) => {
                let (done_title, verb) = if enable {
                    ("Turned on", "on")
                } else {
                    ("Turned off", "off")
                };
                self.run_steps(
                    item.label.clone(),
                    steps,
                    format!("{done_title} {}", item.label),
                    if enable {
                        "It starts automatically again."
                    } else {
                        "It won't start at login any more."
                    }
                    .into(),
                    format!("Couldn't turn {verb} {}", item.label),
                    window,
                    cx,
                );
            }
            Err(message) => notify(window, cx, Tone::Danger, "Not changed", message),
        }
    }

    fn request_remove(&mut self, item: &LaunchItem, window: &mut Window, cx: &mut Context<Self>) {
        if self.acting.is_some() {
            return;
        }
        let steps = match engine::plan_remove(item, engine::uid()) {
            Ok(steps) => steps,
            Err(message) => return notify(window, cx, Tone::Danger, "Not removed", message),
        };
        let label = item.label.clone();
        let file = item
            .plist
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| label.clone());
        let entity = cx.entity();
        confirm(
            Confirm::destructive(
                format!("Remove {label}?"),
                format!("It is stopped and {file} is moved to the Trash. Restore it from the Trash to get it back."),
                "Move to Trash",
            ),
            false,
            window,
            cx,
            move |window, cx| {
                let (steps, label) = (steps.clone(), label.clone());
                entity.update(cx, |this, cx| {
                    this.run_steps(
                        label.clone(),
                        steps,
                        format!("Removed {label}"),
                        "Its plist is in the Trash.".into(),
                        format!("Couldn't remove {label}"),
                        window,
                        cx,
                    )
                });
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn run_steps(
        &mut self,
        label: String,
        steps: Vec<Step>,
        success_title: String,
        success_message: String,
        failure_title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.acting = Some(label);
        let handle = window.window_handle();
        self._action_task = Some(run_blocking(
            cx,
            move || engine::execute(&steps),
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
                // Only launchd state changes; login items don't need re-reading.
                this.rescan(cx);
            },
        ));
        cx.notify();
    }

    // MARK: Rendering

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .gap(sp::MD)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .prefix(icon("search").size(dim::ICON_SM))
                        .cleanable(true),
                ),
            )
            .child(
                Button::new("show-apple")
                    .outline()
                    .icon(icon(if self.show_apple { "eye" } else { "eye-off" }))
                    .label("Apple items")
                    .selected(self.show_apple)
                    .tooltip("Items made by Apple in these folders are hidden by default")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_apple = !this.show_apple;
                        cx.notify();
                    })),
            )
    }

    fn render_banners(&self) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let Some(snapshot) = &self.snapshot else {
            return out;
        };
        for (dir, message, permission) in &snapshot.folder_errors {
            let dir = engine::tilde(&dir.to_string_lossy());
            let mut banner = Banner::new(
                Tone::Warning,
                if *permission {
                    format!("Can't open {dir}")
                } else {
                    format!("Couldn't read {dir}")
                },
            );
            if *permission {
                banner = banner
                    .message("Nera Cleaner needs Full Disk Access to list this folder. Grant access, then refresh.")
                    .action(
                        Button::new("open-fda")
                            .small()
                            .outline()
                            .label("Open System Settings")
                            .on_click(|_, _, _| fs_util::open(FULL_DISK_ACCESS_PANE)),
                    );
            } else {
                banner = banner.message(message.clone());
            }
            out.push(banner.into_any_element());
        }
        if let Some(error) = &snapshot.launchd_error {
            out.push(
                Banner::new(Tone::Warning, "Couldn't read launchd state")
                    .message(format!(
                        "Running and on/off states may be missing or out of date. {error}"
                    ))
                    .into_any_element(),
            );
        }
        out
    }

    fn render_stats(&self) -> Option<AnyElement> {
        let snapshot = self.snapshot.as_ref()?;
        let login = match &self.login {
            Login::Ready(items) => view::LoginState::Ready(items.len()),
            Login::NotLoaded => view::LoginState::NotLoaded,
            Login::Loading => view::LoginState::Loading,
            Login::Failed(_) => view::LoginState::Failed,
        };
        let [logins, user, system] = view::tiles(snapshot, login, self.show_apple);
        Some(
            StatGrid::new(3)
                .child(
                    StatTile::new(logins.label, logins.value)
                        .icon("log-in")
                        .caption(logins.caption),
                )
                .child(
                    StatTile::new(user.label, user.value)
                        .icon("user")
                        .caption(user.caption),
                )
                .child(
                    StatTile::new(system.label, system.value)
                        .icon("server")
                        .caption(system.caption),
                )
                .into_any_element(),
        )
    }

    fn render_login_items(&self, cx: &mut Context<Self>) -> Section {
        let section = Section::new("Login items")
            .description(
                "Apps that open when you log in. Background items added by apps only appear in System Settings > General > Login Items & Extensions.",
            )
            .trailing(
                Button::new("open-login-settings")
                    .small()
                    .outline()
                    .icon(icon("external-link"))
                    .label("Open System Settings")
                    .on_click(|_, _, _| fs_util::open(LOGIN_ITEMS_PANE)),
            );
        match &self.login {
            Login::NotLoaded => section.child(
                EmptyState::new("log-in", "Login items not loaded")
                    .message("Reading them asks macOS to let Nera Cleaner control System Events. Nothing is changed.")
                    .action(
                        Button::new("load-login-items")
                            .small()
                            .label("Show login items")
                            .on_click(cx.listener(|this, _, _, cx| this.load_login_items(cx))),
                    ),
            ),
            Login::Loading => section.child(
                ProgressPanel::new("login-progress", "Reading login items")
                    .detail("macOS may ask to let Nera Cleaner control System Events. Allow it to see your login items."),
            ),
            Login::Failed(error) => section.child(login_error_banner(error, cx)),
            Login::Ready(items) => {
                let query = self.query(cx);
                let shown: Vec<&LoginItem> = items
                    .iter()
                    .filter(|i| {
                        let q = query.trim().to_lowercase();
                        q.is_empty() || i.name.to_lowercase().contains(&q) || i.path.to_lowercase().contains(&q)
                    })
                    .collect();
                if shown.is_empty() {
                    let (title, message) = if items.is_empty() {
                        ("No login items", "No apps are set to open when you log in.")
                    } else {
                        ("No matches", "No login item matches your search.")
                    };
                    section.child(EmptyState::new("log-in", title).message(message))
                } else {
                    section.child(ListCard::new().children(shown.into_iter().map(login_row)))
                }
            }
        }
    }

    fn render_user_agents(&self, cx: &mut Context<Self>) -> Section {
        let section = Section::new("User agents")
            .description("Run in your session from ~/Library/LaunchAgents. You can turn them off or remove them.");
        let Some(snapshot) = &self.snapshot else {
            return section.child(ProgressPanel::new("scan-progress", "Reading launch agents"));
        };
        let query = self.query(cx);
        let items = engine::visible(snapshot.user(), &query, self.show_apple);
        if items.is_empty() {
            return section.child(self.empty_agents(snapshot.user().next().is_some(), "user"));
        }
        let rows: Vec<Row> = items
            .into_iter()
            .map(|item| self.agent_row(item, cx))
            .collect();
        section.child(ListCard::new().children(rows))
    }

    fn render_system_agents(&self, cx: &mut Context<Self>) -> Section {
        let section = Section::new("System agents and daemons")
            .description("Installed for every user in /Library. Changing them needs an administrator, so they are read-only here.");
        let Some(snapshot) = &self.snapshot else {
            return section;
        };
        let query = self.query(cx);
        let items = engine::visible(snapshot.system(), &query, self.show_apple);
        if items.is_empty() {
            return section.child(self.empty_agents(snapshot.system().next().is_some(), "system"));
        }
        let rows: Vec<Row> = items
            .into_iter()
            .map(|item| self.agent_row(item, cx))
            .collect();
        section.child(ListCard::new().children(rows))
    }

    fn empty_agents(&self, has_any: bool, which: &str) -> EmptyState {
        if has_any {
            EmptyState::new("search", "Nothing to show")
                .message("No items match your search. Apple's items are hidden unless you turn on Apple items.")
        } else if which == "user" {
            EmptyState::new("power", "No user agents")
                .message("Nothing in ~/Library/LaunchAgents starts automatically.")
        } else {
            EmptyState::new("server", "No system agents or daemons")
                .message("Nothing in /Library/LaunchAgents or /Library/LaunchDaemons.")
        }
    }

    fn agent_row(&self, item: &LaunchItem, cx: &mut Context<Self>) -> Row {
        let status = item.status();
        let icon_tone = if item.read_error.is_some() || item.program_missing {
            Tone::Warning
        } else if status == Status::Running {
            Tone::Success
        } else {
            Tone::Neutral
        };
        let busy = self.acting.is_some();
        let mut row = Row::new(SharedString::from(item.id.clone()), item.label.clone())
            .icon(if item.scope == engine::Scope::Daemon {
                "server"
            } else {
                "power"
            })
            .icon_tone(icon_tone)
            .subtitle(item.subtitle());
        for b in view::agent_badges(item) {
            let tone = match b.tone {
                BadgeTone::Neutral => Tone::Neutral,
                BadgeTone::Accent => Tone::Accent,
                BadgeTone::Success => Tone::Success,
                BadgeTone::Warning => Tone::Warning,
            };
            row = row.badge(Badge::new(b.label).tone(tone));
        }

        // launchd ignores a file without a Label: there is nothing to turn
        // on or off, only the file to remove.
        if item.is_editable() && !item.has_no_label() {
            let toggled = item.clone();
            row = row.action(
                Switch::new(SharedString::from(format!("toggle:{}", item.id)))
                    .checked(item.enabled())
                    .disabled(busy)
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
        let plist = item.plist.clone();
        row = row.action(
            Button::new(SharedString::from(format!("reveal:{}", item.id)))
                .small()
                .ghost()
                .icon(icon("folder-search"))
                .tooltip("Reveal plist in Finder")
                .on_click(move |_, _, _| fs_util::reveal_in_finder(&plist)),
        );
        if item.is_editable() {
            let removed = item.clone();
            row = row.action(
                Button::new(SharedString::from(format!("remove:{}", item.id)))
                    .small()
                    .ghost()
                    .icon(icon("trash"))
                    .tooltip("Move to Trash")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_remove(&removed, window, cx)
                    })),
            );
        }
        row
    }
}

fn login_row(item: &LoginItem) -> Row {
    let mut row = Row::new(SharedString::from(item.id()), item.name.clone()).icon("log-in");
    row = row.subtitle(view::login_subtitle(item));
    if let Some(label) = view::login_badge(item) {
        row = row.badge(Badge::new(label));
    }
    if !item.path.is_empty() {
        let path = std::path::PathBuf::from(&item.path);
        row = row.action(
            Button::new(SharedString::from(format!("reveal-login:{}", item.id())))
                .small()
                .ghost()
                .icon(icon("folder-search"))
                .tooltip("Reveal in Finder")
                .on_click(move |_, _, _| fs_util::reveal_in_finder(&path)),
        );
    }
    row
}

fn login_error_banner(error: &LoginError, cx: &mut Context<StartupScreen>) -> Banner {
    match error {
        LoginError::NotAuthorized => Banner::new(Tone::Warning, "Login items need permission")
            .message("Allow Nera Cleaner to control System Events under Privacy & Security > Automation, then refresh.")
            .action(
                Button::new("open-automation")
                    .small()
                    .outline()
                    .label("Open System Settings")
                    .on_click(|_, _, _| fs_util::open(AUTOMATION_PANE)),
            ),
        LoginError::TimedOut => Banner::new(Tone::Warning, "System Events didn't respond")
            .message("If macOS asked for permission, allow it. Then try again.")
            .action(
                Button::new("login-retry")
                    .small()
                    .outline()
                    .label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            ),
        LoginError::Failed(message) => {
            Banner::new(Tone::Danger, "Couldn't read login items").message(message.clone())
        }
    }
}

impl Render for StartupScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let loading = self.is_loading();
        let subtitle = view::page_subtitle(self.snapshot.as_ref());
        let mut page = Page::new("startup-page", Screen::Startup.title())
            .subtitle(subtitle)
            .toolbar(self.render_toolbar(cx))
            .action(
                Button::new("refresh")
                    .primary()
                    .icon(icon("refresh-cw"))
                    .label("Refresh")
                    .loading(loading)
                    .disabled(loading || self.acting.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        for banner in self.render_banners() {
            page = page.child(banner);
        }
        if let Some(stats) = self.render_stats() {
            page = page.child(stats);
        }
        page.child(self.render_login_items(cx))
            .child(self.render_user_agents(cx))
            .child(self.render_system_agents(cx))
    }
}

/// Show a toast once the current update finishes (async callbacks have no
/// `Window` of their own).
fn toast_later(window: AnyWindowHandle, cx: &mut App, tone: Tone, title: String, message: String) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
    });
}
