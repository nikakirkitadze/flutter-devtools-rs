mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, Target};
use fdt_store::Snapshot;
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::{Value, json};

const GROUP: &str = "isolateGroups/1";
const LIMIT: Duration = Duration::from_secs(5);

fn config() -> CaptureConfig {
    CaptureConfig {
        memory_poll: Duration::from_millis(20),
        timeline_poll: Duration::from_secs(60),
        http_poll: Duration::from_secs(60),
        socket_poll: Duration::from_secs(60),
        ..CaptureConfig::default()
    }
}

struct Sim {
    heap: AtomicU64,
    rss_fails: bool,
    /// Whether the heap grows on every poll.
    growing: bool,
}

impl Sim {
    fn new(growing: bool, rss_fails: bool) -> Arc<Sim> {
        Arc::new(Sim {
            heap: AtomicU64::new(1000),
            rss_fails,
            growing,
        })
    }

    fn server(self: &Arc<Sim>) -> ScriptedServer {
        let sim = self.clone();
        ScriptedServer::start(move |m, p| {
            let iso = p["isolateId"].as_str().unwrap_or("isolates/1");
            match m {
                "getVM" => Ok(json!({"type": "VM", "pid": 7, "startTime": 99,
                    "operatingSystem": "macos",
                    "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false,
                        "isolateGroupId": GROUP}]})),
                "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "main",
                    "extensionRPCs": [], "rootLib": {"uri": "package:demo/main.dart"}})),
                "streamListen" | "streamCancel" => Ok(json!({"type": "Success"})),
                "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1_000_000})),
                "getVMTimelineFlags" => Ok(json!({"type": "TimelineFlags",
                    "availableStreams": ["Dart", "Embedder"],
                    "recordedStreams": ["Dart", "Embedder"]})),
                "getVMTimeline" => Ok(json!({"type": "Timeline", "traceEvents": [],
                    "timeOriginMicros": 0, "timeExtentMicros": 0})),
                "getIsolateGroupMemoryUsage" => {
                    let used = if sim.growing {
                        sim.heap.fetch_add(1000, Ordering::SeqCst)
                    } else {
                        sim.heap.load(Ordering::SeqCst)
                    };
                    Ok(json!({"type": "MemoryUsage", "heapUsage": used,
                        "heapCapacity": 5000, "externalUsage": 7}))
                }
                "getProcessMemoryUsage" if sim.rss_fails => Err((-32601, "no".into())),
                "getProcessMemoryUsage" => Ok(json!({"type": "ProcessMemoryUsage",
                    "root": {"name": "Process", "size": 123_456_789}})),
                "getAllocationProfile" => Ok(json!({"type": "AllocationProfile",
                    "memoryUsage": {"heapUsage": 4242, "heapCapacity": 5000, "externalUsage": 0},
                    "members": [
                        {"type": "ClassHeapStats", "instancesCurrent": 3, "bytesCurrent": 96,
                         "class": {"type": "@Class", "id": "classes/1", "name": "Leaky",
                            "library": {"type": "@Library", "uri": "package:demo/leaks.dart"},
                            "location": {"type": "SourceLocation",
                                "script": {"type": "@Script", "uri": "package:demo/lib/leaks.dart"},
                                "line": 39}}},
                        {"type": "ClassHeapStats", "instancesCurrent": 1, "bytesCurrent": 16,
                         "class": {"type": "@Class", "id": "classes/2", "name": "_Internal",
                            "library": {"type": "@Instance", "kind": "Null"}}}]})),
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn space(collections: u64, time: f64) -> Value {
    json!({"type": "HeapSpace", "used": 100, "capacity": 200, "external": 5,
        "collections": collections, "time": time})
}

fn gc_event(isolate: &str, group: &str, new: (u64, f64), old: (u64, f64), ts: i64) -> Value {
    json!({"streamId": "GC", "event": {"kind": "GC", "gcType": "Scavenge", "reason": "new space",
        "timestamp": ts, "isolate": {"id": isolate, "isolateGroupId": group},
        "isolateGroup": {"id": group}, "new": space(new.0, new.1), "old": space(old.0, old.1)}})
}

