mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::frames::budget_us;
use fdt_vm::testing::{ReplayServer, ScriptedServer};
use fdt_vm::uri::VmServiceUri;
use serde_json::{Value, json};

/// The first value `getVMTimelineMicros` returns; each later call adds `STEP`.
const BASE: i64 = 1_000_000;
const STEP: i64 = 10_000;
/// Event times in the tests sit this far after the connect, so that they are after the session's
/// origin whatever the real time spent connecting.
const LATER: i64 = 3_000_000;

fn config() -> CaptureConfig {
    CaptureConfig {
        timeline_poll: Duration::from_millis(20),
        http_poll: Duration::from_secs(60),
        socket_poll: Duration::from_secs(60),
        ..CaptureConfig::default()
    }
}

/// A VM that answers the connect sequence and serves queued `getVMTimeline` replies.
struct Sim {
    micros: AtomicI64,
    polls: Mutex<VecDeque<Value>>,
    recorded: Vec<&'static str>,
    /// `None`: `_flutter.listViews` fails.
    fps: Option<f64>,
}

impl Sim {
    fn new() -> Arc<Sim> {
        Arc::new(Sim {
            micros: AtomicI64::new(BASE),
            polls: Mutex::default(),
            recorded: vec!["Dart", "Embedder", "GC"],
            fps: Some(120.0),
        })
    }

    fn with(recorded: Vec<&'static str>, fps: Option<f64>) -> Arc<Sim> {
        Arc::new(Sim {
            micros: AtomicI64::new(BASE),
            polls: Mutex::default(),
            recorded,
            fps,
        })
    }

    /// Queues the events of one `getVMTimeline` reply.
    fn push(&self, events: Vec<Value>) {
        self.polls.lock().unwrap().push_back(json!(events));
    }

    fn server(self: &Arc<Sim>) -> ScriptedServer {
        let sim = self.clone();
        ScriptedServer::start(move |m, p| {
            let iso = p["isolateId"].as_str().unwrap_or("isolates/1");
            match m {
                "getVM" => Ok(
                    json!({"type": "VM", "pid": 7, "startTime": 99, "operatingSystem": "macos",
                    "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]}),
                ),
                "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "main",
                    "extensionRPCs": [], "rootLib": {"uri": "package:demo/main.dart"}})),
                "streamListen" => Ok(json!({"type": "Success"})),
                "getVMTimelineMicros" => Ok(json!({"type": "Timestamp",
                    "timestamp": sim.micros.fetch_add(STEP, Ordering::SeqCst)})),
                "getVMTimelineFlags" => Ok(json!({"type": "TimelineFlags",
                    "availableStreams": ["Dart", "Embedder", "GC", "API"],
                    "recordedStreams": sim.recorded})),
                "setVMTimelineFlags" => Ok(json!({"type": "Success"})),
                "_flutter.listViews" if sim.fps.is_some() => Ok(json!({"type": "FlutterViewList",
                    "views": [{"type": "FlutterView", "id": "_flutterView/0x1"}]})),
                "_flutter.getDisplayRefreshRate" => {
                    Ok(json!({"type": "DisplayRefreshRate", "fps": sim.fps}))
                }
                "getVMTimeline" => {
                    let events = sim
                        .polls
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| json!([]));
                    Ok(json!({"type": "Timeline", "traceEvents": events,
                        "timeOriginMicros": 0, "timeExtentMicros": 0}))
                }
                _ => Err((-32601, "Method not found".into())),
            }
        })
    }
}

fn count(server: &ScriptedServer, method: &str) -> usize {
    server
        .received()
        .iter()
        .filter(|(m, _)| m == method)
        .count()
}

fn start(server: &ScriptedServer, cfg: CaptureConfig, rt: &CaptureRuntime) -> Capture {
    let cap = Capture::start(rt.handle(), cfg, None);
    cap.connect(Target::Uri(
        VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
    ));
    cap
}

fn ev(ph: &str, name: &str, tid: i64, ts: i64) -> Value {
    json!({"ph": ph, "name": name, "cat": "Dart", "pid": 1, "tid": tid, "ts": ts})
}

fn framed(ph: &str, name: &str, tid: i64, ts: i64, n: u64) -> Value {
    let mut v = ev(ph, name, tid, ts);
    v["args"] = json!({"frame_number": n.to_string()});
    v
}

