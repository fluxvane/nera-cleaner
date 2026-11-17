//! macOS maintenance tasks (Free up RAM, Flush DNS, Spotlight, Launch
//! Services, periodic scripts, Quick Look). Each task is a fixed list of
//! commands with absolute program paths; tasks that need root run through
//! `osascript … with administrator privileges`, so macOS shows its own
//! password prompt. Nothing here deletes files. Re-exported by `maintenance`.

use super::maintenance::{success_message, ExecOutput, Invocation, RunError, TaskId};
use super::stats;
use std::path::Path;
use std::process::{Command, Stdio};

pub const PURGE: &str = "/usr/sbin/purge";
pub const DSCACHEUTIL: &str = "/usr/bin/dscacheutil";
pub const KILLALL: &str = "/usr/bin/killall";
pub const MDUTIL: &str = "/usr/bin/mdutil";
pub const LSREGISTER: &str =
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
pub const PERIODIC: &str = "/usr/sbin/periodic";
pub const QLMANAGE: &str = "/usr/bin/qlmanage";
pub const OSASCRIPT: &str = "/usr/bin/osascript";

impl TaskId {
    pub fn needs_admin(self) -> bool {
        matches!(
            self,
            TaskId::FreeRam | TaskId::FlushDns | TaskId::RebuildSpotlight | TaskId::PeriodicScripts
        )
    }

    pub fn confirm_message(self) -> String {
        let body = match self {
            TaskId::FreeRam => {
                "Runs macOS's purge command, which drops cached files from memory. Apps may feel slower for a moment while the cache refills."
            }
            TaskId::FlushDns => {
                "Clears cached domain lookups and restarts the DNS responder. Open connections are not affected."
            }
            TaskId::RebuildSpotlight => {
                "Erases the Spotlight index of your startup disk and rebuilds it from scratch. Indexing runs in the background and can take several hours; until it finishes, search results are incomplete and your Mac may feel busier."
            }
            TaskId::ReindexLaunchServices => {
                "Re-registers the apps on this Mac to fix duplicate entries in “Open With” menus and wrong default apps. This can take a minute."
            }
            TaskId::PeriodicScripts => {
                "Runs the daily, weekly and monthly maintenance scripts that rotate logs and remove old temporary files."
            }
            TaskId::ResetQuickLook => {
                "Clears cached file previews and thumbnails and reloads preview plug-ins. Thumbnails are recreated as you browse."
            }
        };
        if self.needs_admin() {
            format!("{body} macOS will ask for an administrator password.")
        } else {
            format!("{body} No password needed.")
        }
    }

    /// Text in the macOS administrator prompt.
    pub fn auth_prompt(self) -> String {
        let what = match self {
            TaskId::FreeRam => "free up memory",
            TaskId::FlushDns => "flush the DNS cache",
            TaskId::RebuildSpotlight => "rebuild the Spotlight index",
            TaskId::PeriodicScripts => "run the system maintenance scripts",
            TaskId::ReindexLaunchServices => "reindex Launch Services",
            TaskId::ResetQuickLook => "reset the Quick Look cache",
        };
        format!("Nera Cleaner wants to {what}.")
    }

