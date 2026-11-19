use criterion::{Criterion, criterion_group, criterion_main};
use fdt_store::clock::{Calibration, RunClock};
use fdt_store::log_query::{LogFilter, compile_query, filter_logs};
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::SessionTime;
use fdt_store::{SessionWriter, Snapshot, synth};
use std::hint::black_box;
use std::sync::Arc;

fn dataset(n: usize) -> Arc<Snapshot> {
    let mut w = SessionWriter::new(0);
    let app = AppInfo {
        name: "bench".into(),
        device: None,
        mode: Mode::Debug,
        flutter_version: None,
        workspace: None,
        vm_uri: "ws://127.0.0.1:1/ws".into(),
        pid: None,
    };
    let clock = RunClock::new(
        0,
        Calibration {
            skew_us: 0,
            timeline_to_wall_us: 0,
        },
    );
    let run = w.start_run(app, clock, SessionTime(0));
    synth::logs(&mut w, run, n, 7);
    w.publish()
}

fn log_search(c: &mut Criterion, name: &str, n: usize) {
    let snap = dataset(n);
    let mut g = c.benchmark_group(name);
    for (label, input, regex) in [
        ("timeout", "timeout", false),
        ("Timeout", "Timeout", false),
        ("user_42", "user_42", false),
        ("status_regex", r"status=5\d\d", true),
    ] {
        let f = LogFilter {
            text: compile_query(input, regex).unwrap(),
            ..LogFilter::default()
        };
        g.bench_function(label, |b| {
            b.iter(|| black_box(filter_logs(&snap.logs, &f, 0)))
        });
    }
    g.finish();
}

fn log_search_100k(c: &mut Criterion) {
    log_search(c, "log_search_100k", 100_000);
}

fn log_search_1m(c: &mut Criterion) {
    log_search(c, "log_search_1m", 1_000_000);
}

criterion_group!(benches, log_search_100k, log_search_1m);
criterion_main!(benches);
