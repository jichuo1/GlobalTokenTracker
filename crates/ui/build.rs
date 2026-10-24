//! Embed `assets/icon.ico` as icon resource #1. The window titlebar/taskbar
//! pick it up via `AppWindow.SetIcon("1")`, and the tray via
//! `Icon::from_resource(1)` — no asset files need shipping next to the exe.
//! Also embeds `app.manifest` (RT_MANIFEST #1).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The manifest is what lets in-process third-party input methods
        // (WeChat IME) show their candidate window — see app.manifest.
        winresource::WindowsResource::new()
            .set_icon("../../assets/icon.ico")
            .set_manifest_file("app.manifest")
            .compile()
            .expect("embed icon resource");
    }
}
