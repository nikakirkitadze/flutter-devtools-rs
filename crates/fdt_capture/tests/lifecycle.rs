mod common;

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::clock::host_now_us;
use fdt_store::net::{BodyKind, BodyLimits, BodyState};
use fdt_store::time::RunId;
use fdt_vm::discovery::{AppKey, DiscoveredApp};
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::{Value, json};
use tokio::sync::watch;

fn server() -> ScriptedServer {
    ScriptedServer::start(|method, _| match method {
        "getVM" => Ok(json!({"type": "VM", "pid": 1, "operatingSystem": "macos",
            "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
        "getIsolate" => Ok(
            json!({"type": "Isolate", "id": "isolates/1", "name": "main",
            "extensionRPCs": [], "rootLib": {"uri": "package:demo/main.dart"}}),
        ),
        "streamListen" => Ok(json!({"type": "Success"})),
        "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1000})),
        "ext.dart.io.getHttpProfile" => Ok(
            json!({"type": "HttpProfile", "timestamp": 1_700_000_000_000_000_i64, "requests": []}),
        ),
        _ => Err((-32601, "Method not found".into())),
    })
}

#[test]
fn dropping_capture_stops_the_session_and_closes_the_connection() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = server();
        let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
        let cap = Capture::start(rt.handle(), CaptureConfig::default(), None);
        cap.connect(Target::Uri(uri));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        assert_eq!(server.open_connections(), 1);
        drop(cap);
        for _ in 0..100 {
            if server.open_connections() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the connection stayed open after Capture was dropped");
    });
}

#[test]
fn failed_first_connect_does_not_set_the_origin() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("ws://{}/x/ws", l.local_addr().unwrap())
        };
        let cap = Capture::start(rt.handle(), CaptureConfig::default(), None);
        cap.connect(Target::Uri(VmServiceUri::parse(&dead).unwrap()));
        let mut status = cap.status();
        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| matches!(s, ConnStatus::Failed { .. })),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let after_failure = host_now_us();
        let server = server();
        cap.connect(Target::Uri(
            VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
        ));
        let snap = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        assert!(
            snap.origin_host_us >= after_failure,
            "origin {} predates the failed connect ({after_failure})",
            snap.origin_host_us
        );
    });
}

// ---------------------------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------------------------

const T: i64 = 1_700_000_000_000_000;
const ENABLE: &str = "ext.dart.io.httpEnableTimelineLogging";
const STOPPED: &str = "The app stopped before the body was copied.";

/// A fake VM with an HTTP profile whose `updatedSince` filter behaves like the real one.
#[derive(Default)]
struct Profile {
    entries: Mutex<Vec<(i64, Value)>>,
    tick: AtomicI64,
}

impl Profile {
    fn add(&self, mut entry: Value) {
        let at = T + self.tick.fetch_add(1, Ordering::SeqCst) + 1;
        entry["updated"] = json!(at);
        self.entries.lock().unwrap().push((at, entry));
    }

    /// Replaces everything the profile holds with `entry`.
    fn set(&self, entry: Value) {
        self.entries.lock().unwrap().clear();
        self.add(entry);
    }

    fn answer(&self, since: Option<i64>) -> Value {
        let reqs: Vec<Value> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(at, _)| since.is_none_or(|s| *at >= s))
            .map(|(_, e)| e.clone())
            .collect();
        json!({"type": "HttpProfile", "timestamp": T + self.tick.load(Ordering::SeqCst) + 1,
            "requests": reqs})
    }
}

fn done(id: &str) -> Value {
    json!({"id": id, "isolateId": "isolates/1", "method": "GET", "uri": format!("http://h:1/{id}"),
        "startTime": T + 5_000_000, "endTime": T + 5_000_100, "events": [],
        "request": {"headers": {}, "contentLength": 0},
        "response": {"startTime": T + 5_000_200, "endTime": T + 5_000_300, "statusCode": 200,
            "reasonPhrase": "OK", "headers": {}, "contentLength": 3}})
}

