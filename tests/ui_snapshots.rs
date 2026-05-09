//! Full-terminal snapshot tests via `ratatui::TestBackend`.
//!
//! Strategy: drop into a controlled tempdir with a real git repo, redirect
//! `$HOME` to the same tempdir so `App::new()`'s prefs read starts from a
//! blank slate (otherwise the developer's saved tree-mode / diff-layout
//! bleeds into the snapshot), then render and assert against a committed
//! `.snap` file.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui_image::picker::Picker;
use reef::app::App;
use reef::ui;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{
    CwdGuard, HomeGuard, commit_file, force_en_lang, tempdir_repo, write_file, write_striped_png,
};

static CWD_LOCK: Mutex<()> = Mutex::new(());

// `HomeGuard` and `CwdGuard` — redirect $HOME / cwd for the snapshot — live in `test-support`.
// The local `CWD_LOCK` doubles as the HOME_LOCK here because every test in
// this file swaps both in lockstep and nothing else touches HOME concurrently.

fn buffer_to_text(buf: &Buffer) -> String {
    let w = buf.area().width as usize;
    let h = buf.area().height as usize;
    let mut lines = Vec::with_capacity(h);
    for y in 0..h {
        let mut row = String::with_capacity(w);
        for x in 0..w {
            let cell = buf.cell((x as u16, y as u16)).unwrap();
            row.push_str(cell.symbol());
        }
        // trim trailing padding so snapshots stay tidy
        lines.push(row.trim_end().to_string());
    }
    lines.join("\n")
}

fn render_app(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    buffer_to_text(terminal.backend().buffer())
}

fn wait_for_git_status(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.git_status_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for background git status");
}

fn wait_for_file_tree(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for background file tree");
}

/// Apply the shared TMPDIR masks plus any per-test `extra` filters, then
/// run `body` under the resulting insta settings. `extra` is a slice of
/// `(regex, replacement)` pairs — pass `&[]` when no extras are needed.
fn with_filters<F: FnOnce()>(extra: &[(&str, &str)], body: F) {
    let mut settings = insta::Settings::clone_current();
    // TempDir names are `.tmpXXXXXX` on most platforms.
    settings.add_filter(r"\.tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    settings.add_filter(r"tmp[A-Za-z0-9]{6,}", "[TMPDIR]");
    for (pat, replacement) in extra {
        settings.add_filter(pat, *replacement);
    }
    settings.bind(body);
}

#[test]
fn snapshot_empty_repo() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    // HOME must point outside the workdir — prefs creates `.config/reef`
    // and the file tree now shows dotfiles.
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    wait_for_file_tree(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || insta::assert_snapshot!("empty_repo", output));
}

#[test]
fn snapshot_with_staged_and_unstaged() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n"); // unstaged modification
    write_file(&raw, "new.txt", "new\n"); // untracked
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    // Switch to Git tab to show staged/unstaged sections
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("with_staged_and_unstaged", output)
    });
}

#[test]
fn pull_button_stays_visible_when_dirty_or_synced() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);

    app.git_status.ahead_behind = Some((0, 0));
    let synced_output = render_app(&mut app, 80, 20);
    assert!(
        synced_output
            .lines()
            .any(|line| line.contains("Commit") && line.contains("↓ Pull")),
        "pull button should share the commit action row even before a fetch discovers remote commits"
    );

    app.git_status.ahead_behind = Some((0, 1));
    let dirty_behind_output = render_app(&mut app, 80, 20);
    assert!(
        dirty_behind_output
            .lines()
            .any(|line| line.contains("Commit") && line.contains("↓ Pull")),
        "pull button should not disappear from the commit action row just because the worktree is dirty"
    );

    app.git_status.ahead_behind = Some((1, 0));
    let ahead_output = render_app(&mut app, 80, 20);
    assert!(
        ahead_output
            .lines()
            .any(|line| line.contains("Commit") && line.contains("↑ Push")),
        "push button should share the commit action row when local commits are ahead"
    );

    app.git_status.ahead_behind = None;
    let no_upstream_output = render_app(&mut app, 120, 20);
    assert!(
        no_upstream_output
            .lines()
            .any(|line| line.contains("Commit") && line.contains("↑ Publish Branch")),
        "publish button should share the commit action row when the branch has no upstream"
    );
}

