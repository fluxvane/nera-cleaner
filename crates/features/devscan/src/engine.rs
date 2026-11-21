//! Developer junk engine: finds projects under chosen folders and the
//! regenerable build outputs / dependency folders inside them, measures them,
//! and removes the ones the user picks (through `nera_core::fs_util` only).
//!
//! Port of the Swift `DevJunkRules` + `SmartDeepScannerActor` behaviour with a
//! single-pass walk:
//! - a folder holding an ecosystem marker (`Cargo.toml`, `package.json`, ...)
//!   starts a project; nested markers inside it widen the active rule set;
//!   a nested git repository with markers starts its own project;
//! - a child folder whose name matches an active rule is junk: it is recorded
//!   and never descended into;
//! - hidden folders, symlinks, VCS internals, `Library` and app/document
//!   bundles are never descended into;
//! - folder names that are also used for real source (`build`, `bin`, `vendor`,
//!   `target`, ...) are offered only when git does not track anything in them,
//!   or, outside a repository, when the folder looks like tool output.
//!
//! Sizing and locating git differ per platform: `engine_macos.rs` and
//! `engine_windows.rs` export the same functions.

use nera_core::format;
use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "macos")]
#[path = "engine_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "engine_windows.rs"]
mod platform;

pub use platform::measure;
#[cfg(target_os = "macos")]
pub use platform::private_size;
use platform::{git_binary, is_hidden, no_window};

pub mod worktree;

// MARK: - Ecosystems

/// Technology stack of a project. Declaration order is detection priority
/// (most specific first), so `Ord` picks the primary ecosystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ecosystem {
    NextJs,
    Angular,
    Nuxt,
    SvelteKit,
    DotNet,
    Flutter,
    Rust,
    Go,
    Xcode,
    CocoaPods,
    Java,
    Elixir,
    Zig,
    Php,
    Python,
    Ruby,
    Node,
}

impl Ecosystem {
    pub fn label(self) -> &'static str {
        match self {
            Ecosystem::NextJs => "Next.js",
            Ecosystem::Angular => "Angular",
            Ecosystem::Nuxt => "Nuxt",
            Ecosystem::SvelteKit => "SvelteKit",
            Ecosystem::DotNet => ".NET",
            Ecosystem::Flutter => "Flutter",
            Ecosystem::Rust => "Rust",
            Ecosystem::Go => "Go",
            Ecosystem::Xcode => "Xcode",
            Ecosystem::CocoaPods => "CocoaPods",
            Ecosystem::Java => "Java",
            Ecosystem::Elixir => "Elixir",
            Ecosystem::Zig => "Zig",
            Ecosystem::Php => "PHP",
            Ecosystem::Python => "Python",
            Ecosystem::Ruby => "Ruby",
            Ecosystem::Node => "Node.js",
        }
    }

    /// Lucide icon name.
    pub fn icon(self) -> &'static str {
        match self {
            Ecosystem::NextJs => "triangle",
            Ecosystem::Angular => "shield",
            Ecosystem::Nuxt | Ecosystem::SvelteKit => "layers",
            Ecosystem::DotNet => "braces",
            Ecosystem::Flutter => "bird",
            Ecosystem::Rust => "cog",
            Ecosystem::Go => "rabbit",
            Ecosystem::Xcode => "hammer",
            Ecosystem::CocoaPods => "package",
            Ecosystem::Java => "coffee",
            Ecosystem::Elixir => "droplet",
            Ecosystem::Zig => "zap",
            Ecosystem::Php => "file-code",
            Ecosystem::Python => "code-xml",
            Ecosystem::Ruby => "gem",
            Ecosystem::Node => "hexagon",
        }
    }
}

// MARK: - Rules

#[derive(Clone, Copy, Debug)]
enum Pattern {
    /// Exact folder name.
    Name(&'static str),
    /// Folder name ending with this suffix (e.g. `.egg-info`).
    Suffix(&'static str),
    /// Folder `child` directly inside a folder named `parent` (`vendor/bundle`).
    Nested(&'static str, &'static str),
}

/// How much the name alone proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trust {
    /// Only ever tool output (`node_modules`, `.next`, `__pycache__`).
    Always,
    /// Also a common source folder name (`build`, `bin`, `vendor`): offered
    /// only when git tracks nothing inside, or (outside git) with evidence.
    Ambiguous,
    /// Must always show tool fingerprints (`venv` needs `pyvenv.cfg`).
    Evidence,
}

#[derive(Clone, Copy, Debug)]
struct JunkSpec {
    pattern: Pattern,
    trust: Trust,
}

const fn always(name: &'static str) -> JunkSpec {
    JunkSpec {
        pattern: Pattern::Name(name),
        trust: Trust::Always,
    }
}
const fn ambiguous(name: &'static str) -> JunkSpec {
    JunkSpec {
        pattern: Pattern::Name(name),
        trust: Trust::Ambiguous,
    }
}
const fn evidence(name: &'static str) -> JunkSpec {
    JunkSpec {
        pattern: Pattern::Name(name),
        trust: Trust::Evidence,
    }
}

impl JunkSpec {
    fn matches(&self, name: &str, parent_name: &str) -> bool {
        match self.pattern {
            Pattern::Name(n) => name == n,
            Pattern::Suffix(s) => name.len() > s.len() && name.ends_with(s),
            Pattern::Nested(p, c) => name == c && parent_name == p,
        }
    }
}

struct Rule {
    ecosystem: Ecosystem,
    /// File or folder names at a project root; `*.ext` matches a suffix.
    markers: &'static [&'static str],
    junk: &'static [JunkSpec],
}

impl Rule {
    fn matches_marker(&self, file_name: &str) -> bool {
        self.markers.iter().any(|m| match m.strip_prefix('*') {
            Some(suffix) => file_name.len() > suffix.len() && file_name.ends_with(suffix),
            None => file_name == *m,
        })
    }
}

