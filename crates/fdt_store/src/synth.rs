//! Deterministic synthetic datasets for tests and benches (no `rand`).

use crate::cpu::{CpuSample, FunctionId, NodeId};
use crate::frames::{FrameInput, RasterPhase, UiPhases};
use crate::logs::{LogInput, LogLevel, LogSource};
use crate::memory::{GcKind, GcReason, GcRecord, MemorySample};
use crate::net::HttpUpdate;
use crate::session::SessionWriter;
use crate::time::{RunId, SessionTime, TimeRange};
use crate::timeline::{Slice, SliceCat, TrackId, TrackKind};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.next() >> 11) % n
    }
}

const WORDS: &[&str] = &[
    "request", "widget", "rebuild", "frame", "layout", "paint", "cache", "image", "decode",
    "socket", "stream", "future", "state", "provider", "route", "scroll", "gesture", "render",
    "isolate", "message", "update", "value", "key", "list", "item", "load", "save", "sync",
];

const LOGGERS: [&str; 24] = [
    "app",
    "net.http",
    "net.ws",
    "db",
    "db.sync",
    "ui.home",
    "ui.list",
    "ui.detail",
    "ui.nav",
    "auth",
    "auth.token",
    "cache",
    "image",
    "analytics",
    "push",
    "storage",
    "payments",
    "search",
    "feed",
    "profile",
    "settings",
    "router",
    "bloc",
    "platform",
];

/// Appends a line of `len` ASCII bytes made of words and occasional markers.
fn line(r: &mut Rng, len: usize, out: &mut String) {
    let start = out.len();
    while out.len() - start < len {
        match r.below(100) {
            0 => out.push_str("timeout "),
            1 if r.below(4) == 0 => out.push_str("Timeout "),
            2..=9 => {
                out.push_str("user_");
                out.push_str(&r.below(1000).to_string());
                out.push(' ');
            }
            10..=13 => {
                out.push_str("status=");
                out.push_str(&(200 + r.below(4) * 100 + r.below(20)).to_string());
                out.push(' ');
            }
            _ => {
                out.push_str(WORDS[r.below(WORDS.len() as u64) as usize]);
                out.push(' ');
            }
        }
    }
    out.truncate(start + len);
}

/// Appends `n` log records, about 1 ms apart, averaging roughly 150 bytes.
pub fn logs(w: &mut SessionWriter, run: RunId, n: usize, seed: u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for i in 0..n {
        let level = match r.below(100) {
            0..=29 => LogLevel::Debug,
            30..=79 => LogLevel::Info,
            80..=91 => LogLevel::Warning,
            _ => LogLevel::Error,
        };
        let mut text = String::new();
        if r.below(100) < 5 {
            // Multi-line: a message followed by stack-like lines.
            let lines = 2 + r.below(29) as usize;
            for k in 0..lines {
                if k > 0 {
                    text.push('\n');
                }
                let len = 40 + r.below(41) as usize;
                line(&mut r, len, &mut text);
            }
        } else {
            let len = 40 + r.below(135) as usize;
            line(&mut r, len, &mut text);
        }
        w.push_log(LogInput {
            run,
            time: SessionTime(i as u64 * 1_000_000 + r.below(900_000)),
            clamped: false,
            level,
            source: LogSource::Log,
            logger: LOGGERS[r.below(24) as usize].to_string(),
            text,
            error: None,
        });
    }
}

const HOSTS: [&str; 12] = [
    "api.example.com",
    "cdn.example.com",
    "auth.example.com",
    "127.0.0.1:53452",
    "img.example.net",
    "search.example.com",
    "pay.example.com",
    "push.example.org",
    "feed.example.com",
    "maps.example.net",
    "chat.example.io",
    "files.example.org",
];

const PATHS: [&str; 10] = [
    "/v2/products?page=3",
    "/v2/products/42",
    "/v2/orders",
    "/v2/users/me",
    "/login",
    "/static/app.js",
    "/images/hero.png",
    "/search?q=widget",
    "/feed",
    "/health",
];

const TYPES: [&str; 4] = [
    "application/json",
    "text/html; charset=utf-8",
    "image/png",
    "text/plain",
];

