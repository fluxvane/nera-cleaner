//! Applications on Windows: lists the desktop programs Settings > Apps shows
//! and opens a program's own uninstaller after a confirmation. Nera Cleaner
//! never deletes program files itself; it waits until Windows no longer lists
//! the app and refreshes.

use crate::engine::{self, InstalledApp, RunningSet, Scope, SelfIdentity, SortKey};
use crate::view;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    StyledExt as _,
};
use gpui_kit::*;
use nera_core::fs_util;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use std::time::{Duration, Instant};

/// How often the "Running" badges are re-checked.
const RUNNING_POLL: Duration = Duration::from_secs(3);
/// How often Nera checks whether an uninstalled app is gone from the registry.
const WAIT_POLL: Duration = Duration::from_secs(2);
/// How long Nera keeps checking after it opened an uninstaller.
const WAIT_LIMIT: Duration = Duration::from_secs(15 * 60);

/// An uninstaller Nera opened and whose result it is waiting for.
struct Waiting {
    name: String,
}

/// Result of the last uninstall, shown as a banner until the next action.
struct Outcome {
    tone: Tone,
    title: String,
    message: String,
}

pub struct AppsScreen {
    apps: Vec<InstalledApp>,
    unreadable: Vec<Scope>,
    loading: bool,
    launching: bool,
    search: Entity<InputState>,
    query: String,
    sort: SortKey,
    /// Id of the selected app.
    selected: Option<String>,
    waiting: Option<Waiting>,
    outcome: Option<Outcome>,
    _discover_task: Option<Task<()>>,
    _launch_task: Option<Task<()>>,
    _wait_task: Option<Task<()>>,
    _running_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl AppsScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search apps"));
        let sub = cx.subscribe_in(
            &search,
            window,
            |this, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        );
        let mut this = Self {
            apps: Vec::new(),
            unreadable: Vec::new(),
            loading: false,
            launching: false,
            search,
            query: String::new(),
            sort: SortKey::Size,
            selected: None,
            waiting: None,
            outcome: None,
            _discover_task: None,
            _launch_task: None,
            _wait_task: None,
            _running_task: None,
            _subs: vec![sub],
        };
        this.reload(cx);
        this.watch_running(cx);
        this
    }

    // MARK: Discovery

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self._discover_task = Some(run_blocking(
            cx,
            || engine::discover(&SelfIdentity::current(), &RunningSet::snapshot()),
            |this, found, cx| {
                this.loading = false;
                this._discover_task = None;
                this.apps = found.apps;
                this.unreadable = found.unreadable;
                if let Some(sel) = &this.selected {
                    if !this.apps.iter().any(|a| &a.id == sel && a.can_uninstall()) {
                        this.selected = None;
                    }
                }
                cx.notify();
            },
        ));
        cx.notify();
    }

    // MARK: Selection

    fn selected_app(&self) -> Option<&InstalledApp> {
        let sel = self.selected.as_ref()?;
        self.apps.iter().find(|a| &a.id == sel)
    }

    fn toggle_select(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        } else if self.apps.iter().any(|a| a.id == id && a.can_uninstall()) {
            self.selected = Some(id.to_string());
            self.outcome = None;
        }
        cx.notify();
    }

    // MARK: Running

    /// Re-check which apps run every few seconds, so a "Running" badge never
    /// outlives the app and a freshly launched app is flagged.
    fn watch_running(&mut self, cx: &mut Context<Self>) {
        self._running_task = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(RUNNING_POLL).await;
            // Screens stay alive after the first visit: look at the running
            // processes only while this one is shown.
            match this.update(cx, |_, cx| nav::current(cx) == Screen::Apps) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let snapshot = cx.background_spawn(async { RunningSet::snapshot() }).await;
            let alive = this.update(cx, |this, cx| {
                let mut changed = false;
                for app in &mut this.apps {
                    let now = snapshot.is_app_running(app);
                    if app.running != now {
                        app.running = now;
                        changed = true;
                    }
                }
                if changed {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }));
    }

    // MARK: Uninstall

    fn busy(&self) -> bool {
        self.launching || self.waiting.is_some()
    }

    fn request_uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(app) = self.selected_app().filter(|a| a.can_uninstall()).cloned() else {
            return;
        };
        let (title, message, button) = view::confirm_text(&app.name);
        let this = cx.entity().downgrade();
        let handle = window.window_handle();
        confirm(
            Confirm::destructive(title, message, button),
            false,
            window,
            cx,
            move |_window, cx| {
                let app = app.clone();
                let _ = this.update(cx, |this, cx| this.start_uninstall(app, handle, cx));
            },
        );
    }

    fn start_uninstall(
        &mut self,
        app: InstalledApp,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some(cmd) = app.uninstall.clone().filter(|_| app.can_uninstall()) else {
            return;
        };
        if self.busy() {
            return;
        }
        self.launching = true;
        self.outcome = None;
        self._launch_task = Some(run_blocking(
            cx,
            move || engine::launch_uninstaller(&cmd),
            move |this, result, cx| {
                this.launching = false;
                this._launch_task = None;
                match result {
                    Ok(()) => this.start_waiting(app, window, cx),
                    Err(err) => {
                        log::warn!("uninstall {}: {err:?}", app.id);
                        let (title, message) = view::launch_failed_text(&app.name, &err);
                        let tone = match err {
                            engine::LaunchError::Cancelled => Tone::Info,
                            _ => Tone::Danger,
                        };
                        this.show_outcome(tone, title, message, window, cx);
                    }
                }
            },
        ));
        cx.notify();
    }

    /// Poll the registry until Windows no longer lists the app, then refresh.
    fn start_waiting(
        &mut self,
        app: InstalledApp,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.waiting = Some(Waiting {
            name: app.name.clone(),
        });
        let (scope, key, name) = (app.scope, app.key.clone(), app.name.clone());
        self._wait_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let gone = loop {
                cx.background_executor().timer(WAIT_POLL).await;
                let key = key.clone();
                let listed = cx
                    .background_spawn(async move { engine::is_still_listed(scope, &key) })
                    .await;
                if !listed {
                    break true;
                }
                if started.elapsed() >= WAIT_LIMIT {
                    break false;
                }
                if this.update(cx, |this, _| this.waiting.is_some()).ok() != Some(true) {
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.waiting = None;
                this._wait_task = None;
                let (tone, (title, message)) = if gone {
                    (Tone::Success, view::uninstalled_text(&name))
                } else {
                    (Tone::Neutral, view::still_listed_text(&name))
                };
                if gone {
                    system_notify(cx, &title, message);
                }
                this.show_outcome(tone, title, message.to_string(), window, cx);
                this.reload(cx);
            });
        }));
        cx.notify();
    }

    fn stop_waiting(&mut self, cx: &mut Context<Self>) {
        self.waiting = None;
        self._wait_task = None;
        self.reload(cx);
    }

    fn show_outcome(
        &mut self,
        tone: Tone,
        title: String,
        message: String,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let (t, m) = (title.clone(), message.clone());
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, t, m));
        });
        self.outcome = Some(Outcome {
            tone,
            title,
            message,
        });
        cx.notify();
    }

    // MARK: Rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let visible = engine::filter_sort(&self.apps, &self.query, self.sort);
        let sorts = [
            (SortKey::Size, "Size"),
            (SortKey::Name, "Name"),
            (SortKey::InstallDate, "Install date"),
        ];
        let mut group = ButtonGroup::new("sort").small().outline();
        for (key, label) in sorts {
            group = group.child(
                Button::new(SharedString::from(format!("sort-{label}")))
                    .label(label)
                    .selected(self.sort == key),
            );
        }
        let group = group.on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
            if let Some((key, _)) = clicked.first().and_then(|ix| sorts.get(*ix)) {
                this.sort = *key;
                cx.notify();
            }
        }));
        h_flex()
            .w_full()
            .items_center()
            .gap(sp::LG)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .cleanable(true)
                        .prefix(icon("search").size(dim::ICON_SM)),
                ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(sp::SM)
                    .child(div().text_sm().text_color(muted).child("Sort by"))
                    .child(group),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(view::toolbar_summary(&self.apps, &visible)),
            )
    }

    fn render_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(w) = &self.waiting {
            let (title, message) = view::waiting_text(&w.name);
            out.push(
                Banner::new(Tone::Info, title)
                    .message(message)
                    .action(
                        Button::new("stop-waiting")
                            .small()
                            .ghost()
                            .label("Stop waiting")
                            .on_click(cx.listener(|this, _, _, cx| this.stop_waiting(cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(o) = &self.outcome {
            out.push(
                Banner::new(o.tone, o.title.clone())
                    .message(o.message.clone())
                    .action(
                        Button::new("dismiss-outcome")
                            .small()
                            .ghost()
                            .label("Dismiss")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.outcome = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if !self.unreadable.is_empty() {
            out.push(
                Banner::new(Tone::Warning, "Some installed apps could not be read")
                    .message("Windows refused access to part of the installed apps list, so some apps may be missing.")
                    .into_any_element(),
            );
        }
        out
    }

    fn render_stats(&self) -> impl IntoElement {
        let st = view::stats(&self.apps);
        StatGrid::new(3)
            .child(
                StatTile::new("Installed apps", st.apps_value)
                    .icon("package")
                    .caption(st.apps_caption),
            )
            .child(
                StatTile::new("Space used", st.space_value)
                    .icon("hard-drive")
                    .caption(st.space_caption),
            )
            .child(
                StatTile::new("Running now", st.running_value)
                    .icon("activity")
                    .tone(if st.running_any {
                        Tone::Warning
                    } else {
                        Tone::Neutral
                    })
                    .caption(st.running_caption),
            )
    }

    fn render_app_row(&self, app: &InstalledApp, cx: &mut Context<Self>) -> Row {
        let t = cx.theme();
        let leading = div()
            .size(dim::ICON_TILE)
            .rounded(t.radius)
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.secondary)
            .child(
                icon("app-window")
                    .size(dim::ICON_MD)
                    .text_color(t.muted_foreground),
            );
        let selected = self.selected.as_deref() == Some(app.id.as_str());
        let mut row = Row::new(
            SharedString::from(format!("app:{}", app.id)),
            app.name.clone(),
        )
        .subtitle(view::app_subtitle(app))
        .leading(leading)
        .value(view::app_size(app))
        .selected(selected);
        if app.running {
            row = row.badge(Badge::new("Running").tone(Tone::Warning));
        }
        if let Some(text) = view::blocker_badge(app) {
            let tone = if app.protection == engine::Protection::SelfApp {
                Tone::Accent
            } else {
                Tone::Neutral
            };
            row = row.badge(Badge::new(text).tone(tone));
        }
        if app.can_uninstall() {
            let id = app.id.clone();
            row.action(
                icon(if selected {
                    "chevron-down"
                } else {
                    "chevron-right"
                })
                .size(dim::ICON_SM)
                .text_color(t.muted_foreground),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_select(&id, cx)))
        } else {
            row.action(div().w(dim::ICON_SM)).disabled(true)
        }
    }

    fn render_detail(&self, app: &InstalledApp, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, border, bg) = (t.muted_foreground, t.border, t.secondary);
        let mut panel = v_flex().w_full().bg(bg);
        if app.running {
            panel = panel.child(
                div().p(sp::MD).child(
                    Banner::new(Tone::Warning, format!("{} is running", app.name))
                        .message("Its uninstaller may ask you to close it first."),
                ),
            );
        }
        if let Some(folder) = &app.install_location {
            let reveal = folder.clone();
            panel = panel.child(
                div().w_full().border_b_1().border_color(border).child(
                    Row::new(
                        SharedString::from(format!("folder:{}", app.id)),
                        "Install folder",
                    )
                    .icon("folder-open")
                    .subtitle(folder.display().to_string())
                    .action(
                        Button::new(SharedString::from(format!("reveal:{}", app.id)))
                            .ghost()
                            .small()
                            .icon(icon("folder-open"))
                            .tooltip(nera_core::os::SHOW_IN_FILE_MANAGER)
                            .on_click(move |_, _, _| fs_util::reveal_in_finder(&reveal)),
                    ),
                ),
            );
        }
        if let Some(cmd) = &app.uninstall {
            panel = panel.child(
                div().w_full().border_b_1().border_color(border).child(
                    Row::new(
                        SharedString::from(format!("uninstaller:{}", app.id)),
                        "Uninstaller",
                    )
                    .icon("external-link")
                    .subtitle(cmd.display()),
                ),
            );
        }
        panel
            .child(
                div()
                    .w_full()
                    .px(sp::LG)
                    .py(sp::SM)
                    .text_xs()
                    .text_color(muted)
                    .child(view::LEFTOVERS_NOTE),
            )
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let visible = engine::filter_sort(&self.apps, &self.query, self.sort);
        if visible.is_empty() {
            return if self.apps.is_empty() {
                EmptyState::new("package", "No apps found")
                    .message("Nera Cleaner reads the programs listed in Settings > Apps > Installed apps.")
                    .action(
                        Button::new("empty-refresh")
                            .outline()
                            .icon(icon("refresh-cw"))
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element()
            } else {
                EmptyState::new("search", "No matching apps")
                    .message("Try another name or publisher.")
                    .into_any_element()
            };
        }
        let mut list = ListCard::new().header(
            h_flex()
                .w_full()
                .gap(sp::MD)
                .child(
                    div()
                        .flex_1()
                        .child(list_header("apps", "App", Some("Size"), None)),
                )
                .child(div().w(dim::ICON_SM)),
        );
        for i in visible {
            let app = &self.apps[i];
            list = list.child(self.render_app_row(app, cx));
            if self.selected.as_deref() == Some(app.id.as_str()) {
                list = list.child(self.render_detail(app, cx));
            }
        }
        list.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let app = self.selected_app()?;
        let muted = cx.theme().muted_foreground;
        let footer = view::footer(app, self.waiting.is_some());
        let can = footer.ready && !self.launching;
        Some(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap(sp::LG)
                .child(
                    v_flex()
                        .min_w_0()
                        .gap(sp::XXS)
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .truncate()
                                .child(footer.summary),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(footer.caption),
                        ),
                )
                .child(
                    Button::new("uninstall")
                        .danger()
                        .icon(icon("trash"))
                        .label(footer.button)
                        .loading(self.launching)
                        .disabled(!can)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.request_uninstall(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for AppsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = Page::new("apps-page", Screen::Apps.title())
            .subtitle("Uninstall programs with their own uninstallers")
            .action(
                Button::new("refresh")
                    .outline()
                    .icon(icon("refresh-cw"))
                    .label("Refresh")
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );
        if !self.apps.is_empty() {
            page = page.toolbar(self.render_toolbar(cx));
        }
        if let Some(footer) = self.render_footer(cx) {
            page = page.footer(footer);
        }
        let banners = self.render_banners(cx);
        if !banners.is_empty() {
            page = page.child(Section::untitled().children(banners));
        }
        if self.loading && self.apps.is_empty() {
            page = page.child(
                ProgressPanel::new("discover-progress", "Looking for apps")
                    .detail("Reading the installed apps list Windows keeps"),
            );
        }
        if !self.apps.is_empty() {
            page = page.child(Section::untitled().child(self.render_stats()));
            page = page.child(
                Section::new("Apps")
                    .description("Desktop programs from Settings > Apps > Installed apps. Microsoft Store apps aren't listed. Select an app to open its uninstaller.")
                    .child(self.render_list(cx)),
            );
        } else if !self.loading {
            page = page.child(Section::untitled().child(self.render_list(cx)));
        }
        page
    }
}
