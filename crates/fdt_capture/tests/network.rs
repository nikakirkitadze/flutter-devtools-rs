mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, Target};
use fdt_store::net::BodyState;
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::{Value, json};

const T: i64 = 1_700_000_000_000_000;
const ENABLE: &str = "ext.dart.io.httpEnableTimelineLogging";

fn config() -> CaptureConfig {
    CaptureConfig {
        http_poll: Duration::from_millis(20),
        socket_poll: Duration::from_millis(50),
        ..CaptureConfig::default()
    }
}

fn isolate_ref(id: &str) -> Value {
    json!({"type": "@Isolate", "id": id, "name": "main", "isSystemIsolate": false})
}

/// An entry that is complete: `/x` with a 200 and `len` bytes.
fn done(id: &str, len: i64) -> Value {
    json!({"id": id, "isolateId": "isolates/1", "method": "GET", "uri": "http://h:1/x",
        "startTime": T + 5_000_000, "endTime": T + 5_000_100, "events": [],
        "request": {"headers": {}, "contentLength": 0},
        "response": {"startTime": T + 5_000_200, "endTime": T + 5_000_300, "statusCode": 200,
            "reasonPhrase": "OK", "headers": {"content-type": ["text/plain"]}, "contentLength": len}})
}

fn in_flight(id: &str) -> Value {
    json!({"id": id, "isolateId": "isolates/1", "method": "GET", "uri": "http://h:1/slow",
        "startTime": T + 5_000_000, "endTime": T + 5_000_100, "events": [],
        "request": {"headers": {}, "contentLength": 0}})
}

fn profile(requests: Vec<Value>) -> Result<Value, (i64, String)> {
    Ok(json!({"type": "HttpProfile", "timestamp": T, "requests": requests}))
}

fn body(n: usize) -> Value {
    json!(vec![65u8; n])
}

/// The methods every test server answers the same way. `None` means "not a base method".
fn base(method: &str, params: &Value) -> Option<Result<Value, (i64, String)>> {
    let iso = params["isolateId"].as_str().unwrap_or("isolates/1");
    Some(match method {
        "getVM" => Ok(json!({"type": "VM", "pid": 1, "operatingSystem": "macos",
            "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
        "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "main",
            "extensionRPCs": ["ext.dart.io.getHttpProfile", ENABLE],
            "rootLib": {"uri": "package:demo/main.dart"}})),
        "streamListen" => Ok(json!({"type": "Success"})),
        "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1000})),
        m if m == ENABLE => Ok(json!({"type": "Success", "enabled": true})),
        "ext.dart.io.socketProfilingEnabled" => Ok(json!({"type": "Success"})),
        _ => return None,
    })
}

fn names(server: &ScriptedServer) -> Vec<String> {
    server.received().into_iter().map(|(m, _)| m).collect()
}

fn count(server: &ScriptedServer, method: &str) -> usize {
    names(server).iter().filter(|m| *m == method).count()
}

fn start(server: &ScriptedServer, cfg: CaptureConfig, rt: &CaptureRuntime) -> Capture {
    let cap = Capture::start(rt.handle(), cfg, None);
    cap.connect(Target::Uri(
        VmServiceUri::parse(server.ws_uri().as_str()).unwrap(),
    ));
    cap
}

#[test]
fn hot_restart_reenables_profiling() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![]),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        // Profiling is enabled on connect, for HTTP and for sockets.
        common::eventually(Duration::from_secs(5), "enable on connect", || {
            server
                .received()
                .iter()
                .any(|(m, p)| m == ENABLE && p["isolateId"] == "isolates/1" && p["enabled"] == true)
        })
        .await;
        let note = |event: Value| {
            server.notify(
                "streamNotify",
                json!({"streamId": "Isolate", "event": event}),
            )
        };
        note(json!({"kind": "IsolateExit", "isolate": isolate_ref("isolates/1")}));
        note(json!({"kind": "IsolateStart", "isolate": isolate_ref("isolates/2")}));
        note(
            json!({"kind": "ServiceExtensionAdded", "isolate": isolate_ref("isolates/2"),
            "extensionRPC": ENABLE}),
        );
        common::eventually(Duration::from_secs(5), "enable for the new isolate", || {
            let r = server.received();
            r.iter()
                .any(|(m, p)| m == ENABLE && p["isolateId"] == "isolates/2" && p["enabled"] == true)
                && r.iter().any(|(m, p)| {
                    m == "ext.dart.io.socketProfilingEnabled"
                        && p["isolateId"] == "isolates/2"
                        && p["enabled"] == true
                })
        })
        .await;
        let polls = |iso: &str| {
            server
                .received()
                .iter()
                .filter(|(m, p)| m == "ext.dart.io.getHttpProfile" && p["isolateId"] == iso)
                .count()
        };
        common::eventually(
            Duration::from_secs(5),
            "polls move to the new isolate",
            || polls("isolates/2") >= 2,
        )
        .await;
        let old = polls("isolates/1");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            polls("isolates/1") <= old + 1,
            "the old isolate is still being polled"
        );
    });
}