/// The names of every stored slice.
fn slice_names(s: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for t in s.timeline.tracks() {
        for lane in s.timeline.lanes(t.id) {
            for i in 0..lane.len() {
                out.push(s.timeline.name(lane.get(i).name).to_string());
            }
        }
    }
    out
}

fn frame_event(n: u64, start: i64, build: i64) -> Value {
    json!({"streamId": "Extension", "event": {"kind": "Extension", "extensionKind": "Flutter.Frame",
        "isolate": {"type": "@Isolate", "id": "isolates/1", "name": "main", "isSystemIsolate": false},
        "timestamp": 1, "extensionData": {"number": n, "startTime": start, "elapsed": 9000,
            "build": build, "raster": 4000, "vsyncOverhead": 100}}})
}

#[test]
fn flags_add_missing_streams_once() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::with(vec!["GC"], Some(60.0));
        let server = sim.server();
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let sets: Vec<Value> = server
            .received()
            .into_iter()
            .filter(|(m, _)| m == "setVMTimelineFlags")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(sets.len(), 1, "{sets:?}");
        assert_eq!(
            sets[0]["recordedStreams"],
            json!(["GC", "Dart", "Embedder"])
        );
        assert_eq!(count(&server, "getVMTimelineFlags"), 1);

        // The engine's default set already has both: nothing is sent.
        let sim = Sim::with(vec!["Dart", "Embedder", "GC"], Some(60.0));
        let server = sim.server();
        let cap2 = start(&server, config(), &rt);
        common::settle(&cap2, Duration::from_millis(100), Duration::from_secs(5)).await;
        assert_eq!(count(&server, "getVMTimelineFlags"), 1);
        assert_eq!(count(&server, "setVMTimelineFlags"), 0);
        drop(cap);
    });
}

#[test]
fn refresh_rate_lands_in_the_run() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cap = start(&server, config(), &rt);
        let snap = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        let hz = snap.runs[0].refresh_hz;
        assert_eq!(hz, Some(120.0));
        assert_eq!(budget_us(hz), 8_333);

        // `listViews` failing: the budget stays the 60 Hz default and the connect succeeds.
        let server = Sim::with(vec!["Dart", "Embedder"], None).server();
        let cap = start(&server, config(), &rt);
        let snap = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        assert!(matches!(
            &*cap.status().borrow(),
            ConnStatus::Connected { .. }
        ));
        assert_eq!(snap.runs[0].refresh_hz, None);
        assert_eq!(budget_us(snap.runs[0].refresh_hz), 16_667);

        // A negative rate is no rate.
        let server = Sim::with(vec!["Dart", "Embedder"], Some(-30.0)).server();
        let cap = start(&server, config(), &rt);
        let snap = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        assert_eq!(snap.runs[0].refresh_hz, None);
    });
}

#[test]
fn poll_windows_are_contiguous() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        common::eventually(Duration::from_secs(5), "four polls", || {
            count(&server, "getVMTimeline") >= 4
        })
        .await;
        let polls: Vec<(i64, i64)> = server
            .received()
            .into_iter()
            .filter(|(m, _)| m == "getVMTimeline")
            .map(|(_, p)| {
                (
                    p["timeOriginMicros"].as_i64().unwrap(),
                    p["timeExtentMicros"].as_i64().unwrap(),
                )
            })
            .collect();
        // The first window starts at the connect (the calibration's reading of the clock).
        assert_eq!(polls[0].0, BASE);
        for w in polls.windows(2) {
            assert_eq!(w[1].0, w[0].0 + w[0].1 + 1, "{polls:?}");
        }
        drop(cap);
    });
}

/// One frame: a UI-thread BeginFrame with a build, and a raster DoDraw.
fn one_frame(n: u64, t: i64) -> Vec<Value> {
    vec![
        framed("B", "Animator::BeginFrame", 7, t, n),
        ev("B", "BUILD", 7, t + 100),
        ev("E", "BUILD", 7, t + 600),
        ev("E", "Animator::BeginFrame", 7, t + 900),
        framed("B", "Rasterizer::DoDraw", 8, t + 1_000, n),
        ev("E", "Rasterizer::DoDraw", 8, t + 3_000),
    ]
}

