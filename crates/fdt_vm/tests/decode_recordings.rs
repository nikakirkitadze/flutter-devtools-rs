use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use fdt_vm::dart_io::{HttpProfile, HttpProfileEntry, HttpProfileRequestFull, SocketProfile};
use fdt_vm::events::{StreamEvent, WriteStream, decode_notification};
use fdt_vm::rpc::{ConnectOptions, Notification, RpcError, connect};
use fdt_vm::service::{Mode, VmService, detect_mode};
use fdt_vm::testing::{ReplayServer, ScriptedServer};
use fdt_vm::types::{FlagList, FlutterVersion, InstanceRef, Isolate, Version, Vm};
use serde_json::{Value, json};

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/recordings")
        .join(name)
}

struct Frames {
    /// (request, result) for every answered call, in recorded order.
    results: Vec<(Value, Value)>,
    notes: Vec<Notification>,
}

fn load(name: &str) -> Frames {
    let text = std::fs::read_to_string(path(name)).unwrap();
    let mut sent: HashMap<String, Value> = HashMap::new();
    let mut f = Frames {
        results: vec![],
        notes: vec![],
    };
    for line in text.lines() {
        let rec: Value = serde_json::from_str(line).unwrap();
        let msg = rec["msg"].clone();
        match rec["dir"].as_str().unwrap() {
            "send" => {
                sent.insert(msg["id"].as_str().unwrap().to_string(), msg);
            }
            _ if msg["method"] == "streamNotify" => f.notes.push(Notification {
                method: "streamNotify".into(),
                params: msg["params"].clone(),
            }),
            _ => {
                let id = msg["id"].as_str().unwrap();
                let req = sent.get(id).expect("a response has a matching request");
                if let Some(r) = msg.get("result") {
                    f.results.push((req.clone(), r.clone()));
                }
            }
        }
    }
    f
}

/// Methods this task has no typed decoder for.
const SKIPPED: &[&str] = &[
    "streamListen",
    "ext.dart.io.getVersion",
    "ext.dart.io.socketProfilingEnabled",
    "ext.jank_lab.run",
];

/// Every recorded event kind maps to a typed variant, so no `Other` is expected.
const ALLOWED_OTHER: &[(&str, &str)] = &[];

struct Decoded {
    isolate: Option<Isolate>,
    entries: BTreeMap<String, HttpProfileEntry>,
    bodies: Vec<HttpProfileRequestFull>,
    version_method: Option<String>,
}

fn decode_results(f: &Frames) -> Decoded {
    let mut d = Decoded {
        isolate: None,
        entries: BTreeMap::new(),
        bodies: vec![],
        version_method: None,
    };
    for (req, res) in &f.results {
        let method = req["method"].as_str().unwrap();
        match method {
            "getVM" => {
                let vm: Vm = serde_json::from_value(res.clone()).unwrap();
                assert_eq!(vm.operating_system, "macos");
                assert!(vm.isolates.iter().any(|i| i.name == "main"));
                assert!(vm.pid.is_some() && vm.start_time.is_some());
            }
            "getVersion" => {
                let v: Version = serde_json::from_value(res.clone()).unwrap();
                assert_eq!(v.major, 4);
            }
            "getIsolate" => d.isolate = Some(serde_json::from_value(res.clone()).unwrap()),
            "getFlagList" => {
                let l: FlagList = serde_json::from_value(res.clone()).unwrap();
                assert!(!l.flags.is_empty());
            }
            "getVMTimelineMicros" => assert!(res["timestamp"].as_i64().is_some()),
            "getObject" => {
                let o: InstanceRef = serde_json::from_value(res.clone()).unwrap();
                assert_eq!(o.kind, "String");
                assert_eq!(o.class_name.as_deref(), Some("_OneByteString"));
            }
            "ext.dart.io.httpEnableTimelineLogging" => {
                assert_eq!(res["enabled"], true)
            }
            "ext.dart.io.getHttpProfile" => {
                let p: HttpProfile = serde_json::from_value(res.clone()).unwrap();
                assert!(p.timestamp > 0);
                for e in p.requests {
                    d.entries.insert(e.id.clone(), e);
                }
            }
            "ext.dart.io.getHttpProfileRequest" => {
                d.bodies
                    .push(HttpProfileRequestFull::from_value(res.clone()).unwrap());
            }
            "ext.dart.io.getSocketProfile" => {
                let p: SocketProfile = serde_json::from_value(res.clone()).unwrap();
                assert!(!p.sockets.is_empty());
            }
            m if m.ends_with(".flutterVersion") => {
                let v: FlutterVersion = serde_json::from_value(res.clone()).unwrap();
                assert_eq!(v.framework_version.as_deref(), Some("3.47.9"));
                d.version_method = Some(m.to_string());
            }
            m => assert!(SKIPPED.contains(&m), "no decoder or skip entry for {m}"),
        }
    }
    d
}

