# Reef Text-Input & Picker Stack

Use this reference when touching `src/input.rs` text-input handlers, any
picker overlay (`hosts_picker`, `quick_open`, `global_search`,
`graph_branch_picker`), the commit textarea, the SQLite goto-page input,
or any new TUI input field. Reef has a deliberate three-layer
architecture; bypassing it re-introduces the bugs the unification was
built to prevent.

## The three layers

```
┌─────────────────────────────────────────────────────────────┐
│ L2  picker_core::PickerCore                                 │
│     overlay scaffold: filter + cursor + selected_idx +      │
│     last_popup_area + dispatch_key → InputOutcome           │
│     consumers: 4 overlays (quick_open / global_search /     │
│                 hosts_picker / graph_branch_picker)         │
├─────────────────────────────────────────────────────────────┤
│ L1  input_edit_multi::dispatch_key_multi                    │
│     multi-line textarea: bare-Enter→\n, Up/Down across      │
│     lines, line-aware Home/End, Char('\r') swallow          │
│     consumer: 1 (commit-message textarea)                   │
├─────────────────────────────────────────────────────────────┤
│ L0  input_edit::dispatch_key{,_filtered} + free helpers     │
│     single-line readline / VSCode vocabulary: Alt/Ctrl+←/→  │
│     word-jump, Home/End, Ctrl+A/E, Alt+B/F/D word-jump/     │
│     delete, Backspace + Alt/Ctrl/Ctrl+W word-back,          │
│     Delete + Alt/Ctrl/Alt+D word-forward, Ctrl+U clear,     │
│     plain char insert, UTF-8 boundary safe                  │
│     consumers: every input handler in reef                  │
└─────────────────────────────────────────────────────────────┘
```

`tree_edit` / `db_goto` use `dispatch_key_filtered` (L0) with a
content predicate (e.g. digit-only, or "reject `/` `\` NUL").

`find_widget` / `search.rs` (vim `/`) / `settings` editor edit /
search-tab find+replace inputs use bare `dispatch_key` (L0).

## Why these layers exist

Pre-unification each input had its own ~80-line key table calling the
same `input_edit::*` helpers. Inconsistencies leaked in: hosts_picker
had no cursor (only `push/pop`), some pickers handled Alt+B but not
Alt+Backspace, the commit textarea silently dropped pastes, etc.
Migration collapsed all 12 input sites into one of the three layers
above; net diff was −700 lines of duplicated key dispatch.

## Adding a new text input

### Single-line, no list (e.g. inline rename, value prompt)

Hold `(text: String, cursor: usize)` on a state struct. In the input
handler:

```rust
// Phase 1: this-input-only shortcuts (Esc, Enter, etc.)
match key.code {
    KeyCode::Esc => { /* cancel */ return; }
    KeyCode::Enter if !ctrl && !alt && !shift => {
        /* commit on STRICT bare Enter — modifier-bearing Enter is
           reserved so accidental Shift+Enter doesn't commit a
           half-typed value. See R9/R10 in past reviews. */
        return;
    }
    _ => {}
}

// Phase 2: shared editor vocabulary
let _ = crate::input_edit::dispatch_key(&key, &mut state.text, &mut state.cursor);
```

If the buffer has a content predicate (digits only, file-name chars
only, etc.), swap `dispatch_key` → `dispatch_key_filtered` with the
predicate. Rejected chars return `Outcome::Rejected` (distinct from
`CursorOnly` so callers using `Edited`-driven recompute hooks don't
fire on a reject).

### Single-line picker overlay (filter + list)

Embed `picker_core::PickerCore` and write `visible_rows()` /
`confirm()`:

```rust
pub struct MyPickerState {
    pub core: crate::picker_core::PickerCore,
    pub all_rows: Vec<MyRow>,
    // … domain-specific fields …
}

impl MyPickerState {
    pub fn open(&mut self, all_rows: Vec<MyRow>) {
        self.all_rows = all_rows;
        self.core.open();
    }
    pub fn close(&mut self) { self.core.close(); }
    pub fn visible_rows(&self) -> Vec<MyRow> { /* filter against core.filter */ }
    pub fn confirm(&self) -> Option<MyAction> {
        self.visible_rows().get(self.core.selected_idx).map(/* … */)
    }
}
```

Handler:

```rust
fn handle_key_my_picker(key: KeyEvent, app: &mut App) {
    use crate::picker_core::InputOutcome;
    // Bespoke shortcuts FIRST (leader chord, mode toggle, etc.)
    // Then delegate:
    let visible = app.my_picker.visible_rows().len();
    match app.my_picker.core.dispatch_key(&key, visible) {
        InputOutcome::Cancel => app.close_my_picker(),
        InputOutcome::Quit   => { app.close_my_picker(); app.should_quit = true; }
        InputOutcome::Confirm => app.confirm_my_picker(),
        InputOutcome::Edited
        | InputOutcome::Rejected
        | InputOutcome::SelectionMoved
        | InputOutcome::CursorMoved
        | InputOutcome::Unhandled => {}
    }
}
```

