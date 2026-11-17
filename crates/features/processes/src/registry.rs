//! Launch Services' view of running applications, read with Apple's own
//! `lsappinfo list` tool. It is the source of truth for "is this a regular
//! app" (Activity Monitor's Applications filter and the Dock use the same
//! registry) and for the app's display name.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppKind {
    /// Regular, Dock-visible app.
    Foreground,
    /// Menu-bar extra / agent (`LSUIElement`).
    UiElement,
    /// Faceless (`LSBackgroundOnly`, XPC services, headless runs).
    BackgroundOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredApp {
    pub name: String,
    pub bundle_id: Option<String>,
    pub bundle_path: Option<PathBuf>,
    pub kind: AppKind,
}

/// Registered apps by pid; `None` when `lsappinfo` is unavailable or its
/// output isn't understood (callers must then not claim exact app counts).
pub fn query() -> Option<HashMap<i32, RegisteredApp>> {
    let out = Command::new("/usr/bin/lsappinfo")
        .arg("list")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let map = parse(&String::from_utf8_lossy(&out.stdout));
    // A Mac always has some registered apps (loginwindow at least).
    (!map.is_empty()).then_some(map)
}

/// Parses `lsappinfo list`: blocks that start with ` N) "Name" ASN:...:` and
/// carry `bundleID=`, `bundle path=` and `pid = N ... type="Foreground"`.
pub fn parse(text: &str) -> HashMap<i32, RegisteredApp> {
    #[derive(Default)]
    struct Block {
        name: Option<String>,
        bundle_id: Option<String>,
        bundle_path: Option<PathBuf>,
        pid: Option<i32>,
        kind: Option<AppKind>,
    }
    fn flush(b: Block, out: &mut HashMap<i32, RegisteredApp>) {
        if let (Some(name), Some(pid), Some(kind)) = (b.name, b.pid, b.kind) {
            out.insert(
                pid,
                RegisteredApp {
                    name,
                    bundle_id: b.bundle_id,
                    bundle_path: b.bundle_path,
                    kind,
                },
            );
        }
    }
    let quoted = |s: &str, key: &str| -> Option<String> {
        let rest = s.strip_prefix(key)?.strip_prefix('"')?;
        Some(rest[..rest.rfind('"')?].to_string())
    };
    let mut out = HashMap::new();
    let mut cur = Block::default();
    for line in text.lines() {
        let t = line.trim();
        if let Some((head, rest)) = t.split_once(") \"") {
            if head.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() {
                if let Some(end) = rest.find("\" ASN:") {
                    flush(std::mem::take(&mut cur), &mut out);
                    cur.name = Some(rest[..end].to_string());
                    continue;
                }
            }
        }
        if let Some(id) = quoted(t, "bundleID=") {
            cur.bundle_id = Some(id);
        } else if let Some(path) = quoted(t, "bundle path=") {
            cur.bundle_path = Some(PathBuf::from(path));
        } else if let Some(rest) = t.strip_prefix("pid = ") {
            cur.pid = rest.split_whitespace().next().and_then(|n| n.parse().ok());
            cur.kind = rest
                .split_once("type=\"")
                .and_then(|(_, r)| r.split('"').next())
                .and_then(|k| match k {
                    "Foreground" => Some(AppKind::Foreground),
                    "UIElement" => Some(AppKind::UiElement),
                    "BackgroundOnly" => Some(AppKind::BackgroundOnly),
                    _ => None,
                });
        }
    }
    flush(cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#" 1) "ViewBridgeAuxiliary" ASN:0x0-0x2002: 
    bundleID="com.apple.ViewBridgeAuxiliary"
    bundle path="/System/Library/PrivateFrameworks/ViewBridge.framework/Versions/A/XPCServices/ViewBridgeAuxiliary.xpc"
    pid = 529 token=[sess=100021 pid=529 uid:0,0,0 g:0,0 pV:1226] !signalled type="BackgroundOnly" flavor=2 Version=[ NULL ]  fileType="XPC!" creator="????" Arch=ARM64 
 2) "loginwindow" ASN:0x0-0x3003: 
    bundleID="com.apple.loginwindow"
    bundle path="/System/Library/CoreServices/loginwindow.app"
    pid = 416 token=[sess=100020 pid=416 uid:0,0,4294967295 g:0,0 pV:939] type="UIElement" flavor=3 Version="3109" fileType="APPL" creator="lgnw" Arch=ARM64 
 3) "Activity Monitor" ASN:0x0-0x1d01d: 
    bundleID="com.apple.ActivityMonitor"
    bundle path="/System/Applications/Utilities/Activity Monitor.app"
    pid = 1299 token=[sess=100020 pid=1299 uid:501,501,501 g:20,20 pV:1] type="Foreground" flavor=3 Version="1.0" fileType="APPL" creator="actm" Arch=ARM64 
 4) "Docker Desktop" ASN:0x0-0xdddddd: 
    bundleID=[ NULL ] 
    bundle path="/Applications/Docker.app/Contents/MacOS/Docker Desktop.app"
    pid = 65011 token=[sess=1 pid=65011 uid:501,501,501] type="Foreground" flavor=3
 5) "Not running yet" ASN:0x0-0x1: 
    bundleID="x.y"
"#;

    #[test]
    fn parses_pid_name_kind_and_bundle() {
        let m = parse(SAMPLE);
        assert_eq!(m.len(), 4, "the entry without a pid is skipped");
        let am = &m[&1299];
        assert_eq!(am.name, "Activity Monitor");
        assert_eq!(am.kind, AppKind::Foreground);
        assert_eq!(am.bundle_id.as_deref(), Some("com.apple.ActivityMonitor"));
        assert_eq!(
            am.bundle_path.as_deref(),
            Some(std::path::Path::new(
                "/System/Applications/Utilities/Activity Monitor.app"
            ))
        );
        assert_eq!(m[&416].kind, AppKind::UiElement);
        assert_eq!(m[&529].kind, AppKind::BackgroundOnly);
        assert_eq!(m[&65011].bundle_id, None);
        assert_eq!(m[&65011].name, "Docker Desktop");
    }

    #[test]
    fn garbage_parses_to_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("lsappinfo: error").is_empty());
    }
}
