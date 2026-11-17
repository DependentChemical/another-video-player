# Feature Report: Queue / `active_queue` rework + drag-to-reorder + scroll-during-drag

## Pre-flight checks

- **Unique IDs confirmed.** `active_queue.id` is `INTEGER PRIMARY KEY AUTOINCREMENT`
  (`crates/library/src/store.rs:117`) — every row gets a unique, stable `id`. Duplicate
  videos (same URL) get **different** `id`s. `id_hash` (MD5 of cleaned URL,
  `crates/media-model/src/identity.rs:39`) is intentionally *not* unique per entry (same
  URL → same hash), so `id` is the correct key. The queue already keys removals on `id`
  (`remove_entry`).

## Changes (7 files, not committed)

### 1. Track the active video by `id`, not `position`
**`crates/library/src/store.rs`**
- `set_current(&self, id: i64)` — signature changed from `position` to `id`. Sets
  `current = 1 WHERE id = ?`. Unknown id leaves mark untouched.
- Rationale: the active mark is now attached to stable row identity. Reassigning
  `position` during drag reorder never has to move or recompute a "current position"
  reference — that's the "extra math around repositions and updating the position value"
  being removed.

### 2. `advance()` plays the next entry by id
**`crates/library/src/store.rs`**
- Reads current entry's `(id, position)`, finds next row with `position > current.position`,
  sets `current = 1 WHERE id = <next id>`.
- Rationale: "instead of incrementing just search for the ID and then get the next video
  after that" — still orders by `position`, but applies the mark by id.

### 3. Drag reorder persisted via `reorder()`
**`crates/library/src/store.rs`** (new)
- `reorder(&self, ordered_ids: &[i64])` — assigns `position = 0..n-1` in the given id
  order. Current mark follows its entry (tracked by id).
- **Test added:** `reorder_replays_entries_in_the_given_id_order` (verifies order + mark
  stays on its entry).

### 4. App-core: `play_queue_entry(id)`, `reorder_queue()`
**`crates/app-core/src/app.rs`**
- `play_queue_entry(&self, id: i64)` — takes `id`, calls `set_current(id)`.
- `remove_queue_entry` — `set_current(next.id)` instead of `set_current(next.position)`.
- `reorder_queue(&self, ordered_ids: &[i64]) -> bool` — wraps `queue.reorder`; used by
  the UI after a drop.

### 5. Shell wiring
**`crates/app-core/src/shell.rs`**
- `on_queue_item_selected` now carries `id` (was `position`).
- `on_queue_move_requested(src, tgt)` — takes indices from Slint, rebuilds id order in
  Rust (`entries.remove(src); entries.insert(tgt, entry)`), calls `reorder_queue`,
  refreshes view. Order rebuilt in Rust because Slint has no array-splice in-language.
- Registered `on_queue_row_pressed`/`on_queue_row_released` no-ops so Slint callbacks are
  bound (PlayerUi owns the timer).

### 6. UI: long-press (1.5s) drag + indicator bar + current-id + scroll-during-drag
**`crates/ui-kit/src/lib.rs`**
- `show_queue` sets `queue-current-id` (authoritative active ref) and
  `queue-current-index` (derived scroll hint, recomputed every show — never stale).
- `PlayerUi` owns a 1.5s single-shot `Timer` for long-press detection + a 16ms repeated
  `Timer` for drag auto-scroll. `on_queue_row_pressed` arms the long-press timer;
  `on_queue_row_released` disarms it. Long-press fire sets `queue-long-press-index` on
  the window.
- Rationale for Rust timers: Slint 1.18's `.slint` language has **no `Timer` element**
  and **no `grab`/`long-pressed`** (verified by build errors). Timers live in Rust where
  we have full control.
- **Scroll-while-drag:** The 16ms timer fires while `queue-long-press-index >= 0`. The
  dragged row's `TouchArea` reports the pointer in viewport coordinates
  (`queue-drag-pointer-y`), so the timer clamps that position to the content bounds and
  scrolls toward whichever 100px edge zone it is in. Speed scales linearly with how far
  into the zone the pointer is (max 40px/tick).