#[test]
fn commit_message_uses_full_sidebar_width() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    app.git_status.commit_message = "abcdefghijklmnop".to_string();
    app.git_status.commit_cursor = app.git_status.commit_message.len();

    let output = render_app(&mut app, 80, 20);
    assert!(
        output.contains("abcdefghijklmnop"),
        "commit message should use the full sidebar width, not the narrower file-row path budget"
    );
}

#[test]
fn commit_message_view_follows_cursor_into_truncated_tail() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    app.git_status.commit_editing = true;
    app.git_status.commit_message = "abcdefghijklmnopqrstuvwxyz0123456789".to_string();
    app.git_status.commit_cursor = app.git_status.commit_message.len();

    let output = render_app(&mut app, 80, 20);
    assert!(
        output.contains("…") && output.contains("0123456789"),
        "moving the cursor to the end should reveal text after the truncation ellipsis"
    );
}

#[test]
fn branch_selector_renders_as_dropdown() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    let head = raw.head().unwrap().peel_to_commit().unwrap();
    raw.branch("feature", &head, false).unwrap();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);

    let closed = render_app(&mut app, 80, 20);
    assert!(closed.contains("Branch: master ▾"));
    assert!(!closed.lines().any(|line| line.contains("› feature")));

    app.git_status.branch_dropdown_open = true;
    let open = render_app(&mut app, 80, 20);
    assert!(open.contains("Branch: master ▴"));
    assert!(
        open.lines()
            .any(|line| line.contains("› ") && line.contains("feature"))
    );
}

#[test]
fn snapshot_place_mode_renders_banner_and_border() {
    // Lock in the VSCode-style drag-and-drop picker visuals: double-line
    // accent border + top banner + dimmed file rows vs. accent folder rows.
    // Regressions here would indicate the place-mode render path drifted
    // — the shape of the banner and the root drop zone are both part of
    // the feature's UX contract.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    // A small tree with a folder and a file so the snapshot exercises
    // both row styles.
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    // Force a tree rebuild so `src/` and `README.md` show up before the
    // snapshot draw (the async load fires from `App::new` via tick).
    app.refresh_file_tree();
    // Drain worker results until the tree is populated.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Source file used for the banner — has to exist on disk because
    // place mode itself is entered via `enter_place_mode` which accepts
    // any `Vec<PathBuf>`, but users would only ever land here with real
    // paths thanks to `parse_dropped_paths`.
    let source = tmp.path().join("README.md");
    app.enter_place_mode(vec![source]);

    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || insta::assert_snapshot!("place_mode", output));
}

#[test]
fn snapshot_tree_edit_row_new_file() {
    // Locks in the VSCode-style inline editor visuals: the toolbar row,
    // the editable row injected after the anchor folder, the cursor
    // block on the first empty cell, and the placeholder text. If this
    // regresses, the user's typing would either render to the wrong
    // row or the cursor would go invisible.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Kick off a NewFile edit anchored on the first top-level folder so
    // the editable row renders indented one deeper than the folder.
    let parent_dir = tmp.path().join("src");
    app.begin_tree_edit(
        reef::tree_edit::TreeEditMode::NewFile,
        parent_dir,
        None,
        Some(0),
    );

    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("tree_edit_row_new_file", output)
    });
}

#[test]
fn snapshot_tree_context_menu() {
    // Locks in the right-click context-menu popup visuals. If the menu
    // regresses the user would lose access to Rename / Delete / Reveal
    // on a pure mouse workflow.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "README.md", "# hi\n", "init");
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    write_file(&raw, "src/main.rs", "fn main() {}\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Open menu anchored at column 4, row 5 — arbitrary coords that
    // leave room for the popup to render inline without getting
    // clamped by the right edge. target_entry_idx=0 hits the first
    // file-tree row so the full ALL_FOR_ENTRY menu is offered.
    app.open_tree_context_menu(Some(0), (4, 5));

    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || insta::assert_snapshot!("tree_context_menu", output));
}

