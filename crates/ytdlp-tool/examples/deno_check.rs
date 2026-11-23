//! A throwaway check of the Deno provisioning: it installs the runtime the
//! way the player does, runs it and looks at the files it leaves behind.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("a target directory"));
    let paths = ytdlp_tool::ToolPaths::new(&root);

    let binary = ytdlp_tool::deno::install(&paths).expect("deno installs");
    println!("installed: {}", binary.display());
    println!("is a file: {}", binary.is_file());
    println!("deno-files dir exists: {}", paths.deno_files_dir().is_dir());

    let version = Command::new(&binary).arg("--version").output().expect("deno runs");
    println!(
        "version: {} (status {})",
        String::from_utf8_lossy(&version.stdout).trim(),
        version.status
    );

    // The runtime has to keep its files inside the tool directory.
    let evaluated = Command::new(&binary)
        .env("DENO_DIR", paths.deno_files_dir())
        .env("DENO_NO_UPDATE_CHECK", "1")
        .env("DENO_NO_PROMPT", "1")
        .args(["eval", "console.log('runtime works')"])
        .output()
        .expect("deno evaluates");
    println!(
        "eval: {} (status {})",
        String::from_utf8_lossy(&evaluated.stdout).trim(),
        evaluated.status
    );

    // Nothing may be written outside of the tool directory.
    let entries: Vec<String> = std::fs::read_dir(paths.deno_files_dir())
        .expect("the deno files directory can be read")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    println!("deno-files holds: {entries:?}");

    let leftovers: Vec<String> = std::fs::read_dir(&root)
        .expect("the tool directory can be read")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    println!("tools holds: {leftovers:?}");
}
