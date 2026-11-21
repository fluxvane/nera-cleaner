//! Git worktrees nobody needs any more: merged branches, folders that were
//! deleted behind git's back, and long-abandoned ones.
//!
//! The rule that shapes everything here: **fail closed**. A worktree is only
//! ever offered as safe when every check gave a positive answer. A git command
//! that fails, times out or prints something unexpected means "can't verify",
//! never "clean". Removal re-runs the checks right before deleting.

use super::{display_path, git_binary, is_inactive, is_unambiguous_junk_name, measure, mtime};
use super::{no_window, Measured, MAX_DEPTH, SKIP_NAMES};
use nera_core::fs_util::{self, Cancel};
use nera_core::paths;
use rayon::prelude::*;
use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// A branch with no commits of its own that is younger than this is a worktree
/// somebody just created, not one that was merged.
pub const QUIET_DAYS: u64 = 14;
/// Longest a single git command may run before the answer is "unknown".
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Output kept from a git command; the rest is read and dropped.
const OUTPUT_CAP: u64 = 4 << 20;
/// Repositories examined at the same time (git is mostly disk bound).
const THREADS: usize = 4;

// MARK: - Verdict

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Nothing is lost by removing it: ticked by default.
    Safe,
    /// Probably fine, but the user has to look first: never ticked by default.
    Review,
    /// Removing it could lose work: cannot be ticked.
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Its commits are all in the default branch and nothing is left in the folder.
    Merged,
    /// The folder is gone; only git's record of it remains.
    Missing,
    /// Not touched for a long time, but not known to be merged.
    Stale,
    /// Clean and merged, but holds ignored files that are not build output.
    IgnoredFiles,
    /// The folder is missing and so is the volume or folder around it.
    VolumeUnavailable,
    /// A check could not be completed.
    Unverified,
    /// Uncommitted changes or untracked files.
    Dirty,
    /// Commits that exist nowhere else.
    Unpushed,
    /// A stash was made on its branch.
    Stashed,
    Locked,
    /// A program is running from it, or it is the folder Nera Cleaner runs in.
    InUse,
}

impl Reason {
    pub fn level(self) -> Level {
        match self {
            Self::Merged | Self::Missing => Level::Safe,
            Self::Stale | Self::IgnoredFiles | Self::VolumeUnavailable | Self::Unverified => {
                Level::Review
            }
            Self::Dirty | Self::Unpushed | Self::Stashed | Self::Locked | Self::InUse => {
                Level::Blocked
            }
        }
    }
}

/// Everything known about one linked worktree. `None` means "could not find out".
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub locked: bool,
    pub prunable: bool,
    pub dir_exists: bool,
    pub parent_exists: bool,
    /// Its HEAD commit is in the default branch.
    pub merged: Option<bool>,
    /// Its history shows commits, merges or rebases made here.
    pub touched: bool,
    pub last_activity: Option<SystemTime>,
    pub dirty: Option<bool>,
    /// Ignored files other than known build output.
    pub extra_ignored: Option<bool>,
    pub unpushed: Option<bool>,
    pub stashed: Option<bool>,
    pub in_use: bool,
}

/// Why `facts` is (or is not) a cleanup candidate. `None`: in active use, leave it alone.
pub fn classify(f: &Facts, now: SystemTime) -> Option<Reason> {
    if !f.dir_exists {
        if f.locked {
            return Some(Reason::Locked);
        }
        return Some(if f.prunable && f.parent_exists {
            Reason::Missing
        } else {
            Reason::VolumeUnavailable
        });
    }

    let quiet = !f.last_activity.is_some_and(|t| !is_old(t, now));
    let base = if f.merged == Some(true) && (f.touched || quiet) {
        Reason::Merged
    } else if is_inactive(f.last_activity, now) {
        Reason::Stale
    } else {
        return None;
    };

    if f.locked {
        return Some(Reason::Locked);
    }
    if f.in_use {
        return Some(Reason::InUse);
    }
    if f.dirty == Some(true) {
        return Some(Reason::Dirty);
    }
    if f.unpushed == Some(true) {
        return Some(Reason::Unpushed);
    }
    if f.stashed == Some(true) {
        return Some(Reason::Stashed);
    }
    if f.dirty.is_none() || f.unpushed.is_none() || f.stashed.is_none() {
        return Some(Reason::Unverified);
    }
    if f.extra_ignored != Some(false) {
        return Some(if f.extra_ignored.is_none() {
            Reason::Unverified
        } else if base == Reason::Merged {
            Reason::IgnoredFiles
        } else {
            base
        });
    }
    Some(base)
}

/// Older than [`QUIET_DAYS`].
fn is_old(t: SystemTime, now: SystemTime) -> bool {
    now.duration_since(t)
        .is_ok_and(|d| d >= Duration::from_secs(QUIET_DAYS * 86_400))
}

// MARK: - Results

