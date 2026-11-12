//! The Flatline player.

fn main() {
    let url = std::env::args().nth(1);
    if let Err(error) = app_core::run(url.as_deref()) {
        eprintln!("flatline: {error}");
        std::process::exit(1);
    }
}
