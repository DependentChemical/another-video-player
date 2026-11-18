//! Downloads the Material Icons, Inter, Noto Sans CJK SC, Noto Sans Symbols 2,
//! Noto Color Emoji, and STIX Two Math fonts and compiles the user interface.
//!
//! The icons are drawn with the font that Google publishes for Material
//! Design, and the UI typography uses Inter, a variable font.
//! CJK text uses Noto Sans CJK SC (variable), mathematical symbols use
//! Noto Sans Symbols 2 (static), mathematical alphanumerics use STIX Two Math,
//! and emoji use Noto Color Emoji.
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

/// Where the Noto Sans CJK SC variable font is published.
const NOTO_CJK_SC_FONT_URL: &str = "https://raw.githubusercontent.com/googlefonts/noto-cjk/main/Sans/Variable/TTF/NotoSansCJKsc-VF.ttf";

/// The family name for the Noto Sans CJK SC font.
const NOTO_CJK_SC_FONT_FAMILY: &str = "Noto Sans CJK SC";

/// Where the Noto Sans Symbols 2 font is published (static weight 400 from Google Fonts).
const NOTO_SYMBOLS2_FONT_URL: &str =
    "https://fonts.gstatic.com/s/notosanssymbols2/v25/I_uyMoGduATTei9eI8daxVHDyfisHr71ypM.ttf";

/// The family name for the Noto Sans Symbols 2 font.
const NOTO_SYMBOLS2_FONT_FAMILY: &str = "Noto Sans Symbols 2";

/// Where the Noto Color Emoji font is published (from Google Fonts).
const NOTO_EMOJI_FONT_URL: &str =
    "https://fonts.gstatic.com/s/notocoloremoji/v50/Yq6P-KqIXTD0t4D9z1ESnKM3-HpFab4.ttf";

/// The family name for the Noto Color Emoji font.
const NOTO_EMOJI_FONT_FAMILY: &str = "Noto Color Emoji";

/// Where STIX Two Math is published.
const STIX_TWO_MATH_FONT_URL: &str =
    "https://raw.githubusercontent.com/google/fonts/main/ofl/stixtwomath/STIXTwoMath-Regular.ttf";

/// The family name for STIX Two Math.
const STIX_TWO_MATH_FONT_FAMILY: &str = "STIX Two Math";

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
    ("more_vert", 0xe5d4),
    ("content_copy", 0xe14d),
    ("delete", 0xe872),
];

/// Key CJK codepoints to verify Noto Sans CJK SC coverage.
const REQUIRED_CJK_GLYPHS: &[(&str, u32)] = &[
    ("CJK Unified Ideograph (U+4E00)", 0x4e00),
    ("CJK Unified Ideograph (U+5973)", 0x5973),
    ("CJK Unified Ideograph (U+9580)", 0x9580),
    ("CJK Unified Ideograph (U+819C)", 0x819c),
    ("CJK Unified Ideograph (U+55B6)", 0x55b6),
    ("CJK Unified Ideograph (U+5E73)", 0x5e73),
    ("Hiragana (U+306E)", 0x306e),
    ("Katakana (U+30C8)", 0x30c8),
    ("Hangul (U+AC00)", 0xac00),
];

/// Key emoji codepoints to verify Noto Color Emoji coverage.
const REQUIRED_EMOJI_GLYPHS: &[(&str, u32)] = &[
    ("GRINNING FACE (U+1F600)", 0x1f600),
    ("FIRE (U+1F525)", 0x1f525),
    ("PARTY POPPER (U+1F389)", 0x1f389),
    ("THUMBS UP (U+1F44D)", 0x1f44d),
    ("HEART (U+2764)", 0x2764),
    ("SPARKLES (U+2728)", 0x2728),
];

/// Key mathematical alphanumeric codepoints to verify STIX Two Math coverage.
///
/// These are the characters titles copied from video sites tend to use for
/// styled Latin text. The block also contains holes that Unicode leaves
/// unassigned; those are intentionally not listed.
const REQUIRED_MATH_GLYPHS: &[(&str, u32)] = &[
    ("MATHEMATICAL BOLD CAPITAL A (U+1D400)", 0x1d400),
    ("MATHEMATICAL BOLD ITALIC CAPITAL A (U+1D468)", 0x1d468),
    ("MATHEMATICAL BOLD ITALIC SMALL G (U+1D488)", 0x1d488),
    ("MATHEMATICAL BOLD ITALIC SMALL H (U+1D489)", 0x1d489),
    ("MATHEMATICAL BOLD ITALIC SMALL S (U+1D494)", 0x1d494),
    ("PLANCK CONSTANT (U+210E)", 0x210e),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FLATLINE_MATERIAL_ICONS");
    println!("cargo:rerun-if-env-changed=FLATLINE_INTER_FONT");
    println!("cargo:rerun-if-env-changed=FLATLINE_NOTO_CJK_SC_FONT");
    println!("cargo:rerun-if-env-changed=FLATLINE_NOTO_SYMBOLS2_FONT");
    println!("cargo:rerun-if-env-changed=FLATLINE_NOTO_EMOJI_FONT");
    println!("cargo:rerun-if-env-changed=FLATLINE_STIX_TWO_MATH_FONT");

    let icons_font = prepare_icons_font();
    println!("cargo:rerun-if-changed={}", icons_font.display());

    let inter_font = prepare_inter_font();
    println!("cargo:rerun-if-changed={}", inter_font.display());

    let noto_cjk_sc_font = prepare_noto_cjk_sc_font();
    println!("cargo:rerun-if-changed={}", noto_cjk_sc_font.display());

    let noto_symbols2_font = prepare_noto_symbols2_font();
    println!("cargo:rerun-if-changed={}", noto_symbols2_font.display());

    let noto_emoji_font = prepare_noto_emoji_font();
    println!("cargo:rerun-if-changed={}", noto_emoji_font.display());

    let stix_two_math_font = prepare_stix_two_math_font();
    println!("cargo:rerun-if-changed={}", stix_two_math_font.display());

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

/// Makes sure the Noto Sans CJK SC font is present and returns its path.
fn prepare_noto_cjk_sc_font() -> PathBuf {
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("NotoSansCJKsc-VF.ttf");

    match std::env::var_os("FLATLINE_NOTO_CJK_SC_FONT") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_noto_cjk_sc_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(NOTO_CJK_SC_FONT_URL, &font, "Noto Sans CJK SC");
            }
            check_noto_cjk_sc_font(&font);
            font
        }
    }
}

