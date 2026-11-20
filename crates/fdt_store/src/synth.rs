//! Deterministic synthetic datasets for tests and benches (no `rand`).

use crate::frames::{FrameInput, RasterPhase, UiPhases};
use crate::logs::{LogInput, LogLevel, LogSource};
use crate::net::HttpUpdate;
use crate::session::SessionWriter;
use crate::time::{RunId, SessionTime};
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
    let hz = hz.max(1) as u64;
    let budget = (1_000_000 / hz).max(4);
    for i in 0..n as u64 {
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
    let mut r = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
    let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
    w.set_track_order(ui, 0);
    let raster = w.timeline_track(run, TrackKind::Thread, 2, "io.flutter.raster");
    w.set_track_order(raster, 1);
    let frame_row = w.timeline_track(run, TrackKind::Async, 3, "Frame");
    let pipeline = w.timeline_track(run, TrackKind::Async, 4, "PipelineItem");
    let worker = w.timeline_track(run, TrackKind::Thread, 5, "DartWorker");
    let mut ends = [0u64; 4];
    for i in 0..snap.frames.len() {
        let f = snap.frames.get(i).expect("frame in range");
        let (Some(u), Some(rs)) = (f.ui, f.raster) else {
            continue;
        };
        let (frame_start, frame_end) = (f.start.0, f.end().0);
        if u.begin.0 >= ends[0] {
            ends[0] = ui_slices(w, ui, &mut r, &u);
        }
        if rs.start.0 >= ends[1] {
            ends[1] = raster_slices(
                w,
                raster,
                &mut r,
                rs.start.0,
                u64::from(rs.raster_us) * 1_000,
            );
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
    writer(seed, 1_000_000, 100_000, 72_000)
}

/// The `fdt bench-ui` dataset: 1,000,000 logs, 100,000 requests and 72,000 frames (10 minutes
/// at 120 Hz) in one run.
pub fn stress_session(seed: u64) -> std::sync::Arc<crate::Snapshot> {
    stress_writer(seed).0.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::writer;
    use crate::net::StatusClass;

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
}
