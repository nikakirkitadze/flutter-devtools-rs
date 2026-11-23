//! Live check of device-log discovery against a real booted iOS simulator.
//!
//! Ignored unless `FDT_DEVICE_IT=1`. Needs a booted simulator with `fixtures/jank_lab` installed
//! (bundle id `dev.fdt.jankLab`). The Flutter tool is not involved: the app is started with
//! `simctl launch`, and an empty temp directory stands in for the DTD registry. The app is
//! terminated at the end, also when an assertion fails.
//!
//! Prints, in ms after the launch: when the app logged its VM service line (`line_ms`), when
//! the watcher found it (`candidate_ms`), when discovery listed it (`listed_ms`), and how long
//! it took to leave after `simctl terminate` (`gone_ms`).

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Seconds since the Unix epoch of a `log --style ndjson` timestamp such as
/// `2026-11-23 10:08:11.436790+0400`.
fn parse_log_time(ts: &str) -> Option<f64> {
    let (date, rest) = ts.split_once(' ')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let split = rest.rfind(['+', '-'])?;
    let (time, zone) = rest.split_at(split);
    let mut t = time.split(':');
    let (h, min) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let sec: f64 = t.next()?.parse().ok()?;
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let (zh, zm) = (
        zone[1..3].parse::<i64>().ok()?,
        zone[3..5].parse::<i64>().ok()?,
    );
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let local = days * 86_400 + h * 3600 + min * 60;
    Some((local - sign * (zh * 3600 + zm * 60)) as f64 + sec)
}

/// When `pid` logged its VM service line, in seconds since the Unix epoch.
fn line_time(udid: &str, pid: u32) -> Option<f64> {
    let predicate =
        format!("processID == {pid} AND eventMessage CONTAINS \"Dart VM service is listening\"");
    let text = simctl(&[
        "spawn",
        udid,
        "log",
        "show",
        "--last",
        "5m",
        "--style",
        "ndjson",
        "--predicate",
        &predicate,
    ]);
    text.lines().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v.get("processID")?.as_u64()? == u64::from(pid))
            .then(|| parse_log_time(v.get("timestamp")?.as_str()?))
            .flatten()
    })
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
        let launched_wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let out = simctl(&["launch", &udid, BUNDLE]);
        assert!(out.contains(BUNDLE), "simctl launch failed: {out}");
        // `dev.fdt.jankLab: 48226`
        let pid: u32 = out
            .trim()
            .rsplit(' ')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("simctl launch prints the pid");
        let mut cands = cand_dbg.clone();
        let cand_seen = tokio::time::timeout(Duration::from_secs(10), async {
            while !cands.borrow().iter().any(|c| c.pid == pid) {
                cands.changed().await.unwrap();
            }
        })
        .await
        .is_ok();
        println!(
            "candidate_ms={} seen={cand_seen}",
            launched.elapsed().as_millis()
        );
        // The 3 s grace period starts when the candidate appears.
        let limit = Duration::from_secs(8).saturating_sub(launched.elapsed());
        if wait_for(&mut apps, limit, true).await.is_none() {
            panic!(
                "jank_lab not listed within 8 s of launch; candidates {:?}, status {:?}, apps {:?}",
                cand_dbg.borrow(),
                *status_dbg.borrow(),
                apps.borrow()
            );
        }
        let listed = launched.elapsed();
        println!("listed_ms={}", listed.as_millis());
        if let Some(line) = line_time(&udid, pid) {
            let line_ms = (line - launched_wall.as_secs_f64()) * 1000.0;
            println!(
                "line_ms={line_ms:.0} listed_after_line_ms={:.0}",
                listed.as_secs_f64() * 1000.0 - line_ms
            );
        }
        let terminated = Instant::now();
        simctl(&["terminate", &udid, BUNDLE]);
        wait_for(&mut apps, Duration::from_secs(7), false)
            .await
            .expect("jank_lab gone within 7 s");
        println!("gone_ms={}", terminated.elapsed().as_millis());
    });
}