struct Fake {
    server: Arc<ScriptedServer>,
    profile: Arc<Profile>,
}

impl Fake {
    fn count(&self, method: &str) -> usize {
        self.server
            .received()
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }
}

fn fake_with(pid: u32, start: i64, body_len: usize, latency_on_body: bool) -> Fake {
    let profile = Arc::new(Profile::default());
    let p = profile.clone();
    let slot: Arc<OnceLock<std::sync::Weak<ScriptedServer>>> = Arc::default();
    let s2 = slot.clone();
    let server = Arc::new(ScriptedServer::start(move |method, params| match method {
        "getVM" => Ok(json!({"type": "VM", "pid": pid, "startTime": start,
            "operatingSystem": "macos",
            "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
        "getIsolate" => Ok(
            json!({"type": "Isolate", "id": "isolates/1", "name": "main",
            "extensionRPCs": ["ext.dart.io.getHttpProfile", ENABLE],
            "rootLib": {"uri": "package:demo/main.dart"}}),
        ),
        "streamListen" => Ok(json!({"type": "Success"})),
        "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1000})),
        m if m == ENABLE => Ok(json!({"type": "Success", "enabled": true})),
        "ext.dart.io.socketProfilingEnabled" => Ok(json!({"type": "Success"})),
        "ext.dart.io.getHttpProfile" => Ok(p.answer(params["updatedSince"].as_i64())),
        "ext.dart.io.getHttpProfileRequest" => {
            if latency_on_body && let Some(s) = s2.get().and_then(std::sync::Weak::upgrade) {
                s.set_latency(Duration::from_secs(60));
            }
            Ok(
                json!({"id": params["id"], "method": "GET", "uri": "http://h:1/x",
                "startTime": T, "responseBody": vec![65u8; body_len], "requestBody": []}),
            )
        }
        "s1.flutterVersion" => Ok(json!({"frameworkVersion": "3.99.0"})),
        _ => Err((-32601, "Method not found".into())),
    }));
    // (Weak: a strong handle would make the handler keep its own server alive.)
    // The handler needs the server to hold the body reply; without this the body arrives at once
    // and the `Pending` window the test waits for is a race.
    slot.set(Arc::downgrade(&server)).ok();
    Fake { server, profile }
}

fn fake(pid: u32, start: i64) -> Fake {
    fake_with(pid, start, 3, false)
}

fn key() -> AppKey {
    AppKey {
        package: "demo".into(),
        device: "macos".into(),
        workspace: "/w".into(),
    }
}

fn disc(server: &ScriptedServer) -> DiscoveredApp {
    DiscoveredApp {
        key: key(),
        vm_uri: server.ws_uri().to_string(),
        name: "demo".into(),
        device: "macos".into(),
        mode: None,
        flutter_version: None,
        workspace: Some("/w".into()),
        pid: None,
    }
}

fn cfg() -> CaptureConfig {
    CaptureConfig {
        http_poll: Duration::from_millis(20),
        socket_poll: Duration::from_millis(50),
        ..CaptureConfig::default()
    }
}

fn say(server: &ScriptedServer, text: &str) {
    let bytes = STANDARD.encode(format!("flutter: {text}\n"));
    server.notify(
        "streamNotify",
        json!({"streamId": "Stdout", "event": {"kind": "WriteEvent", "timestamp": 1, "bytes": bytes}}),
    );
}

fn isolate_note(event: Value) -> Value {
    json!({"streamId": "Isolate", "event": event})
}

fn isolate_ref(id: &str) -> Value {
    json!({"type": "@Isolate", "id": id, "name": "main", "isSystemIsolate": false})
}

fn texts(s: &Snapshot) -> Vec<String> {
    (0..s.logs.len())
        .map(|i| s.logs.row(i).text.to_string())
        .collect()
}

async fn wait_status(cap: &Capture, what: &str, pred: impl Fn(&ConnStatus) -> bool) -> ConnStatus {
    let mut st = cap.status();
    tokio::time::timeout(Duration::from_secs(5), st.wait_for(|s| pred(s)))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:?}", *cap.status().borrow()))
        .unwrap()
        .clone()
}

fn is_waiting(s: &ConnStatus) -> bool {
    matches!(
        s,
        ConnStatus::Disconnected {
            waiting_for: Some(_),
            ..
        }
    )
}

type Apps = watch::Sender<Arc<[DiscoveredApp]>>;

fn start_discovered(rt: &CaptureRuntime, cfg: CaptureConfig) -> (Capture, Apps) {
    let (tx, rx) = watch::channel::<Arc<[DiscoveredApp]>>(Arc::from(Vec::new()));
    (Capture::start(rt.handle(), cfg, Some(rx)), tx)
}

async fn logs_are(cap: &Capture, want: &[&str]) -> Arc<Snapshot> {
    let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    common::wait_for(
        cap,
        Duration::from_secs(5),
        &format!("logs {want:?}"),
        |s| texts(s) == want,
    )
    .await
}

#[test]
fn app_exit_keeps_data_and_waits_for_the_same_app() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let (cap, _apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&app.server, "hello");
        logs_are(&cap, &["hello"]).await;
        drop(app);
        let st = wait_status(&cap, "disconnected", |s| {
            matches!(s, ConnStatus::Disconnected { .. })
        })
        .await;
        match st {
            ConnStatus::Disconnected {
                app,
                reason,
                waiting_for,
            } => {
                assert_eq!(app.unwrap().name, "demo");
                assert_eq!(reason, "demo stopped.");
                assert_eq!(waiting_for, Some(key()));
            }
            other => panic!("{other:?}"),
        }
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the run to end", |s| {
            s.runs.first().is_some_and(|r| r.ended.is_some())
        })
        .await;
        assert_eq!(texts(&snap), ["hello"]);
    });
}

