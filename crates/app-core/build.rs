//! Embeds the application icon and sets the runtime search path.
//!
//! The version of the application comes from the workspace manifest in
//! `Cargo.toml`, so the build script does not need to carry it.

fn main() {
    println!("cargo:rerun-if-changed=../../resources/flatline.ico");

    #[cfg(target_os = "windows")]
    {
        let icon = manifest_dir().join("resources").join("flatline.ico");
        winres::WindowsResource::new()
            .set_icon(icon.to_str().expect("icon path is valid UTF-8"))
            .compile()
            .expect("the Windows executable icon could not be embedded");
    }

    // The build puts the mpv runtime next to the executable, so the loader
    // has to look there first. On Windows the loader does that by itself.
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    } else if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
    }
}

/// The directory the workspace root lives in.
#[cfg(target_os = "windows")]
fn manifest_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}
