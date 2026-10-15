use std::{env, fs, path::Path};

/// Empty-zip End-Of-Central-Directory record (22 bytes) — lets plain
/// `cargo build` compile a setup stub; package.ps1 supplies the real payload.
const EMPTY_ZIP: &[u8] = &[
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    println!("cargo:rerun-if-env-changed=GTT_EMBED_PAYLOAD");
    // Embed the app icon as resource #1 — installer window/titlebar and the
    // installed exe pick it up via LoadIconW(MAKEINTRESOURCE(1)).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/icon.ico")
            .compile()
            .expect("embed icon resource");
    }
    let src = Path::new("payload/payload.zip");
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("payload.zip");
    if src.exists() {
        fs::copy(src, &out).unwrap();
    } else {
        fs::write(&out, EMPTY_ZIP).unwrap();
    }
}
