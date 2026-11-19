mod common;

use std::path::Path;
use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::logs::{LogLevel, LogSource};
use fdt_store::net::{BodyLimits, BodyState, StatusClass};
use fdt_store::run::Mode;
use fdt_vm::testing::ReplayServer;
use fdt_vm::uri::VmServiceUri;
use regex::Regex;

#[test]
fn replay_of_the_debug_recording_fills_the_session() {
    let rt = CaptureRuntime::new().unwrap();
    let h = rt.handle();
    h.block_on(async {
        let server = ReplayServer::start(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/recordings/jank_lab-debug.jsonl"),
        );
        let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
        let cap = Capture::start(rt.handle(), CaptureConfig::default(), None);
        assert_eq!(*cap.status().borrow(), ConnStatus::Idle);
        cap.connect(Target::Uri(uri));
        let snap = common::settle(&cap, Duration::from_millis(300), Duration::from_secs(5)).await;

        let logs = &snap.logs;
        let rows: Vec<_> = (0..logs.len()).map(|i| logs.row(i)).collect();
        // The recording has jank_lab.net at 500 and 900 and jank_lab.ui at 800 and 1000 (plus a
        // level-0 record, which is Info).
        for (logger, level) in [
            ("jank_lab.net", LogLevel::Debug),
            ("jank_lab.net", LogLevel::Warning),
            ("jank_lab.ui", LogLevel::Info),
            ("jank_lab.ui", LogLevel::Error),
        ] {
            assert!(
                rows.iter()
                    .any(|r| r.source == LogSource::Log && r.logger == logger && r.level == level),
                "missing {logger} {level:?}"
            );
        }
        let burst = Regex::new(r"^burst line \d+$").unwrap();
        // The replay server sends each stream's events as soon as that stream is listened to,
        // so records of other sources may land between burst lines. Among Stdout records the
        // burst is contiguous and in order.
        let stdout: Vec<&str> = rows
            .iter()
            .filter(|r| r.source == LogSource::Stdout)
            .map(|r| r.text)
            .collect();
        let idx: Vec<usize> = stdout
            .iter()
            .enumerate()
            .filter(|(_, t)| burst.is_match(t))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(idx.len(), 1000);
        let want: Vec<String> = (0..1000).map(|i| format!("burst line {i}")).collect();
        assert_eq!(&stdout[idx[0]..=idx[999]], &want[..]);
        assert!(
            rows.iter()
                .any(|r| r.line_count == 30 && r.text.ends_with("line 30 of 30"))
        );
        let errs: Vec<_> = rows
            .iter()
            .filter(|r| r.source == LogSource::FlutterError)
            .collect();
        assert!(
            errs.iter()
                .any(|r| r.error.and_then(|e| e.widget.as_deref()) == Some("_Thrower"))
        );
        assert!(
            rows.iter()
                .any(|r| r.source == LogSource::Navigation && r.text == "Navigated to /logs")
        );
        assert!(!snap.frames.is_empty());
        let run = snap.runs.first().expect("a run");
        assert_eq!(run.app.mode, Mode::Debug);
        assert_eq!(run.app.name, "jank_lab");
        assert!(run.app.flutter_version.is_some());
        assert!(matches!(
            &*cap.status().borrow(),
            ConnStatus::Connected {
                recording: true,
                ..
            }
        ));
    });
}

fn recording() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recordings/jank_lab-debug.jsonl")
}

/// Connects to the recording with fast polling and waits until its slow request has finished and
/// every body has settled.
async fn replay_network(rt: &CaptureRuntime, cfg: CaptureConfig) -> std::sync::Arc<Snapshot> {
    let server = ReplayServer::start(&recording());
    let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
    let cap = Capture::start(rt.handle(), cfg, None);
    cap.connect(Target::Uri(uri));
    let snap = common::wait_for(&cap, Duration::from_secs(10), "all requests", |s| {
        s.net.len() == 7
            && (0..7).all(|i| {
                let r = s.net.request(i);
                r.end.is_some()
                    && !matches!(r.response_body, BodyState::Pending)
                    && !matches!(r.request_body, BodyState::Pending)
            })
    })
    .await;
    drop(cap);
    drop(server);
    snap
}

fn fast() -> CaptureConfig {
    CaptureConfig {
        http_poll: Duration::from_millis(5),
        socket_poll: Duration::from_millis(5),
        ..CaptureConfig::default()
    }
}

fn find<'a>(s: &'a Snapshot, tail: &str) -> &'a fdt_store::net::HttpRecord {
    (0..s.net.len())
        .map(|i| &**s.net.request(i))
        .find(|r| r.url.ends_with(tail))
        .unwrap_or_else(|| panic!("no request ending in {tail}"))
}

fn bytes(b: &BodyState) -> &[u8] {
    match b {
        BodyState::Captured(b) => &b.bytes,
        other => panic!("not captured: {other:?}"),
    }
}

#[test]
fn replay_records_requests_and_bodies() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let snap = replay_network(&rt, fast()).await;
        let products = find(&snap, "/v2/products?page=3");
        assert_eq!(products.status, Some(200));
        assert!(matches!(products.response_body, BodyState::Captured(_)));
        assert_eq!(products.size(), Some(1039));
        assert!(
            serde_json::from_slice::<serde_json::Value>(bytes(&products.response_body)).is_ok()
        );
        let orders = find(&snap, "/v2/orders");
        assert_eq!(orders.status, Some(500));
        assert_eq!(bytes(&orders.request_body), br#"{"cart":1}"#);
        let refused = find(&snap, "http://127.0.0.1:1/");
        assert_eq!(refused.status_class(), StatusClass::Failed);
        assert!(refused.is_error());
        let ws = find(&snap, "/ws");
        assert!(ws.websocket && !ws.is_error());
        assert!(matches!(ws.request_body, BodyState::NotCaptured));
        assert!(matches!(ws.response_body, BodyState::NotCaptured));
        let blob = find(&snap, "/files/blob.bin");
        match &blob.response_body {
            BodyState::Captured(b) => {
                assert_eq!(b.bytes.len(), 262_144);
                assert!(!b.truncated());
            }
            other => panic!("{other:?}"),
        }
        let slow = find(&snap, "/slow");
        assert_eq!(slow.status, Some(200));
        assert!(slow.end.is_some());
    });
}

#[test]
fn replay_truncates_bodies_over_the_limit() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let cfg = CaptureConfig {
            body_limits: BodyLimits {
                per_body: 100 << 10,
                ..BodyLimits::default()
            },
            ..fast()
        };
        let snap = replay_network(&rt, cfg).await;
        match &find(&snap, "/files/blob.bin").response_body {
            BodyState::Captured(b) => {
                assert_eq!(b.bytes.len(), 102_400);
                assert_eq!(b.full_len, 262_144);
            }
            other => panic!("{other:?}"),
        }
    });
}
