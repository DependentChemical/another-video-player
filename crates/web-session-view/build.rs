//! The build step of the sign-in helper.
//!
//! Tauri reads its configuration at build time and writes the context the
//! runtime starts from, so the crate cannot be built without this call.

fn main() {
    tauri_build::build()
}
