//! VSCode-style quick-open palette (bound to Space-P): fuzzy search every
//! file in the workdir (honouring `.gitignore`) and jump to it in the Files
//! tab preview.
//!
//! The state machine mirrors `search.rs`'s "active prompt owns input" pattern:
//! while `active` is true, `input::handle_key` delegates all key events here
//! and the overlay renders on top of the normal UI. Unlike `search.rs` this
//! never mutates backing panel scroll/selection until the user confirms with
//! Enter — so Esc cleanly drops back to exactly what was on screen.
//!
//! Index & MRU:
//! - Index is (re)built lazily on `begin()` when `index_stale` is set. A fresh
//!   `ignore::WalkBuilder` walk pulls every non-`.git` file under the workdir,
//!   respecting every ignore layer (`.gitignore`, `.git/info/exclude`,
//!   global), then caches the UTF-32 encoding nucleo needs so typing stays
//!   hot on large repos.
//! - MRU is an ordered dedup of recently accepted paths, capped at 50 and
//!   persisted via the same flat-kv prefs file the rest of the app uses. We
//!   sanitise `\t` / `\n` in paths on write to keep the file parseable; those
//!   characters in real-world paths are exotic enough to be worth the edge.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crate::app::{App, Tab};
use crate::input::DOUBLE_CLICK_WINDOW;
use crate::input_edit;
use crate::keymap::{Command, InputScope, Keymap};
use crate::prefs;
use crate::ui::mouse::ClickAction;
use reef_core::quick_open::{MRU_MAX, MRU_PREF_KEY};
use reef_io::{Backend, WalkOpts};

/// One file that can be matched. `display` is the workdir-relative path as it
/// appears in the UI; `utf32` is the same string pre-encoded to the form
/// nucleo consumes, so filter() doesn't re-encode every keystroke.
pub type Candidate = reef_core::quick_open::QuickOpenCandidate;

/// A single filtered hit. `indices` are character positions in
/// `index[idx].display` that matched — fed to the renderer for highlighting.
pub type MatchEntry = reef_core::quick_open::QuickOpenMatch;

pub struct QuickOpenState {
    /// Shared overlay scaffolding (`active`, `query` → `core.filter`,
    /// `cursor`, `selected` → `core.selected_idx`, `last_popup_area`).
    /// Edits route through `PickerCore::dispatch_key`.
    pub core: crate::picker_core::PickerCore,
    pub scroll: usize,
    pub index: Vec<Candidate>,
    /// fs-watcher flips this on; `begin()` rebuilds and clears it. Avoids
    /// re-walking the tree on every unrelated fs event.
    pub index_stale: bool,
    pub matches: Vec<MatchEntry>,
    pub mru: VecDeque<PathBuf>,
    /// Last-rendered list viewport height in rows; used by PageUp/PageDown
    /// to pick a step size. Mirrors `last_preview_view_h` etc. in `App`.
    pub last_view_h: u16,
    /// Timestamp of the most recent bare-Space keystroke inside the palette.
    /// Drives the in-palette half of the Space-P chord so the user can
    /// toggle the palette closed without reaching for Esc. Only armed when
    /// `core.filter.is_empty()` so Space becomes a literal char once the
    /// user is actually searching for something with a space in it.
    pub space_leader_at: Option<Instant>,
}

impl Default for QuickOpenState {
    fn default() -> Self {
        Self {
            core: crate::picker_core::PickerCore::default(),
            scroll: 0,
            index: Vec::new(),
            index_stale: true,
            matches: Vec::new(),
            mru: VecDeque::new(),
            last_view_h: 0,
            space_leader_at: None,
        }
    }
}

impl QuickOpenState {
    /// Build state from persisted prefs (loads the MRU). Index stays empty
    /// and stale — it'll be walked on the first `begin()` call so startup
    /// time isn't spent on an index the user may never open.
    pub fn from_prefs() -> Self {
        Self {
            mru: load_mru_from_prefs(),
            ..Self::default()
        }
    }
}

