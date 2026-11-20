use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use fdt_capture::bench_support::{EVENTS_PER_FRAME, decode_and_assemble, synthetic_timeline_json};
use std::hint::black_box;

fn assemble(c: &mut Criterion) {
    let frames = 200_000 / EVENTS_PER_FRAME;
    let json = synthetic_timeline_json(frames);
    let mut g = c.benchmark_group("ingest");
    g.throughput(Throughput::Elements((frames * EVENTS_PER_FRAME) as u64));
    g.bench_function("decode_and_assemble_200k", |b| {
        b.iter(|| decode_and_assemble(black_box(&json)))
    });
    g.finish();
}

criterion_group!(benches, assemble);
criterion_main!(benches);