/// Appends `n` HTTP requests, about 1 ms apart, without bodies.
pub fn requests(w: &mut SessionWriter, run: RunId, n: usize, seed: u64) {
    let mut r = Rng(seed.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1);
    for i in 0..n {
        let host = HOSTS[r.below(12) as usize];
        let path = PATHS[r.below(10) as usize];
        let method = match r.below(100) {
            0..=69 => "GET",
            70..=89 => "POST",
            90..=93 => "PUT",
            94..=96 => "PATCH",
            _ => "DELETE",
        };
        let start = i as u64 * 1_000_000 + r.below(900_000);
        // Skewed towards short requests: 5 ms to 3 s.
        let u = r.below(1000);
        let duration = (5 + u * u * 2995 / 1_000_000) * 1_000_000;
        let roll = r.below(100);
        let (status, error, pending) = match roll {
            0..=79 => (
                Some(200 + [0, 1, 4][r.below(3) as usize] as u16),
                None,
                false,
            ),
            80..=84 => (Some(301 + r.below(3) as u16 * 3), None, false),
            85..=92 => (
                Some([400, 401, 403, 404, 429][r.below(5) as usize]),
                None,
                false,
            ),
            93..=96 => (Some([500, 502, 503][r.below(3) as usize]), None, false),
            97..=98 => (
                None,
                Some("SocketException: Connection refused".to_string()),
                false,
            ),
            _ => (None, None, true),
        };
        let size = r.below(1000);
        let size = (size * size * 2_000_000 / 1_000_000) as i64;
        let ok = status.is_some();
        w.upsert_request(HttpUpdate {
            run,
            isolate: "isolates/1".into(),
            vm_id: i.to_string(),
            method: method.into(),
            url: format!("https://{host}{path}"),
            start: SessionTime(start),
            start_clamped: false,
            request_sent: (!pending).then_some(SessionTime(start + 100_000)),
            response_start: ok.then_some(SessionTime(start + duration / 2)),
            end: (!pending).then_some(SessionTime(start + duration)),
            status,
            reason: None,
            request_headers: Vec::new(),
            response_headers: if ok {
                vec![(
                    "content-type".into(),
                    vec![TYPES[r.below(4) as usize].into()],
                )]
            } else {
                Vec::new()
            },
            request_content_length: None,
            response_content_length: ok.then_some(size),
            events: Vec::new(),
            error,
            websocket: false,
            remote: Some(("127.0.0.1".into(), 53452)),
        });
    }
}

/// Appends `n` frames at `1/hz` spacing: 3 % slow (budget to 2x budget) and
/// 1 % janky (over 2x budget), where budget is `1/hz`.
pub fn frames(w: &mut SessionWriter, run: RunId, n: usize, seed: u64, hz: u32) {
    let mut r = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    push_frames(w, run, &mut r, 0, n, hz);
}

/// Frames `first..first + n`, drawing their timings from `r`.
fn push_frames(w: &mut SessionWriter, run: RunId, r: &mut Rng, first: u64, n: usize, hz: u32) {
    let hz = hz.max(1) as u64;
    let budget = (1_000_000 / hz).max(4);
    for i in first..first + n as u64 {
        let elapsed = match r.below(100) {
            0 => 2 * budget + 1 + r.below(2 * budget),
            1..=3 => budget + r.below(budget),
            _ => budget / 4 + r.below(budget / 2),
        } as u32;
        let build = (elapsed as u64 * (30 + r.below(30)) / 100) as u32;
        let start = SessionTime(i * (1_000_000_000 / hz));
        let stored = w.push_frame(FrameInput {
            run,
            number: i,
            start,
            elapsed_us: elapsed,
            build_us: build,
            raster_us: elapsed - build,
            vsync_overhead_us: r.below(300) as u32,
        });
        if stored.is_some() {
            let part = |pct: u32| build * pct / 100;
            let (layout, paint, composite, finalize) = (part(10), part(15), part(10), part(5));
            w.set_frame_ui(
                run,
                i,
                UiPhases {
                    begin: start,
                    ui_us: build,
                    build_us: build - layout - paint - composite - finalize,
                    layout_us: layout,
                    paint_us: paint,
                    composite_us: composite,
                    finalize_us: finalize,
                    widgets_built: None,
                },
            );
            w.set_frame_raster(
                run,
                i,
                RasterPhase {
                    start: SessionTime(start.0 + u64::from(build) * 1_000),
                    raster_us: elapsed - build,
                },
            );
        }
    }
}

