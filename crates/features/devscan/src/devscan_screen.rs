//! Developer junk screen: pick folders, scan for projects, review the
//! regenerable build outputs / dependency folders inside them, and clean the
//! ones the user ticks. All file work lives in `crate::engine`.

use crate::engine::{self, Phase, Progress, Project, ScanOptions, ScanOutcome};
use crate::view::{self, Kind};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use nera_core::format;
use nera_core::fs_util::{self, Cancel};
use nera_ui::nav::Screen;
use nera_ui::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const FULL_DISK_ACCESS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
/// Projects start expanded when there are at most this many.
const AUTO_EXPAND_MAX: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    Scanning,
    Cleaning,
}

type Notice = (Tone, SharedString, SharedString);

pub struct DevScanScreen {
    roots: Vec<PathBuf>,
    outcome: Option<ScanOutcome>,
    /// Junk folders ticked for cleaning, by path.
    selected: HashSet<PathBuf>,
    /// Project roots whose junk rows are shown.
    expanded: HashSet<PathBuf>,
    busy: Busy,
    cancelling: bool,
    scan_progress: Shared<Progress>,
    clean_progress: Shared<(usize, usize)>,
    cancel: Cancel,
    notice: Option<Notice>,
    _task: Option<Task<()>>,
    _picker: Option<Task<()>>,
}

