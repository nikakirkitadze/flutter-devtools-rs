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
    /// When (on the tokio clock) each memory poll arrived.
    polls: std::sync::Mutex<Vec<std::time::Instant>>,
}

impl Sim {
    fn new(growing: bool, rss_fails: bool) -> Arc<Sim> {
        Arc::new(Sim {
            heap: AtomicU64::new(1000),
            rss_fails,
            growing,
            polls: Default::default(),
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
                "streamListen" | "streamCancel" | "requestHeapSnapshot" => {
                    Ok(json!({"type": "Success"}))
                }
                "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1_000_000})),
                "getVMTimelineFlags" => Ok(json!({"type": "TimelineFlags",
                    "availableStreams": ["Dart", "Embedder"],
                    "recordedStreams": ["Dart", "Embedder"]})),
                "getVMTimeline" => Ok(json!({"type": "Timeline", "traceEvents": [],
                    "timeOriginMicros": 0, "timeExtentMicros": 0})),
                "getIsolateGroupMemoryUsage" => {
                    sim.polls.lock().unwrap().push(std::time::Instant::now());
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

/// The real 500 ms default, read off the arrival stamps of five polls. (A paused clock does not
/// work here: the runtime jumps to the next timer whenever it waits on the socket, before the
/// poll's replies re-arm the interval, so the polls come out in no useful number.)
#[test]
fn memory_is_polled_every_500_ms() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let sim = Sim::new(true, false);
        let server = sim.server();
        let cfg = CaptureConfig {
            memory_poll: CaptureConfig::default().memory_poll,
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        common::eventually(Duration::from_secs(10), "five polls", || {
            sim.polls.lock().unwrap().len() >= 5
        })
        .await;
        let stamps = sim.polls.lock().unwrap().clone();
        let gaps: Vec<Duration> = stamps.windows(2).map(|p| p[1] - p[0]).collect();
        let (min, max) = (gaps.iter().min().unwrap(), gaps.iter().max().unwrap());
        assert!(
            *min >= Duration::from_millis(450) && *max <= Duration::from_millis(2000),
            "polls are 500 ms apart: {gaps:?}"
        );
        let span = stamps[4] - stamps[0];
        assert!(
            span >= Duration::from_millis(1800),
            "four gaps span {span:?}"
        );
        assert!(
            calls(&server, "getIsolateGroupMemoryUsage")
                .iter()
                .all(|p| p["isolateGroupId"] == GROUP)
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
        // The barrier: a later GC that must be stored. Events arrive in order, so once it is
        // here the second copy has been seen, and must not have produced a record.
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (12, 0.0116), (5, 0.0050), t + 20),
        );
        let s = common::wait_for(&cap, LIMIT, "the barrier record", |s| {
            s.memory.gcs().len() >= 3
        })
        .await;
        let g = s.memory.gcs();
        assert_eq!(g.len(), 3, "the second isolate's copy is not a record");
        assert_eq!(g.get(0).unwrap().dur_us, None);
        assert_eq!(g.get(1).unwrap().dur_us, Some(600));
        assert_eq!(g.get(1).unwrap().heap_used, 200);
        assert_eq!(g.get(2).unwrap().dur_us, Some(1000));
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
        // The barrier, after the event that must be ignored.
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (1, 0.1), (1, 0.1), t + 1),
        );
        let s = common::wait_for(&cap, LIMIT, "the barrier", |s| !s.memory.gcs().is_empty()).await;
        assert_eq!(s.memory.gcs().len(), 1);
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
        // The GC goes first, then the profile request. The server writes a notification and a
        // reply to one connection in order, and the client reader hands them on in order, so
        // once the profile is stored the GC frame has been routed, and dropped as paused.
        note_gc(
            &server,
            gc_event("isolates/1", GROUP, (3, 0.1), (3, 0.1), now_ms()),
        );
        cap.fetch_allocation_profile(false);
        common::eventually(LIMIT, "more polls", || {
            calls(&server, "getIsolateGroupMemoryUsage").len() >= polls + 5
        })
        .await;
        let s = common::wait_for(&cap, LIMIT, "a profile", |s| {
            !s.memory.profiles().is_empty()
        })
        .await;
        assert!(s.memory.gcs().is_empty());
        assert_eq!(s.memory.samples().len(), n);
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
        // Not `connected`: the growing sim keeps publishing samples, so it never settles.
        let s = cap.snapshots().borrow().clone();
        assert_eq!(s.memory.gcs().len(), 1);
        assert_eq!(
            s.memory.gcs().get(0).unwrap().dur_us,
            None,
            "a new group's first GC"
        );
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

// ---- Heap snapshots ----

use fdt_analysis::heap::{SnapshotBuilder, Value as HeapValue, encode_full};
use fdt_store::heap::{HeapId, HeapRecord, HeapStage, LeakKind};
use fdt_vm::heap_snapshot::encode_frame;

const FW: &str = "package:flutter/src/widgets/framework.dart";
const APP: &str = "package:demo/leaks.dart";

/// A snapshot with one unmounted `State` (a finding) and `things` objects of a class of the app.
fn heap_bytes(things: u32) -> Vec<u8> {
    let mut b = SnapshotBuilder::new();
    let nul = b.class("Null", "dart:core", &[]);
    b.class("State", FW, &["type_arguments", "_widget", "_element"]);
    let leaky = b.class(
        "_LeakyState",
        APP,
        &["type_arguments", "_widget", "_element", "_text"],
    );
    let thing = b.class("Thing", APP, &["next"]);
    let null = b.object(nul, 0, HeapValue::Null, &[]);
    let st = b.object(leaky, 32, HeapValue::None, &[0, 0, null, 0]);
    b.hash(st, 777);
    b.static_field("instance", st);
    for i in 0..things {
        let t = b.object(thing, 24, HeapValue::None, &[0]);
        b.hash(t, 1000 + i);
        b.static_field(&format!("thing{i}"), t);
    }
    encode_full(&b.finish())
}

fn frames_of(bytes: &[u8], parts: usize) -> Vec<Vec<u8>> {
    let size = bytes.len().div_ceil(parts);
    let n = bytes.chunks(size).count();
    bytes
        .chunks(size)
        .enumerate()
        .map(|(i, c)| encode_frame("isolates/1", now_ms() + i as i64, i + 1 == n, c))
        .collect()
}

fn script(server: &ScriptedServer, bytes: &[u8]) {
    server.script_binary("requestHeapSnapshot", frames_of(bytes, 3));
}

fn heap_config(dir: &std::path::Path) -> CaptureConfig {
    CaptureConfig {
        heap_dir: Some(dir.to_path_buf()),
        reconnect_uri_retry: Duration::from_millis(50),
        ..config()
    }
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

async fn heap_is(cap: &Capture, what: &str, pred: impl Fn(&HeapRecord) -> bool) -> Arc<Snapshot> {
    common::wait_for(cap, LIMIT, what, |s| s.memory.heaps().iter().any(&pred)).await
}

fn ready(h: &HeapRecord) -> bool {
    h.stage == HeapStage::Ready
}

fn failed_with(h: &HeapRecord, text: &str) -> bool {
    matches!(&h.stage, HeapStage::Failed { message, .. } if &**message == text)
}

fn count_ready(s: &Snapshot) -> usize {
    s.memory.heaps().iter().filter(|h| ready(h)).count()
}

/// The calls on the `HeapSnapshot` stream and the request, in the order they arrived.
fn transfer_calls(server: &ScriptedServer) -> Vec<String> {
    server
        .received()
        .into_iter()
        .filter_map(|(m, p)| match m.as_str() {
            "streamListen" | "streamCancel" if p["streamId"] == "HeapSnapshot" => {
                Some(format!("{m} {}", p["streamId"].as_str().unwrap()))
            }
            "requestHeapSnapshot" => Some(format!("{m} {}", p["isolateId"].as_str().unwrap())),
            _ => None,
        })
        .collect()
}

#[test]
fn take_subscribes_requests_and_cancels_in_order() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(0));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        assert!(
            transfer_calls(&server).is_empty(),
            "nothing is subscribed until a snapshot is asked for"
        );
        cap.take_heap_snapshot();
        heap_is(&cap, "a ready snapshot", ready).await;
        common::eventually(LIMIT, "the stream cancel", || {
            transfer_calls(&server).len() == 3
        })
        .await;
        assert_eq!(
            transfer_calls(&server),
            [
                "streamListen HeapSnapshot",
                "requestHeapSnapshot isolates/1",
                "streamCancel HeapSnapshot"
            ]
        );
        drop(cap);
    });
}

