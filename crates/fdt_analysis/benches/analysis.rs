use criterion::{Criterion, criterion_group, criterion_main};
use fdt_analysis::cpu_tree::{bottom_up, busiest, flame_spans, top_down};
use fdt_store::synth;
use fdt_store::time::{SessionTime, TimeRange};
use std::hint::black_box;
use std::sync::atomic::AtomicBool;

fn cpu(c: &mut Criterion) {
    let s = synth::stress_session(42);
    let never = AtomicBool::new(false);
    let ten_s = TimeRange::new(SessionTime(100_000_000_000), SessionTime(110_000_000_000));
    c.bench_function("top_down_450k", |b| {
        b.iter(|| black_box(top_down(&s.cpu, None, &never)))
    });
    c.bench_function("bottom_up_450k", |b| {
        b.iter(|| black_box(bottom_up(&s.cpu, None, &never)))
    });
    c.bench_function("busiest_450k", |b| {
        b.iter(|| black_box(busiest(&s.cpu, None, 50)))
    });
    c.bench_function("flame_10s", |b| {
        b.iter(|| black_box(flame_spans(&s, ten_s, 1_000_000)))
    });
}

criterion_group!(benches, cpu);
criterion_main!(benches);
