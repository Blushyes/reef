use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use reef_git::git::CommitInfo;
use reef_git::graph::build_graph;

/// Deterministically build a commit sequence of `n` commits with a light
/// fork/merge pattern. Every 5th commit has two parents to exercise merge
/// lanes; the rest are linear.
fn synth_commits(n: usize) -> Vec<CommitInfo> {
    (0..n)
        .map(|i| {
            let mut parents = Vec::new();
            if i + 1 < n {
                parents.push(format!("c{}", i + 1));
            }
            if i % 5 == 0 && i + 3 < n {
                parents.push(format!("c{}", i + 3));
            }
            CommitInfo {
                oid: format!("c{}", i),
                short_oid: format!("c{}", i),
                parents,
                author_name: "bench".into(),
                author_email: "bench@example.com".into(),
                time: i as i64,
                subject: format!("commit {}", i),
            }
        })
        .collect()
}

fn bench_build_graph(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_graph");
    for &size in &[50usize, 500, 5000] {
        let commits = synth_commits(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &commits, |b, commits| {
            b.iter(|| build_graph(black_box(commits)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_build_graph);
criterion_main!(benches);