// ─── Entry points ────────────────────────────────────────────────────────────

/// Open the palette. Rebuilds the index if stale (first open, or fs-watcher
/// saw changes since the last build). Preserves `query` across close/reopen
/// so the user can Esc to peek at something and come back.
pub fn begin(app: &mut App) {
    if app.quick_open.index_stale || app.quick_open.index.is_empty() {
        app.quick_open.index = rebuild_index_via_backend(app.backend.as_ref());
        app.quick_open.index_stale = false;
    }
    app.quick_open.core.active = true;
    app.quick_open.core.selected_idx = 0;
    app.quick_open.scroll = 0;
    // Position cursor at end so the first keystroke continues (not splits)
    // the existing query.
    app.quick_open.core.cursor = app.quick_open.core.filter.len();
    // Start with a clean leader slot — a stale timestamp from a previous
    // session would make the first Space-after-open surprisingly close the
    // palette.
    app.quick_open.space_leader_at = None;
    filter(&mut app.quick_open);
}

/// Commit the current selection: update MRU, close the palette, and jump
/// the Files tab to the chosen file with a fresh preview loaded.
pub fn accept(app: &mut App) {
    let Some(m) = app.quick_open.matches.get(app.quick_open.core.selected_idx) else {
        app.quick_open.core.active = false;
        return;
    };
    let Some(cand) = app.quick_open.index.get(m.idx) else {
        app.quick_open.core.active = false;
        return;
    };
    let rel = cand.rel_path.clone();

    reef_core::quick_open::bump_mru(&mut app.quick_open.mru, rel.clone(), MRU_MAX);
    save_mru_to_prefs(&app.quick_open.mru);

    app.push_location_before_jump();
    app.quick_open.core.active = false;
    app.set_active_tab(Tab::Files);
    app.file_tree.reveal(&rel);
    app.refresh_file_tree_with_target(Some(rel.clone()));
    app.load_preview_for_path(rel);
}

