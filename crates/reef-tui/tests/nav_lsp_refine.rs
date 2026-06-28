//! Phase 3 unit tests for the LSP refine cache. We don't actually
//! spawn rust-analyzer here (CI would have to ship it; way too heavy);
//! instead we test the cache contract directly: writing an
//! `LspLocation` for a `(lang, identifier)` makes the next `gd` on the
//! same identifier prefer the LSP answer over the tree-sitter result.

use reef::TuiApp as App;
use reef::ui::selection::PreviewSelection;
use reef::ui::theme::Theme;
use reef_app::{
    AppPanel as Panel, AppTab as Tab, CursorPosition, LocationSnapshot, LocationSurface, NavAnchor,
    NavPendingJump, ScrollPosition,
};
use reef_core::nav::{LspBadge, LspLocation, NavLang, parse_file_if_supported};
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent, TextPreview};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn file_preview_snapshot(path: &str, line: usize, byte_col: usize) -> LocationSnapshot {
    LocationSnapshot {
        surface: LocationSurface::FilePreview,
        path: std::path::PathBuf::from(path),
        cursor: CursorPosition { line, byte_col },
        scroll: ScrollPosition {
            vertical: 0,
            horizontal: 0,
        },
    }
}

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    app.engine.state.active_tab = Tab::Files;
    app.engine.state.active_panel = Panel::Diff;
    app.layout.last_preview_view_h = 20;
    (app, tmp, g)
}

fn install_rust_preview(app: &mut App, path: &str, src: &str) {
    let bytes: Arc<[u8]> = Arc::from(src.as_bytes().to_vec().into_boxed_slice());
    let parsed = parse_file_if_supported(NavLang::Rust, bytes).map(Arc::new);
    app.engine.state.preview_content = Some(
        PreviewContent {
            path: path.to_string(),
            body: PreviewBody::Text(TextPreview {
                lines: src.lines().map(|s| s.to_string()).collect(),
                highlighted: None,
                parsed,
            }),
        }
        .into(),
    );
}

fn set_keyboard_cursor(app: &mut App, line: usize, byte_col: usize) {
    let mut sel = PreviewSelection::new((line, byte_col));
    sel.active = (line, byte_col);
    sel.dragging = false;
    app.preview_selection = Some(sel);
}

fn cursor_at_nth(src: &str, needle: &str, n: usize) -> (usize, usize) {
    let mut start = 0usize;
    let mut idx = None;
    for _ in 0..=n {
        let next = src[start..].find(needle).expect("needle present");
        idx = Some(start + next);
        start += next + needle.len();
    }
    let idx = idx.unwrap();
    let prefix = &src[..idx];
    let row = prefix.bytes().filter(|b| *b == b'\n').count();
    let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    (row, idx - line_start)
}

#[test]
fn refine_cache_overrides_tree_sitter_intra_file_result() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // Avoid identifier substring overlap (e.g. `helper` inside
    // `helper_unused`) — the fixture-cursor helper uses naive substring
    // find, so distinct names keep its hit count predictable.
    let src = "\
fn helper() -> i32 { 42 }
fn other_fn() -> i32 { 99 }
fn main() { let _ = helper(); }
";
    install_rust_preview(&mut app, "scratch.rs", src);

    // Click on `helper` in the call site.
    let cursor = cursor_at_nth(src, "helper", 1);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    // Pre-seed the cache under the POSITION key the lookup uses
    // (`lang, path:line:col`), with a synthetic LSP location at line 5
    // (the fixture has 3 lines) so a cache HIT is unmistakable. The
    // cached path is workdir-relative (the production handler converts
    // before storing).
    let key = reef_core::nav::refine_key(std::path::Path::new("scratch.rs"), cursor);
    app.engine.state.nav_refine_cache.insert(
        (NavLang::Rust, key),
        LspLocation {
            path: std::path::PathBuf::from("scratch.rs"),
            line: 5,
            character: 0,
            character_end: 0,
        },
    );

    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    // The refine cache hit should make the jump go to the cached
    // line (5) instead of tree-sitter's intra-file find (line 0).
    let hl = app
        .engine
        .state
        .preview_highlight
        .as_ref()
        .expect("highlight after refine-cache hit");
    assert_eq!(
        hl.row, 5,
        "expected to honour the refine cache, not fall back to tree-sitter"
    );
}

