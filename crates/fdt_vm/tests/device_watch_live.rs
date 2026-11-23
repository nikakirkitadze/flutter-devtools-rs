//! Live check of device-log discovery against a real booted iOS simulator.
//!
//! Ignored unless `FDT_DEVICE_IT=1`. Needs a booted simulator with `fixtures/jank_lab` installed
//! (bundle id `dev.fdt.jankLab`). The Flutter tool is not involved: the app is started with
//! `simctl launch`, and an empty temp directory stands in for the DTD registry. The app is
//! terminated at the end, also when an assertion fails.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fdt_vm::devices::{DeviceWatch, DeviceWatchConfig, WatchStatus};
use fdt_vm::discovery::{AppSource, DiscoveredApp, Discovery, DiscoveryConfig};
use tokio::sync::watch;

const BUNDLE: &str = "dev.fdt.jankLab";

fn simctl(args: &[&str]) -> String {
    let out = Command::new("xcrun")
        .arg("simctl")
        .args(args)
        .output()
        .expect("run xcrun simctl");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Terminates the app when dropped, so a failed assertion leaves the simulator as it was.
struct Terminate(String);

impl Drop for Terminate {
    fn drop(&mut self) {
        let _ = Command::new("xcrun")
            .args(["simctl", "terminate", &self.0, BUNDLE])
            .output();
    }
}

fn booted_udid() -> String {
    let list = simctl(&["list", "devices", "booted"]);
    list.lines()
        .find(|l| l.contains("(Booted)"))
        .and_then(|l| {
            let open = l.rfind("(Booted)")?;
            let head = &l[..open];
            let start = head.rfind('(')? + 1;
            let end = head.rfind(')')?;
            Some(head[start..end].to_string())
        })
        .expect("a booted simulator")
}

async fn wait_for(
    rx: &mut watch::Receiver<Arc<[DiscoveredApp]>>,
    limit: Duration,
    want_listed: bool,
) -> Option<Duration> {
    let start = Instant::now();
    loop {
        let found = rx
            .borrow()
            .iter()
            .any(|a| a.source == AppSource::Device && a.name == "jank_lab");
        if found == want_listed {
            return Some(start.elapsed());
        }
        let left = limit.checked_sub(start.elapsed())?;
        if tokio::time::timeout(left, rx.changed()).await.is_err() {
            return None;
        }
    }
}

#[test]
#[ignore = "needs a booted simulator with jank_lab; set FDT_DEVICE_IT=1"]
fn simulator_app_opened_without_flutter_tool_is_listed() {
    assert_eq!(
        std::env::var("FDT_DEVICE_IT").as_deref(),
        Ok("1"),
        "set FDT_DEVICE_IT=1"
    );
    let udid = booted_udid();
    // Start from a known state: the app not running.
    let _ = Command::new("xcrun")
        .args(["simctl", "terminate", &udid, BUNDLE])
        .output();
    let _guard = Terminate(udid.clone());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let registry = tempfile::tempdir().unwrap();
    let (cand_tx, cand_rx) = watch::channel::<Arc<[_]>>(Arc::from(Vec::new()));
    let (cand_tx, status_tx) = (
        Arc::new(cand_tx),
        Arc::new(watch::channel(WatchStatus::default()).0),
    );
    let (cand_dbg, status_dbg) = (cand_rx.clone(), status_tx.subscribe());
    let _watch = DeviceWatch::spawn(rt.handle(), DeviceWatchConfig::detect(), cand_tx, status_tx);
    let (_discovery, mut apps) = Discovery::spawn(
        rt.handle(),
        DiscoveryConfig {
            registry_dir: Some(registry.path().to_path_buf()),
            candidates: Some(cand_rx),
            ..DiscoveryConfig::default()
        },
    );

    rt.block_on(async {
        // Let the watchers settle before the launch so the time measured is the live path.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let launched = Instant::now();
        let out = simctl(&["launch", &udid, BUNDLE]);
        assert!(out.contains(BUNDLE), "simctl launch failed: {out}");
        let mut cands = cand_dbg.clone();
        let cand_seen = tokio::time::timeout(Duration::from_secs(10), async {
            while cands.borrow().is_empty() {
                cands.changed().await.unwrap();
            }
        })
        .await
        .is_ok();
        println!(
            "candidate_ms={} seen={cand_seen}",
            launched.elapsed().as_millis()
        );
        // The 3 s grace period starts when the candidate appears (about 1.5 s after launch).
        let limit = Duration::from_secs(8).saturating_sub(launched.elapsed());
        if wait_for(&mut apps, limit, true).await.is_none() {
            panic!(
                "jank_lab not listed within 8 s of launch; candidates {:?}, status {:?}, apps {:?}",
                cand_dbg.borrow(),
                *status_dbg.borrow(),
                apps.borrow()
            );
        }
        println!("listed_ms={}", launched.elapsed().as_millis());
        let terminated = Instant::now();
        simctl(&["terminate", &udid, BUNDLE]);
        wait_for(&mut apps, Duration::from_secs(7), false)
            .await
            .expect("jank_lab gone within 7 s");
        println!("gone_ms={}", terminated.elapsed().as_millis());
    });
}