fn note_gc(server: &ScriptedServer, ev: Value) {
    server.notify("streamNotify", ev);
}

async fn connected(cap: &Capture) -> Arc<Snapshot> {
    common::settle(cap, Duration::from_millis(100), LIMIT).await
}

#[test]
fn gc_stream_is_subscribed_at_connect() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        connected(&cap).await;
        assert!(
            calls(&server, "streamListen")
                .iter()
                .any(|p| p["streamId"] == "GC")
        );
        drop(cap);
    });
}

#[test]
fn memory_is_polled_with_the_main_isolate_group() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(true, false).server();
        let cap = start(&server, config(), &rt);
        let s = common::wait_for(&cap, LIMIT, "three samples", |s| {
            s.memory.samples().len() >= 3
        })
        .await;
        let first = s.memory.samples().get(0).unwrap();
        assert_eq!(first.heap_capacity, 5000);
        assert_eq!(first.external, 7);
        assert_eq!(first.rss, Some(123_456_789));
        assert!(
            calls(&server, "getIsolateGroupMemoryUsage")
                .iter()
                .all(|p| p["isolateGroupId"] == GROUP)
        );
        drop(cap);
    });
}

#[test]
fn rss_failure_keeps_heap_samples() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(true, true).server();
        let cap = start(&server, config(), &rt);
        let s = common::wait_for(&cap, LIMIT, "samples", |s| s.memory.samples().len() >= 3).await;
        assert!(s.memory.samples().get(0).unwrap().rss.is_none());
        assert_eq!(
            calls(&server, "getProcessMemoryUsage").len(),
            1,
            "RSS is not asked for again"
        );
        drop(cap);
    });
}

#[test]
fn idle_app_stores_few_samples() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        common::eventually(LIMIT, "20 polls", || {
            calls(&server, "getIsolateGroupMemoryUsage").len() >= 20
        })
        .await;
        let s = connected(&cap).await;
        assert!(
            s.memory.samples().len() <= 3,
            "{}",
            s.memory.samples().len()
        );
        assert!(!s.memory.samples().is_empty());
        drop(cap);
    });
}

#[test]
fn gc_events_are_deduped_and_timed() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        connected(&cap).await;
        let t = now_ms();
        // The first event of the group: no duration yet.
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (10, 0.0100), (5, 0.0050), t),
        );
        // The next GC, seen by two isolates: new space grew by 0.0006 s.
        for iso in ["isolates/1", "isolates/2"] {
            note_gc(
                &server,
                gc_event(iso, GROUP, (11, 0.0106), (5, 0.0050), t + 10),
            );
        }
        let s = common::wait_for(&cap, LIMIT, "two records", |s| s.memory.gcs().len() >= 2).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s2 = connected(&cap).await;
        assert_eq!(s.memory.gcs().len().max(s2.memory.gcs().len()), 2);
        let g = s2.memory.gcs();
        assert_eq!(g.get(0).unwrap().dur_us, None);
        assert_eq!(g.get(1).unwrap().dur_us, Some(600));
        assert_eq!(g.get(1).unwrap().heap_used, 200);
        drop(cap);
    });
}

#[test]
fn other_isolate_groups_are_ignored() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        connected(&cap).await;
        let t = now_ms();
        note_gc(
            &server,
            gc_event("isolates/9", "isolateGroups/9", (1, 0.1), (1, 0.1), t),
        );
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (1, 0.1), (1, 0.1), t + 1),
        );
        let s = common::wait_for(&cap, LIMIT, "one record", |s| !s.memory.gcs().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = connected(&cap)
            .await
            .memory
            .gcs()
            .len()
            .max(s.memory.gcs().len());
        assert_eq!(s, 1);
        drop(cap);
    });
}