#[test]
fn refine_cache_empty_falls_back_to_tree_sitter() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() {}\nfn main() { helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);
    assert!(app.engine.state.nav_refine_cache.is_empty());

    let cursor = cursor_at_nth(src, "helper", 1);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);
    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    let hl = app
        .engine
        .state
        .preview_highlight
        .as_ref()
        .expect("tree-sitter jump");
    assert_eq!(hl.row, 0, "fell back to the tree-sitter definition");
}

/// Lock down the per-language LSP profile registry. Adding a language
/// without adding a profile entry will fail at compile time (the
/// `NavLang::profile()` match is exhaustive); regressing an existing
/// profile fails here.
#[test]
fn lang_profiles_cover_every_supported_lsp() {
    use reef_core::nav::NavLang;
    // Every language we ship today has a tree-sitter grammar AND an
    // LSP profile. Phase 4 may add languages with grammar but no LSP
    // (then this assertion is loosened to per-lang opt-in).
    for &lang in NavLang::ALL {
        let profile = lang.profile();
        assert!(
            !profile.display_name.is_empty(),
            "display name missing for {:?}",
            lang
        );
        assert_eq!(
            profile.badge_glyph.chars().count(),
            2,
            "badge glyph for {:?} must be exactly 2 chars to fit the status bar",
            lang
        );
        let lsp = profile
            .lsp
            .as_ref()
            .unwrap_or_else(|| panic!("v1 ships an LSP profile for {:?}", lang));
        assert!(!lsp.bin.is_empty(), "lsp.bin empty for {:?}", lang);
        assert!(
            !lsp.language_id.is_empty(),
            "lsp.language_id empty for {:?}",
            lang
        );
    }
}

/// Guard against `SettingItem::ALL` drifting from `NavLang::ALL` — the
/// Settings "Code Navigation" section must have exactly one row per
/// supported language. Adding a `NavLang` without a Settings row (or
/// vice versa) fails here instead of silently shipping a language with
/// no install UI.
#[test]
fn settings_has_one_lsp_row_per_language() {
    use reef::settings::SettingItem;
    use reef_core::nav::NavLang;
    use std::collections::HashSet;
    let rows: Vec<NavLang> = SettingItem::ALL
        .iter()
        .filter_map(|item| match item {
            SettingItem::Lsp(lang) => Some(*lang),
            _ => None,
        })
        .collect();
    // Length check catches a duplicate row (a set comparison alone
    // wouldn't); set comparison catches a missing / extra language.
    assert_eq!(
        rows.len(),
        NavLang::ALL.len(),
        "exactly one Settings LSP row per language (no dups / omissions)"
    );
    let rows_set: HashSet<NavLang> = rows.into_iter().collect();
    let expected: HashSet<NavLang> = NavLang::ALL.iter().copied().collect();
    assert_eq!(
        rows_set, expected,
        "SettingItem::ALL LSP rows must match NavLang::ALL exactly"
    );
}

