//! Windows maintenance tasks. Only Flush DNS is offered: `ipconfig /flushdns`
//! is the tool Windows itself documents for it and reports failure through
//! its exit code. The macOS tasks (purge, Spotlight, Launch Services,
//! periodic scripts, Quick Look) have no honest Windows counterpart and are
//! hidden. Re-exported by `maintenance`.
//!
//! Recent Windows 11 builds only let administrators flush the DNS cache
//! (`ipconfig` then exits with 1: "The requested operation requires
//! elevation"). A task therefore runs unelevated first and, when that fails,
//! once more through the UAC prompt (`ShellExecuteExW` with the `runas`
//! verb). Dismissing the prompt is reported as cancelled; the elevated run's
//! exit code decides success.

use super::maintenance::{success_message, ExecOutput, Invocation, RunError, TaskId};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `CREATE_NO_WINDOW`: console tools run without flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl TaskId {
    /// Tasks that may need administrator approval (a UAC prompt) on this PC.
    pub fn needs_admin(self) -> bool {
        self == TaskId::FlushDns
    }

    pub fn confirm_message(self) -> String {
        match self {
            TaskId::FlushDns => "Clears the cached domain lookups of the Windows DNS Client, so the next visit to each site asks your DNS server again. Open connections are not affected. Windows asks for administrator approval if this PC requires it.".into(),
            // Not offered on Windows (see `is_available`).
            other => format!("{}.", other.description()),
        }
    }

    /// Program the task runs, or `None` when Windows has no equivalent.
    fn program(self) -> Option<PathBuf> {
        match self {
            TaskId::FlushDns => Some(system32().join("ipconfig.exe")),
            _ => None,
        }
    }

    pub fn is_available_with(self, exists: impl Fn(&Path) -> bool) -> bool {
        self.program().is_some_and(|p| exists(&p))
    }

    pub fn is_available(self) -> bool {
        self.is_available_with(Path::exists)
    }
}

/// `C:\Windows\System32` (from `GetSystemDirectoryW`), so tools are started
/// by absolute path and never picked up from the working directory or PATH.
pub fn system32() -> PathBuf {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the buffer length is passed; the call writes at most that many
    // UTF-16 units and returns the length written (0 on failure).
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n > 0 && n < buf.len() {
        PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
    } else {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join("System32")
    }
}

/// Processes to spawn for a task (empty when it is not offered on Windows).
pub fn plan(id: TaskId) -> Vec<Invocation> {
    match (id, id.program()) {
        (TaskId::FlushDns, Some(program)) => vec![Invocation {
            program: program.to_string_lossy().into_owned(),
            args: vec!["/flushdns".into()],
        }],
        _ => Vec::new(),
    }
}

/// How to start a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    /// As the current user, output captured.
    No,
    /// Through the UAC prompt; only the exit code comes back.
    Uac,
}

