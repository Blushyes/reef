use criterion::{Criterion, black_box, criterion_group, criterion_main};
use reef::ui::highlight::highlight_file;

fn synth_rust_lines(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            format!(
                "pub fn compute_{i}(x: i32, y: i32) -> i32 {{ let r = x * {i} + y; r }}",
                i = i
            )
        })
        .collect()
}

fn bench_highlight_rust(c: &mut Criterion) {
    let lines = synth_rust_lines(1000);
    c.bench_function("highlight_file/rust_1000_lines", |b| {
        b.iter(|| highlight_file(black_box("bench.rs"), black_box(&lines), true));
    });
}

criterion_group!(benches, bench_highlight_rust);
criterion_main!(benches);