#[test]
fn polled_slices_reach_the_store_with_phases() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new();
        let server = sim.server();
        // Split mid-frame: the second poll finishes what the first began.
        let mut a = one_frame(5, BASE + LATER);
        let b = a.split_off(3);
        let mut meta = ev("M", "thread_name", 7, 0);
        meta["args"] = json!({"name": "1.ui (7)"});
        a.push(meta);
        sim.push(a);
        sim.push(b);
        let cap = start(&server, config(), &rt);
        // The frame event arrives after the timeline events; phases must find it either way.
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify("streamNotify", frame_event(5, BASE + LATER, 500));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "frame phases", |s| {
            s.frames
                .get(0)
                .is_some_and(|f| f.ui.is_some() && f.raster.is_some())
        })
        .await;
        let f = snap.frames.get(0).unwrap();
        assert_eq!(f.number, 5);
        let ui = f.ui.unwrap();
        assert_eq!((ui.ui_us, ui.build_us), (900, 500));
        assert_eq!(f.raster.unwrap().raster_us, 2_000);
        let names = slice_names(&snap);
        for n in ["Animator::BeginFrame", "BUILD", "Rasterizer::DoDraw"] {
            assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
        }
        let ui_track = snap
            .timeline
            .tracks()
            .iter()
            .find(|t| t.order == 0)
            .expect("a UI track");
        assert_eq!(&*ui_track.name, "1.ui");
        drop(cap);
    });
}

#[test]
fn replayed_frames_before_connect_are_dropped() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        // Frames the app announces again on connect started before it: old ones are dropped.
        server.notify("streamNotify", frame_event(1, BASE - 5_000, 100));
        server.notify("streamNotify", frame_event(2, BASE - 1, 100));
        server.notify("streamNotify", frame_event(3, BASE + 500, 100));
        common::wait_for(&cap, Duration::from_secs(5), "a frame", |s| {
            !s.frames.is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = cap.snapshots().borrow().clone();
        assert_eq!(snap.frames.len(), 1);
        assert_eq!(snap.frames.get(0).unwrap().number, 3);
    });
}

#[test]
fn reconnect_to_the_same_vm_does_not_duplicate_frames() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cfg = CaptureConfig {
            reconnect_uri_retry: Duration::from_millis(50),
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify("streamNotify", frame_event(10, BASE + 100, 100));
        common::wait_for(&cap, Duration::from_secs(5), "frame 10", |s| {
            s.frames.len() == 1
        })
        .await;
        server.close_abruptly();
        common::eventually(Duration::from_secs(10), "a second connect", || {
            count(&server, "getVM") >= 2
        })
        .await;
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        // The app announces its recent frames again, the old one among them.
        server.notify("streamNotify", frame_event(10, BASE + 100, 100));
        server.notify("streamNotify", frame_event(11, BASE + 5_000_000, 100));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "frame 11", |s| {
            s.frames.len() >= 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap2 = cap.snapshots().borrow().clone();
        assert_eq!(snap2.frames.len(), 2, "{}", snap.frames.len());
        assert_eq!(snap2.runs.len(), 1);
    });
}

#[test]
fn new_session_resets_the_assembler() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new();
        let server = sim.server();
        // A slice that begins in the old session...
        sim.push(vec![ev("B", "Straddler", 7, BASE + 500)]);
        let cap = start(&server, config(), &rt);
        let first = common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        common::eventually(Duration::from_secs(5), "the begin was polled", || {
            sim.polls.lock().unwrap().is_empty()
        })
        .await;
        cap.new_session();
        common::wait_for(&cap, Duration::from_secs(5), "a new session", |s| {
            s.session != first.session
        })
        .await;
        // ...and ends in the new one, which must not turn it into a slice.
        let ts = sim.micros.load(Ordering::SeqCst) + LATER;
        sim.push(vec![
            ev("E", "Straddler", 7, ts),
            ev("B", "Fresh", 7, ts + 10),
            ev("E", "Fresh", 7, ts + 20),
        ]);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the fresh slice", |s| {
            slice_names(s).iter().any(|n| n == "Fresh")
        })
        .await;
        assert_eq!(slice_names(&snap), vec!["Fresh".to_string()]);
    });
}

