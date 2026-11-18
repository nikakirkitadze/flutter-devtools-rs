use criterion::{Criterion, criterion_group, criterion_main};
use fdt_app::palette::rank::{rank, synthetic_labels};

fn bench(c: &mut Criterion) {
    let items = synthetic_labels(5000);
    c.bench_function("rank_show_me_5000", |b| {
        b.iter(|| std::hint::black_box(rank("show me", &items, &[1, 2, 3])))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
