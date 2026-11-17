//! Space Lens: drill through folders sorted by size, see what each takes of
//! its parent, reveal or move items to the Trash, and find large files.

use crate::engine::{self, Entry, LargeFile, LargeProgress, Listing, ScanError, ScanProgress};
use crate::view::{self, Capacity};
use gpui_kit::component::breadcrumb::{Breadcrumb, BreadcrumbItem};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_core::format;
use nera_core::fs_util::{self, Cancel};
use nera_core::os::TRASH;
use nera_ui::nav::Screen;
use nera_ui::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rows rendered per folder; the rest are summarised under the list.
const MAX_ROWS: usize = 200;
/// Fixed lanes so the per-row meters and percentages line up across rows.
const METER_W: Pixels = px(120.);
const PERCENT_W: Pixels = px(40.);

enum LargeState {
    Idle,
    Running,
    Done(Vec<LargeFile>),
    Failed(ScanError),
}

pub struct SpaceScreen {
    /// Folder the user chose to analyze (home by default).
    root: PathBuf,
    /// Folder currently shown (inside `root`).
    current: PathBuf,
    /// Measured folders, so going back is instant.
    cache: HashMap<PathBuf, Listing>,

    scanning: Option<PathBuf>,
    scan_error: Option<(PathBuf, ScanError)>,
    scan_cancel: Cancel,
    scan_progress: Shared<ScanProgress>,
    _scan_task: Option<Task<()>>,

    full_disk_access: Option<bool>,
    disk: Option<Result<Capacity, String>>,
    _disk_task: Option<Task<()>>,

    large: LargeState,
    large_root: PathBuf,
    large_cancel: Cancel,
    large_progress: Shared<LargeProgress>,
    _large_task: Option<Task<()>>,

    trashing: Option<PathBuf>,
    _trash_task: Option<Task<()>>,
}

