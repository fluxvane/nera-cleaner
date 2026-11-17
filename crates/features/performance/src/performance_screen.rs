//! Performance screen: live CPU / GPU / memory / swap / uptime stats (sampled every
//! 2 s on a background thread) and one-click maintenance tasks.
//!
//! Port of `PerformanceView` / `PerformanceViewModel`. Each maintenance task
//! asks for confirmation, runs off the UI thread (admin tasks show the macOS
//! password prompt or the Windows UAC prompt) and records its last successful
//! run in `maintenance.json`.
//!
//! On Windows the tiles follow Task Manager: no swap, memory pressure or load
//! average tile (Windows has none), a "Committed" tile instead, and no GPU
//! tile when Windows reports no GPU counters.

use crate::engine::maintenance::{self, RunError, TaskId};
use crate::engine::present::{self, TileText};
#[cfg(target_os = "macos")]
use crate::engine::stats::Pressure;
use crate::engine::stats::{self, Snapshot};
use crate::engine::time;
use crate::engine::MaintenanceLog;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::*;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use std::time::Duration;

/// How often live statistics are re-read.
const REFRESH: Duration = Duration::from_secs(2);
/// How often a hidden screen checks whether it is shown again.
const HIDDEN_POLL: Duration = Duration::from_millis(300);

/// Result of the most recent maintenance run that needs the user's attention.
struct Outcome {
    tone: Tone,
    title: String,
    message: String,
}

pub struct PerformanceScreen {
    snapshot: Option<Snapshot>,
    stats_error: Option<String>,
    tasks: Vec<TaskId>,
    log: MaintenanceLog,
    running: Option<TaskId>,
    outcome: Option<Outcome>,
    _stats_task: Option<Task<()>>,
    _run_task: Option<Task<()>>,
}