/// Frames (see [`frames`], which already carries each frame's UI and raster phases) plus about
/// 70 slices per frame that match those phases: the UI and raster threads, two async rows and a
/// `DartWorker` track with a GC slice every 50 frames. A frame whose work would overlap the
/// previous frame's in a lane (a janky frame running into the next one) gets no slices there.
pub fn perf(w: &mut SessionWriter, run: RunId, n: usize, seed: u64, hz: u32) {
    frames(w, run, n, seed, hz);
    let snap = w.publish();
    let mut slicer = PerfSlicer::new(w, run, seed, [0; 4]);
    slicer.slice_frames(w, &snap, 0..snap.frames.len());
}

/// The tracks and state that put slices under frames, one frame at a time.
struct PerfSlicer {
    r: Rng,
    ui: TrackId,
    raster: TrackId,
    frame_row: TrackId,
    pipeline: TrackId,
    worker: TrackId,
    /// Where the last slice of each lane (UI, raster, Frame, PipelineItem) ends.
    ends: [u64; 4],
}

impl PerfSlicer {
    fn new(w: &mut SessionWriter, run: RunId, seed: u64, ends: [u64; 4]) -> Self {
        let r = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let raster = w.timeline_track(run, TrackKind::Thread, 2, "io.flutter.raster");
        w.set_track_order(raster, 1);
        Self {
            r,
            ui,
            raster,
            frame_row: w.timeline_track(run, TrackKind::Async, 3, "Frame"),
            pipeline: w.timeline_track(run, TrackKind::Async, 4, "PipelineItem"),
            worker: w.timeline_track(run, TrackKind::Thread, 5, "DartWorker"),
            ends,
        }
    }

    /// Slices for the frames of `snap` whose seq is in `range`.
    fn slice_frames(
        &mut self,
        w: &mut SessionWriter,
        snap: &crate::Snapshot,
        range: std::ops::Range<usize>,
    ) {
        let (ui, raster, frame_row, pipeline, worker) = (
            self.ui,
            self.raster,
            self.frame_row,
            self.pipeline,
            self.worker,
        );
        let (r, ends) = (&mut self.r, &mut self.ends);
        for i in range {
            let f = snap.frames.get(i).expect("frame in range");
            let (Some(u), Some(rs)) = (f.ui, f.raster) else {
                continue;
            };
            let (frame_start, frame_end) = (f.start.0, f.end().0);
            if u.begin.0 >= ends[0] {
                ends[0] = ui_slices(w, ui, r, &u);
            }
            if rs.start.0 >= ends[1] {
                ends[1] = raster_slices(w, raster, r, rs.start.0, u64::from(rs.raster_us) * 1_000);
            }
            if frame_start >= ends[2] {
                let dur = frame_end - frame_start;
                let animate = Slice {
                    start: SessionTime(frame_start),
                    dur_ns: dur / 10,
                    name: w.intern_name("Animate"),
                    cat: SliceCat::Embedder,
                };
                w.push_slice(frame_row, 1, animate, false);
                let parent = Slice {
                    dur_ns: dur,
                    name: w.intern_name("Frame"),
                    ..animate
                };
                w.push_slice(frame_row, 0, parent, false);
                ends[2] = frame_end;
            }
            if frame_start >= ends[3] {
                let dur = u64::from(u.ui_us) * 1_000;
                let child = Slice {
                    start: SessionTime(frame_start),
                    dur_ns: dur / 2,
                    name: w.intern_name("PipelineProduce"),
                    cat: SliceCat::Embedder,
                };
                w.push_slice(pipeline, 1, child, false);
                let parent = Slice {
                    dur_ns: dur,
                    name: w.intern_name("PipelineItem"),
                    ..child
                };
                w.push_slice(pipeline, 0, parent, false);
                ends[3] = frame_start + dur;
            }
            if i % 50 == 49 {
                let gc = Slice {
                    start: SessionTime(frame_start + 1_000_000),
                    dur_ns: 1_000_000 + r.below(2_000_000),
                    name: w.intern_name("CollectNewGeneration"),
                    cat: SliceCat::Gc,
                };
                w.push_slice(worker, 0, gc, false);
            }
        }
    }
}

/// Appends frames with their slices to a session [`perf`] built, a few at a time, as a live app
/// would: what `fdt bench-ui live-perf` publishes.
pub struct PerfFeed {
    run: RunId,
    hz: u32,
    frame_rng: Rng,
    slicer: PerfSlicer,
    /// The seq of the next frame.
    next: usize,
}