#[test]
fn oversized_bodies_are_not_fetched() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", 50_000_000)]),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a request", |s| {
            s.net.len() == 1
        })
        .await;
        assert!(matches!(
            snap.net.request(0).response_body,
            BodyState::TooLarge { len: 50_000_000 }
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(count(&server, "ext.dart.io.getHttpProfileRequest"), 0);
    });
}

/// A completed request whose request body is `req_len` bytes and response is `resp_len`.
fn done_both(id: &str, req_len: i64, resp_len: i64) -> Value {
    let mut v = done(id, resp_len);
    v["request"]["contentLength"] = json!(req_len);
    v
}

#[test]
fn an_oversized_request_body_is_not_fetched() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done_both("7", 50_000_000, 10)]),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a request", |s| {
            s.net.len() == 1
        })
        .await;
        let r = snap.net.request(0);
        assert!(matches!(
            r.request_body,
            BodyState::TooLarge { len: 50_000_000 }
        ));
        // The small response is not fetched either, and can be loaded on request.
        assert!(matches!(r.response_body, BodyState::TooLarge { len: 10 }));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(count(&server, "ext.dart.io.getHttpProfileRequest"), 0);
    });
}

#[test]
fn an_unknown_length_body_over_the_cap_becomes_too_large() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                // The length is unknown (chunked), so the body is fetched, and is over the limit.
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", -1)]),
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "7",
                    "method": "GET", "uri": "http://h:1/x", "startTime": T,
                    "requestBody": body(3), "responseBody": body(1000)})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cfg = CaptureConfig {
            auto_fetch_limit: 100,
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a decided body", |s| {
            s.net.len() == 1 && matches!(s.net.request(0).response_body, BodyState::TooLarge { .. })
        })
        .await;
        let r = snap.net.request(0);
        assert!(matches!(r.response_body, BodyState::TooLarge { len: 1000 }));
        assert!(matches!(r.request_body, BodyState::Captured(_)));
    });
}

#[test]
fn duplicate_entries_from_inclusive_updated_since_do_not_duplicate() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", 3)]),
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "7", "method": "GET",
                    "uri": "http://h:1/x", "startTime": T + 5_000_000, "responseBody": body(3),
                    "requestBody": []})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, Duration::from_secs(5), "the body", |s| {
            s.net.len() == 1 && matches!(s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        common::eventually(Duration::from_secs(5), "several polls", || {
            count(&server, "ext.dart.io.getHttpProfile") >= 5
        })
        .await;
        let snap = cap.snapshots().borrow().clone();
        assert_eq!(snap.net.len(), 1);
        assert_eq!(count(&server, "ext.dart.io.getHttpProfileRequest"), 1);
        // The first poll after calibration asks for everything; the rest ask since the last reply.
        let polls: Vec<Value> = server
            .received()
            .into_iter()
            .filter(|(m, _)| m == "ext.dart.io.getHttpProfile")
            .map(|(_, p)| p)
            .collect();
        assert!(polls[1].get("updatedSince").is_none());
        assert_eq!(polls[2]["updatedSince"], T);
    });
}

