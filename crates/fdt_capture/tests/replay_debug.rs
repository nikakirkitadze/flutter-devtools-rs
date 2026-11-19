mod common;

use std::path::Path;
use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::logs::{LogLevel, LogSource};
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
