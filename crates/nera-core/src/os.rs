//! What the host platform calls things, for user-facing copy.
//!
//! Use the constants in formatted text and the macros where a string literal
//! is needed (`concat!("Moved to the ", nera_core::trash_name!(), ".")`), so
//! feature code needs no `cfg` just to say "Trash" or "Recycle Bin".

/// "macOS" or "Windows".
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! os_name {
    () => {
        "macOS"
    };
}
#[cfg(windows)]
#[macro_export]
macro_rules! os_name {
    () => {
        "Windows"
    };
}

/// "Trash" or "Recycle Bin".
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! trash_name {
    () => {
        "Trash"
    };
}
#[cfg(windows)]
#[macro_export]
macro_rules! trash_name {
    () => {
        "Recycle Bin"
    };
}

/// "Finder" or "File Explorer".
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! file_manager {
    () => {
        "Finder"
    };
}
#[cfg(windows)]
#[macro_export]
macro_rules! file_manager {
    () => {
        "File Explorer"
    };
}

/// "Mac" or "PC".
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! computer {
    () => {
        "Mac"
    };
}
#[cfg(windows)]
#[macro_export]
macro_rules! computer {
    () => {
        "PC"
    };
}

pub const NAME: &str = os_name!();
pub const TRASH: &str = trash_name!();
pub const FILE_MANAGER: &str = file_manager!();
pub const COMPUTER: &str = computer!();
/// Label of the button that reveals a file in the file manager.
pub const SHOW_IN_FILE_MANAGER: &str = concat!("Show in ", file_manager!());