#[test]
fn same_key_new_vm_starts_run_two() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let one = fake(1, 100);
        let (cap, apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&one.server)));
        let first = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&one.server, "one");
        logs_are(&cap, &["one"]).await;
        drop(one);
        wait_status(&cap, "waiting", is_waiting).await;
        let two = fake(2, 200);
        apps.send(Arc::from(vec![disc(&two.server)])).unwrap();
        wait_status(&cap, "run 2", |s| {
            matches!(s, ConnStatus::Connected { run: RunId(2), .. })
        })
        .await;
        say(&two.server, "two");
        let snap = logs_are(&cap, &["one", "two"]).await;
        assert_eq!(snap.runs.len(), 2);
        let (r1, r2) = (&snap.runs[0], &snap.runs[1]);
        assert!(r1.ended.is_some() && r2.ended.is_none());
        assert!(r2.started >= r1.ended.unwrap());
        assert_eq!(snap.origin_host_us, first.origin_host_us);
        assert_eq!(snap.logs.row(0).id.run, RunId(1));
        assert_eq!(snap.logs.row(1).id.run, RunId(2));
        assert_ne!(r1.clock, r2.clock);
    });
}

#[test]
fn same_vm_reconnect_continues_the_run() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let (cap, apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&app.server, "a");
        logs_are(&cap, &["a"]).await;
        app.server.close_abruptly();
        wait_status(&cap, "waiting", is_waiting).await;
        apps.send(Arc::from(vec![disc(&app.server)])).unwrap();
        wait_status(&cap, "reconnected", |s| {
            matches!(s, ConnStatus::Connected { .. })
        })
        .await;
        say(&app.server, "b");
        let snap = logs_are(&cap, &["a", "b"]).await;
        assert_eq!(snap.runs.len(), 1);
        assert!(snap.runs[0].ended.is_none());
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Connected { run: RunId(1), .. }
        ));
    });
}

