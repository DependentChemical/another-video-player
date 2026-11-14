//! Downloads the Material Icons and Inter fonts and compiles the user interface.
//!
//! The icons are drawn with the font that Google publishes for Material
//! Design, and the UI typography uses Inter, a variable font.
//! The fonts are fetched, checked and then imported by the interface,
//! which embeds them into the binary.

use std::path::{Path, PathBuf};

/// Where the Material Icons font is published.
const ICONS_FONT_URL: &str = "https://raw.githubusercontent.com/google/material-design-icons/master/font/MaterialIcons-Regular.ttf";

/// The family name for the icons font.
const ICONS_FONT_FAMILY: &str = "Material Icons";

/// Where the Inter variable font is published.
const INTER_FONT_URL: &str =
    "https://raw.githubusercontent.com/google/fonts/main/ofl/inter/Inter%5Bopsz%2Cwght%5D.ttf";

/// The family name for the Inter font.
const INTER_FONT_FAMILY: &str = "Inter";

/// The glyphs the interface uses.
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
    println!("cargo:rerun-if-env-changed=FLATLINE_INTER_FONT");

    let icons_font = prepare_icons_font();
    println!("cargo:rerun-if-changed={}", icons_font.display());

    let inter_font = prepare_inter_font();
    println!("cargo:rerun-if-changed={}", inter_font.display());

    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/main.slint", config)
        .expect("the user interface failed to compile");
}

/// Makes sure the Material Icons font is present and returns its path.
fn prepare_icons_font() -> PathBuf {
    let font =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("MaterialIcons-Regular.ttf");

    match std::env::var_os("FLATLINE_MATERIAL_ICONS") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_icons_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(ICONS_FONT_URL, &font, "Material Icons");
            }
            check_icons_font(&font);
            font
        }
    }
}

/// Makes sure the Inter font is present and returns its path.
fn prepare_inter_font() -> PathBuf {
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("Inter.ttf");

    match std::env::var_os("FLATLINE_INTER_FONT") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_inter_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(INTER_FONT_URL, &font, "Inter");
            }
            check_inter_font(&font);
            font
        }
    }
}

/// Downloads a font to the destination path.
fn download(url: &str, destination: &Path, name: &str) {
    let mut response = ureq::get(url)
        .header("User-Agent", concat!("Flatline/", env!("CARGO_PKG_VERSION")))
        .call()
        .unwrap_or_else(|error| panic!("the {name} font could not be downloaded: {error}"));

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("the font directory could not be created: {error}"));
    }

    let mut body = response.body_mut().as_reader();
    let mut file = std::fs::File::create(destination)
        .unwrap_or_else(|error| panic!("the {name} font could not be written: {error}"));
    std::io::copy(&mut body, &mut file)
        .unwrap_or_else(|error| panic!("the {name} font could not be written: {error}"));
}

/// Checks that the font has the Material Icons family name and required glyphs.
fn check_icons_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0)
        .unwrap_or_else(|error| panic!("the Material Icons font could not be understood: {error}"));

    let family = extract_family(&face);
    if family != ICONS_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {ICONS_FONT_FAMILY:?}");
    }

    for (name, codepoint) in REQUIRED_GLYPHS {
        let character =
            char::from_u32(*codepoint).unwrap_or_else(|| panic!("{name} has an invalid codepoint"));
        if face.glyph_index(character).is_none() {
            panic!("the Material Icons font does not have the glyph {name}");
        }
    }
}

/// Checks that the font has the Inter family name.
fn check_inter_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Inter font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0)
        .unwrap_or_else(|error| panic!("the Inter font could not be understood: {error}"));

    let family = extract_family(&face);
    if family != INTER_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {INTER_FONT_FAMILY:?}");
    }
}

/// Extracts the typographic or standard family name of a font face.
fn extract_family(face: &ttf_parser::Face) -> String {
    face.names()
        .into_iter()
        .filter(|name| {
            name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY
                || name.name_id == ttf_parser::name_id::FAMILY
        })
        .filter_map(|name| name.to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}