/// Every rule, in detection priority order (matches `Ecosystem` order).
static RULES: &[Rule] = &[
    Rule {
        ecosystem: Ecosystem::NextJs,
        markers: &[
            "next.config.js",
            "next.config.mjs",
            "next.config.cjs",
            "next.config.ts",
        ],
        junk: &[
            always(".next"),
            always(".turbo"),
            always("node_modules"),
            ambiguous("out"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Angular,
        markers: &["angular.json", ".angular-cli.json"],
        junk: &[
            always(".angular"),
            always("node_modules"),
            ambiguous("dist"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Nuxt,
        markers: &["nuxt.config.ts", "nuxt.config.js", "nuxt.config.mjs"],
        junk: &[always(".nuxt"), always(".output"), always("node_modules")],
    },
    Rule {
        ecosystem: Ecosystem::SvelteKit,
        markers: &["svelte.config.js", "svelte.config.ts"],
        junk: &[always(".svelte-kit"), always("node_modules")],
    },
    Rule {
        ecosystem: Ecosystem::DotNet,
        markers: &[
            "*.sln",
            "*.slnx",
            "*.csproj",
            "*.fsproj",
            "*.vbproj",
            "Directory.Build.props",
            "global.json",
        ],
        junk: &[
            always(".vs"),
            always("BenchmarkDotNet.Artifacts"),
            ambiguous("bin"),
            ambiguous("obj"),
            ambiguous("TestResults"),
            ambiguous("packages"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Flutter,
        markers: &["pubspec.yaml"],
        junk: &[always(".dart_tool"), ambiguous("build")],
    },
    Rule {
        ecosystem: Ecosystem::Rust,
        markers: &["Cargo.toml"],
        junk: &[ambiguous("target")],
    },
    Rule {
        ecosystem: Ecosystem::Go,
        markers: &["go.mod"],
        junk: &[ambiguous("vendor")],
    },
    Rule {
        ecosystem: Ecosystem::Xcode,
        markers: &["*.xcodeproj", "*.xcworkspace", "Package.swift"],
        junk: &[
            always("DerivedData"),
            always("Index.noindex"),
            always(".build"),
            ambiguous("Build"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::CocoaPods,
        markers: &["Podfile"],
        junk: &[ambiguous("Pods")],
    },
    Rule {
        ecosystem: Ecosystem::Java,
        markers: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "pom.xml",
        ],
        junk: &[
            always(".gradle"),
            always(".kotlin"),
            ambiguous("build"),
            ambiguous("target"),
            ambiguous("out"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Elixir,
        markers: &["mix.exs"],
        junk: &[always("_build"), ambiguous("deps")],
    },
    Rule {
        ecosystem: Ecosystem::Zig,
        markers: &["build.zig"],
        junk: &[
            always(".zig-cache"),
            always("zig-cache"),
            ambiguous("zig-out"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Php,
        markers: &["composer.json"],
        junk: &[ambiguous("vendor")],
    },
    Rule {
        ecosystem: Ecosystem::Python,
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "tox.ini",
        ],
        junk: &[
            always("__pycache__"),
            always(".tox"),
            always(".nox"),
            always(".mypy_cache"),
            always(".pytest_cache"),
            always(".ruff_cache"),
            always(".eggs"),
            JunkSpec {
                pattern: Pattern::Suffix(".egg-info"),
                trust: Trust::Always,
            },
            evidence(".venv"),
            evidence("venv"),
            ambiguous("dist"),
            ambiguous("build"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Ruby,
        markers: &["Gemfile"],
        junk: &[
            JunkSpec {
                pattern: Pattern::Nested("vendor", "bundle"),
                trust: Trust::Ambiguous,
            },
            ambiguous("tmp"),
        ],
    },
    Rule {
        ecosystem: Ecosystem::Node,
        markers: &["package.json"],
        junk: &[
            always("node_modules"),
            always(".parcel-cache"),
            always(".turbo"),
            always(".pnpm-store"),
            ambiguous(".cache"),
        ],
    },
];

/// Never descended into while looking for projects.
const SKIP_NAMES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".Trash",
    ".Spotlight-V100",
    ".fseventsd",
    "Library",
    "site-packages",
];

/// Folder names that must never be offered, whatever a rule says.
const PROTECTED_NAMES: &[&str] = &[
    "src",
    "lib",
    "app",
    "pages",
    "components",
    "modules",
    "assets",
    "resources",
    "public",
    "static",
    "docs",
    "test",
    "tests",
    "spec",
    "specs",
    "__tests__",
    ".git",
    ".github",
    ".vscode",
    ".idea",
    ".config",
    ".ssh",
];

/// Package-style folders (apps, libraries, Xcode containers): never walked.
const BUNDLE_EXTENSIONS: &[&str] = &[
    "app",
    "appex",
    "framework",
    "bundle",
    "plugin",
    "kext",
    "pkg",
    "xcodeproj",
    "xcworkspace",
    "xcassets",
    "xcarchive",
    "playground",
    "dSYM",
    "photoslibrary",
    "photolibrary",
    "musiclibrary",
    "logicx",
    "band",
    "fcpbundle",
    "imovielibrary",
    "tvlibrary",
    "sparsebundle",
];

/// Lock files and manifests whose modification date says when a project was last worked on.
const ACTIVITY_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "packages.lock.json",
    "Cargo.lock",
    "Pipfile.lock",
    "poetry.lock",
    "uv.lock",
    "pubspec.lock",
    "Gemfile.lock",
    "go.sum",
    "Podfile.lock",
    "composer.lock",
    "mix.lock",
    "Package.resolved",
];

/// Maximum folder depth below a scan root.
pub const MAX_DEPTH: usize = 30;

/// Projects untouched for this long are flagged as inactive.
pub const INACTIVE_DAYS: u64 = 90;

const MAX_UNREADABLE: usize = 100;

fn rule_mask_for_markers<'a>(names: impl Iterator<Item = &'a str>) -> u32 {
    let mut mask = 0u32;
    for name in names {
        for (i, rule) in RULES.iter().enumerate() {
            if rule_on_this_platform(rule) && rule.matches_marker(name) {
                mask |= 1 << i;
            }
        }
    }
    mask
}

/// Xcode and CocoaPods output can only be rebuilt on a Mac, so on Windows
/// those projects are not offered.
fn rule_on_this_platform(rule: &Rule) -> bool {
    cfg!(target_os = "macos") || !matches!(rule.ecosystem, Ecosystem::Xcode | Ecosystem::CocoaPods)
}

fn ecosystems_of(mask: u32) -> Vec<Ecosystem> {
    RULES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, r)| r.ecosystem)
        .collect()
}

/// The first active rule (priority order) whose junk list matches `name`.
fn match_junk(mask: u32, name: &str, parent_name: &str) -> Option<(Ecosystem, Trust)> {
    let (ecosystem, trust) = RULES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .find_map(|(_, rule)| {
            rule.junk
                .iter()
                .find(|s| s.matches(name, parent_name))
                .map(|s| (rule.ecosystem, s.trust))
        })?;
    // Dependency folders belong to Node whichever framework sits on top.
    let node_rule = RULES.iter().position(|r| r.ecosystem == Ecosystem::Node)?;
    let ecosystem = if matches!(name, "node_modules" | ".turbo") && mask & (1 << node_rule) != 0 {
        Ecosystem::Node
    } else {
        ecosystem
    };
    Some((ecosystem, trust))
}

/// Names that are tool output in every ecosystem: never worth walking into,
/// even outside a project (e.g. a stray `node_modules`).
fn is_unambiguous_junk_name(name: &str) -> bool {
    RULES
        .iter()
        .flat_map(|r| r.junk.iter())
        .any(|s| s.trust == Trust::Always && matches!(s.pattern, Pattern::Name(n) if n == name))
}

fn is_bundle(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            BUNDLE_EXTENSIONS
                .iter()
                .any(|b| b.eq_ignore_ascii_case(ext))
        })
}

// MARK: - Candidate validation

/// Fingerprints that a folder was produced by a tool, for names that are
/// also used for hand-written content.
fn has_tool_evidence(path: &Path, name: &str) -> bool {
    let has = |rel: &str| std::fs::symlink_metadata(path.join(rel)).is_ok();
    let any = |rels: &[&str]| rels.iter().any(|r| has(r));
    match name {
        ".venv" | "venv" => has("pyvenv.cfg"),
        "bin" => any(&["Debug", "Release"]) || dir_has_extension(path, &["dll", "pdb"]),
        "obj" => any(&["project.assets.json", "Debug", "Release"]),
        "target" => any(&[
            "CACHEDIR.TAG",
            ".rustc_info.json",
            "classes",
            "maven-status",
            "generated-sources",
        ]),
        "build" | "Build" => any(&[
            "intermediates",
            "tmp",
            "classes",
            "generated",
            "kotlin",
            "outputs",
            "Products",
            "Intermediates.noindex",
            "flutter_assets",
            ".last_build_id",
            "XCBuildData",
        ]),
        "vendor" => any(&["modules.txt", "autoload.php"]),
        "bundle" => has("ruby"),
        "Pods" => has("Manifest.lock"),
        "packages" => subdirs_have_extension(path, "nupkg"),
        "TestResults" => dir_has_extension(path, &["trx"]),
        "deps" => subdirs_have_file(path, "mix.exs"),
        _ => false,
    }
}

fn dir_has_extension(dir: &Path, exts: &[&str]) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().take(500).any(|e| {
        Path::new(&e.file_name())
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| exts.contains(&x))
    })
}

fn subdirs_have_extension(dir: &Path, ext: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .take(20)
        .any(|e| dir_has_extension(&e.path(), &[ext]))
}

fn subdirs_have_file(dir: &Path, file: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .take(20)
        .any(|e| e.path().join(file).exists())
}

/// Run git read-only in `path`'s parent. `Some(exit code)`; `None` when git
/// is missing or could not be started.
fn git_status_of(path: &Path, args: &[&str]) -> Option<i32> {
    let git = git_binary()?;
    let parent = path.parent()?;
    let name = path.file_name()?;
    let mut cmd = Command::new(git);
    no_window(&mut cmd);
    cmd.arg("-C")
        .arg(parent)
        .args(args)
        .arg("--")
        .arg(name)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .code()
}

/// `Some(true)` when git tracks at least one file inside `path`; `None` when
/// git could not answer (no git, not a repository, error).
fn git_tracks(path: &Path) -> Option<bool> {
    let git = git_binary()?;
    let parent = path.parent()?;
    let name = path.file_name()?;
    let mut cmd = Command::new(git);
    no_window(&mut cmd);
    let mut child = cmd
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(parent)
        .args(["ls-files", "-z", "--"])
        .arg(name)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut byte = [0u8; 1];
    // Read before waiting so a long listing cannot fill the pipe and stall.
    let read = child
        .stdout
        .take()
        .map(|mut out| out.read(&mut byte).unwrap_or(0))
        .unwrap_or(0);
    if read > 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Some(true);
    }
    let status = child.wait().ok()?;
    status.success().then_some(false)
}

/// `Some(true)` when `.gitignore` (or git's global/exclude files) ignores
/// `path`; `None` when git could not answer.
fn git_ignored(path: &Path) -> Option<bool> {
    match git_status_of(path, &["check-ignore", "-q"])? {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// Final decision for a folder whose name matched an active rule.
///
/// Ambiguous names (`build`, `target`, `vendor`, ...) are offered only when
/// the evidence says "generated": git tracks nothing inside AND ignores the
/// folder (a new, not yet committed source folder is neither), or the folder
/// carries the tool's own fingerprints.
fn accept_candidate(path: &Path, name: &str, trust: Trust, in_repo: bool) -> bool {
    if PROTECTED_NAMES.contains(&name) {
        return false;
    }
    match trust {
        Trust::Always => true,
        Trust::Evidence => has_tool_evidence(path, name),
        Trust::Ambiguous => {
            let tracked = if in_repo { git_tracks(path) } else { None };
            match tracked {
                Some(true) => false,
                Some(false) => git_ignored(path) == Some(true) || has_tool_evidence(path, name),
                None => has_tool_evidence(path, name),
            }
        }
    }
}

// MARK: - Results

/// One junk folder inside a project.
#[derive(Clone, Debug, PartialEq)]
pub struct JunkFolder {
    pub path: PathBuf,
    /// Path relative to the project root, e.g. `packages/web/node_modules`.
    pub relative: String,
    pub ecosystem: Ecosystem,
    /// Space the folder occupies on disk (allocated blocks, what `du` reports).
    pub size: u64,
    /// Part of `size` that only this folder's files hold: deleting the folder
    /// frees at least this much. Less than `size` when files share data with
    /// APFS clones elsewhere (pnpm, .NET build output, Finder copies).
    pub unique: u64,
    pub modified: Option<SystemTime>,
}

impl JunkFolder {
    /// True when a noticeable part of the folder's data is shared with files
    /// outside it (or with other clones), so deleting it frees less than `size`.
    pub fn is_shared(&self) -> bool {
        is_shared(self.size, self.unique)
    }
}

/// Part of `size` is not held by these files alone, by more than the rounding
/// of the displayed sizes: only then does "frees at least" differ from the size shown.
pub fn is_shared(size: u64, unique: u64) -> bool {
    unique < size && format::bytes(size) != format::bytes(unique)
}

/// A project root and its junk folders (largest first).
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    /// Detected stacks, primary first.
    pub ecosystems: Vec<Ecosystem>,
    /// Newest change to a manifest, lock file, the git index or any visible
    /// file outside hidden, build and dependency folders.
    pub last_activity: Option<SystemTime>,
    pub junk: Vec<JunkFolder>,
}

impl Project {
    pub fn size(&self) -> u64 {
        self.junk.iter().map(|j| j.size).sum()
    }

    pub fn unique(&self) -> u64 {
        self.junk.iter().map(|j| j.unique).sum()
    }

    pub fn primary(&self) -> Ecosystem {
        self.ecosystems.first().copied().unwrap_or(Ecosystem::Node)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Discovering,
    Measuring,
    /// Checking the git worktrees of the repositories found.
    Worktrees,
}

/// Live progress, written from the scan thread.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub phase: Phase,
    pub folders_scanned: usize,
    pub projects_found: usize,
    pub current: String,
    pub measured: usize,
    pub to_measure: usize,
}

impl Progress {
    pub fn fraction(&self) -> Option<f64> {
        (matches!(self.phase, Phase::Measuring | Phase::Worktrees) && self.to_measure > 0)
            .then(|| self.measured as f64 / self.to_measure as f64)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// Subtrees never scanned or offered (user's excluded folders).
    pub excluded: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct ScanOutcome {
    /// Sorted by reclaimable size, largest first.
    pub projects: Vec<Project>,
    pub folders_scanned: usize,
    /// Folders that could not be read (first few), and how many in total.
    pub unreadable: Vec<PathBuf>,
    pub unreadable_count: usize,
    /// True when at least one folder was refused by macOS privacy (EPERM).
    pub privacy_denied: bool,
    pub missing_roots: Vec<PathBuf>,
    /// Git worktrees that are merged, gone or abandoned, per repository.
    pub worktrees: Vec<worktree::RepoWorktrees>,
    /// Repositories have linked worktrees but git could not be run.
    pub worktrees_git_missing: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
}

impl ScanOutcome {
    pub fn total_size(&self) -> u64 {
        self.projects.iter().map(Project::size).sum()
    }

    pub fn total_unique(&self) -> u64 {
        self.projects.iter().map(Project::unique).sum()
    }

    pub fn junk_count(&self) -> usize {
        self.projects.iter().map(|p| p.junk.len()).sum()
    }
}

// MARK: - Scan

struct Candidate {
    project: usize,
    path: PathBuf,
    ecosystem: Ecosystem,
}

struct Frame {
    dir: PathBuf,
    depth: usize,
    project: Option<usize>,
    active: u32,
    in_repo: bool,
}

fn inside_git_repo(dir: &Path) -> bool {
    dir.ancestors().any(|a| a.join(".git").exists())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(path).ok()?.modified().ok()
}

/// Walk `opts.roots`, find projects and their junk, then measure the junk.
/// `report` is called from worker threads with live progress. Cancelling
/// returns whatever was fully measured so far with `cancelled = true`.
pub fn scan(
    opts: &ScanOptions,
    cancel: &Cancel,
    report: &(dyn Fn(&Progress) + Sync),
) -> ScanOutcome {
    let started = Instant::now();
    let mut out = ScanOutcome::default();
    let excluded: Vec<PathBuf> = opts.excluded.iter().map(paths::standardize).collect();
    let mut projects: Vec<Project> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut progress = Progress::default();

    for root in normalize_roots(&opts.roots) {
        if cancel.is_cancelled() {
            break;
        }
        if !root.is_dir() {
            out.missing_roots.push(root);
            continue;
        }
        let in_repo = inside_git_repo(&root);
        let mut stack = vec![Frame {
            dir: root,
            depth: 0,
            project: None,
            active: 0,
            in_repo,
        }];
        while let Some(frame) = stack.pop() {
            if cancel.is_cancelled() {
                break;
            }
            if walk_dir(
                frame,
                &excluded,
                &mut projects,
                &mut candidates,
                &mut stack,
                &mut out,
            ) {
                out.folders_scanned += 1;
            }
            if out.folders_scanned % 64 == 0 {
                progress.folders_scanned = out.folders_scanned;
                progress.projects_found = projects.len();
                if let Some(top) = stack.last() {
                    progress.current = display_path(&top.dir);
                }
                report(&progress);
            }
        }
    }

    // Measure every candidate in parallel.
    let total = candidates.len();
    progress = Progress {
        phase: Phase::Measuring,
        folders_scanned: out.folders_scanned,
        projects_found: projects.len(),
        current: String::new(),
        measured: 0,
        to_measure: total,
    };
    report(&progress);
    let done = AtomicUsize::new(0);
    let measured: Vec<Option<(usize, JunkFolder)>> = if cancel.is_cancelled() {
        Vec::new()
    } else {
        candidates
            .into_par_iter()
            .map(|c| {
                if cancel.is_cancelled() {
                    return None;
                }
                let Measured { size, unique } = measure(&c.path, cancel);
                if cancel.is_cancelled() {
                    return None;
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                report(&Progress {
                    phase: Phase::Measuring,
                    folders_scanned: progress.folders_scanned,
                    projects_found: progress.projects_found,
                    current: display_path(&c.path),
                    measured: n,
                    to_measure: total,
                });
                let root = &projects[c.project].root;
                let relative = c
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&c.path)
                    .to_string_lossy()
                    .into_owned();
                let modified = mtime(&c.path);
                Some((
                    c.project,
                    JunkFolder {
                        path: c.path,
                        relative,
                        ecosystem: c.ecosystem,
                        size,
                        unique,
                        modified,
                    },
                ))
            })
            .collect()
    };

    for (idx, junk) in measured.into_iter().flatten() {
        if junk.size > 0 {
            projects[idx].junk.push(junk);
        }
    }
    projects.retain(|p| !p.junk.is_empty());
    for p in &mut projects {
        p.junk.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.relative.cmp(&b.relative))
        });
    }
    projects.sort_by(|a, b| {
        b.size()
            .cmp(&a.size())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let (out_folders, out_projects) = (out.folders_scanned, projects.len());
    out.projects = projects;
    if !cancel.is_cancelled() {
        let roots = normalize_roots(&opts.roots);
        let repos = worktree::find_repos(&roots, &excluded, cancel);
        let found = worktree::scan(&repos, &roots, &excluded, cancel, &|done, total| {
            report(&Progress {
                phase: Phase::Worktrees,
                folders_scanned: out_folders,
                projects_found: out_projects,
                current: String::new(),
                measured: done,
                to_measure: total,
            });
        });
        out.worktrees = found.repos;
        out.worktrees_git_missing = found.git_missing;
    }
    out.cancelled = cancel.is_cancelled();
    out.elapsed = started.elapsed();
    out
}

fn walk_dir(
    frame: Frame,
    excluded: &[PathBuf],
    projects: &mut Vec<Project>,
    candidates: &mut Vec<Candidate>,
    stack: &mut Vec<Frame>,
    out: &mut ScanOutcome,
) -> bool {
    let entries = match std::fs::read_dir(&frame.dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::PermissionDenied {
                out.unreadable_count += 1;
                if cfg!(target_os = "macos") && err.raw_os_error() == Some(libc_eperm()) {
                    out.privacy_denied = true;
                }
                if out.unreadable.len() < MAX_UNREADABLE {
                    out.unreadable.push(frame.dir.clone());
                }
            }
            return false;
        }
    };

    // (name, is_dir) of real children; symlinks are dropped here.
    let mut children: Vec<(String, bool)> = Vec::new();
    // Visible files here: their newest change is the project's activity.
    let mut files: Vec<PathBuf> = Vec::new();
    // Folders hidden by attribute (Windows only): never walked into.
    let mut hidden: HashSet<String> = HashSet::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let hidden_attr = is_hidden(&entry);
        if ft.is_file() && !name.starts_with('.') && !hidden_attr {
            files.push(entry.path());
        }
        if ft.is_dir() && hidden_attr {
            hidden.insert(name.clone());
        }
        children.push((name, ft.is_dir()));
    }

    // A virtualenv is a tool-managed tree (its `site-packages` holds other
    // people's source folders named `build`, `lib`, ...): never look inside.
    if children
        .iter()
        .any(|(n, is_dir)| !is_dir && n == "pyvenv.cfg")
    {
        return true;
    }

    let mask = rule_mask_for_markers(children.iter().map(|(n, _)| n.as_str()));
    let has_git = children.iter().any(|(n, _)| n == ".git");
    let in_repo = frame.in_repo || has_git;
    let (project, active) = if mask != 0 && (frame.project.is_none() || has_git) {
        projects.push(Project {
            root: frame.dir.clone(),
            name: frame
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| display_path(&frame.dir)),
            ecosystems: ecosystems_of(mask),
            last_activity: project_activity(&frame.dir, &children),
            junk: Vec::new(),
        });
        (Some(projects.len() - 1), mask)
    } else {
        (frame.project, frame.active | mask)
    };
    if let Some(idx) = project {
        let newest = files.iter().filter_map(|f| mtime(f)).max();
        projects[idx].last_activity = projects[idx].last_activity.max(newest);
    }

    let parent_name = frame
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (name, is_dir) in children {
        if !is_dir || SKIP_NAMES.contains(&name.as_str()) {
            continue;
        }
        let path = frame.dir.join(&name);
        if !excluded.is_empty() && paths::is_under_any(&path, excluded) {
            continue;
        }
        if let (Some(idx), Some((ecosystem, trust))) =
            (project, match_junk(active, &name, &parent_name))
        {
            let holds_excluded = excluded.iter().any(|e| paths::is_under(e, &path));
            if !holds_excluded && accept_candidate(&path, &name, trust, in_repo) {
                candidates.push(Candidate {
                    project: idx,
                    path,
                    ecosystem,
                });
                continue;
            }
        }
        if name.starts_with('.')
            || hidden.contains(&name)
            || is_unambiguous_junk_name(&name)
            || is_bundle(&name)
        {
            continue;
        }
        if frame.depth + 1 < MAX_DEPTH {
            stack.push(Frame {
                dir: path,
                depth: frame.depth + 1,
                project,
                active,
                in_repo,
            });
        }
    }
    true
}

const fn libc_eperm() -> i32 {
    1 // EPERM on Darwin: refused by TCC (Full Disk Access / Files and Folders)
}

fn project_activity(root: &Path, children: &[(String, bool)]) -> Option<SystemTime> {
    let mut latest: Option<SystemTime> = None;
    for (name, _) in children {
        let relevant =
            ACTIVITY_FILES.contains(&name.as_str()) || RULES.iter().any(|r| r.matches_marker(name));
        if relevant {
            latest = latest.max(mtime(&root.join(name)));
        }
    }
    latest.max(mtime(&root.join(".git/index")))
}

// MARK: - Measuring

/// What a folder occupies: `size` (like `du`) and the part of it that deleting
/// the folder is guaranteed to free (`unique`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measured {
    pub size: u64,
    pub unique: u64,
}

