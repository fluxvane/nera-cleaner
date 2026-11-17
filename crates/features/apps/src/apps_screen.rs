//! Apps: an uninstaller. Lists installed applications, shows the leftovers an
//! app keeps in `~/Library`, and moves the app plus the chosen leftovers to the
//! Trash after a confirmation.

use crate::engine::{
    self, InstalledApp, Leftover, LeftoverQuery, LeftoverScan, Protection, RunningSet,
    SelfIdentity, SortKey, UninstallPlan, UninstallReport,
};
use crate::view;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_core::format;
use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
const APP_MANAGEMENT_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles";
/// How often the "Running" badges are re-checked.
const RUNNING_POLL: Duration = Duration::from_secs(3);

/// Facts about one app gathered in the background after discovery.
struct Enriched {
    path: PathBuf,
    size: Option<u64>,
    icon_png: Option<PathBuf>,
    /// `Some` when Spotlight was asked; the inner `None` means "no usage data".
    last_used: Option<Option<SystemTime>>,
}

/// Leftovers of the selected app.
struct Leftovers {
    app: PathBuf,
    scanning: bool,
    items: Vec<Leftover>,
    checked: HashSet<PathBuf>,
    denied: Vec<PathBuf>,
    shared_skipped: usize,
}

/// Result of the last uninstall, shown as a banner until the next action.
struct Outcome {
    tone: Tone,
    title: String,
    message: String,
    settings_pane: Option<&'static str>,
}