**`crates/ui-kit/ui/main.slint`**
- Properties: `queue-content-y` (in-out length), `queue-scroll-view-height` (in-out
  length); callback `scroll-view-resized(float)`; wired into QueueDialog.

**`crates/ui-kit/ui/slint-ui/QueueDialog.slint`**
- Property `queue-content-y` (bound to ScrollView's `content-y`), property
  `queue-scroll-view-height` (bound from `scroll-view.height` via
  `scroll-view-resized` callback).
- `content-y` is now managed: set from `current-index` on open in Rust (`show_queue`),
  updated by the scroll timer during drag.
- Drag gesture: press → arms 1.5s timer. Release before timer fires → short press →
  `item-selected(id)` (play). Release after timer fires → `commit_move()` →
  `move-requested(source, target)`.
- Drop target: a long-pressed row is tinted `#00000033`. While it is held, the row's own
  `TouchArea.moved` / `mouse-y` update `drag-pointer-y`; `drag-target` is a derived
  property (`floor((pointer - content-y) / row-step)`). The 3px `Theme.accent` bar is
  drawn above the target when dragging up and below it when dragging down, matching the
  remove-then-insert order the host applies.
- `QueueItem` gained `index: int` (set via Rust `enumerate` — Slint has no for-loop
  index).

**`crates/ui-kit/ui/slint-ui/types.slint`**
- `QueueItem.index: int` added.

## Position retention on delete
**No renumbering on delete.** `remove_entry` deletes by `id`, leaves other rows'
`position` values untouched (gaps allowed). `advance()` and `next_after()` query
`position > ?`, so gaps don't break playback order. `reorder()` (drag) is what re-compacts
positions to `0..n-1`. Position-based ordering is retained; positions are only re-sequenced
on drag reorder.

## Migration strategy
**None needed, none added.** `active_queue` already has
`id INTEGER PRIMARY KEY AUTOINCREMENT` and `position INTEGER` — no schema change. Existing
`current = 1` rows are stored as a column value, so switching `set_current(position)` →
`set_current(id)` is purely behavioral. Old databases open and work as-is. `id` uniqueness
is guaranteed by the PK constraint, so no startup guard is needed.

## Verification results
- `cargo build --workspace` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo xtask isolation` — "the crates are properly isolated"
- `cargo test --workspace` — all pass (library 34 tests incl. new `reorder` test; ui-kit 3
  tests; app-core clean)

## What's NOT committed
No commits were made. Files changed:
`crates/app-core/src/app.rs`, `crates/app-core/src/shell.rs`,
`crates/library/src/store.rs`, `crates/ui-kit/src/lib.rs`,
`crates/ui-kit/ui/main.slint`, `crates/ui-kit/ui/slint-ui/QueueDialog.slint`,
`crates/ui-kit/ui/slint-ui/types.slint`.

Run it yourself first, then tell me if you want me to commit on `dev` (per AGENTS.md).

## Follow-up: drag did not start while the button was held

**Root cause.** While a TouchArea is pressed it grabs the mouse. Slint keeps sending
events only to the grabbed row, so sibling rows never receive a hover update and
changed has-hover never fired during a drag. The drop target therefore stayed -1
and the release was a no-op. The scroll math also used the dragged row's original
position with an inverted content-y sign, so it could not follow the pointer.

**Fix.**
- QueueDialog.slint: the dragged row's own TouchArea now reports the pointer via
  moved and mouse-y (Slint 1.18 does expose mouse-x/mouse-y/moved).
  drag-pointer-y is in viewport coordinates; drag-target is derived from it, so it
  tracks the cursor and stays correct while the list auto-scrolls.
- Release drops at the pointer position (commit_move() reads the derived target).
  A press taken over by the ScrollView (has-hover cleared) is ignored instead of
  playing the row.
- lib.rs: auto-scroll uses the pointer position and clamps content-y to the real
  content height; the long-press state is cleared on release; show_queue keeps the
  scroll position when the dialog is already open (reorder no longer jumps back to the
  current entry). Model is imported for ow_count().
- main.slint: exposes queue-drag-pointer-y.

**Verified:** cargo check -p ui-kit, cargo clippy --workspace --all-targets -- -D
warnings, cargo test --workspace (all pass) and cargo xtask isolation (clean).
