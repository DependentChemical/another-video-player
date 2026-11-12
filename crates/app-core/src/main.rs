//! The Flatline player.
//!
//! On Windows the player is a graphical program, so it is linked against the
//! Windows subsystem: no console window opens when it starts. The notes of
//! the program are written to a log file next to it instead.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let mut arguments = std::env::args().skip(1);
    let argument = arguments.next();

    if matches!(argument.as_deref(), Some("--version") | Some("-V")) {
        println!("flatline {}", app_core::VERSION);
        return;
    }
    if matches!(argument.as_deref(), Some("--help") | Some("-h")) {
        print_help();
        return;
    }

    if let Err(error) = app_core::run(argument.as_deref()) {
        app_core::diag::note(&error.to_string());
        std::process::exit(1);
    }
}

/// Prints what the program can do.
fn print_help() {
    println!(
        "Flatline {version} — a video player for everything yt-dlp can reach

Usage:
  flatline [URL]      open the player and load the URL right away
  flatline --version  print the version and leave
  flatline --help     print this text and leave
",
        version = app_core::VERSION
    );
}