pub struct AppsScreen {
    apps: Vec<InstalledApp>,
    unreadable: Vec<PathBuf>,
    loading: bool,
    measuring: bool,
    uninstalling: bool,
    measure_progress: Shared<(usize, usize)>,
    enriched: Shared<Vec<Enriched>>,
    search: Entity<InputState>,
    query: String,
    sort: SortKey,
    show_apple: bool,
    selected: Option<PathBuf>,
    leftovers: Option<Leftovers>,
    outcome: Option<Outcome>,
    /// Apps whose usage date Spotlight was asked for (many have none, and
    /// must not be asked again on every reload).
    usage_asked: HashSet<PathBuf>,
    measure_cancel: Cancel,
    leftover_cancel: Cancel,
    _discover_task: Option<Task<()>>,
    _measure_task: Option<Task<()>>,
    _ticker: Option<Task<()>>,
    _leftover_task: Option<Task<()>>,
    _uninstall_task: Option<Task<()>>,
    _quit_task: Option<Task<()>>,
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
            measuring: false,
            uninstalling: false,
            usage_asked: HashSet::new(),
            measure_progress: Shared::new((0, 0)),
            enriched: Shared::new(Vec::new()),
            search,
            query: String::new(),
            sort: SortKey::Size,
            show_apple: false,
            selected: None,
            leftovers: None,
            outcome: None,
            measure_cancel: Cancel::new(),
            leftover_cancel: Cancel::new(),
            _discover_task: None,
            _measure_task: None,
            _ticker: None,
            _leftover_task: None,
            _uninstall_task: None,
            _quit_task: None,
            _running_task: None,
            _subs: vec![sub],
        };
        this.reload(false, cx);
        this.watch_running(cx);
        this
    }

    // MARK: Discovery

    fn busy(&self) -> bool {
        self.loading || self.uninstalling
    }

    /// Discover apps again. `keep_measured`: reuse sizes and usage dates of apps
    /// that are still installed (right after an uninstall, when nothing else
    /// changed); a user refresh measures everything anew so no stale size,
    /// version or usage date survives.
    fn reload(&mut self, keep_measured: bool, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.measure_cancel.cancel();
        self._measure_task = None;
        self._ticker = None;
        self.measuring = false;
        self.loading = true;
        self._discover_task = Some(run_blocking(
            cx,
            || {
                let running = RunningSet::snapshot();
                engine::discover(&engine::default_roots(), &SelfIdentity::current(), &running)
            },
            move |this, found, cx| this.finish_discover(found, keep_measured, cx),
        ));
        cx.notify();
    }

    fn finish_discover(
        &mut self,
        found: engine::Discovery,
        keep_measured: bool,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        self._discover_task = None;
        if !keep_measured {
            self.usage_asked.clear();
        }
        let mut apps = found.apps;
        for app in &mut apps {
            if let Some(old) = self.apps.iter().find(|o| o.path == app.path) {
                // The icon only depends on the bundle's .icns file.
                app.icon_png = old.icon_png.clone();
                if keep_measured && old.version == app.version {
                    app.size = old.size;
                    app.last_used = old.last_used;
                }
            }
        }
        self.apps = apps;
        self.unreadable = found.unreadable;
        if let Some(sel) = &self.selected {
            if !self.apps.iter().any(|a| &a.path == sel) {
                self.deselect();
            }
        }
        self.start_measure(cx);
    }

    fn start_measure(&mut self, cx: &mut Context<Self>) {
        struct Todo {
            path: PathBuf,
            need_size: bool,
            icns: Option<PathBuf>,
        }
        let todo: Vec<Todo> = self
            .apps
            .iter()
            .filter(|a| {
                a.size.is_none()
                    || (a.last_used.is_none() && !self.usage_asked.contains(&a.path))
                    || (a.icon_file.is_some() && a.icon_png.is_none())
            })
            .map(|a| Todo {
                path: a.path.clone(),
                need_size: a.size.is_none(),
                icns: a.icon_file.clone().filter(|_| a.icon_png.is_none()),
            })
            .collect();
        if todo.is_empty() {
            cx.notify();
            return;
        }
        let cancel = Cancel::new();
        self.measure_cancel = cancel.clone();
        self.measuring = true;
        let progress = self.measure_progress.clone();
        progress.set((0, todo.len()));
        let out = self.enriched.clone();
        out.set(Vec::new());
        let out_work = out.clone();
        let cache = engine::icon_cache_dir();
        self._ticker = Some(refresh_every(cx, Duration::from_millis(250), |this, _| {
            this.drain_enriched()
        }));
        self._measure_task = Some(run_blocking(
            cx,
            move || {
                let done = AtomicUsize::new(0);
                let total = todo.len();
                todo.par_iter().for_each(|t| {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let size = t.need_size.then(|| engine::app_size(&t.path, &cancel));
                    if cancel.is_cancelled() {
                        return; // a cancelled size is partial
                    }
                    let icon_png = t.icns.as_ref().and_then(|p| engine::icon_png(p, &cache));
                    out_work.update(|v| {
                        v.push(Enriched {
                            path: t.path.clone(),
                            size,
                            icon_png,
                            last_used: None,
                        })
                    });
                    progress.set((done.fetch_add(1, Ordering::Relaxed) + 1, total));
                });
                // Usage dates are asked for even after "Cancel": cancelling only stops the
                // slow size measurement, and without this every app would read "No usage
                // data" although Spotlight was never asked.
                let paths: Vec<PathBuf> = todo.iter().map(|t| t.path.clone()).collect();
                let dates = engine::last_used_dates(&paths);
                out_work.update(|v| {
                    for (path, last_used) in paths.into_iter().zip(dates) {
                        v.push(Enriched {
                            path,
                            size: None,
                            icon_png: None,
                            last_used: Some(last_used),
                        });
                    }
                });
            },
            |this, (), cx| {
                this.drain_enriched();
                this.measuring = false;
                this._ticker = None;
                this._measure_task = None;
                // A cancelled run may have skipped the selected app: measure
                // it alone, or its panel would wait for a size forever.
                if let Some(sel) = this.selected.clone() {
                    if this.apps.iter().any(|a| a.path == sel && a.size.is_none()) {
                        this.select(&sel, cx);
                    }
                }
                cx.notify();
            },
        ));
        cx.notify();
    }

    fn drain_enriched(&mut self) {
        let batch = {
            let mut taken = Vec::new();
            self.enriched.update(|v| std::mem::swap(v, &mut taken));
            taken
        };
        for e in batch {
            if let Some(app) = self.apps.iter_mut().find(|a| a.path == e.path) {
                if e.size.is_some() {
                    app.size = e.size;
                }
                if e.icon_png.is_some() {
                    app.icon_png = e.icon_png;
                }
                if let Some(date) = e.last_used {
                    app.last_used = date;
                    self.usage_asked.insert(e.path);
                }
            }
        }
    }

    /// Order of the list. While sizes are still arriving, sorting by size
    /// would move rows under the pointer several times a second, so the list
    /// stays in name order until every size is known.
    fn list_order(&self) -> SortKey {
        if self.measuring && self.sort == SortKey::Size {
            SortKey::Name
        } else {
            self.sort
        }
    }

    fn cancel_measure(&mut self, cx: &mut Context<Self>) {
        self.measure_cancel.cancel();
        cx.notify();
    }

    // MARK: Selection and leftovers

    fn selected_app(&self) -> Option<&InstalledApp> {
        let sel = self.selected.as_ref()?;
        self.apps.iter().find(|a| &a.path == sel)
    }

    fn deselect(&mut self) {
        self.leftover_cancel.cancel();
        self._leftover_task = None;
        self.selected = None;
        self.leftovers = None;
    }

    fn toggle_select(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.uninstalling {
            return;
        }
        if self.selected.as_deref() == Some(path) {
            self.deselect();
        } else {
            self.select(path, cx);
        }
        cx.notify();
    }

    fn select(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(app) = self.apps.iter().find(|a| a.path == path).cloned() else {
            return;
        };
        if !app.is_removable() {
            return;
        }
        self.leftover_cancel.cancel();
        let cancel = Cancel::new();
        self.leftover_cancel = cancel.clone();
        self.outcome = None;
        self.selected = Some(app.path.clone());
        self.leftovers = Some(Leftovers {
            app: app.path.clone(),
            scanning: true,
            items: Vec::new(),
            checked: HashSet::new(),
            denied: Vec::new(),
            shared_skipped: 0,
        });
        let all = self.apps.clone();
        let library = paths::home_join("Library");
        let need_size = app.size.is_none();
        let app_path = app.path.clone();
        self._leftover_task = Some(run_blocking(
            cx,
            move || {
                let query = LeftoverQuery::for_app(&app, &all);
                let scan = engine::find_leftovers(&library, &query, &cancel);
                let size = (need_size && !cancel.is_cancelled())
                    .then(|| engine::app_size(&app.path, &cancel));
                (scan, size)
            },
            move |this, (scan, size), cx| this.finish_leftovers(&app_path, scan, size, cx),
        ));
    }

    fn finish_leftovers(
        &mut self,
        app: &Path,
        scan: LeftoverScan,
        size: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_deref() != Some(app) {
            return;
        }
        self._leftover_task = None;
        if let Some(size) = size {
            if let Some(a) = self.apps.iter_mut().find(|a| a.path == app) {
                a.size = Some(size);
            }
        }
        self.leftovers = Some(Leftovers {
            app: app.to_path_buf(),
            scanning: false,
            checked: scan.items.iter().map(|l| l.path.clone()).collect(),
            items: scan.items,
            denied: scan.denied,
            shared_skipped: scan.shared_skipped,
        });
        cx.notify();
    }

    fn set_leftover(&mut self, path: &Path, on: bool, cx: &mut Context<Self>) {
        if let Some(lo) = self.leftovers.as_mut() {
            if on {
                lo.checked.insert(path.to_path_buf());
            } else {
                lo.checked.remove(path);
            }
        }
        cx.notify();
    }

    fn set_all_leftovers(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(lo) = self.leftovers.as_mut() {
            lo.checked = if on {
                lo.items.iter().map(|l| l.path.clone()).collect()
            } else {
                HashSet::new()
            };
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

    // MARK: Quit

    fn quit_selected(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.selected_app() else {
            return;
        };
        engine::request_quit(&app.bundle_id);
        self._quit_task = Some(cx.spawn(async move |this, cx| {
            for _ in 0..20 {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let snapshot = cx.background_spawn(async { RunningSet::snapshot() }).await;
                let still_running = this.update(cx, |this, cx| {
                    for app in &mut this.apps {
                        app.running = snapshot.is_app_running(app);
                    }
                    cx.notify();
                    this.selected_app().is_some_and(|a| a.running)
                });
                if !matches!(still_running, Ok(true)) {
                    break;
                }
            }
        }));
    }

    // MARK: Uninstall

    fn request_uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((plan, name, protection)) = self.build_plan() else {
            return;
        };
        if self.busy() {
            return;
        }
        let left_n = plan.leftovers.len();
        let left_size: u64 = plan.leftovers.iter().map(|l| l.size).sum();
        let message = view::confirm_message(&name, plan.app_size, left_n, left_size);
        let this = cx.entity().downgrade();
        let handle = window.window_handle();
        confirm(
            Confirm::destructive(format!("Uninstall {name}?"), message, "Uninstall"),
            false,
            window,
            cx,
            move |_window, cx| {
                let plan = plan.clone();
                let name = name.clone();
                let _ = this.update(cx, |this, cx| {
                    this.start_uninstall(plan, name, protection, handle, cx)
                });
            },
        );
    }

    fn build_plan(&self) -> Option<(UninstallPlan, String, Protection)> {
        let app = self.selected_app()?;
        let lo = self
            .leftovers
            .as_ref()
            .filter(|l| !l.scanning && l.app == app.path)?;
        if !app.is_removable() || app.running {
            return None;
        }
        let app_size = app.size?;
        let leftovers = lo
            .items
            .iter()
            .filter(|l| lo.checked.contains(&l.path))
            .cloned()
            .collect();
        Some((
            UninstallPlan {
                app: app.path.clone(),
                app_size,
                leftovers,
            },
            app.name.clone(),
            app.protection,
        ))
    }

    fn start_uninstall(
        &mut self,
        plan: UninstallPlan,
        name: String,
        protection: Protection,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            return;
        }
        self.uninstalling = true;
        self.outcome = None;
        let library = paths::home_join("Library");
        let work_plan = plan.clone();
        self._uninstall_task = Some(run_blocking(
            cx,
            move || {
                engine::uninstall(
                    &work_plan,
                    &library,
                    protection,
                    |p| RunningSet::snapshot().is_running(p),
                    fs_util::trash_item,
                )
            },
            move |this, report, cx| this.finish_uninstall(&plan, &name, report, window, cx),
        ));
        cx.notify();
    }

    fn finish_uninstall(
        &mut self,
        plan: &UninstallPlan,
        name: &str,
        report: UninstallReport,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.uninstalling = false;
        self._uninstall_task = None;
        for (path, reason) in &report.failures {
            log::warn!("uninstall: {}: {reason}", path.display());
        }
        let first_failure = report
            .failures
            .first()
            .map(|(p, r)| format!("{}: {r}", tilde(p)))
            .unwrap_or_default();
        let outcome = if report.refused_running {
            Outcome {
                tone: Tone::Warning,
                title: format!("{name} is running"),
                message: "Quit the app, then uninstall it. Nothing was moved.".into(),
                settings_pane: None,
            }
        } else if !report.app_removed {
            Outcome {
                tone: Tone::Danger,
                title: format!("Couldn't uninstall {name}"),
                message: if report.app_permission_denied {
                    "macOS blocked the change. Allow Nera Cleaner under App Management (or Full Disk Access), then try again."
                        .to_string()
                } else {
                    format!("Nothing was moved. {first_failure}")
                },
                settings_pane: report.app_permission_denied.then_some(APP_MANAGEMENT_PANE),
            }
        } else if report.failures.is_empty() {
            Outcome {
                tone: Tone::Success,
                title: format!("Uninstalled {name}"),
                message: view::moved_message(report.leftovers_removed, report.freed),
                settings_pane: None,
            }
        } else {
            Outcome {
                tone: Tone::Warning,
                title: format!(
                    "Uninstalled {name}, but {} stayed behind",
                    format::count(report.failures.len(), "item")
                ),
                message: view::partial_message(report.freed, &first_failure),
                settings_pane: None,
            }
        };
        if report.app_removed {
            system_notify(cx, &outcome.title, &outcome.message);
            self.apps.retain(|a| a.path != plan.app);
        }
        let (tone, title, message) = (outcome.tone, outcome.title.clone(), outcome.message.clone());
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
        });
        self.outcome = Some(outcome);
        self.deselect();
        self.reload(true, cx);
        cx.notify();
    }

    // MARK: Rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let visible = engine::filter_sort(
            &self.apps,
            &self.query,
            self.list_order(),
            self.show_apple,
            SystemTime::now(),
        );
        let sorts = [
            (SortKey::Size, "Size"),
            (SortKey::Name, "Name"),
            (SortKey::LastUsed, "Last used"),
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
                Checkbox::new("show-apple")
                    .label("Show Apple apps")
                    .checked(self.show_apple)
                    .on_change(cx.listener(|this, on: &bool, _, cx| {
                        this.show_apple = *on;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(view::toolbar_summary(&self.apps, &visible, self.measuring)),
            )
    }

    fn render_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(o) = &self.outcome {
            let mut banner = Banner::new(o.tone, o.title.clone()).message(o.message.clone());
            let dismiss = Button::new("dismiss-outcome")
                .small()
                .ghost()
                .label("Dismiss")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.outcome = None;
                    cx.notify();
                }));
            banner = match o.settings_pane {
                Some(pane) => banner.action(
                    h_flex().gap(sp::SM).child(dismiss).child(
                        Button::new("open-app-management")
                            .small()
                            .outline()
                            .label("Open System Settings")
                            .on_click(move |_, _, _| fs_util::open(pane)),
                    ),
                ),
                None => banner.action(dismiss),
            };
            out.push(banner.into_any_element());
        }
        if !self.unreadable.is_empty() {
            let first = tilde(&self.unreadable[0]);
            out.push(
                Banner::new(Tone::Warning, "Some app folders could not be read")
                    .message(format!(
                        "Nera Cleaner can't list {first}. Grant Full Disk Access, then refresh."
                    ))
                    .action(
                        Button::new("open-fda-apps")
                            .small()
                            .outline()
                            .label("Open System Settings")
                            .on_click(|_, _, _| fs_util::open(FULL_DISK_ACCESS_PANE)),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn render_stats(&self) -> impl IntoElement {
        let st = view::stats(&self.apps, self.measuring, SystemTime::now());
        StatGrid::new(3)
            .child(
                StatTile::new("Removable apps", st.removable_value)
                    .icon("package")
                    .caption(st.removable_caption),
            )
            .child(
                StatTile::new("Space used", st.space_value)
                    .icon("hard-drive")
                    .caption(st.space_caption),
            )
            .child(
                StatTile::new("Not used in 6 months", st.unused_value)
                    .icon("clock")
                    .tone(if st.unused_any {
                        Tone::Warning
                    } else {
                        Tone::Neutral
                    })
                    .caption(st.unused_caption),
            )
    }

    fn render_app_row(&self, app: &InstalledApp, cx: &mut Context<Self>) -> Row {
        let t = cx.theme();
        let tile = div()
            .size(dim::ICON_TILE)
            .rounded(t.radius)
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        let leading = match &app.icon_png {
            Some(png) => tile.child(img(png.clone()).size(dim::ICON_TILE)),
            None => tile.bg(t.secondary).child(
                icon("app-window-mac")
                    .size(dim::ICON_MD)
                    .text_color(t.muted_foreground),
            ),
        };
        let path = app.path.clone();
        let selected = self.selected.as_deref() == Some(app.path.as_path());
        let mut row = Row::new(
            SharedString::from(format!("app:{}", app.path.display())),
            app.name.clone(),
        )
        .subtitle(view::app_subtitle(app, self.measuring, SystemTime::now()))
        .leading(leading)
        .value(view::app_size(app))
        .selected(selected);
        if app.running {
            row = row.badge(Badge::new("Running").tone(Tone::Warning));
        }
        match app.protection {
            Protection::Removable => {
                row = row
                    .action(
                        icon(if selected {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        })
                        .size(dim::ICON_SM)
                        .text_color(t.muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_select(&path, cx)));
            }
            Protection::Apple => {
                row = row
                    .badge(Badge::new("Apple"))
                    .action(div().w(dim::ICON_SM))
                    .disabled(true)
            }
            Protection::SelfApp => {
                row = row
                    .badge(Badge::new("This app").tone(Tone::Accent))
                    .action(div().w(dim::ICON_SM))
                    .disabled(true)
            }
        }
        row
    }

    fn render_detail(
        &self,
        app: &InstalledApp,
        lo: &Leftovers,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (muted, border, bg) = (t.muted_foreground, t.border, t.secondary);
        let mut panel = v_flex().w_full().bg(bg);

        if app.running {
            panel = panel.child(
                div().p(sp::MD).child(
                    Banner::new(Tone::Warning, format!("{} is running", app.name))
                        .message("Quit it before uninstalling.")
                        .action(
                            Button::new("quit-app")
                                .small()
                                .outline()
                                .label("Quit app")
                                .on_click(cx.listener(|this, _, _, cx| this.quit_selected(cx))),
                        ),
                ),
            );
        }

        if lo.scanning {
            return panel
                .child(
                    h_flex()
                        .px(sp::LG)
                        .py(sp::MD)
                        .gap(sp::SM)
                        .items_center()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child("Looking for leftover files…"),
                        ),
                )
                .into_any_element();
        }

        if !lo.denied.is_empty() {
            panel = panel.child(
                div().p(sp::MD).child(
                    Banner::new(Tone::Info, "Some Library folders could not be read")
                        .message("Leftovers inside them are not listed. Grant Full Disk Access to include them.")
                        .action(
                            Button::new("open-fda-leftovers")
                                .small()
                                .outline()
                                .label("Open System Settings")
                                .on_click(|_, _, _| fs_util::open(FULL_DISK_ACCESS_PANE)),
                        ),
                ),
            );
        }

        if lo.items.is_empty() {
            return panel
                .child(
                    div()
                        .px(sp::LG)
                        .py(sp::MD)
                        .text_sm()
                        .text_color(muted)
                        .child(
                        "No leftover files found. Only the app itself will be moved to the Trash.",
                    ),
                )
                .into_any_element();
        }

        let all_checked = lo.items.iter().all(|l| lo.checked.contains(&l.path));
        let checked_size: u64 = lo
            .items
            .iter()
            .filter(|l| lo.checked.contains(&l.path))
            .map(|l| l.size)
            .sum();
        let has_banner = app.running || !lo.denied.is_empty();
        panel = panel.child(
            h_flex()
                .w_full()
                .gap(sp::MD)
                .px(sp::LG)
                .py(sp::SM)
                .border_b_1()
                .when(has_banner, |d| d.border_t_1())
                .border_color(border)
                .text_xs()
                .text_color(muted)
                .child(div().flex_1().child(list_header(
                    "leftovers",
                    view::leftover_header(lo.checked.len(), checked_size),
                    Some("Size"),
                    Some((
                        all_checked,
                        Box::new(
                            cx.listener(|this, on: &bool, _, cx| this.set_all_leftovers(*on, cx)),
                        ),
                    )),
                )))
                // Same width as the "Show in Finder" button in each row.
                .child(div().w(dim::ICON_LG + sp::XS)),
        );
        let count = lo.items.len();
        for (i, item) in lo.items.iter().enumerate() {
            let path = item.path.clone();
            let reveal = item.path.clone();
            let name = item
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parent = item.path.parent().map(tilde).unwrap_or_default();
            let key = item.path.display().to_string();
            let row = Row::new(SharedString::from(format!("leftover:{key}")), name)
                .icon(item.kind.icon())
                .subtitle(view::leftover_subtitle(item.kind, &parent))
                .value(view::bytes(item.size))
                .checkbox(
                    lo.checked.contains(&item.path),
                    cx.listener(move |this, on: &bool, _, cx| this.set_leftover(&path, *on, cx)),
                )
                .action(
                    Button::new(SharedString::from(format!("reveal:{key}")))
                        .ghost()
                        .small()
                        .icon(icon("folder-open"))
                        .tooltip("Show in Finder")
                        .on_click(move |_, _, _| fs_util::reveal_in_finder(&reveal)),
                );
            panel = panel.child(
                div()
                    .w_full()
                    .when(i + 1 < count, |d| d.border_b_1().border_color(border))
                    .child(row),
            );
        }
        if lo.shared_skipped > 0 {
            panel = panel.child(
                div()
                    .w_full()
                    .px(sp::LG)
                    .py(sp::SM)
                    .border_t_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(view::shared_note(lo.shared_skipped)),
            );
        }
        panel.into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let visible = engine::filter_sort(
            &self.apps,
            &self.query,
            self.list_order(),
            self.show_apple,
            SystemTime::now(),
        );
        if visible.is_empty() {
            return if self.apps.is_empty() {
                EmptyState::new("package", "No apps found")
                    .message("Nera Cleaner looked in /Applications and ~/Applications.")
                    .action(
                        Button::new("empty-refresh")
                            .outline()
                            .icon(icon("refresh-cw"))
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(false, cx))),
                    )
                    .into_any_element()
            } else {
                EmptyState::new("search", "No matching apps")
                    .message("Try another name, or show Apple apps.")
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
            if self.selected.as_deref() == Some(app.path.as_path()) {
                if let Some(lo) = self.leftovers.as_ref().filter(|l| l.app == app.path) {
                    list = list.child(self.render_detail(app, lo, cx));
                }
            }
        }
        list.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let app = self.selected_app()?;
        let lo = self.leftovers.as_ref().filter(|l| l.app == app.path)?;
        let muted = cx.theme().muted_foreground;
        let left_n = lo
            .items
            .iter()
            .filter(|l| lo.checked.contains(&l.path))
            .count();
        let left_size: u64 = lo
            .items
            .iter()
            .filter(|l| lo.checked.contains(&l.path))
            .map(|l| l.size)
            .sum();
        let footer = view::footer(app, lo.scanning, left_n, left_size);
        let can = footer.ready && !self.busy();
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
                        .loading(self.uninstalling)
                        .disabled(!can)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.request_uninstall(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Drop for AppsScreen {
    fn drop(&mut self) {
        self.measure_cancel.cancel();
        self.leftover_cancel.cancel();
    }
}

impl Render for AppsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = Page::new("apps-page", Screen::Apps.title())
            .subtitle("Uninstall apps together with the files they leave behind")
            .action(
                Button::new("refresh")
                    .outline()
                    .icon(icon("refresh-cw"))
                    .label("Refresh")
                    .disabled(self.busy())
                    .on_click(cx.listener(|this, _, _, cx| this.reload(false, cx))),
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
        if self.loading {
            page = page.child(
                ProgressPanel::new("discover-progress", "Looking for apps")
                    .detail("Reading /Applications and ~/Applications"),
            );
        } else if self.uninstalling {
            page = page.child(
                ProgressPanel::new("uninstall-progress", "Moving to the Trash")
                    .detail("Your apps and files are being moved. This can take a moment."),
            );
        } else if self.measuring {
            let (done, total) = self.measure_progress.get();
            let fraction = (total > 0).then(|| done as f64 / total as f64);
            page = page.child(
                ProgressPanel::new("measure-progress", "Measuring apps")
                    .detail(format!("{done} of {total}"))
                    .fraction(fraction)
                    .on_cancel(cx.listener(|this, _, _, cx| this.cancel_measure(cx))),
            );
        }
        if !self.apps.is_empty() {
            page = page.child(Section::untitled().child(self.render_stats()));
            page = page.child(
                Section::new("Apps")
                    .description("Select an app to see the files it left in your Library.")
                    .child(self.render_list(cx)),
            );
        } else if !self.loading {
            page = page.child(Section::untitled().child(self.render_list(cx)));
        }
        page
    }
}

/// `~/Library/...` for paths inside the home folder.
fn tilde(path: &Path) -> String {
    let home = paths::home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
