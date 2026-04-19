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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ignore::WalkBuilder;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use ratatui::layout::Rect;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::app::{App, Tab};
use crate::input::DOUBLE_CLICK_WINDOW;
use crate::input_edit;
use crate::prefs;
use crate::ui::mouse::ClickAction;

const MRU_MAX: usize = 50;
const MRU_PREF_KEY: &str = "quickopen.mru";
const MRU_SEP: char = '\t';

/// One file that can be matched. `display` is the workdir-relative path as it
/// appears in the UI; `utf32` is the same string pre-encoded to the form
/// nucleo consumes, so filter() doesn't re-encode every keystroke.
pub struct Candidate {
    pub rel_path: PathBuf,
    pub display: String,
    utf32: Utf32String,
}

/// A single filtered hit. `indices` are character positions in
/// `index[idx].display` that matched — fed to the renderer for highlighting.
#[derive(Clone)]
pub struct MatchEntry {
    pub idx: usize,
    pub score: u32,
    pub indices: Vec<u32>,
}

pub struct QuickOpenState {
    pub active: bool,
    pub query: String,
    /// Byte offset into `query`. Always on a char boundary.
    pub cursor: usize,
    pub selected: usize,
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
    /// Last-rendered popup bounds. Read by the mouse dispatcher to decide
    /// whether a click landed inside the palette (stays in palette mode) or
    /// outside (dismisses). `None` means the popup hasn't been rendered yet —
    /// any click in that window is treated as "inside" so the first click
    /// after opening never accidentally dismisses.
    pub last_popup_area: Option<Rect>,

    /// Timestamp of the most recent bare-Space keystroke inside the palette.
    /// Drives the in-palette half of the Space-P chord so the user can
    /// toggle the palette closed without reaching for Esc. Only armed when
    /// `query.is_empty()` so that Space becomes a literal char once the
    /// user is actually searching for something with a space in it.
    pub space_leader_at: Option<Instant>,
}

