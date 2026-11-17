//! Cloud-drive files that are stored on this Mac (Google Drive, OneDrive,
//! Dropbox, iCloud Drive...). They are NOT junk and are never cleaned: this
//! only measures them, so the user can see how much space "Remove Download"
//! in Finder would give back. Read-only.

use nera_core::fs_util::Cancel;
use nera_core::paths::home_join;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// `SF_DATALESS`: a placeholder whose content (or, for a folder, whose
/// listing) is only in the cloud. Reading it would download it.
const SF_DATALESS: u32 = 0x4000_0000;

/// One cloud drive and the space its downloaded files use on this disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudDrive {
    /// "Google Drive", "OneDrive", "iCloud Drive"...
    pub service: String,
    /// The account or drive name, when the folder name carries one.
    pub account: Option<String>,
    pub path: PathBuf,
    /// Allocated bytes of the files whose content is on this disk.
    pub local_bytes: u64,
    /// Number of files whose content is on this disk.
    pub local_files: u64,
}

/// Every cloud drive in `~/Library/CloudStorage`, plus iCloud Drive, largest
/// first. Drives with nothing downloaded are left out.
pub fn cloud_drives(cancel: &Cancel) -> Vec<CloudDrive> {
    let mut roots: Vec<(String, Option<String>, PathBuf)> =
        std::fs::read_dir(home_join("Library/CloudStorage"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| {
                let path = e.path();
                let (service, account) = split_name(&file_name(&path));
                (service, account, path)
            })
            .collect();
    roots.push((
        "iCloud Drive".to_string(),
        None,
        home_join("Library/Mobile Documents"),
    ));
    let mut drives: Vec<CloudDrive> = roots
        .into_iter()
        .filter_map(|(service, account, path)| {
            let (local_bytes, local_files) = local_usage(&path, cancel);
            (local_bytes > 0 && !cancel.is_cancelled()).then_some(CloudDrive {
                service,
                account,
                path,
                local_bytes,
                local_files,
            })
        })
        .collect();
    drives.sort_by_key(|d| std::cmp::Reverse(d.local_bytes));
    drives
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `GoogleDrive-me@example.com` -> ("Google Drive", Some("me@example.com")).
pub fn split_name(folder: &str) -> (String, Option<String>) {
    let (service, account) = match folder.split_once('-') {
        Some((service, account)) if !account.is_empty() => (service, Some(account.to_string())),
        _ => (folder, None),
    };
    let service = match service {
        "GoogleDrive" => "Google Drive".to_string(),
        "OneDrive" => "OneDrive".to_string(),
        "Dropbox" => "Dropbox".to_string(),
        "Box" => "Box".to_string(),
        other => other.to_string(),
    };
    (service, account)
}

/// Bytes and number of files under `root` whose content is on this disk.
/// Never follows symlinks and never opens a cloud-only folder (listing it
/// would make the drive fetch it).
pub fn local_usage(root: &Path, cancel: &Cancel) -> (u64, u64) {
    let (mut bytes, mut files) = (0u64, 0u64);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue; // `DirEntry::metadata` does not follow symlinks
            };
            let cloud_only = meta.st_flags() & SF_DATALESS != 0;
            if meta.is_dir() {
                if !cloud_only {
                    stack.push(entry.path());
                }
            } else if meta.is_file() && !cloud_only {
                let allocated = meta.blocks() * 512;
                if allocated > 0 {
                    bytes += allocated;
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_split_into_service_and_account() {
        assert_eq!(
            split_name("GoogleDrive-me@example.com"),
            (
                "Google Drive".to_string(),
                Some("me@example.com".to_string())
            )
        );
        assert_eq!(
            split_name("OneDrive-Personal"),
            ("OneDrive".to_string(), Some("Personal".to_string()))
        );
        assert_eq!(split_name("Dropbox"), ("Dropbox".to_string(), None));
    }

    #[test]
    fn counts_files_on_disk_without_following_symlinks() {
        let root = std::env::temp_dir().join(format!("nera-cloud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/one.bin"), vec![1u8; 100_000]).unwrap();
        std::fs::write(root.join("a/b/two.bin"), vec![1u8; 50_000]).unwrap();
        std::fs::write(root.join("empty"), b"").unwrap();
        let outside = std::env::temp_dir().join(format!("nera-cloud-out-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big.bin"), vec![1u8; 500_000]).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let (bytes, files) = local_usage(&root, &Cancel::new());
        assert_eq!(files, 2);
        assert!((150_000..400_000).contains(&bytes), "{bytes}");
        assert_eq!(local_usage(&root.join("missing"), &Cancel::new()), (0, 0));

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