#[test]
fn chunks_become_a_ready_record_with_findings() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let bytes = heap_bytes(2);
        script(&server, &bytes);
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        let s = heap_is(&cap, "a ready snapshot", |h| ready(h) && h.index.is_some()).await;
        let h = &s.memory.heaps()[0];
        assert_eq!(h.bytes, bytes.len() as u64);
        assert_eq!(&*h.isolate, "isolates/1");
        assert_eq!(s.memory.held(), Some(h.id));
        let sum = h.summary.as_ref().unwrap();
        assert_eq!(sum.findings_total, 1, "{:?}", sum.checks);
        assert_eq!(sum.findings[0].kind, LeakKind::UnmountedState);
        assert_eq!(sum.root_package.as_deref(), Some("demo"));
        // The file holds exactly the chunks, in order, in the heap directory.
        let path = h.file.as_ref().unwrap().path().to_path_buf();
        assert_eq!(path.parent(), Some(dir.path()));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // `<pid>-<seq>-<session>-<id>`; the sequence is process-wide, so only its shape is fixed.
        let parts: Vec<&str> = name.strip_suffix(".dartheap").unwrap().split('-').collect();
        assert_eq!(parts.len(), 4, "{name}");
        assert_eq!(parts[0], std::process::id().to_string());
        assert!(parts[1].parse::<u64>().is_ok(), "{name}");
        assert_eq!(parts[2], "0");
        assert_eq!(parts[3], h.id.0.to_string());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "snapshots are private to the user");
        }
        drop(cap);
    });
}

