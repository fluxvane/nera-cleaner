# Nera Cleaner — Architecture

Desktop cleaner for macOS and Windows written in Rust with GPUI (`gpui-kit` 0.9.1 = gpui-pre 0.3.12 + gpui-component 0.9.1).

```
crates/
  nera-core/            foundation: paths, format, fs_util (sizing + the ONLY delete path), safety, settings, disk,
                        os (platform wording); all OS-specific code sits in the private `sys` module
  nera-ui/              design system (Page, Section, Card, Row, ...), nav (Screen enum), state (settings global)
  nera-app/             binary: window bootstrap, sidebar shell, screen registry
  features/<name>/      crate `nera-<name>`: `engine` (FS/OS logic, unit-tested) + screen view(s)
```

Each feature crate depends only on `nera-core`, `nera-ui` and `gpui-kit` (plus any crates it adds to its
own `Cargo.toml`; shared versions are listed in `[workspace.dependencies]`). Features never depend on each
other, so `cargo build -p nera-<name>` builds one feature in isolation.

## Platforms

Every crate builds and runs on macOS and Windows.

- `nera-core::sys` has one file per platform (`macos.rs`, `windows.rs`) exporting the same functions. The public
  modules (`paths`, `fs_util`, `disk`, `safety`) wrap them, so feature code calls `nera_core` and stays free of
  `cfg`. Add a function to both files when a feature needs a new OS primitive.
- A feature crate keeps shared logic in `engine.rs` and puts OS code in sibling modules, one per platform,
  exporting the same names (for example `engine_macos.rs` / `engine_windows.rs`, or `engine/macos.rs` /
  `engine/windows.rs`), selected with `#[cfg(target_os = "macos")]` / `#[cfg(windows)]`. Apple-only crates
  (`core-foundation`, `io-kit-sys`, `mach2`) are declared under `[target.'cfg(target_os = "macos")'.dependencies]`
  and `windows-sys` under `[target.'cfg(windows)'.dependencies]`.
- Parts of a screen with no trustworthy equivalent on a platform are left out there (sensors and fans, Full Disk
  Access, load average, clone detection on Windows), never approximated.
- `nera_ui::nav::Screen::is_available` exists for a feature that ships on one platform first; today it is true
  for every screen. Screen titles may differ per platform ("Login Items" / "Startup Apps").
