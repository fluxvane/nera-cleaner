//! Privacy screen: browser data (cache, cookies, history, website data, form
//! data) and macOS recent-items lists. All file work lives in `crate::engine`.
//!
//! Windows has no recent-items lists, Safari or Full Disk Access, so those
//! parts are not shown there.

use crate::engine::{self, CleanItem, CleanReport, Group, Item, ScanResult};
use crate::view::{self, RowState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::*;
use nera_core::format;
use nera_core::fs_util::Cancel;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use std::collections::HashSet;
use std::time::Duration;

/// How often running browsers are re-detected.
const RUNNING_POLL: Duration = Duration::from_secs(3);

/// The macOS recent-items lists are part of this screen.
const SHOW_RECENT: bool = cfg!(target_os = "macos");

#[cfg(target_os = "macos")]
const PAGE_SUBTITLE: &str = "Browser data and recent-items history";
#[cfg(not(target_os = "macos"))]
const PAGE_SUBTITLE: &str = "Browser caches, cookies and history";

#[cfg(target_os = "macos")]
const PERMANENT_NOTE: &str = "History, cookies, website data, form data and recent items are deleted permanently and can't be restored. ";
#[cfg(not(target_os = "macos"))]
const PERMANENT_NOTE: &str =
    "History, cookies, website data and form data are deleted permanently and can't be restored. ";

#[cfg(target_os = "macos")]
const RUNNING_NOTE: &str =
    "Quit a browser to clean its data. Open browsers keep their files in use.";
/// Chromium browsers can keep running after their last window closes.
#[cfg(not(target_os = "macos"))]
const RUNNING_NOTE: &str = "Quit a browser to clean its data. Open browsers keep their files in use, and some keep running in the background after the last window closes.";

/// Full Disk Access banner. Windows has no such permission.
#[cfg(target_os = "macos")]
fn full_disk_access_banner(result: &ScanResult) -> Option<AnyElement> {
    if result.full_disk_access == engine::Access::Missing {
        return Some(
            Banner::new(Tone::Warning, "Full Disk Access is off")
                .message(
                    "macOS hides Safari's history, cookies and website data from apps without Full Disk Access. Grant it to Nera Cleaner, then quit and reopen the app.",
                )
                .action(
                    Button::new("open-fda")
                        .small()
                        .outline()
                        .label("Open Privacy settings")
                        .on_click(|_, _, _| {
                            nera_core::fs_util::open(engine::FULL_DISK_ACCESS_PANE)
                        }),
                )
                .into_any_element(),
        );
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn full_disk_access_banner(_result: &ScanResult) -> Option<AnyElement> {
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    Scanning,
    Cleaning,
}

type Notice = (Tone, SharedString, SharedString);

pub struct PrivacyScreen {
    result: Option<ScanResult>,
    selected: HashSet<String>,
    busy: Busy,
    cancelling: bool,
    notice: Option<Notice>,
    progress: Shared<String>,
    cancel: Cancel,
    _task: Option<Task<()>>,
    _watch: Task<()>,
}

impl PrivacyScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let watch = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(RUNNING_POLL).await;
            // Screens stay alive after the first visit: look at the running
            // processes only while this one is shown and has results.
            match this.update(cx, |screen, cx| {
                nav::current(cx) == Screen::Privacy && screen.result.is_some()
            }) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            let exes = cx
                .background_executor()
                .spawn(async { engine::running_executables() })
                .await;
            if this
                .update(cx, |screen, cx| screen.apply_running(&exes, cx))
                .is_err()
            {
                return;
            }
        });
        let mut screen = Self {
            result: None,
            selected: HashSet::new(),
            busy: Busy::Idle,
            cancelling: false,
            notice: None,
            progress: Shared::new(String::new()),
            cancel: Cancel::new(),
            _task: None,
            _watch: watch,
        };
        screen.start_scan(cx);
        screen
    }

    fn is_busy(&self) -> bool {
        self.busy != Busy::Idle
    }

    fn groups(&self) -> &[Group] {
        self.result
            .as_ref()
            .map(|r| r.groups.as_slice())
            .unwrap_or(&[])
    }

    // MARK: Scan

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let progress = self.progress.clone();
        progress.set("Looking for browsers".into());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.busy = Busy::Scanning;
        self.cancelling = false;
        self._task = Some(run_blocking(
            cx,
            move || {
                let exes = engine::running_executables();
                engine::scan(&engine::Env::real(), &exes, &cancel, &|line| {
                    progress.set(line.to_string())
                })
            },
            |this, result, cx| this.finish_scan(result, cx),
        ));
        cx.notify();
    }

    fn finish_scan(&mut self, result: ScanResult, cx: &mut Context<Self>) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;
        self.result = Some(result);
        self.prune_selection();
        cx.notify();
    }

    /// Drop selections that are no longer possible (gone, empty, running).
    fn prune_selection(&mut self) {
        let valid: HashSet<String> = self
            .groups()
            .iter()
            .flat_map(|g| {
                g.items
                    .iter()
                    .filter(|i| self.state_of(g, i).selectable())
                    .map(|i| i.id.clone())
            })
            .collect();
        self.selected.retain(|id| valid.contains(id));
    }

    /// Refresh which browsers are running (called by the background poll).
    fn apply_running(&mut self, exes: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let Some(result) = self.result.as_mut() else {
            return;
        };
        let mut changed = false;
        for group in &mut result.groups {
            let running = group
                .app_name
                .is_some_and(|app| engine::is_running(app, exes));
            if running != group.running {
                group.running = running;
                changed = true;
            }
        }
        if changed {
            self.prune_selection();
            cx.notify();
        }
    }

    fn state_of(&self, group: &Group, item: &Item) -> RowState {
        view::row_state(
            group,
            item,
            self.result.as_ref().map(|r| r.full_disk_access),
        )
    }

    // MARK: Selection

    fn toggle_item(&mut self, id: &str, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let ready = self
            .groups()
            .iter()
            .flat_map(|g| g.items.iter().map(move |i| (g, i)))
            .any(|(g, i)| i.id == id && self.state_of(g, i).selectable());
        if !ready {
            return;
        }
        if checked {
            self.selected.insert(id.to_string());
        } else {
            self.selected.remove(id);
        }
        cx.notify();
    }

    fn ready_ids(&self, group_id: &str) -> Vec<String> {
        self.groups()
            .iter()
            .filter(|g| g.id == group_id)
            .flat_map(|g| {
                g.items
                    .iter()
                    .filter(|i| self.state_of(g, i).selectable())
                    .map(|i| i.id.clone())
            })
            .collect()
    }

    fn group_all_selected(&self, group_id: &str) -> bool {
        let ids = self.ready_ids(group_id);
        !ids.is_empty() && ids.iter().all(|id| self.selected.contains(id))
    }

    fn toggle_group(&mut self, group_id: &str, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        for id in self.ready_ids(group_id) {
            if checked {
                self.selected.insert(id);
            } else {
                self.selected.remove(&id);
            }
        }
        cx.notify();
    }

    fn selected_items(&self) -> Vec<CleanItem> {
        self.groups()
            .iter()
            .flat_map(|g| g.items.iter().map(move |i| (g, i)))
            .filter(|(_, i)| self.selected.contains(&i.id))
            .map(|(g, i)| CleanItem::from_item(g, i))
            .collect()
    }

    fn summary(&self) -> view::Summary {
        view::summary(
            self.groups(),
            &self.selected,
            self.result.as_ref().map(|r| r.full_disk_access),
        )
        .scan_stopped(self.result.as_ref().is_some_and(|r| r.cancelled))
    }

    // MARK: Clean

    fn request_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let items = self.selected_items();
        let permanent = items.iter().filter(|i| i.kind.permanent()).count();
        let caches = items.len() - permanent;
        let size = self.summary().selected_value();
        let to_trash = nera_ui::state::settings(cx).move_to_trash;

        let mut message = format!(
            "{} ({size}) will be cleaned. ",
            format::count(items.len(), "item")
        );
        if permanent > 0 {
            message.push_str(PERMANENT_NOTE);
        }
        if caches > 0 {
            message.push_str(if to_trash {
                concat!("Caches are moved to the ", nera_core::trash_name!(), ". ")
            } else {
                "Caches are deleted permanently. "
            });
        }
        message.push_str("Bookmarks and saved passwords are not touched.");

        let this = cx.entity().downgrade();
        let handle = window.window_handle();
        confirm(
            Confirm::destructive(
                format!("Clean {}?", format::count(items.len(), "item")),
                message,
                "Clean",
            ),
            // Data that can't be restored always asks, whatever the
            // "Confirm before cleanup" preference says.
            permanent == 0,
            window,
            cx,
            move |_window, cx| {
                let _ = this.update(cx, |this, cx| this.start_clean(handle, cx));
            },
        );
    }

    fn start_clean(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let items = self.selected_items();
        let to_trash = nera_ui::state::settings(cx).move_to_trash;
        let progress = self.progress.clone();
        progress.set("Preparing".into());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.busy = Busy::Cleaning;
        self.cancelling = false;
        self.notice = None;
        self._task = Some(run_blocking(
            cx,
            move || {
                // Detect again right before deleting: a browser may have been opened since the scan.
                let exes = engine::running_executables();
                engine::clean(&items, &exes, to_trash, &cancel, &|line| {
                    progress.set(line.to_string())
                })
            },
            move |this, report, cx| this.finish_clean(report, window, cx),
        ));
        cx.notify();
    }

    fn finish_clean(
        &mut self,
        report: CleanReport,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;
        for (path, reason) in &report.failed {
            log::warn!("privacy data not removed: {}: {reason}", path.display());
        }
        self.selected.clear();

        let outcome = view::clean_outcome(&report);
        let tone = match outcome.severity {
            view::Severity::Info => Tone::Info,
            view::Severity::Warning => Tone::Warning,
            view::Severity::Danger => Tone::Danger,
            view::Severity::Success => Tone::Success,
        };
        let (title, message) = (outcome.title, outcome.message);
        self.notice = Some((tone, title.clone().into(), message.clone().into()));
        if report.removed > 0 {
            system_notify(cx, "Privacy data cleaned", &format!("{title}. {message}"));
        }
        let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
        // Show the new sizes.
        self.start_scan(cx);
        cx.notify();
    }

    fn cancel_work(&mut self, cx: &mut Context<Self>) {
        self.cancelling = true;
        self.cancel.cancel();
        cx.notify();
    }

    // MARK: Render pieces

    fn render_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some((tone, title, message)) = &self.notice {
            out.push(
                Banner::new(*tone, title.clone())
                    .message(message.clone())
                    .action(
                        Button::new("dismiss-notice")
                            .small()
                            .ghost()
                            .label("Dismiss")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        let Some(result) = &self.result else {
            return out;
        };
        if result.cancelled {
            out.push(
                Banner::new(Tone::Info, "Scan stopped early")
                    .message("These results are partial. Rescan to measure every browser.")
                    .into_any_element(),
            );
        }
        out.extend(full_disk_access_banner(result));
        let running = view::running_names(result);
        if !running.is_empty() {
            out.push(
                Banner::new(Tone::Info, view::running_title(&running))
                    .message(RUNNING_NOTE)
                    .into_any_element(),
            );
        }
        out
    }

    fn render_progress(&self, cx: &mut Context<Self>) -> AnyElement {
        let (id, title) = match self.busy {
            Busy::Cleaning => ("clean-progress", "Cleaning selected data"),
            _ => ("scan-progress", "Looking for private data"),
        };
        let detail = if self.cancelling {
            "Stopping…".to_string()
        } else {
            self.progress.get()
        };
        let mut panel = ProgressPanel::new(id, title).detail(detail);
        if !self.cancelling {
            panel = panel.on_cancel(cx.listener(|this, _, _, cx| this.cancel_work(cx)));
        }
        panel.into_any_element()
    }

    fn render_summary(&self) -> AnyElement {
        let sum = self.summary();
        StatGrid::new(if SHOW_RECENT { 3 } else { 2 })
            .child(
                StatTile::new("Browser data", sum.browser_value())
                    .icon("globe")
                    .caption(sum.browser_caption()),
            )
            .children(SHOW_RECENT.then(|| {
                StatTile::new("Recent items", sum.recent_value())
                    .icon("clock")
                    .caption("Apple menu and Open Recent")
            }))
            .child(
                StatTile::new("Selected", sum.selected_value())
                    .icon("shield-check")
                    .tone(if sum.selected_bytes > 0 {
                        Tone::Accent
                    } else {
                        Tone::Neutral
                    })
                    .caption(sum.selected_caption()),
            )
            .into_any_element()
    }

    fn render_group(&self, group: &Group, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let access = self.result.as_ref().map(|r| r.full_disk_access);
        let to_trash = nera_ui::state::settings(cx).move_to_trash;
        let gid = group.id.clone();
        let header_id = format!("group-{}", group.id);
        let label = if group.running {
            "Quit the browser to select data"
        } else {
            "Data to clean"
        };

        let mut list = ListCard::new().header(list_header(
            &header_id,
            label,
            Some("Size"),
            Some((
                self.group_all_selected(&group.id),
                Box::new(cx.listener(move |this, checked: &bool, _, cx| {
                    this.toggle_group(&gid, *checked, cx)
                })),
            )),
        ));

        for item in &group.items {
            let state = self.state_of(group, item);
            let id = item.id.clone();
            let kind = item.kind;
            let value = view::row_value(item, access);
            let subtitle = view::row_subtitle(kind, to_trash);
            let mut row = Row::new(SharedString::from(item.id.clone()), kind.label())
                .subtitle(subtitle)
                .icon(kind.icon())
                .value(value)
                .disabled(busy || !state.selectable())
                .checkbox(
                    self.selected.contains(&item.id),
                    cx.listener(move |this, checked: &bool, _, cx| {
                        this.toggle_item(&id, *checked, cx)
                    }),
                );
            if kind.permanent() && state.selectable() {
                row = row.badge(Badge::new("Permanent").tone(Tone::Warning));
            }
            list = list.child(row);
        }

        let mut section =
            Section::new(group.name.clone()).description(view::group_description(group, access));
        if group.running {
            section = section.trailing(Badge::new("Running — quit to clean").tone(Tone::Warning));
        }
        section.child(list).into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.result.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let sum = self.summary();
        let n = sum.selected_items;
        let summary = view::footer_summary(&sum);
        let permanent = self
            .groups()
            .iter()
            .flat_map(|g| g.items.iter())
            .any(|i| self.selected.contains(&i.id) && i.kind.permanent());
        let hint = if permanent {
            "History, cookies and other data are deleted permanently."
        } else if nera_ui::state::settings(cx).move_to_trash {
            concat!("Caches are moved to the ", nera_core::trash_name!(), ".")
        } else {
            "Caches are deleted permanently. Change this in Settings."
        };
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
                        .child(div().text_sm().font_medium().child(summary))
                        .child(div().text_xs().text_color(muted).child(hint)),
                )
                .child(
                    Button::new("clean")
                        .primary()
                        .icon(icon("trash"))
                        .label(view::clean_button_label(&sum))
                        .loading(self.busy == Busy::Cleaning)
                        .disabled(self.is_busy() || n == 0)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.request_clean(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Drop for PrivacyScreen {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Render for PrivacyScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = Page::new("privacy-page", Screen::Privacy.title())
            .subtitle(PAGE_SUBTITLE)
            .action(
                Button::new("rescan")
                    .outline()
                    .icon(icon("refresh-cw"))
                    .label("Rescan")
                    .loading(self.busy == Busy::Scanning)
                    .disabled(self.is_busy())
                    .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
            );
        if let Some(footer) = self.render_footer(cx) {
            page = page.footer(footer);
        }

        let banners = self.render_banners(cx);
        if !banners.is_empty() {
            page = page.child(v_flex().w_full().gap(sp::MD).children(banners));
        }

        if self.is_busy() {
            page = page.child(self.render_progress(cx));
        }

        if self.result.is_some() {
            page = page.child(self.render_summary());
            for group in self.groups() {
                page = page.child(self.render_group(group, cx));
            }
        } else if !self.is_busy() {
            page = page.child(
                Section::untitled().child(
                    EmptyState::new(Screen::Privacy.icon(), "Nothing scanned yet")
                        .message("Scan to see the private data your browsers keep.")
                        .action(
                            Button::new("scan-empty")
                                .outline()
                                .label("Scan")
                                .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
                        ),
                ),
            );
        }
        page
    }
}