#[test]
fn profile_fetch_stores_classes_and_rows() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        connected(&cap).await;
        cap.fetch_allocation_profile(false);
        let s = common::wait_for(&cap, LIMIT, "a profile", |s| {
            !s.memory.profiles().is_empty()
        })
        .await;
        let p = &s.memory.profiles()[0];
        assert_eq!((p.gc, p.heap_used, p.rows.len()), (false, 4242, 2));
        assert_eq!(&*p.isolate, "isolates/1");
        let c = s.memory.alloc_class(p.rows[0].class);
        assert_eq!(&*c.name, "Leaky");
        assert_eq!(c.library.as_deref(), Some("package:demo/leaks.dart"));
        assert_eq!(c.location.as_deref(), Some("leaks.dart:39"));
        assert_eq!((p.rows[0].instances, p.rows[0].bytes), (3, 96));
        let c = s.memory.alloc_class(p.rows[1].class);
        assert_eq!((c.library.as_deref(), c.location.as_deref()), (None, None));
        drop(cap);
    });
}

#[test]
fn profile_params_never_reset() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(false, false).server();
        let cap = start(&server, config(), &rt);
        connected(&cap).await;
        cap.fetch_allocation_profile(false);
        common::eventually(LIMIT, "first fetch", || {
            !calls(&server, "getAllocationProfile").is_empty()
        })
        .await;
        cap.fetch_allocation_profile(true);
        common::eventually(LIMIT, "second fetch", || {
            calls(&server, "getAllocationProfile").len() == 2
        })
        .await;
        let c = calls(&server, "getAllocationProfile");
        assert!(c.iter().all(|p| p.get("reset").is_none()));
        assert!(c[0].get("gc").is_none(), "{}", c[0]);
        assert_eq!(c[1]["gc"], json!(true));
        assert!(c.iter().all(|p| p["isolateId"] == "isolates/1"));
        drop(cap);
    });
}

#[test]
fn paused_recording_drops_samples_but_keeps_profiles() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(true, false).server();
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, LIMIT, "a sample", |s| !s.memory.samples().is_empty()).await;
        cap.set_recording(false);
        let before = connected(&cap).await;
        let n = before.memory.samples().len();
        let polls = calls(&server, "getIsolateGroupMemoryUsage").len();
        cap.fetch_allocation_profile(false);
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (3, 0.1), (3, 0.1), now_ms()),
        );
        common::eventually(LIMIT, "more polls", || {
            calls(&server, "getIsolateGroupMemoryUsage").len() >= polls + 5
        })
        .await;
        let s = common::wait_for(&cap, LIMIT, "a profile", |s| {
            !s.memory.profiles().is_empty()
        })
        .await;
        assert_eq!(s.memory.samples().len(), n);
        assert!(s.memory.gcs().is_empty());
        drop(cap);
    });
}

#[test]
fn hot_restart_moves_polling_to_the_new_group() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = Sim::new(true, false).server();
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, LIMIT, "a sample", |s| !s.memory.samples().is_empty()).await;
        let note = |event: Value| {
            server.notify(
                "streamNotify",
                json!({"streamId": "Isolate", "event": event}),
            )
        };
        let isolate = |id: &str, group: &str| {
            json!({"type": "@Isolate", "id": id, "name": "main", "isSystemIsolate": false,
                "isolateGroupId": group})
        };
        note(json!({"kind": "IsolateExit", "isolate": isolate("isolates/1", GROUP)}));
        note(json!({"kind": "IsolateStart", "isolate": isolate("isolates/2", "isolateGroups/2")}));
        common::eventually(LIMIT, "polling the new group", || {
            calls(&server, "getIsolateGroupMemoryUsage")
                .iter()
                .any(|p| p["isolateGroupId"] == "isolateGroups/2")
        })
        .await;
        // GC events now belong to the new group only.
        let t = now_ms();
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (1, 0.1), (1, 0.1), t),
        );
        note_gc(
            &server,
            gc_event("isolates/2", "isolateGroups/2", (1, 0.1), (1, 0.1), t + 1),
        );
        common::wait_for(&cap, LIMIT, "the new group's record", |s| {
            !s.memory.gcs().is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(connected(&cap).await.memory.gcs().len(), 1);
        // Profiles come from the new isolate.
        cap.fetch_allocation_profile(false);
        common::eventually(LIMIT, "a profile", || {
            calls(&server, "getAllocationProfile")
                .iter()
                .any(|p| p["isolateId"] == "isolates/2")
        })
        .await;
        drop(cap);
    });
}