/// Wait for the graph worker to populate `rows` and then for the initial
/// commit-detail / range-detail load to finish. Polls `tick` with a 2s
/// deadline — matches the `wait_for_git_status` pattern above.
fn wait_for_graph_ready(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        let graph_ready = !app.graph_load.loading && !app.git_graph.rows.is_empty();
        let detail_ready = !app.commit_detail_load.loading;
        if graph_ready && detail_ready {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for graph / commit detail to load");
}

#[test]
fn snapshot_graph_range_mode() {
    // Lock in the Graph tab's range-mode visuals: three contiguous commits
    // selected via `extend_graph_selection`, right panel showing the
    // "Range · N commits" header, the commit list, and the union of files
    // changed. Regressions here would indicate either the range payload
    // plumbing (app.rs → tasks.rs → git/mod.rs) or the commit_detail_panel
    // range-mode render path drifted. 80×24 is tall enough to show the
    // full header + all commits + the 3-file list without truncation.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "a.txt", "v1\n", "first");
    commit_file(&raw, "b.txt", "v1\n", "second");
    commit_file(&raw, "c.txt", "v1\n", "third");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(reef::app::Tab::Graph);
    wait_for_graph_ready(&mut app);
    // Extend by 2 → range of 3 (newest + 2 older). `rows` is newest-first,
    // so `selected_idx=0` starts on "third"; extending downward (+delta)
    // grows the range toward older commits.
    app.extend_graph_selection(2);
    // Wait for the range-detail worker (files union) to land.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        let has_files = app
            .commit_detail
            .range_detail
            .as_ref()
            .map(|r| !r.files.is_empty())
            .unwrap_or(false);
        if has_files && !app.commit_detail_load.loading {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let output = render_app(&mut app, 80, 24);
    // Short OIDs (7-hex prefix) are fresh per run; mask them so the
    // snapshot captures layout/text, not the non-deterministic SHAs.
    with_filters(&[(r"\b[0-9a-f]{7}\b", "[OID]")], || {
        insta::assert_snapshot!("graph_range_mode", output)
    });
}

/// Hosts picker snapshot — empty state (no `~/.ssh/config` parseable, no
/// recent connections). Verifies the overlay renders an input row, a
/// separator, the "no matches" placeholder, and the footer help line.
#[test]
fn snapshot_hosts_picker_empty() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    wait_for_file_tree(&mut app);
    app.hosts_picker.open(vec![], vec![]);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("hosts_picker_empty", output)
    });
}

/// Hosts picker snapshot — populated list (three hosts from `~/.ssh/config`
/// + one recent connection).
///
/// Locks in the recent→config row ordering, selection highlighting,
/// and the two-column alias/hostname layout.
#[test]
fn snapshot_hosts_picker_populated() {
    use reef::hosts::HostEntry;
    use reef::hosts_picker::SshTarget;

    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    let hosts = vec![
        HostEntry {
            alias: "hongxuan".into(),
            hostname: Some("47.101.167.85".into()),
            user: Some("root".into()),
        },
        HostEntry {
            alias: "dev-box".into(),
            hostname: Some("dev.internal".into()),
            user: Some("pan".into()),
        },
        HostEntry {
            alias: "ci".into(),
            hostname: None,
            user: None,
        },
    ];
    let recent = vec![SshTarget {
        host: "root@47.101.167.85".into(),
        path: "/srv/app".into(),
    }];
    wait_for_file_tree(&mut app);
    app.hosts_picker.open(hosts, recent);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("hosts_picker_populated", output)
    });
}