- The macOS code cannot be compiled on Windows (GPUI's Metal shader step needs Xcode) and vice versa, so CI on
  both platforms is the only proof a change works on the other one. Keep OS code in the per-platform modules so
  a change for one platform does not touch what the other compiles.
- User-facing words that differ per platform come from `nera_core::os` ("Trash" / "Recycle Bin", "Finder" /
  "File Explorer", "Mac" / "PC"): constants for formatted text, macros (`nera_core::trash_name!()`) inside
  `concat!` for string literals.
- Safety rules are per platform (`nera_core::safety`): on Windows they protect drive roots, `Windows` (except
  the contents of `Windows\Temp`), `Program Files`, other users' profiles, the user's own folders and
  `AppData` roots, virtual disks (`.vhdx`: Docker Desktop, WSL) and the same JetBrains / Android Studio state.

## Feature crates

| Crate | Screen(s) | Where the numbers come from |
|---|---|---|
| `nera-cleanup` | Dashboard, System Junk | Allocated sizes per category (APFS private size, hard links counted once); Finder's "available" disk space, read again before and after a cleanup; cloud-drive files stored on disk and Siri / Apple Intelligence data, measured like `du` and never removed |
| `nera-devscan` | Developer Junk | `node_modules`, build folders, etc. sized like `du`, with "frees at least" when data is shared; git worktrees that are merged, missing or abandoned (`engine/worktree.rs`: every check must pass, anything unverified is review-only, removal re-checks and never forces or deletes branches) |
| `nera-duplicates` | Duplicates | Size → partial hash → full BLAKE3 hash; clones and outside hard links are marked "Shares space" |
| `nera-space` | Space Lens | Folder sizes like `du -x` (mounted volumes skipped), compressed files sized like Finder |
| `nera-apps` | Applications | App bundles and leftovers; running state from `proc_pidpath` + `KERN_PROCARGS2` |
| `nera-processes` | Processes | Per-process `phys_footprint` and CPU over ≥1 s (Activity Monitor's definitions) |
| `nera-startup` | Login Items | Launch agents/daemons + `launchctl`; login items via System Events, loaded on demand |
| `nera-performance` | Performance | CPU/RAM/swap as Activity Monitor; GPU load from IOKit `IOAccelerator` `PerformanceStatistics` |
| `nera-battery` | Battery & Sensors | IOPowerSources / AppleSmartBattery, Apple's health % from `system_profiler`, SMC temperatures (per-chip core keys) and fans |
| `nera-privacy` | Privacy | Browser caches, cookies and history per browser profile |
| `nera-settings` | Settings | `nera_core::settings` + Full Disk Access probe |

Every crate has `examples/dump_<name>.rs`, which prints exactly the strings its screen shows, so values can be
compared with Apple's tools: `cargo run -p nera-<name> --example dump_<name>`.

Shared crates (`nera-core`, `nera-ui`, `nera-app`) hold anything two features need; features never depend on
each other.

## Hard rules

1. **Deleting files**: only through `nera_core::fs_util::{remove_item, trash_item, remove_or_trash}`. Never call `std::fs::remove_*`, `trash::delete` or `rm` yourself. These consult `nera_core::safety`, which protects system roots, user folders, Nera's own caches and live IDE state (JetBrains / Android Studio system dirs, Toolbox).
   - One exception: the Windows Recycle Bin is emptied through the shell (`SHEmptyRecycleBinW`), the only supported way.
   - Respect the user's preference: `nera_ui::state::settings(cx).move_to_trash` decides trash vs delete for cleanups.
   - Destructive actions in the UI must go through `nera_ui::confirm(...)`.
2. **No blocking on the UI thread.** Wrap any file-system / process work in `nera_ui::run_blocking(cx, work, done)`; show progress with `Shared<T>` + `ProgressPanel`. Keep the returned `Task` in a field (`_task: Option<Task<()>>`).
3. **Layout only from `nera_ui`** (see `docs/DESIGN.md`): one `Page` per screen, `Section`s inside, content in `Card` / `ListCard` + `Row` / `StatGrid` + `StatTile`. Spacing only from `ui::sp`, sizes from `ui::dim`, colours only from `cx.theme()` / `ui::Tone`. No raw hex colours, no ad-hoc `px()` gaps.
4. **Never invent an API.** gpui-kit docs online are newer than 0.9.1. Verify signatures in `~/.cargo/registry/src/index.crates.io-*/gpui-component-0.9.1/src`, `gpui-base-0.9.1/src`, `gpui-pre-0.3.12/src`. The guides at https://gpui-kit.com/docs explain concepts.
5. **Stable ids.** Rows use domain ids (`path`, `bundle_id`, `pid`), never list indexes.
6. **Tests.** Every core module has `#[cfg(test)]` tests that run without touching real user data (use temp dirs). `cargo test -p nera-<name>` must pass.
7. **Navigation** between screens: `nera_ui::nav::navigate(Screen::X, cx)`.
8. Copy is sentence case, short, specific. File and disk sizes via `nera_core::format::bytes` (decimal, like Finder); RAM via `nera_core::format::memory_bytes` (binary, like Activity Monitor).
9. **Accuracy.** Show a value only when it matches an independent source on that platform (macOS: Finder, Activity Monitor, `pmset`, `system_profiler`, `du`...; Windows: File Explorer, Task Manager). If it cannot be verified, label it (e.g. the "Unverified" badge on SMC temperatures) or hide it; never show a guess as fact.

## Async pattern

```rust
pub struct MyScreen {
    progress: Shared<Option<String>>,
    result: Option<Vec<Item>>,
    cancel: nera_core::fs_util::Cancel,
    _task: Option<Task<()>>,
}

fn start(&mut self, cx: &mut Context<Self>) {
    let progress = self.progress.clone();
    let cancel = nera_core::fs_util::Cancel::new();
    self.cancel = cancel.clone();
    self._task = Some(run_blocking(
        cx,
        move || crate::engine::scan(&cancel, |p| progress.set(Some(p))),
        |this, result, _cx| { this.result = Some(result); this._task = None; },
    ));
}
```

Event handlers: `cx.listener(|this, _ev, window, cx| { ... })` on buttons; inside `on_click` closures that need the window use `window`.

## Build & run

```
cargo test -p nera-core -p nera-<name>
cargo build -p nera-<name>      # one feature in isolation
cargo build -p nera-app         # whole app
cargo run -p nera-app -- --screen Cleanup   # open a specific screen
scripts/bundle.sh [--release] [--zip]       # macOS: target/<profile>/Nera Cleaner.app (+ dist/*.zip)
scripts/package.ps1 [-Debug]                # Windows: dist/NeraCleaner-<version>-windows-<arch>.zip
```

On Windows `crates/nera-app/build.rs` embeds the icon and version info
(`crates/nera-app/resources/windows/`) into `nera-cleaner.exe` (GPUI supplies the manifest); release builds have no console window.
`.github/workflows/ci.yml` runs fmt, clippy, tests and packaging on both platforms.