impl PerformanceScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let stats_task = cx.spawn(async move |this, cx| {
            let mut prev = None;
            loop {
                // Screens stay alive after the first visit: sample only while
                // shown. Coming back starts a fresh CPU window.
                match this.update(cx, |_, cx| nav::current(cx) == Screen::Performance) {
                    Ok(true) => {}
                    Ok(false) => {
                        prev = None;
                        cx.background_executor().timer(HIDDEN_POLL).await;
                        continue;
                    }
                    Err(_) => return,
                }
                let result = cx
                    .background_executor()
                    .spawn(async move { stats::sample(prev) })
                    .await;
                let alive = this
                    .update(cx, |view, cx| {
                        match result {
                            Ok(snap) => {
                                prev = Some(snap.ticks);
                                view.stats_error = None;
                                // Keep the last CPU figure when the window was too short.
                                let cpu = snap
                                    .cpu
                                    .or_else(|| view.snapshot.as_ref().and_then(|s| s.cpu));
                                view.snapshot = Some(Snapshot { cpu, ..snap });
                            }
                            Err(err) => {
                                // Never keep showing numbers from before a failed read.
                                view.snapshot = None;
                                view.stats_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    return;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });

        Self {
            snapshot: None,
            stats_error: None,
            tasks: maintenance::available_tasks(),
            log: MaintenanceLog::load(),
            running: None,
            outcome: None,
            _stats_task: Some(stats_task),
            _run_task: None,
        }
    }

    /// Ask for confirmation, then run `id`.
    fn request(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let this = cx.entity();
        confirm(
            Confirm::normal(id.confirm_title(), id.confirm_message(), id.confirm_label()),
            false,
            window,
            cx,
            move |window, cx| {
                this.update(cx, |view, cx| view.start(id, window, cx));
            },
        );
    }

    fn start(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        self.running = Some(id);
        self.outcome = None;
        let handle = window.window_handle();

        self._run_task = Some(run_blocking(
            cx,
            move || {
                let result = maintenance::run(id);
                let mut log = MaintenanceLog::load();
                match &result {
                    Ok(_) => log.record_success(id, time::now_unix()),
                    Err(RunError::Cancelled) => {}
                    Err(err) => log.record_failure(id, err.message()),
                }
                if let Err(err) = log.save() {
                    ::log::warn!("could not save maintenance log: {err}");
                }
                (result, log)
            },
            move |view, (result, log), cx| {
                view.log = log;
                view.running = None;
                view._run_task = None;
                let (tone, title, message) = match result {
                    Ok(msg) => (Tone::Success, format!("{} done", id.title()), msg),
                    Err(RunError::Cancelled) => (
                        Tone::Info,
                        format!("{} cancelled", id.title()),
                        RunError::Cancelled.message(),
                    ),
                    Err(err @ RunError::NotAuthorized(_)) => {
                        view.outcome = Some(Outcome {
                            tone: Tone::Warning,
                            title: format!("{} needs an administrator", id.title()),
                            message: err.message(),
                        });
                        (
                            Tone::Warning,
                            format!("{} not allowed", id.title()),
                            err.message(),
                        )
                    }
                    Err(err @ RunError::Failed(_)) => {
                        view.outcome = Some(Outcome {
                            tone: Tone::Danger,
                            title: format!("{} failed", id.title()),
                            message: err.message(),
                        });
                        (
                            Tone::Danger,
                            format!("{} failed", id.title()),
                            err.message(),
                        )
                    }
                };
                if tone == Tone::Success {
                    system_notify(cx, "Nera Cleaner", &message);
                }
                let _ = handle.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
            },
        ));
    }

    fn live_section(&self) -> Section {
        let section = Section::new("Live status").trailing(Badge::new("Updates every 2 s"));
        let Some(s) = &self.snapshot else {
            return section
                .child(Card::new().child(div().text_sm().child("Reading system statistics…")));
        };

        let cpu = tile(&present::cpu(s.cpu.as_ref()));
        let cpu = match s.cpu {
            Some(c) => cpu.tone(Tone::for_usage(c.busy())).meter(c.busy()),
            None => cpu.tone(Tone::Neutral),
        };
        // macOS always shows the GPU tile ("Unavailable" when IOKit reports
        // none); Windows hides it when it has no GPU counters.
        let gpu = (cfg!(target_os = "macos") || !s.gpus.is_empty()).then(|| {
            let gpu = tile(&present::gpu(&s.gpus));
            match present::busiest_gpu(&s.gpus) {
                Some(g) => gpu
                    .tone(Tone::for_usage(g.utilization))
                    .meter(g.utilization),
                None => gpu,
            }
        });
        let memory = tile(&present::memory(&s.memory))
            .tone(memory_tone(s))
            .meter(s.memory.used_fraction());
        let disk = match &s.disk {
            Some(d) => tile(&present::disk(Some(d)))
                .tone(Tone::for_usage(d.space.used_fraction()))
                .meter(d.space.used_fraction()),
            None => tile(&present::disk(None)),
        };
        let uptime = tile(&present::uptime(s.uptime_secs, s.boot_time));

        let mut tiles = vec![cpu.icon("cpu")];
        tiles.extend(gpu.map(|g| g.icon("monitor")));
        tiles.push(memory.icon("memory-stick"));
        tiles.push(disk.icon("hard-drive"));
        tiles.extend(platform_tiles(s));
        tiles.push(uptime.icon("clock"));
        // Eight tiles on macOS fill two rows of four; Windows' five or six
        // read better in rows of three.
        let columns = if tiles.len() % 4 == 0 { 4 } else { 3 };
        section.child(StatGrid::new(columns).children(tiles))
    }

    /// Per-GPU detail: overall, renderer and tiler load plus GPU memory.
    fn graphics_section(&self, cx: &App) -> Option<Section> {
        let gpus = &self.snapshot.as_ref()?.gpus;
        if gpus.is_empty() {
            return None;
        }
        let mut section = Section::new("Graphics").description(if cfg!(target_os = "macos") {
            "GPU load as reported by the graphics driver, refreshed every 2 seconds."
        } else {
            "GPU load as Task Manager shows it (the busiest engine), refreshed every 2 seconds."
        });
        for gpu in gpus {
            let mut card = Card::new();
            for (i, (label, value)) in present::gpu_rows(gpu).into_iter().enumerate() {
                card = card.child(kv_row(label, value, cx));
                if i == 0 {
                    card = card
                        .child(Meter::new(gpu.utilization).tone(Tone::for_usage(gpu.utilization)));
                }
            }
            section = section.child(card);
        }
        Some(section)
    }

    fn memory_section(&self, cx: &App) -> Option<Section> {
        let s = self.snapshot.as_ref()?;
        let mut card = Card::new();
        for (label, value) in present::memory_rows(&s.memory) {
            card = card.child(kv_row(label, value, cx));
        }
        Some(
            Section::new("Memory breakdown")
                .description(MEMORY_DESCRIPTION)
                .child(card),
        )
    }

    /// `None` on Windows when no task is offered (an empty section helps no one).
    fn maintenance_section(&self, cx: &mut Context<Self>) -> Option<Section> {
        if cfg!(windows) && self.tasks.is_empty() {
            return None;
        }
        let busy = self.running.is_some();
        let now = time::now_unix();
        let mut section = Section::new("Maintenance").description(MAINTENANCE_DESCRIPTION);

        if let Some(o) = &self.outcome {
            section =
                section.child(Banner::new(o.tone, o.title.clone()).message(o.message.clone()));
        }

        if self.tasks.is_empty() {
            return Some(section.child(
                EmptyState::new("wrench", "No maintenance tasks available").message(
                    "The system tools these tasks rely on aren't present on this version of macOS.",
                ),
            ));
        }

        let mut list = ListCard::new();
        for &id in &self.tasks {
            let entry = self.log.entry(id);
            let last_run = entry.and_then(|e| e.last_run);
            let failed = entry.is_some_and(|e| e.last_error.is_some());
            let is_running = self.running == Some(id);

            let mut row = Row::new(SharedString::from(format!("task-{}", id.key())), id.title())
                .subtitle(id.description())
                .icon(id.icon())
                .value(time::last_run_label(last_run, now));
            if is_running {
                row = row.badge(Badge::new("Running").tone(Tone::Accent));
            } else if failed {
                row = row.badge(Badge::new("Last attempt failed").tone(Tone::Danger));
            }
            if id.long_running() {
                row = row.badge(Badge::new("Takes hours").tone(Tone::Warning));
            }
            if id.needs_admin() {
                row = row.badge(Badge::new("Admin"));
            }
            row = row.action(
                Button::new(SharedString::from(format!("run-{}", id.key())))
                    .small()
                    .outline()
                    .label("Run")
                    .loading(is_running)
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| this.request(id, window, cx))),
            );
            list = list.child(row);
        }
        Some(section.child(list))
    }
}

#[cfg(target_os = "macos")]
const MEMORY_DESCRIPTION: &str = "App memory, wired, compressed and cached files are defined as in Activity Monitor; sizes use binary units as Activity Monitor does (1 GB = 1,024 MB). Free is memory on the free list. Cached files are released automatically when apps need room.";
#[cfg(windows)]
const MEMORY_DESCRIPTION: &str = "In use, available, cached, pools and committed are defined as in Task Manager; sizes use binary units as Task Manager does (1 GB = 1,024 MB). Cached memory is released automatically when apps need room. Committed counts memory backed by RAM or the page file.";

#[cfg(target_os = "macos")]
const MAINTENANCE_DESCRIPTION: &str =
    "Fixes for common macOS slowdowns. Tasks marked Admin ask for your password.";
#[cfg(windows)]
const MAINTENANCE_DESCRIPTION: &str =
    "Fixes for common problems. Tasks marked Admin may show a Windows administrator prompt.";

/// Memory tile colour: macOS's pressure level; Windows has none, so usage.
#[cfg(target_os = "macos")]
fn memory_tone(s: &Snapshot) -> Tone {
    s.pressure.map_or(Tone::Neutral, pressure_tone)
}
#[cfg(windows)]
fn memory_tone(s: &Snapshot) -> Tone {
    Tone::for_usage(s.memory.used_fraction())
}

/// Swap, memory pressure and load average (macOS only).
#[cfg(target_os = "macos")]
fn platform_tiles(s: &Snapshot) -> Vec<StatTile> {
    let swap = tile(&present::swap(s.swap.as_ref()));
    let swap = match s.swap.filter(|w| w.total > 0) {
        None => swap,
        Some(w) => swap.tone(fill_tone(w.used as f64 / w.total as f64)),
    };
    let pressure =
        tile(&present::pressure(s.pressure)).tone(s.pressure.map_or(Tone::Neutral, pressure_tone));
    let load = tile(&present::load(
        s.load,
        s.cores,
        s.physical_cores,
        s.core_split,
    ));
    let load = match s.load {
        Some([a, ..]) => load.tone(Tone::for_usage((a / s.cores as f64).min(1.0))),
        None => load,
    };
    vec![
        swap.icon("layers"),
        pressure.icon("gauge"),
        load.icon("activity"),
    ]
}

/// Commit charge, Task Manager's "Committed" (Windows only).
#[cfg(windows)]
fn platform_tiles(s: &Snapshot) -> Vec<StatTile> {
    let frac = s.memory.commit_fraction();
    vec![tile(&present::committed(&s.memory))
        .tone(fill_tone(frac))
        .meter(frac)
        .icon("layers")]
}

/// Tone for swap / commit fill: neutral below half, warning from 50 %,
/// danger from 90 %.
fn fill_tone(frac: f64) -> Tone {
    if frac >= 0.9 {
        Tone::Danger
    } else if frac >= 0.5 {
        Tone::Warning
    } else {
        Tone::Neutral
    }
}

/// Meter / tile colour for memory pressure.
#[cfg(target_os = "macos")]
fn pressure_tone(p: Pressure) -> Tone {
    match p {
        Pressure::Normal => Tone::Success,
        Pressure::Warning => Tone::Warning,
        Pressure::Critical => Tone::Danger,
    }
}

impl Render for PerformanceScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.running.is_some();
        let mut page =
            Page::new("performance-page", Screen::Performance.title()).subtitle(concat!(
                "Live CPU, GPU and memory, plus tune-ups for a sluggish ",
                nera_core::computer!()
            ));
        // Free up RAM (`purge`) is macOS only; Windows has no honest
        // equivalent, so the header button is not shown there.
        if cfg!(target_os = "macos") {
            page =
                page.action(
                    Button::new("free-ram")
                        .primary()
                        .label("Free up RAM")
                        .loading(self.running == Some(TaskId::FreeRam))
                        .disabled(busy || !self.tasks.contains(&TaskId::FreeRam))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.request(TaskId::FreeRam, window, cx)
                        })),
                );
        }

        if let Some(err) = &self.stats_error {
            page = page.child(
                Banner::new(Tone::Danger, "Couldn't read system statistics").message(err.clone()),
            );
        }
        page = page.child(self.live_section());
        if let Some(section) = self.graphics_section(cx) {
            page = page.child(section);
        }
        if let Some(section) = self.memory_section(cx) {
            page = page.child(section);
        }
        if let Some(section) = self.maintenance_section(cx) {
            page = page.child(section);
        }
        page
    }
}

/// Overview tile from its pure text (tone and meter are added by the caller).
fn tile(t: &TileText) -> StatTile {
    let tile = StatTile::new(t.title, t.value.clone());
    match &t.caption {
        Some(c) => tile.caption(c.clone()),
        None => tile,
    }
}

#[cfg(test)]
mod tests {
    // No `use super::*`: the gpui glob import would shadow `#[test]`.
    use super::{fill_tone, Tone};
    #[cfg(target_os = "macos")]
    use super::{pressure_tone, Pressure};

    #[cfg(target_os = "macos")]
    #[test]
    fn pressure_maps_to_tones() {
        assert_eq!(pressure_tone(Pressure::Normal), Tone::Success);
        assert_eq!(pressure_tone(Pressure::Warning), Tone::Warning);
        assert_eq!(pressure_tone(Pressure::Critical), Tone::Danger);
    }

    #[test]
    fn fill_tones() {
        assert_eq!(fill_tone(0.1), Tone::Neutral);
        assert_eq!(fill_tone(0.5), Tone::Warning);
        assert_eq!(fill_tone(0.95), Tone::Danger);
    }
}