#[test]
fn success_without_chunks_fails() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "a failure", |h| {
            failed_with(h, "The app didn't send a snapshot.")
        })
        .await;
        common::eventually(LIMIT, "the stream cancel", || {
            transfer_calls(&server)
                .last()
                .is_some_and(|c| c.starts_with("streamCancel"))
        })
        .await;
        assert!(files(dir.path()).is_empty(), "the empty file is deleted");
        // The next snapshot works.
        script(&server, &heap_bytes(0));
        cap.take_heap_snapshot();
        let s = heap_is(&cap, "a ready snapshot", ready).await;
        assert_eq!(s.memory.heaps().len(), 2);
        drop(cap);
    });
}

#[test]
fn success_without_the_last_chunk_fails() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let mut frames = frames_of(&heap_bytes(0), 3);
        frames.pop();
        server.script_binary("requestHeapSnapshot", frames);
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "a failure", |h| {
            failed_with(h, "The app stopped sending the snapshot.")
        })
        .await;
        assert!(files(dir.path()).is_empty());
        drop(cap);
    });
}

#[test]
fn unsolicited_chunks_are_ignored() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let bytes = heap_bytes(0);
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        // Another tool's snapshot while none of ours is in flight.
        for f in frames_of(&bytes, 3) {
            server.send_binary(f);
        }
        // During ours, a chunk of another isolate comes first.
        let mut ours = vec![encode_frame("isolates/9", 1, true, b"not our snapshot")];
        ours.extend(frames_of(&bytes, 3));
        server.script_binary("requestHeapSnapshot", ours);
        cap.take_heap_snapshot();
        let s = heap_is(&cap, "a ready snapshot", ready).await;
        assert_eq!(s.memory.heaps().len(), 1);
        assert_eq!(s.memory.heaps()[0].bytes, bytes.len() as u64);
        drop(cap);
    });
}