/// Hosts picker snapshot — path-input mode. After pressing Ctrl+P (or
/// Enter on a selected row) the overlay title flips to `Connect to ·
/// [user@]host[:path]` and the prompt glyph changes `›` → `➜`.
#[test]
fn snapshot_hosts_picker_path_mode() {
    use reef::hosts::HostEntry;

    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.hosts_picker.open(
        vec![HostEntry {
            alias: "hongxuan".into(),
            hostname: Some("47.101.167.85".into()),
            user: Some("root".into()),
        }],
        vec![],
    );
    wait_for_file_tree(&mut app);
    app.hosts_picker.enter_path_mode();
    app.hosts_picker.path_buffer = "root@47.101.167.85:/tmp/work".into();
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("hosts_picker_path_mode", output)
    });
}

#[test]
fn snapshot_with_staged_and_unstaged_light_theme() {
    // Locks in the light-theme wiring: a dark-vs-light snapshot diff must exist
    // somewhere, otherwise the Theme plumbing could silently regress to dark.
    // Text content should match the dark snapshot; only style bytes differ, but
    // TestBackend's `Cell::symbol()` drops styles, so the plain-text dump here
    // intentionally asserts "same content, same layout" — not color fidelity.
    // Color fidelity is verified by the unit tests in `src/ui/theme.rs`.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "tracked.txt", "v1\n", "init");
    write_file(&raw, "tracked.txt", "v2\n");
    write_file(&raw, "new.txt", "new\n");
    let _h = HomeGuard::enter(tmp.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::light(), None);
    app.set_active_tab(reef::app::Tab::Git);
    app.refresh_status();
    wait_for_git_status(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("with_staged_and_unstaged_light", output)
    });
}

/// Wait until the preview worker delivers a result (either text, image,
/// or binary). Used by image-preview snapshot tests to avoid racing
/// against the background file worker.
fn wait_for_preview(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        app.tick();
        if !app.preview_load.loading && app.preview_content.is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for preview worker");
}

#[test]
fn snapshot_image_preview_halfblocks() {
    // Locks in the image-preview layout: header + separator + metadata
    // line ("4×4 · PNG · N B") + blank row + halfblocks body. The
    // Halfblocks protocol is the only ratatui-image backend that writes
    // into the ratatui Buffer (the others emit escape sequences straight
    // to stdout, invisible to TestBackend), so we force it via
    // `Picker::halfblocks()` to keep the snapshot deterministic.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    // Striped red/green PNG — halfblocks' `pick_side` collapses cells
    // whose upper and lower pixel halves match to a literal space, so
    // uniform solid-colour PNGs produce no visible text in the snapshot
    // dump even though their colors are set on the cells. Striped rows
    // guarantee top≠bottom at each cell so the `▀` glyph renders and
    // the snapshot actually asserts something.
    write_striped_png(tmp.path(), "striped.png", 40, 40, [255, 0, 0], [0, 255, 0]);
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), Some(Picker::halfblocks()));
    // Populate the tree, then drive selection to the PNG so the worker
    // queues a preview load. `navigate_files` also fires the preview
    // request internally.
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Seed `selected_file` on whichever row is "striped.png".
    let idx = app
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "striped.png")
        .expect("striped.png in tree");
    app.file_tree.selected = idx;
    app.load_preview();
    wait_for_preview(&mut app);

    // Three-phase sequence for the async ThreadProtocol pipeline:
    // 1. After `wait_for_preview`, the decode is done but
    //    `preview_image_protocol` is `Some(ThreadProtocol { inner: None })`
    //    — the build thread hasn't populated the inner protocol yet.
    //    Spin until it does.
    // 2. First render call dispatches a ResizeRequest to the resize
    //    worker (inner → None again while the resize runs).
    // 3. Spin until the resize response lands and inner is Some again,
    //    now carrying encoded halfblocks data.
    // 4. Second render writes the pixels into the buffer.
    let wait_for_inner_some = |app: &mut App| {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            app.tick();
            if app
                .preview_image_protocol
                .as_ref()
                .and_then(|p| p.protocol_type())
                .is_some()
            {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for image protocol inner");
    };
    wait_for_inner_some(&mut app);
    let _warmup = render_app(&mut app, 80, 20);
    wait_for_inner_some(&mut app);
    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || {
        insta::assert_snapshot!("image_preview_halfblocks", output)
    });
}