// MARK: - Clean

#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    /// Removed (or already gone) folders.
    pub removed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

/// Remove `paths` one by one, to the Trash when `to_trash`. Only real
/// folders (not symlinks, not files) are touched, and only through
/// `fs_util::remove_or_trash`, which applies `nera_core::safety`.
pub fn clean(
    paths: &[PathBuf],
    to_trash: bool,
    cancel: &Cancel,
    report: &(dyn Fn(usize, usize) + Sync),
) -> CleanReport {
    let mut rep = CleanReport::default();
    let total = paths.len();
    for (i, path) in paths.iter().enumerate() {
        if cancel.is_cancelled() {
            rep.cancelled = true;
            break;
        }
        report(i, total);
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => rep.removed.push(path.clone()),
            Err(e) => rep.failed.push((path.clone(), e.to_string())),
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => rep
                .failed
                .push((path.clone(), "No longer a folder; left untouched".into())),
            Ok(_) => match fs_util::remove_or_trash(path, to_trash) {
                Ok(()) => rep.removed.push(path.clone()),
                Err(e) => rep.failed.push((path.clone(), e.to_string())),
            },
        }
    }
    report(rep.removed.len() + rep.failed.len(), total);
    rep
}

/// Drop removed folders from the results; projects left empty disappear.
pub fn apply_removed(projects: &mut Vec<Project>, removed: &[PathBuf]) {
    let gone: HashSet<&PathBuf> = removed.iter().collect();
    for p in projects.iter_mut() {
        p.junk.retain(|j| !gone.contains(&j.path));
    }
    projects.retain(|p| !p.junk.is_empty());
    projects.sort_by(|a, b| {
        b.size()
            .cmp(&a.size())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

// MARK: - Scan roots

/// Usual places developers keep code, relative to home.
#[cfg(target_os = "macos")]
pub const DEFAULT_ROOT_CANDIDATES: &[&str] = &[
    "Developer",
    "Projects",
    "workspaces",
    "workspace",
    "code",
    "src",
    "repos",
    "dev",
    "git",
    "GitHub",
    "Sites",
    "Documents/GitHub",
    "Documents/Projects",
    "Documents/Developer",
];

/// Usual places developers keep code, relative to the user profile
/// (`source\repos` is where Visual Studio creates projects).
#[cfg(windows)]
pub const DEFAULT_ROOT_CANDIDATES: &[&str] = &[
    r"source\repos",
    "Developer",
    "Projects",
    "workspaces",
    "workspace",
    "code",
    "src",
    "repos",
    "dev",
    "git",
    "GitHub",
    r"Documents\GitHub",
    r"Documents\Projects",
    r"Documents\Developer",
];

/// Top-level folders of the system drive where Windows developers often keep
/// code (short paths stay clear of the 260-character path limit).
#[cfg(windows)]
pub const DRIVE_ROOT_CANDIDATES: &[&str] = &[
    "dev",
    "src",
    "source",
    "repos",
    "code",
    "git",
    "GitHub",
    "Projects",
    "workspace",
    "workspaces",
];

/// Folders `default_roots` looks at, in order.
fn default_root_candidates(home: &Path) -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut out: Vec<PathBuf> = DEFAULT_ROOT_CANDIDATES
        .iter()
        .map(|rel| home.join(rel))
        .collect();
    #[cfg(windows)]
    {
        let drive = paths::system_drive();
        out.extend(DRIVE_ROOT_CANDIDATES.iter().map(|rel| drive.join(rel)));
    }
    out
}

/// Existing default roots under `home` (case-insensitive duplicates folded).
/// On Windows, matching top-level folders of the system drive too.
pub fn default_roots(home: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in default_root_candidates(home) {
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key.to_string_lossy().to_lowercase()) {
            out.push(p);
        }
    }
    out
}

/// Deduplicate and drop roots nested inside another root (keeps order).
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let std: Vec<PathBuf> = roots.iter().map(paths::standardize).collect();
    let mut out: Vec<PathBuf> = Vec::new();
    for (i, r) in std.iter().enumerate() {
        let nested = std
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && r != other && r.starts_with(other));
        if !nested && !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}

