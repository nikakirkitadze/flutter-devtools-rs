mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::{Value, json};

const BASE: i64 = 1_000_000;
const TRACE: &str = "ext.flutter.profileWidgetBuilds";
const HTTP: &str = "ext.dart.io.httpEnableTimelineLogging";
const SOCKET: &str = "ext.dart.io.socketProfilingEnabled";

fn config() -> CaptureConfig {
    CaptureConfig {
        timeline_poll: Duration::from_millis(20),
        http_poll: Duration::from_secs(60),
        socket_poll: Duration::from_secs(60),
        ..CaptureConfig::default()
    }
}

/// A VM whose profiling switches (HTTP, sockets, widget builds) remember what they were set to.
struct Sim {
    micros: AtomicI64,
    http: AtomicBool,
    socket: AtomicBool,
    trace: AtomicBool,
    /// How many enables of widget-build tracing the app still rejects.
    reject_enables: AtomicUsize,
}

impl Sim {
    fn new(http_before: bool) -> Arc<Sim> {
        Arc::new(Sim {
            micros: AtomicI64::new(BASE),
            http: AtomicBool::new(http_before),
            socket: AtomicBool::new(http_before),
            trace: AtomicBool::new(false),
            reject_enables: AtomicUsize::new(0),
        })
    }

    fn server(self: &Arc<Sim>) -> ScriptedServer {
        let sim = self.clone();
        ScriptedServer::start(move |m, p| {
            let iso = p["isolateId"].as_str().unwrap_or("isolates/1");
            // A switch: set when `enabled` is given (a bool or a string), always read back.
            let switch = |flag: &AtomicBool, ty: &str| {
                match &p["enabled"] {
                    Value::Bool(b) => flag.store(*b, Ordering::SeqCst),
                    Value::String(s) => flag.store(s == "true", Ordering::SeqCst),
                    _ => {}
                }
                let on = flag.load(Ordering::SeqCst);
                Ok(
                    json!({"type": ty, "enabled": if ty == "_extensionType" { json!(on.to_string()) } else { json!(on) }}),
                )
            };
            match m {
                "getVM" => Ok(
                    json!({"type": "VM", "pid": 7, "startTime": 99, "operatingSystem": "macos",
                    "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]}),
                ),
                "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "main",
                    "extensionRPCs": [HTTP, TRACE], "rootLib": {"uri": "package:demo/main.dart"}})),
                "streamListen" | "streamCancel" => Ok(json!({"type": "Success"})),
                "getVMTimelineMicros" => Ok(json!({"type": "Timestamp",
                    "timestamp": sim.micros.fetch_add(10_000, Ordering::SeqCst)})),
                "getVMTimelineFlags" => Ok(json!({"type": "TimelineFlags",
                    "availableStreams": ["Dart", "Embedder", "GC"],
                    "recordedStreams": ["Dart", "Embedder", "GC"]})),
                "setVMTimelineFlags" => Ok(json!({"type": "Success"})),
                "getVMTimeline" => Ok(json!({"type": "Timeline", "traceEvents": [],
                    "timeOriginMicros": 0, "timeExtentMicros": 0})),
                "getHttpProfile" | "ext.dart.io.getHttpProfile" => {
                    Ok(json!({"type": "HttpProfile", "timestamp": 1, "requests": []}))
                }
                HTTP => switch(&sim.http, "HttpTimelineLoggingState"),
                SOCKET => switch(&sim.socket, "_SocketProfilingState"),
                TRACE => {
                    let enabling = p["enabled"] == "true";
                    if enabling
                        && sim
                            .reject_enables
                            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                    {
                        return Err((-32000, "rejected".into()));
                    }
                    switch(&sim.trace, "_extensionType")
                }
                _ => Err((-32601, "Method not found".into())),
            }
        })
    }
}

fn start(server: &ScriptedServer, cfg: CaptureConfig, rt: &CaptureRuntime) -> Capture {
    let cap = Capture::start(rt.handle(), cfg, None);
    cap.connect(Target::Uri(
        VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
    ));
    cap
}

fn calls(server: &ScriptedServer, method: &str) -> Vec<Value> {
    server
        .received()
        .into_iter()
        .filter(|(m, _)| m == method)
        .map(|(_, p)| p)
        .collect()
}