#[derive(Default)]
struct Counts {
    logging: usize,
    stdout: usize,
    stderr: usize,
    errors: usize,
    frames: usize,
    navigation: usize,
    events: Vec<StreamEvent>,
}

fn decode_events(f: &Frames) -> Counts {
    let mut c = Counts::default();
    for n in &f.notes {
        let ev = decode_notification(n).expect("every streamNotify decodes");
        match &ev {
            StreamEvent::Logging { .. } => c.logging += 1,
            StreamEvent::Write { stream, .. } => match stream {
                WriteStream::Stdout => c.stdout += 1,
                WriteStream::Stderr => c.stderr += 1,
            },
            StreamEvent::Extension { kind, .. } => match kind.as_str() {
                "Flutter.Error" => c.errors += 1,
                "Flutter.Frame" => c.frames += 1,
                "Flutter.Navigation" => c.navigation += 1,
                _ => {}
            },
            StreamEvent::Other { stream, kind } => {
                assert!(
                    ALLOWED_OTHER.contains(&(stream.as_str(), kind.as_str())),
                    "unexpected Other {stream}/{kind}"
                );
            }
            _ => {}
        }
        c.events.push(ev);
    }
    c
}

#[test]
fn debug_recording_decodes_completely() {
    let f = load("jank_lab-debug.jsonl");
    let d = decode_results(&f);
    let c = decode_events(&f);
    assert!(c.logging >= 5, "logging {}", c.logging);
    assert!(c.stdout >= 2000, "stdout {}", c.stdout);
    assert_eq!(c.stderr, 0);
    assert!(c.errors >= 1);
    assert!(c.frames >= 1);
    assert!(c.navigation >= 1);
    assert_eq!(detect_mode(d.isolate.as_ref().unwrap()), Mode::Debug);
    assert_eq!(d.version_method.as_deref(), Some("s1.flutterVersion"));
}

#[test]
fn profile_recording_decodes_completely() {
    let f = load("jank_lab-profile.jsonl");
    let d = decode_results(&f);
    let c = decode_events(&f);
    assert_eq!(c.errors, 0);
    assert!(c.logging >= 5);
    assert!(c.stdout >= 2000);
    assert_eq!(detect_mode(d.isolate.as_ref().unwrap()), Mode::Profile);
}

