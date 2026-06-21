//! Micro-benchmarks for the diff/preview hot paths the recent refactor
//! touched. Run with `cargo bench --bench diff_perf`. Each bench is sized
//! to be representative of "non-trivial PR diff" — 1k content lines split
//! across 20 hunks of 50 lines each.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use reef::app::HighlightedDiff;
use reef::search::{MatchLoc, SearchState, SearchTarget};
use reef_core::diff::{DiffContent, DiffHunk, DiffLine, LineTag, unified_display_rows};
use std::sync::Arc;

/// Generates a diff with `hunks` hunks of `lines_per_hunk` lines each.
/// Mix of Context (60%), Added (25%), Removed (15%) to approximate a
/// realistic edit pattern.
fn synth_diff(hunks: usize, lines_per_hunk: usize) -> DiffContent {
    let mk_hunks: Vec<DiffHunk> = (0..hunks)
        .map(|h| {
            let lines: Vec<DiffLine> = (0..lines_per_hunk)
                .map(|i| {
                    let tag = match i % 20 {
                        0..=11 => LineTag::Context,
                        12..=16 => LineTag::Added,
                        _ => LineTag::Removed,
                    };
                    let content_str = format!(
                        "    let value_{h}_{i} = compute({}, {});",
                        h * 100 + i,
                        i * 7
                    );
                    DiffLine {
                        tag,
                        content: Arc::from(content_str),
                        old_lineno: Some((h * lines_per_hunk + i) as u32 + 1),
                        new_lineno: Some((h * lines_per_hunk + i) as u32 + 1),
                    }
                })
                .collect();
            DiffHunk {
                header: Arc::from(
                    format!(
                        "@@ -{0},{1} +{0},{1} @@",
                        h * lines_per_hunk + 1,
                        lines_per_hunk
                    )
                    .as_str(),
                ),
                lines,
            }
        })
        .collect();
    DiffContent {
        path: "src/lib.rs".to_string(),
        hunks: mk_hunks,
    }
}

/// Build HighlightedDiff (which builds DiffDisplay both layouts).
fn bench_diff_display_build(c: &mut Criterion) {
    let diff = synth_diff(20, 50); // 1000 lines / 20 hunks
    c.bench_function("DiffDisplay::build/1k_lines_20_hunks", |b| {
        b.iter(|| {
            let d = HighlightedDiff::new(black_box(diff.clone()), None);
            black_box(d);
        });
    });
}

/// `reef_core::diff::unified_display_rows` flattens a DiffContent into
/// `Vec<Arc<str>>` (post-Arc<str> migration — used to be `Vec<String>`)
/// for the match-finding stage. Called per keystroke in find widget.
fn bench_unified_display_rows(c: &mut Criterion) {
    let diff = synth_diff(20, 50);
    c.bench_function("unified_display_rows/1k_lines", |b| {
        b.iter(|| {
            let rows = unified_display_rows(black_box(&diff));
            black_box(rows);
        });
    });
}

/// `SearchState::ranges_on_row` lookup on a 1000-row search result.
/// Simulates per-row overlay lookup during render of a long diff.
fn bench_ranges_on_row(c: &mut Criterion) {
    let mut state = SearchState {
        target: Some(SearchTarget::Diff),
        ..SearchState::default()
    };
    // 1000 matches scattered across 500 rows (avg 2 matches/row).
    let matches: Vec<MatchLoc> = (0..1000)
        .map(|i| MatchLoc {
            row: i / 2,
            byte_range: (i % 50)..(i % 50 + 3),
        })
        .collect();
    state.set_matches(matches);

    c.bench_function("ranges_on_row/lookup_50_rows_of_1k_matches", |b| {
        b.iter(|| {
            for row in 0..50 {
                let (ranges, _cur) = state.ranges_on_row(SearchTarget::Diff, black_box(row));
                black_box(ranges);
            }
        });
    });
}

/// Simulate one find-widget keystroke against a Unified diff: flatten
/// the diff via `unified_display_rows` + run the matcher. Pre-fix the
/// `to_string()` storm dominated; post-fix the Arc-clone path is the
/// hot work.
fn bench_find_widget_keystroke(c: &mut Criterion) {
    use reef_core::search::find_literal_all;
    let diff = synth_diff(20, 50);
    c.bench_function("find_keystroke/unified_1k_lines_smallcase", |b| {
        b.iter(|| {
            let rows = unified_display_rows(black_box(&diff));
            let mut total = 0usize;
            for r in &rows {
                // `&Arc<str>` derefs to `&str`.
                total += find_literal_all(r, black_box("compute"), true).len();
            }
            black_box(total);
        });
    });
}

/// `highlight_diff` on a brand-new key — pays the full hash, flat build
/// and syntect cost. First-time-loaded diff baseline. Resets the cache
/// upfront so the bench's numbers don't depend on which other benches
/// ran first in this binary.
fn bench_highlight_diff_miss(c: &mut Criterion) {
    let diff = synth_diff(20, 50);
    let mut iter_count = 0u64;
    reef::tasks::_reset_highlight_cache();
    c.bench_function("highlight_diff/miss_1k_lines", |b| {
        b.iter(|| {
            // Mutate path each iteration so the cache misses every time —
            // otherwise we'd be measuring the in-flight wait or the cache
            // hit, not actual syntect cost.
            iter_count += 1;
            let path = format!("src/bench_{}.rs", iter_count);
            let r = reef::tasks::highlight_diff(black_box(&path), black_box(&diff), true);
            black_box(r);
        });
    });
}

/// `highlight_diff` on a warm cache key — should short-circuit on the
/// LRU lookup and return an `Arc<DiffHighlighted>` clone (refcount bump).
/// Resets first then warms exactly one key, so the measured short-circuit
/// is reproducible regardless of prior bench state.
fn bench_highlight_diff_hit(c: &mut Criterion) {
    let diff = synth_diff(20, 50);
    reef::tasks::_reset_highlight_cache();
    // Warm the cache once outside the timing loop.
    let _ = reef::tasks::highlight_diff("src/cached.rs", &diff, true);
    c.bench_function("highlight_diff/cache_hit_1k_lines", |b| {
        b.iter(|| {
            let r = reef::tasks::highlight_diff(
                black_box("src/cached.rs"),
                black_box(&diff),
                black_box(true),
            );
            black_box(r);
        });
    });
}

criterion_group!(
    benches,
    bench_diff_display_build,
    bench_unified_display_rows,
    bench_ranges_on_row,
    bench_find_widget_keystroke,
    bench_highlight_diff_miss,
    bench_highlight_diff_hit,
);
criterion_main!(benches);