impl SpaceScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = nera_core::paths::home();
        let mut this = Self {
            root: home.clone(),
            current: home.clone(),
            cache: HashMap::new(),
            scanning: None,
            scan_error: None,
            scan_cancel: Cancel::new(),
            scan_progress: Shared::default(),
            _scan_task: None,
            full_disk_access: None,
            disk: None,
            _disk_task: None,
            large: LargeState::Idle,
            large_root: home.clone(),
            large_cancel: Cancel::new(),
            large_progress: Shared::default(),
            _large_task: None,
            trashing: None,
            _trash_task: None,
        };
        this.refresh_disk(cx);
        this.start_scan(home, cx);
        this
    }

    // MARK: - Actions

    fn refresh_disk(&mut self, cx: &mut Context<Self>) {
        self._disk_task = Some(run_blocking(
            cx,
            engine::startup_capacity,
            |this, result, _| {
                this.disk = Some(result);
                this._disk_task = None;
            },
        ));
    }

    /// Show `path`: from the cache when measured, else measure it now.
    fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.cache.contains_key(&path) {
            self.stop_scan();
            self.current = path;
        } else {
            self.current = path.clone();
            self.start_scan(path, cx);
        }
        cx.notify();
    }

    fn start_scan(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.stop_scan();
        let cancel = Cancel::new();
        self.scan_cancel = cancel.clone();
        self.scan_progress.set(ScanProgress::default());
        self.scanning = Some(path.clone());
        self.scan_error = None;

        let progress = self.scan_progress.clone();
        let probe_access = self.full_disk_access.is_none();
        let target = path.clone();
        self._scan_task = Some(run_blocking(
            cx,
            move || {
                let listing = engine::list_children(&target, &cancel, &|p| progress.set(p));
                let access = if probe_access {
                    engine::has_full_disk_access()
                } else {
                    None
                };
                (listing, access)
            },
            move |this, (listing, access), cx| {
                this.scanning = None;
                this._scan_task = None;
                if access.is_some() {
                    this.full_disk_access = access;
                }
                match listing {
                    Ok(listing) => {
                        engine::merge_remeasured(&mut this.cache, listing);
                    }
                    Err(err) => this.scan_error = Some((path, err)),
                }
                cx.notify();
            },
        ));
        cx.notify();
    }

    /// Stop a running measurement without recording anything.
    fn stop_scan(&mut self) {
        if self.scanning.take().is_some() {
            self.scan_cancel.cancel();
            self._scan_task = None;
        }
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.scanning.clone() {
            self.stop_scan();
            self.scan_error = Some((path, ScanError::Cancelled));
        }
        cx.notify();
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        engine::invalidate(&mut self.cache, &self.current);
        let path = self.current.clone();
        self.start_scan(path, cx);
        self.refresh_disk(cx);
        self.drop_missing_large_files(cx);
    }

    /// Rescan also clears large files that were removed outside the app.
    fn drop_missing_large_files(&mut self, cx: &mut Context<Self>) {
        let LargeState::Done(items) = &self.large else {
            return;
        };
        let paths: Vec<PathBuf> = items.iter().map(|f| f.path.clone()).collect();
        cx.spawn(async move |this, cx| {
            let gone: Vec<PathBuf> = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .filter(|p| std::fs::symlink_metadata(p).is_err())
                        .collect()
                })
                .await;
            if gone.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if let LargeState::Done(items) = &mut this.large {
                    for path in &gone {
                        engine::prune_large(items, path);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = engine::parent_within(&self.root, &self.current) {
            self.open_folder(parent, cx);
        }
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Analyze".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.set_root(path, cx));
        })
        .detach();
    }

    fn set_root(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.cancel_large_scan();
        self.large = LargeState::Idle;
        self.large_root = path.clone();
        self.root = path.clone();
        self.scan_error = None;
        self.open_folder(path, cx);
    }

    fn find_large_files(&mut self, cx: &mut Context<Self>) {
        if matches!(self.large, LargeState::Running) {
            return;
        }
        let cancel = Cancel::new();
        self.large_cancel = cancel.clone();
        self.large_progress.set(LargeProgress::default());
        self.large = LargeState::Running;
        self.large_root = self.root.clone();

        let progress = self.large_progress.clone();
        let root = self.root.clone();
        self._large_task = Some(run_blocking(
            cx,
            move || {
                engine::find_large_files(
                    &root,
                    engine::LARGE_FILE_THRESHOLD,
                    engine::LARGE_FILE_LIMIT,
                    &cancel,
                    &|p| progress.set(p),
                )
            },
            |this, result, cx| {
                this._large_task = None;
                this.large = match result {
                    Ok(items) => LargeState::Done(items),
                    Err(ScanError::Cancelled) => LargeState::Idle,
                    Err(err) => LargeState::Failed(err),
                };
                cx.notify();
            },
        ));
        cx.notify();
    }

    fn cancel_large_scan(&mut self) {
        if matches!(self.large, LargeState::Running) {
            self.large_cancel.cancel();
            self._large_task = None;
            self.large = LargeState::Idle;
        }
    }

    /// Confirm, then move `path` to the Trash. Protected items are refused
    /// with the reason.
    fn request_trash(
        &mut self,
        path: PathBuf,
        name: String,
        size: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.trashing.is_some() {
            return;
        }
        if let Some(reason) = engine::protection_reason(&path) {
            notify(
                window,
                cx,
                Tone::Warning,
                format!("Can't move “{name}” to the {TRASH}"),
                reason,
            );
            return;
        }
        let entity = cx.entity();
        let spec = Confirm::destructive(
            format!("Move “{name}” to the {TRASH}?"),
            format!(
                "{} ({}) will be moved to the {TRASH}. You can put it back until you empty the {TRASH}.",
                engine::display_path(&path),
                view::bytes(size)
            ),
            concat!("Move to ", nera_core::trash_name!()),
        );
        confirm(spec, false, window, cx, move |_, cx| {
            let (path, name) = (path.clone(), name.clone());
            entity.update(cx, |this, cx| this.trash(path, name, size, cx));
        });
    }

    fn trash(&mut self, path: PathBuf, name: String, size: u64, cx: &mut Context<Self>) {
        if self.trashing.is_some() {
            return;
        }
        self.trashing = Some(path.clone());
        let target = path.clone();
        self._trash_task = Some(run_blocking(
            cx,
            move || fs_util::trash_item(&target).map_err(|e| e.to_string()),
            move |this, result, cx| {
                this.trashing = None;
                this._trash_task = None;
                let toast = match result {
                    Ok(()) => {
                        this.after_removal(&path, size, cx);
                        (
                            Tone::Success,
                            format!("Moved to {TRASH}"),
                            format!("{name} · {}", view::bytes(size)),
                        )
                    }
                    Err(err) => (
                        Tone::Danger,
                        format!("Couldn't move “{name}” to the {TRASH}"),
                        err,
                    ),
                };
                let id = cx.entity_id();
                cx.with_window(id, |window, cx| {
                    notify(window, cx, toast.0, toast.1, toast.2)
                });
                cx.notify();
            },
        ));
        cx.notify();
    }

    fn after_removal(&mut self, path: &Path, size: u64, cx: &mut Context<Self>) {
        engine::apply_removal(&mut self.cache, path, size);
        if let LargeState::Done(items) = &mut self.large {
            engine::prune_large(items, path);
        }
        if self.current.starts_with(path) {
            let parent = path
                .parent()
                .map(Path::to_path_buf)
                .filter(|p| p.starts_with(&self.root));
            let target = parent.unwrap_or_else(|| self.root.clone());
            self.open_folder(target, cx);
        } else if let Some(scanning) = self.scanning.clone() {
            // A measurement that ran across the removal may or may not have
            // counted the item: start it again rather than guess.
            self.start_scan(scanning, cx);
        }
        self.refresh_disk(cx);
    }

    // MARK: - Rendering

    fn listing(&self) -> Option<&Listing> {
        self.cache.get(&self.current)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let parent = engine::parent_within(&self.root, &self.current);
        let trail = engine::breadcrumb(&self.root, &self.current);
        let last = trail.len().saturating_sub(1);
        let items: Vec<BreadcrumbItem> = trail
            .into_iter()
            .enumerate()
            .map(|(i, (label, path))| {
                let item = BreadcrumbItem::new(label);
                if i == last {
                    item
                } else {
                    item.on_click(
                        cx.listener(move |this, _, _, cx| this.open_folder(path.clone(), cx)),
                    )
                }
            })
            .collect();

        let summary = match (self.listing(), &self.scanning) {
            (_, Some(_)) => "Measuring…".to_string(),
            (Some(l), None) => view::folder_summary(l),
            (None, None) => String::new(),
        };

        h_flex()
            .w_full()
            .items_center()
            .gap(sp::MD)
            .child(
                Button::new("space-up")
                    .ghost()
                    .small()
                    .icon(icon("arrow-left"))
                    .tooltip("Back to enclosing folder")
                    .disabled(parent.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(Breadcrumb::new().children(items)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(muted)
                    .child(summary),
            )
    }

    fn render_overview(&self) -> impl IntoElement {
        let folder = engine::folder_name(&self.current);
        let folder_size = self.listing().map(|l| l.total);

        let o = view::overview(
            self.disk.as_ref(),
            &folder,
            &engine::display_path(&self.current),
            folder_size,
            self.listing().is_none_or(|l| l.on_startup_disk),
        );
        let disk_error = self.disk.as_ref().and_then(|r| r.as_ref().err().cloned());
        let mut used = StatTile::new("Used", o.used_value)
            .icon("hard-drive")
            .caption(o.used_caption);
        if let Some(f) = o.used_fraction {
            used = used.tone(Tone::for_usage(f)).meter(f);
        }
        let available = StatTile::new("Available", o.available_value)
            .icon("circle-dashed")
            .caption(o.available_caption);
        let folder_tile = StatTile::new(o.folder_title, o.folder_value)
            .icon("folder")
            .caption(o.folder_caption);

        let unreadable = self.listing().map(|l| l.unreadable).unwrap_or(0);
        // Full Disk Access is macOS only; Windows reports denied folders as unreadable.
        #[cfg(not(target_os = "macos"))]
        let permission_banner: Option<Banner> = None;
        #[cfg(target_os = "macos")]
        let denied = matches!(&self.scan_error, Some((p, ScanError::PermissionDenied)) if *p == self.current);
        #[cfg(target_os = "macos")]
        let permission_banner = if self.full_disk_access == Some(false)
            && (unreadable > 0 || denied)
        {
            Some(
                Banner::new(Tone::Warning, "Some folders couldn't be measured")
                    .message(
                        "Give Nera Cleaner Full Disk Access so sizes include protected folders like Mail, Messages and Safari.",
                    )
                    .action(open_settings_button("space-fda")),
            )
        } else {
            None
        };
        let unreadable_banner = (permission_banner.is_none() && unreadable > 0).then(|| {
            Banner::new(
                Tone::Info,
                format!(
                    "{} couldn't be read",
                    format::count(unreadable as usize, "folder")
                ),
            )
            .message(concat!(
                "Sizes shown are a minimum. ",
                nera_core::os_name!(),
                " doesn't let Nera Cleaner open these folders."
            ))
        });

        Section::untitled()
            .child(
                StatGrid::new(3)
                    .child(used)
                    .child(available)
                    .child(folder_tile),
            )
            .when_some(disk_error, |s, err| {
                s.child(Banner::new(Tone::Danger, "Couldn't read disk capacity").message(err))
            })
            .when_some(permission_banner, |s, b| s.child(b))
            .when_some(unreadable_banner, |s, b| s.child(b))
    }

    fn render_contents(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = engine::folder_name(&self.current);
        let section = Section::new(name.clone()).description(format!(
            "{} · sizes are space used on disk",
            engine::display_path(&self.current)
        ));

        if self.scanning.as_ref() == Some(&self.current) {
            let p = self.scan_progress.get();
            let detail = view::scan_detail(&p);
            return section.child(
                ProgressPanel::new("space-scan-cancel", format!("Measuring {name}"))
                    .detail(detail)
                    .fraction(p.fraction())
                    .on_cancel(cx.listener(|this, _, _, cx| this.cancel_scan(cx))),
            );
        }

        if let Some(listing) = self.listing() {
            if listing.entries.is_empty() {
                return section.child(
                    EmptyState::new("folder-open", "This folder is empty")
                        .message("Nothing here takes up space."),
                );
            }
            let shown = listing.entries.len().min(MAX_ROWS);
            let rows: Vec<AnyElement> = listing.entries[..shown]
                .iter()
                .map(|e| self.child_row(listing, e, cx).into_any_element())
                .collect();
            let muted = cx.theme().muted_foreground;
            return section
                .child(ListCard::new().children(rows))
                .when_some(view::more_note(listing, shown), |s, note| {
                    s.child(div().text_xs().text_color(muted).child(note))
                });
        }

        match &self.scan_error {
            Some((p, err)) if *p == self.current => match err {
                ScanError::Cancelled => section.child(
                    EmptyState::new("circle-pause", "Measuring stopped")
                        .message("Choose Rescan to measure this folder."),
                ),
                #[cfg(target_os = "macos")]
                ScanError::PermissionDenied => section.child(
                    Banner::new(Tone::Warning, "Can't read this folder")
                        .message(format!(
                            "{} Allow Full Disk Access in System Settings, then rescan.",
                            err
                        ))
                        .action(open_settings_button("space-denied-fda")),
                ),
                #[cfg(not(target_os = "macos"))]
                ScanError::PermissionDenied => section.child(
                    Banner::new(Tone::Warning, "Can't read this folder").message(err.to_string()),
                ),
                other => section.child(
                    Banner::new(Tone::Danger, "Couldn't measure this folder")
                        .message(other.to_string()),
                ),
            },
            _ => section.child(
                EmptyState::new("chart-pie", "Not measured yet")
                    .message("Choose Rescan to measure this folder."),
            ),
        }
    }

    fn child_row(&self, listing: &Listing, e: &Entry, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let fraction = listing.fraction(e.size);
        let subtitle = view::child_subtitle(e);
        let drillable = e.kind.is_drillable();
        let path = e.path.clone();

        Row::new(format!("child:{}", e.path.display()), e.name.clone())
            .icon(engine::icon_for(e.kind, &e.name))
            .icon_tone(if drillable {
                Tone::Accent
            } else {
                Tone::Neutral
            })
            .subtitle(subtitle)
            .badge(
                h_flex()
                    .items_center()
                    .gap(sp::SM)
                    .child(div().w(METER_W).child(Meter::new(fraction)))
                    .child(
                        div()
                            .w(PERCENT_W)
                            .text_right()
                            .text_xs()
                            .text_color(muted)
                            .child(view::child_percent(listing, e)),
                    ),
            )
            .value(view::bytes(e.size))
            .disabled(self.trashing.as_ref() == Some(&e.path))
            .action(self.reveal_button("child", &e.path))
            .action(self.trash_button("child", e.path.clone(), e.name.clone(), e.size, cx))
            .action(if drillable {
                icon("chevron-right")
                    .size(dim::ICON_SM)
                    .text_color(muted)
                    .into_any_element()
            } else {
                div().size(dim::ICON_SM).into_any_element()
            })
            .when(drillable, |row| {
                row.on_click(cx.listener(move |this, _, _, cx| this.open_folder(path.clone(), cx)))
            })
    }

    fn reveal_button(&self, scope: &str, path: &Path) -> Button {
        let path = path.to_path_buf();
        Button::new(format!("{scope}-reveal:{}", path.display()))
            .ghost()
            .small()
            .icon(icon("folder-search"))
            .tooltip(concat!("Reveal in ", nera_core::file_manager!()))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                fs_util::reveal_in_finder(&path);
            })
    }

    fn trash_button(
        &self,
        scope: &str,
        path: PathBuf,
        name: String,
        size: u64,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(format!("{scope}-trash:{}", path.display()))
            .ghost()
            .small()
            .icon(icon("trash"))
            .tooltip(concat!("Move to ", nera_core::trash_name!()))
            .disabled(self.trashing.is_some())
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.request_trash(path.clone(), name.clone(), size, window, cx);
            }))
    }

    fn render_large(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let section = Section::new("Large files").description(view::large_description(
            &engine::display_path(&self.large_root),
        ));
        let scan_button = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("space-large-scan")
                .outline()
                .small()
                .icon(icon("file-search"))
                .label(label)
                .on_click(cx.listener(|this, _, _, cx| this.find_large_files(cx)))
        };

        match &self.large {
            LargeState::Idle => section.trailing(scan_button("Find large files", cx)).child(
                EmptyState::new("file-search", format!("Find files using {} or more", view::bytes(engine::LARGE_FILE_THRESHOLD))).message(format!(
                    "Looks through every folder in {} for videos, disk images, archives and libraries you may not need.",
                    engine::display_path(&self.root)
                )),
            ),
            LargeState::Running => {
                let p = self.large_progress.get();
                let fraction = self
                    .cache
                    .get(&self.large_root)
                    .filter(|l| l.total > 0)
                    .map(|l| p.bytes as f64 / l.total as f64);
                section.child(
                    ProgressPanel::new("space-large-cancel", "Looking for large files")
                        .detail(format!(
                            "{} files checked · {} found",
                            engine::group_digits(p.files),
                            p.found
                        ))
                        .fraction(fraction)
                        .on_cancel(cx.listener(|this, _, _, cx| {
                            this.cancel_large_scan();
                            cx.notify();
                        })),
                )
            }
            LargeState::Failed(err) => {
                let banner = Banner::new(Tone::Danger, "Couldn't look for large files").message(err.to_string());
                #[cfg(target_os = "macos")]
                let banner = if *err == ScanError::PermissionDenied {
                    banner.action(open_settings_button("space-large-fda"))
                } else {
                    banner
                };
                section.trailing(scan_button("Scan again", cx)).child(banner)
            }
            LargeState::Done(items) if items.is_empty() => section.trailing(scan_button("Scan again", cx)).child(
                EmptyState::new("circle-check", "No large files")
                    .tone(Tone::Success)
                    .message(format!(
                        "Nothing in {} uses {} or more on disk.",
                        engine::display_path(&self.large_root),
                        view::bytes(engine::LARGE_FILE_THRESHOLD)
                    )),
            ),
            LargeState::Done(items) => {
                let rows: Vec<AnyElement> = items.iter().map(|f| self.large_row(f, cx).into_any_element()).collect();
                section.trailing(scan_button("Scan again", cx)).child(ListCard::new().children(rows))
            }
        }
    }

    fn large_row(&self, f: &LargeFile, cx: &mut Context<Self>) -> impl IntoElement {
        Row::new(format!("large:{}", f.path.display()), f.name.clone())
            .icon(engine::icon_for(f.kind, &f.name))
            .subtitle(view::large_subtitle(f))
            .value(view::bytes(f.size))
            .disabled(self.trashing.as_ref() == Some(&f.path))
            .action(self.reveal_button("large", &f.path))
            .action(self.trash_button("large", f.path.clone(), f.name.clone(), f.size, cx))
    }
}