The `match` MUST list every `InputOutcome` variant. `Rejected` is
unreachable today (PickerCore uses non-filtered `dispatch_key`) but
the explicit arm is the compile-time net for a future swap.

### Multi-line textarea

Hold `(text: String, cursor: usize)` and route through
`input_edit_multi::dispatch_key_multi`. The handler must
**unconditionally consume** Ctrl+letter / Alt+letter chords (return
true / `Edited`) so the outer handler's unguarded letter arms don't
fire mid-message. See `handle_key_git_commit` for the canonical
template — especially the `Char(_)` + (ctrl || alt) swallow guard on
the Unhandled path.

For paste: route through the appropriate `handle_paste` branch in
`src/input.rs`. Don't depend on `Char('\r')` / `Char('\n')` events;
many terminals deliver bracketed-paste payloads instead, and
non-bracketed terminals deliver discrete key events that the
defensive `Char('\r') → CursorOnly` arm in `dispatch_key_multi`
swallows.

## Mandatory invariants

These have all been violated and re-fixed at least once during the
unification work. Don't repeat the bugs:

1. **Strict bare Enter** for any "commit / accept" action that
   destroys editing state. Shift+Enter, Alt+Enter, Ctrl+Enter are
   reserved (modifier-bearing variants are universal "soft newline"
   muscle memory). Exception: in textareas, `dispatch_key_multi`'s
   `Enter if !ctrl` correctly treats Shift+Enter / Alt+Enter as
   newline; only bare `Ctrl+Enter` is "submit" (and the caller
   intercepts before delegating).
2. **Multi-line handlers MUST swallow Ctrl/Alt+Char chords on
   Unhandled.** The outer Git / Files / Graph handlers have
   `KeyCode::Char('s' | 't' | 'r' | …) =>` arms with NO modifier
   guards. A textarea that returns `false` on Unhandled leaks
   Ctrl+S etc. to those arms and silently stages files mid-message.
3. **Pickers MUST close on `confirm()` returning None.** Filter
   matching zero rows + Enter must not trap the keyboard. Close
   FIRST, then act on the `Option`.
4. **Edit-derived work fires on `Outcome::Edited` ONLY.** Use the
   pre/post `text.len()` downgrade built into `dispatch_key{,_filtered}`
   — a no-op Backspace at cursor=0 returns `CursorOnly` so
   PickerCore's `Edited → selected_idx = 0` doesn't reset the row
   cursor on a non-edit.
5. **Cursor is a byte offset into `text`** that must always sit on a
   UTF-8 char boundary. Use `input_edit::prev_char_boundary` /
   `next_char_boundary` if you ever set `cursor` manually outside
   `dispatch_key`. Renderers slicing `&text[..cursor]` will panic
   mid-codepoint.
6. **PickerCore consumers must list every `InputOutcome` variant.**
   When adding a new variant (`Quit`, `Rejected` were added in
   review rounds), the compiler catches missed arms only if the
   `match` is exhaustive. Avoid `_ => {}` wildcards on `InputOutcome`.
7. **No protocol bumps lurking elsewhere.** `Request::ListCommits`
   gained a required `scope` field at v10; if you change any
   `Request` / `Response` field shape on `crates/reef-proto`, bump
   `PROTOCOL_VERSION` AND document the migration so handshake fails
   loudly rather than degrading silently.

## Picker-specific gotchas (graph_branch_picker)

- `visible_rows()` is memoised in `RefCell<Option<(CacheKey, Vec<…>)>>`.
  The cache key is `(lowercased_filter, all_branches.len(),
  recent.len())` — push/pop on either Vec auto-invalidates.
  In-place mutation of an element does NOT invalidate; call
  `mark_dirty()` if a future writer needs that.
- The renderer auto-scrolls `selected_idx` into view. New picker
  overlays should clone this pattern (`scroll: usize` field +
  `if sel < scroll { scroll = sel; } else if sel >= scroll + list_h
  { scroll = sel + 1 - list_h }`); without it, monorepo branch lists
  push the highlight off-screen on autorepeat.
- Cold-start guard: `open_graph_branch_picker` refuses to open while
  `ref_map.is_empty()` AND auto-falls-back to AllRefs when the
  persisted Branch scope's ref isn't in ref_map. Both prevent a
  silent overwrite of the user's persisted scope.

## Where to find the canonical example for each layer

| Need | Look at |
|---|---|
| L0 single-line | `src/find_widget.rs` (handle_key) — cleanest two-phase pattern |
| L0 filtered | `src/input.rs::handle_key_db_goto` — digit-only predicate |
| L1 multi-line | `src/input.rs::handle_key_git_commit` — Ctrl/Alt+Char swallow guard included |
| L2 picker | `src/input.rs::handle_key_graph_branch_picker` — minimal InputOutcome translation; or `src/input.rs::handle_key_hosts_picker` for the double-buffer (filter + path mode) variant |

Tests live in `src/{input_edit,input_edit_multi,picker_core,
graph_branch_picker}::tests` plus integration coverage under
`tests/`. When adding a new text input, mirror the existing test
patterns: filter + cursor / autorepeat / UTF-8 boundary / no-op
edits / Ctrl+letter swallow / strict-Enter modifier handling.
