//! Watching booted iOS simulators: the `device.plist` files say which are up, and a `log stream`
//! per simulator (plus a one-off `log show` backfill) reports the VM service line.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::follow::{Backoff, Follower, follow, run_once};
use super::marker::simulator_hit;
use super::simulators::SimulatorScan;
use super::{Candidate, Platform, PlatformState, WatchTiming};
use crate::discovery::pid_alive;

const BACKFILL_TIMEOUT: Duration = Duration::from_secs(20);
const PREDICATE: &str = "eventMessage CONTAINS \"Dart VM service is listening\"";

/// Handle to the simulator watcher. Dropping it stops everything and empties its set.
pub(crate) struct SimulatorWatch {
    task: JoinHandle<()>,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
}

impl SimulatorWatch {
    pub(crate) fn spawn(
        handle: &Handle,
        xcrun: PathBuf,
        devices_dir: PathBuf,
        cfg: WatchTiming,
        out: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<PlatformState>>,
    ) -> SimulatorWatch {
        let task = handle.spawn(
            Watcher {
                handle: handle.clone(),
                xcrun,
                scan: SimulatorScan::new(devices_dir),
                cfg,
                out: out.clone(),
                status,
                sims: HashMap::new(),
                found: HashMap::new(),
            }
            .run(),
        );
        SimulatorWatch { task, out }
    }
}

impl Drop for SimulatorWatch {
    fn drop(&mut self) {
        // Aborting drops the followers and backfills, which are all `kill_on_drop`.
        self.task.abort();
        self.out.send_replace(Arc::from(Vec::new()));
    }
}

/// The log processes of one booted simulator, stopped when it shuts down.
struct SimTasks {
    name: String,
    _follower: Follower,
    pump: JoinHandle<()>,
    backfill: JoinHandle<()>,
}

impl Drop for SimTasks {
    fn drop(&mut self) {
        self.pump.abort();
        self.backfill.abort();
    }
}

struct Line {
    udid: String,
    line: String,
}

struct Watcher {
    handle: Handle,
    xcrun: PathBuf,
    scan: SimulatorScan,
    cfg: WatchTiming,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
    status: Arc<watch::Sender<PlatformState>>,
    sims: HashMap<String, SimTasks>,
    found: HashMap<(String, u32), Candidate>,
}

