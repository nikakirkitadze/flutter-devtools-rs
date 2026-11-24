use criterion::{Criterion, criterion_group, criterion_main};
use fdt_analysis::cpu_tree::{bottom_up, busiest, flame_spans, top_down};
use fdt_analysis::heap::synth::{self as heap_synth, FLUTTER_VERSION, ROOT_PACKAGE};
use fdt_analysis::heap::{AnalyzeInput, analyze};
use fdt_store::heap::HeapFile;
use fdt_store::run::Mode;
use fdt_store::synth;
use fdt_store::time::{SessionTime, TimeRange};
use std::hint::black_box;
use std::sync::Arc;
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

/// Parse, reachability, the leak scan, dominators and the index of a 1,000,000-object heap.
fn heap(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("heap_1m.dartheap");
    std::fs::write(&path, heap_synth::flutter_heap(1, 1_000_000, Mode::Profile)).expect("write");
    // Dropping the last handle deletes the file, so every iteration shares this one.
    let file = Arc::new(HeapFile::new(path));
    let input = AnalyzeInput {
        file,
        flutter_version: Some(FLUTTER_VERSION.into()),
        mode: Mode::Profile,
        root_package: Some(ROOT_PACKAGE.into()),
    };
    let never = AtomicBool::new(false);
    let mut group = c.benchmark_group("heap");
    group.sample_size(10);
    group.bench_function("heap_analyze_1m", |b| {
        b.iter(|| black_box(analyze(&input, &never, &mut |_| {}).expect("analyze")))
    });
    group.finish();
}

criterion_group!(benches, cpu, heap);
criterion_main!(benches);
