//! Processes screen: live CPU and memory use of apps and processes, with
//! search, filters, sorting and a guarded Quit / Force quit.

use crate::engine::{
    self, Control, Filter, Me, ProcessInfo, QuitMode, QuitOutcome, Sampler, Snapshot, SortKey,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    h_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the list is re-sampled.
const REFRESH: Duration = Duration::from_secs(2);
/// Rows drawn at once; the rest is reachable through search and sorting.
const MAX_ROWS: usize = 150;
/// Fixed lanes so memory and the actions line up between rows and header.
const MEMORY_W: Pixels = px(88.);
const CONTROLS_W: Pixels = px(176.);
/// Per-process CPU (% of one core) from which the value is highlighted.
const HIGH_CPU: f32 = 50.0;

/// The highlight threshold in the platform's CPU unit: macOS reports % of
/// one core, Windows (like Task Manager) % of all logical processors.
#[cfg(target_os = "macos")]
fn high_cpu(_processors: usize) -> f32 {
    HIGH_CPU
}
#[cfg(windows)]
fn high_cpu(processors: usize) -> f32 {
    HIGH_CPU / processors.max(1) as f32
}

/// Badge of a process whose quitting has a side effect (`Control::Caution`).
#[cfg(target_os = "macos")]
const CAUTION_BADGE: &str = "Restarts automatically";
#[cfg(windows)]
const CAUTION_BADGE: &str = "Runs the taskbar";

pub struct ProcessesScreen {
    sampler: Arc<Mutex<Sampler>>,
    me: Me,
    snapshot: Option<Snapshot>,
    /// Last sampling error; the previous snapshot stays visible meanwhile.
    error: Option<String>,

    filter: Filter,
    sort: SortKey,
    search: Entity<InputState>,
    query: String,

    /// Key of the process being quit, so the same job can't start twice.
    quitting: Option<String>,
    /// The Refresh button was pressed and its sample has not arrived yet.
    manual_refresh: bool,
    _sample_task: Option<Task<()>>,
    _quit_task: Option<Task<()>>,
    _refresh: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ProcessesScreen {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search by name, user or PID"));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this: &mut Self, state, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        );
        let mut this = Self {
            sampler: Arc::new(Mutex::new(Sampler::new())),
            me: Me::current(),
            snapshot: None,
            error: None,
            filter: Filter::default(),
            sort: SortKey::default(),
            search,
            query: String::new(),
            quitting: None,
            manual_refresh: false,
            _sample_task: None,
            _quit_task: None,
            _refresh: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        // Screens stay alive after the first visit: sample only while shown,
        // and catch up at once when the user comes back.
        this._refresh = refresh_every(cx, REFRESH, |this: &mut Self, cx| {
            if nav::current(cx) == Screen::Processes {
                this.sample(cx);
            }
        });
        this._subscriptions
            .push(cx.observe_global::<nav::Nav>(|this, cx| {
                if nav::current(cx) == Screen::Processes {
                    this.sample(cx);
                }
            }));
        this.sample(cx);
        this
    }

    // MARK: - Actions

    /// Take a sample on a background thread (the sampler keeps the previous
    /// counters, so CPU deltas stay correct). Never overlaps itself.
    fn sample(&mut self, cx: &mut Context<Self>) {
        if self._sample_task.is_some() {
            return;
        }
        let sampler = self.sampler.clone();
        self._sample_task = Some(run_blocking(
            cx,
            move || {
                sampler
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sample()
                    .map_err(|e| e.to_string())
            },
            |this, result, _| {
                this._sample_task = None;
                this.manual_refresh = false;
                match result {
                    Ok(snapshot) => {
                        this.snapshot = Some(snapshot);
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err),
                }
            },
        ));
    }

    fn request_quit(
        &mut self,
        p: ProcessInfo,
        mode: QuitMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting.is_some() {
            return;
        }
        let control = engine::control(&p, &self.me);
        if let Control::Protected { reason, .. } = control {
            notify(
                window,
                cx,
                Tone::Warning,
                format!("Can't quit {}", p.name),
                reason,
            );
            return;
        }
        let side_effect = match control {
            Control::Caution(note) => format!(" {note}"),
            _ => String::new(),
        };
        let spec = match mode {
            QuitMode::Force => Confirm::destructive(
                format!("Force quit {}?", p.name),
                format!(
                    "{} (PID {}) stops immediately. Unsaved work in it is lost.{side_effect}",
                    p.name, p.pid
                ),
                "Force quit",
            ),
            QuitMode::Quit => Confirm::normal(
                format!("Quit {}?", p.name),
                // macOS: SIGTERM ends most apps at once, without their
                // "save changes?" prompt. Windows closes the app's windows.
                if cfg!(target_os = "macos") {
                    format!(
                        "{} (PID {}) is asked to exit. Unsaved work in it may be lost.{side_effect}",
                        p.name, p.pid
                    )
                } else {
                    format!(
                        "{} (PID {}) is asked to close. It may ask you to save your work first.{side_effect}",
                        p.name, p.pid
                    )
                },
                "Quit",
            ),
        };
        let entity = cx.entity();
        confirm(spec, false, window, cx, move |_, cx| {
            let p = p.clone();
            entity.update(cx, |this, cx| this.quit(p, mode, cx));
        });
    }

    fn quit(&mut self, p: ProcessInfo, mode: QuitMode, cx: &mut Context<Self>) {
        if self.quitting.is_some() {
            return;
        }
        self.quitting = Some(p.key());
        let me = self.me.clone();
        let name = p.name.clone();
        self._quit_task = Some(run_blocking(
            cx,
            move || engine::quit(&p, &me, mode),
            move |this, outcome, cx| {
                this.quitting = None;
                this._quit_task = None;
                let (tone, title, message) = quit_toast(&name, mode, outcome);
                let id = cx.entity_id();
                cx.with_window(id, |window, cx| notify(window, cx, tone, title, message));
                // Show the result right away instead of waiting for the next tick.
                this.sample(cx);
            },
        ));
        cx.notify();
    }

    // MARK: - Rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_ix = Filter::ALL
            .iter()
            .position(|f| *f == self.filter)
            .unwrap_or(0);
        let sort_ix = SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
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
                TabBar::new("processes-filter")
                    .segmented()
                    .small()
                    .selected_index(filter_ix)
                    .children(Filter::ALL.map(|f| Tab::new().label(f.label())))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.filter = Filter::ALL.get(*ix).copied().unwrap_or_default();
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap(sp::SM)
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Sort by"),
                    )
                    .child(
                        TabBar::new("processes-sort")
                            .segmented()
                            .small()
                            .selected_index(sort_ix)
                            .children(SortKey::ALL.map(|s| Tab::new().label(s.label())))
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.sort = SortKey::ALL.get(*ix).copied().unwrap_or_default();
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_stats(&self, snap: &Snapshot) -> impl IntoElement {
        let t = engine::stats_text(snap, self.me.uid);
        StatGrid::new(3)
            .child(
                StatTile::new("CPU load", t.cpu_value)
                    .caption(t.cpu_caption)
                    .icon("cpu")
                    .tone(Tone::for_usage(t.cpu_fraction))
                    .meter(t.cpu_fraction),
            )
            .child(
                StatTile::new(engine::MEMORY_TILE, t.memory_value)
                    .caption(t.memory_caption)
                    .icon("memory-stick")
                    .tone(Tone::for_usage(t.memory_fraction))
                    .meter(t.memory_fraction),
            )
            .child(
                StatTile::new("Processes", t.processes_value)
                    .caption(t.processes_caption)
                    .icon("activity"),
            )
    }

    fn render_row(&self, p: &ProcessInfo, cx: &mut Context<Self>) -> Row {
        let control = engine::control(p, &self.me);
        let busy = self.quitting.is_some();
        let is_app = p.is_app();
        let key = p.key();
        let processors = self.snapshot.as_ref().map_or(1, |s| s.cores);

        let memory_cell = div()
            .w(MEMORY_W)
            .flex_shrink_0()
            .text_right()
            .text_sm()
            .font_medium()
            .when(p.memory.is_none(), |d| {
                d.text_color(cx.theme().muted_foreground)
            })
            .child(engine::memory_cell(p));

        let controls = div()
            .w(CONTROLS_W)
            .flex_shrink_0()
            .h_flex()
            .justify_end()
            .gap(sp::XS)
            .map(|d| match &control {
                Control::Protected { badge, .. } => d.child(Badge::new(*badge)),
                _ => {
                    let quit_target = p.clone();
                    let force_target = p.clone();
                    d.child(
                        Button::new(SharedString::from(format!("{key}-quit")))
                            .small()
                            .outline()
                            .label("Quit")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_quit(quit_target.clone(), QuitMode::Quit, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{key}-force")))
                            .small()
                            .ghost()
                            .text_color(Tone::Danger.color(cx))
                            .label("Force quit")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_quit(force_target.clone(), QuitMode::Force, window, cx)
                            })),
                    )
                }
            });

        let subtitle = engine::subtitle(p);
        let row = Row::new(SharedString::from(key), p.name.clone())
            .icon(if is_app { "app-window" } else { "terminal" })
            .icon_tone(if is_app { Tone::Accent } else { Tone::Neutral })
            .subtitle(subtitle)
            .value(engine::cpu_cell(p))
            .value_tone(if p.cpu.is_some_and(|c| c >= high_cpu(processors)) {
                Tone::Warning
            } else {
                Tone::Neutral
            })
            .action(
                h_flex()
                    .gap(sp::SM)
                    .items_center()
                    .child(memory_cell)
                    .child(controls),
            );
        match control {
            Control::Caution(_) => row.badge(Badge::new(CAUTION_BADGE).tone(Tone::Warning)),
            _ => row,
        }
    }

    fn render_list(&self, snap: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        if self.filter == Filter::Apps && !snap.apps_known {
            return EmptyState::new("app-window", "Can't tell which processes are apps")
                .message(concat!(
                    nera_core::os_name!(),
                    " didn't report its list of running apps. Switch to My processes or All."
                ))
                .into_any_element();
        }
        let list = engine::visible(
            &snap.processes,
            self.filter,
            self.sort,
            &self.query,
            self.me.uid,
        );
        if list.is_empty() {
            let searching = !self.query.trim().is_empty();
            let (title, message) = if searching {
                (
                    "No matching processes",
                    "Nothing matches your search in this view. Try another name, or switch to All.",
                )
            } else {
                match self.filter {
                    Filter::Apps => (
                        "No apps running",
                        "Switch to My processes or All to see background processes.",
                    ),
                    _ => (
                        "No processes",
                        concat!(
                            nera_core::os_name!(),
                            " reported no processes for this view."
                        ),
                    ),
                }
            };
            return EmptyState::new(if searching { "search" } else { "cpu" }, title)
                .message(message)
                .into_any_element();
        }

        let total = list.len();
        let header = h_flex()
            .w_full()
            .gap(sp::MD)
            .items_center()
            .child(div().flex_1().font_medium().child("Process"))
            .child(
                div()
                    .min_w(MEMORY_W)
                    .text_right()
                    .font_medium()
                    .child("CPU"),
            )
            .child(
                h_flex()
                    .gap(sp::SM)
                    .child(div().w(MEMORY_W).text_right().font_medium().child("Memory"))
                    .child(div().w(CONTROLS_W)),
            );
        let rows: Vec<Row> = list
            .into_iter()
            .take(MAX_ROWS)
            .map(|p| self.render_row(p, cx))
            .collect();
        let hidden = total.saturating_sub(rows.len());
        let muted = cx.theme().muted_foreground;
        v_flex_gap()
            .child(ListCard::new().header(header).children(rows))
            .when(hidden > 0, |d| {
                d.child(div().text_sm().text_color(muted).child(format!(
                    "Showing the first {MAX_ROWS} of {total} processes. Search or change the sort to find the rest."
                )))
            })
            .when(cfg!(windows), |d| {
                d.child(div().text_sm().text_color(muted).child(
                    "Memory is the private working set and CPU the share of all logical processors, as in Task Manager's Details tab.",
                ))
            })
            .into_any_element()
    }
}

