# AGENTS.md

Flatline is a small video player for everything `yt-dlp` can reach. It is
written in Rust, it draws its interface with Slint and it plays with libmpv.

This file is the contract for every agent and every contributor working in
this repository. Read it before changing anything.

## Release from `dev` to `main`

Work on `dev` by default. Keep `main` for changes that have been tested and
solidified; do not develop directly on `main`.

To release, first confirm the working tree is clean and `dev` is the current
branch. Compare `dev` with `main` from their merge base and collect the commit
subjects since the previous merge. Use those subjects to prepare a concise,
grouped changelog; do not infer additional changes from diffs or other notes.
Check that `version.txt` on `dev` is higher than the version on `main`, and
that the corresponding `v<version>` tag has not already been released.

Update this file on `dev` if the release procedure changes, then create a
`--no-ff` merge commit on `main` whose message contains the prepared
changelog. The Release workflow uses that merge commit message as the GitHub
release body after the Build workflow succeeds on `main`. Push `main` to
trigger the build and release workflows, then return to `dev` for all further
work. Check the GitHub Actions run and report whether the build and release
completed successfully. If the status cannot be checked, leave that check to
the user and say why.

After returning to `dev`, remove generated build output such as `target/`
and `dist/` so the workspace is clean. Keep `.flatline/`, which holds
downloaded runtime dependencies and state rather than build output.

## The isolation rule

This is the rule the whole repository is built around. It is not a style
preference — it is checked by `cargo xtask isolation`, which fails the build
when it is broken.

> **A crate owns exactly one capability, knows nothing about its siblings,
> and can be used, tested and understood on its own.**

Concretely:

* A crate that does not need another crate must not depend on it, not even
  "just for a type". If two capabilities need the same data, the data lives
  in `media-model` and both crates depend on that — never on each other.
* Capabilities are wired together in **one** place, `app-core`. It is the
  only crate that knows more than one other crate exists.
* Dependencies point **down** the ladder, never sideways and never up.

The ladder:

| Layer | Crates | May depend on |
| --- | --- | --- |
| 0 — vocabulary | `media-model` | nothing |
| 1 — capabilities | `playback-engine`, `stream-resolver`, `storyboard`, `ui-kit`, `ytdlp-tool` | `media-model` at most |
| 2 — adapter | `video-surface` | `playback-engine` only |
| 3 — composition root | `app-core` | everything |

`video-surface` is the single exception the ladder allows: the mpv render
context belongs to the player, but it can only be created while a UI toolkit
holds an OpenGL context. The coupling is deliberate, narrow and documented
in that crate.

When you add a crate, put it on the ladder and extend the table in
`xtask/src/isolation.rs` in the same change. The checker reads the real
dependency graph with `cargo metadata`, so a rule that is not in the table
is not enforced.

### What this buys

* Every capability can be tested on its own. `stream-resolver` has tests that
  never start a player.
* A crash in one capability cannot take another one with it.
* The look of the player, the way it is resolved and the way it is played
  can each change without touching the others.
* A new agent can read one crate and understand it completely.

## Setup commands

- Install dependencies: `cargo xtask setup` — downloads `libmpv` for Windows
  into `.flatline/deps/mpv`. On Linux and macOS the system `libmpv` is used.
- Build everything: `cargo xtask build` — the one command that prepares the
  dependencies, compiles every crate and collects the result in `dist/`.
- Build without packaging: `cargo build --workspace`
- Run the player: `cargo run -p app-core`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`
- Check the isolation rules: `cargo xtask isolation`

`cargo run -p app-core` is enough to try it: the player downloads `yt-dlp`
for itself when it is missing, and looks for a newer one on every start.

## Project layout

- `crates/app-core` — the composition root: the window, the wiring and the
  `flatline` binary. The only crate that knows the others exist.
- `crates/media-model` — the shared vocabulary: media sources, stream
  catalogs, qualities and playback plans.
- `crates/playback-engine` — the libmpv wrapper: transport control, state
  and the OpenGL renderer.
- `crates/video-surface` — the adapter that binds the mpv renderer into a
  Slint window.
- `crates/stream-resolver` — turns a URL into a catalog of streams and
  decides which streams to play.
- `crates/ytdlp-tool` — finds, installs and updates the `yt-dlp` executable.
- `crates/storyboard` — cuts preview images out of a media with `ffmpeg`.
- `crates/ui-kit` — the Slint components, the theme and the widgets.
- `xtask` — the build task: dependency provisioning, the isolation check
  and packaging.
- `.flatline/` — downloaded dependencies. Not part of the repository.

## Code style

- Rust 2024, rustfmt with the settings in `rustfmt.toml`.
- `cargo clippy --workspace --all-targets -- -D warnings` has to stay quiet.
- Every public item carries a doc comment that says *what it is for*, not
  what it does. The crate level comment says which layer the crate sits on.
- Errors are values. A capability returns `Result` with its own error type
  and never panics on bad input.
- No `unwrap()` outside tests, except where the invariant is documented
  right above the line.
- Comments explain *why*, never *what*. The code already says what.

## Testing instructions

- Unit tests: `cargo test --workspace`. They live next to the code they
  cover, in a `#[cfg(test)]` module.
- Every new behaviour needs a test. The leaves are cheap to test, so there
  is no excuse.
- The crates that need an external program (`yt-dlp`, `ffmpeg`) are tested
  on their parsing and planning logic, not on the program itself.
- The isolation rules are tested by `cargo xtask isolation`.

## PR & commit conventions

- Work on `dev`; `main` is reserved for tested and solidified releases.
- Conventional commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`,
  `chore:`.
- Keep one capability per commit. A commit that touches three crates for
  three different reasons is three commits.
- When you add a crate, the same change has to update the table in
  `xtask/src/isolation.rs` and the layer table above.

## Security

- Never commit secrets. `.env` is in `.gitignore`, and so is `.flatline/`.
- The player runs `yt-dlp` with a URL the user typed. It never builds a
  shell command out of it: the URL is passed as a separate argument.
- Downloaded files land in a directory Flatline owns and are replaced
  atomically, so an interrupted download never leaves a broken executable
  behind.