#[test]
fn snapshot_binary_info_pdf() {
    // Non-image binary card: header + separator + "application/pdf · N B"
    // + centred "binary file" line. Regression guard for the friendly
    // metadata branch — if it breaks, users selecting a PDF would see
    // "(binary file)" again with no size/MIME context.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    std::fs::write(tmp.path().join("doc.pdf"), b"%PDF-1.4\n%...\n").unwrap();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.tick();
        if !app.file_tree_load.loading && !app.file_tree.entries.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let idx = app
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "doc.pdf")
        .expect("doc.pdf in tree");
    app.file_tree.selected = idx;
    app.load_preview();
    wait_for_preview(&mut app);

    let output = render_app(&mut app, 80, 20);
    with_filters(&[], || insta::assert_snapshot!("binary_info_pdf", output));
}

#[test]
fn snapshot_search_tab_replace_open_with_excluded_row() {
    // Lock in the VSCode-style Find & Replace layout in Tab::Search:
    // chevron toggle in the find input row, second `↪ ` prompt for the
    // replace text, per-result `[✓] / [ ] ` checkbox column, and the
    // footer `· N to replace · [Apply]` summary. Regressing any of
    // these visuals would break the muscle-memory the user built up
    // around VSCode's panel.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, raw) = tempdir_repo();
    commit_file(&raw, "alpha.txt", "needle in alpha\nuntouched\n", "init");
    write_file(&raw, "beta.txt", "another needle here\n");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    wait_for_file_tree(&mut app);
    app.set_active_tab(reef::app::Tab::Search);
    app.global_search.query = "needle".to_string();
    app.global_search.cursor = "needle".len();
    app.global_search.replace_text = "thread".to_string();
    app.global_search.replace_cursor = "thread".len();
    app.global_search.replace_open = true;
    app.global_search.focus = reef::global_search::SearchPanelFocus::List;
    // Seed two synthetic hits — going through the actual ripgrep
    // worker would couple the snapshot to walker timing on CI.
    app.global_search.results = vec![
        reef::global_search::MatchHit {
            path: std::path::PathBuf::from("alpha.txt"),
            display: "alpha.txt".to_string(),
            line: 0,
            line_text: "needle in alpha".to_string(),
            byte_range: 0..6,
        },
        reef::global_search::MatchHit {
            path: std::path::PathBuf::from("beta.txt"),
            display: "beta.txt".to_string(),
            line: 0,
            line_text: "another needle here".to_string(),
            byte_range: 8..14,
        },
    ];
    // Exclude the second row — strikethrough + dim styling should show.
    app.global_search.toggle_match_excluded(1);

    let output = render_app(&mut app, 100, 16);
    with_filters(&[], || {
        insta::assert_snapshot!("search_tab_replace_open_with_excluded_row", output)
    });
}

#[test]
fn snapshot_settings_page_default() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    wait_for_file_tree(&mut app);
    app.open_settings();
    let output = render_app(&mut app, 80, 24);
    with_filters(&[], || {
        insta::assert_snapshot!("settings_page_default", output)
    });
}

#[test]
fn snapshot_settings_page_editing_editor_command() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    wait_for_file_tree(&mut app);
    app.open_settings();
    let editor_idx = reef::settings::SettingItem::ALL
        .iter()
        .position(|i| matches!(i, reef::settings::SettingItem::EditorCommand))
        .unwrap();
    app.settings.select(editor_idx);
    reef::settings::begin_edit_editor_command(&mut app.settings);
    if let Some(edit) = app.settings.editor_edit.as_mut() {
        edit.buffer = "nvim --clean".to_string();
        edit.cursor = edit.buffer.len();
    }
    let output = render_app(&mut app, 80, 24);
    with_filters(&[], || {
        insta::assert_snapshot!("settings_page_editing_editor_command", output)
    });
}