impl DevScanScreen {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        let roots = engine::load_or_default_roots(&engine::roots_file(), &nera_core::paths::home());
        Self {
            roots,
            outcome: None,
            selected: HashSet::new(),
            expanded: HashSet::new(),
            busy: Busy::Idle,
            cancelling: false,
            scan_progress: Shared::new(Progress::default()),
            clean_progress: Shared::new((0, 0)),
            cancel: Cancel::new(),
            notice: None,
            _task: None,
            _picker: None,
        }
    }

    fn is_busy(&self) -> bool {
        self.busy != Busy::Idle
    }

    fn projects(&self) -> &[Project] {
        self.outcome
            .as_ref()
            .map(|o| o.projects.as_slice())
            .unwrap_or(&[])
    }

    // MARK: Scan folders

    fn save_roots(&self, cx: &mut Context<Self>) {
        let roots = self.roots.clone();
        cx.background_spawn(async move {
            if let Err(err) = engine::save_roots(&engine::roots_file(), &roots) {
                log::warn!("could not save developer junk folders: {err}");
            }
        })
        .detach();
    }

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
        let mut covered = Vec::new();
        for p in paths {
            if let engine::AddRoot::AlreadyCovered(by) = engine::add_root(&mut self.roots, &p) {
                covered.push((p, by));
            }
        }
        self.notice = covered.first().map(|(p, by)| {
            (
                Tone::Info,
                SharedString::from("Folder already included"),
                SharedString::from(format!(
                    "{} is inside {}, which is already scanned.",
                    engine::display_path(p),
                    engine::display_path(by)
                )),
            )
        });
        self.save_roots(cx);
        cx.notify();
    }

    fn remove_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.roots.retain(|p| p != path);
        self.save_roots(cx);
        cx.notify();
    }

    fn reset_folders(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.roots = engine::default_roots(&nera_core::paths::home());
        self.save_roots(cx);
        cx.notify();
    }

    // MARK: Scan

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() || self.roots.is_empty() {
            return;
        }
        let settings = nera_ui::state::settings(cx);
        let opts = ScanOptions {
            roots: self.roots.clone(),
            excluded: settings.excluded_paths.clone(),
        };
        self.outcome = None;
        self.selected.clear();
        self.expanded.clear();
        self.notice = None;
        self.busy = Busy::Scanning;
        self.cancelling = false;

        let progress = self.scan_progress.clone();
        progress.set(Progress::default());
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self._task = Some(run_blocking(
            cx,
            move || engine::scan(&opts, &cancel, &|p| progress.set(p.clone())),
            |this, outcome, _cx| this.finish_scan(outcome),
        ));
        cx.notify();
    }

    fn finish_scan(&mut self, outcome: ScanOutcome) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;
        if outcome.projects.len() <= AUTO_EXPAND_MAX {
            self.expanded = outcome.projects.iter().map(|p| p.root.clone()).collect();
        }
        self.outcome = Some(outcome);
    }

    fn cancel_work(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        self.cancelling = true;
        cx.notify();
    }

    // MARK: Selection

    fn selected_size(&self) -> u64 {
        self.projects()
            .iter()
            .flat_map(|p| p.junk.iter())
            .filter(|j| self.selected.contains(&j.path))
            .map(|j| j.size)
            .sum()
    }

    /// Part of the selection that is guaranteed to be freed.
    fn selected_unique(&self) -> u64 {
        self.projects()
            .iter()
            .flat_map(|p| p.junk.iter())
            .filter(|j| self.selected.contains(&j.path))
            .map(|j| j.unique)
            .sum()
    }

    fn all_selected(&self) -> bool {
        let total = self.outcome.as_ref().map(|o| o.junk_count()).unwrap_or(0);
        total > 0 && self.selected.len() == total
    }

    fn project_selected(&self, project: &Project) -> bool {
        !project.junk.is_empty() && project.junk.iter().all(|j| self.selected.contains(&j.path))
    }

    fn toggle_junk(&mut self, path: &Path, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if checked {
            self.selected.insert(path.to_path_buf());
        } else {
            self.selected.remove(path);
        }
        cx.notify();
    }

    fn toggle_project(&mut self, root: &Path, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let paths: Vec<PathBuf> = self
            .projects()
            .iter()
            .find(|p| p.root == root)
            .map(|p| p.junk.iter().map(|j| j.path.clone()).collect())
            .unwrap_or_default();
        for p in paths {
            if checked {
                self.selected.insert(p);
            } else {
                self.selected.remove(&p);
            }
        }
        cx.notify();
    }

    fn toggle_all(&mut self, checked: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.selected.clear();
        if checked {
            let all: Vec<PathBuf> = self
                .projects()
                .iter()
                .flat_map(|p| p.junk.iter().map(|j| j.path.clone()))
                .collect();
            self.selected.extend(all);
        }
        cx.notify();
    }

    fn toggle_expanded(&mut self, root: &Path, cx: &mut Context<Self>) {
        if !self.expanded.remove(root) {
            self.expanded.insert(root.to_path_buf());
        }
        cx.notify();
    }

    fn set_all_expanded(&mut self, expand: bool, cx: &mut Context<Self>) {
        self.expanded.clear();
        if expand {
            let roots: Vec<PathBuf> = self.projects().iter().map(|p| p.root.clone()).collect();
            self.expanded.extend(roots);
        }
        cx.notify();
    }

    // MARK: Clean

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

    fn request_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_excluded(cx);
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let n = self.selected.len();
        let size = self.selected_size();
        let note = view::shared_note(size, self.selected_unique());
        let to_trash = nera_ui::state::settings(cx).move_to_trash;
        let this = cx.entity().downgrade();
        let handle = window.window_handle();
        let trash = nera_core::os::TRASH;
        let (title, message, action) = if to_trash {
            (
                format!("Move {} to the {trash}?", view::count(n, "folder")),
                format!(
                    "{} ({} on disk) will be moved to the {trash}. Dependencies and build output can be rebuilt, and you can restore them from the {trash}.{note}",
                    view::count(n, "folder"),
                    format::bytes(size)
                ),
                concat!("Move to ", nera_core::trash_name!()),
            )
        } else {
            (
                format!("Delete {} permanently?", view::count(n, "folder")),
                format!(
                    "{} ({} on disk) will be deleted permanently and can't be restored. Dependencies and build output can be rebuilt.{note}",
                    view::count(n, "folder"),
                    format::bytes(size)
                ),
                "Delete",
            )
        };
        confirm(
            Confirm::destructive(title, message, action),
            // A permanent delete always asks, whatever the preference says.
            to_trash,
            window,
            cx,
            move |_window, cx| {
                let _ = this.update(cx, |this, cx| this.start_clean(handle, cx));
            },
        );
    }

    fn start_clean(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        self.drop_excluded(cx);
        if self.is_busy() || self.selected.is_empty() {
            return;
        }
        let mut paths: Vec<PathBuf> = self.selected.iter().cloned().collect();
        paths.sort();
        let to_trash = nera_ui::state::settings(cx).move_to_trash;
        // Sizes of what we are about to remove, to report what was freed.
        let sizes: Vec<(PathBuf, u64, u64)> = self
            .projects()
            .iter()
            .flat_map(|p| p.junk.iter())
            .filter(|j| self.selected.contains(&j.path))
            .map(|j| (j.path.clone(), j.size, j.unique))
            .collect();
        let progress = self.clean_progress.clone();
        progress.set((0, paths.len()));
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        self.busy = Busy::Cleaning;
        self.cancelling = false;
        self.notice = None;
        self._task = Some(run_blocking(
            cx,
            move || {
                engine::clean(&paths, to_trash, &cancel, &|done, total| {
                    progress.set((done, total))
                })
            },
            move |this, report, cx| this.finish_clean(report, sizes, to_trash, window, cx),
        ));
        cx.notify();
    }

    fn finish_clean(
        &mut self,
        report: engine::CleanReport,
        sizes: Vec<(PathBuf, u64, u64)>,
        to_trash: bool,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.busy = Busy::Idle;
        self.cancelling = false;
        self._task = None;

        let removed: HashSet<&PathBuf> = report.removed.iter().collect();
        let size: u64 = sizes
            .iter()
            .filter(|(p, ..)| removed.contains(p))
            .map(|(_, s, _)| *s)
            .sum();
        let unique: u64 = sizes
            .iter()
            .filter(|(p, ..)| removed.contains(p))
            .map(|(_, _, u)| *u)
            .sum();
        if let Some(outcome) = self.outcome.as_mut() {
            engine::apply_removed(&mut outcome.projects, &report.removed);
        }
        let alive: HashSet<PathBuf> = self
            .projects()
            .iter()
            .flat_map(|p| p.junk.iter().map(|j| j.path.clone()))
            .collect();
        self.selected.retain(|p| alive.contains(p));
        let live_roots: HashSet<PathBuf> = self.projects().iter().map(|p| p.root.clone()).collect();
        self.expanded.retain(|r| live_roots.contains(r));

        for (path, reason) in &report.failed {
            log::warn!("developer junk not removed: {}: {reason}", path.display());
        }
        let n = report.removed.len();
        let first = report
            .failed
            .first()
            .map(|(p, why)| format!("{}: {why}", engine::display_path(p)))
            .unwrap_or_default();
        let (kind, title, message) =
            view::clean_result(n, report.failed.len(), size, unique, to_trash, &first);
        let tone = match kind {
            Kind::Info => Tone::Info,
            Kind::Success => Tone::Success,
            Kind::Warning => Tone::Warning,
            Kind::Danger => Tone::Danger,
        };
        self.notice = Some((tone, title.clone().into(), message.clone().into()));
        if n > 0 {
            system_notify(cx, "Developer junk cleaned", &format!("{title}. {message}"));
        }
        let _ = window.update(cx, |_, window, cx| notify(window, cx, tone, title, message));
    }

    // MARK: Render pieces

    fn render_banners(&self) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some((tone, title, message)) = &self.notice {
            out.push(
                Banner::new(*tone, title.clone())
                    .message(message.clone())
                    .into_any_element(),
            );
        }
        let Some(outcome) = &self.outcome else {
            return out;
        };
        if outcome.cancelled {
            out.push(
                Banner::new(Tone::Info, "Scan stopped early")
                    .message("These results are partial. Scan again to search every folder.")
                    .into_any_element(),
            );
        }
        if outcome.privacy_denied {
            let first = outcome
                .unreadable
                .first()
                .map(|p| engine::display_path(p))
                .unwrap_or_default();
            out.push(
                Banner::new(Tone::Warning, "Some folders are protected by macOS")
                    .message(format!(
                        "Nera Cleaner needs Full Disk Access to look inside {first}{}. Grant access, then scan again.",
                        if outcome.unreadable_count > 1 { " and others" } else { "" }
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
        } else if outcome.unreadable_count > 0 {
            out.push(
                Banner::new(
                    Tone::Info,
                    format!(
                        "{} could not be read",
                        view::count(outcome.unreadable_count, "folder")
                    ),
                )
                .message("They were skipped and are not in the results.")
                .into_any_element(),
            );
        }
        if !outcome.missing_roots.is_empty() {
            let names: Vec<String> = outcome
                .missing_roots
                .iter()
                .map(|p| engine::display_path(p))
                .collect();
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
        out
    }

    fn render_folders(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let reset = Button::new("reset-folders")
            .small()
            .ghost()
            .icon(icon("rotate-ccw"))
            .label("Use defaults")
            .disabled(busy)
            .on_click(cx.listener(|this, _, _, cx| this.reset_folders(cx)));
        #[cfg(target_os = "macos")]
        let description = "Projects are searched with all subfolders. Hidden folders, symlinks and app bundles are skipped.";
        #[cfg(windows)]
        let description = "Projects are searched with all subfolders. Hidden folders, symlinks and junctions are skipped.";
        let section = Section::new("Folders to scan")
            .description(description)
            .trailing(reset);
        if self.roots.is_empty() {
            return section
                .child(
                    EmptyState::new("folder-search", "No folders chosen")
                        .message("Add the folder where you keep your code, or use the usual developer folders.")
                        .action(
                            Button::new("add-folder-empty")
                                .outline()
                                .icon(icon("folder-plus"))
                                .label("Add folder…")
                                .on_click(cx.listener(|this, _, _, cx| this.pick_folders(cx))),
                        ),
                )
                .into_any_element();
        }
        let rows = self.roots.iter().map(|folder| {
            let key = folder.to_string_lossy().into_owned();
            let path = folder.clone();
            let name = folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| key.clone());
            Row::new(SharedString::from(format!("root:{key}")), name)
                .icon("folder")
                .subtitle(engine::display_path(folder))
                .action(
                    Button::new(SharedString::from(format!("remove-root:{key}")))
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

    fn render_progress(&self, cx: &mut Context<Self>) -> AnyElement {
        let cancel = cx.listener(|this, _, _, cx| this.cancel_work(cx));
        let panel = match self.busy {
            Busy::Scanning => {
                let p = self.scan_progress.get();
                let (title, detail) = match p.phase {
                    Phase::Discovering => (
                        "Looking for projects",
                        format!(
                            "{} scanned · {} found{}",
                            view::count(p.folders_scanned, "folder"),
                            view::count(p.projects_found, "project"),
                            if p.current.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", p.current)
                            }
                        ),
                    ),
                    Phase::Measuring => (
                        "Measuring build folders",
                        format!(
                            "{} of {}",
                            view::thousands(p.measured),
                            view::count(p.to_measure, "folder")
                        ),
                    ),
                };
                ProgressPanel::new(
                    "scan-progress",
                    if self.cancelling {
                        "Stopping…"
                    } else {
                        title
                    },
                )
                .detail(detail)
                .fraction(p.fraction())
            }
            _ => {
                let (done, total) = self.clean_progress.get();
                let to_trash = nera_ui::state::settings(cx).move_to_trash;
                ProgressPanel::new(
                    "clean-progress",
                    if self.cancelling {
                        "Stopping…"
                    } else if to_trash {
                        concat!("Moving to the ", nera_core::trash_name!())
                    } else {
                        "Deleting"
                    },
                )
                .detail(format!(
                    "{} of {}",
                    view::thousands(done),
                    view::count(total, "folder")
                ))
                .fraction((total > 0).then(|| done as f64 / total as f64))
            }
        };
        Section::untitled()
            .child(panel.on_cancel(cancel))
            .into_any_element()
    }

    fn render_results(&self, outcome: &ScanOutcome, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if outcome.projects.is_empty() {
            let message = if outcome.cancelled {
                "The scan was stopped before anything was found.".to_string()
            } else {
                view::empty_message(outcome)
            };
            out.push(
                Section::untitled()
                    .child(
                        EmptyState::new("circle-check", "No developer junk found")
                            .tone(Tone::Success)
                            .message(message),
                    )
                    .into_any_element(),
            );
            return out;
        }

        let busy = self.is_busy();
        let n_sel = self.selected.len();
        let (l1, v1, c1) = view::tile_size(outcome);
        let (l2, v2, c2) = view::tile_projects(outcome);
        let (l3, v3, c3) = view::tile_selected(self.selected_size(), self.selected_unique(), n_sel);
        out.push(
            Section::untitled()
                .child(
                    StatGrid::new(3)
                        .child(
                            StatTile::new(l1, v1)
                                .icon("hard-drive")
                                .tone(Tone::Accent)
                                .caption(c1),
                        )
                        .child(StatTile::new(l2, v2).icon("folder-code").caption(c2))
                        .child(
                            StatTile::new(l3, v3)
                                .icon("check")
                                .tone(if n_sel > 0 {
                                    Tone::Success
                                } else {
                                    Tone::Neutral
                                })
                                .caption(c3),
                        ),
                )
                .into_any_element(),
        );

        let all_expanded = self.expanded.len() == outcome.projects.len();
        let toggle_all = Button::new("expand-all")
            .small()
            .ghost()
            .icon(icon(if all_expanded {
                "chevrons-down-up"
            } else {
                "chevrons-up-down"
            }))
            .label(if all_expanded {
                "Collapse all"
            } else {
                "Expand all"
            })
            .on_click(cx.listener(move |this, _, _, cx| this.set_all_expanded(!all_expanded, cx)));

        let now = SystemTime::now();
        let mut list = ListCard::new().header(list_header(
            "projects",
            "Projects, largest first",
            Some("Size"),
            Some((
                self.all_selected(),
                Box::new(
                    cx.listener(move |this, checked: &bool, _, cx| this.toggle_all(*checked, cx)),
                ),
            )),
        ));
        for project in &outcome.projects {
            list = list.child(self.project_row(project, now, busy, cx));
            if self.expanded.contains(&project.root) {
                for junk in &project.junk {
                    let path = junk.path.clone();
                    let key = junk.path.to_string_lossy().into_owned();
                    let sub = view::junk_subtitle(junk, now);
                    let row = Row::new(
                        SharedString::from(format!("junk:{key}")),
                        junk.relative.clone(),
                    )
                    .icon("folder")
                    .subtitle(sub)
                    .badge(Badge::new(junk.ecosystem.label()))
                    .value(format::bytes(junk.size))
                    .selected(self.selected.contains(&junk.path))
                    .disabled(busy)
                    .checkbox(
                        self.selected.contains(&junk.path),
                        cx.listener(move |this, checked: &bool, _, cx| {
                            this.toggle_junk(&path, *checked, cx)
                        }),
                    );
                    list = list.child(div().w_full().pl(sp::XL).child(row));
                }
            }
        }
        out.push(
            Section::new("Projects")
                .trailing(toggle_all)
                .child(list)
                .into_any_element(),
        );
        out
    }

    fn project_row(
        &self,
        project: &Project,
        now: SystemTime,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = project.root.to_string_lossy().into_owned();
        let root = project.root.clone();
        let toggle_root = project.root.clone();
        let open = self.expanded.contains(&project.root);
        let n_sel = project
            .junk
            .iter()
            .filter(|j| self.selected.contains(&j.path))
            .count();
        let sub = view::project_subtitle(project, n_sel, now);
        let mut row = Row::new(
            SharedString::from(format!("project:{key}")),
            project.name.clone(),
        )
        .icon(project.primary().icon())
        .subtitle(sub)
        .value(format::bytes(project.size()))
        .disabled(busy)
        .checkbox(
            self.project_selected(project),
            cx.listener(move |this, checked: &bool, _, cx| {
                this.toggle_project(&root, *checked, cx)
            }),
        )
        .action(
            Button::new(SharedString::from(format!("expand:{key}")))
                .small()
                .ghost()
                .icon(icon(if open {
                    "chevron-down"
                } else {
                    "chevron-right"
                }))
                .tooltip(if open { "Hide folders" } else { "Show folders" })
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_expanded(&toggle_root, cx)),
                ),
        );
        for label in view::project_badges(project, now) {
            row = row.badge(Badge::new(label).tone(if label == "Inactive" {
                Tone::Info
            } else {
                Tone::Accent
            }));
        }
        row.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let outcome = self.outcome.as_ref()?;
        if outcome.projects.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let n = self.selected.len();
        let size = self.selected_size();
        let to_trash = nera_ui::state::settings(cx).move_to_trash;
        let summary = view::footer_summary(n, size, self.selected_unique());
        let hint = if to_trash {
            concat!(
                "Moved to the ",
                nera_core::trash_name!(),
                ", so you can restore them."
            )
        } else {
            "Deleted permanently. Change this in Settings."
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
                        .label(if n == 0 {
                            "Clean".to_string()
                        } else {
                            format!("Clean {}", format::bytes(size))
                        })
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

impl Drop for DevScanScreen {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Render for DevScanScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let has_results = self
            .outcome
            .as_ref()
            .is_some_and(|o| !o.projects.is_empty());

        let mut page = Page::new("devscan-page", Screen::DevScan.title())
            .subtitle("Build outputs and dependency folders in your projects")
            .action(
                Button::new("add-folder")
                    .outline()
                    .icon(icon("folder-plus"))
                    .label("Add folder…")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.pick_folders(cx))),
            )
            .action(
                Button::new("scan")
                    .when(!has_results, |b| b.primary())
                    .when(has_results, |b| b.outline())
                    .icon(icon("scan-search"))
                    .label(if self.outcome.is_some() {
                        "Scan again"
                    } else {
                        "Scan"
                    })
                    .loading(self.busy == Busy::Scanning)
                    .disabled(busy || self.roots.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
            );
        if let Some(footer) = self.render_footer(cx) {
            page = page.footer(footer);
        }

        let banners = self.render_banners();
        if !banners.is_empty() {
            page = page.child(v_flex().w_full().gap(sp::MD).children(banners));
        }
        page = page.child(self.render_folders(cx));
        if busy {
            page = page.child(self.render_progress(cx));
        }
        if let Some(outcome) = &self.outcome {
            // Results stay visible (disabled) while cleaning; hidden while rescanning.
            if self.busy != Busy::Scanning {
                page = page.children(self.render_results(outcome, cx));
            }
        } else if !busy && self.roots.is_empty() {
            // The folder section already shows the empty state.
        } else if !busy {
            page = page.child(
                Section::untitled().child(
                    EmptyState::new(Screen::DevScan.icon(), "Find developer junk")
                        .message("Scan your project folders for node_modules, build output, caches and other folders you can rebuild.")
                        .action(
                            Button::new("scan-empty")
                                .outline()
                                .icon(icon("scan-search"))
                                .label("Scan")
                                .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
                        ),
                ),
            );
        }
        page
    }
}
