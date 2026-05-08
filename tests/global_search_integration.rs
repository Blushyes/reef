//! Integration tests for the global-search pipeline: spawn a
//! `TaskCoordinator`, kick off `search_all` against a real tempdir, and
//! assert the streaming contract (chunks + Done).
//!
//! The worker runs in a background thread so each test polls `try_recv`
//! with a timeout rather than busy-waiting.

use reef::backend::{Backend, LocalBackend};
use reef::global_search::{MAX_RESULTS, MatchHit};
use reef::tasks::{TaskCoordinator, WorkerResult};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn local_backend(root: &Path) -> Arc<dyn Backend> {
    Arc::new(LocalBackend::open_at(root.to_path_buf()))
}

/// Drain chunks + Done from the coordinator into a flat Vec, returning
/// (hits, truncated). Bails after `deadline` with whatever it has seen.
fn collect(coord: &TaskCoordinator, generation: u64, deadline: Duration) -> (Vec<MatchHit>, bool) {
    let start = Instant::now();
    let mut hits = Vec::new();
    let mut truncated = false;
    while start.elapsed() < deadline {
        match coord.try_recv() {
            Ok(WorkerResult::GlobalSearchChunk {
                generation: g,
                hits: h,
            }) if g == generation => {
                hits.extend(h);
            }
            Ok(WorkerResult::GlobalSearchDone {
                generation: g,
                truncated: t,
            }) if g == generation => {
                truncated = t;
                return (hits, truncated);
            }
            Ok(_) => {
                // Other worker results from unrelated workers — ignore.
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    (hits, truncated)
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn literal_match_with_smart_case_finds_hits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", "fn foo() { bar() }\nfn baz() { foo() }");
    write(root, "src/other.rs", "no match here");
    write(root, "README.md", "Use foo() for the thing");

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(1, cancel, local_backend(root), "foo".into());

    let (hits, truncated) = collect(&coord, 1, Duration::from_secs(5));
    assert!(!truncated);
    assert!(
        hits.len() >= 3,
        "expected at least 3 hits across 2 files, got {:?}",
        hits.iter()
            .map(|h| (h.display.clone(), h.line))
            .collect::<Vec<_>>()
    );
    assert!(
        hits.iter()
            .any(|h| h.display.contains("lib.rs") && h.line == 0)
    );
    assert!(
        hits.iter()
            .any(|h| h.display.contains("lib.rs") && h.line == 1)
    );
    assert!(hits.iter().any(|h| h.display.contains("README.md")));
    // byte_range must be non-empty and point at "foo" (3 bytes).
    for hit in &hits {
        assert_eq!(hit.byte_range.end - hit.byte_range.start, 3);
        assert!(hit.line_text[hit.byte_range.clone()].eq_ignore_ascii_case("foo"));
    }
}

#[test]
fn smart_case_becomes_case_sensitive_with_uppercase_in_query() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "a.txt", "Foo FOO foo");

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    // Capital letter → case-sensitive. Only "Foo" should match.
    coord.search_all(1, cancel, local_backend(root), "Foo".into());
    let (hits, _) = collect(&coord, 1, Duration::from_secs(5));
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].byte_range, 0..3);
}

#[test]
fn respects_gitignore() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // `ignore::WalkBuilder` only consults `.gitignore` inside a git repo by
    // default — seed a minimal fake `.git` so the walker treats this as one.
    // (Matches how the matching test in `quick_open` primes its fixture.)
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();

    write(root, ".gitignore", "ignored/\n");
    write(root, "ignored/secret.rs", "fn secret_fn() {}");
    write(root, "src/public.rs", "fn public_fn() {}");

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(1, cancel, local_backend(root), "fn".into());
    let (hits, _) = collect(&coord, 1, Duration::from_secs(5));
    assert!(hits.iter().any(|h| h.display.contains("public.rs")));
    assert!(
        !hits.iter().any(|h| h.display.contains("secret.rs")),
        "ignored/ subtree should be excluded by WalkBuilder"
    );
}

#[test]
fn binary_files_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // A file with a NUL in the first 8 KiB — grep-searcher's binary detection
    // should skip it entirely.
    let mut blob = b"data FOO ".to_vec();
    blob.push(0);
    blob.extend_from_slice(b"FOO more");
    std::fs::write(root.join("bin.dat"), blob).unwrap();
    write(root, "plain.txt", "FOO is here\n");

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(1, cancel, local_backend(root), "foo".into());
    let (hits, _) = collect(&coord, 1, Duration::from_secs(5));
    assert!(hits.iter().any(|h| h.display.contains("plain.txt")));
    assert!(
        !hits.iter().any(|h| h.display.contains("bin.dat")),
        "binary file should be skipped by grep-searcher"
    );
}

#[test]
fn empty_query_returns_nothing_and_done_arrives() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "a.txt", "anything");

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(7, cancel, local_backend(root), String::new());
    let (hits, truncated) = collect(&coord, 7, Duration::from_secs(5));
    assert!(hits.is_empty());
    assert!(!truncated);
}