#[test]
fn second_take_while_busy_is_ignored() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        server.silence("requestHeapSnapshot");
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the request", || {
            !calls(&server, "requestHeapSnapshot").is_empty()
        })
        .await;
        cap.take_heap_snapshot();
        // The profile fetch is handled after the second take, so its arrival is the barrier.
        cap.fetch_allocation_profile(false);
        common::eventually(LIMIT, "the barrier", || {
            !calls(&server, "getAllocationProfile").is_empty()
        })
        .await;
        assert_eq!(calls(&server, "requestHeapSnapshot").len(), 1);
        let s = connected(&cap).await;
        assert_eq!(s.memory.heaps().len(), 1);
        drop(cap);
    });
}

#[test]
fn chunk_timeout_fails_and_deletes_the_file() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        server.silence("requestHeapSnapshot");
        let cfg = CaptureConfig {
            heap_chunk_timeout: Duration::from_millis(300),
            ..heap_config(dir.path())
        };
        let cap = start(&server, cfg, &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the request", || {
            !calls(&server, "requestHeapSnapshot").is_empty()
        })
        .await;
        server.send_binary(frames_of(&heap_bytes(0), 3).remove(0));
        heap_is(&cap, "bytes received", |h| {
            matches!(h.stage, HeapStage::Receiving { chunks: 1, .. })
        })
        .await;
        assert_eq!(files(dir.path()).len(), 1);
        heap_is(&cap, "a failure", |h| {
            failed_with(h, "The app stopped sending the snapshot.")
        })
        .await;
        assert!(files(dir.path()).is_empty());
        drop(cap);
    });
}

#[test]
fn disconnect_mid_snapshot_fails_cleanly() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        server.silence("requestHeapSnapshot");
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the request", || {
            !calls(&server, "requestHeapSnapshot").is_empty()
        })
        .await;
        server.send_binary(frames_of(&heap_bytes(0), 3).remove(0));
        heap_is(&cap, "bytes received", |h| {
            matches!(h.stage, HeapStage::Receiving { .. })
        })
        .await;
        server.close_abruptly();
        heap_is(&cap, "a failure", |h| {
            failed_with(h, "The app disconnected before the snapshot finished.")
        })
        .await;
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        drop(cap);
    });
}

#[test]
fn cancel_while_receiving_and_while_analysing() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        // Receiving: the file goes at once, the rest of the snapshot is thrown away.
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        server.silence("requestHeapSnapshot");
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the request", || {
            !calls(&server, "requestHeapSnapshot").is_empty()
        })
        .await;
        let mut frames = frames_of(&heap_bytes(0), 3);
        server.send_binary(frames.remove(0));
        heap_is(&cap, "bytes received", |h| {
            matches!(h.stage, HeapStage::Receiving { .. })
        })
        .await;
        cap.cancel_heap_snapshot();
        let s = heap_is(&cap, "cancelled", |h| h.stage == HeapStage::Cancelled).await;
        assert_eq!(s.memory.busy(), None);
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        for f in frames {
            server.send_binary(f);
        }
        common::eventually(LIMIT, "the stream cancel", || {
            transfer_calls(&server)
                .last()
                .is_some_and(|c| c.starts_with("streamCancel"))
        })
        .await;
        assert!(files(dir.path()).is_empty(), "later chunks write nothing");
        let s = connected(&cap).await;
        assert_eq!(s.memory.heaps()[0].stage, HeapStage::Cancelled);
        drop(cap);

        // Analysing: the job stops, and the file stays with its record.
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let big =
            fdt_analysis::heap::synth::flutter_heap(1, 400_000, fdt_store::run::Mode::Profile);
        server.script_binary("requestHeapSnapshot", frames_of(&big, 8));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "analysing", |h| {
            matches!(h.stage, HeapStage::Parsing { .. })
        })
        .await;
        cap.cancel_heap_snapshot();
        let s = heap_is(&cap, "cancelled", |h| h.stage == HeapStage::Cancelled).await;
        assert_eq!(s.memory.held(), None);
        // The record has no summary to keep, so its file goes; `analyze` itself never deletes.
        assert!(s.memory.heaps()[0].file.is_none());
        drop(s);
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        drop(cap);
    });
}