#[test]
fn paused_recording_drops_batches() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new();
        let server = sim.server();
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        cap.set_recording(false);
        common::eventually(Duration::from_secs(5), "paused", || {
            matches!(
                &*cap.status().borrow(),
                ConnStatus::Connected {
                    recording: false,
                    ..
                }
            )
        })
        .await;
        sim.push(vec![
            ev("B", "WhilePaused", 7, BASE + LATER),
            ev("E", "WhilePaused", 7, BASE + LATER + 100),
        ]);
        common::eventually(
            Duration::from_secs(5),
            "the paused batch was polled",
            || sim.polls.lock().unwrap().is_empty(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(slice_names(&cap.snapshots().borrow()).is_empty());
        cap.set_recording(true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        sim.push(vec![
            ev("B", "Resumed", 7, BASE + LATER + 100_000),
            ev("E", "Resumed", 7, BASE + LATER + 100_100),
        ]);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "resumed slice", |s| {
            !slice_names(s).is_empty()
        })
        .await;
        assert_eq!(slice_names(&snap), vec!["Resumed".to_string()]);
    });
}

#[test]
fn closed_connection_ends_the_poller() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cfg = CaptureConfig {
            reconnect_uri_retry: Duration::from_secs(60),
            reconnect_uri_give_up: Duration::from_secs(120),
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        common::eventually(Duration::from_secs(5), "polling", || {
            count(&server, "getVMTimeline") >= 2
        })
        .await;
        server.close_abruptly();
        common::eventually(Duration::from_secs(5), "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        let n = count(&server, "getVMTimeline");
        let micros = count(&server, "getVMTimelineMicros");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(count(&server, "getVMTimeline"), n);
        assert_eq!(count(&server, "getVMTimelineMicros"), micros);
    });
}

#[test]
fn timeline_stream_events_reach_the_store() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new().server();
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify(
            "streamNotify",
            json!({"streamId": "Timeline", "event": {"kind": "TimelineEvents",
                "timelineEvents": [ev("B", "Streamed", 9, BASE + LATER), ev("E", "Streamed", 9, BASE + LATER + 200)]}}),
        );
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a streamed slice", |s| {
            !slice_names(s).is_empty()
        })
        .await;
        assert_eq!(slice_names(&snap), vec!["Streamed".to_string()]);
    });
}

#[test]
fn replay_profile_perf() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ReplayServer::start(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/recordings/jank_lab-perf.jsonl"),
        );
        let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
        let cfg = CaptureConfig {
            timeline_poll: Duration::from_millis(10),
            ..CaptureConfig::default()
        };
        let cap = Capture::start(rt.handle(), cfg, None);
        cap.connect(Target::Uri(uri));
        let snap = common::wait_for(&cap, Duration::from_secs(15), "frames with phases", |s| {
            (0..s.frames.len())
                .filter_map(|i| s.frames.get(i))
                .filter(|f| f.ui.is_some() && f.raster.is_some())
                .count()
                >= 5
        })
        .await;
        assert_eq!(snap.runs[0].refresh_hz, Some(120.0));
        let mut checked = 0;
        for i in 0..snap.frames.len() {
            let f = snap.frames.get(i).unwrap();
            let (Some(ui), Some(_)) = (f.ui, f.raster) else {
                continue;
            };
            let d = i64::from(ui.ui_us) - i64::from(f.build_us);
            assert!(
                (0..=300).contains(&d),
                "frame {}: ui {} - build {}",
                f.number,
                ui.ui_us,
                f.build_us
            );
            checked += 1;
        }
        assert!(checked >= 5);
    });
}

// ---- CPU samples ----

use fdt_store::cpu::CpuStatus;
use fdt_vm::perfetto::encode::{Response, cpu_samples};

/// A VM like [`Sim`], serving `getPerfettoCpuSamples`. Each call returns 5 samples, newer than
/// those of the call before.
struct CpuSim {
    micros: AtomicI64,
    calls: AtomicI64,
    /// `Some`: `getPerfettoCpuSamples` fails with this server error.
    error: Mutex<Option<(i64, String)>>,
}

impl CpuSim {
    fn new() -> Arc<CpuSim> {
        Arc::new(CpuSim {
            micros: AtomicI64::new(BASE),
            calls: AtomicI64::new(0),
            error: Mutex::default(),
        })
    }