#[cfg(windows)]
impl SpaceScreen {
    /// Switch between the two roots Windows users browse: the user folder
    /// and the system drive (named like Explorer, e.g. "Local Disk (C:)").
    fn root_switch(&self, cx: &mut Context<Self>) -> Button {
        let drive = nera_core::paths::system_drive();
        let at_drive =
            nera_core::paths::standardize(&self.root) == nera_core::paths::standardize(&drive);
        let (target, icon_name, label) = if at_drive {
            (
                nera_core::paths::home(),
                "user",
                "Analyze user folder".to_string(),
            )
        } else {
            let label = format!("Analyze {}", engine::folder_name(&drive));
            (drive, "hard-drive", label)
        };
        Button::new("space-root")
            .outline()
            .icon(icon(icon_name))
            .label(label)
            .on_click(cx.listener(move |this, _, _, cx| this.set_root(target.clone(), cx)))
    }
}

#[cfg(target_os = "macos")]
fn open_settings_button(id: &'static str) -> Button {
    Button::new(id)
        .outline()
        .small()
        .label("Open System Settings")
        .on_click(|_, _, _| fs_util::open(engine::FULL_DISK_ACCESS_URL))
}

impl Render for SpaceScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.scanning.is_some();
        // One scroll position per folder: a folder opened from the cache
        // starts at its top instead of where the previous one was left.
        let page_id = SharedString::from(format!("space-page:{}", self.current.display()));
        let page = Page::new(page_id, Screen::Space.title())
            .subtitle("See what takes up space on your disk");
        #[cfg(windows)]
        let page = page.action(self.root_switch(cx));
        page.action(
            Button::new("space-choose")
                .outline()
                .icon(icon("folder-open"))
                .label("Choose folder…")
                .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
        )
        .action(
            Button::new("space-rescan")
                .primary()
                .icon(icon("refresh-cw"))
                .label("Rescan")
                .disabled(busy)
                .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
        )
        .toolbar(self.render_toolbar(cx))
        .child(self.render_overview())
        .child(self.render_contents(cx))
        .child(self.render_large(cx))
    }
}
