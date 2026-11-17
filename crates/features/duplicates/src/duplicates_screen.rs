//! Duplicates: find identical files in chosen folders and move extra copies
//! to the Trash, always keeping at least one copy of each file.

use crate::engine::{
    self, DuplicateGroup, KeepRule, Phase, Prefs, ScanOptions, ScanProgress, ScanReport,
};
use crate::view::{self, Kind};
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_core::format;
use nera_core::fs_util::{self, Cancel};
use nera_ui::nav::Screen;
use nera_ui::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Groups rendered at first and per "Show more" click.
const GROUP_PAGE: usize = 50;
#[cfg(target_os = "macos")]
const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Idle,
    Scanning,
    Trashing,
}

pub struct DuplicatesScreen {
    prefs: Prefs,
    /// Latest finished scan; groups shrink as copies are trashed.
    report: Option<ScanReport>,
    /// Minimum size the shown results were scanned with.
    scanned_min_size: u64,
    /// Paths marked for the Trash (domain ids).
    selected: HashSet<PathBuf>,
    shown_groups: usize,
    busy: Busy,
    cancelling: bool,
    scan_progress: Shared<ScanProgress>,
    trash_progress: Shared<(usize, usize)>,
    cancel: Cancel,
    _task: Option<Task<()>>,
    _picker: Option<Task<()>>,
}