    fn server(self: &Arc<CpuSim>) -> ScriptedServer {
        let sim = self.clone();
        ScriptedServer::start(move |m, p| {
            let iso = p["isolateId"].as_str().unwrap_or("isolates/1");
            match m {
                "getVM" => Ok(
                    json!({"type": "VM", "pid": 7, "startTime": 99, "operatingSystem": "macos",
                    "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]}),
                ),
                "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "main",
                    "extensionRPCs": [], "rootLib": {"uri": "package:demo/main.dart"}})),
                "streamListen" | "setVMTimelineFlags" => Ok(json!({"type": "Success"})),
                "getVMTimelineMicros" => Ok(json!({"type": "Timestamp",
                    "timestamp": sim.micros.fetch_add(STEP, Ordering::SeqCst)})),
                "getVMTimelineFlags" => Ok(json!({"type": "TimelineFlags",
                    "availableStreams": ["Dart", "Embedder", "GC"],
                    "recordedStreams": ["Dart", "Embedder", "GC"]})),
                "getVMTimeline" => Ok(json!({"type": "Timeline", "traceEvents": [],
                    "timeOriginMicros": 0, "timeExtentMicros": 0})),
                "getPerfettoCpuSamples" => {
                    if let Some((code, message)) = sim.error.lock().unwrap().clone() {
                        return Err((code, message));
                    }
                    let n = sim.calls.fetch_add(1, Ordering::SeqCst);
                    let at = BASE + LATER + n * 100_000;
                    let samples: Vec<(u64, u32, usize)> = (0..5)
                        .map(|i| (((at + i * 1_000) * 1_000) as u64, 259, 0))
                        .collect();
                    let bytes = cpu_samples(&[Response {
                        functions: &[
                            ("main", Some("package:demo/main.dart")),
                            (
                                "_HeavyBuildPanelState.build",
                                Some("package:demo/heavy.dart"),
                            ),
                            ("mach_continuous_time", None),
                        ],
                        stacks: &[&[0, 1, 2]],
                        samples: &samples,
                    }]);
                    use base64::Engine;
                    Ok(json!({"type": "PerfettoCpuSamples", "samplePeriod": 250,
                        "sampleCount": 5, "timeOriginMicros": p["timeOriginMicros"],
                        "timeExtentMicros": p["timeExtentMicros"],
                        "samples": base64::engine::general_purpose::STANDARD.encode(bytes)}))
                }
                _ => Err((-32601, "Method not found".into())),
            }
        })
    }
}

fn cpu_config() -> CaptureConfig {
    CaptureConfig {
        cpu_poll: Duration::from_millis(20),
        cpu_call_timeout: Duration::from_millis(300),
        ..config()
    }
}

fn cpu_calls(server: &ScriptedServer) -> Vec<(String, i64, i64)> {
    server
        .received()
        .into_iter()
        .filter(|(m, _)| m == "getPerfettoCpuSamples")
        .map(|(_, p)| {
            (
                p["isolateId"].as_str().unwrap().to_string(),
                p["timeOriginMicros"].as_i64().unwrap(),
                p["timeExtentMicros"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn cpu_fetch_waits_for_frames() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = CpuSim::new().server();
        let cap = start(&server, cpu_config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(cpu_calls(&server).is_empty(), "no frames, so no fetch");
        server.notify("streamNotify", frame_event(1, BASE + 100, 100));
        common::eventually(Duration::from_secs(5), "a fetch", || {
            !cpu_calls(&server).is_empty()
        })
        .await;
        let snap = common::wait_for(&cap, Duration::from_secs(5), "samples", |s| {
            s.cpu.samples().len() == 5
        })
        .await;
        assert_eq!(snap.cpu.status(snap.runs[0].id), &CpuStatus::On);
        assert_eq!(snap.cpu.period_us(snap.runs[0].id), 250);
        assert!(snap.cpu.coverage_end(snap.runs[0].id).is_some());
        // The one frame is spent: nothing more is fetched.
        let n = cpu_calls(&server).len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(cpu_calls(&server).len(), n);
    });
}

#[test]
fn fetch_now_fetches_immediately() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = CpuSim::new().server();
        let cfg = CaptureConfig {
            cpu_poll: Duration::from_secs(60),
            ..cpu_config()
        };
        let cap = start(&server, cfg, &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        assert!(cpu_calls(&server).is_empty());
        cap.fetch_cpu_samples();
        common::wait_for(&cap, Duration::from_secs(5), "samples", |s| {
            s.cpu.samples().len() == 5
        })
        .await;
        assert_eq!(cpu_calls(&server).len(), 1);
    });
}

#[test]
fn cpu_windows_are_contiguous() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = CpuSim::new().server();
        let cap = start(&server, cpu_config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        let mut n = 0;
        while cpu_calls(&server).len() < 4 {
            n += 1;
            server.notify("streamNotify", frame_event(n, BASE + 100, 100));
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(n < 300, "four fetches never happened");
        }
        let calls = cpu_calls(&server);
        // The first window starts at the connect.
        assert_eq!(calls[0].1, BASE);
        for w in calls.windows(2) {
            assert_eq!(w[1].1, w[0].1 + w[0].2 + 1, "{calls:?}");
        }
        drop(cap);
    });
}