    /// Programs the task needs; a task whose tools are missing on this
    /// macOS version is hidden (e.g. `periodic` was removed in macOS 15).
    fn required_programs(self) -> &'static [&'static str] {
        match self {
            TaskId::FreeRam => &[PURGE],
            TaskId::FlushDns => &[DSCACHEUTIL, KILLALL],
            TaskId::RebuildSpotlight => &[MDUTIL],
            TaskId::ReindexLaunchServices => &[LSREGISTER],
            TaskId::PeriodicScripts => &[PERIODIC],
            TaskId::ResetQuickLook => &[QLMANAGE],
        }
    }

    pub fn is_available_with(self, exists: impl Fn(&str) -> bool) -> bool {
        let needs_osascript = self.needs_admin();
        self.required_programs().iter().all(|p| exists(p))
            && (!needs_osascript || exists(OSASCRIPT))
    }

    pub fn is_available(self) -> bool {
        self.is_available_with(|p| Path::new(p).exists())
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `lsregister` arguments. Older systems support the classic `-kill … -domain`
/// rebuild; newer ones (no `-kill` in `-h`) re-register every domain with
/// `-all` and garbage-collect stale records.
pub fn lsregister_args(help: &str) -> Vec<String> {
    if help.contains("-kill") {
        argv(&[
            "-kill", "-r", "-domain", "local", "-domain", "system", "-domain", "user",
        ])
    } else {
        argv(&["-gc", "-r", "-f", "-all", "local,system,user"])
    }
}

/// The task's commands as argv lists (program first).
pub fn commands(id: TaskId, lsregister_help: &str) -> Vec<Vec<String>> {
    match id {
        TaskId::FreeRam => vec![argv(&[PURGE])],
        TaskId::FlushDns => vec![
            argv(&[DSCACHEUTIL, "-flushcache"]),
            argv(&[KILLALL, "-HUP", "mDNSResponder"]),
        ],
        TaskId::RebuildSpotlight => vec![argv(&[MDUTIL, "-E", "/"])],
        TaskId::ReindexLaunchServices => {
            let mut cmd = argv(&[LSREGISTER]);
            cmd.extend(lsregister_args(lsregister_help));
            vec![cmd]
        }
        TaskId::PeriodicScripts => vec![argv(&[PERIODIC, "daily", "weekly", "monthly"])],
        TaskId::ResetQuickLook => vec![argv(&[QLMANAGE, "-r", "cache"]), argv(&[QLMANAGE, "-r"])],
    }
}

/// Processes to spawn: admin tasks become one `osascript` call running the
/// joined shell line with administrator privileges; others run directly.
pub fn plan(id: TaskId, lsregister_help: &str) -> Vec<Invocation> {
    let cmds = commands(id, lsregister_help);
    if id.needs_admin() {
        let script = admin_script(&shell_line(&cmds), &id.auth_prompt());
        vec![Invocation {
            program: OSASCRIPT.into(),
            args: vec!["-e".into(), script],
        }]
    } else {
        cmds.into_iter()
            .map(|mut c| {
                let program = c.remove(0);
                Invocation { program, args: c }
            })
            .collect()
    }
}

/// Quote one argument for `/bin/sh`.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ',' | '=' | ':')
        });
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Commands joined with `&&` so a failure stops the chain.
pub fn shell_line(cmds: &[Vec<String>]) -> String {
    cmds.iter()
        .map(|c| {
            c.iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" && ")
}

/// AppleScript string literal.
pub fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn admin_script(shell: &str, prompt: &str) -> String {
    format!(
        "do shell script {} with prompt {} with administrator privileges",
        applescript_string(shell),
        applescript_string(prompt)
    )
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Spawn and wait. Blocking; call from a background executor.
pub fn execute(inv: &Invocation) -> std::io::Result<ExecOutput> {
    let out = Command::new(&inv.program)
        .args(&inv.args)
        .stdin(Stdio::null())
        .output()?;
    Ok(ExecOutput {
        success: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Split an `osascript` error line such as
/// `0:84: execution error: User canceled. (-128)` into code and message.
pub fn parse_osascript_error(stderr: &str) -> (Option<i32>, String) {
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let body = line
        .split_once("execution error: ")
        .map(|(_, b)| b)
        .unwrap_or(line);
    if let Some(open) = body.rfind(" (") {
        if let Some(code) = body[open + 2..]
            .strip_suffix(')')
            .and_then(|c| c.parse::<i32>().ok())
        {
            return (Some(code), body[..open].trim().to_string());
        }
    }
    (None, body.to_string())
}

/// Map a failed process to a [`RunError`].
pub fn classify_failure(inv: &Invocation, out: &ExecOutput) -> RunError {
    let name = Path::new(&inv.program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&inv.program)
        .to_string();
    if inv.program == OSASCRIPT {
        let (code, msg) = parse_osascript_error(&out.stderr);
        return match code {
            Some(-128) | Some(-60006) => RunError::Cancelled,
            Some(-60005) | Some(-60007) | Some(-60008) => {
                RunError::NotAuthorized(if msg.is_empty() {
                    "This account can't approve administrator tasks.".into()
                } else {
                    msg
                })
            }
            _ if msg.is_empty() => RunError::Failed(format!(
                "The command exited with code {}.",
                code.unwrap_or(-1)
            )),
            _ => RunError::Failed(msg),
        };
    }
    let detail = if !out.stderr.is_empty() {
        out.stderr.clone()
    } else {
        out.stdout.clone()
    };
    match (detail.is_empty(), out.code) {
        (true, Some(c)) => RunError::Failed(format!("{name} exited with code {c}.")),
        (true, None) => RunError::Failed(format!("{name} was interrupted.")),
        (false, _) => RunError::Failed(format!("{name}: {}", first_line(&detail))),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim()
}

/// Commands that exit 0 but report a problem on stdout.
fn check_output(id: TaskId, out: &ExecOutput) -> Option<RunError> {
    let text = out.stdout.to_lowercase();
    match id {
        TaskId::RebuildSpotlight if text.contains("indexing disabled") => Some(RunError::Failed(
            "Spotlight indexing is turned off for the startup disk.".into(),
        )),
        _ => None,
    }
}

/// Run a task's plan through `exec` (injected for tests).
pub fn run_with(
    id: TaskId,
    lsregister_help: &str,
    exec: &mut dyn FnMut(&Invocation) -> std::io::Result<ExecOutput>,
) -> Result<(), RunError> {
    for inv in plan(id, lsregister_help) {
        let out = exec(&inv)
            .map_err(|e| RunError::Failed(format!("Couldn't start {}: {e}", inv.program)))?;
        if !out.success {
            return Err(classify_failure(&inv, &out));
        }
        if let Some(err) = check_output(id, &out) {
            return Err(err);
        }
    }
    Ok(())
}

/// `lsregister -h` output (read-only probe used to pick its arguments).
fn lsregister_help() -> String {
    Command::new(LSREGISTER)
        .arg("-h")
        .stdin(Stdio::null())
        .output()
        .map(|o| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        })
        .unwrap_or_default()
}

/// Run a task for real and return the success message. Blocking.
pub fn run(id: TaskId) -> Result<String, RunError> {
    let help = if id == TaskId::ReindexLaunchServices {
        lsregister_help()
    } else {
        String::new()
    };
    let before = (id == TaskId::FreeRam)
        .then(|| stats::read_memory().ok())
        .flatten();
    run_with(id, &help, &mut execute)?;
    let after = before.and_then(|_| stats::read_memory().ok());
    let free_memory = match (before, after) {
        (Some(b), Some(a)) => Some((b.free, a.free)),
        _ => None,
    };
    Ok(success_message(id, free_memory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::maintenance::available_tasks;

    #[test]
    fn copy_mentions_password_only_for_admin_tasks() {
        for t in TaskId::ALL {
            let msg = t.confirm_message();
            assert_eq!(
                msg.contains("administrator password"),
                t.needs_admin(),
                "{t:?}"
            );
            assert!(t.confirm_title().ends_with('?'));
            assert!(!t.description().is_empty());
        }
        assert!(TaskId::RebuildSpotlight
            .confirm_message()
            .contains("several hours"));
    }

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote("/usr/sbin/purge"), "/usr/sbin/purge");
        assert_eq!(shell_quote("local,system,user"), "local,system,user");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm)"), "'$(rm)'");
    }

    #[test]
    fn escapes_applescript_strings() {
        assert_eq!(
            applescript_string(r#"say "hi" \ bye"#),
            r#""say \"hi\" \\ bye""#
        );
    }

    #[test]
    fn admin_tasks_go_through_osascript() {
        let p = plan(TaskId::FlushDns, "");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].program, OSASCRIPT);
        assert_eq!(p[0].args[0], "-e");
        assert_eq!(
            p[0].args[1],
            "do shell script \"/usr/bin/dscacheutil -flushcache && /usr/bin/killall -HUP mDNSResponder\" \
             with prompt \"Nera Cleaner wants to flush the DNS cache.\" with administrator privileges"
        );
        let purge = plan(TaskId::FreeRam, "");
        assert!(purge[0].args[1].starts_with("do shell script \"/usr/sbin/purge\""));
        let spot = plan(TaskId::RebuildSpotlight, "");
        assert!(spot[0].args[1].contains("/usr/bin/mdutil -E /"));
        let periodic = plan(TaskId::PeriodicScripts, "");
        assert!(periodic[0].args[1].contains("/usr/sbin/periodic daily weekly monthly"));
    }

    #[test]
    fn user_tasks_run_directly() {
        let ql = plan(TaskId::ResetQuickLook, "");
        assert_eq!(
            ql,
            vec![
                Invocation {
                    program: QLMANAGE.into(),
                    args: argv(&["-r", "cache"])
                },
                Invocation {
                    program: QLMANAGE.into(),
                    args: argv(&["-r"])
                },
            ]
        );
        let ls = plan(TaskId::ReindexLaunchServices, "usage: -kill -r -domain");
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].program, LSREGISTER);
        assert_eq!(ls[0].args[0], "-kill");
    }

    #[test]
    fn picks_lsregister_arguments_from_help() {
        assert_eq!(lsregister_args("  -kill   Reset the database")[0], "-kill");
        let modern = lsregister_args("  -delete  ...\n  -gc  Garbage collect\n  -all <domain>");
        assert_eq!(
            modern,
            argv(&["-gc", "-r", "-f", "-all", "local,system,user"])
        );
    }

    #[test]
    fn availability_depends_on_installed_tools() {
        assert!(TaskId::PeriodicScripts.is_available_with(|_| true));
        assert!(!TaskId::PeriodicScripts.is_available_with(|p| p != PERIODIC));
        assert!(!TaskId::FreeRam.is_available_with(|p| p != OSASCRIPT));
        assert!(TaskId::ResetQuickLook.is_available_with(|p| p != OSASCRIPT));
        // Read-only probe of this Mac.
        let live = available_tasks();
        assert_eq!(
            live.contains(&TaskId::PeriodicScripts),
            Path::new(PERIODIC).exists()
        );
    }

    #[test]
    fn parses_osascript_errors() {
        assert_eq!(
            parse_osascript_error("0:84: execution error: User canceled. (-128)"),
            (Some(-128), "User canceled.".into())
        );
        assert_eq!(
            parse_osascript_error("0:120: execution error: mdutil: / (1) error. (1)\n"),
            (Some(1), "mdutil: / (1) error.".into())
        );
        assert_eq!(parse_osascript_error("weird"), (None, "weird".into()));
    }

    fn fail(stderr: &str, code: i32) -> ExecOutput {
        ExecOutput {
            success: false,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    fn ok(stdout: &str) -> ExecOutput {
        ExecOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn classifies_failures() {
        let osa = Invocation {
            program: OSASCRIPT.into(),
            args: vec![],
        };
        assert_eq!(
            classify_failure(
                &osa,
                &fail("0:1: execution error: User canceled. (-128)", 1)
            ),
            RunError::Cancelled
        );
        assert!(matches!(
            classify_failure(&osa, &fail("execution error: Not authorized. (-60005)", 1)),
            RunError::NotAuthorized(_)
        ));
        assert_eq!(
            classify_failure(&osa, &fail("0:1: execution error: purge: failed (1)", 1)),
            RunError::Failed("purge: failed".into())
        );
        let ql = Invocation {
            program: QLMANAGE.into(),
            args: vec![],
        };
        assert_eq!(
            classify_failure(&ql, &fail("", 3)),
            RunError::Failed("qlmanage exited with code 3.".into())
        );
        assert_eq!(
            classify_failure(&ql, &fail("bad thing\nmore", 3)),
            RunError::Failed("qlmanage: bad thing".into())
        );
    }

    #[test]
    fn run_with_stops_at_first_failure_and_never_spawns_for_real() {
        let mut calls = Vec::new();
        let res = run_with(TaskId::ResetQuickLook, "", &mut |inv| {
            calls.push(inv.clone());
            Ok(fail("cache locked", 2))
        });
        assert_eq!(res, Err(RunError::Failed("qlmanage: cache locked".into())));
        assert_eq!(calls.len(), 1);

        let mut n = 0;
        assert_eq!(
            run_with(TaskId::ResetQuickLook, "", &mut |_| {
                n += 1;
                Ok(ok(""))
            }),
            Ok(())
        );
        assert_eq!(n, 2);

        let cancelled = run_with(TaskId::FlushDns, "", &mut |_| {
            Ok(fail("execution error: User canceled. (-128)", 1))
        });
        assert_eq!(cancelled, Err(RunError::Cancelled));

        let spawn_err = run_with(TaskId::FreeRam, "", &mut |_| {
            Err(std::io::Error::other("nope"))
        });
        assert!(matches!(spawn_err, Err(RunError::Failed(m)) if m.contains("nope")));
    }

    #[test]
    fn detects_disabled_spotlight() {
        let res = run_with(TaskId::RebuildSpotlight, "", &mut |_| {
            Ok(ok("/:\n\tIndexing disabled."))
        });
        assert!(matches!(res, Err(RunError::Failed(m)) if m.contains("turned off")));
        assert_eq!(
            run_with(TaskId::RebuildSpotlight, "", &mut |_| Ok(ok(
                "/:\n\tIndexing enabled."
            ))),
            Ok(())
        );
    }
}