impl DuplicatesScreen {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            prefs: Prefs::load_from(&Prefs::file_path()),
            report: None,
            scanned_min_size: engine::DEFAULT_MIN_SIZE,
            selected: HashSet::new(),
            shown_groups: GROUP_PAGE,
            busy: Busy::Idle,
            cancelling: false,
            scan_progress: Shared::new(ScanProgress::default()),
            trash_progress: Shared::new((0, 0)),
            cancel: Cancel::new(),
            _task: None,
            _picker: None,
        }
    }

    fn is_busy(&self) -> bool {
        self.busy != Busy::Idle
    }

    fn groups(&self) -> &[DuplicateGroup] {
        self.report
            .as_ref()
            .map(|r| r.groups.as_slice())
            .unwrap_or(&[])
    }

    fn save_prefs(&self, cx: &mut Context<Self>) {
        let prefs = self.prefs.clone();
        cx.background_spawn(async move {
            if let Err(err) = prefs.save_to(&Prefs::file_path()) {
                log::warn!("could not save duplicate finder preferences: {err}");
            }
        })
        .detach();
    }

    // MARK: Folders and options

    fn pick_folders(&mut self, cx: &mut Context<Self>) {
        // The folder panel is not modal: one at a time, or the first panel's
        // choice would be dropped.
        if self.is_busy() || self._picker.is_some() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add".into()),
        });
        self._picker = Some(cx.spawn(async move |this, cx| {
            let picked = rx.await;
            let _ = this.update(cx, |this, cx| {
                this._picker = None;
                // A scan started while the panel was open keeps its folders.
                if let (Ok(Ok(Some(paths))), false) = (picked, this.is_busy()) {
                    this.add_folders(paths, cx);
                }
            });
        }));
    }

    fn add_folders(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for p in paths {
            let p = nera_core::paths::standardize(p);
            if !self.prefs.folders.contains(&p) {
                self.prefs.folders.push(p);
            }
        }
        self.save_prefs(cx);
        cx.notify();
    }

    fn remove_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.prefs.folders.retain(|p| p != path);
        self.save_prefs(cx);
        cx.notify();
    }

    fn set_min_size(&mut self, size: u64, cx: &mut Context<Self>) {
        self.prefs.min_size = size;
        self.save_prefs(cx);
        cx.notify();
    }

    fn set_rule(&mut self, rule: KeepRule, cx: &mut Context<Self>) {
        self.prefs.keep_rule = rule;
        self.selected = engine::auto_select(self.groups(), rule);
        self.save_prefs(cx);
        cx.notify();
    }

    // MARK: Scan

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() || self.prefs.folders.is_empty() {
            return;
        }
        let settings = nera_ui::state::settings(cx);
        let opts = ScanOptions {
            roots: self.prefs.folders.clone(),
            min_size: self.prefs.min_size,
            include_hidden: settings.scan_hidden_files,
            excluded: settings.excluded_paths.clone(),
        };
        self.scanned_min_size = opts.min_size;
        self.report = None;
        self.selected.clear();
        self.shown_groups = GROUP_PAGE;
        self.busy = Busy::Scanning;
        self.cancelling = false;

        let progress = self.scan_progress.clone();
        progress.set(ScanProgress::default());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self._task = Some(run_blocking(
            cx,
            move || engine::scan(&opts, &cancel, &|p| progress.set(p)),
            |this, report, _cx| this.finish_scan(report),
        ));
        cx.notify();
    }

    fn finish_scan(&mut self, report: ScanReport) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;
        if report.cancelled {
            return;
        }
        self.selected = engine::auto_select(&report.groups, self.prefs.keep_rule);
        self.report = Some(report);
    }

    fn cancel_work(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        self.cancelling = true;
        cx.notify();
    }

    fn start_over(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.report = None;
        self.selected.clear();
        cx.notify();
    }

    // MARK: Selection

    fn toggle_file(
        &mut self,
        path: &Path,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        if !checked {
            self.selected.remove(path);
        } else if let Some(group) = self.groups().iter().find(|g| g.contains(path)) {
            if !engine::can_select(group, &self.selected, path) {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                notify(
                    window,
                    cx,
                    Tone::Warning,
                    "Keep at least one copy",
                    format!("Deselect another copy of {name} first."),
                );
                return;
            }
            self.selected.insert(path.to_path_buf());
        }
        cx.notify();
    }

    fn toggle_group(&mut self, group_id: &str, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(group) = self.groups().iter().find(|g| g.id == group_id) else {
            return;
        };
        let paths: Vec<PathBuf> = group.files.iter().map(|f| f.path.clone()).collect();
        let picks = engine::auto_select_group(group, self.prefs.keep_rule);
        for p in &paths {
            self.selected.remove(p);
        }
        if checked {
            self.selected.extend(picks);
        }
        cx.notify();
    }

    fn deselect_all(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        cx.notify();
    }

    // MARK: Trash

    /// Folders excluded in Settings since the scan are never removed, even
    /// if they are still ticked in old results.
    fn drop_excluded(&mut self, cx: &mut Context<Self>) {
        let excluded = &nera_ui::state::settings(cx).excluded_paths;
        if excluded.is_empty() {
            return;
        }
        let before = self.selected.len();
        self.selected.retain(|p| {
            !nera_core::paths::is_under_any(p, excluded)
                && !excluded.iter().any(|e| nera_core::paths::is_under(e, p))
        });
        if self.selected.len() != before {
            cx.notify();
        }
    }

    fn request_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_excluded(cx);
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let n = self.selected.len();
        let size = engine::selected_size(self.groups(), &self.selected);
        let this = cx.entity().downgrade();
        let handle = window.window_handle();
        confirm(
            Confirm::destructive(
                format!(
                    "Move {} to the {}?",
                    view::count(n, "duplicate"),
                    nera_core::os::TRASH
                ),
                view::confirm_message(
                    n,
                    size,
                    engine::selection_is_upper_bound(self.groups(), &self.selected),
                ),
                concat!("Move to ", nera_core::trash_name!()),
            ),
            true,
            window,
            cx,
            move |_window, cx| {
                let _ = this.update(cx, |this, cx| this.start_trash(handle, cx));
            },
        );
    }

    fn start_trash(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        self.drop_excluded(cx);
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let groups = self.groups().to_vec();
        let selected = self.selected.clone();
        let progress = self.trash_progress.clone();
        progress.set((0, selected.len()));
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.busy = Busy::Trashing;
        self.cancelling = false;
        self._task = Some(run_blocking(
            cx,
            move || {
                engine::trash_selected(
                    &groups,
                    &selected,
                    &cancel,
                    &|done, total| progress.set((done, total)),
                    &|p| fs_util::trash_item(p),
                )
            },
            move |this, outcome, cx| this.finish_trash(outcome, window, cx),
        ));
        cx.notify();
    }

    fn finish_trash(
        &mut self,
        out: engine::TrashOutcome,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;
        if let Some(report) = self.report.as_mut() {
            engine::apply_removal(&mut report.groups, &out.trashed);
        }
        for p in &out.trashed {
            self.selected.remove(p);
        }
        // Drop selections whose group disappeared.
        let groups = self
            .report
            .as_ref()
            .map(|r| r.groups.as_slice())
            .unwrap_or(&[]);
        let alive: HashSet<&PathBuf> = groups
            .iter()
            .flat_map(|g| g.files.iter().map(|f| &f.path))
            .collect();
        self.selected.retain(|p| alive.contains(p));

        for (path, reason) in &out.failed {
            log::warn!("duplicate not moved to Trash: {}: {reason}", path.display());
        }
        let moved = out.trashed.len();
        let (kind, title, message) =
            view::trash_result(moved, out.failed.len(), out.freed, &first_reason(&out));
        let tone = match kind {
            Kind::Info => Tone::Info,
            Kind::Success => Tone::Success,
            Kind::Warning => Tone::Warning,
            Kind::Danger => Tone::Danger,
        };
        if moved > 0 {
            system_notify(cx, &title, &message);
        }
        toast_later(window, cx, tone, title, message);
    }

    // MARK: Rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let busy = self.is_busy();
        let label = |text: &'static str| div().text_sm().text_color(muted).child(text);

        if let Some(report) = &self.report {
            if report.groups.is_empty() {
                return None;
            }
            let rule = self.prefs.keep_rule;
            let mut rules = ButtonGroup::new("keep-rule")
                .small()
                .outline()
                .disabled(busy);
            for r in KeepRule::ALL {
                rules = rules.child(
                    Button::new(SharedString::from(format!("keep-{:?}", r)))
                        .label(r.label())
                        .selected(r == rule),
                );
            }
            let rules = rules.on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                if let Some(rule) = clicked.first().and_then(|ix| KeepRule::ALL.get(*ix)) {
                    this.set_rule(*rule, cx);
                }
            }));
            return Some(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap(sp::LG)
                    .child(
                        h_flex()
                            .items_center()
                            .gap(sp::MD)
                            .child(label("Keep the"))
                            .child(rules)
                            .child(label("copy of each file")),
                    )
                    .child(
                        Button::new("deselect-all")
                            .small()
                            .ghost()
                            .label("Deselect all")
                            .disabled(busy || self.selected.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.deselect_all(cx))),
                    )
                    .into_any_element(),
            );
        }

        let current = self.prefs.min_size;
        let mut sizes = ButtonGroup::new("min-size")
            .small()
            .outline()
            .disabled(busy);
        for size in engine::MIN_SIZE_CHOICES {
            sizes = sizes.child(
                Button::new(SharedString::from(format!("min-{size}")))
                    .label(format!("{}+", view::threshold(size)))
                    .selected(size == current),
            );
        }
        let sizes = sizes.on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
            if let Some(size) = clicked
                .first()
                .and_then(|ix| engine::MIN_SIZE_CHOICES.get(*ix))
            {
                this.set_min_size(*size, cx);
            }
        }));
        Some(
            h_flex()
                .w_full()
                .items_center()
                .gap(sp::MD)
                .child(label("Minimum file size"))
                .child(sizes)
                .into_any_element(),
        )
    }

    fn render_banners(&self) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let Some(report) = &self.report else {
            return out;
        };
        if !report.denied.is_empty() {
            let n = report.denied.len();
            let first = view::tilde(&report.denied[0]);
            #[cfg(target_os = "macos")]
            out.push(
                Banner::new(Tone::Warning, format!("{} could not be read", view::count(n, "folder")))
                    .message(format!(
                        "Nera Cleaner needs Full Disk Access to look inside {first}{}. Grant access, then scan again.",
                        if n > 1 { " and others" } else { "" }
                    ))
                    .action(
                        Button::new("open-fda")
                            .small()
                            .outline()
                            .label("Open System Settings")
                            .on_click(|_, _, _| fs_util::open(FULL_DISK_ACCESS_PANE)),
                    )
                    .into_any_element(),
            );
            // Windows has no blanket access switch: the folder's own permissions decide.
            #[cfg(windows)]
            out.push(
                Banner::new(
                    Tone::Warning,
                    format!("{} could not be read", view::count(n, "folder")),
                )
                .message(format!(
                    "Windows denied access to {first}{}, so {} skipped.",
                    if n > 1 { " and others" } else { "" },
                    if n > 1 { "they were" } else { "it was" }
                ))
                .into_any_element(),
            );
        }
        if !report.missing.is_empty() {
            let names: Vec<String> = report.missing.iter().map(|p| view::tilde(p)).collect();
            out.push(
                Banner::new(
                    Tone::Info,
                    format!("{} no longer exist", view::count(names.len(), "folder")),
                )
                .message(format!(
                    "Skipped {}. Remove them from the folder list.",
                    names.join(", ")
                ))
                .into_any_element(),
            );
        }
        if report.shared_sets > 0 {
            #[cfg(target_os = "macos")]
            let message = "Identical files that are hard links or APFS clones (for example Finder copies) of each other, or of files outside the scanned folders, share their disk space. Deleting them frees nothing, so they are not listed.";
            #[cfg(windows)]
            let message = "Identical files that are hard links of each other, or of files outside the scanned folders, share their disk space. Deleting them frees nothing, so they are not listed.";
            out.push(
                Banner::new(
                    Tone::Info,
                    format!(
                        "{} share their disk space",
                        view::count(report.shared_sets, "set")
                    ),
                )
                .message(message)
                .into_any_element(),
            );
        }
        if report.unreadable > 0 {
            out.push(
                Banner::new(
                    Tone::Info,
                    format!(
                        "{} could not be compared",
                        view::count(report.unreadable, "file")
                    ),
                )
                .message("They could not be read and were left out of the results.")
                .into_any_element(),
            );
        }
        out
    }

    fn render_progress(&self, cx: &mut Context<Self>) -> AnyElement {
        let cancel = cx.listener(|this, _, _, cx| this.cancel_work(cx));
        let panel = match self.busy {
            Busy::Trashing => {
                let (done, total) = self.trash_progress.get();
                ProgressPanel::new(
                    "trash-progress",
                    concat!("Moving duplicates to the ", nera_core::trash_name!()),
                )
                .detail(format!("{} of {}", done, view::count(total, "file")))
                .fraction(Some(if total == 0 {
                    0.0
                } else {
                    done as f64 / total as f64
                }))
            }
            _ => {
                let p = self.scan_progress.get();
                let (title, detail) = match p.phase {
                    Phase::Indexing => (
                        "Looking for files",
                        format!(
                            "{} checked{}",
                            view::count(p.files_seen, "item"),
                            p.current
                                .as_ref()
                                .map(|c| format!(" · {c}"))
                                .unwrap_or_default()
                        ),
                    ),
                    Phase::Comparing => ("Comparing files of the same size", progress_detail(&p)),
                    Phase::Verifying => ("Verifying matches", progress_detail(&p)),
                };
                ProgressPanel::new("scan-progress", title)
                    .detail(detail)
                    .fraction(p.fraction())
            }
        };
        let panel = if self.cancelling {
            panel.detail("Stopping…")
        } else {
            panel.on_cancel(cancel)
        };
        Section::untitled().child(panel).into_any_element()
    }

    fn render_folders(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let hidden = nera_ui::state::settings(cx).scan_hidden_files;
        #[cfg(target_os = "macos")]
        let description = if hidden {
            "Searched with all subfolders. App bundles and symlinks are skipped."
        } else {
            "Searched with all subfolders. App bundles, symlinks and hidden files are skipped."
        };
        #[cfg(windows)]
        let description = if hidden {
            "Searched with all subfolders. Symbolic links, junctions and online-only OneDrive files are skipped."
        } else {
            "Searched with all subfolders. Symbolic links, junctions, online-only OneDrive files and hidden files are skipped."
        };
        let add = Button::new("add-folder")
            .small()
            .ghost()
            .icon(icon("folder-plus"))
            .label("Add folder")
            .disabled(busy)
            .on_click(cx.listener(|this, _, _, cx| this.pick_folders(cx)));
        let section = Section::new("Folders")
            .description(description)
            .trailing(add);
        if self.prefs.folders.is_empty() {
            return section
                .child(
                    EmptyState::new("folder-search", "No folders chosen")
                        .message("Add a folder to search it for duplicate files."),
                )
                .into_any_element();
        }
        let rows = self.prefs.folders.iter().map(|folder| {
            let key = folder.to_string_lossy().into_owned();
            let path = folder.clone();
            Row::new(
                SharedString::from(format!("folder:{key}")),
                display_name(folder),
            )
            .icon("folder")
            .subtitle(view::tilde(folder))
            .action(
                Button::new(SharedString::from(format!("remove:{key}")))
                    .small()
                    .ghost()
                    .icon(icon("x"))
                    .tooltip("Remove folder")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_folder(&path, cx))),
            )
        });
        section
            .child(ListCard::new().children(rows))
            .into_any_element()
    }

    fn render_results(&self, report: &ScanReport, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if report.groups.is_empty() {
            out.push(
                Section::untitled()
                    .child(
                        EmptyState::new("circle-check", "No duplicates found")
                            .tone(Tone::Success)
                            .message(view::empty_message(report, self.scanned_min_size)),
                    )
                    .into_any_element(),
            );
            return out;
        }

        let tile = |t: view::Tile, icon_name: &'static str| {
            StatTile::new(t.0, t.1).icon(icon_name).caption(t.2)
        };
        out.push(
            Section::untitled()
                .child(
                    StatGrid::new(4)
                        .child(
                            tile(view::tile_reclaimable(report), "hard-drive").tone(Tone::Accent),
                        )
                        .child(tile(view::tile_sets(report), "files"))
                        .child(tile(view::tile_extra(report), "copy"))
                        .child(tile(
                            view::tile_compared(report, self.scanned_min_size),
                            "scan-search",
                        )),
                )
                .into_any_element(),
        );

        let rule = self.prefs.keep_rule;
        let busy = self.is_busy();
        let shown = self.shown_groups.min(report.groups.len());
        let mut section = Section::new("Duplicate sets")
            .description(if report.groups.iter().any(|g| g.has_pinned()) {
                concat!("Selected copies go to the ", nera_core::trash_name!(), ". One copy of each file is always kept, and a copy that shares disk space with a file outside the scanned folders is kept first.")
            } else {
                concat!("Selected copies go to the ", nera_core::trash_name!(), ". One copy of each file is always kept.")
            })
            .trailing(Badge::new(view::count(report.groups.len(), "set")));
        for group in &report.groups[..shown] {
            section = section.child(self.render_group(group, rule, busy, cx));
        }
        if shown < report.groups.len() {
            let more = (report.groups.len() - shown).min(GROUP_PAGE);
            section = section.child(
                h_flex().w_full().justify_center().child(
                    Button::new("show-more")
                        .small()
                        .ghost()
                        .label(format!("Show {more} more"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shown_groups += GROUP_PAGE;
                            cx.notify();
                        })),
                ),
            );
        }
        out.push(section.into_any_element());
        out
    }

    fn render_group(
        &self,
        group: &DuplicateGroup,
        rule: KeepRule,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Header box: the keep rule's picks are all ticked (files sharing the keeper's data are never picked).
        let picks = engine::auto_select_group(group, rule);
        let all_but_one = !picks.is_empty() && picks.iter().all(|p| self.selected.contains(p));
        let keep = engine::keeper(group, rule).map(|f| f.path.clone());
        let label = view::group_label(group);
        let gid = group.id.clone();
        let header = list_header(
            &group.id,
            label,
            None,
            Some((
                all_but_one,
                Box::new(cx.listener(move |this, checked: &bool, _, cx| {
                    this.toggle_group(&gid, *checked, cx)
                })),
            )),
        );
        let rows = group.files.iter().map(|file| {
            let key = file.path.to_string_lossy().into_owned();
            let checked = self.selected.contains(&file.path);
            let toggle_path = file.path.clone();
            let reveal_path = file.path.clone();
            let subtitle = view::file_subtitle(file);
            let shared = group.shares_data(file);
            Row::new(SharedString::from(key.clone()), file.name())
                .checkbox(
                    checked,
                    cx.listener(move |this, checked: &bool, window, cx| {
                        this.toggle_file(&toggle_path, *checked, window, cx)
                    }),
                )
                .icon(file_icon(&file.path))
                .subtitle(subtitle)
                .when(shared, |row| {
                    row.badge(Badge::new("Shares space").tone(Tone::Info))
                })
                .when(!checked, |row| {
                    let tone = if keep.as_ref() == Some(&file.path) {
                        Tone::Success
                    } else {
                        Tone::Neutral
                    };
                    row.badge(Badge::new("Keep").tone(tone))
                })
                .value(format::bytes(file.size))
                .disabled(busy)
                .action(
                    Button::new(SharedString::from(format!("reveal:{key}")))
                        .small()
                        .ghost()
                        .icon(icon("folder-search"))
                        .tooltip(if cfg!(target_os = "macos") {
                            "Reveal in Finder"
                        } else {
                            nera_core::os::SHOW_IN_FILE_MANAGER
                        })
                        .on_click(move |_, _, _| fs_util::reveal_in_finder(&reveal_path)),
                )
        });
        ListCard::new()
            .header(header)
            .children(rows)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let report = self.report.as_ref()?;
        if report.groups.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let n = self.selected.len();
        let size = engine::selected_size(&report.groups, &self.selected);
        let summary = view::footer_summary(
            n,
            size,
            engine::selection_is_upper_bound(&report.groups, &self.selected),
        );
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
                        .child(div().text_xs().text_color(muted).child(concat!(
                            "Moved to the ",
                            nera_core::trash_name!(),
                            ", so you can restore them. Space is freed when you empty it."
                        ))),
                )
                .child(
                    Button::new("move-to-trash")
                        .danger()
                        .icon(icon("trash"))
                        .label(view::trash_button_label(n))
                        .disabled(self.is_busy() || n == 0)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.request_trash(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Drop for DuplicatesScreen {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Render for DuplicatesScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let has_report = self.report.is_some();

        let mut page = Page::new("duplicates-page", Screen::Duplicates.title())
            .subtitle("Identical files taking up space twice");
        if has_report {
            page = page.action(
                Button::new("start-over")
                    .ghost()
                    .icon(icon("folders"))
                    .label("Change folders")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.start_over(cx))),
            );
        }
        page = page.action(
            Button::new("scan")
                .primary()
                .icon(icon("scan-search"))
                .label(if has_report { "Scan again" } else { "Scan" })
                .loading(self.busy == Busy::Scanning)
                .disabled(busy || self.prefs.folders.is_empty())
                .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
        );
        if let Some(toolbar) = self.render_toolbar(cx) {
            page = page.toolbar(toolbar);
        }
        if let Some(footer) = self.render_footer(cx) {
            page = page.footer(footer);
        }

        let banners = self.render_banners();
        if !banners.is_empty() {
            page = page.child(Section::untitled().children(banners));
        }
        if busy {
            page = page.child(self.render_progress(cx));
        }
        match &self.report {
            Some(report) if self.busy != Busy::Scanning => {
                for el in self.render_results(report, cx) {
                    page = page.child(el);
                }
            }
            None if self.busy != Busy::Scanning => {
                page = page.child(self.render_folders(cx));
            }
            _ => {}
        }
        page
    }
}

