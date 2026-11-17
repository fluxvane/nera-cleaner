//! System Junk: review what a scan found, pick categories and clean them.

use crate::engine;
use crate::present;
use crate::store::{categories, scan_store, summarize, Phase, ScanStore};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::*;
use nera_core::fs_util;
use nera_ui::nav::Screen;
use nera_ui::*;
use std::time::Duration;

#[cfg(target_os = "macos")]
const SCAN_CHECKS: &str = "Nera checks caches, logs, browser data, developer tools and more. Nothing is removed until you choose what to clean and confirm.";
/// Browser data belongs to the Privacy screen on Windows.
#[cfg(windows)]
const SCAN_CHECKS: &str = "Nera checks temporary files, caches, logs, developer tools and more. Nothing is removed until you choose what to clean and confirm.";

pub struct CleanupScreen {
    store: Entity<ScanStore>,
    _observe: Subscription,
    _tick: Task<()>,
}

impl CleanupScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = scan_store(cx);
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        // "scanned 5 minutes ago" must keep counting while the screen is open.
        let tick = refresh_every(cx, Duration::from_secs(30), |_, _| {});
        Self {
            store,
            _observe: observe,
            _tick: tick,
        }
    }

    fn render_progress(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let (title, p) = match store.phase() {
            Phase::Idle => return None,
            Phase::Scanning => ("Scanning for junk", store.scan_progress()),
            Phase::Cleaning => ("Cleaning", store.clean_progress()),
        };
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
        let handle = self.store.clone();
        Some(
            ProgressPanel::new("cleanup-progress", title)
                .detail(detail)
                .fraction(p.fraction())
                .on_cancel(move |_, _, cx| handle.update(cx, |s, cx| s.cancel(cx)))
                .into_any_element(),
        )
    }

    fn render_report(&self, cx: &App) -> Option<AnyElement> {
        let report = self.store.read(cx).report()?;
        let (tone, title, summary) = summarize(report);
        let mut lines: Vec<String> = Vec::new();
        if !summary.is_empty() {
            lines.push(summary);
        }
        for outcome in report.issues() {
            if let Some(message) = outcome.message() {
                lines.push(format!("{}: {message}.", outcome.name));
            }
        }
        let store = self.store.read(cx);
        // What the disk itself says, next to what the cleanup counted.
        if let Some((before, after)) = store.measured() {
            lines.push(present::measured_line(before, after));
        }
        let trash = store.trash_bytes();
        let busy = store.is_busy();
        let handle = self.store.clone();
        let empty = self.store.clone();
        let mut actions = h_flex().gap(sp::SM);
        // Items moved to the Trash free nothing until it is emptied.
        if trash >= present::EMPTY_TRASH_MIN {
            actions = actions.child(
                Button::new("empty-trash")
                    .small()
                    .outline()
                    .icon(icon("trash"))
                    .label(format!(
                        "Empty {} · {}",
                        nera_core::os::TRASH,
                        present::bytes(trash)
                    ))
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        ScanStore::request_empty_trash(&empty, window, cx)
                    }),
            );
        }
        actions = actions.child(
            Button::new("dismiss-report")
                .small()
                .ghost()
                .label("Dismiss")
                .on_click(move |_, _, cx| handle.update(cx, |s, cx| s.dismiss_report(cx))),
        );
        let mut banner = Banner::new(tone, title).action(actions);
        if !lines.is_empty() {
            banner = banner.message(lines.join("\n"));
        }
        Some(banner.into_any_element())
    }

    fn render_results(&self, cx: &App) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let busy = store.is_busy();
        let Some(results) = store.results() else {
            return Vec::new();
        };
        let muted = cx.theme().muted_foreground;
        let groups = present::groups(&results.categories);
        // Folders that could not be counted are explained under either state.
        let note = |text: String| {
            Section::untitled()
                .child(div().text_xs().text_color(muted).child(text))
                .into_any_element()
        };
        if groups.is_empty() {
            let mut out = vec![Section::untitled()
                .child(
                    EmptyState::new("circle-check", "Nothing to clean")
                        .message(format!(
                            "Checked {}. Caches, logs and temporary files are already tidy.",
                            categories(results.categories.len())
                        ))
                        .tone(Tone::Success),
                )
                .into_any_element()];
            out.extend(self.render_cloud(cx));
            out.extend(self.render_system_assets(cx));
            out.extend(present::notes(results).into_iter().map(note));
            return out;
        }

        let icons: std::collections::HashMap<&str, &str> = results
            .categories
            .iter()
            .map(|s| (s.category.id, s.category.icon))
            .collect();
        let mut out: Vec<AnyElement> = Vec::new();
        for group in groups {
            let mut list = ListCard::new();
            for text in group.rows {
                let id = text.id;
                let handle = self.store.clone();
                let mut row = Row::new(SharedString::from(format!("cat-{id}")), text.name)
                    .icon(icons.get(id).copied().unwrap_or("folder"))
                    .subtitle(text.description)
                    .value(text.value)
                    .disabled(busy)
                    .checkbox(store.is_selected(id), move |on, _, cx| {
                        let on = *on;
                        handle.update(cx, |s, cx| s.set_selected(id, on, cx));
                    });
                if text.badges.contains(&"Review first") {
                    row = row
                        .icon_tone(Tone::Warning)
                        .badge(Badge::new("Review first").tone(Tone::Warning));
                }
                if text.badges.contains(&present::MOVES_TO_TRASH) {
                    row = row.badge(Badge::new(present::MOVES_TO_TRASH));
                }
                list = list.child(row);
            }
            out.push(
                Section::new(group.label)
                    .trailing(div().text_sm().text_color(muted).child(group.total))
                    .child(list)
                    .into_any_element(),
            );
        }

        out.extend(self.render_cloud(cx));
        out.extend(self.render_system_assets(cx));
        out.extend(present::notes(results).into_iter().map(note));
        out.push(note(
            concat!(
                "Sizes count only the space a cleanup frees. Space shared with other files is not counted, so ",
                nera_core::file_manager!(),
                " can show more."
            )
            .to_string(),
        ));
        out
    }

    /// Cloud-drive files stored on this disk: shown, never cleaned.
    fn render_cloud(&self, cx: &App) -> Option<AnyElement> {
        let drives: Vec<_> = self
            .store
            .read(cx)
            .cloud_drives()
            .iter()
            .filter(|d| d.local_bytes >= present::CLOUD_DRIVE_MIN)
            .collect();
        if drives.is_empty() {
            return None;
        }
        let total: u64 = drives.iter().map(|d| d.local_bytes).sum();
        let muted = cx.theme().muted_foreground;
        let mut list = ListCard::new();
        for drive in drives {
            let path = drive.path.clone();
            let id = format!("cloud-{}", drive.path.display());
            list = list.child(
                Row::new(SharedString::from(id.clone()), drive.service.clone())
                    .icon("cloud")
                    .icon_tone(Tone::Info)
                    .subtitle(present::cloud_subtitle(drive))
                    .value(present::bytes(drive.local_bytes))
                    .action(
                        Button::new(SharedString::from(format!("{id}-show")))
                            .small()
                            .ghost()
                            .icon(icon("folder-search"))
                            .tooltip(concat!("Show in ", nera_core::file_manager!()))
                            .on_click(move |_, _, _| fs_util::reveal_in_finder(&path)),
                    ),
            );
        }
        Some(
            Section::new("Cloud files stored on this Mac")
                .description(present::CLOUD_HINT)
                .trailing(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(present::bytes(total)),
                )
                .child(list)
                .into_any_element(),
        )
    }

    /// Data of system features (Siri, Apple Intelligence): shown with the
    /// setting that makes the OS remove it, never cleaned.
    fn render_system_assets(&self, cx: &App) -> Option<AnyElement> {
        let assets: Vec<_> = self
            .store
            .read(cx)
            .system_assets()
            .iter()
            .filter(|a| a.bytes >= present::SYSTEM_ASSET_MIN)
            .collect();
        if assets.is_empty() {
            return None;
        }
        let total: u64 = assets.iter().map(|a| a.bytes).sum();
        let muted = cx.theme().muted_foreground;
        let mut list = ListCard::new();
        for asset in assets {
            let feature = asset.feature;
            let id = format!("system-asset-{}", feature.name());
            list = list.child(
                Row::new(SharedString::from(id.clone()), feature.name())
                    .icon("mic")
                    .subtitle(feature.detail())
                    .value(present::bytes(asset.bytes))
                    .action(
                        Button::new(SharedString::from(format!("{id}-settings")))
                            .small()
                            .outline()
                            .label("Open settings")
                            .on_click(move |_, _, _| fs_util::open(feature.settings_url())),
                    ),
            );
        }
        Some(
            Section::new("Managed by macOS")
                .description(present::SYSTEM_ASSET_HINT)
                .trailing(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(present::bytes(total)),
                )
                .child(list)
                .into_any_element(),
        )
    }

    fn render_toolbar(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let results = store.results()?;
        let busy = store.is_busy();
        let muted = cx.theme().muted_foreground;
        let summary = present::toolbar_summary(results, engine::unix_now());
        let (a, b) = (self.store.clone(), self.store.clone());
        Some(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap(sp::LG)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(muted)
                        .child(summary),
                )
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap(sp::SM)
                        .child(
                            Button::new("select-safe")
                                .small()
                                .outline()
                                .label("Select safe")
                                .disabled(busy)
                                .on_click(move |_, _, cx| a.update(cx, |s, cx| s.select_safe(cx))),
                        )
                        .child(
                            Button::new("select-none")
                                .small()
                                .ghost()
                                .label("Select none")
                                .disabled(busy)
                                .on_click(move |_, _, cx| b.update(cx, |s, cx| s.select_none(cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, cx: &App) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let results = store.results()?;
        if results.total() == 0 {
            return None;
        }
        let count = store.selected_count();
        let bytes = store.selected_bytes();
        let busy = store.is_busy();
        let muted = cx.theme().muted_foreground;
        let text = present::footer(
            count,
            bytes,
            &store.disposal(state::settings(cx).move_to_trash),
        );
        let handle = self.store.clone();
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
                                .child(text.headline),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(text.hint),
                        ),
                )
                .child(
                    Button::new("clean")
                        .primary()
                        .label(text.button)
                        .disabled(busy || count == 0)
                        .on_click(move |_, window, cx| {
                            ScanStore::request_clean(&handle, window, cx)
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for CleanupScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (busy, scanning, has_results, needs_fda) = {
            let s = self.store.read(cx);
            (
                s.is_busy(),
                s.phase() == Phase::Scanning,
                s.results().is_some(),
                s.results().is_some_and(|r| r.needs_full_disk_access()),
            )
        };
        let scan_store = self.store.clone();
        let scan = move |primary: bool| {
            let handle = scan_store.clone();
            let button = Button::new("scan")
                .label(if has_results { "Scan again" } else { "Scan" })
                .disabled(busy)
                .on_click(move |_, _, cx| handle.update(cx, |s, cx| s.start_scan(cx)));
            if primary {
                button.primary()
            } else {
                button.outline()
            }
        };

        let mut page = Page::new("cleanup-page", Screen::Cleanup.title())
            .subtitle("Caches, logs and temporary files");
        if has_results || busy {
            page = page.action(scan(false));
        }
        if let Some(toolbar) = self.render_toolbar(cx) {
            page = page.toolbar(toolbar);
        }
        if !scanning {
            if let Some(footer) = self.render_footer(cx) {
                page = page.footer(footer);
            }
        }
        if needs_fda {
            page = page.child(
                Banner::new(Tone::Warning, "Some folders could not be read")
                    .message("Allow Full Disk Access so Nera can scan caches, logs and browser data in protected folders. The sizes below may be too low.")
                    .action(
                        Button::new("open-fda")
                            .small()
                            .outline()
                            .label("Open settings")
                            .on_click(|_, _, _| fs_util::open(engine::FULL_DISK_ACCESS_URL)),
                    ),
            );
        }
        page = page
            .children(self.render_report(cx))
            .children(self.render_progress(cx));
        if !scanning {
            if has_results {
                page = page.children(self.render_results(cx));
            } else {
                page = page.child(
                    Section::untitled().child(
                        EmptyState::new(Screen::Cleanup.icon(), "Scan for junk")
                            .message(SCAN_CHECKS)
                            .action(scan(true)),
                    ),
                );
            }
        }
        page
    }
}