impl Default for QuickOpenState {
    fn default() -> Self {
        Self {
            active: false,
            query: String::new(),
            cursor: 0,
            selected: 0,
            scroll: 0,
            index: Vec::new(),
            index_stale: true,
            matches: Vec::new(),
            mru: VecDeque::new(),
            last_view_h: 0,
            last_popup_area: None,
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
    let root = app.file_tree.root.clone();
    if app.quick_open.index_stale || app.quick_open.index.is_empty() {
        app.quick_open.index = rebuild_index(&root);
        app.quick_open.index_stale = false;
    }
    app.quick_open.active = true;
    app.quick_open.selected = 0;
    app.quick_open.scroll = 0;
    // Position cursor at end so the first keystroke continues (not splits)
    // the existing query.
    app.quick_open.cursor = app.quick_open.query.len();
    // Start with a clean leader slot — a stale timestamp from a previous
    // session would make the first Space-after-open surprisingly close the
    // palette.
    app.quick_open.space_leader_at = None;
    filter(&mut app.quick_open);
}

/// Commit the current selection: update MRU, close the palette, and jump
/// the Files tab to the chosen file with a fresh preview loaded.
pub fn accept(app: &mut App) {
    let Some(m) = app.quick_open.matches.get(app.quick_open.selected) else {
        app.quick_open.active = false;
        return;
    };
    let Some(cand) = app.quick_open.index.get(m.idx) else {
        app.quick_open.active = false;
        return;
    };
    let rel = cand.rel_path.clone();

    // MRU: move-to-front with dedup, cap at MRU_MAX.
    app.quick_open.mru.retain(|p| p != &rel);
    app.quick_open.mru.push_front(rel.clone());
    while app.quick_open.mru.len() > MRU_MAX {
        app.quick_open.mru.pop_back();
    }
    save_mru_to_prefs(&app.quick_open.mru);

    app.quick_open.active = false;
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
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Space-leader close: mirrors the global open chord. Only armed while
    // `query.is_empty()` so once the user starts typing a path that might
    // legitimately contain a space (or a `p`), the chord shuts off and
    // characters go straight into the query.
    match crate::input::leader_decision(
        &key,
        /* allow_arm */ app.quick_open.query.is_empty(),
        app.quick_open.space_leader_at,
        Instant::now(),
        crate::input::LEADER_TIMEOUT,
    ) {
        crate::input::LeaderVerdict::Arm => {
            app.quick_open.space_leader_at = Some(Instant::now());
            return;
        }
        crate::input::LeaderVerdict::Fire => {
            app.quick_open.space_leader_at = None;
            app.quick_open.active = false;
            return;
        }
        crate::input::LeaderVerdict::Consume => {
            app.quick_open.space_leader_at = None;
            // Fall through — the current key still runs below.
        }
        crate::input::LeaderVerdict::None => {}
    }

    match key.code {
        KeyCode::Esc => {
            app.quick_open.active = false;
        }
        KeyCode::Char('c') if ctrl => {
            app.quick_open.active = false;
            app.should_quit = true;
        }
        KeyCode::Enter => accept(app),

        // ── Deletion ─────────────────────────────────────────────
        // Alt+Backspace (macOS Option+Backspace) and Ctrl+Backspace
        // (Windows/Linux) both ask for "delete previous word". Crossterm
        // only surfaces these modifiers when the terminal uses a kitty /
        // fixterms-style protocol; older terminals collapse Alt+Backspace
        // onto plain Backspace, so Ctrl+W stays as the reliable fallback.
        KeyCode::Backspace if alt || ctrl => {
            input_edit::delete_word_backward(&mut app.quick_open.query, &mut app.quick_open.cursor);
            filter(&mut app.quick_open);
        }
        KeyCode::Char('w') if ctrl => {
            input_edit::delete_word_backward(&mut app.quick_open.query, &mut app.quick_open.cursor);
            filter(&mut app.quick_open);
        }
        KeyCode::Char('u') if ctrl => {
            input_edit::clear(&mut app.quick_open.query, &mut app.quick_open.cursor);
            filter(&mut app.quick_open);
        }
        KeyCode::Backspace => {
            input_edit::backspace(&mut app.quick_open.query, &mut app.quick_open.cursor);
            filter(&mut app.quick_open);
        }

        // ── List navigation ──────────────────────────────────────
        KeyCode::Up => move_selection(&mut app.quick_open, -1),
        KeyCode::Char('p') if ctrl => move_selection(&mut app.quick_open, -1),
        KeyCode::Char('k') if ctrl => move_selection(&mut app.quick_open, -1),
        KeyCode::Down => move_selection(&mut app.quick_open, 1),
        KeyCode::Char('n') if ctrl => move_selection(&mut app.quick_open, 1),
        KeyCode::Char('j') if ctrl => move_selection(&mut app.quick_open, 1),
        KeyCode::PageUp => {
            let step = app.quick_open.last_view_h.max(1) as i32;
            move_selection(&mut app.quick_open, -step);
        }
        KeyCode::PageDown => {
            let step = app.quick_open.last_view_h.max(1) as i32;
            move_selection(&mut app.quick_open, step);
        }

        // ── Edit-cursor movement ─────────────────────────────────
        KeyCode::Left => {
            input_edit::move_cursor(&app.quick_open.query, &mut app.quick_open.cursor, -1);
        }
        KeyCode::Right => {
            input_edit::move_cursor(&app.quick_open.query, &mut app.quick_open.cursor, 1);
        }
        KeyCode::Home => {
            app.quick_open.cursor = 0;
        }
        KeyCode::End => {
            app.quick_open.cursor = app.quick_open.query.len();
        }

        // Any other Ctrl-combo is a no-op; we don't want Ctrl+A etc.
        // landing as a literal 'a' in the query.
        KeyCode::Char(c) if !ctrl => {
            input_edit::insert_char(&mut app.quick_open.query, &mut app.quick_open.cursor, c);
            filter(&mut app.quick_open);
        }
        _ => {}
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
    for c in s.chars() {
        if c == '\n' || c == '\r' {
            continue;
        }
        input_edit::insert_char(&mut app.quick_open.query, &mut app.quick_open.cursor, c);
    }
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
    let popup = match app.quick_open.last_popup_area {
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
                app.quick_open.active = false;
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
                app.quick_open.selected = idx;
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
    state.matches.clear();

    if state.query.is_empty() {
        let mut seen: HashSet<usize> = HashSet::new();
        for path in &state.mru {
            if let Some(idx) = state.index.iter().position(|c| &c.rel_path == path) {
                state.matches.push(MatchEntry {
                    idx,
                    score: 0,
                    indices: Vec::new(),
                });
                seen.insert(idx);
            }
        }
        for idx in 0..state.index.len() {
            if !seen.contains(&idx) {
                state.matches.push(MatchEntry {
                    idx,
                    score: 0,
                    indices: Vec::new(),
                });
            }
        }
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(&state.query, CaseMatching::Smart, Normalization::Smart);
        for (idx, cand) in state.index.iter().enumerate() {
            let mut indices: Vec<u32> = Vec::new();
            if let Some(score) = pattern.indices(cand.utf32.slice(..), &mut matcher, &mut indices) {
                state.matches.push(MatchEntry {
                    idx,
                    score,
                    indices,
                });
            }
        }
        // Primary: score desc. Tiebreak: shorter path (basename hits beat
        // deep-path hits with the same score). Secondary tiebreak:
        // lexicographic so the order is stable.
        state.matches.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                let la = state.index[a.idx].display.len();
                let lb = state.index[b.idx].display.len();
                la.cmp(&lb)
                    .then_with(|| state.index[a.idx].display.cmp(&state.index[b.idx].display))
            })
        });
    }

