//! Settings screen: appearance, cleanup, scanning, excluded folders,
//! permissions, storage and about. Every control edits
//! `nera_core::settings::Settings` through `nera_ui::state::update_settings`,
//! which persists and refreshes all windows.
//!
//! Port of `SettingsView` / `SettingsViewModel` / `AboutView`.

use crate::engine::{self, AccessReport, AddExcluded, FullDiskAccess};
use crate::view::{self, PermissionTone};
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    h_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, StyledExt as _,
};
use gpui_kit::*;
use nera_core::fs_util;
use nera_core::settings::{Settings, ThemeMode};
use nera_ui::nav::Screen;
use nera_ui::state::{settings, update_settings};
use nera_ui::*;

/// Message shown under the folder list after an add attempt.
struct Notice {
    tone: Tone,
    title: String,
    message: String,
}

pub struct SettingsScreen {
    access: Option<AccessReport>,
    checking_access: bool,
    cache_size: Option<u64>,
    notice: Option<Notice>,
    tasks: Vec<Task<()>>,
}

impl SettingsScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            access: None,
            checking_access: false,
            cache_size: None,
            notice: None,
            tasks: Vec::new(),
        };
        if engine::HAS_FULL_DISK_ACCESS {
            this.check_access(cx);
        }
        this.measure_cache(cx);
        this
    }

    fn check_access(&mut self, cx: &mut Context<Self>) {
        if self.checking_access {
            return;
        }
        self.checking_access = true;
        let task = run_blocking(cx, engine::check_full_disk_access, |this, report, _| {
            this.access = Some(report);
            this.checking_access = false;
        });
        self.tasks.push(task);
    }

    fn measure_cache(&mut self, cx: &mut Context<Self>) {
        let task = run_blocking(cx, engine::app_cache_size, |this, size, _| {
            this.cache_size = Some(size)
        });
        self.tasks.push(task);
    }

    /// Empty Nera's own cache folder, then re-measure it.
    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        self.cache_size = None;
        let task = run_blocking(cx, fs_util::clear_app_cache, |this, result, cx| {
            let (tone, title, message) = match result {
                Ok(freed) => (
                    Tone::Success,
                    "Cache cleared",
                    format!("Freed {}.", nera_core::format::bytes(freed)),
                ),
                Err(e) => {
                    log::warn!("clearing the app cache failed: {e}");
                    (Tone::Danger, "Couldn't clear the cache", e.to_string())
                }
            };
            let id = cx.entity_id();
            cx.with_window(id, |window, cx| notify(window, cx, tone, title, message));
            this.measure_cache(cx);
        });
        self.tasks.push(task);
        cx.notify();
    }

    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Exclude".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.add_folders(paths, cx));
        })
        .detach();
    }

    fn add_folders(&mut self, picked: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let mut list = settings(cx).excluded_paths.clone();
        let mut added = 0usize;
        let mut notice = None;
        for path in &picked {
            match engine::add_excluded(&mut list, path) {
                AddExcluded::Added(_) => added += 1,
                AddExcluded::AlreadyCovered(p) => {
                    notice = Some(Notice {
                        tone: Tone::Info,
                        title: "Already excluded".into(),
                        message: format!(
                            "{} is already covered by an excluded folder.",
                            engine::display_path(&p)
                        ),
                    })
                }
                AddExcluded::Rejected(why) => {
                    notice = Some(Notice {
                        tone: Tone::Warning,
                        title: "Folder not added".into(),
                        message: why,
                    })
                }
            }
        }
        self.notice = if added > 0 { None } else { notice };
        if added > 0 {
            update_settings(cx, |s| s.excluded_paths = list);
        }
        cx.notify();
    }

    fn remove_folder(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let mut list = settings(cx).excluded_paths.clone();
        if engine::remove_excluded(&mut list, path) {
            update_settings(cx, |s| s.excluded_paths = list);
        }
        self.notice = None;
        cx.notify();
    }

    // -- Rendering helpers ---------------------------------------------------

    fn switch_row(
        id: &'static str,
        icon_name: &'static str,
        title: &'static str,
        description: &'static str,
        checked: bool,
        set: fn(&mut Settings, bool),
    ) -> Row {
        Row::new(id, title)
            .icon(icon_name)
            .subtitle(description)
            .action(
                Switch::new(SharedString::from(format!("{id}-switch")))
                    .checked(checked)
                    .accessibility_label(title)
                    .on_change(move |value, _, cx| {
                        let value = *value;
                        update_settings(cx, |s| set(s, value));
                    }),
            )
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> Section {
        let current = settings(cx).theme;
        let group = ButtonGroup::new("theme-group")
            .outline()
            .small()
            .children(ThemeMode::ALL.into_iter().map(|mode| {
                Button::new(SharedString::from(format!("theme-{mode:?}")))
                    .label(mode.label())
                    .selected(mode == current)
            }))
            .on_click(|selected, _, cx| {
                if let Some(mode) = selected
                    .first()
                    .and_then(|ix| ThemeMode::ALL.get(*ix))
                    .copied()
                {
                    update_settings(cx, |s| s.theme = mode);
                }
            });
        Section::new("Appearance").child(
            ListCard::new().child(
                Row::new("theme", "Theme")
                    .icon("palette")
                    .subtitle(view::THEME_DESC)
                    .action(group),
            ),
        )
    }

    fn render_cleanup(&self, cx: &mut Context<Self>) -> Section {
        let s = settings(cx);
        let trash = s.move_to_trash;
        // Only macOS has a system notification hook so far (see
        // `nera_ui::system_notify`); don't offer a switch that does nothing.
        let notify_row = cfg!(target_os = "macos").then(|| {
            Self::switch_row(
                "notify-cleanup",
                "bell",
                "Notify after cleanup",
                view::NOTIFY_DESC,
                s.notify_after_cleanup,
                |s, v| s.notify_after_cleanup = v,
            )
        });
        Section::new("Cleanup")
            .description(view::cleanup_description(trash))
            .child(
                ListCard::new()
                    .child(Self::switch_row(
                        "move-to-trash",
                        "trash",
                        view::MOVE_TO_TRASH_TITLE,
                        view::MOVE_TO_TRASH_DESC,
                        trash,
                        |s, v| s.move_to_trash = v,
                    ))
                    .child(Self::switch_row(
                        "confirm-cleanup",
                        "shield-check",
                        "Confirm before cleanup",
                        view::CONFIRM_DESC,
                        s.confirm_before_cleanup,
                        |s, v| s.confirm_before_cleanup = v,
                    ))
                    .children(notify_row),
            )
    }

    fn render_scanning(&self, cx: &mut Context<Self>) -> Section {
        let s = settings(cx);
        let depth = view::depth(s);
        let stepper = h_flex()
            .gap(sp::SM)
            .items_center()
            .child(
                Button::new("depth-down")
                    .small()
                    .outline()
                    .icon(icon("minus"))
                    .disabled(depth <= engine::MIN_SCAN_DEPTH)
                    .tooltip("Decrease depth")
                    .on_click(|_, _, cx| {
                        update_settings(cx, |s| {
                            s.max_scan_depth = engine::step_depth(view::depth(s), -1)
                        })
                    }),
            )
            .child(
                div()
                    .min_w(dim::ICON_TILE)
                    .text_center()
                    .text_sm()
                    .font_medium()
                    .child(depth.to_string()),
            )
            .child(
                Button::new("depth-up")
                    .small()
                    .outline()
                    .icon(icon("plus"))
                    .disabled(depth >= engine::MAX_SCAN_DEPTH)
                    .tooltip("Increase depth")
                    .on_click(|_, _, cx| {
                        update_settings(cx, |s| {
                            s.max_scan_depth = engine::step_depth(view::depth(s), 1)
                        })
                    }),
            );
        Section::new("Scanning").child(
            ListCard::new()
                .child(Self::switch_row(
                    "scan-hidden",
                    "eye",
                    "Include hidden files",
                    view::HIDDEN_DESC,
                    s.scan_hidden_files,
                    |s, v| s.scan_hidden_files = v,
                ))
                .child(
                    Row::new("scan-depth", "Maximum scan depth")
                        .icon("layers")
                        .subtitle(view::DEPTH_DESC)
                        .action(stepper),
                )
                .child(Self::switch_row(
                    "scan-on-launch",
                    "scan-search",
                    "Scan on launch",
                    view::LAUNCH_DESC,
                    s.auto_scan_on_launch,
                    |s, v| s.auto_scan_on_launch = v,
                )),
        )
    }

    fn render_excluded(&self, cx: &mut Context<Self>) -> Section {
        let paths = settings(cx).excluded_paths.clone();
        let add = Button::new("add-folder")
            .small()
            .outline()
            .icon(icon("folder-plus"))
            .label("Add folder…")
            .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx)));
        let section = Section::new("Excluded folders")
            .description(view::EXCLUDED_DESC)
            .trailing(add);
        let section = match &self.notice {
            Some(n) => {
                section.child(Banner::new(n.tone, n.title.clone()).message(n.message.clone()))
            }
            None => section,
        };
        if paths.is_empty() {
            return section.child(
                EmptyState::new("folder-x", "No excluded folders")
                    .message("Add a folder you want Nera Cleaner to leave alone."),
            );
        }
        section.child(
            ListCard::new()
                .header(list_header("excluded", "Folder", None, None))
                .children(paths.into_iter().map(|path| {
                    let remove_path = path.clone();
                    let (title, subtitle) = view::excluded_row(&path);
                    Row::new(
                        SharedString::from(format!("excluded:{}", path.display())),
                        title,
                    )
                    .icon("folder")
                    .subtitle(subtitle)
                    .action(
                        Button::new(SharedString::from(format!("remove:{}", path.display())))
                            .small()
                            .ghost()
                            .icon(icon("x"))
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_folder(&remove_path, cx)
                            })),
                    )
                })),
        )
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> Section {
        let perm = view::permission(self.access.as_ref(), self.checking_access);
        let tone = match perm.tone {
            PermissionTone::Neutral => Tone::Neutral,
            PermissionTone::Success => Tone::Success,
            PermissionTone::Warning => Tone::Warning,
        };
        let badge = if tone == Tone::Neutral {
            Badge::new(perm.badge)
        } else {
            Badge::new(perm.badge).tone(tone)
        };
        let recheck = Button::new("recheck-access")
            .small()
            .ghost()
            .icon(icon("rotate-ccw"))
            .label("Check again")
            .disabled(self.checking_access)
            .on_click(cx.listener(|this, _, _, cx| this.check_access(cx)));
        let open = Button::new("open-fda")
            .small()
            .outline()
            .icon(icon("external-link"))
            .label("Open System Settings")
            .on_click(|_, _, _| open_access_settings());
        let icon_name = if tone == Tone::Warning {
            "shield-alert"
        } else {
            "shield-check"
        };
        let mut section = Section::new("Permissions");
        if self
            .access
            .as_ref()
            .is_some_and(|r| r.status == FullDiskAccess::Denied)
        {
            section = section.child(
                Banner::new(Tone::Warning, "Full Disk Access is off")
                    .message("Grant access in System Settings, then restart Nera Cleaner so macOS applies it.")
                    .action(
                        Button::new("open-fda-banner")
                            .small()
                            .outline()
                            .label("Open System Settings")
                            .on_click(|_, _, _| open_access_settings()),
                    ),
            );
        }
        section.child(
            ListCard::new().child(
                Row::new("full-disk-access", "Full Disk Access")
                    .icon(icon_name)
                    .icon_tone(tone)
                    .subtitle(perm.subtitle)
                    .badge(badge)
                    .action(recheck)
                    .action(open),
            ),
        )
    }

    fn render_storage(&self, cx: &mut Context<Self>) -> Section {
        let dir = engine::app_cache_dir();
        let size = view::storage_value(self.cache_size);
        let reveal_dir = dir.clone();
        let reveal = Button::new("reveal-cache")
            .small()
            .outline()
            .label(nera_core::os::SHOW_IN_FILE_MANAGER)
            .disabled(!dir.exists())
            .on_click(move |_, _, _| fs_util::reveal_in_finder(&reveal_dir));
        let refresh = Button::new("refresh-cache")
            .small()
            .ghost()
            .icon(icon("rotate-ccw"))
            .tooltip("Recalculate")
            .on_click(cx.listener(|this, _, _, cx| {
                this.cache_size = None;
                this.measure_cache(cx);
            }));
        let weak = cx.entity().downgrade();
        let clear = Button::new("clear-cache")
            .small()
            .outline()
            .label("Clear")
            .disabled(self.cache_size.is_none_or(|n| n == 0))
            .on_click(move |_, window, cx| {
                let weak = weak.clone();
                confirm(
                    Confirm::destructive(
                        "Clear Nera Cleaner's cache?",
                        "Only files Nera Cleaner keeps for itself are deleted. They are rebuilt when needed.",
                        "Clear cache",
                    ),
                    false,
                    window,
                    cx,
                    move |_, cx| {
                        let _ = weak.update(cx, |this, cx| this.clear_cache(cx));
                    },
                );
            });
        Section::new("Storage").child(
            ListCard::new().child(
                Row::new("app-cache", "Nera Cleaner cache")
                    .icon("database")
                    .subtitle(engine::display_path(&dir))
                    .value(size)
                    .action(refresh)
                    .action(clear)
                    .action(reveal),
            ),
        )
    }

    fn render_about(&self, cx: &mut Context<Self>) -> Section {
        Section::new("About").child(
            Card::new()
                .child(
                    h_flex()
                        .gap(sp::MD)
                        .items_center()
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(dim::ICON_TILE)
                                .rounded(cx.theme().radius)
                                .bg(Tone::Accent.subtle_bg(cx))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    icon(Screen::Cleanup.icon())
                                        .size(dim::ICON_MD)
                                        .text_color(Tone::Accent.color(cx)),
                                ),
                        )
                        .child(
                            v_stack()
                                .child(div().text_sm().font_semibold().child("Nera Cleaner"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(view::TAGLINE),
                                ),
                        ),
                )
                .child(kv_row("Version", view::version_text(), cx))
                .child(kv_row("Copyright", view::COPYRIGHT, cx)),
        )
    }

    fn reset_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = Confirm::destructive(
            "Reset all settings?",
            "Every preference returns to its default and the excluded folders list is cleared. Your files are not touched.",
            "Reset settings",
        );
        confirm(spec, false, window, cx, |window, cx| {
            update_settings(cx, |s| *s = Settings::default());
            notify(
                window,
                cx,
                Tone::Success,
                "Settings reset",
                "All preferences are back to their defaults.",
            );
        });
        self.notice = None;
    }
}

/// Open the Full Disk Access pane of System Settings (macOS only; the
/// Permissions section is not shown elsewhere).
fn open_access_settings() {
    #[cfg(target_os = "macos")]
    fs_util::open(engine::FULL_DISK_ACCESS_PANE);
}

fn v_stack() -> Div {
    gpui_kit::component::v_flex().gap(sp::XXS).min_w_0()
}

impl Render for SettingsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_default = *settings(cx) == Settings::default();
        let reset = Button::new("reset-settings")
            .outline()
            .icon(icon("rotate-ccw"))
            .label("Reset to defaults")
            .disabled(is_default)
            .on_click(cx.listener(|this, _, window, cx| this.reset_defaults(window, cx)));

        Page::new("settings-page", Screen::Settings.title())
            .subtitle("Preferences for scanning and cleanup")
            .action(reset)
            .child(self.render_appearance(cx))
            .child(self.render_cleanup(cx))
            .child(self.render_scanning(cx))
            .child(self.render_excluded(cx))
            .children(engine::HAS_FULL_DISK_ACCESS.then(|| self.render_permissions(cx)))
            .child(self.render_storage(cx))
            .child(self.render_about(cx))
    }
}