/// Dispatch one key while the palette is active. The caller (input.rs)
/// guarantees exclusivity — no other handler sees these keys.
///
/// Binding map:
/// - `Esc`                                 close palette (keeps query for re-open)
/// - `Ctrl+C`                              close + quit app
/// - `Enter`                               accept selected
/// - `Backspace`                           delete char
/// - `Alt+Backspace` / `Ctrl+Backspace` / `Ctrl+W`  delete previous word
/// - `Ctrl+U`                              clear the whole query
/// - `Up` / `Ctrl+P` / `Ctrl+K`            previous candidate
/// - `Down` / `Ctrl+N` / `Ctrl+J`          next candidate
/// - `PageUp` / `PageDown`                 page by viewport height
/// - `Left` / `Right` / `Home` / `End`     edit-cursor movement
///
/// Historic note: an earlier revision made `Ctrl+P` close the palette
/// (toggle-on, toggle-off). That conflicted with users' expectation that
/// `Ctrl+P` navigates inside the palette (VSCode / readline / vim parity),
/// so `Ctrl+P` now only means "previous candidate" — Esc is the sole
/// keyboard close.
pub fn handle_key(key: KeyEvent, app: &mut App) {
    // Space-leader close: mirrors the global open chord. Only armed while
    // `query.is_empty()` so once the user starts typing a path that might
    // legitimately contain a space (or a `p`), the chord shuts off and
    // characters go straight into the query.
    match crate::input::leader_decision(
        &key,
        /* allow_arm */ app.quick_open.core.filter.is_empty(),
        app.quick_open.space_leader_at,
        Instant::now(),
        crate::input::LEADER_TIMEOUT,
    ) {
        crate::input::LeaderVerdict::Arm => {
            app.quick_open.space_leader_at = Some(Instant::now());
            return;
        }
        crate::input::LeaderVerdict::Fire => {
            // `leader_decision` Fires on *any* chord target (p/f/h/v).
            // But quick-open's chord identity is Space+P — only that
            // pair should toggle the palette closed. For other chord
            // letters (`v` from FocusedPreview, `h` from help, etc.)
            // the user pressed Space-then-letter while the palette
            // happened to be empty; we don't want to swallow it. Treat
            // those as Consume so the literal char still appends to the
            // query below.
            app.quick_open.space_leader_at = None;
            if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
                app.quick_open.core.active = false;
                return;
            }
            // Fall through — non-P chord lands in the char-append arm.
        }
        crate::input::LeaderVerdict::Consume => {
            app.quick_open.space_leader_at = None;
            // Fall through — the current key still runs below.
        }
        crate::input::LeaderVerdict::None => {}
    }

    // PageUp/PageDown depend on the rendered viewport height
    // (`last_view_h`), which PickerCore doesn't know about — handle
    // them inline before delegating the rest to the shared core.
    let mapped = Keymap::resolve(InputScope::QuickOpen, &key);
    if matches!(mapped, Some(Command::PageUp | Command::PageDown)) {
        let step = app.quick_open.last_view_h.max(1) as i32;
        let signed = if mapped == Some(Command::PageUp) {
            -step
        } else {
            step
        };
        move_selection(&mut app.quick_open, signed);
        return;
    }

    // Shared picker dispatch. `Edited` re-runs the fuzzy filter; the
    // other outcomes are no-ops (PickerCore already updated state).
    use crate::picker_core::InputOutcome;
    let visible = app.quick_open.matches.len();
    match app
        .quick_open
        .core
        .dispatch_key(InputScope::QuickOpen, &key, visible)
    {
        InputOutcome::Cancel => app.quick_open.core.active = false,
        InputOutcome::Quit => {
            app.quick_open.core.active = false;
            app.should_quit = true;
        }
        InputOutcome::Confirm => accept(app),
        InputOutcome::Edited => filter(&mut app.quick_open),
        // Rejected unreachable today (PickerCore uses non-filtered
        // dispatch_key); the arm is here so a future swap forces
        // an explicit choice.
        InputOutcome::Rejected
        | InputOutcome::SelectionMoved
        | InputOutcome::CursorMoved
        | InputOutcome::Unhandled => {}
    }
}

/// Bracketed-paste arrival while the palette is active. Stripping newlines
/// keeps a multi-line paste from breaking the single-line query model; CRs
/// from Windows clipboards get the same treatment. Tabs stay as literal
/// characters — users searching for odd filenames can type them on purpose
/// and we don't want to drop that signal.
///
/// Called from `input::handle_paste` after the drop-path parser has already
/// ruled out the payload as a file drop.
pub fn handle_paste(s: &str, app: &mut App) {
    input_edit::paste_single_line(
        s,
        &mut app.quick_open.core.filter,
        &mut app.quick_open.core.cursor,
    );
    filter(&mut app.quick_open);
}

/// Dispatch one mouse event while the palette is active. The caller
/// (input.rs) routes all events here instead of the normal mouse pipeline,
/// so the underlying panels can't receive clicks through the overlay.
///
/// Semantics:
/// - Left click outside the popup area → close palette
/// - Left click on a row → select that candidate
/// - Double left-click on a row → select + accept (open file)
/// - Scroll wheel inside popup → move selection (3 rows per tick)
/// - Everything else (drag, right-click, move) → ignored
pub fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    let popup = match app.quick_open.core.last_popup_area {
        Some(r) => r,
        // No popup rendered yet — swallow the event without side-effects so
        // a spurious click during the first tick can't dismiss the palette.
        None => return,
    };
    let inside = mouse.column >= popup.x
        && mouse.column < popup.x + popup.width
        && mouse.row >= popup.y
        && mouse.row < popup.y + popup.height;

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !inside {
                // Click-away dismisses the palette, just like clicking
                // outside a dropdown in a GUI. Keeps the pre-palette state
                // intact (filter doesn't touch scroll/selection elsewhere).
                app.quick_open.core.active = false;
                app.last_click = None;
                return;
            }

            let now = Instant::now();
            let is_double = matches!(
                app.last_click,
                Some((t, c, r))
                    if c == mouse.column
                        && r == mouse.row
                        && now.duration_since(t) < DOUBLE_CLICK_WINDOW
            );

            // hit_test walks the registry in reverse registration order, and
            // the palette registers its rows AFTER the underlying panels, so
            // the palette's zones always win on overlap — meaning a click on
            // the popup never leaks through to a TreeClick / GitCommand
            // behind it.
            if let Some(ClickAction::QuickOpenSelect(idx)) =
                app.hit_registry.hit_test(mouse.column, mouse.row)
            {
                app.quick_open.core.selected_idx = idx;
                if is_double {
                    accept(app);
                    app.last_click = None;
                    return;
                }
            }

            // Track click for the next frame's double-click check.
            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::ScrollUp if inside => {
            move_selection(&mut app.quick_open, -3);
        }
        MouseEventKind::ScrollDown if inside => {
            move_selection(&mut app.quick_open, 3);
        }
        _ => {}
    }
}

