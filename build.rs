//! Embeds the application manifest, icon and version information into the .exe,
//! and pins the Windows libraries it is linked against to the system folder.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Windows normally looks for a program's libraries in the program's own
    // folder first. This program is run from wherever it was downloaded to, so
    // a planted `winhttp.dll` sitting in that folder would be loaded into it.
    // This linker option records, in the .exe itself, that the libraries it
    // depends on are to be loaded from the Windows system folder only
    // (LOAD_LIBRARY_SEARCH_SYSTEM32). Libraries loaded later are covered by
    // `win::restrict_library_search`.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
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