fn v_flex_gap() -> Div {
    div().w_full().flex().flex_col().gap(sp::MD)
}

/// Toast (tone, title, message) describing what happened to a quit request.
fn quit_toast(name: &str, mode: QuitMode, outcome: QuitOutcome) -> (Tone, String, String) {
    match outcome {
        QuitOutcome::Exited => (
            Tone::Success,
            format!(
                "{} {name}",
                if mode == QuitMode::Force {
                    "Force quit"
                } else {
                    "Quit"
                }
            ),
            "The process has exited.".to_string(),
        ),
        QuitOutcome::StillRunning => match mode {
            QuitMode::Quit => (
                Tone::Warning,
                format!("{name} is still running"),
                "It may be waiting for you to save work. Use Force quit to stop it now."
                    .to_string(),
            ),
            QuitMode::Force => (
                Tone::Warning,
                format!("{name} is still running"),
                concat!(
                    nera_core::os_name!(),
                    " hasn't finished stopping it yet. Check again in a moment."
                )
                .to_string(),
            ),
        },
        QuitOutcome::AlreadyGone => (
            Tone::Info,
            format!("{name} had already closed"),
            "Nothing was stopped.".to_string(),
        ),
        QuitOutcome::Refused(reason) => (
            Tone::Warning,
            format!("Can't quit {name}"),
            reason.to_string(),
        ),
        QuitOutcome::Failed(err) => (Tone::Danger, format!("Couldn't quit {name}"), err),
    }
}