#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The main working copy this worktree belongs to.
    pub repo: PathBuf,
    pub branch: Option<String>,
    pub reason: Reason,
    /// Space the folder occupies; 0 when the folder is gone.
    pub size: u64,
    pub last_activity: Option<SystemTime>,
    /// The tool that made it, when the location says so.
    pub origin: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepoWorktrees {
    pub repo: PathBuf,
    pub name: String,
    pub worktrees: Vec<Worktree>,
}

impl RepoWorktrees {
    pub fn size(&self) -> u64 {
        self.worktrees.iter().map(|w| w.size).sum()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Found {
    pub repos: Vec<RepoWorktrees>,
    /// Repositories have linked worktrees but git could not be run.
    pub git_missing: bool,
}

pub fn origin_of(path: &Path) -> Option<&'static str> {
    let text = path.to_string_lossy().replace('\\', "/");
    text.contains("/.claude/worktrees/")
        .then_some("Claude Code")
}

// MARK: - Git

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn lines(&self) -> Vec<String> {
        self.text()
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Run git in `cwd` with a timeout. `None` when git is missing, could not
/// start or ran too long. Both pipes are drained on threads, so output of any
/// size cannot stall the child.
fn git(cwd: &Path, args: &[&str]) -> Option<Out> {
    let bin = git_binary()?;
    let mut cmd = Command::new(bin);
    no_window(&mut cmd);
    let mut child = cmd
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = (&mut pipe).take(OUTPUT_CAP).read_to_end(&mut kept);
                let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            }
            kept
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < GIT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(4));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let _ = stderr.join();
    let status = status?;
    Some(Out {
        ok: status.success(),
        code: status.code(),
        stdout,
    })
}

/// One entry of `git worktree list --porcelain -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub fn parse_porcelain(raw: &[u8]) -> Vec<Listed> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut cur: Option<Listed> = None;
    for field in text.split('\0') {
        if field.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            out.extend(cur.take());
            cur = Some(Listed {
                path: PathBuf::from(value),
                ..Listed::default()
            });
            continue;
        }
        let Some(entry) = cur.as_mut() else { continue };
        match key {
            "HEAD" => entry.head = Some(value.to_string()),
            "branch" => {
                entry.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).into());
            }
            "bare" => entry.bare = true,
            "locked" => entry.locked = true,
            "prunable" => entry.prunable = true,
            _ => {}
        }
    }
    out.extend(cur);
    out
}

/// The worktrees of the repository at `repo`, main one first.
fn list(repo: &Path) -> Option<Vec<Listed>> {
    let out = git(repo, &["worktree", "list", "--porcelain", "-z"])?;
    out.ok.then(|| parse_porcelain(&out.stdout))
}

/// The branch the others are measured against: what the remote calls its
/// default, else the first of the usual names that exists. Remote refs win, so
/// a stale local `main` cannot make everything look merged.
fn default_ref(repo: &Path) -> Option<String> {
    let exists = |name: &str| {
        git(
            repo,
            &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")],
        )
        .is_some_and(|o| o.ok)
    };
    if let Some(out) = git(
        repo,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let name = out.text().trim().to_string();
        if out.ok && !name.is_empty() && exists(&name) {
            return Some(name);
        }
    }
    [
        "origin/main",
        "origin/master",
        "main",
        "master",
        "trunk",
        "develop",
    ]
    .into_iter()
    .find(|n| exists(n))
    .map(String::from)
}

/// Stash subjects of the whole repository (stashes are not per worktree).
fn stash_subjects(repo: &Path) -> Option<Vec<String>> {
    let out = git(repo, &["stash", "list", "--format=%gs"])?;
    out.ok.then(|| out.lines())
}

/// `.git/worktrees/<name>` of the linked worktree at `path`.
fn admin_dir(repo: &Path, path: &Path) -> Option<PathBuf> {
    let root = repo.join(".git").join("worktrees");
    let want = paths::standardize(path);
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let Some(dot_git) = Path::new(text.trim()).parent() else {
            continue;
        };
        let have = paths::standardize(dot_git);
        if paths::is_under(&have, &want) && paths::is_under(&want, &have) {
            return Some(entry.path());
        }
    }
    None
}

/// True when the reflog shows commits, merges or rebases made in this worktree.
fn history_shows_work(admin: &Path) -> bool {
    let Ok(log) = std::fs::read_to_string(admin.join("logs").join("HEAD")) else {
        return false;
    };
    log.lines().any(|line| {
        line.split_once('\t').is_some_and(|(_, action)| {
            ["commit", "merge", "rebase", "pull", "cherry-pick", "revert"]
                .iter()
                .any(|a| action.starts_with(a))
        })
    })
}