// ─── Filtering ───────────────────────────────────────────────────────────────

/// Recompute `matches` from the current `query` and `index`. When `query` is
/// empty we surface MRU first (alive paths only) and then the rest of the
/// index in alphabetical order, so an empty palette is still useful. When
/// `query` is non-empty we delegate to nucleo and sort by score desc, with
/// shorter paths as a tiebreaker (keeps basename hits above deep-path hits).
pub fn filter(state: &mut QuickOpenState) {
    state.matches =
        reef_core::quick_open::filter_candidates(&state.index, &state.core.filter, &state.mru);

    // Query change resets the viewport so the top match is visible.
    state.core.selected_idx = 0;
    state.scroll = 0;
}

/// Mark the index as stale. Called from `App::tick` when fs-watcher fires —
/// cheaper than re-walking the tree on every event, and if the user never
/// opens the palette we never pay the walk cost at all.
pub fn mark_stale(state: &mut QuickOpenState) {
    state.index_stale = true;
}

// ─── Index construction ──────────────────────────────────────────────────────

/// Ask the backend to walk the workdir and build the palette candidate
/// list. For `LocalBackend` this is `ignore::WalkBuilder` over the cwd
/// (identical to the pre-M3 behaviour). For `RemoteBackend` this ships a
/// `WalkRepoPaths` RPC so the agent walks its own workdir and the reef
/// client never touches the remote filesystem directly.
fn rebuild_index_via_backend(backend: &dyn Backend) -> Vec<Candidate> {
    let opts = WalkOpts::default();
    let resp = match backend.walk_repo_paths(&opts) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    reef_core::quick_open::build_candidates(resp.paths)
}

/// Legacy direct-walk path kept only for the `rebuild_index_respects_gitignore`
/// regression test — production code routes through
/// `rebuild_index_via_backend`.
#[cfg(test)]
fn rebuild_index(root: &std::path::Path) -> Vec<Candidate> {
    use ignore::WalkBuilder;
    let mut out: Vec<Candidate> = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|dent| dent.file_name() != ".git")
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let display = rel.to_string_lossy().to_string();
        out.extend(reef_core::quick_open::build_candidates([display]));
    }
    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}

// ─── MRU persistence ─────────────────────────────────────────────────────────

fn load_mru_from_prefs() -> VecDeque<PathBuf> {
    let Some(raw) = prefs::get(MRU_PREF_KEY) else {
        return VecDeque::new();
    };
    reef_core::quick_open::decode_mru(&raw)
}

fn save_mru_to_prefs(mru: &VecDeque<PathBuf>) {
    prefs::set(MRU_PREF_KEY, &reef_core::quick_open::encode_mru(mru));
}

// ─── Input helpers ───────────────────────────────────────────────────────────
//
// Text-editing primitives (insert/backspace/delete_word_backward/clear/
// move_cursor) live in `crate::input_edit` and are shared with
// `crate::global_search`.