#[test]
fn free_app_memory_clears_only_when_nothing_is_in_flight() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let log: Arc<Mutex<Vec<(String, Instant)>>> = Arc::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let (l, n) = (log.clone(), polls.clone());
        let server = ScriptedServer::start(move |m, p| {
            l.lock().unwrap().push((m.to_string(), Instant::now()));
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => {
                    // Poll 1 is the calibration call. 2..=5 see a request still in flight.
                    match n.fetch_add(1, Ordering::SeqCst) {
                        0 | 6.. => profile(vec![]),
                        1..=4 => profile(vec![in_flight("9")]),
                        _ => profile(vec![done("9", 3)]),
                    }
                }
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "9", "method": "GET",
                    "uri": "http://h:1/slow", "startTime": T, "responseBody": body(3)})),
                "ext.dart.io.clearHttpProfile" => Ok(json!({"type": "Success"})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cfg = CaptureConfig {
            free_app_http_memory: true,
            ..config()
        };
        // Every reply takes 150 ms, so the body fetch is in flight for a while.
        server.set_latency(Duration::from_millis(150));
        let cap = start(&server, cfg, &rt);
        common::eventually(Duration::from_secs(10), "a clear", || {
            names(&server).contains(&"ext.dart.io.clearHttpProfile".to_string())
        })
        .await;
        let log = log.lock().unwrap().clone();
        let at = |name: &str| log.iter().find(|(m, _)| m == name).map(|(_, t)| *t);
        let fetch = at("ext.dart.io.getHttpProfileRequest").expect("a body fetch");
        let clear = at("ext.dart.io.clearHttpProfile").unwrap();
        assert!(
            clear.duration_since(fetch) >= Duration::from_millis(120),
            "cleared while the body fetch was in flight"
        );
        // Nothing was cleared while the request was incomplete.
        let first_complete = log
            .iter()
            .position(|(m, _)| m == "ext.dart.io.getHttpProfileRequest")
            .unwrap();
        let clear_pos = log
            .iter()
            .position(|(m, _)| m == "ext.dart.io.clearHttpProfile")
            .unwrap();
        assert!(clear_pos > first_complete);
        // The next poll after a clear asks for everything again.
        common::eventually(Duration::from_secs(5), "a poll after the clear", || {
            let r = server.received();
            let c = r
                .iter()
                .position(|(m, _)| m == "ext.dart.io.clearHttpProfile")
                .unwrap();
            r[c..]
                .iter()
                .any(|(m, _)| m == "ext.dart.io.getHttpProfile")
        })
        .await;
        let r = server.received();
        let c = r
            .iter()
            .position(|(m, _)| m == "ext.dart.io.clearHttpProfile")
            .unwrap();
        let next = r[c..]
            .iter()
            .find(|(m, _)| m == "ext.dart.io.getHttpProfile")
            .unwrap();
        assert!(next.1.get("updatedSince").is_none());
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the body", |s| {
            matches!(s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        assert_eq!(snap.net.len(), 1);
    });
}

#[test]
fn app_dies_during_body_fetch() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let slot: Arc<OnceLock<Arc<ScriptedServer>>> = Arc::default();
        let s2 = slot.clone();
        let server = Arc::new(ScriptedServer::start(move |m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", 3)]),
                "ext.dart.io.getHttpProfileRequest" => {
                    // The answer never arrives in time: the app is about to die.
                    s2.get().unwrap().set_latency(Duration::from_secs(60));
                    Ok(json!({"id": "7", "method": "GET", "uri": "http://h:1/x",
                        "startTime": T, "responseBody": body(3)}))
                }
                _ => Err((-32601, "Method not found".into())),
            })
        }));
        slot.set(server.clone()).ok();
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, Duration::from_secs(5), "a pending body", |s| {
            s.net.len() == 1 && matches!(s.net.request(0).response_body, BodyState::Pending)
        })
        .await;
        common::eventually(Duration::from_secs(5), "the fetch to arrive", || {
            count(&server, "ext.dart.io.getHttpProfileRequest") == 1
        })
        .await;
        server.close_abruptly();
        let snap = common::wait_for(&cap, Duration::from_secs(5), "unavailable bodies", |s| {
            matches!(
                s.net.request(0).response_body,
                BodyState::Unavailable { .. }
            )
        })
        .await;
        assert!(matches!(
            snap.net.request(0).request_body,
            BodyState::Unavailable { .. }
        ));
    });
}

#[test]
fn sockets_are_polled_only_after_a_websocket_and_linked() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let ws_seen = Arc::new(AtomicBool::new(false));
        let w = ws_seen.clone();
        let server = ScriptedServer::start(move |m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" if w.load(Ordering::SeqCst) => profile(vec![
                    json!({"id": "3", "isolateId": "isolates/1", "method": "GET",
                        "uri": "http://127.0.0.1:9000/ws", "startTime": T + 5_000_000,
                        "endTime": T + 5_000_100, "events": [],
                        "request": {"headers": {"upgrade": ["websocket"]},
                            "connectionInfo": {"localPort": 1, "remoteAddress": "127.0.0.1",
                                "remotePort": 9000},
                            "contentLength": 0},
                        "response": {"startTime": T + 5_000_200, "statusCode": 101,
                            "reasonPhrase": "Switching Protocols", "headers": {},
                            "contentLength": 0, "error": "Socket has been detached"}}),
                ]),
                "ext.dart.io.getHttpProfile" => profile(vec![done("1", 0)]),
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "1", "method": "GET",
                    "uri": "http://h:1/x", "startTime": T, "responseBody": []})),
                "ext.dart.io.getSocketProfile" => Ok(json!({"type": "SocketProfile",
                    "sockets": [{"id": "s1", "address": "127.0.0.1", "port": 9000,
                        "socketType": "tcp", "startTime": 1000 + 5_100_000,
                        "readBytes": 10, "writeBytes": 20}]})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, Duration::from_secs(5), "a plain request", |s| {
            s.net.len() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(count(&server, "ext.dart.io.getSocketProfile"), 0);
        ws_seen.store(true, Ordering::SeqCst);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a linked socket", |s| {
            s.net.len() == 2 && s.net.socket_for(s.net.request(1).id).is_some()
        })
        .await;
        let r = snap.net.request(1);
        assert!(r.websocket && !r.is_error());
        assert_eq!(snap.net.socket_for(r.id).unwrap().read_bytes, 10);
        assert!(matches!(r.response_body, BodyState::NotCaptured));
        assert!(matches!(r.request_body, BodyState::NotCaptured));
    });
}