/// Ignored files that are not build output: `.env`, local settings, data.
fn has_extra_ignored(worktree: &Path) -> Option<bool> {
    let out = git(
        worktree,
        &[
            "ls-files",
            "-o",
            "-i",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    if !out.ok {
        return None;
    }
    Some(
        out.text()
            .split('\0')
            .filter(|e| !e.is_empty())
            .any(|entry| {
                !entry
                    .split('/')
                    .any(|part| !part.is_empty() && is_unambiguous_junk_name(part))
            }),
    )
}

/// Collect the facts for one listed (non-main) worktree.
fn facts_for(
    repo: &Path,
    entry: &Listed,
    default: Option<&str>,
    stashes: Option<&[String]>,
    now: SystemTime,
) -> Facts {
    let dir_exists = entry.path.is_dir();
    let mut f = Facts {
        locked: entry.locked,
        prunable: entry.prunable,
        dir_exists,
        parent_exists: entry.path.parent().is_some_and(Path::is_dir),
        ..Facts::default()
    };
    if !dir_exists {
        return f;
    }
    let sha = entry.head.as_deref();

    f.merged = match (sha, default) {
        (Some(sha), Some(default)) => git(repo, &["merge-base", "--is-ancestor", sha, default])
            .and_then(|o| match o.code {
                Some(0) => Some(true),
                Some(1) => Some(false),
                _ => None,
            }),
        _ => None,
    };

    let admin = admin_dir(repo, &entry.path);
    f.touched = admin.as_deref().is_some_and(history_shows_work);
    let commit_time = sha.and_then(|sha| {
        let out = git(repo, &["log", "-1", "--format=%ct", sha])?;
        let secs: u64 = out.text().trim().parse().ok().filter(|_| out.ok)?;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    });
    f.last_activity = [
        commit_time,
        admin.as_ref().and_then(|a| mtime(&a.join("index"))),
        admin
            .as_ref()
            .and_then(|a| mtime(&a.join("logs").join("HEAD"))),
        admin.as_ref().and_then(|a| mtime(&a.join("HEAD"))),
    ]
    .into_iter()
    .flatten()
    .filter(|t| *t <= now + Duration::from_secs(86_400))
    .max();

    f.dirty = git(
        &entry.path,
        &[
            "status",
            "--porcelain=v1",
            "-uall",
            "--ignore-submodules=none",
        ],
    )
    .filter(|o| o.ok)
    .map(|o| !o.stdout.is_empty());

    f.extra_ignored = has_extra_ignored(&entry.path);

    f.unpushed = sha.and_then(|sha| {
        let mut args = vec!["rev-list", "-n", "1", sha, "--not", "--remotes"];
        args.extend(default);
        let out = git(repo, &args)?;
        out.ok.then_some(!out.stdout.is_empty())
    });

    f.stashed = stashes.map(|subjects| match entry.branch.as_deref() {
        Some(branch) => subjects
            .iter()
            .any(|s| s.contains(&format!("on {branch}:")) || s.contains(&format!("On {branch}:"))),
        None => !subjects.is_empty(),
    });
    f
}

/// Folders Nera Cleaner itself runs in or from.
fn own_places() -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        places.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        places.push(exe);
    }
    places
}

/// Re-check `in_use` for a worktree that is otherwise a candidate.
fn in_use(path: &Path, own: &[PathBuf]) -> bool {
    own.iter().any(|p| paths::is_under(p, path)) || fs_util::any_process_under(path)
}

/// Facts and verdict for one worktree, including the (slower) in-use check.
fn assess(
    repo: &Path,
    entry: &Listed,
    default: Option<&str>,
    stashes: Option<&[String]>,
    own: &[PathBuf],
    now: SystemTime,
) -> (Facts, Option<Reason>) {
    let mut facts = facts_for(repo, entry, default, stashes, now);
    let mut verdict = classify(&facts, now);
    if facts.dir_exists && verdict.is_some_and(|r| r.level() != Level::Blocked) {
        facts.in_use = in_use(&entry.path, own);
        verdict = classify(&facts, now);
    }
    (facts, verdict)
}

// MARK: - Finding

/// Repositories in the scan roots: folders whose `.git` is a directory.
/// (Linked worktrees have a `.git` *file*, and git lists them itself.)
pub fn find_repos(roots: &[PathBuf], excluded: &[PathBuf], cancel: &Cancel) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if cancel.is_cancelled() {
                return repos;
            }
            if std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
                if seen.insert(paths::standardize(&dir)) {
                    repos.push(dir);
                }
                continue;
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                if !ft.is_dir()
                    || ft.is_symlink()
                    || name.starts_with('.')
                    || SKIP_NAMES.contains(&name.as_str())
                    || is_unambiguous_junk_name(&name)
                {
                    continue;
                }
                let path = entry.path();
                if !excluded.is_empty() && paths::is_under_any(&path, excluded) {
                    continue;
                }
                stack.push((path, depth + 1));
            }
        }
    }
    repos
}

fn repo_name(repo: &Path) -> String {
    repo.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| display_path(repo))
}