fn move_selection(state: &mut QuickOpenState, delta: i32) {
    if state.matches.is_empty() {
        state.core.selected_idx = 0;
        return;
    }
    let last = state.matches.len() - 1;
    let cur = state.core.selected_idx as i32;
    let next = (cur + delta).clamp(0, last as i32) as usize;
    state.core.selected_idx = next;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_state(paths: &[&str]) -> QuickOpenState {
        let index = reef_core::quick_open::build_candidates(paths.iter().map(|p| p.to_string()));
        QuickOpenState {
            index,
            index_stale: false,
            ..QuickOpenState::default()
        }
    }

    #[test]
    fn empty_query_lists_all_with_mru_first() {
        let mut s = mk_state(&["a/x.rs", "b/y.rs", "c/z.rs"]);
        s.mru.push_back(PathBuf::from("c/z.rs"));
        s.mru.push_back(PathBuf::from("a/x.rs"));
        filter(&mut s);
        assert_eq!(s.matches.len(), 3);
        // MRU order first, then the rest
        assert_eq!(s.index[s.matches[0].idx].display, "c/z.rs");
        assert_eq!(s.index[s.matches[1].idx].display, "a/x.rs");
        assert_eq!(s.index[s.matches[2].idx].display, "b/y.rs");
    }

    #[test]
    fn empty_query_skips_mru_entries_that_no_longer_exist() {
        let mut s = mk_state(&["a/x.rs", "b/y.rs"]);
        s.mru.push_back(PathBuf::from("ghost.rs"));
        s.mru.push_back(PathBuf::from("a/x.rs"));
        filter(&mut s);
        assert_eq!(s.matches.len(), 2);
        assert_eq!(s.index[s.matches[0].idx].display, "a/x.rs");
        assert_eq!(s.index[s.matches[1].idx].display, "b/y.rs");
    }

    #[test]
    fn subsequence_match_hits_camelcase() {
        let mut s = mk_state(&["src/ui/file_tree_panel.rs", "src/app.rs", "README.md"]);
        s.core.filter = "uiftp".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert!(!s.matches.is_empty());
        // The file_tree_panel path must rank first — it's the only one
        // containing the subsequence.
        assert_eq!(
            s.index[s.matches[0].idx].display,
            "src/ui/file_tree_panel.rs"
        );
    }

    #[test]
    fn shorter_path_wins_on_score_tie() {
        let mut s = mk_state(&["deep/nested/foo.rs", "foo.rs"]);
        s.core.filter = "foo".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert_eq!(s.index[s.matches[0].idx].display, "foo.rs");
    }

    #[test]
    fn non_match_is_excluded_when_query_nonempty() {
        let mut s = mk_state(&["alpha.rs", "beta.rs"]);
        s.core.filter = "zzz".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert!(s.matches.is_empty());
    }

    #[test]
    fn move_selection_clamps() {
        let mut s = mk_state(&["a.rs", "b.rs", "c.rs"]);
        filter(&mut s);
        move_selection(&mut s, 10);
        assert_eq!(s.core.selected_idx, 2);
        move_selection(&mut s, -99);
        assert_eq!(s.core.selected_idx, 0);
    }

    #[test]
    fn rebuild_index_respects_gitignore_and_prunes_dotgit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // Seed files: a tracked source file, a dotfile (should surface), a
        // gitignored build artefact (should be excluded), and a fake .git
        // entry (must never appear).
        std::fs::write(root.join("src.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/build.o"), "binary").unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

        let index = rebuild_index(root);
        let displays: Vec<&str> = index.iter().map(|c| c.display.as_str()).collect();

        assert!(displays.contains(&"src.rs"), "tracked source must appear");
        assert!(
            displays.contains(&".gitignore"),
            "dotfiles must appear — hidden(false)"
        );
        assert!(
            !displays.iter().any(|d| d.starts_with("target/")),
            "gitignored target/ must be excluded"
        );
        assert!(
            !displays.iter().any(|d| d.starts_with(".git/")),
            ".git/ must never appear in the palette"
        );
    }
}
