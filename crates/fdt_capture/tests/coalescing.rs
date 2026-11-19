mod common;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, Target};
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::json;

fn write(n: Option<usize>) -> serde_json::Value {
    let text = match n {
        Some(n) => format!("flutter: line {n}"),
        None => "\n".to_string(),
    };
    json!({"streamId": "Stdout", "event": {"kind": "WriteEvent", "timestamp": 1, "bytes": STANDARD.encode(text)}})
}

#[test]
fn a_burst_of_writes_publishes_few_snapshots() {
    let rt = CaptureRuntime::new().unwrap();
    rt.handle().block_on(async {
        let server = ScriptedServer::start(|method, _| match method {
            "getVM" => Ok(json!({"type": "VM", "pid": 1, "operatingSystem": "macos",
                "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
            "getIsolate" => Ok(json!({"type": "Isolate", "id": "isolates/1", "name": "main",
                "extensionRPCs": ["ext.flutter.reassemble"],
                "rootLib": {"uri": "package:demo/main.dart"}})),
            "streamListen" => Ok(json!({"type": "Success"})),
            "getVMTimelineMicros" => Ok(json!({"type": "Timestamp", "timestamp": 1000})),
            "ext.dart.io.getHttpProfile" => {
                Ok(json!({"type": "HttpProfile", "timestamp": 1_700_000_000_000_000_i64, "requests": []}))
            }
            _ => Err((-32601, "Method not found".into())),
        });
        let uri = VmServiceUri::parse(server.ws_uri().as_str()).unwrap();
        let cap = Capture::start(rt.handle(), CaptureConfig::default(), None);
        cap.connect(Target::Uri(uri));
        let before = common::settle(&cap, Duration::from_millis(100), Duration::from_secs(5)).await;
        let mut snaps = cap.snapshots();
        for i in 1..=1000 {
            server.notify("streamNotify", write(Some(i)));
            server.notify("streamNotify", write(None));
        }
        let start = tokio::time::Instant::now();
        let mut last = before.generation;
        while start.elapsed() < Duration::from_millis(1500) {
            if tokio::time::timeout(Duration::from_millis(300), snaps.changed())
                .await
                .is_err()
                && snaps.borrow().logs.len() >= 1000
            {
                break;
            }
            last = snaps.borrow().generation;
        }
        let end = snaps.borrow().clone();
        assert_eq!(end.logs.len(), 1000);
        assert_eq!(end.logs.row(999).text, "line 1000");
        let published = last - before.generation;
        println!("generations during the burst: {published}");
        assert!(published <= 25, "{published} generations");
    });
}