impl Watcher {
    async fn run(mut self) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut scan_tick = tokio::time::interval(self.cfg.sim_scan);
        scan_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut live_tick = tokio::time::interval(self.cfg.liveness);
        live_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                Some(l) = rx.recv() => self.on_line(&l),
                _ = scan_tick.tick() => self.on_scan(&tx),
                _ = live_tick.tick() => self.check_liveness(),
            }
        }
    }

    fn on_scan(&mut self, tx: &mpsc::UnboundedSender<Line>) {
        let booted = self.scan.booted();
        let now = PlatformState::Watching {
            devices: booted.len(),
        };
        self.status.send_if_modified(|cur| {
            if *cur == now {
                false
            } else {
                *cur = now;
                true
            }
        });
        let gone: Vec<String> = self
            .sims
            .keys()
            .filter(|u| !booted.iter().any(|s| &s.udid == *u))
            .cloned()
            .collect();
        for udid in gone {
            self.sims.remove(&udid);
            self.found.retain(|(u, _), _| *u != udid);
        }
        for sim in booted {
            if !self.sims.contains_key(&sim.udid) {
                let tasks = self.start_sim(&sim.udid, &sim.name, tx);
                self.sims.insert(sim.udid, tasks);
            }
        }
        self.publish();
    }

    fn start_sim(&self, udid: &str, name: &str, tx: &mpsc::UnboundedSender<Line>) -> SimTasks {
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let follower = follow(
            &self.handle,
            self.xcrun.clone(),
            log_args(udid, "stream"),
            Backoff::default(),
            line_tx,
        );
        let pump = {
            let (tx, udid) = (tx.clone(), udid.to_string());
            self.handle.spawn(async move {
                while let Some(line) = line_rx.recv().await {
                    let l = Line {
                        udid: udid.clone(),
                        line,
                    };
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            })
        };
        let backfill = {
            let (tx, udid, xcrun) = (tx.clone(), udid.to_string(), self.xcrun.clone());
            self.handle.spawn(async move {
                let mut a = log_args(&udid, "show");
                a.splice(5..5, ["--last".to_string(), "1h".to_string()]);
                let refs: Vec<&str> = a.iter().map(String::as_str).collect();
                let Some(text) = run_once(&xcrun, &refs, BACKFILL_TIMEOUT).await else {
                    return;
                };
                for line in text.lines() {
                    let l = Line {
                        udid: udid.clone(),
                        line: line.to_string(),
                    };
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            })
        };
        SimTasks {
            name: name.to_string(),
            _follower: follower,
            pump,
            backfill,
        }
    }

    fn on_line(&mut self, l: &Line) {
        let Some(sim) = self.sims.get(&l.udid) else {
            return;
        };
        let Some(hit) = simulator_hit(&l.line) else {
            return;
        };
        if !pid_alive(hit.pid) {
            return;
        }
        let candidate = Candidate {
            platform: Platform::IosSimulator,
            device_id: l.udid.clone(),
            device_name: sim.name.clone(),
            pid: hit.pid,
            vm_uri: hit.url,
        };
        self.found.insert((l.udid.clone(), hit.pid), candidate);
        self.publish();
    }

    fn check_liveness(&mut self) {
        self.found.retain(|(_, pid), _| pid_alive(*pid));
        self.publish();
    }

    fn publish(&self) {
        let mut all: Vec<Candidate> = self.found.values().cloned().collect();
        all.sort_by(|a, b| (&a.device_id, a.pid).cmp(&(&b.device_id, b.pid)));
        self.out.send_if_modified(|cur| {
            if cur[..] == all[..] {
                false
            } else {
                *cur = Arc::from(all);
                true
            }
        });
    }
}

/// `simctl spawn <udid> log <verb> --style ndjson --predicate <PREDICATE>`.
fn log_args(udid: &str, verb: &str) -> Vec<String> {
    ["simctl", "spawn", udid, "log", verb]
        .into_iter()
        .chain(["--style", "ndjson", "--predicate", PREDICATE])
        .map(String::from)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    fn plist(udid: &str, name: &str, state: i32) -> String {
        format!(
            "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>UDID</key>\n\t<string>{udid}</string>\n\t<key>name</key>\n\t<string>{name}</string>\n\t<key>state</key>\n\t<integer>{state}</integer>\n</dict>\n</plist>\n"
        )
    }

    fn marker(pid: u32) -> String {
        format!(
            r#"{{"processID":{pid},"eventMessage":"The Dart VM service is listening on http://127.0.0.1:51234/tok=/"}}"#
        )
    }

    /// `stream` and `show` are shell snippets run for the two verbs.
    fn fake_xcrun(dir: &Path, stream: &str, show: &str) -> PathBuf {
        let script = format!(
            r#"#!/bin/sh
DIR="{dir}"
case "$*" in
  *"log stream"*) echo $$ > "$DIR/stream_pid"; {stream};;
  *"log show --last 1h"*) {show};;
esac
"#,
            dir = dir.display()
        );
        let tmp = dir.join("xcrun.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("xcrun");
        std::fs::rename(&tmp, &path).unwrap();
        path
    }

    fn timing() -> WatchTiming {
        WatchTiming {
            liveness: Duration::from_millis(200),
            alive_wait: Duration::from_millis(100),
            sim_scan: Duration::from_millis(100),
        }
    }

    type Chans = (
        Arc<watch::Sender<Arc<[Candidate]>>>,
        Arc<watch::Sender<PlatformState>>,
    );

    fn chans() -> Chans {
        (
            Arc::new(watch::channel(Arc::from(Vec::new())).0),
            Arc::new(watch::channel(PlatformState::Off).0),
        )
    }

    async fn until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        f()
    }

    fn setup(stream: &str, show: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sims = dir.path().join("devices");
        std::fs::create_dir_all(sims.join("U1")).unwrap();
        std::fs::write(
            sims.join("U1/device.plist"),
            plist("U1", "iPhone 18 Pro", 3),
        )
        .unwrap();
        let xcrun = fake_xcrun(dir.path(), stream, show);
        (dir, xcrun, sims)
    }

    fn start(xcrun: PathBuf, sims: PathBuf) -> (SimulatorWatch, Chans) {
        let c = chans();
        let w = SimulatorWatch::spawn(
            &Handle::current(),
            xcrun,
            sims,
            timing(),
            c.0.clone(),
            c.1.clone(),
        );
        (w, c)
    }

    fn sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn watcher_finds_and_drops_a_live_app() {
        let mut child = sleeper();
        let pid = child.id();
        let (_d, xcrun, sims) = setup(&format!("echo '{}'; exec sleep 30", marker(pid)), "true");
        let (_w, (out, status)) = start(xcrun, sims);
        assert!(until(|| out.borrow().len() == 1, 3).await);
        assert_eq!(
            out.borrow()[0],
            Candidate {
                platform: Platform::IosSimulator,
                device_id: "U1".into(),
                device_name: "iPhone 18 Pro".into(),
                pid,
                vm_uri: "http://127.0.0.1:51234/tok=/".into(),
            }
        );
        assert_eq!(*status.borrow(), PlatformState::Watching { devices: 1 });
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(until(|| out.borrow().is_empty(), 3).await);
    }

    /// The pid of a process that has exited and been reaped.
    fn dead_pid() -> u32 {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[tokio::test]
    async fn stale_backfill_line_is_skipped() {
        let show = format!("echo '{}'; touch \"$DIR/show_ran\"", marker(dead_pid()));
        let (d, xcrun, sims) = setup("exec sleep 30", &show);
        let (_w, (out, _)) = start(xcrun, sims);
        let ran = d.path().join("show_ran");
        assert!(until(|| ran.exists(), 3).await, "backfill ran");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(out.borrow().is_empty());
    }

    #[tokio::test]
    async fn backfill_line_of_a_live_app_is_found() {
        let mut child = sleeper();
        let (_d, xcrun, sims) = setup("exec sleep 30", &format!("echo '{}'", marker(child.id())));
        let (_w, (out, _)) = start(xcrun, sims);
        assert!(until(|| out.borrow().len() == 1, 3).await);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn shutting_down_a_simulator_stops_its_follower() {
        let mut child = sleeper();
        let (d, xcrun, sims) = setup(
            &format!("echo '{}'; exec sleep 30", marker(child.id())),
            "true",
        );
        let (_w, (out, status)) = start(xcrun, sims.clone());
        assert!(until(|| out.borrow().len() == 1, 3).await);
        let pid: u32 = std::fs::read_to_string(d.path().join("stream_pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::write(
            sims.join("U1/device.plist"),
            plist("U1", "iPhone 18 Pro", 1),
        )
        .unwrap();
        assert!(
            until(
                || *status.borrow() == PlatformState::Watching { devices: 0 },
                3
            )
            .await
        );
        assert!(until(|| !pid_alive(pid), 3).await, "follower stopped");
        assert!(out.borrow().is_empty(), "its apps leave with it");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