impl PerfFeed {
    /// A feed continuing the frames already in `w` (published here), with its own random streams.
    pub fn new(w: &mut SessionWriter, run: RunId, seed: u64, hz: u32) -> Self {
        let snap = w.publish();
        let end = snap
            .frames
            .len()
            .checked_sub(1)
            .and_then(|i| snap.frames.get(i))
            .map_or(0, |f| f.end().0);
        let salt = seed ^ 0x9E37_79B9_7F4A_7C15;
        Self {
            run,
            hz,
            frame_rng: Rng(salt.wrapping_mul(0xA24B_AED4_963E_E407) | 1),
            slicer: PerfSlicer::new(w, run, salt, [end; 4]),
            next: snap.frames.len(),
        }
    }

    /// Appends `n` frames (with phases) and the slices under them, and publishes. The slicer
    /// reads the new frames from a snapshot, so this publishes twice; the first is discarded.
    pub fn append(&mut self, w: &mut SessionWriter, n: usize) -> std::sync::Arc<crate::Snapshot> {
        let first = self.next;
        push_frames(w, self.run, &mut self.frame_rng, first as u64, n, self.hz);
        let snap = w.publish();
        self.next = snap.frames.len();
        self.slicer.slice_frames(w, &snap, first..self.next);
        w.publish()
    }
}

fn slice(w: &mut SessionWriter, name: &str, start: u64, dur: u64, cat: SliceCat) -> Slice {
    Slice {
        start: SessionTime(start),
        dur_ns: dur,
        name: w.intern_name(name),
        cat,
    }
}

/// The UI thread's slices for one frame, children before parents; returns where they end.
fn ui_slices(w: &mut SessionWriter, t: TrackId, r: &mut Rng, u: &UiPhases) -> u64 {
    let us = |v: u32| u64::from(v) * 1_000;
    let begin = u.begin.0;
    let build_at = begin;
    let layout_at = build_at + us(u.build_us);
    let paint_at = layout_at + us(u.layout_us);
    let comp_at = paint_at + us(u.paint_us);
    let fin_at = comp_at + us(u.composite_us);
    let end = begin + us(u.ui_us);
    if r.below(10) == 0 {
        // Three levels of four widget builds, each level inside the first slice of the one above.
        let (at, mut dur) = (build_at, us(u.build_us));
        for depth in 3..6u16 {
            let each = dur / 4;
            for k in 0..4 {
                let name = format!("Widget{depth}_{k}.build");
                let s = slice(w, &name, at + k * each, each, SliceCat::Dart);
                w.push_slice(t, depth, s, true);
            }
            dur = each;
        }
    }
    let s = slice(w, "BUILD", build_at, us(u.build_us), SliceCat::Dart);
    w.push_slice(t, 2, s, false);
    let s = slice(w, "LAYOUT", layout_at, us(u.layout_us), SliceCat::Dart);
    w.push_slice(t, 3, s, false);
    let s = slice(
        w,
        "LAYOUT (root)",
        layout_at,
        us(u.layout_us),
        SliceCat::Dart,
    );
    w.push_slice(t, 2, s, false);
    let s = slice(w, "PAINT", paint_at, us(u.paint_us), SliceCat::Dart);
    w.push_slice(t, 3, s, false);
    let s = slice(w, "PAINT (root)", paint_at, us(u.paint_us), SliceCat::Dart);
    w.push_slice(t, 2, s, false);
    let s = slice(
        w,
        "Animator::Render",
        comp_at,
        us(u.composite_us),
        SliceCat::Embedder,
    );
    w.push_slice(t, 3, s, false);
    let s = slice(
        w,
        "COMPOSITING",
        comp_at,
        us(u.composite_us),
        SliceCat::Dart,
    );
    w.push_slice(t, 2, s, false);
    let fin = us(u.finalize_us);
    let s = slice(w, "FINALIZE TREE", fin_at, fin - fin / 4, SliceCat::Dart);
    w.push_slice(t, 2, s, false);
    let s = slice(w, "POST_FRAME", end - fin / 4, fin / 4, SliceCat::Dart);
    w.push_slice(t, 2, s, false);
    let s = slice(
        w,
        "Animator::BeginFrame",
        begin,
        us(u.ui_us),
        SliceCat::Embedder,
    );
    w.push_slice(t, 1, s, false);
    let s = slice(
        w,
        "VsyncProcessCallback",
        begin,
        us(u.ui_us),
        SliceCat::Embedder,
    );
    w.push_slice(t, 0, s, false);
    end
}