#[test]
fn profiler_off_marks_status_and_stops() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = CpuSim::new();
        *sim.error.lock().unwrap() = Some((-32000, "profiler is disabled".into()));
        let server = sim.server();
        let cap = start(&server, cpu_config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify("streamNotify", frame_event(1, BASE + 100, 100));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "profiler off", |s| {
            s.runs
                .first()
                .is_some_and(|r| matches!(s.cpu.status(r.id), CpuStatus::Off(_)))
        })
        .await;
        assert_eq!(
            snap.cpu.status(snap.runs[0].id),
            &CpuStatus::Off("CPU sampling is off in this app.".into())
        );
        for n in 2..6 {
            server.notify("streamNotify", frame_event(n, BASE + 100, 100));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        cap.fetch_cpu_samples();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cpu_calls(&server).len(), 1, "no CPU ticks after off");
    });
}

#[test]
fn cpu_timeout_retries_on_the_next_tick() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = CpuSim::new().server();
        let cfg = CaptureConfig {
            cpu_call_timeout: Duration::from_millis(60),
            ..cpu_config()
        };
        let cap = start(&server, cfg, &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify("streamNotify", frame_event(1, BASE + 100, 100));
        common::wait_for(&cap, Duration::from_secs(5), "first samples", |s| {
            s.cpu.samples().len() == 5
        })
        .await;
        server.silence("getPerfettoCpuSamples");
        // A frame arrived before the timeout; the retry must not need another one.
        server.notify("streamNotify", frame_event(2, BASE + 100, 100));
        common::eventually(Duration::from_secs(5), "two timed-out calls", || {
            cpu_calls(&server).len() >= 3
        })
        .await;
        let calls = cpu_calls(&server);
        assert_eq!(
            calls[1].1, calls[2].1,
            "the window is not advanced: {calls:?}"
        );
        let snap = cap.snapshots().borrow().clone();
        assert_eq!(snap.cpu.status(snap.runs[0].id), &CpuStatus::On);
    });
}

#[test]
fn hot_restart_moves_cpu_to_the_new_isolate() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = CpuSim::new().server();
        let cap = start(&server, cpu_config(), &rt);
        common::settle(&cap, Duration::from_millis(50), Duration::from_secs(5)).await;
        server.notify("streamNotify", frame_event(1, BASE + 100, 100));
        common::eventually(Duration::from_secs(5), "a fetch", || {
            !cpu_calls(&server).is_empty()
        })
        .await;
        assert_eq!(cpu_calls(&server)[0].0, "isolates/1");
        let isolate = json!({"type": "@Isolate", "id": "isolates/2", "name": "main",
            "isSystemIsolate": false});
        server.notify(
            "streamNotify",
            json!({"streamId": "Isolate", "event": {"kind": "IsolateStart", "isolate": isolate}}),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.notify("streamNotify", frame_event(2, BASE + 100, 100));
        common::eventually(Duration::from_secs(5), "a fetch on the new isolate", || {
            cpu_calls(&server).iter().any(|c| c.0 == "isolates/2")
        })
        .await;
    });
}

#[test]
fn replay_cpu_has_the_heavy_build_function() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ReplayServer::start(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/recordings/jank_lab-perf.jsonl"),
        );
        let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
        let cfg = CaptureConfig {
            timeline_poll: Duration::from_millis(10),
            cpu_poll: Duration::from_millis(10),
            ..CaptureConfig::default()
        };
        let cap = Capture::start(rt.handle(), cfg, None);
        cap.connect(Target::Uri(uri));
        let snap = common::wait_for(&cap, Duration::from_secs(15), "CPU samples", |s| {
            s.cpu.samples().len() >= 100
        })
        .await;
        let heavy = (0..snap.cpu.function_count())
            .map(|i| snap.cpu.function(fdt_store::cpu::FunctionId(i as u32)))
            .find(|f| &*f.name == "_HeavyBuildPanelState.build")
            .expect("_HeavyBuildPanelState.build");
        assert!(heavy.library.is_some());
        assert_eq!(snap.cpu.status(snap.runs[0].id), &CpuStatus::On);
    });
}