/// Examine the linked worktrees of each repository. Only worktrees inside
/// `roots` (and outside `excluded`) are offered. `report(done, total)` is
/// called from worker threads.
pub fn scan(
    repos: &[PathBuf],
    roots: &[PathBuf],
    excluded: &[PathBuf],
    cancel: &Cancel,
    report: &(dyn Fn(usize, usize) + Sync),
) -> Found {
    // Only repositories that have linked worktrees need any git work.
    let with_linked: Vec<&PathBuf> = repos
        .iter()
        .filter(|r| {
            std::fs::read_dir(r.join(".git").join("worktrees"))
                .is_ok_and(|mut d| d.next().is_some())
        })
        .collect();
    let mut found = Found::default();
    if with_linked.is_empty() {
        return found;
    }
    if git_binary().is_none() {
        found.git_missing = true;
        return found;
    }
    let own = own_places();
    let now = SystemTime::now();
    let total = with_linked.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        with_linked
            .par_iter()
            .filter_map(|repo| {
                if cancel.is_cancelled() {
                    return None;
                }
                let result = scan_repo(repo, roots, excluded, &own, cancel, now);
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                report(n, total);
                result
            })
            .collect::<Vec<_>>()
    };
    found.repos = match rayon::ThreadPoolBuilder::new().num_threads(THREADS).build() {
        Ok(pool) => pool.install(work),
        Err(_) => work(),
    };
    found.repos.sort_by(|a, b| {
        b.size()
            .cmp(&a.size())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    found
}

fn scan_repo(
    repo: &Path,
    roots: &[PathBuf],
    excluded: &[PathBuf],
    own: &[PathBuf],
    cancel: &Cancel,
    now: SystemTime,
) -> Option<RepoWorktrees> {
    let listed = list(repo)?;
    let default = default_ref(repo);
    let stashes = stash_subjects(repo);
    let mut worktrees = Vec::new();
    for entry in listed.iter().skip(1).filter(|e| !e.bare) {
        if cancel.is_cancelled() {
            return None;
        }
        let path = paths::standardize(&entry.path);
        if !paths::is_under_any(&path, roots) || paths::is_under_any(&path, excluded) {
            continue;
        }
        let (facts, verdict) = assess(
            repo,
            entry,
            default.as_deref(),
            stashes.as_deref(),
            own,
            now,
        );
        let Some(reason) = verdict else { continue };
        let size = if facts.dir_exists {
            let Measured { size, .. } = measure(&entry.path, cancel);
            size
        } else {
            0
        };
        worktrees.push(Worktree {
            origin: origin_of(&path),
            path: entry.path.clone(),
            repo: repo.to_path_buf(),
            branch: entry.branch.clone(),
            reason,
            size,
            last_activity: facts.last_activity,
        });
    }
    if worktrees.is_empty() {
        return None;
    }
    worktrees.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    Some(RepoWorktrees {
        repo: repo.to_path_buf(),
        name: repo_name(repo),
        worktrees,
    })
}

// MARK: - Removing

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    /// `git worktree prune` also succeeded, so git's own record is gone too.
    pub pruned: bool,
}

/// Remove one worktree, to the Trash when `to_trash`. The checks run again
/// first: anything that would block it now refuses, even if the screen had
/// ticked it. Never forces and never touches branches.
pub fn remove(
    repo: &Path,
    path: &Path,
    to_trash: bool,
    cancel: &Cancel,
) -> Result<Removed, String> {
    if git_binary().is_none() {
        return Err("Git could not be found".into());
    }
    let listed = list(repo).ok_or("Git could not list the worktrees")?;
    let want = paths::standardize(path);
    let entry = listed
        .iter()
        .skip(1)
        .filter(|e| !e.bare)
        .find(|e| paths::standardize(&e.path) == want)
        .ok_or("No longer a linked worktree of this repository; left untouched")?;
    if cancel.is_cancelled() {
        return Err("Cancelled".into());
    }

    let default = default_ref(repo);
    let stashes = stash_subjects(repo);
    let (facts, verdict) = assess(
        repo,
        entry,
        default.as_deref(),
        stashes.as_deref(),
        &own_places(),
        SystemTime::now(),
    );
    match verdict {
        None => return Err("It changed since the scan and is in use again; left untouched".into()),
        Some(r) if r.level() == Level::Blocked => {
            return Err(format!("Left untouched: {}", blocked_text(r)));
        }
        Some(_) => {}
    }

    if facts.dir_exists {
        let meta = std::fs::symlink_metadata(&entry.path).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err("Not a real folder; left untouched".into());
        }
        // The main working copy has a `.git` folder; a linked one a `.git` file.
        let dot_git = std::fs::symlink_metadata(entry.path.join(".git"));
        if !dot_git.is_ok_and(|m| m.is_file()) {
            return Err("Not a linked worktree; left untouched".into());
        }
        fs_util::remove_or_trash(&entry.path, to_trash).map_err(|e| e.to_string())?;
    }
    let pruned = git(repo, &["worktree", "prune"]).is_some_and(|o| o.ok);
    Ok(Removed { pruned })
}