/// Calls that set `method` to `on` (`enabled` given as a bool or a string).
fn sets(server: &ScriptedServer, method: &str, on: bool) -> usize {
    calls(server, method)
        .iter()
        .filter(|p| match &p["enabled"] {
            Value::Bool(b) => *b == on,
            Value::String(s) => *s == on.to_string(),
            _ => false,
        })
        .count()
}

fn streams(server: &ScriptedServer, method: &str, id: &str) -> usize {
    calls(server, method)
        .iter()
        .filter(|p| p["streamId"] == id)
        .count()
}

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

fn tracing(s: &Snapshot) -> bool {
    s.runs[0].tracing
}

const SECS5: Duration = Duration::from_secs(5);

async fn connected(server: &ScriptedServer, cfg: CaptureConfig, rt: &CaptureRuntime) -> Capture {
    let cap = start(server, cfg, rt);
    common::settle(&cap, Duration::from_millis(50), SECS5).await;
    cap
}

#[test]
fn tracing_on_sets_the_extension_and_streams() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        assert!(sim.trace.load(Ordering::SeqCst));
        assert_eq!(sets(&server, TRACE, true), 1);
        assert_eq!(streams(&server, "streamListen", "Timeline"), 1);
        // The extension is set before the stream is listened to.
        let order: Vec<String> = server
            .received()
            .into_iter()
            .filter(|(m, p)| m == TRACE && p["enabled"] == "true" || p["streamId"] == "Timeline")
            .map(|(m, _)| m)
            .collect();
        assert_eq!(order, [TRACE, "streamListen"]);
        // Polling stops: the timeline comes from the stream.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let polled = calls(&server, "getVMTimeline").len();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls(&server, "getVMTimeline").len(), polled);
        let ev = |ph: &str, ts: i64| {
            json!({"ph": ph, "name": "Streamed", "cat": "Dart", "pid": 1, "tid": 9, "ts": ts})
        };
        server.notify(
            "streamNotify",
            json!({"streamId": "Timeline", "event": {"kind": "TimelineEvents",
                "timelineEvents": [ev("B", BASE + 3_000_000), ev("E", BASE + 3_000_200)]}}),
        );
        let snap = common::wait_for(&cap, SECS5, "a streamed slice", |s| {
            !slice_names(s).is_empty()
        })
        .await;
        assert_eq!(slice_names(&snap), vec!["Streamed".to_string()]);
    });
}

fn tracing_on(s: &Snapshot) -> bool {
    !s.runs.is_empty() && tracing(s)
}

#[test]
fn tracing_off_cancels_and_resumes_polling() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let polled = calls(&server, "getVMTimeline").len();
        cap.set_trace_widget_builds(false);
        common::wait_for(&cap, SECS5, "tracing off", |s| !tracing(s)).await;
        assert!(!sim.trace.load(Ordering::SeqCst));
        assert_eq!(streams(&server, "streamCancel", "Timeline"), 1);
        common::eventually(SECS5, "polling again", || {
            calls(&server, "getVMTimeline").len() > polled
        })
        .await;
    });
}

#[test]
fn wanted_tracing_applies_on_connect() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            ..config()
        };
        let cap = connected(&server, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        assert!(sim.trace.load(Ordering::SeqCst));
        assert_eq!(streams(&server, "streamListen", "Timeline"), 1);
    });
}

#[test]
fn wanted_before_connect_is_kept() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = Capture::start(rt.handle(), config(), None);
        cap.set_trace_widget_builds(true);
        cap.connect(Target::Uri(
            VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
        ));
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
    });
}

#[test]
fn hot_restart_reapplies_tracing() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        let isolate = json!({"type": "@Isolate", "id": "isolates/2", "name": "main",
            "isSystemIsolate": false});
        server.notify(
            "streamNotify",
            json!({"streamId": "Isolate", "event": {"kind": "IsolateStart", "isolate": isolate}}),
        );
        server.notify(
            "streamNotify",
            json!({"streamId": "Isolate", "event": {"kind": "ServiceExtensionAdded",
                "isolate": isolate, "extensionRPC": TRACE}}),
        );
        common::eventually(SECS5, "tracing set on the new isolate", || {
            calls(&server, TRACE)
                .iter()
                .any(|p| p["isolateId"] == "isolates/2" && p["enabled"] == "true")
        })
        .await;
    });
}