#[test]
fn a_request_that_finishes_while_disconnected_is_body_fetched_after_the_reconnect() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        app.profile.add(
            json!({"id": "7", "isolateId": "isolates/1", "method": "GET",
            "uri": "http://h:1/slow", "startTime": T + 5_000_000, "events": [],
            "request": {"headers": {}, "contentLength": 0}}),
        );
        let (cap, apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        common::wait_for(&cap, Duration::from_secs(5), "a pending request", |s| {
            s.net.len() == 1
        })
        .await;
        app.server.close_abruptly();
        wait_status(&cap, "waiting", is_waiting).await;
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the request to end", |s| {
            s.net.request(0).end.is_some()
        })
        .await;
        assert!(snap.net.request(0).error.is_some());
        // While the link was down the app finished the request.
        app.profile.set(done("7"));
        apps.send(Arc::from(vec![disc(&app.server)])).unwrap();
        wait_status(&cap, "reconnected", |s| {
            matches!(s, ConnStatus::Connected { .. })
        })
        .await;
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the body", |s| {
            matches!(s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        assert_eq!(snap.net.len(), 1);
        assert_eq!(app.count("ext.dart.io.getHttpProfileRequest"), 1);
    });
}

#[test]
fn stop_waiting_ignores_the_app_when_it_returns() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let one = fake(1, 100);
        let (cap, apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&one.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        drop(one);
        wait_status(&cap, "waiting", is_waiting).await;
        cap.stop_waiting();
        wait_status(&cap, "not waiting", |s| {
            matches!(
                s,
                ConnStatus::Disconnected {
                    waiting_for: None,
                    ..
                }
            )
        })
        .await;
        let two = fake(2, 200);
        apps.send(Arc::from(vec![disc(&two.server)])).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Disconnected {
                waiting_for: None,
                ..
            }
        ));
        assert_eq!(two.server.open_connections(), 0);
        assert!(two.server.received().is_empty());
    });
}

#[test]
fn url_target_retries_then_gives_up() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let app = fake(1, 100);
        let addr = app.server.ws_uri().socket_addrs(|| None).unwrap()[0];
        let cap = Capture::start(
            tokio::runtime::Handle::current(),
            CaptureConfig {
                reconnect_uri_retry: Duration::from_secs(1),
                reconnect_uri_give_up: Duration::from_secs(5),
                ..cfg()
            },
            None,
        );
        cap.connect(Target::Uri(
            VmServiceUri::parse(app.server.ws_uri().as_str()).unwrap(),
        ));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        drop(app);
        // Take the port over with a listener that counts attempts, then freeze time.
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = loop {
            match std::net::TcpListener::bind(addr) {
                Ok(l) => break l,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let h = hits.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                h.fetch_add(1, Ordering::SeqCst);
                drop(s);
            }
        });
        wait_status(&cap, "disconnected", |s| {
            matches!(s, ConnStatus::Disconnected { .. })
        })
        .await;
        tokio::time::pause();
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::thread::sleep(Duration::from_millis(30));
            tokio::task::yield_now().await;
        }
        let attempts = hits.load(Ordering::SeqCst);
        assert!((2..=6).contains(&attempts), "{attempts} attempts");
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::thread::sleep(Duration::from_millis(5));
            tokio::task::yield_now().await;
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            attempts,
            "kept retrying after giving up"
        );
        match &*cap.status().borrow() {
            ConnStatus::Disconnected {
                reason,
                waiting_for: None,
                ..
            } => assert_eq!(reason, "demo stopped."),
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn hot_restart_keeps_the_run() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let cap = Capture::start(rt.handle(), cfg(), None);
        cap.connect(Target::Uri(
            VmServiceUri::parse(app.server.ws_uri().as_str()).unwrap(),
        ));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        let note = |e: Value| app.server.notify("streamNotify", isolate_note(e));
        note(json!({"kind": "IsolateExit", "isolate": isolate_ref("isolates/1")}));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Connected { .. }
        ));
        note(json!({"kind": "IsolateStart", "isolate": isolate_ref("isolates/2")}));
        note(
            json!({"kind": "ServiceExtensionAdded", "isolate": isolate_ref("isolates/2"),
            "extensionRPC": ENABLE}),
        );
        app.server.notify(
            "streamNotify",
            json!({"streamId": "Service", "event": {"kind": "ServiceRegistered",
                "service": "flutterVersion", "method": "s1.flutterVersion"}}),
        );
        common::eventually(
            Duration::from_secs(5),
            "version asked of the new main isolate",
            || {
                app.server
                    .received()
                    .iter()
                    .any(|(m, p)| m == "s1.flutterVersion" && p["isolateId"] == "isolates/2")
            },
        )
        .await;
        say(&app.server, "after restart");
        let snap = logs_are(&cap, &["after restart"]).await;
        assert_eq!(snap.runs.len(), 1);
        assert!(snap.runs[0].ended.is_none());
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Connected { run: RunId(1), .. }
        ));
    });
}