/// Spawn without a console window and wait. Blocking; call from a background
/// executor.
pub fn execute(inv: &Invocation) -> std::io::Result<ExecOutput> {
    let out = Command::new(&inv.program)
        .args(&inv.args)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    Ok(ExecOutput {
        success: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// `ShellExecuteExW` with `verb` (`Some("runas")` shows the UAC prompt), the
/// window hidden, then wait for the process and read its exit code. The
/// prompt being dismissed comes back as an error with raw OS code
/// `ERROR_CANCELLED` (1223).
pub fn shell_execute_wait(inv: &Invocation, verb: Option<&str>) -> std::io::Result<ExecOutput> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let file = wide(&inv.program);
    let params = wide(&join_args(&inv.args));
    let verb = verb.map(wide);
    // SAFETY: all-zero is a valid SHELLEXECUTEINFOW (null pointers/handles).
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: `info` and the NUL-terminated strings it points to outlive the
    // call; on success `hProcess` is a handle we own and close below.
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if info.hProcess.is_null() {
            return Err(std::io::Error::other(
                "Windows did not return a process handle",
            ));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        let err = std::io::Error::last_os_error();
        CloseHandle(info.hProcess);
        if ok == 0 {
            return Err(err);
        }
        Ok(ExecOutput {
            success: code == 0,
            code: Some(code as i32),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// [`execute`] or, for [`Elevation::Uac`], [`shell_execute_wait`] with `runas`.
pub fn execute_as(inv: &Invocation, elevation: Elevation) -> std::io::Result<ExecOutput> {
    match elevation {
        Elevation::No => execute(inv),
        Elevation::Uac => shell_execute_wait(inv, Some("runas")),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Command line for `lpParameters`; arguments with spaces or quotes are quoted
/// the way the C runtime parses them back.
fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '\t', '"']) {
                a.clone()
            } else {
                format!("\"{}\"", a.replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Last non-empty line: `ipconfig` prints a "Windows IP Configuration" header
/// first and the reason ("Could not flush the DNS Resolver Cache: …") last.
fn last_line(s: &str) -> &str {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(s)
        .trim()
}

/// Map a failed process to a [`RunError`].
pub fn classify_failure(inv: &Invocation, out: &ExecOutput) -> RunError {
    let name = Path::new(&inv.program)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(&inv.program)
        .to_string();
    let detail = if !out.stderr.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    match (detail.is_empty(), out.code) {
        (true, Some(c)) => RunError::Failed(format!("{name} exited with code {c}.")),
        (true, None) => RunError::Failed(format!("{name} was interrupted.")),
        (false, _) => RunError::Failed(format!("{name}: {}", last_line(detail))),
    }
}

/// `ERROR_CANCELLED`: the user dismissed the UAC prompt.
const ERROR_CANCELLED: i32 = 1223;

/// Run a task's plan through `exec` (injected for tests): each command runs
/// unelevated first and, for tasks that may need it, again through UAC when
/// that fails.
pub fn run_with(
    id: TaskId,
    exec: &mut dyn FnMut(&Invocation, Elevation) -> std::io::Result<ExecOutput>,
) -> Result<(), RunError> {
    let plan = plan(id);
    if plan.is_empty() {
        return Err(RunError::Failed(format!(
            "{} isn't available on Windows.",
            id.title()
        )));
    }
    let start_failed = |inv: &Invocation, e: std::io::Error| {
        RunError::Failed(format!("Couldn't start {}: {e}", inv.program))
    };
    for inv in plan {
        let out = exec(&inv, Elevation::No).map_err(|e| start_failed(&inv, e))?;
        if out.success {
            continue;
        }
        if !id.needs_admin() {
            return Err(classify_failure(&inv, &out));
        }
        let elevated = match exec(&inv, Elevation::Uac) {
            Ok(out) => out,
            Err(e) if e.raw_os_error() == Some(ERROR_CANCELLED) => return Err(RunError::Cancelled),
            Err(e) => return Err(start_failed(&inv, e)),
        };
        if !elevated.success {
            return Err(classify_failure(&inv, &elevated));
        }
    }
    Ok(())
}

/// Run a task for real and return the success message. Blocking.
pub fn run(id: TaskId) -> Result<String, RunError> {
    run_with(id, &mut execute_as)?;
    Ok(success_message(id, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::maintenance::available_tasks;

    fn out(success: bool, code: i32, stdout: &str) -> ExecOutput {
        ExecOutput {
            success,
            code: Some(code),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    const NEEDS_ELEVATION: &str = "The requested operation requires elevation.";

    #[test]
    fn only_flush_dns_is_offered() {
        for t in TaskId::ALL {
            assert_eq!(
                t.is_available_with(|_| true),
                t == TaskId::FlushDns,
                "{t:?}"
            );
            assert!(!t.is_available_with(|_| false));
        }
        // Read-only probe of this PC: ipconfig ships with every Windows.
        assert_eq!(available_tasks(), vec![TaskId::FlushDns]);
    }

    #[test]
    fn flush_dns_runs_ipconfig_from_system32() {
        let p = plan(TaskId::FlushDns);
        assert_eq!(p.len(), 1);
        let program = Path::new(&p[0].program);
        assert!(program.is_absolute(), "{program:?}");
        // Windows reports the folder in its own casing (often `system32`).
        let lower = program.to_string_lossy().to_ascii_lowercase();
        assert!(lower.ends_with(r"\system32\ipconfig.exe"), "{program:?}");
        assert_eq!(p[0].args, vec!["/flushdns".to_string()]);
        assert!(plan(TaskId::FreeRam).is_empty());
        assert!(system32().join("ipconfig.exe").exists());
    }

    #[test]
    fn copy_never_mentions_macos_or_passwords() {
        let msg = TaskId::FlushDns.confirm_message();
        assert!(msg.ends_with('.'));
        assert!(msg.contains("administrator approval"));
        for word in ["macOS", "Mac", "password", "responder"] {
            assert!(!msg.contains(word), "{word} in {msg}");
        }
    }

    #[test]
    fn quotes_parameters() {
        assert_eq!(join_args(&["/flushdns".into()]), "/flushdns");
        assert_eq!(join_args(&["a b".into(), "".into()]), "\"a b\" \"\"");
        assert_eq!(join_args(&["say \"hi\"".into()]), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn unelevated_success_needs_no_prompt() {
        let mut calls = Vec::new();
        let res = run_with(TaskId::FlushDns, &mut |inv, e| {
            calls.push((inv.clone(), e));
            Ok(out(
                true,
                0,
                "Windows IP Configuration\r\n\r\nSuccessfully flushed the DNS Resolver Cache.",
            ))
        });
        assert_eq!(res, Ok(()));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, Elevation::No);
    }

    #[test]
    fn falls_back_to_uac_and_reports_its_result() {
        let mut modes = Vec::new();
        let res = run_with(TaskId::FlushDns, &mut |_, e| {
            modes.push(e);
            Ok(match e {
                Elevation::No => out(false, 1, NEEDS_ELEVATION),
                Elevation::Uac => out(true, 0, ""),
            })
        });
        assert_eq!(res, Ok(()));
        assert_eq!(modes, vec![Elevation::No, Elevation::Uac]);

        let failed = run_with(TaskId::FlushDns, &mut |_, e| {
            Ok(match e {
                Elevation::No => out(false, 1, NEEDS_ELEVATION),
                Elevation::Uac => out(false, 1, ""),
            })
        });
        assert_eq!(
            failed,
            Err(RunError::Failed("ipconfig exited with code 1.".into()))
        );
    }

    #[test]
    fn dismissed_uac_prompt_is_cancelled() {
        let res = run_with(TaskId::FlushDns, &mut |_, e| match e {
            Elevation::No => Ok(out(false, 1, NEEDS_ELEVATION)),
            Elevation::Uac => Err(std::io::Error::from_raw_os_error(ERROR_CANCELLED)),
        });
        assert_eq!(res, Err(RunError::Cancelled));
        let other = run_with(TaskId::FlushDns, &mut |_, e| match e {
            Elevation::No => Ok(out(false, 1, NEEDS_ELEVATION)),
            Elevation::Uac => Err(std::io::Error::from_raw_os_error(2)),
        });
        assert!(matches!(other, Err(RunError::Failed(m)) if m.starts_with("Couldn't start")));
    }

    #[test]
    fn classifies_failures_and_never_spawns_unoffered_tasks() {
        let inv = &plan(TaskId::FlushDns)[0];
        assert_eq!(
            classify_failure(
                inv,
                &out(
                    false,
                    1,
                    "Windows IP Configuration\r\n\r\nCould not flush the DNS Resolver Cache: Function failed during execution."
                )
            ),
            RunError::Failed(
                "ipconfig: Could not flush the DNS Resolver Cache: Function failed during execution."
                    .into()
            )
        );
        let spawn_err = run_with(TaskId::FlushDns, &mut |_, _| {
            Err(std::io::Error::other("nope"))
        });
        assert!(matches!(spawn_err, Err(RunError::Failed(m)) if m.contains("nope")));
        let mut spawned = false;
        assert!(run_with(TaskId::RebuildSpotlight, &mut |_, _| {
            spawned = true;
            Ok(out(true, 0, ""))
        })
        .is_err());
        assert!(!spawned);
    }

    /// The ShellExecuteEx path (minus the `runas` verb, which would show a UAC
    /// prompt) returns the real exit code of a hidden process.
    #[test]
    fn shell_execute_wait_returns_the_exit_code() {
        let cmd = Invocation {
            program: system32().join("cmd.exe").to_string_lossy().into_owned(),
            args: vec!["/d".into(), "/c".into(), "exit".into(), "3".into()],
        };
        let out = shell_execute_wait(&cmd, None).unwrap();
        assert_eq!((out.success, out.code), (false, Some(3)));
        let ok = Invocation {
            args: vec!["/d".into(), "/c".into(), "exit".into(), "0".into()],
            ..cmd
        };
        assert!(shell_execute_wait(&ok, None).unwrap().success);
    }
}