#[test]
fn user_disconnect_restores_what_fdt_changed() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            ..config()
        };
        let cap = connected(&server, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        common::eventually(SECS5, "http on", || sim.http.load(Ordering::SeqCst)).await;
        cap.disconnect();
        common::eventually(SECS5, "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        // Everything is back as it was before fdt, by the time the status says so.
        assert!(!sim.trace.load(Ordering::SeqCst));
        assert!(!sim.http.load(Ordering::SeqCst));
        assert!(!sim.socket.load(Ordering::SeqCst));
    });
}

#[test]
fn user_disconnect_keeps_http_on_when_it_was_on_before() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(true);
        let server = sim.server();
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            ..config()
        };
        let cap = connected(&server, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        cap.disconnect();
        common::eventually(SECS5, "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        assert!(!sim.trace.load(Ordering::SeqCst));
        assert!(sim.http.load(Ordering::SeqCst));
        assert!(sim.socket.load(Ordering::SeqCst));
        assert_eq!(sets(&server, HTTP, false), 0);
    });
}

#[test]
fn closing_the_window_restores_too() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            ..config()
        };
        let cap = connected(&server, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        drop(cap);
        common::eventually(SECS5, "restored", || {
            !sim.trace.load(Ordering::SeqCst) && !sim.http.load(Ordering::SeqCst)
        })
        .await;
    });
}

#[test]
fn unexpected_drop_makes_no_restore_calls() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            reconnect_uri_retry: Duration::from_secs(60),
            reconnect_uri_give_up: Duration::from_secs(120),
            ..config()
        };
        let cap = connected(&server, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        server.close_abruptly();
        common::eventually(SECS5, "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        for m in [TRACE, HTTP, SOCKET] {
            assert_eq!(sets(&server, m, false), 0, "{m} was switched off");
        }
    });
}

#[test]
fn switching_apps_restores_the_previous_app() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (sim_a, sim_b) = (Sim::new(false), Sim::new(false));
        let (a, b) = (sim_a.server(), sim_b.server());
        let cfg = CaptureConfig {
            trace_widget_builds: true,
            ..config()
        };
        let cap = connected(&a, cfg, &rt).await;
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        common::eventually(SECS5, "http on", || sim_a.http.load(Ordering::SeqCst)).await;
        cap.connect(Target::Uri(
            VmServiceUri::parse(b.ws_uri().as_str()).unwrap(),
        ));
        common::eventually(SECS5, "app B traced", || sim_b.trace.load(Ordering::SeqCst)).await;
        assert!(!sim_a.trace.load(Ordering::SeqCst));
        assert!(!sim_a.http.load(Ordering::SeqCst));
        assert!(!sim_a.socket.load(Ordering::SeqCst));
        assert_eq!(sets(&a, TRACE, false), 1);
        for m in [TRACE, HTTP, SOCKET] {
            assert_eq!(sets(&b, m, false), 0, "{m} was switched off in B");
        }
    });
}

#[test]
fn on_off_on_then_disconnect_restores_off() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        cap.set_trace_widget_builds(false);
        common::wait_for(&cap, SECS5, "tracing off", |s| !tracing(s)).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on again", tracing_on).await;
        cap.disconnect();
        common::eventually(SECS5, "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        assert!(!sim.trace.load(Ordering::SeqCst));
    });
}

#[test]
fn on_then_off_then_disconnect_sends_no_restore() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on", tracing_on).await;
        cap.set_trace_widget_builds(false);
        common::wait_for(&cap, SECS5, "tracing off", |s| !tracing(s)).await;
        let offs = sets(&server, TRACE, false);
        cap.disconnect();
        common::eventually(SECS5, "disconnected", || {
            matches!(&*cap.status().borrow(), ConnStatus::Disconnected { .. })
        })
        .await;
        assert_eq!(sets(&server, TRACE, false), offs);
    });
}

#[test]
fn retry_after_rejected_enable_reaches_the_app() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(false);
        sim.reject_enables.store(1, Ordering::SeqCst);
        let server = sim.server();
        let cap = connected(&server, config(), &rt).await;
        cap.set_trace_widget_builds(true);
        common::eventually(SECS5, "first enable sent", || {
            sets(&server, TRACE, true) == 1
        })
        .await;
        // Rejected: the run still reports tracing off.
        assert!(!tracing(&cap.snapshots().borrow()));
        cap.set_trace_widget_builds(true);
        common::wait_for(&cap, SECS5, "tracing on after the retry", tracing_on).await;
        assert_eq!(sets(&server, TRACE, true), 2);
    });
}