/// Result of adding a folder to the scan roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddRoot {
    Added,
    /// Already covered by this root.
    AlreadyCovered(PathBuf),
    /// Added, and it replaced these narrower roots.
    Replaced(Vec<PathBuf>),
}

pub fn add_root(roots: &mut Vec<PathBuf>, new: &Path) -> AddRoot {
    let new = paths::standardize(new);
    if let Some(cover) = roots
        .iter()
        .find(|r| new.starts_with(paths::standardize(r)))
    {
        return AddRoot::AlreadyCovered(cover.clone());
    }
    let (inner, keep): (Vec<PathBuf>, Vec<PathBuf>) = roots
        .drain(..)
        .partition(|r| paths::standardize(r).starts_with(&new));
    *roots = keep;
    roots.push(new);
    if inner.is_empty() {
        AddRoot::Added
    } else {
        AddRoot::Replaced(inner)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct RootsFile {
    roots: Vec<PathBuf>,
}

/// Where the chosen scan roots are remembered.
pub fn roots_file() -> PathBuf {
    paths::app_support_dir().join("developer-junk-roots.json")
}

/// Saved roots, or `None` when nothing was saved yet (use defaults).
pub fn load_roots(file: &Path) -> Option<Vec<PathBuf>> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice::<RootsFile>(&bytes)
        .ok()
        .map(|f| f.roots)
}

pub fn save_roots(file: &Path, roots: &[PathBuf]) -> anyhow::Result<()> {
    nera_core::fs_util::write_atomic(
        file,
        &serde_json::to_vec_pretty(&RootsFile {
            roots: roots.to_vec(),
        })?,
    )?;
    Ok(())
}

pub fn load_or_default_roots(file: &Path, home: &Path) -> Vec<PathBuf> {
    load_roots(file).unwrap_or_else(|| default_roots(home))
}

// MARK: - Formatting

/// `~/…` for paths under home on macOS; the full path on Windows, where `~`
/// is not a convention users know.
pub fn display_path(path: &Path) -> String {
    if cfg!(windows) {
        return path.display().to_string();
    }
    let home = paths::home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Local time of `t`; `None` for a timestamp outside the calendar's range
/// (a corrupt modification date must not crash the screen).
fn local_time(t: SystemTime) -> Option<chrono::DateTime<chrono::Local>> {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok()?,
        Err(e) => i64::try_from(e.duration().as_secs()).ok()?.checked_neg()?,
    };
    chrono::DateTime::from_timestamp(secs, 0).map(|utc| utc.with_timezone(&chrono::Local))
}

fn local_date(t: SystemTime) -> chrono::NaiveDate {
    local_time(t).map(|d| d.date_naive()).unwrap_or_default()
}

/// Calendar days from `then` to `now` in the local time zone (what Finder
/// counts: yesterday 23:00 is "yesterday" at 01:00, not "today"). Zero for the future.
fn days_between(then: SystemTime, now: SystemTime) -> u64 {
    local_date(now)
        .signed_duration_since(local_date(then))
        .num_days()
        .max(0) as u64
}

/// Whole calendar months from `then` to `now`.
fn months_between(then: SystemTime, now: SystemTime) -> u64 {
    use chrono::Datelike as _;
    let (a, b) = (local_date(then), local_date(now));
    let months = (b.year() - a.year()) * 12 + b.month() as i32
        - a.month() as i32
        - i32::from(b.day() < a.day());
    months.max(0) as u64
}

/// "today", "yesterday", "5 days ago", "3 weeks ago", "4 months ago", "2 years ago".
pub fn age_label(then: SystemTime, now: SystemTime) -> String {
    let days = days_between(then, now);
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..=13 => plural(days, "day"),
        14..=59 => plural(days / 7, "week"),
        _ => match months_between(then, now) {
            0..=1 => "2 months ago".into(), // 60+ days always span two calendar months
            m @ 2..=11 => plural(m, "month"),
            m => plural(m / 12, "year"),
        },
    }
}

