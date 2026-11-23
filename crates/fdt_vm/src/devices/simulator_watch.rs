//! Watching booted iOS simulators. The `device.plist` files say which simulators are up. A
//! simulator app is a process on this Mac, so a scan of the host's processes (read in-process,
//! no child process) spots each new Flutter app, and one `log show` query for its pid reads the
//! VM service line. No process runs while nothing new starts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::follow::{Children, run_once};
use super::marker::simulator_hit;
use super::processes::Processes;
use super::simulators::SimulatorScan;
use super::{Candidate, Platform, PlatformState, WatchTiming};
use crate::discovery::pid_alive;

/// How long one `log show` query may run.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest stretch of log a query reads: an app that started longer ago than this is not
/// found (its line is out of reach).
const MAX_WINDOW: Duration = Duration::from_secs(3600);
/// How far before the process started a query begins reading.
const WINDOW_MARGIN: Duration = Duration::from_secs(60);

/// What the simulator watcher reads: `xcrun`, the simulator devices folder, and the host's
/// processes.
pub(crate) struct SimTools {
    pub(crate) xcrun: PathBuf,
    pub(crate) devices_dir: PathBuf,
    pub(crate) processes: Arc<dyn Processes>,
}

/// Handle to the simulator watcher. Dropping it stops everything and empties its set.
pub(crate) struct SimulatorWatch {
    task: JoinHandle<()>,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
}

impl SimulatorWatch {
    pub(crate) fn spawn(
        handle: &Handle,
        tools: SimTools,
        children: Children,
        cfg: WatchTiming,
        out: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<PlatformState>>,
    ) -> SimulatorWatch {
        let SimTools {
            xcrun,
            devices_dir,
            processes,
        } = tools;
        // `proc_pidpath` reports resolved paths, while the folder may be reached through a
        // symbolic link (a custom home), so both spellings are matched.
        let mut roots = vec![devices_dir.clone()];
        if let Ok(real) = std::fs::canonicalize(&devices_dir)
            && real != devices_dir
        {
            roots.push(real);
        }
        let task = handle.spawn(
            Watcher {
                handle: handle.clone(),
                xcrun,
                roots,
                scan: SimulatorScan::new(devices_dir),
                processes,
                children,
                cfg,
                out: out.clone(),
                status,
                booted: HashMap::new(),
                seen: HashSet::new(),
                lookups: HashMap::new(),
                found: HashMap::new(),
            }
            .run(),
        );
        SimulatorWatch { task, out }
    }

    /// Stops the watcher and empties its set. Aborting drops the lookups, whose `log show`
    /// children are `kill_on_drop`; a shutdown sends them SIGTERM first.
    pub(crate) fn stop(&self) {
        self.task.abort();
        self.out.send_replace(Arc::from(Vec::new()));
    }
}

impl Drop for SimulatorWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A running query for one app's VM service line.
struct Lookup {
    udid: String,
    task: JoinHandle<()>,
}