#[derive(Clone, Debug, Default)]
pub struct RemoveReport {
    /// Removed (or already gone) worktree folders.
    pub removed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    /// Removed, but git's own record could not be pruned.
    pub unpruned: usize,
    pub cancelled: bool,
}

/// Remove `(repo, worktree)` pairs one by one; see [`remove`].
pub fn remove_many(
    items: &[(PathBuf, PathBuf)],
    to_trash: bool,
    cancel: &Cancel,
    report: &(dyn Fn(usize, usize) + Sync),
) -> RemoveReport {
    let mut rep = RemoveReport::default();
    let total = items.len();
    for (i, (repo, path)) in items.iter().enumerate() {
        if cancel.is_cancelled() {
            rep.cancelled = true;
            break;
        }
        report(i, total);
        match remove(repo, path, to_trash, cancel) {
            Ok(done) => {
                rep.unpruned += usize::from(!done.pruned);
                rep.removed.push(path.clone());
            }
            Err(why) => rep.failed.push((path.clone(), why)),
        }
    }
    report(rep.removed.len() + rep.failed.len(), total);
    rep
}

/// What stopped a removal, in a sentence.
pub fn blocked_text(reason: Reason) -> &'static str {
    match reason {
        Reason::Dirty => "it has uncommitted changes or untracked files",
        Reason::Unpushed => "it has commits that exist nowhere else",
        Reason::Stashed => "a stash was made on its branch",
        Reason::Locked => "it is locked",
        Reason::InUse => "it is in use",
        _ => "it needs a look first",
    }
}