impl Render for ProcessesScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Only a refresh the user asked for disables the button; the
        // background samples every 2 s would make it blink.
        let refreshing = self.manual_refresh;
        let mut page = Page::new("processes-page", Screen::Processes.title())
            .subtitle("Apps and processes using CPU and memory, refreshed about every 2 seconds")
            .action(
                Button::new("processes-refresh")
                    .primary()
                    .icon(icon("refresh-cw"))
                    .label("Refresh")
                    .disabled(refreshing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sample(cx);
                        this.manual_refresh = this._sample_task.is_some();
                        cx.notify();
                    })),
            );

        if self.snapshot.is_some() {
            page = page.toolbar(self.render_toolbar(cx));
        }

        let mut body: Vec<AnyElement> = Vec::new();
        if let Some(err) = &self.error {
            let banner = Banner::new(
                Tone::Warning,
                if self.snapshot.is_some() {
                    "Couldn't refresh the process list"
                } else {
                    "Couldn't read the process list"
                },
            )
            .message(err.clone())
            .action(
                Button::new("processes-retry")
                    .outline()
                    .small()
                    .label("Try again")
                    .disabled(refreshing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sample(cx);
                        this.manual_refresh = this._sample_task.is_some();
                        cx.notify();
                    })),
            );
            body.push(Section::untitled().child(banner).into_any_element());
        }

        match self.snapshot.clone() {
            None if self.error.is_none() => {
                body.push(
                    Section::untitled()
                        .child(
                            ProgressPanel::new("processes-loading", "Reading processes")
                                .detail("Measuring CPU use over a short interval")
                                .fraction(None),
                        )
                        .into_any_element(),
                );
            }
            None => {}
            Some(snap) => {
                body.push(
                    Section::untitled()
                        .child(self.render_stats(&snap))
                        .into_any_element(),
                );
                #[cfg(target_os = "macos")]
                if self.filter == Filter::All {
                    body.push(
                        Section::untitled()
                            .child(
                                Banner::new(Tone::Info, "Some figures are hidden")
                                    .message("macOS only shares CPU and memory use for your own processes. System and other accounts' processes show a dash."),
                            )
                            .into_any_element(),
                    );
                }
                #[cfg(windows)]
                if self.filter == Filter::All {
                    body.push(
                        Section::untitled()
                            .child(
                                Banner::new(Tone::Info, "Some owners are hidden")
                                    .message("Windows only tells apps running as administrator who runs system and service processes. Those rows show no user name and can't be quit from Nera."),
                            )
                            .into_any_element(),
                    );
                }
                let title = match self.filter {
                    Filter::Apps => "Apps",
                    Filter::Mine => "My processes",
                    Filter::All => "All processes",
                };
                body.push(
                    Section::new(title)
                        .child(self.render_list(&snap, cx))
                        .into_any_element(),
                );
            }
        }
        page.children(body)
    }
}
