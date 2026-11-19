//! Drives a running `fixtures/jank_lab` app and records the VM service traffic as a redacted
//! JSONL transcript.
//!
//! ```text
//! cargo run -p fdt_vm --example record_jank_lab -- --uri ws://127.0.0.1:PORT/TOKEN/ws --out file.jsonl
//! ```
//!
//! Everything written goes through `Tap`, so tokens and home-directory paths are redacted.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fdt_vm::rpc::{ConnectOptions, RpcClient, RpcError, connect};
use fdt_vm::tap::Tap;
use serde_json::{Value, json};
use tokio::time::sleep;

const STREAMS: [&str; 7] = [
    "Logging",
    "Stdout",
    "Stderr",
    "Extension",
    "Isolate",
    "Service",
    "Debug",
];

#[derive(Default)]
struct Seen {
    flutter_version_method: Option<String>,
    truncated_ids: BTreeSet<String>,
}

fn arg(name: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    match args.iter().position(|a| a == name) {
        Some(i) if i + 1 < args.len() => args[i + 1].clone(),
        _ => {
            eprintln!("usage: record_jank_lab --uri <ws url> --out <file.jsonl>");
            std::process::exit(2);
        }
    }
}

async fn call(client: &RpcClient, method: &str, params: Value) -> Option<Value> {
    match client.call(method, params).await {
        Ok(v) => Some(v),
        // Duplicate stream subscriptions (no DDS) are fine.
        Err(RpcError::Server { code: 103, .. }) => None,
        Err(e) => {
            eprintln!("{method}: {e}");
            None
        }
    }
}

#[tokio::main]
async fn main() {
    let uri = url::Url::parse(&arg("--uri")).expect("--uri must be a URL");
    let out = arg("--out");
    let tap = Tap::create(&out).expect("create output file");
    let opts = ConnectOptions {
        tap: Some(tap),
        ..ConnectOptions::default()
    };
    let (client, mut notes) = connect(&uri, opts).await.expect("connect");

    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s = seen.clone();
    let reader = tokio::spawn(async move {
        while let Some(n) = notes.recv().await {
            let event = &n.params["event"];
            let mut seen = s.lock().unwrap();
            match event["kind"].as_str() {
                Some("ServiceRegistered") => {
                    if let Some(m) = event["method"].as_str()
                        && m.ends_with("flutterVersion")
                    {
                        seen.flutter_version_method = Some(m.to_string());
                    }
                }
                Some("Logging") => {
                    let msg = &event["logRecord"]["message"];
                    if msg["valueAsStringIsTruncated"] == json!(true)
                        && let Some(id) = msg["id"].as_str()
                    {
                        seen.truncated_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    });

    // 1. Identity.
    call(&client, "getVersion", json!({})).await;
    let vm = call(&client, "getVM", json!({})).await.expect("getVM");
    let main_id = vm["isolates"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["name"] == "main"))
        .and_then(|i| i["id"].as_str())
        .expect("no main isolate")
        .to_string();
    let iso = json!({"isolateId": main_id});
    let isolate = call(&client, "getIsolate", iso.clone())
        .await
        .expect("getIsolate");
    let is_debug = isolate["extensionRPCs"]
        .as_array()
        .is_some_and(|a| a.iter().any(|e| e == "ext.flutter.reassemble"));
    call(&client, "getFlagList", json!({})).await;

    // 2. Streams.
    for stream in STREAMS {
        call(&client, "streamListen", json!({"streamId": stream})).await;
    }

    // 3. Calibration and profiling switches.
    call(&client, "getVMTimelineMicros", json!({})).await;
    call(&client, "ext.dart.io.getVersion", iso.clone()).await;
    call(
        &client,
        "ext.dart.io.httpEnableTimelineLogging",
        json!({"isolateId": main_id, "enabled": true}),
    )
    .await;
    call(
        &client,
        "ext.dart.io.socketProfilingEnabled",
        json!({"isolateId": main_id, "enabled": true}),
    )
    .await;
    call(
        &client,
        "ext.dart.io.getHttpProfile",
        json!({"isolateId": main_id, "updatedSince": 9_000_000_000_000_000u64}),
    )
    .await;

    // 4. Flutter version, from the service the Service stream announces.
    let deadline = Instant::now() + Duration::from_secs(2);
    let method = loop {
        let m = seen.lock().unwrap().flutter_version_method.clone();
        if m.is_some() || Instant::now() >= deadline {
            break m;
        }
        sleep(Duration::from_millis(50)).await;
    };
    if let Some(m) = method {
        call(&client, &m, iso.clone()).await;
    }

    // 5. Actions.
    let run = |action: &'static str| {
        let client = client.clone();
        let main_id = main_id.clone();
        async move {
            call(
                &client,
                "ext.jank_lab.run",
                json!({"isolateId": main_id, "action": action}),
            )
            .await;
            sleep(Duration::from_millis(300)).await;
        }
    };
    for a in [
        "goto_logs",
        "emit_levels",
        "print_burst",
        "multi_line",
        "report_error",
    ] {
        run(a).await;
    }
    if is_debug {
        run("throw_in_build").await;
    }
    for a in [
        "pop",
        "goto_network",
        "load_products",
        "place_order",
        "refused",
        "download",
        "hero_image",
        "open_socket",
    ] {
        run(a).await;
    }
    sleep(Duration::from_secs(5)).await;
    run("close_socket").await;
    run("slow").await;

    // 6. Poll the HTTP profile, then fetch each completed request once.
    let mut since: u64 = 0;
    let mut fetched: BTreeSet<String> = BTreeSet::new();
    let end = Instant::now() + Duration::from_secs(12);
    while Instant::now() < end {
        let profile = call(
            &client,
            "ext.dart.io.getHttpProfile",
            json!({"isolateId": main_id, "updatedSince": since}),
        )
        .await;
        if let Some(p) = profile {
            if let Some(ts) = p["timestamp"].as_u64() {
                since = ts.saturating_sub(1);
            }
            for r in p["requests"].as_array().into_iter().flatten() {
                let done = r["response"]["endTime"].is_number()
                    || r["request"]["error"].is_string()
                    || r["response"]["error"].is_string();
                let Some(id) = r["id"].as_str() else { continue };
                if done && fetched.insert(id.to_string()) {
                    call(
                        &client,
                        "ext.dart.io.getHttpProfileRequest",
                        json!({"isolateId": main_id, "id": id}),
                    )
                    .await;
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    // 7. Sockets.
    call(&client, "ext.dart.io.getSocketProfile", iso.clone()).await;

    // 8. Full text of every truncated log message.
    let ids: Vec<String> = seen.lock().unwrap().truncated_ids.iter().cloned().collect();
    for id in ids {
        call(
            &client,
            "getObject",
            json!({"isolateId": main_id, "objectId": id}),
        )
        .await;
    }

    // 9. Close.
    sleep(Duration::from_millis(300)).await;
    client.close();
    drop(client);
    reader.abort();
    eprintln!(
        "recorded {} ({})",
        out,
        if is_debug { "debug" } else { "profile" }
    );
}
