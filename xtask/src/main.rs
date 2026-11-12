//! The build orchestrator of Flatline.
//!
//! One command builds everything:
//!
//! ```text
//! cargo xtask build
//! ```
//!
//! It prepares the external dependencies, compiles the whole workspace and
//! puts the result into `dist/`.

use std::process::{Command, ExitCode};

mod deps;
mod isolation;
mod package;

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "build".to_string());
    let rest: Vec<String> = arguments.collect();

    let outcome = match command.as_str() {
        "build" => build(&rest),
        "setup" => deps::setup(),
        "isolation" => isolation::check(),
        "package" => package_only(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command `{other}`")),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Collects the player in an archive.
fn package_only() -> Result<(), String> {
    let archive = package::package()?;
    println!("xtask: the player is ready in {}", archive.display());
    Ok(())
}

/// Builds the whole workspace.
fn build(arguments: &[String]) -> Result<(), String> {
    let release = !arguments.iter().any(|argument| argument == "--debug");

    deps::setup()?;

    let mut command = Command::new(cargo());
    command.current_dir(deps::workspace_root());
    if release {
        command.args(["build", "--release", "--workspace"]);
    } else {
        command.args(["build", "--workspace"]);
    }
    if let Some(directory) = deps::mpv_library_dir() {
        command.env("MPV_LIBRARY_DIR", directory);
    }

    println!("xtask: building the workspace…");
    let status =
        command.status().map_err(|error| format!("cargo could not be started: {error}"))?;
    if !status.success() {
        return Err("the build failed".to_string());
    }

    if release {
        let archive = package::package()?;
        println!("xtask: the player is ready in {}", archive.display());
    }
    Ok(())
}

/// The cargo executable that runs this task.
fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Prints what the task can do.
fn print_help() {
    println!(
        "Flatline build task

Usage:
  cargo xtask build [--debug]  prepare the dependencies and build everything
  cargo xtask setup            only prepare the external dependencies
  cargo xtask isolation        check that the crates stay isolated
  cargo xtask package          collect the player in dist/
"
    );
}
