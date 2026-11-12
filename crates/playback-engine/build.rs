use std::path::{Path, PathBuf};

/// Points at the mpv library that `cargo xtask setup` downloads.
fn main() {
    println!("cargo:rerun-if-env-changed=MPV_LIBRARY_DIR");

    if cfg!(windows) {
        let found = std::env::var_os("MPV_LIBRARY_DIR")
            .map(|directory| Path::new(&directory).join("mpv.lib").is_file())
            .unwrap_or(false);
        if !found {
            println!("cargo:warning=libmpv was not found. Run `cargo xtask setup` to download it.");
        }
    }

    place_runtime();
}

/// Copies the mpv runtime next to the binaries that need it.
///
/// Windows looks for a DLL next to the executable, and the test binaries
/// live in `target/<profile>/deps`. Without the runtime, every program that
/// talks to libmpv refuses to start.
fn place_runtime() {
    if !cfg!(windows) {
        return;
    }
    let Some(library) = std::env::var_os("MPV_LIBRARY_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(runtime) = library.parent().map(|parent| parent.join("bin")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&runtime) else {
        return;
    };

    // <target>/<profile>/build/<package>-<hash>/out
    let Some(profile) = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .and_then(|out| out.ancestors().nth(3).map(Path::to_path_buf))
    else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("dll")) {
            let name = path.file_name().unwrap_or_default();
            copy_if_needed(&path, &profile.join(name));
            copy_if_needed(&path, &profile.join("deps").join(name));
        }
    }
}

/// Copies a file unless the destination is already the same.
fn copy_if_needed(from: &Path, to: &Path) {
    let same = match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(source), Ok(target)) => source.len() == target.len(),
        _ => false,
    };
    if same {
        return;
    }
    if let Some(parent) = to.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(from, to);
}