/// Vue is the canonical LSP-only language: tree-sitter can't see
/// inside the `<script>` raw_text blob, so `goto_definition_at_cursor`
/// must route to the pending-jump path. This test confirms the
/// pending slot gets populated AND that the cache is consulted on a
/// subsequent click at the same position.
#[test]
fn vue_goto_registers_pending_jump_and_uses_cache_on_repeat() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.engine.state.active_tab = Tab::Files;
    app.engine.state.active_panel = Panel::Diff;
    app.layout.last_preview_view_h = 20;

    let src = "<template>\n  <h1>{{ msg }}</h1>\n</template>\n\
               <script setup>\nconst msg = 'hi'\n</script>\n";
    let bytes: Arc<[u8]> = Arc::from(src.as_bytes().to_vec().into_boxed_slice());
    let parsed = parse_file_if_supported(NavLang::Vue, bytes).map(Arc::new);
    assert!(
        parsed.is_some(),
        "tree-sitter-vue parses SFCs even though queries are empty"
    );
    app.engine.state.preview_content = Some(
        PreviewContent {
            path: "App.vue".to_string(),
            body: PreviewBody::Text(TextPreview {
                lines: src.lines().map(|s| s.to_string()).collect(),
                highlighted: None,
                parsed,
            }),
        }
        .into(),
    );
    // Cursor anywhere in `<script>` — the raw_text region.
    let mut sel = PreviewSelection::new((4, 6));
    sel.active = (4, 6);
    sel.dragging = false;
    app.preview_selection = Some(sel);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    // The LSP-only path registers a pending jump rather than
    // attempting tree-sitter resolution.
    let pending = app
        .engine
        .state
        .nav_pending_lsp_jump
        .as_ref()
        .expect("Vue should route to LSP-only pending-jump path");
    assert_eq!(pending.lang, NavLang::Vue);
    assert!(
        pending.cache_key.starts_with("App.vue:4:6"),
        "cache key encodes (path, line, col): got {}",
        pending.cache_key
    );
    let cache_key_first = pending.cache_key.clone();

    // Pre-seed the refine cache with what an LSP response would have
    // written. Next click at the same (line, col) must take the cache
    // fast-path: no new pending jump.
    app.engine.state.nav_pending_lsp_jump = None;
    app.engine.state.nav_refine_cache.insert(
        (NavLang::Vue, cache_key_first.clone()),
        reef_core::nav::LspLocation {
            path: std::path::PathBuf::from("App.vue"),
            line: 4,
            character: 6,
            character_end: 6,
        },
    );
    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    assert!(
        app.engine.state.nav_pending_lsp_jump.is_none(),
        "cache hit shouldn't issue a fresh LSP request"
    );
    assert!(
        app.engine.state.preview_highlight.is_some(),
        "cache hit should jump to the cached location"
    );
}

/// Crashed/Off supervisor must clear a waiting Vue pending jump,
/// otherwise the next click would stall waiting on a response that
/// can't arrive. Drives the production handler
/// (`handle_lsp_state_change`, the same method `apply_worker_result`
/// calls on a `LspStateChange`) directly.
#[test]
fn lsp_state_crashed_drops_pending_jump() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);

    app.engine.state.nav_pending_lsp_jump = Some(NavPendingJump {
        lang: NavLang::Vue,
        cache_key: "App.vue:1:1".to_string(),
        origin: file_preview_snapshot("App.vue", 1, 1),
        generation: 42,
    });

    // A Crashed state for the SAME language clears the pending jump.
    app.handle_lsp_state_change(NavLang::Vue, LspBadge::Crashed);
    assert!(
        app.engine.state.nav_pending_lsp_jump.is_none(),
        "Crashed supervisor must drop the waiting pending jump"
    );
    assert_eq!(
        app.engine.state.lsp_states.get(&NavLang::Vue),
        Some(&LspBadge::Crashed)
    );
}

/// A state change for a DIFFERENT language must NOT clear an unrelated
/// pending jump.
#[test]
fn lsp_state_change_for_other_lang_keeps_pending() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);

    app.engine.state.nav_pending_lsp_jump = Some(NavPendingJump {
        lang: NavLang::Vue,
        cache_key: "App.vue:1:1".to_string(),
        origin: file_preview_snapshot("App.vue", 1, 1),
        generation: 42,
    });

    app.handle_lsp_state_change(NavLang::Rust, LspBadge::Crashed);
    assert!(
        app.engine.state.nav_pending_lsp_jump.is_some(),
        "an unrelated language's crash must not drop a Vue pending jump"
    );
}

#[test]
fn lsp_state_map_updates_when_supervisor_changes() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // The state map is empty by default — missing keys are interpreted
    // as `LspBadge::Off` by `nav_lsp_badge_text`.
    assert!(!app.engine.state.lsp_states.contains_key(&NavLang::Rust));

    // Simulating what the worker would do on a state change.
    app.engine
        .state
        .lsp_states
        .insert(NavLang::Rust, LspBadge::Booting);
    assert_eq!(
        app.engine.state.lsp_states.get(&NavLang::Rust),
        Some(&LspBadge::Booting)
    );

    app.engine
        .state
        .lsp_states
        .insert(NavLang::Rust, LspBadge::Ready);
    assert_eq!(
        app.engine.state.lsp_states.get(&NavLang::Rust),
        Some(&LspBadge::Ready)
    );
}