#[test]
fn load_full_body_keeps_what_per_body_cut_off() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", 50)]),
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "7", "method": "GET",
                    "uri": "http://h:1/x", "startTime": T, "responseBody": body(50)})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cfg = CaptureConfig {
            body_limits: fdt_store::net::BodyLimits {
                per_body: 10,
                total: 1 << 20,
            },
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a truncated body", |s| {
            s.net.len() == 1 && matches!(&s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        let r = snap.net.request(0);
        match &r.response_body {
            BodyState::Captured(b) => assert_eq!((b.bytes.len(), b.truncated()), (10, true)),
            other => panic!("{other:?}"),
        }
        cap.load_full_body(r.id, fdt_store::net::BodyKind::Response);
        common::wait_for(&cap, Duration::from_secs(5), "the full body", |s| {
            matches!(&s.net.request(0).response_body, BodyState::Captured(b) if b.bytes.len() == 50)
        })
        .await;
    });
}

#[test]
fn slow_body_fetch_times_out_as_unavailable() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let slot: Arc<OnceLock<Arc<ScriptedServer>>> = Arc::default();
        let s2 = slot.clone();
        let server = Arc::new(ScriptedServer::start(move |m, p| {
            base(m, p).unwrap_or_else(|| match m {
                // Unknown length, as for a chunked response.
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", -1)]),
                "ext.dart.io.getHttpProfileRequest" => {
                    s2.get().unwrap().set_latency(Duration::from_secs(60));
                    Ok(json!({"id": "7", "method": "GET", "uri": "http://h:1/x",
                        "startTime": T, "responseBody": body(3)}))
                }
                _ => Err((-32601, "Method not found".into())),
            })
        }));
        slot.set(server.clone()).ok();
        let cfg = CaptureConfig {
            body_fetch_timeout: Duration::from_millis(100),
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a timeout", |s| {
            s.net.len() == 1
                && matches!(
                    &s.net.request(0).response_body,
                    BodyState::Unavailable { .. }
                )
        })
        .await;
        assert!(matches!(&snap.net.request(0).response_body,
            BodyState::Unavailable { reason } if reason == "The body took too long to copy."));
    });
}

