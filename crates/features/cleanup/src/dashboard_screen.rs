//! Dashboard: disk, junk and last cleanup at a glance, Smart Scan, and
//! shortcuts to every other tool.

use crate::engine;
use crate::present;
use crate::store::{scan_store, Phase, ScanStore};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::*;
use nera_core::fs_util;
use nera_ui::nav::{navigate, Screen};
use nera_ui::*;
use std::time::Duration;

/// Tools reachable from the dashboard, with a one-line description.
const TOOLS: [(Screen, &str); 10] = [
    (
        Screen::Cleanup,
        "Review caches, logs and temporary files before removing them",
    ),
    (
        Screen::DevScan,
        "Find node_modules, build folders and other files projects can regenerate",
    ),
    (
        Screen::Duplicates,
        "Find identical files and keep a single copy",
    ),
    (Screen::Space, "See what takes up space, folder by folder"),
    (Screen::Apps, "Uninstall apps together with their leftovers"),
    (Screen::Processes, "See what is using CPU and memory"),
    (Screen::Startup, "Choose what opens when you log in"),
    (Screen::Performance, "Memory, CPU and disk activity"),
    (Screen::Battery, "Battery health, temperatures and fans"),
    (Screen::Privacy, "Clear browser traces and app history"),
];

#[cfg(target_os = "macos")]
const SCAN_CHECKS: &str = "Smart Scan checks caches, logs, browser data and developer tools. Nothing is removed until you review it and confirm.";
/// Browser data belongs to the Privacy screen on Windows.
#[cfg(windows)]
const SCAN_CHECKS: &str = "Smart Scan checks temporary files, caches, logs and developer tools. Nothing is removed until you review it and confirm.";

pub struct DashboardScreen {
    store: Entity<ScanStore>,
    _observe: Subscription,
    _tick: Task<()>,
}

impl DashboardScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = scan_store(cx);
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        // Free space changes while the app is open.
        let tick = refresh_every(cx, Duration::from_secs(30), |this, cx| {
            this.store.update(cx, |store, _| store.refresh_disk());
        });
        // The store may have been created long ago; show the disk as it is now.
        store.update(cx, |store, cx| {
            store.refresh_disk();
            store.auto_scan_if_enabled(cx);
        });
        Self {
            store,
            _observe: observe,
            _tick: tick,
        }
    }

    fn disk_tile(&self, cx: &App) -> StatTile {
        let disk = self.store.read(cx).disk();
        let t = present::disk_tile(disk);
        let tile = StatTile::new(t.title, t.value)
            .icon("hard-drive")
            .caption(t.caption);
        match (disk, t.fraction) {
            (Some(d), Some(f)) => tile.tone(Tone::for_usage(d.used_fraction())).meter(f),
            _ => tile,
        }
    }

    fn junk_tile(&self, cx: &App) -> StatTile {
        let store = self.store.read(cx);
        let t = present::junk_tile(store.results(), store.phase());
        let tile = StatTile::new(t.title, t.value)
            .icon("brush-cleaning")
            .caption(t.caption);
        match (store.results(), store.phase()) {
            (Some(r), Phase::Idle) => tile.tone(if r.total() > 0 {
                Tone::Accent
            } else {
                Tone::Success
            }),
            _ => tile,
        }
    }

    fn last_cleanup_tile(&self, cx: &App) -> StatTile {
        let last = self.store.read(cx).last_cleanup();
        let t = present::last_cleanup_tile(last, engine::unix_now());
        let tile = StatTile::new(t.title, t.value)
            .icon("sparkles")
            .caption(t.caption);
        if last.is_some() {
            tile.tone(Tone::Success)
        } else {
            tile
        }
    }

    fn render_progress(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let (title, p) = match store.phase() {
            Phase::Idle => return None,
            Phase::Scanning => (
                concat!("Scanning your ", nera_core::computer!()),
                store.scan_progress(),
            ),
            Phase::Cleaning => ("Cleaning", store.clean_progress()),
        };
        let handle = self.store.clone();
        let title = if store.is_cancelling() {
            "Stopping"
        } else {
            title
        };
        let detail = if p.current.is_empty() {
            "Getting ready".to_string()
        } else {
            format!("{} of {} · {}", p.done, p.total, p.current)
        };
        Some(
            ProgressPanel::new("dashboard-progress", title)
                .detail(detail)
                .fraction(p.fraction())
                .on_cancel(move |_, _, cx| handle.update(cx, |s, cx| s.cancel(cx)))
                .into_any_element(),
        )
    }

    fn render_summary(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        if store.is_busy() {
            return None;
        }
        let results = store.results()?;
        let muted = cx.theme().muted_foreground;
        let summary = present::dashboard_summary(results, engine::unix_now());
        let Some(body) = summary.body else {
            return Some(
                Section::new("Last scan")
                    .description(summary.description)
                    .child(
                        EmptyState::new("circle-check", "Nothing to clean")
                            .message("Caches, logs and temporary files are already tidy.")
                            .tone(Tone::Success),
                    )
                    .into_any_element(),
            );
        };
        let mut card = Card::new().child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap(sp::LG)
                .child(
                    v_flex()
                        .min_w_0()
                        .gap(sp::XXS)
                        .child(div().text_base().font_semibold().child(body.headline))
                        .child(div().text_sm().text_color(muted).child(body.detail)),
                )
                .child(
                    Button::new("review-and-clean")
                        .outline()
                        .label("Review and clean")
                        .on_click(|_, _, cx| navigate(Screen::Cleanup, cx)),
                ),
        );
        for (name, size) in body.top {
            card = card.child(kv_row(name, size, cx));
        }
        Some(
            Section::new("Last scan")
                .description(summary.description)
                .child(card)
                .into_any_element(),
        )
    }
}