#[test]
fn paused_recording_drops_events_and_resumes() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let cap = Capture::start(rt.handle(), cfg(), None);
        cap.connect(Target::Uri(
            VmServiceUri::parse(app.server.ws_uri().as_str()).unwrap(),
        ));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&app.server, "before");
        logs_are(&cap, &["before"]).await;
        cap.set_recording(false);
        wait_status(&cap, "paused", |s| {
            matches!(
                s,
                ConnStatus::Connected {
                    recording: false,
                    ..
                }
            )
        })
        .await;
        say(&app.server, "during");
        app.profile.add(done("paused-request"));
        let polls = app.count("ext.dart.io.getHttpProfile");
        common::eventually(Duration::from_secs(5), "polls while paused", || {
            app.count("ext.dart.io.getHttpProfile") >= polls + 3
        })
        .await;
        cap.set_recording(true);
        wait_status(&cap, "recording", |s| {
            matches!(
                s,
                ConnStatus::Connected {
                    recording: true,
                    ..
                }
            )
        })
        .await;
        say(&app.server, "after");
        app.profile.add(done("live-request"));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "live request", |s| {
            s.net.len() == 1 && texts(s) == ["before", "after"]
        })
        .await;
        assert_eq!(snap.net.request(0).url, "http://h:1/live-request");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(cap.snapshots().borrow().net.len(), 1);
    });
}

#[test]
fn new_session_starts_empty_and_keeps_the_connection() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let cap = Capture::start(rt.handle(), cfg(), None);
        cap.connect(Target::Uri(
            VmServiceUri::parse(app.server.ws_uri().as_str()).unwrap(),
        ));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&app.server, "old");
        app.profile.add(done("old-request"));
        let old = common::wait_for(&cap, Duration::from_secs(5), "old data", |s| {
            s.net.len() == 1 && s.logs.len() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cap.new_session();
        let fresh = common::wait_for(&cap, Duration::from_secs(5), "an empty session", |s| {
            s.origin_host_us > old.origin_host_us && s.runs.len() == 1
        })
        .await;
        assert_eq!((fresh.logs.len(), fresh.net.len()), (0, 0));
        assert_eq!(fresh.runs[0].id, RunId(1));
        assert!(fresh.runs[0].ended.is_none());
        assert_eq!(fresh.runs[0].app.name, old.runs[0].app.name);
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Connected { run: RunId(1), .. }
        ));
        say(&app.server, "new");
        app.profile.add(done("new-request"));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "new data", |s| {
            s.net.len() == 1 && s.logs.len() == 1
        })
        .await;
        assert_eq!(texts(&snap), ["new"]);
        assert_eq!(snap.net.request(0).url, "http://h:1/new-request");
        assert_eq!(snap.runs.len(), 1);
        assert_eq!(app.count("getVM"), 1);
        assert_eq!(app.server.open_connections(), 1);
    });
}