#[test]
fn new_session_drops_late_results_and_deletes_files() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        // A held snapshot goes with the session.
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(0));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "a ready snapshot", |h| ready(h) && h.index.is_some()).await;
        assert_eq!(files(dir.path()).len(), 1);
        cap.new_session();
        common::wait_for(&cap, LIMIT, "an empty session", |s| {
            s.memory.heaps().is_empty()
        })
        .await;
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        drop(cap);

        // One still arriving is stopped, and what comes later is thrown away.
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        server.silence("requestHeapSnapshot");
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the request", || {
            !calls(&server, "requestHeapSnapshot").is_empty()
        })
        .await;
        let mut frames = frames_of(&heap_bytes(0), 3);
        server.send_binary(frames.remove(0));
        heap_is(&cap, "bytes received", |h| {
            matches!(h.stage, HeapStage::Receiving { .. })
        })
        .await;
        cap.new_session();
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        for f in frames {
            server.send_binary(f);
        }
        common::eventually(LIMIT, "the stream cancel", || {
            transfer_calls(&server)
                .last()
                .is_some_and(|c| c.starts_with("streamCancel"))
        })
        .await;
        let s = connected(&cap).await;
        assert!(s.memory.heaps().is_empty());
        assert!(files(dir.path()).is_empty());
        drop(cap);

        // One being analysed: its result is dropped and its file deleted.
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let big =
            fdt_analysis::heap::synth::flutter_heap(2, 400_000, fdt_store::run::Mode::Profile);
        server.script_binary("requestHeapSnapshot", frames_of(&big, 8));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "analysing", |h| {
            matches!(h.stage, HeapStage::Parsing { .. })
        })
        .await;
        cap.new_session();
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        let s = connected(&cap).await;
        assert!(s.memory.heaps().is_empty());
        assert_eq!(s.memory.held(), None);
        drop(cap);
    });
}

#[test]
fn reconnect_keeps_records_and_allows_a_new_take() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(1));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        heap_is(&cap, "a ready snapshot", ready).await;
        server.close_abruptly();
        common::wait_for(&cap, LIMIT, "the old snapshot to stay", |s| {
            s.memory.heaps().len() == 1
        })
        .await;
        // The app comes back (the scripted server accepts again): same VM, same run.
        let s = connected(&cap).await;
        assert!(ready(&s.memory.heaps()[0]));
        assert_eq!(files(dir.path()).len(), 1);
        cap.take_heap_snapshot();
        let s = common::wait_for(&cap, LIMIT, "two snapshots", |s| count_ready(s) == 2).await;
        let (a, b) = (&s.memory.heaps()[0], &s.memory.heaps()[1]);
        assert_eq!(a.run, b.run);
        // The first was compared with by the second.
        let d = b.diff.as_ref().expect("a diff against the first snapshot");
        assert_eq!(d.baseline, a.id);
        assert!(d.comparable.is_ok());
        drop(cap);
    });
}

#[test]
fn stale_files_from_dead_processes_are_removed() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        // Too large for a process id on any system: it is dead.
        let dead = dir.path().join("4000000000-3-1.dartheap");
        let mine = dir
            .path()
            .join(format!("{}-0-9.dartheap", std::process::id()));
        let other = dir.path().join("notes.txt");
        let odd = dir.path().join("abc-1-1.dartheap");
        for f in [&dead, &mine, &other, &odd] {
            std::fs::write(f, b"x").unwrap();
        }
        let server = Sim::new(false, false).server();
        let cap = start(&server, heap_config(dir.path()), &rt);
        common::eventually(LIMIT, "the stale file to go", || !dead.exists()).await;
        connected(&cap).await;
        assert!(mine.exists(), "a live process's file stays");
        assert!(other.exists() && odd.exists(), "other files stay");
        drop(cap);
    });
}

