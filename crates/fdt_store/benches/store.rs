use criterion::{Criterion, criterion_group, criterion_main};
use fdt_store::clock::{Calibration, RunClock};
use fdt_store::frames::strip_bars;
use fdt_store::log_query::{LogFilter, compile_query, filter_logs};
use fdt_store::net::StatusClass;
use fdt_store::net_query::{ClassSet, MethodSet, NetFilter, NetTab, filter_requests};
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::{SessionTime, TimeRange};
use fdt_store::{SessionWriter, Snapshot, synth};
use std::hint::black_box;
use std::sync::Arc;

fn start(w: &mut SessionWriter) -> fdt_store::time::RunId {
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
    w.start_run(app, clock, SessionTime(0))
}

fn dataset(n: usize) -> Arc<Snapshot> {
    let mut w = SessionWriter::new(0);
    let run = start(&mut w);
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

fn net_filter_100k(c: &mut Criterion) {
    let mut w = SessionWriter::new(0);
    let run = start(&mut w);
    synth::requests(&mut w, run, 100_000, 7);
    let snap = w.publish();
    let mut errors = ClassSet::NONE;
    errors.toggle(StatusClass::ClientError);
    errors.toggle(StatusClass::ServerError);
    let host = snap.net.hosts()[3].clone();
    let filters = [
        (
            "text_products",
            NetFilter {
                text: "products".into(),
                ..NetFilter::default()
            },
        ),
        (
            "classes_4xx_5xx",
            NetFilter {
                classes: errors,
                ..NetFilter::default()
            },
        ),
        (
            "host_and_post",
            NetFilter {
                host: Some(host),
                methods: MethodSet::only(&["POST"]),
                ..NetFilter::default()
            },
        ),
        (
            "errors_tab",
            NetFilter {
                tab: NetTab::Errors,
                ..NetFilter::default()
            },
        ),
    ];
    let mut g = c.benchmark_group("net_filter_100k");
    for (label, f) in &filters {
        g.bench_function(*label, |b| {
            b.iter(|| black_box(filter_requests(&snap.net, f)))
        });
    }
    g.finish();
}

fn strip_bars_72k(c: &mut Criterion) {
    let mut w = SessionWriter::new(0);
    let run = start(&mut w);
    synth::frames(&mut w, run, 72_000, 7, 120);
    let snap = w.publish();
    let whole = TimeRange::new(SessionTime(0), snap.latest_time());
    let sub = TimeRange::new(SessionTime(100_000_000_000), SessionTime(110_000_000_000));
    let mut g = c.benchmark_group("strip_bars_72k");
    for (label, range, width) in [("whole_1000", whole, 1_000), ("sub_500", sub, 500)] {
        g.bench_function(label, |b| {
            b.iter(|| black_box(strip_bars(&snap.frames, range, width)))
        });
    }
    g.finish();
}

fn timeline_cells(c: &mut Criterion) {
    let mut w = SessionWriter::new(0);
    let run = start(&mut w);
    synth::perf(&mut w, run, 72_000, 7, 120);
    let snap = w.publish();
    let whole = TimeRange::new(SessionTime(0), snap.latest_time());
    let ui = snap.timeline.tracks()[0].id;
    let mut g = c.benchmark_group("timeline_cells");
    g.bench_function("ui_lanes_whole_2000", |b| {
        b.iter(|| {
            for l in snap.timeline.lanes(ui) {
                black_box(l.range_indices(whole));
                black_box(l.cells(whole, 2_000));
            }
        })
    });
    g.finish();
}

fn cpu_range_450k(c: &mut Criterion) {
    let mut w = SessionWriter::new(0);
    let run = start(&mut w);
    let span = TimeRange::new(SessionTime(0), SessionTime(600_000_000_000));
    synth::cpu(&mut w, run, 450_000, 7, span);
    let snap = w.publish();
    let sub = TimeRange::new(SessionTime(100_000_000_000), SessionTime(110_000_000_000));
    let mut g = c.benchmark_group("cpu_range_450k");
    g.bench_function("range_10s", |b| b.iter(|| black_box(snap.cpu.range(sub))));
    g.bench_function("walk_10s_stacks", |b| {
        b.iter(|| {
            let mut n = 0usize;
            for i in snap.cpu.range(sub) {
                n += snap
                    .cpu
                    .stack(snap.cpu.samples().get(i).unwrap().leaf)
                    .count();
            }
            black_box(n)
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    log_search_100k,
    log_search_1m,
    net_filter_100k,
    strip_bars_72k,
    timeline_cells,
    cpu_range_450k
);
criterion_main!(benches);