#[test]
fn write_events_decode_base64_bytes() {
    let f = load("jank_lab-debug.jsonl");
    let c = decode_events(&f);
    let first = c
        .events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Write { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(String::from_utf8(first).unwrap(), "flutter: burst line 0");
}

#[test]
fn service_registered_events_name_the_method() {
    let f = load("jank_lab-debug.jsonl");
    let c = decode_events(&f);
    assert!(c.events.iter().any(|e| matches!(e,
        StreamEvent::ServiceRegistered { service, method }
            if service == "flutterVersion" && method == "s1.flutterVersion")));
}

#[test]
fn the_long_log_message_is_flagged_truncated() {
    let f = load("jank_lab-debug.jsonl");
    let c = decode_events(&f);
    let long = c
        .events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Logging { message, .. } if message.length == Some(410) => {
                Some(message.clone())
            }
            _ => None,
        })
        .expect("the 30-line message");
    assert!(long.value_as_string_is_truncated);
    let short = c
        .events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Logging { message, .. } if message.length != Some(410) => {
                Some(message.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(!short.value_as_string_is_truncated);
}

#[test]
fn http_entries_decode_with_status_errors_and_bodies() {
    let f = load("jank_lab-debug.jsonl");
    let d = decode_results(&f);
    let by_uri = |needle: &str| {
        d.entries
            .values()
            .find(|e| e.uri.contains(needle))
            .unwrap_or_else(|| panic!("no entry for {needle}"))
    };
    let products = by_uri("/v2/products?page=3");
    assert_eq!(products.method, "GET");
    assert_eq!(products.response.as_ref().unwrap().status_code, Some(200));
    assert!(products.is_complete());
    assert!(!products.is_websocket_upgrade());

    let refused = by_uri("127.0.0.1:1/");
    let err = refused.request.as_ref().unwrap().error.as_deref().unwrap();
    assert!(err.contains("Connection refused"));
    assert!(refused.is_complete());

    let ws = by_uri("/ws");
    assert!(ws.is_websocket_upgrade());
    assert_eq!(ws.response.as_ref().unwrap().status_code, Some(101));

    assert_eq!(d.bodies.len(), 7);
    let big = d
        .bodies
        .iter()
        .find(|b| b.response_body.as_ref().is_some_and(|x| x.len() == 262_144))
        .expect("the 256 KiB body");
    assert!(big.entry.is_complete());
    let page = d
        .bodies
        .iter()
        .find(|b| b.entry.uri.contains("/v2/products"))
        .unwrap();
    assert!(
        page.response_body
            .as_ref()
            .unwrap()
            .starts_with(b"{\"page\":3")
    );
}

#[test]
fn in_flight_entries_are_incomplete() {
    let e: HttpProfileEntry = serde_json::from_value(json!({
        "id": "-5", "isolateId": "isolates/1", "method": "GET", "uri": "http://x/",
        "startTime": 1, "events": []
    }))
    .unwrap();
    assert!(!e.is_complete());
}

#[tokio::test]
async fn vm_service_reads_vm_and_full_string_from_replay() {
    let server = ReplayServer::start(&path("jank_lab-debug.jsonl"));
    let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let svc = VmService::new(client);
    let vm = svc.get_vm().await.unwrap();
    let main = vm.isolates.iter().find(|i| i.name == "main").unwrap();
    let iso = svc.get_isolate(&main.id).await.unwrap();
    assert_eq!(detect_mode(&iso), Mode::Debug);
    let s = svc
        .get_object_string(&main.id, "objects/15/0")
        .await
        .unwrap();
    assert_eq!(s.chars().count(), 410);
    assert!(s.starts_with("line 1 of 30\n"));
}

#[tokio::test]
async fn flutter_version_uses_the_recorded_method_name() {
    let server = ReplayServer::start(&path("jank_lab-debug.jsonl"));
    let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let svc = VmService::new(client);
    let v = svc
        .flutter_version("s1.flutterVersion", "isolates/1")
        .await
        .unwrap();
    assert_eq!(v.framework_version.as_deref(), Some("3.47.9"));
    assert_eq!(v.dart_sdk_version.as_deref(), Some("3.13.7"));
    let got = server.received();
    let (m, p) = got.last().unwrap();
    assert_eq!(m, "s1.flutterVersion");
    assert_eq!(p["isolateId"], "isolates/1");
}

#[tokio::test]
async fn stream_listen_accepts_already_subscribed() {
    let server = ScriptedServer::start(|m, _| match m {
        "streamListen" => Err((103, "Stream already subscribed".to_string())),
        _ => Err((-32601, "nope".to_string())),
    });
    let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let svc = VmService::new(client);
    svc.stream_listen("Logging").await.unwrap();
    let err = svc.get_vm().await.unwrap_err();
    assert!(matches!(err, RpcError::Server { code: -32601, .. }));
}

#[tokio::test]
async fn http_calls_pass_the_isolate_and_updated_since() {
    let server = ScriptedServer::start(|m, p| match m {
        "ext.dart.io.getHttpProfile" => {
            Ok(json!({"type":"HttpProfile","timestamp":42,"requests":[], "echo": p.clone()}))
        }
        "ext.dart.io.httpEnableTimelineLogging" => {
            Ok(json!({"type":"HttpTimelineLoggingState","enabled": p["enabled"]}))
        }
        _ => Err((-32601, "nope".to_string())),
    });
    let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
        .await
        .unwrap();
    let svc = VmService::new(client);
    assert!(svc.http_enable("isolates/1", true).await.unwrap());
    let p = svc
        .get_http_profile("isolates/1", Some(fdt_vm::dart_io::HTTP_CALIBRATION_SINCE))
        .await
        .unwrap();
    assert_eq!(p.timestamp, 42);
    let got = server.received();
    let (_, params) = got.last().unwrap();
    assert_eq!(params["isolateId"], "isolates/1");
    assert_eq!(params["updatedSince"], 9_000_000_000_000_000i64);
}

// ---- perf recording ----

/// Every `getVMTimeline` result as (JSON text of the result), in recorded order.
fn timeline_results(f: &Frames) -> Vec<String> {
    f.results
        .iter()
        .filter(|(req, _)| req["method"] == "getVMTimeline")
        .map(|(_, res)| res.to_string())
        .collect()
}

/// Each `TimelineEvents` notification's event array as JSON text.
fn timeline_blocks(f: &Frames) -> Vec<String> {
    f.notes
        .iter()
        .filter(|n| n.params["event"]["kind"] == "TimelineEvents")
        .map(|n| n.params["event"]["timelineEvents"].to_string())
        .collect()
}

#[test]
fn perf_recording_decodes() {
    use fdt_vm::perfetto::decode_cpu_samples;
    use fdt_vm::timeline::{Phase, decode_trace_events};

    let f = load("jank_lab-perf.jsonl");
    let polls = timeline_results(&f);
    let blocks = timeline_blocks(&f);
    assert!(polls.len() >= 4, "expected at least 4 polls");
    assert!(!blocks.is_empty(), "expected TimelineEvents blocks");

    let mut events = Vec::new();
    for text in polls.iter().chain(blocks.iter()) {
        let (e, skipped) = decode_trace_events(text);
        assert_eq!(skipped, 0, "events skipped while decoding");
        events.extend(e);
    }

    // Frames in the timeline match the Flutter.Frame events.
    let frames: std::collections::HashSet<u64> = f
        .notes
        .iter()
        .filter(|n| n.params["event"]["extensionKind"] == "Flutter.Frame")
        .filter_map(|n| n.params["event"]["extensionData"]["number"].as_u64())
        .collect();
    assert!(!frames.is_empty(), "no Flutter.Frame events recorded");
    assert!(
        events.iter().any(|e| &*e.name == "Animator::BeginFrame"
            && e.frame_number.is_some_and(|n| frames.contains(&n))),
        "no Animator::BeginFrame matches a recorded frame number"
    );

    // Thread names come from metadata events.
    assert!(
        events
            .iter()
            .any(|e| e.ph == Phase::Metadata && e.thread_name.is_some()),
        "no thread names"
    );

    // Widget tracing shows up in the pushed blocks.
    let pushed: Vec<_> = blocks
        .iter()
        .flat_map(|b| decode_trace_events(b).0)
        .collect();
    assert!(
        pushed.iter().any(|e| &*e.name == "ProductTile"),
        "no ProductTile slice in the TimelineEvents blocks"
    );

    // CPU samples.
    let cpu = f
        .results
        .iter()
        .find(|(req, _)| req["method"] == "getPerfettoCpuSamples")
        .expect("a getPerfettoCpuSamples call")
        .1
        .clone();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cpu["samples"].as_str().unwrap())
        .unwrap();
    let profile = decode_cpu_samples(&bytes).unwrap();
    assert!(
        profile.samples.len() >= 100,
        "{} samples",
        profile.samples.len()
    );
    assert_eq!(
        cpu["sampleCount"].as_u64(),
        Some(profile.samples.len() as u64)
    );
    let heavy = profile
        .functions
        .iter()
        .position(|f| f.name == "_HeavyBuildPanelState.build");
    let heavy = heavy.expect("_HeavyBuildPanelState.build among the functions") as u32;
    assert!(
        profile
            .samples
            .iter()
            .any(|s| profile.stacks[s.stack as usize].contains(&heavy)),
        "no sample has _HeavyBuildPanelState.build on its stack"
    );
}

#[test]
fn perf_recording_is_small() {
    let len = std::fs::metadata(path("jank_lab-perf.jsonl"))
        .unwrap()
        .len();
    assert!(len <= 12 * 1024 * 1024, "{len} bytes");
}