#[test]
fn open_makes_an_older_snapshot_held() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        for n in [1, 3] {
            script(&server, &heap_bytes(n));
            cap.take_heap_snapshot();
            common::wait_for(&cap, LIMIT, "a ready snapshot", |s| {
                count_ready(s) == n.div_ceil(2) as usize && s.memory.busy().is_none()
            })
            .await;
        }
        let s = connected(&cap).await;
        let (first, second) = (s.memory.heaps()[0].id, s.memory.heaps()[1].id);
        assert_eq!(s.memory.held(), Some(second));
        assert!(s.memory.heaps()[0].index.is_none(), "one index is held");
        cap.open_heap_snapshot(first);
        let s = common::wait_for(&cap, LIMIT, "the first held", |s| {
            s.memory.held() == Some(first) && s.memory.busy().is_none()
        })
        .await;
        assert!(s.memory.heaps()[0].index.is_some());
        assert!(s.memory.heaps()[1].index.is_none());
        assert!(ready(&s.memory.heaps()[0]) && ready(&s.memory.heaps()[1]));
        drop(cap);
    });
}

#[test]
fn remove_held_clears_it() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(1));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        let s = heap_is(&cap, "a held snapshot", |h| ready(h) && h.index.is_some()).await;
        let id = s.memory.heaps()[0].id;
        drop(s);
        cap.remove_heap_snapshot(id);
        let s = common::wait_for(&cap, LIMIT, "the snapshot to go", |s| {
            s.memory.heaps().is_empty()
        })
        .await;
        assert_eq!(s.memory.held(), None);
        drop(s);
        common::eventually(LIMIT, "the file to go", || files(dir.path()).is_empty()).await;
        drop(cap);
    });
}

#[test]
fn two_captures_write_distinct_files_and_removing_one_keeps_the_other() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (server_a, server_b) = (
            Sim::new(false, false).server(),
            Sim::new(false, false).server(),
        );
        script(&server_a, &heap_bytes(1));
        script(&server_b, &heap_bytes(1));
        let a = start(&server_a, heap_config(dir.path()), &rt);
        let b = start(&server_b, heap_config(dir.path()), &rt);
        connected(&a).await;
        connected(&b).await;
        a.take_heap_snapshot();
        b.take_heap_snapshot();
        let sa = heap_is(&a, "a's snapshot", |h| ready(h) && h.index.is_some()).await;
        let sb = heap_is(&b, "b's snapshot", |h| ready(h) && h.index.is_some()).await;
        let (ha, hb) = (&sa.memory.heaps()[0], &sb.memory.heaps()[0]);
        let (pa, pb) = (
            ha.file.as_ref().unwrap().path().to_path_buf(),
            hb.file.as_ref().unwrap().path().to_path_buf(),
        );
        assert_ne!(pa, pb, "each capture has its own file");
        let id = ha.id;
        drop(sa);
        a.remove_heap_snapshot(id);
        common::eventually(LIMIT, "a's file to go", || !pa.exists()).await;
        assert!(pb.exists(), "the other capture's file stays");
        assert_eq!(std::fs::read(&pb).unwrap(), heap_bytes(1));
        drop(sb);
        drop(a);
        drop(b);
    });
}

