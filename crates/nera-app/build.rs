//! Windows: embed the icon and version info into
//! `nera-cleaner.exe` (GPUI already embeds the application manifest: DPI
//! awareness, common controls). Other platforms need nothing at build time (the macOS
//! bundle is assembled by `scripts/bundle.sh`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=resources/windows");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("resources/windows/app-icon.ico");
        res.set("ProductName", "Nera Cleaner");
        res.set("FileDescription", "Nera Cleaner");
        res.set("CompanyName", "Nextera");
        res.set(
            "LegalCopyright",
            "Copyright © Nextera. All rights reserved.",
        );
        res.set("OriginalFilename", "nera-cleaner.exe");
        if let Err(err) = res.compile() {
            panic!("could not embed Windows resources: {err}");
        }
    }
}
