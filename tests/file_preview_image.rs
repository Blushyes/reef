//! End-to-end test for the image-preview pipeline: write a PNG to a temp
//! repo, let the file-worker decode it, and assert that the App ended up
//! holding an `Image` body plus a ratatui-image StatefulProtocol ready
//! for render.
//!
//! Unlike `ui_snapshots.rs` (which asserts the rendered *buffer*), this
//! file asserts the *state machine* — the pieces the render panel
//! consumes — so a regression that broke decoding or protocol wiring
//! would fail here even if the UI layer happened to paper over it.

use ratatui_image::picker::Picker;
use reef::app::App;
use reef::file_tree::PreviewBody;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{HomeGuard, tempdir_repo, write_striped_png};

// HOME/CWD mutations are process-global and this file lives in its own
// test binary, but still guard it — the lock cost is trivial and it
// keeps the pattern consistent with `ui_snapshots.rs` in case other
// integration tests get added to this binary later.
static LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

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
fn selecting_png_decodes_and_builds_protocol() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    write_striped_png(tmp.path(), "img.png", 16, 16, [255, 0, 0], [0, 0, 255]);

    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), Some(Picker::halfblocks()));
    app.refresh_file_tree();

    // Drain worker results until the file tree is populated.
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
        .position(|e| e.name == "img.png")
        .expect("img.png in tree");
    app.file_tree.selected = idx;
    app.load_preview();
    wait_for_preview(&mut app);

    let preview = app.preview_content.as_ref().expect("preview present");
    match &preview.body {
        PreviewBody::Image(img) => {
            assert_eq!(img.width_px, 16);
            assert_eq!(img.height_px, 16);
            assert_eq!(img.format, image::ImageFormat::Png);
        }
        other => panic!("expected Image body, got {other:?}"),
    }
    // apply_worker_result must have built the protocol on the main
    // thread using our halfblocks Picker.
    assert!(
        app.preview_image_protocol.is_some(),
        "expected StatefulProtocol wired up when image body + picker both present"
    );
    // The decoded `DynamicImage` was moved into the protocol, so the
    // ImagePreview we're holding shouldn't still be carrying a copy
    // (that would double the memory footprint per selection).
    if let PreviewBody::Image(img) = &app.preview_content.as_ref().unwrap().body {
        assert!(
            img.image.is_none(),
            "pixels must be moved into the protocol, not duplicated on PreviewContent"
        );
    }
}

#[test]
fn re_selecting_same_image_reuses_protocol() {
    // Two consecutive load_preview calls on the same file must not
    // rebuild the StatefulProtocol — that would trigger a re-encode
    // and visibly flicker the preview when fs-watcher or the user
    // re-enters the selection. `preview_image_protocol_builds` is a
    // monotonic counter; it's the observable signal for "we built a
    // fresh protocol." Address-based identity doesn't work because the
    // `Option<StatefulProtocol>` slot lives at a fixed offset inside
    // `App`, so an in-place replace looks identical to no change.
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    write_striped_png(tmp.path(), "img.png", 16, 16, [255, 0, 0], [0, 0, 255]);
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), Some(Picker::halfblocks()));
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
        .position(|e| e.name == "img.png")
        .expect("img.png in tree");
    app.file_tree.selected = idx;
    app.load_preview();
    wait_for_preview(&mut app);
    assert!(app.preview_image_protocol.is_some());
    assert_eq!(
        app.preview_image_protocol_builds, 1,
        "first load should build exactly one protocol"
    );

    // Re-load without changing the file. The worker re-decodes (we
    // don't cache at the worker layer in v1) but the main-thread
    // merge should detect bytes_on_disk + w + h + format match and
    // keep the existing protocol.
    app.load_preview();
    wait_for_preview(&mut app);
    assert!(app.preview_image_protocol.is_some());
    assert_eq!(
        app.preview_image_protocol_builds, 1,
        "same-file reload should not rebuild the protocol"
    );
}

#[test]
fn selecting_png_without_picker_keeps_body_but_no_protocol() {
    // Terminal with no image support: worker still decodes the pixels
    // (the `Binary(…)` card has less info than an `Image` body, so we
    // prefer the image body) but the protocol stays None, and the
    // render path falls through to the "image preview unavailable"
    // text card. This test is the regression guard for that wiring.
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    write_striped_png(tmp.path(), "img.png", 8, 8, [255, 0, 0], [0, 0, 255]);
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
        .position(|e| e.name == "img.png")
        .expect("img.png in tree");
    app.file_tree.selected = idx;
    app.load_preview();
    wait_for_preview(&mut app);

    match app.preview_content.as_ref().unwrap().body {
        PreviewBody::Image(_) => {}
        ref other => panic!("expected Image body, got {other:?}"),
    }
    assert!(
        app.preview_image_protocol.is_none(),
        "no picker ⇒ no protocol"
    );
}

#[test]
fn pdf_is_classified_as_non_image_binary() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _raw) = tempdir_repo();
    std::fs::write(tmp.path().join("doc.pdf"), b"%PDF-1.4\nbogus\n").unwrap();
    let home = tempfile::TempDir::new().expect("home tempdir");
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), Some(Picker::halfblocks()));
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

    match &app.preview_content.as_ref().unwrap().body {
        PreviewBody::Binary(info) => {
            assert_eq!(info.mime, Some("application/pdf"));
        }
        other => panic!("expected Binary body, got {other:?}"),
    }
    // Non-image body → protocol must have been torn down (we don't
    // want a stale image lingering on the App from a previous preview).
    assert!(
        app.preview_image_protocol.is_none(),
        "non-image body must not carry a StatefulProtocol"
    );
}
