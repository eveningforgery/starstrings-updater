//! Embeds the application manifest, icon and version information into the .exe.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("assets/app.manifest");
    res.set_icon("assets/icon.ico");
    // "FileDescription" is the name Task Manager shows for the process.
    res.set("FileDescription", "StarStrings Updater");
    res.set("ProductName", "StarStrings Updater");
    res.set("OriginalFilename", "starstrings-updater.exe");
    res.set("InternalName", "starstrings-updater");
    res.set("CompanyName", "starstrings-updater contributors");
    res.set("LegalCopyright", "MIT License");
    res.compile()
        .expect("failed to embed Windows resources (is the Windows SDK installed?)");
}