/// Drop removed worktrees from the results; repositories left empty disappear.
pub fn apply_removed(repos: &mut Vec<RepoWorktrees>, removed: &[PathBuf]) {
    let gone: HashSet<PathBuf> = removed.iter().map(paths::standardize).collect();
    for r in repos.iter_mut() {
        r.worktrees
            .retain(|w| !gone.contains(&paths::standardize(&w.path)));
    }
    repos.retain(|r| !r.worktrees.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DAY: Duration = Duration::from_secs(86_400);

    fn days_ago(now: SystemTime, d: u64) -> Option<SystemTime> {
        Some(now - DAY * d as u32)
    }

    /// A worktree that is clean and has been checked everywhere.
    fn clean(now: SystemTime) -> Facts {
        Facts {
            dir_exists: true,
            parent_exists: true,
            merged: Some(true),
            touched: true,
            last_activity: days_ago(now, 1),
            dirty: Some(false),
            extra_ignored: Some(false),
            unpushed: Some(false),
            stashed: Some(false),
            ..Facts::default()
        }
    }

    #[test]
    fn a_clean_merged_worktree_is_safe() {
        let now = SystemTime::now();
        assert_eq!(classify(&clean(now), now), Some(Reason::Merged));
        assert_eq!(Reason::Merged.level(), Level::Safe);
    }

    #[test]
    fn a_new_worktree_is_not_called_merged() {
        let now = SystemTime::now();
        let new = Facts {
            touched: false,
            last_activity: days_ago(now, 0),
            ..clean(now)
        };
        assert_eq!(classify(&new, now), None, "just created: leave it alone");
        let old_new = Facts {
            last_activity: days_ago(now, QUIET_DAYS + 1),
            ..new
        };
        assert_eq!(classify(&old_new, now), Some(Reason::Merged));
    }

    #[test]
    fn every_kind_of_unsaved_work_blocks() {
        let now = SystemTime::now();
        for (facts, want) in [
            (
                Facts {
                    dirty: Some(true),
                    ..clean(now)
                },
                Reason::Dirty,
            ),
            (
                Facts {
                    unpushed: Some(true),
                    ..clean(now)
                },
                Reason::Unpushed,
            ),
            (
                Facts {
                    stashed: Some(true),
                    ..clean(now)
                },
                Reason::Stashed,
            ),
            (
                Facts {
                    locked: true,
                    ..clean(now)
                },
                Reason::Locked,
            ),
            (
                Facts {
                    in_use: true,
                    ..clean(now)
                },
                Reason::InUse,
            ),
        ] {
            let got = classify(&facts, now).unwrap();
            assert_eq!(got, want);
            assert_eq!(got.level(), Level::Blocked);
        }
    }

    #[test]
    fn a_failed_check_is_never_safe() {
        let now = SystemTime::now();
        for facts in [
            Facts {
                dirty: None,
                ..clean(now)
            },
            Facts {
                unpushed: None,
                ..clean(now)
            },
            Facts {
                stashed: None,
                ..clean(now)
            },
            Facts {
                extra_ignored: None,
                ..clean(now)
            },
        ] {
            let got = classify(&facts, now).unwrap();
            assert_eq!(got, Reason::Unverified);
            assert_eq!(got.level(), Level::Review);
        }
        let unknown_merge = Facts {
            merged: None,
            ..clean(now)
        };
        assert_eq!(classify(&unknown_merge, now), None);
    }

    #[test]
    fn ignored_files_that_are_not_build_output_need_a_look() {
        let now = SystemTime::now();
        let facts = Facts {
            extra_ignored: Some(true),
            ..clean(now)
        };
        assert_eq!(classify(&facts, now), Some(Reason::IgnoredFiles));
    }

    #[test]
    fn old_unmerged_worktrees_are_review_only_and_still_blocked_by_work() {
        let now = SystemTime::now();
        let stale = Facts {
            merged: Some(false),
            touched: true,
            last_activity: days_ago(now, 200),
            ..clean(now)
        };
        assert_eq!(classify(&stale, now), Some(Reason::Stale));
        assert_eq!(Reason::Stale.level(), Level::Review);
        let unpushed = Facts {
            unpushed: Some(true),
            ..stale.clone()
        };
        assert_eq!(classify(&unpushed, now), Some(Reason::Unpushed));
        let active = Facts {
            last_activity: days_ago(now, 3),
            ..stale
        };
        assert_eq!(classify(&active, now), None);
    }

    #[test]
    fn a_missing_folder_is_safe_only_when_its_surroundings_exist() {
        let now = SystemTime::now();
        let gone = Facts {
            dir_exists: false,
            prunable: true,
            parent_exists: true,
            ..Facts::default()
        };
        assert_eq!(classify(&gone, now), Some(Reason::Missing));
        let unplugged = Facts {
            parent_exists: false,
            ..gone.clone()
        };
        assert_eq!(classify(&unplugged, now), Some(Reason::VolumeUnavailable));
        let locked = Facts {
            locked: true,
            prunable: false,
            ..gone
        };
        assert_eq!(classify(&locked, now), Some(Reason::Locked));
    }

    #[test]
    fn porcelain_output_is_parsed() {
        let raw = b"worktree /code/app\0HEAD aaa\0branch refs/heads/main\0\0\
            worktree /code/app/.claude/worktrees/x y\0HEAD bbb\0branch refs/heads/feat/x\0locked why\0\0\
            worktree /gone\0HEAD ccc\0detached\0prunable gitdir file points to non-existent location\0\0\
            worktree /bare\0bare\0\0";
        let list = parse_porcelain(raw);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(
            list[1].path,
            PathBuf::from("/code/app/.claude/worktrees/x y")
        );
        assert_eq!(list[1].branch.as_deref(), Some("feat/x"));
        assert!(list[1].locked && !list[1].prunable);
        assert!(list[2].prunable && list[2].branch.is_none());
        assert!(list[3].bare);
    }

    #[test]
    fn origin_is_named_from_the_location() {
        assert_eq!(
            origin_of(Path::new("/code/app/.claude/worktrees/x")),
            Some("Claude Code")
        );
        assert_eq!(
            origin_of(Path::new(r"C:\code\app\.claude\worktrees\x")),
            Some("Claude Code")
        );
        assert_eq!(origin_of(Path::new("/code/app-feature")), None);
    }

    #[test]
    fn removed_worktrees_leave_the_results() {
        let wt = |p: &str| Worktree {
            path: PathBuf::from(p),
            repo: PathBuf::from("/r"),
            branch: None,
            reason: Reason::Merged,
            size: 1,
            last_activity: None,
            origin: None,
        };
        let mut repos = vec![RepoWorktrees {
            repo: PathBuf::from("/r"),
            name: "r".into(),
            worktrees: vec![wt("/r/a"), wt("/r/b")],
        }];
        apply_removed(&mut repos, &[PathBuf::from("/r/a")]);
        assert_eq!(repos[0].worktrees.len(), 1);
        apply_removed(&mut repos, &[PathBuf::from("/r/b")]);
        assert!(repos.is_empty());
    }

    // MARK: Real repositories

    const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let bin = git_binary().expect("git");
        let mut cmd = Command::new(bin);
        no_window(&mut cmd);
        let out = cmd
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.autocrlf=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "protocol.file.allow=always",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A folder name made into a valid branch name.
    fn branch_of(name: &str) -> String {
        name.replace(' ', "-")
    }

    struct Repo {
        base: PathBuf,
        repo: PathBuf,
    }

    impl Repo {
        /// A clone of a local "remote" with one commit on `main`, pushed.
        fn new(tag: &str) -> Option<Self> {
            git_binary()?;
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let base = fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("nera-wt-{tag}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            let origin = base.join("origin.git");
            fs::create_dir_all(&origin).unwrap();
            run_git(&origin, &["init", "-q", "--bare"]);
            let repo = base.join("work").join("app");
            fs::create_dir_all(&repo).unwrap();
            run_git(&repo, &["init", "-q"]);
            run_git(
                &repo,
                &["remote", "add", "origin", &origin.to_string_lossy()],
            );
            let me = Self { base, repo };
            me.write(&me.repo, "README.md", "hello\n");
            run_git(&me.repo, &["add", "-A"]);
            run_git(&me.repo, &["commit", "-q", "-m", "first"]);
            run_git(&me.repo, &["push", "-q", "-u", "origin", "main"]);
            run_git(&me.repo, &["remote", "set-head", "origin", "main"]);
            Some(me)
        }

        fn write(&self, dir: &Path, rel: &str, text: &str) {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut f = fs::File::create(&p).unwrap();
            std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }

        /// `<base>/work/<name>` as a new worktree on a new branch.
        fn add(&self, name: &str) -> PathBuf {
            let wt = self.base.join("work").join(name);
            run_git(
                &self.repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &branch_of(name),
                    &wt.to_string_lossy(),
                ],
            );
            fs::canonicalize(&wt).unwrap()
        }

        /// Commit a file in `wt`, then merge its branch into main and push.
        fn merge(&self, wt: &Path, name: &str) {
            self.write(wt, &format!("{name}.txt"), "work\n");
            run_git(wt, &["add", "-A"]);
            run_git(wt, &["commit", "-q", "-m", name]);
            run_git(
                &self.repo,
                &["merge", "-q", "--no-ff", "-m", "merge", &branch_of(name)],
            );
            run_git(&self.repo, &["push", "-q", "origin", "main"]);
        }

        fn entry(&self, wt: &Path) -> Listed {
            let listed = list(&self.repo).unwrap();
            let want = fs::canonicalize(wt).unwrap_or_else(|_| wt.to_path_buf());
            listed
                .into_iter()
                .skip(1)
                .find(|e| {
                    fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone()) == want
                        || paths::standardize(&e.path) == paths::standardize(wt)
                })
                .expect("worktree is listed")
        }

        fn assess(&self, wt: &Path) -> (Facts, Option<Reason>) {
            let entry = self.entry(wt);
            let default = default_ref(&self.repo);
            let stashes = stash_subjects(&self.repo);
            assess(
                &self.repo,
                &entry,
                default.as_deref(),
                stashes.as_deref(),
                &[],
                SystemTime::now(),
            )
        }

        fn verdict(&self, wt: &Path) -> Option<Reason> {
            self.assess(wt).1
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn default_branch_comes_from_the_remote() {
        let Some(r) = Repo::new("default") else {
            return;
        };
        assert_eq!(default_ref(&r.repo).as_deref(), Some("origin/main"));
    }

    #[test]
    fn a_merged_and_pushed_worktree_is_safe_and_removable() {
        let Some(r) = Repo::new("merged") else { return };
        let wt = r.add("feat-a");
        assert_eq!(r.verdict(&wt), None, "just created, nothing done yet");
        r.merge(&wt, "feat-a");
        assert_eq!(r.verdict(&wt), Some(Reason::Merged));

        let cancel = Cancel::new();
        let done = remove(&r.repo, &wt, false, &cancel).expect("removed");
        assert!(done.pruned);
        assert!(!wt.exists());
        let after = list(&r.repo).unwrap();
        assert_eq!(after.len(), 1, "only the main worktree is left: {after:?}");
        assert!(r.repo.join("README.md").is_file());
    }

    #[test]
    fn uncommitted_and_untracked_work_blocks_removal() {
        let Some(r) = Repo::new("dirty") else { return };
        let wt = r.add("feat-b");
        r.merge(&wt, "feat-b");
        r.write(&wt, "scratch.txt", "not committed\n");
        assert_eq!(r.verdict(&wt), Some(Reason::Dirty));
        let err = remove(&r.repo, &wt, false, &Cancel::new()).unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
        assert!(wt.join("scratch.txt").is_file(), "nothing was deleted");

        fs::remove_file(wt.join("scratch.txt")).unwrap();
        r.write(&wt, "feat-b.txt", "changed\n");
        assert_eq!(r.verdict(&wt), Some(Reason::Dirty), "tracked change");
    }

    #[test]
    fn ignored_files_are_kept_unless_they_are_build_output() {
        let Some(r) = Repo::new("ignored") else {
            return;
        };
        r.write(&r.repo, ".gitignore", "node_modules/\n.env\n");
        run_git(&r.repo, &["add", "-A"]);
        run_git(&r.repo, &["commit", "-q", "-m", "ignore"]);
        run_git(&r.repo, &["push", "-q", "origin", "main"]);
        let wt = r.add("feat-c");
        r.merge(&wt, "feat-c");
        r.write(&wt, "node_modules/pkg/index.js", "x\n");
        assert_eq!(r.verdict(&wt), Some(Reason::Merged), "build output only");
        r.write(&wt, ".env", "SECRET=1\n");
        assert_eq!(r.verdict(&wt), Some(Reason::IgnoredFiles));
    }

    #[test]
    fn commits_that_exist_nowhere_else_are_unpushed() {
        let Some(r) = Repo::new("unpushed") else {
            return;
        };
        let wt = r.add("feat-d");
        r.write(&wt, "d.txt", "work\n");
        run_git(&wt, &["add", "-A"]);
        run_git(&wt, &["commit", "-q", "-m", "d"]);
        let (facts, _) = r.assess(&wt);
        assert_eq!(facts.unpushed, Some(true));
        assert_eq!(facts.merged, Some(false));
        run_git(&wt, &["push", "-q", "-u", "origin", "feat-d"]);
        let (facts, _) = r.assess(&wt);
        assert_eq!(facts.unpushed, Some(false), "now on the remote");
    }

    #[test]
    fn a_stash_on_the_branch_blocks() {
        let Some(r) = Repo::new("stash") else { return };
        let wt = r.add("feat-e");
        r.merge(&wt, "feat-e");
        r.write(&wt, "feat-e.txt", "changed\n");
        run_git(&wt, &["stash", "push", "-q", "-m", "wip"]);
        assert_eq!(r.verdict(&wt), Some(Reason::Stashed));
    }

    #[test]
    fn a_locked_worktree_blocks() {
        let Some(r) = Repo::new("locked") else { return };
        let wt = r.add("feat-f");
        r.merge(&wt, "feat-f");
        run_git(&r.repo, &["worktree", "lock", &wt.to_string_lossy()]);
        assert_eq!(r.verdict(&wt), Some(Reason::Locked));
        assert!(remove(&r.repo, &wt, false, &Cancel::new()).is_err());
        assert!(wt.is_dir());
    }

    #[test]
    fn a_deleted_folder_is_pruned() {
        let Some(r) = Repo::new("missing") else {
            return;
        };
        let wt = r.add("feat-g");
        fs::remove_dir_all(&wt).unwrap();
        let entry = r.entry(&wt);
        assert!(entry.prunable, "git reports it as prunable");
        assert_eq!(r.verdict(&wt), Some(Reason::Missing));
        let done = remove(&r.repo, &wt, false, &Cancel::new()).expect("pruned");
        assert!(done.pruned);
        assert_eq!(list(&r.repo).unwrap().len(), 1);
    }

    #[test]
    fn paths_with_spaces_and_accents_work() {
        let Some(r) = Repo::new("spaces") else { return };
        let wt = r.add("Dự án mới");
        r.merge(&wt, "Dự án mới");
        assert_eq!(r.verdict(&wt), Some(Reason::Merged));
        remove(&r.repo, &wt, false, &Cancel::new()).expect("removed");
        assert!(!wt.exists());
    }

    #[test]
    fn the_main_working_copy_is_never_removable() {
        let Some(r) = Repo::new("main") else { return };
        let wt = r.add("feat-h");
        r.merge(&wt, "feat-h");
        let err = remove(&r.repo, &r.repo, false, &Cancel::new()).unwrap_err();
        assert!(err.contains("linked worktree"), "{err}");
        assert!(r.repo.join(".git").is_dir());
    }

    #[test]
    fn a_path_that_is_not_a_worktree_is_refused() {
        let Some(r) = Repo::new("stranger") else {
            return;
        };
        let other = r.base.join("work").join("not-a-worktree");
        fs::create_dir_all(&other).unwrap();
        r.write(&other, "keep.txt", "mine\n");
        assert!(remove(&r.repo, &other, false, &Cancel::new()).is_err());
        assert!(other.join("keep.txt").is_file());
    }

    #[test]
    fn scan_finds_repos_and_lists_only_candidates() {
        let Some(r) = Repo::new("scan") else { return };
        let merged = r.add("feat-i");
        r.merge(&merged, "feat-i");
        let busy = r.add("feat-j");
        r.write(&busy, "wip.txt", "x\n");
        let _fresh = r.add("feat-k");

        let roots = vec![r.base.join("work")];
        let cancel = Cancel::new();
        let repos = find_repos(&roots, &[], &cancel);
        assert_eq!(
            repos,
            vec![r.repo.clone()],
            "linked worktrees are not repos"
        );

        let found = scan(&repos, &roots, &[], &cancel, &|_, _| {});
        assert!(!found.git_missing);
        assert_eq!(found.repos.len(), 1);
        let rows = &found.repos[0].worktrees;
        assert_eq!(rows.len(), 1, "only the merged one: {rows:?}");
        assert_eq!(rows[0].reason, Reason::Merged);
        assert_eq!(rows[0].branch.as_deref(), Some("feat-i"));
        assert!(rows[0].size > 0);

        let outside = vec![r.base.join("elsewhere")];
        assert!(scan(&repos, &outside, &[], &cancel, &|_, _| {})
            .repos
            .is_empty());
        let skipped = vec![merged.clone()];
        assert!(scan(&repos, &roots, &skipped, &cancel, &|_, _| {})
            .repos
            .is_empty());
    }

    #[test]
    fn excluded_folders_are_not_searched_for_repos() {
        let Some(r) = Repo::new("excluded") else {
            return;
        };
        let roots = vec![r.base.join("work")];
        let skipped = vec![r.repo.clone()];
        assert!(find_repos(&roots, &skipped, &Cancel::new()).is_empty());
    }
}