#[test]
fn baseline_change_recomputes_the_diff() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        for (i, n) in [1u32, 3, 5].into_iter().enumerate() {
            script(&server, &heap_bytes(n));
            cap.take_heap_snapshot();
            common::wait_for(&cap, LIMIT, "a ready snapshot", |s| {
                count_ready(s) == i + 1 && s.memory.busy().is_none()
            })
            .await;
        }
        let s = common::wait_for(&cap, LIMIT, "the last diff", |s| {
            s.memory.heaps()[2].diff.is_some()
        })
        .await;
        let (a, b) = (s.memory.heaps()[0].id, s.memory.heaps()[1].id);
        assert_eq!(s.memory.heaps()[2].diff.as_ref().unwrap().baseline, b);
        cap.set_heap_baseline(Some(a));
        let s = common::wait_for(&cap, LIMIT, "a diff against the first", |s| {
            s.memory.heaps()[2]
                .diff
                .as_ref()
                .is_some_and(|d| d.baseline == a)
        })
        .await;
        assert_eq!(s.memory.baseline(), Some(a));
        assert!(
            s.memory.heaps()[2]
                .diff
                .as_ref()
                .unwrap()
                .comparable
                .is_ok()
        );
        // Removing the baseline goes back to the previous snapshot.
        cap.remove_heap_snapshot(a);
        common::wait_for(&cap, LIMIT, "a diff against the second", |s| {
            s.memory.heaps().len() == 2
                && s.memory.heaps()[1]
                    .diff
                    .as_ref()
                    .is_some_and(|d| d.baseline == b)
        })
        .await;
        drop(cap);
    });
}

#[test]
fn a_profile_follows_each_snapshot() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(0));
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        assert!(calls(&server, "getAllocationProfile").is_empty());
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "a profile", || {
            calls(&server, "getAllocationProfile").len() == 1
        })
        .await;
        common::wait_for(&cap, LIMIT, "it stored", |s| {
            !s.memory.profiles().is_empty()
        })
        .await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "a second profile", || {
            calls(&server, "getAllocationProfile").len() == 2
        })
        .await;
        assert!(
            calls(&server, "getAllocationProfile")
                .iter()
                .all(|p| p.get("gc").is_none()),
            "a snapshot's profile never collects garbage"
        );
        drop(cap);
    });
}

#[test]
fn a_chunk_after_the_last_is_ignored() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let bytes = heap_bytes(2);
        // Another client's snapshot of the same isolate follows ours on the connection.
        let mut frames = frames_of(&bytes, 3);
        frames.extend(frames_of(&heap_bytes(7), 2));
        server.script_binary("requestHeapSnapshot", frames);
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        let s = heap_is(&cap, "a ready snapshot", |h| ready(h) && h.index.is_some()).await;
        let h = &s.memory.heaps()[0];
        assert_eq!(h.bytes, bytes.len() as u64);
        assert_eq!(
            std::fs::read(h.file.as_ref().unwrap().path()).unwrap(),
            bytes
        );
        let sum = h.summary.as_ref().unwrap();
        let expected = fdt_analysis::heap::decode_full(&bytes)
            .unwrap()
            .objects
            .len() as u64;
        assert_eq!(sum.objects, expected);
        drop(cap);
    });
}

#[test]
fn a_late_transfer_of_an_old_session_cannot_touch_the_new_one() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        script(&server, &heap_bytes(0));
        // The old transfer is done but for the stream cancel, which takes its two seconds.
        server.silence("streamCancel");
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        cap.take_heap_snapshot();
        common::eventually(LIMIT, "the old stream cancel", || {
            !calls(&server, "streamCancel").is_empty()
        })
        .await;
        // From here on the requests go unanswered, so the new transfer stays in flight.
        server.silence("requestHeapSnapshot");
        cap.new_session();
        cap.take_heap_snapshot(); // the new session's first id is the old one's
        common::eventually(LIMIT, "the new request", || {
            calls(&server, "requestHeapSnapshot").len() == 2
        })
        .await;
        // The old result went out before the new request did; this command comes after both.
        cap.fetch_allocation_profile(false);
        common::eventually(LIMIT, "the barrier", || {
            !calls(&server, "getAllocationProfile").is_empty()
        })
        .await;
        let s = connected(&cap).await;
        assert_eq!(s.memory.heaps().len(), 1);
        let h = &s.memory.heaps()[0];
        assert!(
            h.file.is_none() && h.bytes == 0 && h.summary.is_none(),
            "the old snapshot attached to the new record: {:?}",
            h.stage
        );
        assert!(h.stage.is_busy() && h.index.is_none());
        let names = files(dir.path());
        assert_eq!(names.len(), 1, "the old file is deleted: {names:?}");
        assert!(names[0].contains("-1-0."), "{names:?}");
        // The new snapshot can still be cancelled: its file goes.
        cap.cancel_heap_snapshot();
        heap_is(&cap, "cancelled", |h| h.stage == HeapStage::Cancelled).await;
        common::eventually(LIMIT, "the new file to go", || files(dir.path()).is_empty()).await;
        drop(cap);
    });
}

