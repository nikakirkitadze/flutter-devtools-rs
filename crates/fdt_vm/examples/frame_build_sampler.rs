//! Collects `Flutter.Frame` build times from a running app, for measuring the overhead of a
//! second VM-service client (the 2 % budget in M2).
//!
//! ```text
//! cargo run --release -p fdt_vm --example frame_build_sampler -- --uri ws://127.0.0.1:PORT/TOKEN/ws [--frames 600] [--timeout 120] [--long-ms 40] [--act NAME] [--trace on|off]
//! ```
//!
//! Subscribes to the `Extension` stream only, takes the `build` value (microseconds) of each
//! `Flutter.Frame` event that started after the subscription (the VM replays older ones), and prints the count, median, mean, p90 and p99 (nearest rank) once `--frames` were seen.
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
    // `--long-ms N`: also print the wall-clock time (epoch ms) of every frame whose build took
    // at least N ms, so long frames can be matched against when something else did its work.
    let long_us: Option<i64> = arg("--long-ms")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|ms| (ms * 1000.0) as i64);
    let timeout = Duration::from_secs(arg("--timeout").and_then(|v| v.parse().ok()).unwrap_or(120));

    let (client, mut notes) = connect(&uri, ConnectOptions::default())
        .await
        .expect("connect");
    // `--act NAME` (jank_lab only) runs `ext.jank_lab.run` first, e.g. `goto_jank`;
    // `--trace on|off` sets `ext.flutter.profileUserWidgetBuilds`, as fdt's "Trace widget builds"
    // switch does. Both settle for two seconds before frames count.
    if arg("--act").is_some() || arg("--trace").is_some() {
        let vm = client.call("getVM", json!({})).await.expect("getVM");
        let iso = vm["isolates"][0]["id"]
            .as_str()
            .expect("isolate")
            .to_string();
        if let Some(a) = arg("--act") {
            client
                .call("ext.jank_lab.run", json!({"isolateId": iso, "action": a}))
                .await
                .expect("ext.jank_lab.run");
        }
        if let Some(t) = arg("--trace") {
            client
                .call(
                    "ext.flutter.profileUserWidgetBuilds",
                    json!({"isolateId": iso, "enabled": t == "on"}),
                )
                .await
                .expect("profileUserWidgetBuilds");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
            if let Some(limit) = long_us
                && build >= limit
            {
                let at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                println!(
                    "long number={} build_us={build} at_epoch_ms={at_ms}",
                    event["extensionData"]["number"].as_u64().unwrap_or(0)
                );
            }
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
        "frames={} first_number={} last_number={} seconds={:.1} build_us median={} mean={mean:.1} p90={} p99={} min={} max={}",
        builds.len(),
        numbers[0],
        numbers[numbers.len() - 1],
        started.elapsed().as_secs_f64(),
        percentile(&builds, 0.5),
        percentile(&builds, 0.9),
        percentile(&builds, 0.99),
        builds[0],
        builds[builds.len() - 1],
    );
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentiles_are_nearest_rank_over_sorted_builds() {
        let builds: Vec<i64> = (1..=150).collect();
        assert_eq!(percentile(&builds, 0.5), 75);
        assert_eq!(percentile(&builds, 0.9), 135);
        // ceil(150 * 0.99) = 149, so p99 of 150 frames is the second-largest build.
        assert_eq!(percentile(&builds, 0.99), 149);
        assert_eq!(percentile(&builds, 1.0), 150);
        assert_eq!(percentile(&builds, 0.0), 1);
        assert_eq!(percentile(&[7], 0.99), 7);
    }
}