    // Query change resets the viewport so the top match is visible.
    state.selected = 0;
    state.scroll = 0;
}

/// Mark the index as stale. Called from `App::tick` when fs-watcher fires —
/// cheaper than re-walking the tree on every event, and if the user never
/// opens the palette we never pay the walk cost at all.
pub fn mark_stale(state: &mut QuickOpenState) {
    state.index_stale = true;
}

// ─── Index construction ──────────────────────────────────────────────────────

fn rebuild_index(root: &Path) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // `hidden(false)` so dotfiles (`.gitignore`, `.vimrc`, …) surface, which
    // matches VSCode's Ctrl+P. We still need to prune `.git` itself — that's
    // version-control metadata, not source you'd ever want to open through
    // this palette. `filter_entry` prunes the whole subtree at the matching
    // directory, so the walker never descends into it.
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|dent| {
            let name = dent.file_name();
            name != ".git"
        })
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
        if display.is_empty() {
            continue;
        }
        let utf32 = Utf32String::from(display.as_str());
        out.push(Candidate {
            rel_path: rel.to_path_buf(),
            display,
            utf32,
        });
    }

    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}

// ─── MRU persistence ─────────────────────────────────────────────────────────

fn load_mru_from_prefs() -> VecDeque<PathBuf> {
    let Some(raw) = prefs::get(MRU_PREF_KEY) else {
        return VecDeque::new();
    };
    raw.split(MRU_SEP)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn save_mru_to_prefs(mru: &VecDeque<PathBuf>) {
    // The prefs file uses `key=value\n` lines split via `split_once('=')` and
    // per-line trim, so values mustn't contain `\n`, and we use `\t` as our
    // in-value separator. Either character in a real path is pathological,
    // but we replace them defensively so one weird path can't corrupt the
    // whole prefs file.
    let joined: String = mru
        .iter()
        .map(|p| p.to_string_lossy().replace(['\t', '\n'], " "))
        .collect::<Vec<_>>()
        .join(&MRU_SEP.to_string());
    prefs::set(MRU_PREF_KEY, &joined);
}

// ─── Input helpers ───────────────────────────────────────────────────────────
//
// Text-editing primitives (insert/backspace/delete_word_backward/clear/
// move_cursor) live in `crate::input_edit` and are shared with
// `crate::global_search`.

fn move_selection(state: &mut QuickOpenState, delta: i32) {
    if state.matches.is_empty() {
        state.selected = 0;
        return;
    }
    let last = state.matches.len() - 1;
    let cur = state.selected as i32;
    let next = (cur + delta).clamp(0, last as i32) as usize;
    state.selected = next;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_state(paths: &[&str]) -> QuickOpenState {
        let index = paths
            .iter()
            .map(|p| Candidate {
                rel_path: PathBuf::from(p),
                display: p.to_string(),
                utf32: Utf32String::from(*p),
            })
            .collect();
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
        s.query = "uiftp".to_string();
        s.cursor = s.query.len();
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
        s.query = "foo".to_string();
        s.cursor = s.query.len();
        filter(&mut s);
        assert_eq!(s.index[s.matches[0].idx].display, "foo.rs");
    }

    #[test]
    fn non_match_is_excluded_when_query_nonempty() {
        let mut s = mk_state(&["alpha.rs", "beta.rs"]);
        s.query = "zzz".to_string();
        s.cursor = s.query.len();
        filter(&mut s);
        assert!(s.matches.is_empty());
    }

    #[test]
    fn move_selection_clamps() {
        let mut s = mk_state(&["a.rs", "b.rs", "c.rs"]);
        filter(&mut s);
        move_selection(&mut s, 10);
        assert_eq!(s.selected, 2);
        move_selection(&mut s, -99);
        assert_eq!(s.selected, 0);
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