/// The raster thread's slices for one frame, children before parents.
fn raster_slices(w: &mut SessionWriter, t: TrackId, r: &mut Rng, start: u64, dur: u64) -> u64 {
    let preroll = dur / 5;
    let paint_at = start + preroll;
    let paint = dur * 7 / 10;
    let n = 40 + r.below(23);
    let each = paint / n;
    for k in 0..n {
        let s = slice(
            w,
            "Canvas::saveLayer",
            paint_at + k * each,
            each * 4 / 5,
            SliceCat::Embedder,
        );
        w.push_slice(t, 4, s, false);
    }
    let s = slice(w, "LayerTree::Preroll", start, preroll, SliceCat::Embedder);
    w.push_slice(t, 3, s, false);
    let s = slice(w, "LayerTree::Paint", paint_at, paint, SliceCat::Embedder);
    w.push_slice(t, 3, s, false);
    for (depth, name) in [
        (2, "Rasterizer::DrawToSurfaces"),
        (1, "Rasterizer::DoDraw"),
        (0, "GPURasterizer::Draw"),
    ] {
        let s = slice(w, name, start, dur, SliceCat::Embedder);
        w.push_slice(t, depth, s, false);
    }
    start + dur
}

/// Appends `samples` CPU samples spread evenly over `span`, drawn from about 300 functions
/// (20 % in the app's `screen.dart`, 50 % in the framework, 30 % native) and a call-stack trie
/// of about 3,000 nodes, at most 40 deep.
pub fn cpu(w: &mut SessionWriter, run: RunId, samples: usize, seed: u64, span: TimeRange) {
    const APP: &str = "file:///app/lib/screen.dart";
    const FRAMEWORK: &str = "file:///sdk/flutter/packages/flutter/lib/src/widgets/framework.dart";
    const FUNCTIONS: u64 = 300;
    const NODES: usize = 3_000;
    const MAX_DEPTH: usize = 40;
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let funcs: Vec<FunctionId> = (0..FUNCTIONS)
        .map(|i| match i % 10 {
            0..=1 => w.intern_function(&format!("screenFn{i}"), Some(APP)),
            2..=6 => w.intern_function(&format!("widgetFn{i}"), Some(FRAMEWORK)),
            _ => w.intern_function(&format!("native::fn{i}"), None),
        })
        .collect();
    // Grow the trie from random existing stacks; each stack's leaf is a sampling target.
    let mut stacks: Vec<Vec<FunctionId>> = vec![Vec::new()];
    let mut leaves: Vec<NodeId> = Vec::new();
    let mut nodes = 1usize;
    while nodes < NODES {
        let mut st = stacks[r.below(stacks.len() as u64) as usize].clone();
        for _ in 0..1 + r.below(4) {
            if st.len() < MAX_DEPTH {
                st.push(funcs[r.below(FUNCTIONS) as usize]);
            }
        }
        let leaf = w.intern_stack(&st);
        if leaf.0 as usize >= nodes {
            nodes = leaf.0 as usize + 1;
            leaves.push(leaf);
            stacks.push(st);
        }
    }
    let dur = span.duration_ns() as u128;
    let n = samples.max(1) as u128;
    let batch = (0..samples)
        .map(|i| CpuSample {
            time: SessionTime(span.start.0 + (dur * i as u128 / n) as u64),
            leaf: leaves[r.below(leaves.len() as u64) as usize],
        })
        .collect();
    w.push_cpu_samples(run, batch);
}