#[test]
fn disconnect_command() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let (cap, _apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        say(&app.server, "kept");
        logs_are(&cap, &["kept"]).await;
        cap.disconnect();
        let st = wait_status(&cap, "disconnected", |s| {
            matches!(s, ConnStatus::Disconnected { .. })
        })
        .await;
        match st {
            ConnStatus::Disconnected {
                app,
                reason,
                waiting_for,
            } => {
                assert!(app.is_some());
                assert_eq!(reason, "Disconnected.");
                assert_eq!(waiting_for, None);
            }
            other => panic!("{other:?}"),
        }
        let snap = common::wait_for(&cap, Duration::from_secs(5), "run ended", |s| {
            s.runs.first().is_some_and(|r| r.ended.is_some())
        })
        .await;
        assert_eq!(texts(&snap), ["kept"]);
        common::eventually(Duration::from_secs(5), "connection closed", || {
            app.server.open_connections() == 0
        })
        .await;
    });
}

#[test]
fn connect_to_nothing_reports_failed_with_message() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("ws://{}/x/ws", l.local_addr().unwrap())
        };
        let cap = Capture::start(rt.handle(), cfg(), None);
        cap.connect(Target::Uri(VmServiceUri::parse(&dead).unwrap()));
        match wait_status(&cap, "failed", |s| matches!(s, ConnStatus::Failed { .. })).await {
            ConnStatus::Failed { message, details } => {
                assert!(message.starts_with("Nothing is listening at"), "{message}");
                assert!(!details.is_empty());
            }
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn app_dies_during_the_connect_sequence() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let slot: Arc<OnceLock<Arc<ScriptedServer>>> = Arc::default();
        let s2 = slot.clone();
        let server = Arc::new(ScriptedServer::start(move |method, _| match method {
            "getVM" => Ok(json!({"type": "VM", "pid": 1, "operatingSystem": "macos",
                "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
            "getIsolate" => {
                // The app dies right after answering getVM.
                s2.get().unwrap().close_abruptly();
                Ok(json!({}))
            }
            _ => Ok(json!({"type": "Success"})),
        }));
        slot.set(server.clone()).ok();
        let cap = Capture::start(rt.handle(), cfg(), None);
        cap.connect(Target::Uri(
            VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
        ));
        let st = wait_status(&cap, "an end state", |s| {
            matches!(
                s,
                ConnStatus::Failed { .. } | ConnStatus::Disconnected { .. }
            )
        })
        .await;
        if let ConnStatus::Failed { message, .. } = &st {
            assert_eq!(message, "Connected, but the app didn't answer as expected.");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = cap.snapshots().borrow().clone();
        assert!(snap.runs.iter().all(|r| r.ended.is_some()));
    });
}

#[test]
fn pending_bodies_become_unavailable_when_the_app_stops() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake_with(1, 100, 3, true);
        let (cap, _apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        app.profile.add(done("slow"));
        common::wait_for(&cap, Duration::from_secs(5), "a pending body", |s| {
            s.net.len() == 1 && matches!(s.net.request(0).response_body, BodyState::Pending)
        })
        .await;
        drop(app);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "unavailable", |s| {
            s.net.len() == 1
                && matches!(
                    s.net.request(0).response_body,
                    BodyState::Unavailable { .. }
                )
        })
        .await;
        for kind in [BodyKind::Request, BodyKind::Response] {
            assert!(matches!(snap.net.request(0).body(kind),
                BodyState::Unavailable { reason } if reason == STOPPED));
        }
    });
}

#[test]
fn load_full_body_after_the_app_stopped_changes_nothing() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake_with(1, 100, 50, false);
        let (cap, _apps) = start_discovered(
            &rt,
            CaptureConfig {
                body_limits: BodyLimits {
                    per_body: 10,
                    total: 1 << 20,
                },
                ..cfg()
            },
        );
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        app.profile.add(done("x"));
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a body", |s| {
            s.net.len() == 1 && matches!(s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        let id = snap.net.request(0).id;
        drop(app);
        wait_status(&cap, "waiting", is_waiting).await;
        cap.load_full_body(id, BodyKind::Response);
        tokio::time::sleep(Duration::from_millis(150)).await;
        match &cap.snapshots().borrow().net.request(0).response_body {
            BodyState::Captured(b) => assert_eq!(b.bytes.len(), 10),
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn new_session_then_app_stops_is_detected() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let (cap, _apps) = start_discovered(&rt, cfg());
        cap.connect(Target::Discovered(disc(&app.server)));
        let first = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cap.new_session();
        common::wait_for(&cap, Duration::from_secs(5), "the new session", |s| {
            s.origin_host_us > first.origin_host_us && s.runs.len() == 1
        })
        .await;
        drop(app);
        wait_status(&cap, "waiting", is_waiting).await;
        common::wait_for(&cap, Duration::from_secs(5), "the run to end", |s| {
            s.runs.first().is_some_and(|r| r.ended.is_some())
        })
        .await;
    });
}