/// Makes sure the Noto Sans Symbols 2 font is present and returns its path.
fn prepare_noto_symbols2_font() -> PathBuf {
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("NotoSansSymbols2.ttf");

    match std::env::var_os("FLATLINE_NOTO_SYMBOLS2_FONT") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_noto_symbols2_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(NOTO_SYMBOLS2_FONT_URL, &font, "Noto Sans Symbols 2");
            }
            check_noto_symbols2_font(&font);
            font
        }
    }
}

/// Makes sure the Noto Color Emoji font is present and returns its path.
fn prepare_noto_emoji_font() -> PathBuf {
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("NotoColorEmoji.ttf");

    match std::env::var_os("FLATLINE_NOTO_EMOJI_FONT") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_noto_emoji_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(NOTO_EMOJI_FONT_URL, &font, "Noto Color Emoji");
            }
            check_noto_emoji_font(&font);
            font
        }
    }
}

/// Makes sure the STIX Two Math font is present and returns its path.
fn prepare_stix_two_math_font() -> PathBuf {
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts").join("STIXTwoMath.ttf");

    match std::env::var_os("FLATLINE_STIX_TWO_MATH_FONT") {
        Some(path) => {
            let path = PathBuf::from(path);
            check_stix_two_math_font(&path);
            path
        }
        None => {
            if !font.is_file() {
                download(STIX_TWO_MATH_FONT_URL, &font, "STIX Two Math");
            }
            check_stix_two_math_font(&font);
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

/// Checks that the font has the Noto Sans CJK SC family name and key CJK glyphs.
fn check_noto_cjk_sc_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Noto Sans CJK SC font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0).unwrap_or_else(|error| {
        panic!("the Noto Sans CJK SC font could not be understood: {error}")
    });

    let family = extract_family(&face);
    if family != NOTO_CJK_SC_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {NOTO_CJK_SC_FONT_FAMILY:?}");
    }

    for (name, codepoint) in REQUIRED_CJK_GLYPHS {
        let character =
            char::from_u32(*codepoint).unwrap_or_else(|| panic!("{name} has an invalid codepoint"));
        if face.glyph_index(character).is_none() {
            panic!("the Noto Sans CJK SC font does not have the glyph {name}");
        }
    }
}

/// Checks that the font has the Noto Sans Symbols 2 family name.
fn check_noto_symbols2_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Noto Sans Symbols 2 font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0).unwrap_or_else(|error| {
        panic!("the Noto Sans Symbols 2 font could not be understood: {error}")
    });

    let family = extract_family(&face);
    if family != NOTO_SYMBOLS2_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {NOTO_SYMBOLS2_FONT_FAMILY:?}");
    }
}

/// Checks that the font has the Noto Color Emoji family name and key emoji glyphs.
fn check_noto_emoji_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the Noto Color Emoji font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0).unwrap_or_else(|error| {
        panic!("the Noto Color Emoji font could not be understood: {error}")
    });

    let family = extract_family(&face);
    if family != NOTO_EMOJI_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {NOTO_EMOJI_FONT_FAMILY:?}");
    }

    for (name, codepoint) in REQUIRED_EMOJI_GLYPHS {
        let character =
            char::from_u32(*codepoint).unwrap_or_else(|| panic!("{name} has an invalid codepoint"));
        if face.glyph_index(character).is_none() {
            panic!("the Noto Color Emoji font does not have the glyph {name}");
        }
    }
}

/// Checks that the font has the STIX Two Math family name and key glyphs.
fn check_stix_two_math_font(path: &Path) {
    let data = std::fs::read(path)
        .unwrap_or_else(|error| panic!("the STIX Two Math font could not be read: {error}"));
    let face = ttf_parser::Face::parse(&data, 0)
        .unwrap_or_else(|error| panic!("the STIX Two Math font could not be understood: {error}"));

    let family = extract_family(&face);
    if family != STIX_TWO_MATH_FONT_FAMILY {
        panic!("the downloaded font is {family:?} instead of {STIX_TWO_MATH_FONT_FAMILY:?}");
    }

    for (name, codepoint) in REQUIRED_MATH_GLYPHS {
        let character =
            char::from_u32(*codepoint).unwrap_or_else(|| panic!("{name} has an invalid codepoint"));
        if face.glyph_index(character).is_none() {
            panic!("the STIX Two Math font does not have the glyph {name}");
        }
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