/// Appends memory samples every 500 ms over `span`, with a sawtooth heap (it climbs for 20 s,
/// then drops), and a GC about every 90 ms: 90 % scavenges and the rest alternating mark-sweep
/// and mark-compact. A 30-minute span holds 3,600 samples and 20,000 GC records.
pub fn memory(w: &mut SessionWriter, run: RunId, span: TimeRange, seed: u64) {
    const MIB: u64 = 1024 * 1024;
    const SAMPLE_NS: u64 = 500_000_000;
    const GC_NS: u64 = 90_000_000;
    const SAW_NS: u64 = 20_000_000_000;
    let mut r = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let used_at = |t: u64| 30 * MIB + (t % SAW_NS) * (40 * MIB) / SAW_NS;
    let capacity_at = |t: u64| (64 + 16 * (t / SAW_NS).min(8)) * MIB;
    let dur = span.duration_ns();
    for i in 0..dur / SAMPLE_NS {
        let t = i * SAMPLE_NS;
        w.push_memory_sample(
            run,
            MemorySample {
                time: SessionTime(span.start.0 + t),
                heap_used: used_at(t) + r.below(64) * 1024,
                heap_capacity: capacity_at(t),
                external: 6 * MIB + (t / SAW_NS) * 64 * 1024,
                rss: Some(180 * MIB + used_at(t) + (t / SAW_NS).min(32) * MIB),
            },
        );
    }
    let mut majors = 0u64;
    for i in 0..dur / GC_NS {
        let t = i * GC_NS + r.below(20_000_000);
        let (kind, reason, dur_us) = if i % 10 == 9 {
            majors += 1;
            let kind = if majors.is_multiple_of(2) {
                GcKind::MarkCompact
            } else {
                GcKind::MarkSweep
            };
            (kind, GcReason::OldSpace, 5_000 + r.below(15_000) as u32)
        } else {
            (
                GcKind::Scavenge,
                GcReason::NewSpace,
                400 + r.below(400) as u32,
            )
        };
        w.push_gc(
            run,
            GcRecord {
                time: SessionTime(span.start.0 + t),
                dur_us: Some(dur_us),
                kind,
                reason,
                heap_used: used_at(t),
                heap_capacity: capacity_at(t),
                external: 6 * MIB,
            },
        );
    }
}