#[test]
fn stop_waiting_cancels_an_in_flight_url_retry() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let cap = Capture::start(
            rt.handle(),
            CaptureConfig {
                reconnect_uri_retry: Duration::from_millis(50),
                reconnect_uri_give_up: Duration::from_secs(60),
                ..cfg()
            },
            None,
        );
        cap.connect(Target::Uri(
            VmServiceUri::parse(app.server.ws_uri().as_str()).unwrap(),
        ));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        // The link drops; the retry reaches the app again, but the app never answers getVM.
        app.server.set_latency(Duration::from_secs(60));
        app.server.close_abruptly();
        common::eventually(Duration::from_secs(5), "a retry stuck in getVM", || {
            app.count("getVM") >= 2 && app.server.open_connections() == 1
        })
        .await;
        // The retry is quiet: the status still says Disconnected.
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Disconnected { .. }
        ));
        cap.stop_waiting();
        common::eventually(Duration::from_secs(2), "the retry to be cancelled", || {
            app.server.open_connections() == 0
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(app.count("getVM"), 2, "kept retrying");
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Disconnected {
                waiting_for: None,
                ..
            }
        ));
    });
}

#[test]
fn dropped_link_with_the_app_still_listed_reconnects() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let app = fake(1, 100);
        let (cap, apps) = start_discovered(
            &rt,
            CaptureConfig {
                reconnect_uri_retry: Duration::from_millis(50),
                ..cfg()
            },
        );
        apps.send(Arc::from(vec![disc(&app.server)])).unwrap();
        cap.connect(Target::Discovered(disc(&app.server)));
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        app.server.close_abruptly();
        // The list never changes; the capture looks at it again by itself.
        wait_status(&cap, "reconnected", |s| {
            matches!(s, ConnStatus::Connected { .. })
        })
        .await;
        assert!(matches!(
            *cap.status().borrow(),
            ConnStatus::Connected { run: RunId(1), .. }
        ));
    });
}

#[test]
fn connect_waits_at_most_five_seconds_by_default() {
    let c = CaptureConfig::default();
    assert_eq!(c.connect_deadline, Duration::from_secs(5));
    assert_eq!(c.calibration_timeout, Duration::from_millis(1500));
}

#[test]
fn a_server_that_never_answers_get_vm_fails_within_the_deadline() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = server();
        server.silence("getVM");
        // The real default is 5 s; the test shortens it.
        let cfg = CaptureConfig {
            connect_deadline: Duration::from_millis(300),
            ..cfg()
        };
        let cap = Capture::start(rt.handle(), cfg, None);
        let started = std::time::Instant::now();
        cap.connect(Target::Uri(
            VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
        ));
        match wait_status(&cap, "failed", |s| matches!(s, ConnStatus::Failed { .. })).await {
            ConnStatus::Failed { message, .. } => {
                assert!(message.contains("didn't answer"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    });
}

#[test]
fn a_calibration_call_that_never_answers_falls_back_and_connects() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = server();
        // The main isolate is paused: its extension call is never answered.
        server.silence("ext.dart.io.getHttpProfile");
        let cfg = CaptureConfig {
            calibration_timeout: Duration::from_millis(200),
            ..cfg()
        };
        let cap = Capture::start(rt.handle(), cfg, None);
        let started = std::time::Instant::now();
        cap.connect(Target::Uri(
            VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
        ));
        wait_status(&cap, "connected", |s| {
            matches!(s, ConnStatus::Connected { .. })
        })
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
    });
}