// MARK: - Helpers

/// Show a toast once the current update finishes (async callbacks have no
/// `Window` of their own).
fn toast_later(window: AnyWindowHandle, cx: &mut App, tone: Tone, title: String, message: String) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
    });
}

fn first_reason(out: &engine::TrashOutcome) -> String {
    out.failed
        .first()
        .map(|(_, r)| format!("{r}."))
        .unwrap_or_default()
}

fn progress_detail(p: &ScanProgress) -> String {
    format!(
        "{} of {}{}",
        view::thousands(p.done),
        view::count(p.total, "file"),
        p.current
            .as_ref()
            .map(|c| format!(" · {c}"))
            .unwrap_or_default()
    )
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn file_icon(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "webp" | "tif" | "tiff" | "bmp"
        | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" | "svg" => "file-image",
        "mov" | "mp4" | "m4v" | "avi" | "mkv" | "webm" | "wmv" => "file-video-camera",
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "aiff" | "aif" | "ogg" => "file-music",
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "dmg" | "iso" | "pkg" => {
            "file-archive"
        }
        "pdf" | "txt" | "md" | "rtf" | "doc" | "docx" | "pages" | "odt" | "epub" => "file-text",
        "xls" | "xlsx" | "csv" | "numbers" => "file-spreadsheet",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::file_icon;
    use std::path::Path;

    #[test]
    fn formats_helpers() {
        assert_eq!(file_icon(Path::new("/a/B.JPG")), "file-image");
        assert_eq!(file_icon(Path::new("/a/b")), "file");
    }
}
