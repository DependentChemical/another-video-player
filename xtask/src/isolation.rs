//! The isolation rules of the workspace, checked by the build.
//!
//! Flatline is built from small crates that own one capability each. The
//! rules below make sure that they stay that way: a crate may only reach
//! for the vocabulary crate, and only the composition root may know about
//! more than one capability.

use std::collections::BTreeSet;
use std::process::Command;

/// The crates of the workspace and the internal crates they may use.
///
/// A crate that is not listed here may not depend on any other crate of the
/// workspace.
const RULES: &[(&str, &[&str])] = &[
    ("database", &[]),
    ("library", &["database", "media-model"]),
    ("media-model", &[]),
    ("settings", &["media-model"]),
    ("playback-engine", &[]),
    ("storyboard", &[]),
    ("ui-kit", &[]),
    ("ytdlp-tool", &[]),
    ("stream-resolver", &["media-model"]),
    ("video-surface", &["playback-engine"]),
    (
        "app-core",
        &[
            "library",
            "media-model",
            "playback-engine",
            "stream-resolver",
            "settings",
            "storyboard",
            "ui-kit",
            "video-surface",
            "ytdlp-tool",
        ],
    ),
];

/// The crate that composes the others.
const COMPOSITION_ROOT: &str = "app-core";

/// Checks that no crate breaks the isolation rules.
pub fn check() -> Result<(), String> {
    let metadata = metadata()?;
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();

    let mut problems: Vec<String> = Vec::new();

    for package in &packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        if name == "xtask" {
            continue;
        }

        let dependencies: BTreeSet<String> = package["dependencies"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry["path"].is_string())
                    .filter_map(|entry| entry["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let allowed: BTreeSet<String> = RULES
            .iter()
            .find(|(crate_name, _)| *crate_name == name)
            .map(|(_, allowed)| allowed.iter().map(|entry| entry.to_string()).collect())
            .unwrap_or_default();

        for dependency in &dependencies {
            if !allowed.contains(dependency) {
                problems.push(format!(
                    "{name} must not depend on {dependency}; only {} may use it",
                    if allowed.is_empty() {
                        "no other crate".to_string()
                    } else {
                        allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                    }
                ));
            }
        }
    }

    // Nothing may depend on the composition root.
    for package in &packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        if name == COMPOSITION_ROOT || name == "xtask" {
            continue;
        }
        let depends_on_root = package["dependencies"]
            .as_array()
            .map(|entries| {
                entries.iter().any(|entry| {
                    entry["path"].is_string() && entry["name"].as_str() == Some(COMPOSITION_ROOT)
                })
            })
            .unwrap_or(false);
        if depends_on_root {
            problems.push(format!("{name} must not depend on {COMPOSITION_ROOT}"));
        }
    }

    if problems.is_empty() {
        println!("xtask: the crates are properly isolated");
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// Reads the metadata of the workspace.
fn metadata() -> Result<serde_json::Value, String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(crate::deps::workspace_root())
        .output()
        .map_err(|error| format!("cargo metadata could not be started: {error}"))?;

    if !output.status.success() {
        return Err("cargo metadata failed".to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata is broken: {error}"))
}