pub fn is_inactive(last: Option<SystemTime>, now: SystemTime) -> bool {
    last.is_some_and(|t| days_between(t, now) >= INACTIVE_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("nera-devscan-{tag}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(paths::standardize(&dir))
        }

        fn file(&self, rel: &str, len: usize) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            // Synced, so APFS has allocated the blocks before a test reads
            // the file's private size (otherwise it reads 0 now and then).
            let mut f = fs::File::create(&p).unwrap();
            std::io::Write::write_all(&mut f, &vec![7u8; len]).unwrap();
            f.sync_all().unwrap();
            p
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(roots: &[PathBuf]) -> ScanOutcome {
        scan(
            &ScanOptions {
                roots: roots.to_vec(),
                excluded: Vec::new(),
            },
            &Cancel::new(),
            &|_| {},
        )
    }

    /// Relative junk paths with `/` separators (Windows uses `\`).
    fn junk_names(p: &Project) -> Vec<String> {
        let mut v: Vec<String> = p
            .junk
            .iter()
            .map(|j| j.relative.replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    /// Directory symlink on macOS; a junction on Windows (what npm and pnpm
    /// create, and needs no administrator rights).
    fn dir_link(target: &Path, link: &Path) {
        #[cfg(target_os = "macos")]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            // `mklink` reads a `/` inside a path as a switch.
            let native = |p: &Path| p.to_string_lossy().replace('/', "\\");
            let ok = Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(native(link))
                .arg(native(target))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "mklink /J failed");
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
    }

    /// `git` config files to ignore in tests (the user's own config).
    const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

    #[test]
    fn markers_and_priority() {
        let mask =
            rule_mask_for_markers(["package.json", "next.config.mjs", "README.md"].into_iter());
        assert_eq!(
            ecosystems_of(mask),
            vec![Ecosystem::NextJs, Ecosystem::Node]
        );
        let mask = rule_mask_for_markers(["App.sln", "Lib.csproj"].into_iter());
        assert_eq!(ecosystems_of(mask), vec![Ecosystem::DotNet]);
        assert_eq!(rule_mask_for_markers([".sln", "notes.txt"].into_iter()), 0);
        let mask = rule_mask_for_markers(["Demo.xcodeproj"].into_iter());
        #[cfg(target_os = "macos")]
        assert_eq!(ecosystems_of(mask), vec![Ecosystem::Xcode]);
        // Xcode / CocoaPods output can only be rebuilt on a Mac.
        #[cfg(windows)]
        {
            assert_eq!(mask, 0);
            assert_eq!(rule_mask_for_markers(["Podfile"].into_iter()), 0);
        }
    }

    #[test]
    fn junk_matching_respects_active_rules() {
        let node = rule_mask_for_markers(["package.json"].into_iter());
        assert!(match_junk(node, "node_modules", "app").is_some());
        assert!(match_junk(node, "target", "app").is_none());
        let py = rule_mask_for_markers(["pyproject.toml"].into_iter());
        assert_eq!(
            match_junk(py, "mypkg.egg-info", "x"),
            Some((Ecosystem::Python, Trust::Always))
        );
        assert!(match_junk(py, ".egg-info", "x").is_none());
        let ruby = rule_mask_for_markers(["Gemfile"].into_iter());
        assert!(match_junk(ruby, "bundle", "vendor").is_some());
        assert!(match_junk(ruby, "bundle", "other").is_none());
        assert!(is_unambiguous_junk_name("node_modules"));
        assert!(!is_unambiguous_junk_name("build"));
    }

    #[test]
    fn finds_projects_and_junk_without_descending_into_junk() {
        let t = TempDir::new("walk");
        // Node project with a nested package inside node_modules (must not be a project).
        t.file("web/package.json", 10);
        t.file("web/node_modules/left-pad/package.json", 10);
        t.file("web/node_modules/left-pad/index.js", 50_000);
        t.file("web/src/index.js", 100);
        // Rust project, target with Rust fingerprints (no git → evidence needed).
        t.file("tools/cli/Cargo.toml", 10);
        t.file("tools/cli/target/CACHEDIR.TAG", 10);
        t.file("tools/cli/target/debug/cli", 80_000);
        // Python with a venv and __pycache__ deep inside.
        t.file("py/pyproject.toml", 10);
        t.file("py/.venv/pyvenv.cfg", 10);
        t.file("py/.venv/lib/site.py", 20_000);
        t.file("py/pkg/__pycache__/a.pyc", 20_000);
        // Not a project: junk-looking names are ignored.
        t.file("notes/build/readme.txt", 5_000);
        t.file("notes/node_modules/x/package.json", 10);

        let out = run(std::slice::from_ref(&t.0));
        assert!(!out.cancelled);
        let names: Vec<&str> = out.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(out.projects.len(), 3, "{names:?}");
        let web = out.projects.iter().find(|p| p.name == "web").unwrap();
        assert_eq!(junk_names(web), vec!["node_modules"]);
        assert_eq!(web.primary(), Ecosystem::Node);
        assert!(web.size() >= 50_000);
        let cli = out.projects.iter().find(|p| p.name == "cli").unwrap();
        assert_eq!(junk_names(cli), vec!["target"]);
        let py = out.projects.iter().find(|p| p.name == "py").unwrap();
        assert_eq!(junk_names(py), vec![".venv", "pkg/__pycache__"]);
        // Sorted by size, largest first.
        assert!(out.projects.windows(2).all(|w| w[0].size() >= w[1].size()));
        assert!(out.projects[0]
            .junk
            .windows(2)
            .all(|w| w[0].size >= w[1].size));
    }

    #[test]
    fn ambiguous_names_need_evidence_outside_git() {
        let t = TempDir::new("ambig");
        t.file("app/build.gradle", 10);
        t.file("app/build/scripts/deploy.sh", 9_000); // hand-written "build" folder
        t.file("lib/build.gradle", 10);
        t.file("lib/build/intermediates/x.bin", 9_000); // Gradle output
        t.file("svc/go.mod", 10);
        t.file("svc/vendor/github.com/x/y.go", 9_000); // no modules.txt
        t.file("venvless/pyproject.toml", 10);
        t.file("venvless/venv/notes.txt", 9_000); // no pyvenv.cfg

        let out = run(std::slice::from_ref(&t.0));
        let names: Vec<&str> = out.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["lib"]);
        assert_eq!(junk_names(&out.projects[0]), vec!["build"]);
    }

    #[test]
    fn git_tracked_folders_are_never_offered() {
        let Some(git) = git_binary() else { return };
        let t = TempDir::new("git");
        let repo = t.dir("repo");
        t.file("repo/package.json", 10);
        t.file("repo/angular.json", 10);
        t.file("repo/dist/committed.js", 9_000);
        t.file("repo/.cache/generated.bin", 9_000);
        t.file("repo/.gitignore", 10);
        fs::write(t.0.join("repo/.gitignore"), ".cache/\nnode_modules/\n").unwrap();
        t.file("repo/node_modules/a/index.js", 9_000);
        let git_ok = |args: &[&str]| {
            Command::new(git)
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !git_ok(&["init", "-q"]) || !git_ok(&["add", "--", "dist"]) {
            return;
        }
        assert_eq!(git_tracks(&repo.join("dist")), Some(true));
        assert_eq!(git_tracks(&repo.join(".cache")), Some(false));
        assert_eq!(git_ignored(&repo.join(".cache")), Some(true));
        assert_eq!(git_ignored(&repo.join("dist")), Some(false));

        let out = run(std::slice::from_ref(&t.0));
        assert_eq!(out.projects.len(), 1);
        // `dist` is tracked → kept; `.cache` is untracked and ignored → offered.
        assert_eq!(out.projects[0].primary(), Ecosystem::Angular);
        assert_eq!(junk_names(&out.projects[0]), vec![".cache", "node_modules"]);
    }

    #[test]
    fn untracked_but_not_ignored_source_folder_is_not_offered() {
        let Some(git) = git_binary() else { return };
        let t = TempDir::new("untracked");
        let repo = t.dir("repo");
        t.file("repo/build.gradle", 10);
        t.file("repo/build/new-source/Main.java", 9_000); // new folder, not committed, not ignored
        t.file("repo/lib/build.gradle", 10);
        t.file("repo/lib/build/classes/A.class", 9_000); // has Gradle fingerprints
        let init = Command::new(git)
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !init {
            return;
        }
        let out = run(std::slice::from_ref(&t.0));
        let junk: Vec<String> = out
            .projects
            .iter()
            .flat_map(|p| p.junk.iter().map(|j| j.relative.replace('\\', "/")))
            .collect();
        assert_eq!(junk, vec!["lib/build"], "{junk:?}");
    }

    #[test]
    fn virtualenv_internals_are_never_offered() {
        let t = TempDir::new("venv");
        t.file("proj/pyproject.toml", 10);
        t.file("proj/venv_mgr/pyvenv.cfg", 10);
        t.file("proj/venv_mgr/lib/python3.12/site-packages/pip/_internal/operations/build/intermediates/x.py", 9_000);
        t.file(
            "proj/venv_mgr/lib/python3.12/site-packages/pkg/__pycache__/a.pyc",
            9_000,
        );
        t.file("proj/other/site-packages/pkg/build/classes/a.py", 9_000);
        let out = run(std::slice::from_ref(&t.0));
        assert!(
            out.projects.is_empty(),
            "{:?}",
            out.projects.iter().map(junk_names).collect::<Vec<_>>()
        );
    }

    #[test]
    fn node_modules_is_node_whatever_the_framework() {
        let node_next = rule_mask_for_markers(["package.json", "next.config.js"].into_iter());
        assert_eq!(
            match_junk(node_next, "node_modules", "app").map(|m| m.0),
            Some(Ecosystem::Node)
        );
        assert_eq!(
            match_junk(node_next, ".next", "app").map(|m| m.0),
            Some(Ecosystem::NextJs)
        );
        let ng = rule_mask_for_markers(["package.json", "angular.json"].into_iter());
        assert_eq!(
            match_junk(ng, ".angular", "app").map(|m| m.0),
            Some(Ecosystem::Angular)
        );
        assert_eq!(
            match_junk(ng, "dist", "app").map(|m| m.0),
            Some(Ecosystem::Angular)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sizes_match_du_and_count_hard_links_once() {
        let t = TempDir::new("size");
        let cancel = Cancel::new();
        let a = t.file("d/a.bin", 100_000);
        t.file("d/sub/b.bin", 50_000);
        fs::hard_link(&a, t.0.join("d/sub/a-link.bin")).unwrap();
        let alloc = |rel: &str| fs_util::allocated_size(&fs::metadata(t.0.join(rel)).unwrap());
        // Both links inside: the data is freed, and counted once.
        let expect = alloc("d/a.bin") + alloc("d/sub/b.bin");
        let dirs = fs_util::allocated_size(&fs::metadata(t.0.join("d")).unwrap())
            + fs_util::allocated_size(&fs::metadata(t.0.join("d/sub")).unwrap());
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!((m.size, m.unique), (expect + dirs, expect + dirs));
        let du = |rel: &str| -> Option<u64> {
            let out = Command::new("/usr/bin/du")
                .arg("-sk")
                .arg(t.0.join(rel))
                .output()
                .ok()?;
            Some(
                String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()?
                    * 1024,
            )
        };
        if let Some(bytes) = du("d") {
            assert_eq!(bytes, m.size); // the size shown is what `du` reports
        }
        // One link outside: `du` still counts the file once, but deleting the
        // folder frees nothing for it, so `unique` leaves it out.
        let outside = t.0.join("outside-link.bin");
        fs::hard_link(&a, &outside).unwrap();
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!(
            (m.size, m.unique),
            (expect + dirs, alloc("d/sub/b.bin") + dirs)
        );
        if let Some(bytes) = du("d") {
            assert_eq!(bytes, m.size);
        }
        // A symlink is not followed.
        std::os::unix::fs::symlink(&outside, t.0.join("d/sym")).unwrap();
        let sym = fs_util::allocated_size(&fs::symlink_metadata(t.0.join("d/sym")).unwrap());
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!(
            (m.size, m.unique),
            (expect + dirs + sym, alloc("d/sub/b.bin") + dirs + sym)
        );
        assert_eq!(measure(&t.0.join("missing"), &cancel).size, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn data_shared_with_clones_is_not_counted_as_guaranteed_free() {
        let t = TempDir::new("clone");
        let cancel = Cancel::new();
        let a = t.file("orig/a.bin", 2_000_000);
        t.file("own/c.bin", 2_000_000); // independent copy with its own blocks
        let cloned = Command::new("/bin/cp")
            .arg("-c")
            .arg(&a)
            .arg(t.0.join("clone.bin"))
            .status()
            .is_ok_and(|s| s.success());
        let (Some(p), true) = (private_size(&t.0.join("own/c.bin")), cloned) else {
            return;
        }; // not APFS
        assert_eq!(
            p,
            fs_util::allocated_size(&fs::metadata(t.0.join("own/c.bin")).unwrap())
        );
        // The clone and its original share all extents: neither frees anything alone.
        assert_eq!(private_size(&t.0.join("clone.bin")), Some(0));
        std::fs::create_dir_all(t.0.join("cl")).unwrap();
        fs::rename(t.0.join("clone.bin"), t.0.join("cl/clone.bin")).unwrap();
        let m = measure(&t.0.join("cl"), &cancel);
        let file = fs_util::allocated_size(&fs::metadata(t.0.join("cl/clone.bin")).unwrap());
        assert!(m.size >= file && m.unique < m.size, "{m:?}");
        assert!(
            measure(&t.0.join("cl"), &cancel).unique
                <= fs_util::allocated_size(&fs::metadata(t.0.join("cl")).unwrap())
        );
        let own = measure(&t.0.join("own"), &cancel);
        assert_eq!(own.size, own.unique);
        assert!(!is_shared(own.size, own.unique));
        assert!(is_shared(m.size, m.unique));
        // Shared whenever the displayed sizes differ, never when they read the same.
        assert!(is_shared(10_000_000, 9_600_000));
        assert!(is_shared(500_000, 0));
        assert!(is_shared(10_000_000, 1_000_000));
        assert!(!is_shared(810_971_136, 810_959_000)); // both "811 MB"
        assert!(!is_shared(5_000, 5_000));
        assert!(!is_shared(5_000, 6_000)); // unique can never exceed size
    }

    #[test]
    fn project_activity_is_the_newest_visible_project_file() {
        let t = TempDir::new("activity");
        let old = SystemTime::now() - Duration::from_secs(200 * 86_400);
        let recent = SystemTime::now() - Duration::from_secs(5 * 86_400);
        let set = |p: &PathBuf, when: SystemTime| {
            fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        set(&t.file("p/package.json", 10), old);
        set(&t.file("p/package-lock.json", 10), old);
        set(&t.file("p/src/deep/app.js", 10), recent);
        set(&t.file("p/.DS_Store", 10), SystemTime::now()); // hidden: ignored
        set(
            &t.file("p/node_modules/a/index.js", 5_000),
            SystemTime::now(),
        ); // junk: never walked
        set(&t.file("q/package.json", 10), old);
        t.file("q/node_modules/a.js", 5_000);

        let out = run(std::slice::from_ref(&t.0));
        let p = out.projects.iter().find(|p| p.name == "p").unwrap();
        let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs(p.last_activity.unwrap()), secs(recent));
        assert!(!is_inactive(p.last_activity, SystemTime::now()));
        let q = out.projects.iter().find(|p| p.name == "q").unwrap();
        assert_eq!(secs(q.last_activity.unwrap()), secs(old));
        assert!(is_inactive(q.last_activity, SystemTime::now()));
    }

    #[test]
    fn folders_scanned_counts_listed_folders_only() {
        let t = TempDir::new("count");
        t.file("p/package.json", 10);
        t.file("p/src/a/b.js", 10); // p, p/src, p/src/a
        t.file("p/node_modules/x/y.js", 5_000); // junk: not listed
        t.file(".hidden/z", 10); // hidden: not listed
        t.file("Thing.app/Contents/x", 10); // bundle: not listed
        dir_link(&t.0.join("p/src"), &t.0.join("link"));
        let out = run(std::slice::from_ref(&t.0));
        assert_eq!(out.folders_scanned, 4); // root + p + p/src + p/src/a
    }

    #[test]
    fn nested_repo_with_markers_is_its_own_project_and_subprojects_merge() {
        let t = TempDir::new("nest");
        t.file("mono/package.json", 10);
        t.file("mono/packages/api/Cargo.toml", 10);
        t.file("mono/packages/api/target/CACHEDIR.TAG", 4_000);
        t.file("mono/packages/web/node_modules/a.js", 4_000);
        t.file("mono/vendor-repo/.git/HEAD", 10);
        t.file("mono/vendor-repo/package.json", 10);
        t.file("mono/vendor-repo/node_modules/b.js", 4_000);

        let out = run(std::slice::from_ref(&t.0));
        let mono = out.projects.iter().find(|p| p.name == "mono").unwrap();
        assert_eq!(
            junk_names(mono),
            vec!["packages/api/target", "packages/web/node_modules"]
        );
        let rust = mono
            .junk
            .iter()
            .find(|j| j.relative.ends_with("target"))
            .unwrap();
        assert_eq!(rust.ecosystem, Ecosystem::Rust);
        assert!(out.projects.iter().any(|p| p.name == "vendor-repo"));
    }

    #[test]
    fn skips_symlinks_hidden_bundles_and_excluded() {
        let t = TempDir::new("skip");
        t.file("real/package.json", 10);
        t.file("real/node_modules/a.js", 4_000);
        dir_link(&t.0.join("real"), &t.0.join("link"));
        t.file(".hidden/proj/package.json", 10);
        t.file(".hidden/proj/node_modules/a.js", 4_000);
        t.file("Thing.app/Contents/package.json", 10);
        t.file("Thing.app/Contents/node_modules/a.js", 4_000);
        t.file("skipme/package.json", 10);
        t.file("skipme/node_modules/a.js", 4_000);
        t.file("keep/package.json", 10);
        t.file("keep/node_modules/precious/a.js", 4_000);

        let opts = ScanOptions {
            roots: vec![t.0.clone()],
            excluded: vec![t.0.join("skipme"), t.0.join("keep/node_modules/precious")],
        };
        let out = scan(&opts, &Cancel::new(), &|_| {});
        let names: Vec<&str> = out.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn cancelled_scan_returns_quickly_and_flags_it() {
        let t = TempDir::new("cancel");
        t.file("p/package.json", 10);
        t.file("p/node_modules/a.js", 4_000);
        let cancel = Cancel::new();
        cancel.cancel();
        let out = scan(
            &ScanOptions {
                roots: vec![t.0.clone()],
                excluded: vec![],
            },
            &cancel,
            &|_| {},
        );
        assert!(out.cancelled);
        assert!(out.projects.is_empty());
    }

    #[test]
    fn reports_missing_roots_and_progress() {
        let t = TempDir::new("prog");
        t.file("p/package.json", 10);
        t.file("p/node_modules/a.js", 4_000);
        let seen = std::sync::Mutex::new(Vec::new());
        let missing = paths::standardize(
            std::env::temp_dir().join(format!("nera-devscan-missing-{}", std::process::id())),
        );
        let out = scan(
            &ScanOptions {
                roots: vec![t.0.clone(), missing.clone()],
                excluded: vec![],
            },
            &Cancel::new(),
            &|p| seen.lock().unwrap().push(p.clone()),
        );
        assert_eq!(out.missing_roots, vec![missing]);
        let seen = seen.into_inner().unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.phase, Phase::Measuring);
        assert_eq!(last.measured, last.to_measure);
        assert_eq!(last.fraction(), Some(1.0));
    }

    #[test]
    fn cleans_only_real_folders_and_updates_results() {
        let t = TempDir::new("clean");
        let a = t.dir("p/node_modules");
        t.file("p/node_modules/x.js", 1_000);
        let file = t.file("p/not-a-dir", 10);
        let gone = t.0.join("p/already-gone");
        let target = t.dir("elsewhere");
        let link = t.0.join("p/link");
        dir_link(&target, &link);

        let paths = vec![a.clone(), file.clone(), gone.clone(), link.clone()];
        let calls = AtomicUsize::new(0);
        let rep = clean(&paths, false, &Cancel::new(), &|_, _| {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(rep.removed, vec![a.clone(), gone]);
        assert_eq!(rep.failed.len(), 2);
        assert!(!a.exists() && file.exists() && target.exists());
        assert!(calls.load(Ordering::Relaxed) >= paths.len());

        let mut projects = vec![Project {
            root: t.0.join("p"),
            name: "p".into(),
            ecosystems: vec![Ecosystem::Node],
            last_activity: None,
            junk: vec![JunkFolder {
                path: a.clone(),
                relative: "node_modules".into(),
                ecosystem: Ecosystem::Node,
                size: 1,
                unique: 1,
                modified: None,
            }],
        }];
        apply_removed(&mut projects, &rep.removed);
        assert!(projects.is_empty());
    }

    #[test]
    fn roots_defaults_normalize_add_and_persist() {
        let t = TempDir::new("roots");
        t.dir("Developer");
        t.dir("Documents/GitHub");
        t.file("code", 10); // a file, not a folder
        let defaults = default_roots(&t.0);
        #[cfg(target_os = "macos")]
        assert_eq!(
            defaults,
            vec![t.0.join("Developer"), t.0.join("Documents/GitHub")]
        );
        // Home folders first, then whatever exists at the top of the system drive.
        #[cfg(windows)]
        {
            assert_eq!(
                defaults[..2],
                [t.0.join("Developer"), t.0.join(r"Documents\GitHub")]
            );
            let drive = paths::system_drive();
            assert!(defaults[2..].iter().all(|p| p.parent() == Some(&drive)));
        }

        let norm = normalize_roots(&[t.0.join("a/b"), t.0.join("a"), t.0.join("a"), t.0.join("c")]);
        assert_eq!(norm, vec![t.0.join("a"), t.0.join("c")]);

        let mut roots = vec![t.0.join("x/one"), t.0.join("y")];
        assert_eq!(
            add_root(&mut roots, &t.0.join("y/inner")),
            AddRoot::AlreadyCovered(t.0.join("y"))
        );
        assert_eq!(
            add_root(&mut roots, &t.0.join("x")),
            AddRoot::Replaced(vec![t.0.join("x/one")])
        );
        assert_eq!(add_root(&mut roots, &t.0.join("z")), AddRoot::Added);
        assert_eq!(roots, vec![t.0.join("y"), t.0.join("x"), t.0.join("z")]);

        let file = t.0.join("roots.json");
        assert_eq!(load_roots(&file), None);
        assert_eq!(load_or_default_roots(&file, &t.0), defaults);
        save_roots(&file, &roots).unwrap();
        assert_eq!(load_roots(&file), Some(roots.clone()));
        save_roots(&file, &[]).unwrap();
        assert_eq!(load_or_default_roots(&file, &t.0), Vec::<PathBuf>::new());

        // Visual Studio's project folder comes first.
        #[cfg(windows)]
        {
            t.dir(r"source\repos");
            assert_eq!(default_roots(&t.0)[0], t.0.join(r"source\repos"));
        }
    }

    #[test]
    fn ages_and_inactivity() {
        // Noon today: ages count calendar days, so "a minute ago" taken
        // just after midnight would be yesterday.
        let noon = chrono::Local::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap();
        let now = SystemTime::from(noon);
        let ago = |days: u64| now - Duration::from_secs(days * 86_400 + 60);
        assert_eq!(age_label(ago(0), now), "today");
        assert_eq!(age_label(ago(1), now), "yesterday");
        assert_eq!(age_label(ago(5), now), "5 days ago");
        assert_eq!(age_label(ago(14), now), "2 weeks ago");
        assert_eq!(age_label(ago(65), now), "2 months ago");
        assert_eq!(age_label(ago(400), now), "1 year ago");
        assert_eq!(age_label(now + Duration::from_secs(500), now), "today");

        // Calendar days in the local time zone, not 24-hour periods.
        use chrono::TimeZone as _;
        let at = |y: i32, m: u32, d: u32, h: u32, min: u32| -> SystemTime {
            chrono::Local
                .with_ymd_and_hms(y, m, d, h, min, 0)
                .earliest()
                .unwrap()
                .into()
        };
        let now = at(2026, 11, 10, 1, 0);
        assert_eq!(age_label(at(2026, 11, 9, 23, 30), now), "yesterday"); // 1.5 hours ago
        assert_eq!(age_label(at(2026, 11, 8, 23, 0), now), "2 days ago"); // 26 hours ago
        assert_eq!(age_label(at(2026, 11, 10, 0, 5), now), "today");
        assert_eq!(age_label(at(2026, 10, 27, 12, 0), now), "2 weeks ago"); // 14 days
        assert_eq!(age_label(at(2026, 9, 12, 12, 0), now), "8 weeks ago"); // 59 days
        assert_eq!(age_label(at(2026, 9, 11, 12, 0), now), "2 months ago"); // 60 days
        assert_eq!(age_label(at(2026, 9, 10, 12, 0), now), "2 months ago");
        assert_eq!(age_label(at(2026, 6, 10, 12, 0), now), "5 months ago");
        assert_eq!(age_label(at(2026, 6, 11, 12, 0), now), "4 months ago");
        assert_eq!(age_label(at(2025, 11, 11, 12, 0), now), "11 months ago"); // 364 days
        assert_eq!(age_label(at(2025, 11, 10, 12, 0), now), "1 year ago");
        assert_eq!(age_label(at(2024, 11, 10, 12, 0), now), "2 years ago");
        assert!(is_inactive(Some(at(2026, 8, 12, 12, 0)), now)); // 90 days
        assert!(!is_inactive(Some(at(2026, 8, 13, 12, 0)), now)); // 89 days
        assert!(is_inactive(Some(ago(120)), now));
        assert!(!is_inactive(Some(ago(10)), now));
        assert!(!is_inactive(None, now));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(display_path(&paths::home().join("x/y")), "~/x/y");
            assert_eq!(display_path(Path::new("/opt/x")), "/opt/x");
        }
        #[cfg(windows)]
        assert_eq!(
            display_path(&paths::home().join(r"x\y")),
            format!(r"{}\x\y", paths::home().display())
        );
    }

    // MARK: Windows

    #[cfg(windows)]
    fn attrib(flag: &str, path: &Path) {
        let ok = Command::new("attrib")
            .arg(flag)
            .arg(path)
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "attrib {flag} failed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_sizes_count_hard_links_once_and_never_follow_junctions() {
        let t = TempDir::new("winsize");
        let cancel = Cancel::new();
        let a = t.file("d/a.bin", 100_000);
        t.file("d/sub/b.bin", 50_000);
        t.file("d/empty.txt", 0);
        fs::hard_link(&a, t.0.join("d/sub/a-link.bin")).unwrap();
        // Both links inside: freed with the folder, counted once (folders hold
        // no bytes in Explorer's figures).
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!((m.size, m.unique), (150_000, 150_000));

        // One link outside (like pnpm's store): still counted once in the size,
        // but deleting the folder frees nothing for it.
        let outside = t.0.join("outside-link.bin");
        fs::hard_link(&a, &outside).unwrap();
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!((m.size, m.unique), (150_000, 50_000));
        assert!(is_shared(m.size, m.unique));

        // A junction (npm / pnpm) is never followed, however big its target.
        t.file("big/huge.bin", 3_000_000);
        dir_link(&t.0.join("big"), &t.0.join("d/sub/junction"));
        let m = measure(&t.0.join("d"), &cancel);
        assert_eq!((m.size, m.unique), (150_000, 50_000));

        // A single file: its length, nothing when another link keeps it alive.
        let single = measure(&t.0.join("d/sub/b.bin"), &cancel);
        assert_eq!((single.size, single.unique), (50_000, 50_000));
        assert_eq!(measure(&a, &cancel), Measured::default());
        assert_eq!(measure(&t.0.join("missing"), &cancel), Measured::default());
    }

    #[cfg(windows)]
    #[test]
    fn windows_scan_skips_junctions_and_hidden_folders() {
        let t = TempDir::new("winwalk");
        // A .NET project: bin/obj with build fingerprints, .vs (hidden by VS).
        t.file("App/App.csproj", 10);
        t.file("App/Program.cs", 10);
        t.file("App/bin/Debug/net8.0/App.dll", 20_000);
        t.file("App/obj/project.assets.json", 5_000);
        t.file("App/.vs/App/v17/.suo", 3_000);
        attrib("+h", &t.0.join("App/.vs"));
        // A hand-written bin folder (scripts): no fingerprints, not offered.
        t.file("Tool/Tool.csproj", 10);
        t.file("Tool/bin/deploy.ps1", 2_000);
        // A project inside a hidden folder (like AppData) is never found.
        t.file("Hidden/proj/package.json", 10);
        t.file("Hidden/proj/node_modules/a.js", 4_000);
        attrib("+h", &t.0.join("Hidden"));
        // A junction to a project is not followed.
        t.file("real/package.json", 10);
        t.file("real/node_modules/a.js", 4_000);
        dir_link(&t.0.join("real"), &t.0.join("linked"));

        let out = run(std::slice::from_ref(&t.0));
        let mut names: Vec<&str> = out.projects.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["App", "real"]);
        let app = out.projects.iter().find(|p| p.name == "App").unwrap();
        assert_eq!(junk_names(app), vec![".vs", "bin", "obj"]);
        assert_eq!(app.size(), 28_000);
        assert!(app.junk.iter().all(|j| j.ecosystem == Ecosystem::DotNet));
        assert!(!out.privacy_denied);
    }

    #[cfg(windows)]
    #[test]
    fn windows_clean_removes_junctions_inside_without_following_them() {
        let t = TempDir::new("winclean");
        // pnpm workspace: node_modules/@scope/lib is a junction to a source folder.
        let lib = t.file("ws/packages/lib/index.js", 1_000);
        let nm = t.dir("ws/node_modules/@scope");
        t.file("ws/node_modules/.pnpm/x/index.js", 2_000);
        dir_link(&t.0.join("ws/packages/lib"), &nm.join("lib"));
        let junk = t.0.join("ws/node_modules");
        let rep = clean(
            std::slice::from_ref(&junk),
            false,
            &Cancel::new(),
            &|_, _| {},
        );
        assert_eq!(rep.removed, vec![junk.clone()], "{:?}", rep.failed);
        assert!(!junk.exists());
        assert!(lib.exists(), "the junction target must survive");
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_git() {
        // Git for Windows is optional; when present it must be an absolute .exe.
        if let Some(git) = git_binary() {
            assert!(git.is_absolute() && git.is_file(), "{}", git.display());
        }
    }
}