impl Drop for Lookup {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A VM service line found for a simulator app.
struct Hit {
    udid: String,
    pid: u32,
    url: String,
}

struct Watcher {
    handle: Handle,
    xcrun: PathBuf,
    /// The simulator devices folder, as configured and with links resolved.
    roots: Vec<PathBuf>,
    scan: SimulatorScan,
    processes: Arc<dyn Processes>,
    children: Children,
    cfg: WatchTiming,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
    status: Arc<watch::Sender<PlatformState>>,
    /// Booted simulators: udid to name.
    booted: HashMap<String, String>,
    /// Pids already looked at, so each process's path is read once.
    seen: HashSet<u32>,
    lookups: HashMap<u32, Lookup>,
    found: HashMap<u32, Candidate>,
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
                Some(hit) = rx.recv() => self.on_hit(hit),
                _ = scan_tick.tick() => self.on_scan(&tx),
                _ = live_tick.tick() => self.check_liveness(),
            }
        }
    }

    fn on_scan(&mut self, tx: &mpsc::UnboundedSender<Hit>) {
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
        self.booted = booted.into_iter().map(|s| (s.udid, s.name)).collect();
        // A simulator that shut down takes its apps and lookups with it.
        let booted = &self.booted;
        self.found.retain(|_, c| booted.contains_key(&c.device_id));
        self.lookups.retain(|_, l| booted.contains_key(&l.udid));
        if self.booted.is_empty() {
            // No simulator app can run; when one boots, every process is looked at once.
            self.seen.clear();
        } else {
            self.scan_processes(tx);
        }
        self.publish();
    }

    /// Looks at each process not seen before, and starts a lookup for each new Flutter app on a
    /// booted simulator.
    fn scan_processes(&mut self, tx: &mpsc::UnboundedSender<Hit>) {
        let pids = self.processes.pids();
        let mut seen = HashSet::with_capacity(pids.len());
        for pid in pids {
            if self.seen.contains(&pid) {
                seen.insert(pid);
                continue;
            }
            let app = self
                .processes
                .exe(pid)
                .and_then(|info| Some((flutter_sim_app(&self.roots, &info.path)?, info.started)));
            match app {
                Some((udid, started)) if self.booted.contains_key(&udid) => {
                    let task = self.handle.spawn(lookup(
                        self.xcrun.clone(),
                        udid.clone(),
                        pid,
                        started,
                        self.cfg,
                        self.children.clone(),
                        tx.clone(),
                    ));
                    self.lookups.insert(pid, Lookup { udid, task });
                    seen.insert(pid);
                }
                // On a simulator not listed as booted yet: look again at the next scan.
                Some(_) => {}
                None => {
                    seen.insert(pid);
                }
            }
        }
        self.seen = seen;
        // A process that exited stops its lookup.
        let seen = &self.seen;
        self.lookups.retain(|pid, _| seen.contains(pid));
    }

    fn on_hit(&mut self, hit: Hit) {
        if self.lookups.remove(&hit.pid).is_none() || !pid_alive(hit.pid) {
            return;
        }
        let Some(name) = self.booted.get(&hit.udid) else {
            return;
        };
        let candidate = Candidate {
            platform: Platform::IosSimulator,
            device_id: hit.udid,
            device_name: name.clone(),
            pid: hit.pid,
            vm_uri: hit.url,
        };
        self.found.insert(hit.pid, candidate);
        self.publish();
    }

    fn check_liveness(&mut self) {
        // `kill(pid, 0)` has no false negatives, so one failed check is enough.
        self.found.retain(|pid, _| pid_alive(*pid));
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

/// The simulator udid, when `exe` is a Flutter app's executable on a simulator:
/// `<root>/<udid>/data/Containers/Bundle/Application/<uuid>/<Name>.app/<exe>`, with
/// `<Name>.app/Frameworks/Flutter.framework` present.
pub(crate) fn flutter_sim_app(roots: &[PathBuf], exe: &Path) -> Option<String> {
    let rest = roots.iter().find_map(|r| exe.strip_prefix(r).ok())?;
    let parts: Vec<&str> = rest
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let [
        udid,
        "data",
        "Containers",
        "Bundle",
        "Application",
        _uuid,
        app,
        _exe,
    ] = parts[..]
    else {
        return None;
    };
    if !app.ends_with(".app") {
        return None;
    }
    let bundle = exe.parent()?;
    bundle
        .join("Frameworks/Flutter.framework")
        .is_dir()
        .then(|| udid.to_string())
}

/// Reads the app's VM service line with one `log show` query, and again at `lookup_retry`,
/// twice that, four times that... after the app was first seen, while the line may not be
/// logged yet, giving up `lookup_give_up` after. An app that was already running for longer
/// than that gets one query: its line was logged long ago, or never.
async fn lookup(
    xcrun: PathBuf,
    udid: String,
    pid: u32,
    started: Option<SystemTime>,
    cfg: WatchTiming,
    children: Children,
    tx: mpsc::UnboundedSender<Hit>,
) {
    let first = Instant::now();
    let age = started.and_then(|s| SystemTime::now().duration_since(s).ok());
    let retry = age.is_none_or(|a| a < cfg.lookup_give_up);
    let mut next = cfg.lookup_retry;
    loop {
        if !pid_alive(pid) || children.is_stopped() {
            return;
        }
        let args = show_args(&udid, pid, window(started));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Some(text) = run_once(&xcrun, &refs, QUERY_TIMEOUT, &children).await
            && let Some(hit) = text
                .lines()
                .filter_map(simulator_hit)
                .find(|h| h.pid == pid)
        {
            let _ = tx.send(Hit {
                udid,
                pid,
                url: hit.url,
            });
            return;
        }
        let at = first + next;
        let give_up = first + cfg.lookup_give_up;
        if !retry || at >= give_up || Instant::now() >= give_up {
            return;
        }
        tokio::time::sleep_until(at).await;
        next *= 2;
    }
}

/// How far back a query reads: from a minute before the process started, at most an hour.
fn window(started: Option<SystemTime>) -> Duration {
    started
        .and_then(|s| SystemTime::now().duration_since(s).ok())
        .map_or(MAX_WINDOW, |age| (age + WINDOW_MARGIN).min(MAX_WINDOW))
}

/// `simctl spawn <udid> log show --last <n>s --style ndjson --predicate <pid and marker>`.
fn show_args(udid: &str, pid: u32, window: Duration) -> Vec<String> {
    let predicate =
        format!("processID == {pid} AND eventMessage CONTAINS \"Dart VM service is listening\"");
    let last = format!("{}s", window.as_secs().max(1));
    [
        "simctl",
        "spawn",
        udid,
        "log",
        "show",
        "--last",
        &last,
        "--style",
        "ndjson",
        "--predicate",
        &predicate,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flutter_sim_app_needs_the_bundle_layout_and_the_flutter_framework() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Devices");
        let app = root.join("U1/data/Containers/Bundle/Application/A1/Runner.app");
        std::fs::create_dir_all(app.join("Frameworks/Flutter.framework")).unwrap();
        let other = root.join("U1/data/Containers/Bundle/Application/A2/Safari.app");
        std::fs::create_dir_all(&other).unwrap();
        let roots = vec![root.clone()];
        assert_eq!(
            flutter_sim_app(&roots, &app.join("Runner")),
            Some("U1".to_string())
        );
        // No Flutter.framework: not a Flutter app.
        assert_eq!(flutter_sim_app(&roots, &other.join("Safari")), None);
        // A helper deeper inside the bundle, or outside the devices folder: not the app.
        assert_eq!(
            flutter_sim_app(&roots, &app.join("Frameworks/Flutter.framework/Flutter")),
            None
        );
        assert_eq!(
            flutter_sim_app(&roots, Path::new("/usr/bin/Runner.app/Runner")),
            None
        );
        // Reached through the second spelling of the folder.
        let alias = vec![PathBuf::from("/elsewhere"), root];
        assert_eq!(
            flutter_sim_app(&alias, &app.join("Runner")),
            Some("U1".to_string())
        );
    }

    #[test]
    fn show_args_filter_on_the_pid_and_the_marker() {
        assert_eq!(
            show_args("U1", 42, Duration::from_secs(61)),
            [
                "simctl",
                "spawn",
                "U1",
                "log",
                "show",
                "--last",
                "61s",
                "--style",
                "ndjson",
                "--predicate",
                "processID == 42 AND eventMessage CONTAINS \"Dart VM service is listening\"",
            ]
        );
    }

    #[test]
    fn window_reaches_back_to_the_process_start_within_an_hour() {
        let ago = |s| Some(SystemTime::now() - Duration::from_secs(s));
        let w = window(ago(5)).as_secs();
        assert!((65..=66).contains(&w), "{w}");
        let w = window(ago(600)).as_secs();
        assert!((660..=661).contains(&w), "{w}");
        assert_eq!(window(ago(7200)), MAX_WINDOW);
        assert_eq!(window(None), MAX_WINDOW);
    }

    #[cfg(unix)]
    mod fake {
        use super::super::*;
        use crate::devices::processes::{ExeInfo, FakeProcesses};
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        use std::sync::atomic::Ordering;

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

        /// `show` is a shell snippet run for `log show`; every call's arguments go to `calls`.
        fn fake_xcrun(dir: &Path, show: &str) -> PathBuf {
            let script = format!(
                r#"#!/bin/sh
DIR="{dir}"
echo "$*" >> "$DIR/calls"
case "$*" in
  *"log show"*) {show};;
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
                lookup_retry: Duration::from_millis(200),
                lookup_give_up: Duration::from_secs(30),
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
            let end = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < end {
                if f() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            f()
        }

        struct Rig {
            dir: tempfile::TempDir,
            sims: PathBuf,
            xcrun: PathBuf,
            procs: Arc<FakeProcesses>,
        }

        impl Rig {
            fn new(show: &str) -> Rig {
                let dir = tempfile::tempdir().unwrap();
                let sims = dir.path().join("devices");
                std::fs::create_dir_all(sims.join("U1")).unwrap();
                std::fs::write(
                    sims.join("U1/device.plist"),
                    plist("U1", "iPhone 18 Pro", 3),
                )
                .unwrap();
                let xcrun = fake_xcrun(dir.path(), show);
                Rig {
                    dir,
                    sims,
                    xcrun,
                    procs: Arc::new(FakeProcesses::default()),
                }
            }

            /// The executable of an app bundle on simulator U1, with or without Flutter.
            fn app(&self, name: &str, flutter: bool) -> PathBuf {
                let bundle = self.sims.join(format!(
                    "U1/data/Containers/Bundle/Application/{name}/Runner.app"
                ));
                std::fs::create_dir_all(&bundle).unwrap();
                if flutter {
                    std::fs::create_dir_all(bundle.join("Frameworks/Flutter.framework")).unwrap();
                }
                bundle.join("Runner")
            }

            fn start_with(&self, cfg: WatchTiming) -> (SimulatorWatch, Chans) {
                let c = chans();
                let w = SimulatorWatch::spawn(
                    &Handle::current(),
                    SimTools {
                        xcrun: self.xcrun.clone(),
                        devices_dir: self.sims.clone(),
                        processes: self.procs.clone(),
                    },
                    Children::default(),
                    cfg,
                    c.0.clone(),
                    c.1.clone(),
                );
                (w, c)
            }

            fn start(&self) -> (SimulatorWatch, Chans) {
                self.start_with(timing())
            }

            fn calls(&self) -> Vec<String> {
                std::fs::read_to_string(self.dir.path().join("calls"))
                    .map(|t| t.lines().map(String::from).collect())
                    .unwrap_or_default()
            }
        }

        fn sleeper() -> std::process::Child {
            Command::new("sleep")
                .arg("30")
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        }

        fn info(path: PathBuf, age: u64) -> ExeInfo {
            ExeInfo {
                path,
                started: Some(SystemTime::now() - Duration::from_secs(age)),
            }
        }

        /// The pid of a process that has exited and been reaped.
        fn dead_pid() -> u32 {
            let mut c = Command::new("true").spawn().unwrap();
            let pid = c.id();
            c.wait().unwrap();
            pid
        }

        #[tokio::test]
        async fn new_flutter_app_is_found_and_leaves_when_it_quits() {
            let mut child = sleeper();
            let pid = child.id();
            let r = Rig::new(&format!("echo '{}'", marker(pid)));
            r.procs.set(vec![(pid, info(r.app("A1", true), 1))]);
            let (_w, (out, status)) = r.start();
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
            let calls = r.calls();
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert!(calls[0].starts_with("simctl spawn U1 log show --last 6"));
            assert!(calls[0].contains(&format!("processID == {pid} AND")));
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(until(|| out.borrow().is_empty(), 3).await);
        }

        #[tokio::test]
        async fn a_line_logged_after_the_first_query_is_found_by_a_retry() {
            let mut child = sleeper();
            let pid = child.id();
            let r = Rig::new(&format!(
                "if [ -e \"$DIR/logged\" ]; then echo '{}'; fi",
                marker(pid)
            ));
            r.procs.set(vec![(pid, info(r.app("A1", true), 0))]);
            let (_w, (out, _)) = r.start();
            assert!(until(|| r.calls().len() == 1, 3).await);
            std::fs::write(r.dir.path().join("logged"), "").unwrap();
            assert!(until(|| out.borrow().len() == 1, 3).await, "found later");
            assert!(r.calls().len() >= 2);
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn lookups_give_up_after_the_cap() {
            let mut child = sleeper();
            let r = Rig::new("true");
            r.procs.set(vec![(child.id(), info(r.app("A1", true), 0))]);
            let (_w, (out, _)) = r.start_with(WatchTiming {
                lookup_retry: Duration::from_millis(50),
                lookup_give_up: Duration::from_millis(400),
                ..timing()
            });
            // Queries at 0, 50, 100 and 200 ms; the next (400 ms) is past the cap.
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let n = r.calls().len();
            assert!((2..=4).contains(&n), "{n} queries");
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(r.calls().len(), n, "no query after giving up");
            assert!(out.borrow().is_empty());
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn an_app_running_before_the_watch_gets_one_query_reaching_its_start() {
            let mut child = sleeper();
            let r = Rig::new("true");
            r.procs
                .set(vec![(child.id(), info(r.app("A1", true), 600))]);
            let (_w, _c) = r.start();
            assert!(until(|| r.calls().len() == 1, 3).await);
            let call = &r.calls()[0];
            assert!(
                call.contains("--last 660s") || call.contains("--last 661s"),
                "{call}"
            );
            tokio::time::sleep(Duration::from_millis(800)).await;
            assert_eq!(r.calls().len(), 1, "no retry for an old app");
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn other_processes_are_read_once_and_never_queried() {
            let mut child = sleeper();
            let pid = child.id();
            let r = Rig::new("true");
            r.procs.set(vec![
                (pid, info(r.app("A1", false), 0)),
                (pid + 1, info(PathBuf::from("/usr/bin/true"), 0)),
                (dead_pid(), info(r.app("A2", true), 0)),
            ]);
            let (_w, (out, status)) = r.start();
            assert!(
                until(
                    || *status.borrow() == PlatformState::Watching { devices: 1 },
                    3
                )
                .await
            );
            tokio::time::sleep(Duration::from_millis(600)).await;
            // Each of the three pids is read once, over several scans.
            assert_eq!(r.procs.exe_calls.load(Ordering::SeqCst), 3);
            // Not Flutter, not a simulator app, and a dead Flutter app: nothing is queried.
            assert!(r.calls().is_empty(), "{:?}", r.calls());
            assert!(out.borrow().is_empty());
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn no_process_is_listed_while_no_simulator_is_booted() {
            let r = Rig::new("true");
            std::fs::write(
                r.sims.join("U1/device.plist"),
                plist("U1", "iPhone 18 Pro", 1),
            )
            .unwrap();
            r.procs
                .set(vec![(1, info(PathBuf::from("/sbin/launchd"), 0))]);
            let (_w, (_, status)) = r.start();
            assert!(
                until(
                    || *status.borrow() == PlatformState::Watching { devices: 0 },
                    3
                )
                .await
            );
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(r.procs.exe_calls.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn shutting_down_a_simulator_drops_its_apps() {
            let mut child = sleeper();
            let pid = child.id();
            let r = Rig::new(&format!("echo '{}'", marker(pid)));
            r.procs.set(vec![(pid, info(r.app("A1", true), 1))]);
            let (_w, (out, status)) = r.start();
            assert!(until(|| out.borrow().len() == 1, 3).await);
            std::fs::write(
                r.sims.join("U1/device.plist"),
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
            assert!(until(|| out.borrow().is_empty(), 3).await, "its apps leave");
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