/// A writer holding one run of synthetic data (`logs_n` log records, `requests_n` requests and
/// `frames_n` frames at 120 Hz), not yet published.
pub fn writer(
    seed: u64,
    logs_n: usize,
    requests_n: usize,
    frames_n: usize,
) -> (SessionWriter, RunId) {
    use crate::clock::{Calibration, RunClock};
    use crate::run::{AppInfo, Mode};
    let mut w = SessionWriter::new(0);
    let app = AppInfo {
        name: "stress".into(),
        device: Some("macOS".into()),
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
    logs(&mut w, run, logs_n, seed);
    requests(&mut w, run, requests_n, seed);
    frames(&mut w, run, frames_n, seed, 120);
    (w, run)
}

/// One run of synthetic data, published as a single snapshot.
pub fn session(
    seed: u64,
    logs_n: usize,
    requests_n: usize,
    frames_n: usize,
) -> std::sync::Arc<crate::Snapshot> {
    writer(seed, logs_n, requests_n, frames_n).0.publish()
}

/// The writer behind [`stress_session`], for scenarios that keep appending to it.
pub fn stress_writer(seed: u64) -> (SessionWriter, RunId) {
    const FRAMES: usize = 72_000;
    // `perf` pushes the frames itself, so the plain frames of `writer` are left out.
    let (mut w, run) = writer(seed, 1_000_000, 100_000, 0);
    perf(&mut w, run, FRAMES, seed, 120);
    w.set_refresh_rate(run, 120.0);
    let span = TimeRange::new(
        SessionTime(0),
        SessionTime(FRAMES as u64 * (1_000_000_000 / 120)),
    );
    cpu(&mut w, run, 450_000, seed, span);
    // The memory series is 30 minutes: 3,600 samples and 20,000 GC records.
    memory(
        &mut w,
        run,
        TimeRange::new(SessionTime(0), SessionTime(30 * 60 * 1_000_000_000)),
        seed,
    );
    (w, run)
}

/// The `fdt bench-ui` and M3 dataset in one run: 1,000,000 logs, 100,000 requests, 72,000
/// frames (10 minutes at 120 Hz) with about 5,000,000 timeline slices, and 450,000 CPU samples.
pub fn stress_session(seed: u64) -> std::sync::Arc<crate::Snapshot> {
    stress_writer(seed).0.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::writer;
    use crate::net::StatusClass;

    #[test]
    fn synthetic_memory_follows_the_documented_shape() {
        let (mut w, run) = super::writer(1, 0, 0, 0);
        let span = TimeRange::new(SessionTime(0), SessionTime(30 * 60 * 1_000_000_000));
        memory(&mut w, run, span, 3);
        let s = w.publish();
        assert_eq!(s.memory.samples().len(), 3_600);
        assert_eq!(s.memory.gcs().len(), 20_000);
        let majors = s
            .memory
            .gcs()
            .chunks()
            .flatten()
            .filter(|g| g.kind.is_major())
            .count();
        assert_eq!(majors, 2_000);
    }

    #[test]
    fn small_session_has_the_requested_sizes() {
        let s = session(1, 100, 50, 20);
        assert_eq!((s.logs.len(), s.net.len()), (100, 50));
        assert_eq!(s.runs.len(), 1);
    }

    #[test]
    fn synthetic_requests_follow_the_documented_mix() {
        let (mut w, run) = writer(1024);
        requests(&mut w, run, 20_000, 7);
        let s = w.publish();
        assert_eq!(s.net.len(), 20_000);
        assert_eq!(s.net.hosts().len(), 12);
        let n = |c: StatusClass| {
            (0..s.net.len())
                .filter(|&i| s.net.request(i).status_class() == c)
                .count() as f64
                / 200.0
        };
        assert!((75.0..85.0).contains(&n(StatusClass::Success)));
        assert!((5.0..11.0).contains(&n(StatusClass::ClientError)));
        assert!((0.3..2.0).contains(&n(StatusClass::Pending)));
        let posts = (0..s.net.len())
            .filter(|&i| s.net.request(i).method == "POST")
            .count();
        assert!((3_000..5_000).contains(&posts));
    }

    /// FNV-1a over the bytes fed to it, so the pin below does not depend on a hasher's version.
    struct Fnv(u64);
    impl Fnv {
        fn put(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
            }
        }
        fn num(&mut self, n: u64) {
            self.put(&n.to_le_bytes());
        }
    }

    /// `(frames, slices, hash)` of everything `perf` stores: every frame with its phases, every
    /// track, and every slice of every lane as (track, depth, start, dur, name, category).
    fn perf_fingerprint(frames: usize, seed: u64) -> (usize, u64, u64) {
        let (mut w, run) = super::writer(seed, 0, 0, 0);
        perf(&mut w, run, frames, seed, 120);
        let snap = w.publish();
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        for i in 0..snap.frames.len() {
            h.put(format!("{:?}", snap.frames.get(i).unwrap()).as_bytes());
        }
        for (t, info) in snap.timeline.tracks().iter().enumerate() {
            h.put(format!("{info:?}").as_bytes());
            for (depth, lane) in snap.timeline.lanes(info.id).iter().enumerate() {
                for i in 0..lane.len() {
                    let s = lane.get(i);
                    h.num(t as u64);
                    h.num(depth as u64);
                    h.num(s.start.0);
                    h.num(s.dur_ns);
                    h.put(snap.timeline.name(s.name).as_bytes());
                    h.put(format!("{:?}", s.cat).as_bytes());
                }
            }
        }
        (snap.frames.len(), snap.timeline.slice_count(), h.0)
    }

    const PERF_PIN: (usize, u64, u64) = (2_000, 137_658, 271_936_546_378_956_273);

    #[test]
    fn perf_output_is_unchanged_by_the_generator_split() {
        // Recorded from `perf` as of commit 16ee413, before PerfSlicer was split out.
        assert_eq!(perf_fingerprint(2_000, 42), PERF_PIN);
    }

    #[test]
    fn the_perf_feed_appends_frames_with_phases_and_slices() {
        let (mut w, run) = perf_writer();
        let mut feed = PerfFeed::new(&mut w, run, 7, 120);
        let before = w.publish();
        let (frames, slices) = (before.frames.len(), before.timeline.slice_count());
        let after = feed.append(&mut w, 2);
        assert_eq!(after.frames.len(), frames + 2);
        let last = after.frames.get(frames + 1).unwrap();
        assert!(last.ui.is_some() && last.raster.is_some());
        assert_eq!(last.number, frames as u64 + 1);
        let added = after.timeline.slice_count() - slices;
        assert!((60..=300).contains(&added), "{added} slices for two frames");
    }

    fn perf_writer() -> (SessionWriter, RunId) {
        let (mut w, run) = super::writer(3, 0, 0, 0);
        perf(&mut w, run, 200, 3, 120);
        (w, run)
    }
}
