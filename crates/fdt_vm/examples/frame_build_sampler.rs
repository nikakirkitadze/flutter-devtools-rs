//! Collects `Flutter.Frame` build times from a running app, for measuring the overhead of a
//! second VM-service client (the 2 % budget in M2).
//!
//! ```text
//! cargo run --release -p fdt_vm --example frame_build_sampler -- --uri ws://127.0.0.1:PORT/TOKEN/ws [--frames 600] [--timeout 120]
//! ```
//!
//! Subscribes to the `Extension` stream only, takes the `build` value (microseconds) of each
//! `Flutter.Frame` event that started after the subscription (the VM replays older ones), and prints the count, median, mean and p90 once `--frames` were seen.
//! Exits 1 if fewer arrive before the timeout.

use std::time::{Duration, Instant};

use fdt_vm::rpc::{ConnectOptions, connect};
use serde_json::json;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    let ix = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[ix]
}

#[tokio::main]
async fn main() {
    let Some(uri) = arg("--uri") else {
        eprintln!("usage: frame_build_sampler --uri <ws url> [--frames N] [--timeout SECONDS]");
        std::process::exit(2);
    };
    let uri = url::Url::parse(&uri).expect("--uri must be a URL");
    let want: usize = arg("--frames").and_then(|v| v.parse().ok()).unwrap_or(600);
    let timeout = Duration::from_secs(arg("--timeout").and_then(|v| v.parse().ok()).unwrap_or(120));

    let (client, mut notes) = connect(&uri, ConnectOptions::default())
        .await
        .expect("connect");
    // The VM replays the events it kept from before the subscription, so only frames that
    // started after this timeline time count.
    let since = client
        .call("getVMTimelineMicros", json!({}))
        .await
        .expect("getVMTimelineMicros")["timestamp"]
        .as_i64()
        .expect("timeline timestamp");
    client
        .call("streamListen", json!({ "streamId": "Extension" }))
        .await
        .expect("streamListen Extension");

    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut numbers: Vec<u64> = Vec::new();
    let mut builds: Vec<i64> = Vec::with_capacity(want);
    while builds.len() < want {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(note)) = tokio::time::timeout(left, notes.recv()).await else {
            break;
        };
        let event = &note.params["event"];
        if event["extensionKind"] == "Flutter.Frame"
            && event["extensionData"]["startTime"].as_i64().unwrap_or(0) >= since
            && let Some(build) = event["extensionData"]["build"].as_i64()
        {
            builds.push(build);
            numbers.push(event["extensionData"]["number"].as_u64().unwrap_or(0));
        }
    }
    if builds.len() < want {
        eprintln!("only {} of {want} frames arrived", builds.len());
        std::process::exit(1);
    }
    builds.sort_unstable();
    let mean = builds.iter().sum::<i64>() as f64 / builds.len() as f64;
    println!(
        "frames={} first_number={} last_number={} seconds={:.1} build_us median={} mean={mean:.1} p90={} min={} max={}",
        builds.len(),
        numbers[0],
        numbers[numbers.len() - 1],
        started.elapsed().as_secs_f64(),
        percentile(&builds, 0.5),
        percentile(&builds, 0.9),
        builds[0],
        builds[builds.len() - 1],
    );
}