#[test]
fn failed_full_body_load_keeps_the_truncated_body() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let fail = Arc::new(AtomicBool::new(false));
        let f = fail.clone();
        let server = ScriptedServer::start(move |m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![done("7", 50)]),
                "ext.dart.io.getHttpProfileRequest" if f.load(Ordering::SeqCst) => {
                    Err((-32000, "gone".into()))
                }
                "ext.dart.io.getHttpProfileRequest" => Ok(json!({"id": "7", "method": "GET",
                    "uri": "http://h:1/x", "startTime": T, "responseBody": body(50)})),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cfg = CaptureConfig {
            body_limits: fdt_store::net::BodyLimits {
                per_body: 10,
                total: 1 << 20,
            },
            ..config()
        };
        let cap = start(&server, cfg, &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a truncated body", |s| {
            s.net.len() == 1 && matches!(&s.net.request(0).response_body, BodyState::Captured(_))
        })
        .await;
        let id = snap.net.request(0).id;
        fail.store(true, Ordering::SeqCst);
        // Two loads at once: the second is ignored while the first is in flight.
        cap.load_full_body(id, fdt_store::net::BodyKind::Response);
        cap.load_full_body(id, fdt_store::net::BodyKind::Response);
        common::eventually(Duration::from_secs(5), "the reload", || {
            count(&server, "ext.dart.io.getHttpProfileRequest") >= 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(count(&server, "ext.dart.io.getHttpProfileRequest"), 2);
        let snap = cap.snapshots().borrow().clone();
        match &snap.net.request(0).response_body {
            BodyState::Captured(b) => assert_eq!(b.bytes.len(), 10),
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn every_running_isolate_with_the_profile_is_polled_at_connect() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            let iso = p["isolateId"].as_str().unwrap_or("isolates/1").to_string();
            match m {
                "getVM" => Ok(json!({"type": "VM", "pid": 1, "operatingSystem": "macos",
                    "isolates": [
                        {"id": "isolates/1", "name": "main", "isSystemIsolate": false},
                        {"id": "isolates/2", "name": "worker", "isSystemIsolate": false},
                        {"id": "isolates/3", "name": "vm-service", "isSystemIsolate": true}]})),
                // Isolates 1 and 2 expose the profile; 3 is a system isolate.
                "getIsolate" => Ok(json!({"type": "Isolate", "id": iso, "name": "x",
                    "extensionRPCs": ["ext.dart.io.getHttpProfile", ENABLE],
                    "rootLib": {"uri": "package:demo/main.dart"}})),
                "ext.dart.io.getHttpProfile" => profile(vec![]),
                _ => base(m, p).unwrap_or_else(|| Err((-32601, "Method not found".into()))),
            }
        });
        let cap = start(&server, config(), &rt);
        common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        let polls = |iso: &'static str| {
            let server = &server;
            move || {
                server
                    .received()
                    .iter()
                    .filter(|(m, p)| {
                        m == "ext.dart.io.getHttpProfile"
                            && p["isolateId"] == iso
                            && p["updatedSince"].as_i64() != Some(9_000_000_000_000_000)
                    })
                    .count()
                    >= 2
            }
        };
        common::eventually(
            Duration::from_secs(5),
            "isolate 2 polled",
            polls("isolates/2"),
        )
        .await;
        common::eventually(
            Duration::from_secs(5),
            "isolate 1 polled",
            polls("isolates/1"),
        )
        .await;
        assert!(
            server.received().iter().any(|(m, p)| m == ENABLE
                && p["isolateId"] == "isolates/2"
                && p["enabled"] == true)
        );
        assert!(
            !server
                .received()
                .iter()
                .any(|(_, p)| p["isolateId"] == "isolates/3"),
            "system isolates are left alone"
        );
    });
}

const STOPPED: &str = "The app stopped before this request finished.";

#[test]
fn an_in_flight_request_ends_with_an_error_when_the_app_dies() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => profile(vec![in_flight("7")]),
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        let snap = common::wait_for(&cap, Duration::from_secs(5), "a pending request", |s| {
            s.net.len() == 1
        })
        .await;
        assert!(snap.net.request(0).end.is_none());
        server.close_abruptly();
        let snap = common::wait_for(&cap, Duration::from_secs(5), "the request to end", |s| {
            s.net.request(0).end.is_some()
        })
        .await;
        let r = snap.net.request(0);
        assert_eq!(r.error.as_deref(), Some(STOPPED));
        assert_eq!(r.status_class(), fdt_store::net::StatusClass::Failed);
        assert!(r.end.unwrap() >= r.start);
    });
}

#[test]
fn a_hot_restart_ends_only_the_old_isolates_in_flight_requests() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|m, p| {
            base(m, p).unwrap_or_else(|| match m {
                "ext.dart.io.getHttpProfile" => match p["isolateId"].as_str() {
                    Some("isolates/2") => profile(vec![in_flight("9")]),
                    _ => profile(vec![in_flight("7")]),
                },
                _ => Err((-32601, "Method not found".into())),
            })
        });
        let cap = start(&server, config(), &rt);
        common::wait_for(&cap, Duration::from_secs(5), "the first request", |s| {
            s.net.len() == 1
        })
        .await;
        let note = |event: Value| {
            server.notify(
                "streamNotify",
                json!({"streamId": "Isolate", "event": event}),
            )
        };
        note(json!({"kind": "IsolateExit", "isolate": isolate_ref("isolates/1")}));
        note(json!({"kind": "IsolateStart", "isolate": isolate_ref("isolates/2")}));
        note(
            json!({"kind": "ServiceExtensionAdded", "isolate": isolate_ref("isolates/2"),
            "extensionRPC": ENABLE}),
        );
        let snap = common::wait_for(&cap, Duration::from_secs(5), "both requests", |s| {
            s.net.len() == 2 && s.net.request(0).end.is_some()
        })
        .await;
        let (old, new) = (snap.net.request(0), snap.net.request(1));
        assert_eq!(old.vm_id, "7");
        assert_eq!(old.error.as_deref(), Some(STOPPED));
        assert_eq!(new.vm_id, "9");
        assert!(
            new.end.is_none() && new.error.is_none(),
            "the new isolate's request is untouched"
        );
    });
}
