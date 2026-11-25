//! Attaches the real capture to a running app and holds the connection, with no window.
//!
//! ```text
//! cargo run --release -p fdt_capture --example attach_hold -- --uri ws://127.0.0.1:PORT/TOKEN/ws [--seconds 60] [--cpu-poll 5]
//! ```
//!
//! `--cpu-poll` sets how often CPU samples are fetched (seconds; default: the capture's own).
//!
//! Used to measure what a connected `fdt` costs the app when no display is available: the
//! capture is the only part of `fdt` that talks to the app. Prints `connected` once attached and
//! `done` (with the stored log and frame counts) when the time is up.

use std::time::Duration;

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_vm::uri::VmServiceUri;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let Some(uri) = arg("--uri") else {
        eprintln!("usage: attach_hold --uri <vm service url> [--seconds N]");
        std::process::exit(2);
    };
    let uri = VmServiceUri::parse(&uri).unwrap_or_else(|e| {
        eprintln!("attach_hold: {}", e.message());
        std::process::exit(2);
    });
    let seconds: u64 = arg("--seconds").and_then(|v| v.parse().ok()).unwrap_or(60);
    let rt = CaptureRuntime::new().expect("runtime");
    rt.handle().block_on(async {
        let mut cfg = CaptureConfig::default();
        if let Some(secs) = arg("--cpu-poll").and_then(|v| v.parse::<f64>().ok()) {
            cfg.cpu_poll = Duration::from_secs_f64(secs);
        }
        let cap = Capture::start(rt.handle(), cfg, None);
        cap.connect(Target::Uri(uri));
        let mut status = cap.status();
        tokio::time::timeout(
            Duration::from_secs(20),
            status.wait_for(|s| matches!(s, ConnStatus::Connected { .. })),
        )
        .await
        .expect("never connected")
        .expect("status closed");
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        println!("connected at_epoch_ms={at_ms}");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let snap = cap.snapshots().borrow().clone();
        println!(
            "done logs={} frames={} cpu_samples={}",
            snap.logs.len(),
            snap.frames.len(),
            snap.cpu.samples().len()
        );
    });
}
