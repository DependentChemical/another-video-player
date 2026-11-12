//! Downloads the Material Icons font and compiles the user interface.
//!
//! The icons are drawn with the font that Google publishes for Material
//! Design, so the interface does not have to carry vector paths of its own.
//! The font is fetched once, checked for the glyphs the interface needs and
//! then imported by the interface, which embeds it into the binary.

use std::path::{Path, PathBuf};

/// Where the font is published.
const FONT_URL: &str = "https://raw.githubusercontent.com/google/material-design-icons/master/font/MaterialIcons-Regular.ttf";

/// The name the font carries, and the name the interface asks for.
const FONT_FAMILY: &str = "Material Icons";

/// The glyphs the interface uses.
///
/// The codepoints are the ones Google publishes for the Material Icons font.
/// They are checked against the downloaded file, so a font that does not
/// match stops the build instead of showing empty boxes.
const REQUIRED_GLYPHS: &[(&str, u32)] = &[
    ("play_arrow", 0xe037),
    ("pause", 0xe034),
    ("arrow_drop_down", 0xe5c5),
    ("fullscreen", 0xe5d0),
    ("fullscreen_exit", 0xe5d1),
    ("live_tv", 0xe639),
    ("settings", 0xe8b8),
    ("fiber_manual_record", 0xe061),
    ("volume_up", 0xe050),
    ("volume_off", 0xe04f),
    ("replay", 0xe042),
    ("replay_10", 0xe059),
    ("forward_10", 0xe056),
    ("arrow_back", 0xe5c4),
    ("arrow_forward", 0xe5c8),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FLATLINE_MATERIAL_ICONS");

    let font = prepare_font();
    println!("cargo:rerun-if-changed={}", font.display());

    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/main.slint", config)
        .expect("the user interface failed to compile");
}

/// Makes sure the font is there and returns where it is.
///
/// The font is kept in the crate, so the interface can import it by a path
/// that does not change between builds.
fn prepare_font() -> PathBuf {
    let font =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("MaterialIcons-Regular.ttf");

    match std::env::var_os("FLATLINE_MATERIAL_ICONS") {
        Some(path) => {
            let path = PathBuf::from(path);
            check(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(&font);
            }
            check(&font);
            font
        }
    }
}

/// Downloads the font.
fn download(destination: &Path) {
    let mut response = ureq::get(FONT_URL)
        .header("User-Agent", concat!("Flatline/", env!("CARGO_PKG_VERSION")))
        .call()
        .unwrap_or_else(|error| panic!("the Material Icons font could not be downloaded: {error}"));

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("the font directory could not be created: {error}"));
    }

    let mut body = response.body_mut().as_reader();
    let mut file = std::fs::File::create(destination)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be written: {error}"));
    std::io::copy(&mut body, &mut file)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be written: {error}"));
}

/// Checks that the font has the family name and the glyphs the interface needs.
fn check(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be understood: {error}"));

    let names = face.names();
    let family = names
        .into_iter()
        .filter(|name| {
            name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY
                || name.name_id == ttf_parser::name_id::FAMILY
        })
        .filter_map(|name| name.to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_default();
    if family != FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {FONT_FAMILY:?}");
    }

    for (name, codepoint) in REQUIRED_GLYPHS {
        let character =
            char::from_u32(*codepoint).unwrap_or_else(|| panic!("{name} has an invalid codepoint"));
        if face.glyph_index(character).is_none() {
            panic!("the Material Icons font does not have the glyph {name}");
        }
    }
}