impl Render for DashboardScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let busy = store.read(cx).is_busy();
        let scanning = store.read(cx).phase() == Phase::Scanning;
        let needs_fda = store
            .read(cx)
            .results()
            .is_some_and(|r| r.needs_full_disk_access());

        let scan = Button::new("smart-scan")
            .primary()
            .icon(icon("sparkles"))
            .label(if scanning { "Scanning" } else { "Smart Scan" })
            .disabled(busy)
            .on_click(move |_, _, cx| store.update(cx, |s, cx| s.start_scan(cx)));

        let tools = TOOLS
            .iter()
            .filter(|(screen, _)| screen.is_available())
            .fold(ListCard::new(), |list, &(screen, subtitle)| {
                list.child(
                    Row::new(
                        SharedString::from(format!("tool-{screen:?}")),
                        screen.title(),
                    )
                    .icon(screen.icon())
                    .subtitle(subtitle)
                    .action(
                        icon("chevron-right")
                            .size(dim::ICON_SM)
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(move |_, _, cx| navigate(screen, cx)),
                )
            });

        Page::new("dashboard-page", Screen::Dashboard.title())
            .subtitle(concat!("Your ", nera_core::computer!(), " at a glance"))
            .action(scan)
            .child(
                Section::untitled().child(
                    StatGrid::new(3)
                        .child(self.disk_tile(cx))
                        .child(self.junk_tile(cx))
                        .child(self.last_cleanup_tile(cx)),
                ),
            )
            .children(needs_fda.then(|| {
                Banner::new(Tone::Warning, "Some folders could not be read")
                    .message("Allow Full Disk Access so Nera can scan caches, logs and browser data in protected folders.")
                    .action(
                        Button::new("open-fda")
                            .small()
                            .outline()
                            .label("Open settings")
                            .on_click(|_, _, _| fs_util::open(engine::FULL_DISK_ACCESS_URL)),
                    )
            }))
            .children(self.render_progress(cx))
            .children(self.render_summary(cx))
            .children(
                (!busy && self.store.read(cx).results().is_none()).then(|| {
                    Section::untitled().child(
                        EmptyState::new("scan-search", "Find what is taking up space")
                            .message(SCAN_CHECKS),
                    )
                }),
            )
            .child(Section::new("Tools").description("Everything else Nera can do").child(tools))
    }
}
