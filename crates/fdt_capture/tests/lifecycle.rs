mod common;

use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::clock::host_now_us;
use fdt_vm::testing::ScriptedServer;
use fdt_vm::uri::VmServiceUri;
use serde_json::json;

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