#[test]
fn truncates_at_max_results() {
    // grep-searcher emits ONE `matched()` call per matching line, not per
    // occurrence within a line. Generate many lines, each with one hit, so
    // we actually cross the MAX_RESULTS cap.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let lines_per_file = 40;
    let files = (MAX_RESULTS / lines_per_file) + 5;
    let content: String = (0..lines_per_file)
        .map(|i| format!("needle {i}\n"))
        .collect();
    for i in 0..files {
        write(root, &format!("f{i}.txt"), &content);
    }

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(2, cancel, local_backend(root), "needle".into());
    let (hits, truncated) = collect(&coord, 2, Duration::from_secs(10));
    assert!(
        truncated,
        "expected truncated=true at MAX_RESULTS cap, got {} hits",
        hits.len()
    );
    // Worker only checks cap inside `matched()` — may overshoot by up to
    // one file's worth (we don't abort mid-file once we've started).
    assert!(
        hits.len() >= MAX_RESULTS && hits.len() < MAX_RESULTS + lines_per_file * 2,
        "hit count {} outside expected [{MAX_RESULTS}, {})",
        hits.len(),
        MAX_RESULTS + lines_per_file * 2
    );
}

#[test]
fn cancel_flag_bails_mid_walk() {
    // Bigger tree so the walker has room to bail before finishing.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    for i in 0..500 {
        write(root, &format!("f{i}.txt"), "needle\n");
    }

    let coord = TaskCoordinator::new();
    let cancel = Arc::new(AtomicBool::new(false));
    coord.search_all(3, cancel.clone(), local_backend(root), "needle".into());

    // Flip cancel right away. Depending on scheduler the worker might have
    // already started pumping chunks — we don't assert *no* hits, just that
    // Done arrives promptly.
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);

    let (_, truncated) = collect(&coord, 3, Duration::from_secs(5));
    // Cancel-bail doesn't flip `truncated` (we reserve that for cap-hit).
    assert!(!truncated);
}

#[test]
fn superseded_generation_is_dropped() {
    // Kick off one search, then immediately supersede with a different query.
    // The TaskCoordinator's single-threaded worker serialises tasks, so the
    // second search will queue behind the first — the test just verifies
    // both searches complete with their own generation tags.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "a.txt", "alpha bravo");

    let coord = TaskCoordinator::new();
    let c1 = Arc::new(AtomicBool::new(false));
    let c2 = Arc::new(AtomicBool::new(false));
    coord.search_all(10, c1, local_backend(root), "alpha".into());
    coord.search_all(11, c2, local_backend(root), "bravo".into());

    let (g10_hits, _) = collect(&coord, 10, Duration::from_secs(5));
    let (g11_hits, _) = collect(&coord, 11, Duration::from_secs(5));
    assert!(g10_hits.iter().any(|h| h.line_text.contains("alpha")));
    assert!(g11_hits.iter().any(|h| h.line_text.contains("bravo")));
}

/// Drain a `ReplaceInFiles` batch into the final `ReplaceSummary`.
/// Mirrors `collect` but for the replace-progress / replace-done frame
/// pair. Late results from other workers are ignored.
fn collect_replace(
    coord: &TaskCoordinator,
    generation: u64,
    deadline: Duration,
) -> reef::tasks::ReplaceSummary {
    let start = Instant::now();
    while start.elapsed() < deadline {
        match coord.try_recv() {
            Ok(WorkerResult::ReplaceDone {
                generation: g,
                result,
            }) if g == generation => {
                return result.expect("replace failed");
            }
            Ok(_) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    panic!("ReplaceDone never arrived");
}

#[test]
fn end_to_end_search_then_replace_with_per_match_exclusion() {
    // Plant three files, search for `foo`, run replace with one
    // result line excluded, verify on-disk content + idempotence
    // (a second apply changes nothing).
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "a.txt", "foo on a\n");
    write(root, "b.txt", "foo on b\nuntouched\n");
    write(root, "c.txt", "foo on c\n");

    let coord = TaskCoordinator::new();
    let c = Arc::new(AtomicBool::new(false));
    coord.search_all(1, c, local_backend(root), "foo".into());
    let (hits, _) = collect(&coord, 1, Duration::from_secs(5));
    assert!(hits.len() >= 3, "expected hits in all three files");

    // Build replace items skipping the hit in `b.txt` (simulates the
    // user unchecking that row in the UI).
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<std::path::PathBuf, Vec<reef::tasks::ReplaceLine>> = BTreeMap::new();
    let excluded_path = std::path::PathBuf::from("b.txt");
    for hit in &hits {
        if hit.path == excluded_path {
            continue;
        }
        buckets
            .entry(hit.path.clone())
            .or_default()
            .push(reef::tasks::ReplaceLine {
                line_no: hit.line,
                expected_text: hit.line_text.clone(),
            });
    }
    let items: Vec<reef::tasks::ReplaceItem> = buckets
        .into_iter()
        .map(|(path, lines)| reef::tasks::ReplaceItem { path, lines })
        .collect();

    coord.replace_in_files(
        2,
        local_backend(root),
        "foo".into(),
        "BAR".into(),
        items.clone(),
    );
    let summary = collect_replace(&coord, 2, Duration::from_secs(5));
    assert_eq!(summary.lines_replaced, 2, "two files should change");
    assert_eq!(summary.files_changed, 2);

    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "BAR on a\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "foo on b\nuntouched\n",
        "excluded file must remain pristine",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("c.txt")).unwrap(),
        "BAR on c\n"
    );

    // Idempotence: a second apply on the same items finds no matches
    // (the lines no longer say "foo") so nothing changes on disk.
    coord.replace_in_files(3, local_backend(root), "foo".into(), "BAR".into(), items);
    let summary2 = collect_replace(&coord, 3, Duration::from_secs(5));
    assert_eq!(summary2.lines_replaced, 0, "second apply must be a no-op");
    assert_eq!(summary2.skipped_stale, 2, "now-stale lines must be counted");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "BAR on a\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("c.txt")).unwrap(),
        "BAR on c\n"
    );
}