fn big_heap(seed: u64) -> Vec<u8> {
    fdt_analysis::heap::synth::flutter_heap(seed, 300_000, fdt_store::run::Mode::Profile)
}

#[test]
fn cancelling_an_open_keeps_the_record_ready() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        // A big one first (a slow open later), then a small one that is held.
        server.script_binary("requestHeapSnapshot", frames_of(&big_heap(3), 8));
        cap.take_heap_snapshot();
        common::wait_for(&cap, LIMIT * 4, "the first", |s| {
            count_ready(s) == 1 && s.memory.busy().is_none()
        })
        .await;
        script(&server, &heap_bytes(1));
        cap.take_heap_snapshot();
        let s = common::wait_for(&cap, LIMIT, "the second", |s| {
            count_ready(s) == 2 && s.memory.busy().is_none()
        })
        .await;
        let (first, second) = (s.memory.heaps()[0].id, s.memory.heaps()[1].id);
        assert_eq!(s.memory.held(), Some(second));
        cap.open_heap_snapshot(first);
        heap_is(&cap, "the open", |h| {
            h.id == first && matches!(h.stage, HeapStage::Parsing { .. })
        })
        .await;
        cap.cancel_heap_snapshot();
        let s = common::wait_for(&cap, LIMIT * 4, "the first ready again", |s| {
            s.memory.busy().is_none() && ready(&s.memory.heaps()[0])
        })
        .await;
        let a = &s.memory.heaps()[0];
        assert!(a.summary.is_some() && a.file.is_some() && a.index.is_none());
        assert_eq!(s.memory.held(), Some(second), "the held one is unchanged");
        assert!(s.memory.heaps()[1].index.is_some());
        drop(cap);
    });
}

#[test]
fn a_failed_open_keeps_the_record_ready() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let (dir, server) = (
            tempfile::tempdir().unwrap(),
            Sim::new(false, false).server(),
        );
        let cap = start(&server, heap_config(dir.path()), &rt);
        connected(&cap).await;
        for n in 1..=3u32 {
            script(&server, &heap_bytes(n));
            cap.take_heap_snapshot();
            common::wait_for(&cap, LIMIT, "a ready snapshot", |s| {
                count_ready(s) == n as usize && s.memory.busy().is_none()
            })
            .await;
        }
        let s = connected(&cap).await;
        let ids: Vec<HeapId> = s.memory.heaps().iter().map(|h| h.id).collect();
        // The file of the first is gone from under us; opening it fails to read.
        std::fs::remove_file(s.memory.heaps()[0].file.as_ref().unwrap().path()).unwrap();
        drop(s);
        cap.open_heap_snapshot(ids[0]);
        // The second open is asked for while the first may still run, and waits its turn.
        cap.open_heap_snapshot(ids[1]);
        let s = common::wait_for(&cap, LIMIT, "the second held", |s| {
            s.memory.held() == Some(ids[1]) && s.memory.busy().is_none()
        })
        .await;
        let a = &s.memory.heaps()[0];
        assert_eq!(
            a.stage,
            HeapStage::Ready,
            "a failed open is not a failed record"
        );
        assert!(a.summary.is_some() && a.file.is_some() && a.index.is_none());
        drop(cap);
    });
}
