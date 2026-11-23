//! Finding Flutter apps opened on iOS simulators and Android devices without `flutter run`, by
//! watching device logs for the VM service line the engine prints at startup.

pub mod android;
pub mod follow;
pub mod marker;
pub mod processes;
pub mod simulator_watch;
pub mod simulators;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::discovery::Os;
use android::AndroidWatch;
use follow::Children;
use processes::{HostProcesses, Processes};
use simulator_watch::{SimTools, SimulatorWatch};

/// The longest a shutdown waits, overall, for the watchers' children to exit and for their adb
/// forwards to be removed.
pub(crate) const SHUTDOWN_CAP: Duration = Duration::from_millis(1500);

/// How often the watchers check things, so tests can run them fast.
#[derive(Clone, Copy, Debug)]
pub struct WatchTiming {
    /// Interval between liveness checks of found apps.
    pub liveness: Duration,
    /// How long a connection to a forwarded port must stay open to count as alive.
    pub alive_wait: Duration,
    /// Interval between simulator scans (device files and host processes).
    pub sim_scan: Duration,
    /// When a new simulator app's VM service line is first looked for again; each later try
    /// waits twice as long.
    pub lookup_retry: Duration,
    /// How long after a simulator app was first seen its line is looked for.
    pub lookup_give_up: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            liveness: Duration::from_secs(2),
            alive_wait: Duration::from_millis(400),
            sim_scan: Duration::from_secs(2),
            lookup_retry: Duration::from_secs(1),
            lookup_give_up: Duration::from_secs(30),
        }
    }
}

/// What one platform's watcher is doing, for the UI.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum PlatformState {
    #[default]
    Off,
    Watching {
        devices: usize,
    },
    Unavailable(String),
}

/// The kind of device a candidate app runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    IosSimulator,
    Android,
}

impl Platform {
    /// The value the VM reports as `getVM.operatingSystem` for apps on this platform.
    pub fn os(self) -> &'static str {
        match self {
            Platform::IosSimulator => "ios",
            Platform::Android => "android",
        }
    }
}

/// An app found in a device log, not yet confirmed to be a live VM service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub platform: Platform,
    pub device_id: String,
    pub device_name: String,
    pub pid: u32,
    /// An `http://127.0.0.1:<port>/<token>/` URL reachable from this machine.
    pub vm_uri: String,
}

/// Both platforms' watcher states, for the UI.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WatchStatus {
    pub simulator: PlatformState,
    pub android: PlatformState,
}

/// Where the tools are and how fast to poll. `None` means the tool or folder was not found.
#[derive(Clone, Debug)]
pub struct DeviceWatchConfig {
    pub adb: Option<PathBuf>,
    pub xcrun: Option<PathBuf>,
    pub simulator_dir: Option<PathBuf>,
    /// The host's processes, where simulator apps run and orphaned watchers are looked for.
    pub processes: Arc<dyn Processes>,
    /// Where the adb forwards fdt holds are recorded (`<config dir>/fdt`), so a later run can
    /// remove them if this one dies without doing so. `None` records nothing.
    pub state_dir: Option<PathBuf>,
    pub timing: WatchTiming,
}

impl DeviceWatchConfig {
    /// Looks for the tools in the real environment.
    pub fn detect() -> Self {
        let home = dirs::home_dir();
        let exists = |p: &std::path::Path| p.exists();
        DeviceWatchConfig {
            adb: tools::find_adb(
                Os::current(),
                &|k| std::env::var(k).ok(),
                home.clone(),
                &exists,
            ),
            xcrun: tools::find_xcrun(&exists),
            simulator_dir: simulators::simulator_devices_dir(home),
            processes: Arc::new(HostProcesses),
            state_dir: dirs::config_dir().map(|d| d.join("fdt")),
            timing: WatchTiming::default(),
        }
    }
}

/// Runs the simulator and Android watchers and publishes the union of what they find.
/// [`shutdown`](DeviceWatch::shutdown), which `Drop` also runs, stops both, ends their child
/// processes, removes their forwards, and publishes an empty set and the default status.
pub struct DeviceWatch {
    merge: JoinHandle<()>,
    simulator: Option<SimulatorWatch>,
    android: Option<AndroidWatch>,
    /// Every child process the watchers started and have not reaped.
    children: Children,
    candidates: Arc<watch::Sender<Arc<[Candidate]>>>,
    status: Arc<watch::Sender<WatchStatus>>,
    /// Set by `shutdown`; also keeps a merge already running from undoing its empty set.
    closed: Arc<AtomicBool>,
}

impl DeviceWatch {
    pub fn spawn(
        handle: &Handle,
        config: DeviceWatchConfig,
        candidates: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<WatchStatus>>,
    ) -> DeviceWatch {
        let new_set = || Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let new_state = |s| Arc::new(watch::channel(s).0);
        let children = Children::default();
        {
            // Watchers an earlier fdt left running when it crashed or was killed.
            let processes = config.processes.clone();
            handle.spawn_blocking(move || android::sweep_orphans(&*processes));
        }

        let sim_out = new_set();
        let and_out = new_set();
        let (sim_state, simulator) = match (config.xcrun, config.simulator_dir) {
            (Some(xcrun), Some(dir)) => {
                let st = new_state(PlatformState::Off);
                let w = SimulatorWatch::spawn(
                    handle,
                    SimTools {
                        xcrun,
                        devices_dir: dir,
                        processes: config.processes,
                    },
                    children.clone(),
                    config.timing,
                    sim_out.clone(),
                    st.clone(),
                );
                (st, Some(w))
            }
            _ if Os::current() != Os::MacOs => (new_state(PlatformState::Off), None),
            (xcrun, _) => {
                let why = if xcrun.is_some() {
                    "simulator folder not found"
                } else {
                    "xcrun not found"
                };
                (new_state(PlatformState::Unavailable(why.into())), None)
            }
        };
        let (and_state, android) = match config.adb {
            Some(adb) => {
                let st = new_state(PlatformState::Off);
                let w = AndroidWatch::spawn(
                    handle,
                    adb,
                    config.timing,
                    and_out.clone(),
                    st.clone(),
                    children.clone(),
                    config.state_dir,
                );
                (st, Some(w))
            }
            None => (
                new_state(PlatformState::Unavailable("adb not found".into())),
                None,
            ),
        };

        let closed = Arc::new(AtomicBool::new(false));
        let merge = {
            let closed = closed.clone();
            let (candidates, status) = (candidates.clone(), status.clone());
            let (mut sim_rx, mut and_rx) = (sim_out.subscribe(), and_out.subscribe());
            let (mut sim_st, mut and_st) = (sim_state.subscribe(), and_state.subscribe());
            handle.spawn(async move {
                // Keep the senders alive for the receivers.
                let _keep = (sim_out, and_out, sim_state, and_state);
                loop {
                    let mut all: Vec<Candidate> = sim_rx
                        .borrow_and_update()
                        .iter()
                        .chain(and_rx.borrow_and_update().iter())
                        .cloned()
                        .collect();
                    all.sort_by(|a, b| a.vm_uri.cmp(&b.vm_uri));
                    candidates.send_if_modified(|cur| {
                        // A merge already running when `Drop` fires must not undo its empty set.
                        if closed.load(Ordering::SeqCst) || cur[..] == all[..] {
                            false
                        } else {
                            *cur = Arc::from(all);
                            true
                        }
                    });
                    let now = WatchStatus {
                        simulator: sim_st.borrow_and_update().clone(),
                        android: and_st.borrow_and_update().clone(),
                    };
                    status.send_if_modified(|cur| {
                        if closed.load(Ordering::SeqCst) || *cur == now {
                            false
                        } else {
                            *cur = now;
                            true
                        }
                    });
                    tokio::select! {
                        r = sim_rx.changed() => if r.is_err() { return },
                        r = and_rx.changed() => if r.is_err() { return },
                        r = sim_st.changed() => if r.is_err() { return },
                        r = and_st.changed() => if r.is_err() { return },
                    }
                }
            })
        };
        DeviceWatch {
            merge,
            simulator,
            android,
            children,
            candidates,
            status,
            closed,
        }
    }

    /// Stops watching, synchronously. Sends SIGTERM to every child process the watchers
    /// started and waits for them to exit, then removes the adb forwards they hold, in
    /// parallel; all of it within [`SHUTDOWN_CAP`] overall. Then publishes an empty set and
    /// the default status. Runs once; later calls return at once.
    ///
    /// The app calls it when it quits (GPUI never drops its globals) and on SIGTERM, SIGINT and
    /// SIGHUP.
    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let deadline = std::time::Instant::now() + SHUTDOWN_CAP;
        // With `closed` set, no merge write can land after the final values below.
        self.merge.abort();
        let pids = self.children.terminate();
        // Wait for them to exit before the watchers are aborted, whose `kill_on_drop` would
        // follow the SIGTERM with a SIGKILL.
        while !pids.iter().all(|p| follow::has_exited(*p)) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(s) = &self.simulator {
            s.stop();
        }
        if let Some(a) = &self.android {
            a.shutdown(deadline);
        }
        self.candidates.send_replace(Arc::from(Vec::new()));
        self.status.send_replace(WatchStatus::default());
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use processes::{ExeInfo, FakeProcesses};
    use std::os::unix::fs::PermissionsExt;
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

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, body).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join(name);
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

    /// A booted simulator `U1` running one Flutter app (a `sleep` child), whose `log show`
    /// query prints its VM service line.
    struct Rig {
        dir: tempfile::TempDir,
        out: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<WatchStatus>>,
        child: std::process::Child,
        config: DeviceWatchConfig,
        watch: Option<DeviceWatch>,
    }

    fn rig() -> Rig {
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sims = dir.path().join("devices");
        std::fs::create_dir_all(sims.join("U1")).unwrap();
        std::fs::write(
            sims.join("U1/device.plist"),
            plist("U1", "iPhone 18 Pro", 3),
        )
        .unwrap();
        let bundle = sims.join("U1/data/Containers/Bundle/Application/A1/Runner.app");
        std::fs::create_dir_all(bundle.join("Frameworks/Flutter.framework")).unwrap();
        let procs = FakeProcesses::default();
        procs.set(vec![(
            child.id(),
            ExeInfo {
                path: bundle.join("Runner"),
                started: Some(std::time::SystemTime::now()),
            },
        )]);
        let xcrun = write_script(
            dir.path(),
            "xcrun",
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"log show\"*) echo '{}';;\nesac\n",
                marker(child.id())
            ),
        );
        let config = DeviceWatchConfig {
            adb: None,
            xcrun: Some(xcrun),
            simulator_dir: Some(sims),
            processes: Arc::new(procs),
            state_dir: None,
            timing: timing(),
        };
        let out = Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let status = Arc::new(watch::channel(WatchStatus::default()).0);
        let watch = DeviceWatch::spawn(
            &Handle::current(),
            config.clone(),
            out.clone(),
            status.clone(),
        );
        Rig {
            dir,
            out,
            status,
            child,
            config,
            watch: Some(watch),
        }
    }

    #[tokio::test]
    async fn fake_simulator_end_to_end() {
        let mut r = rig();
        assert!(until(|| r.out.borrow().len() == 1, 3).await);
        assert_eq!(
            r.out.borrow()[0],
            Candidate {
                platform: Platform::IosSimulator,
                device_id: "U1".into(),
                device_name: "iPhone 18 Pro".into(),
                pid: r.child.id(),
                vm_uri: "http://127.0.0.1:51234/tok=/".into(),
            }
        );
        assert_eq!(
            *r.status.borrow(),
            WatchStatus {
                simulator: PlatformState::Watching { devices: 1 },
                android: PlatformState::Unavailable("adb not found".into()),
            }
        );
        r.child.kill().unwrap();
        r.child.wait().unwrap();
        assert!(until(|| r.out.borrow().is_empty(), 3).await);
    }

    #[tokio::test]
    async fn device_watch_merges_simulator_and_android_sets() {
        use tokio::net::TcpListener;
        let r = rig();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _holder = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                keep.push(s);
            }
        });
        let adb = write_script(
            r.dir.path(),
            "adb",
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  \"track-devices\") printf '0015emulator-5554\\tdevice\\n'; exec sleep 30;;\n  \"devices -l\") echo 'emulator-5554 device model:Pixel_8';;\n  *\"logcat -d\"*) true;;\n  *\"logcat -v brief -T 1\"*) echo 'I/flutter ( 4242): The Dart VM service is listening on http://127.0.0.1:40421/tok=/'; exec sleep 30;;\n  *\"forward tcp:0 tcp:\"*) echo {port};;\nesac\n"
            ),
        );

        let out = Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let status = Arc::new(watch::channel(WatchStatus::default()).0);
        let _both = DeviceWatch::spawn(
            &Handle::current(),
            DeviceWatchConfig {
                adb: Some(adb),
                ..r.config.clone()
            },
            out.clone(),
            status,
        );
        assert!(until(|| out.borrow().len() == 2, 3).await);
        {
            let cur = out.borrow();
            let mut sorted: Vec<_> = cur.iter().map(|c| c.vm_uri.clone()).collect();
            sorted.sort();
            assert_eq!(
                sorted,
                cur.iter().map(|c| c.vm_uri.clone()).collect::<Vec<_>>()
            );
            assert!(cur.iter().any(|c| c.platform == Platform::IosSimulator));
            assert!(cur.iter().any(|c| c.platform == Platform::Android
                && c.vm_uri == format!("http://127.0.0.1:{port}/tok=/")));
        }
        let mut rx = out.subscribe();
        rx.borrow_and_update();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!rx.has_changed().unwrap(), "no wake while nothing changes");
    }

    #[tokio::test]
    async fn dropping_device_watch_empties_the_set() {
        let mut r = rig();
        assert!(until(|| r.out.borrow().len() == 1, 3).await);
        drop(r.watch.take());
        assert!(r.out.borrow().is_empty());
        assert_eq!(*r.status.borrow(), WatchStatus::default());
        r.child.kill().unwrap();
        r.child.wait().unwrap();
    }

    /// A fake adb whose long-lived commands note a SIGTERM in `term_track` / `term_logcat`.
    fn trapping_adb(dir: &std::path::Path, port: u16) -> PathBuf {
        write_script(
            dir,
            "adb",
            &format!(
                r#"#!/bin/sh
DIR="{dir}"
case "$*" in
  "track-devices")
    trap 'touch "$DIR/term_track"; exit 0' TERM
    printf '0015emulator-5554\tdevice\n'
    while :; do sleep 0.05; done;;
  "devices -l") echo 'emulator-5554 device model:Pixel_8';;
  *"logcat -d"*) true;;
  *"logcat -v brief -T 1"*)
    trap 'touch "$DIR/term_logcat"; exit 0' TERM
    echo 'I/flutter ( 4242): The Dart VM service is listening on http://127.0.0.1:40421/tok=/'
    while :; do sleep 0.05; done;;
  *"forward tcp:0 tcp:"*) echo {port};;
  *"forward --remove"*) echo "$@" >> "$DIR/removed";;
esac
"#,
                dir = dir.display()
            ),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_ends_children_with_sigterm_and_removes_forwards() {
        use tokio::net::TcpListener;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _holder = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                keep.push(s);
            }
        });
        let out = Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let status = Arc::new(watch::channel(WatchStatus::default()).0);
        let w = DeviceWatch::spawn(
            &Handle::current(),
            DeviceWatchConfig {
                adb: Some(trapping_adb(dir.path(), port)),
                xcrun: None,
                simulator_dir: None,
                processes: Arc::new(FakeProcesses::default()),
                state_dir: Some(state.clone()),
                timing: timing(),
            },
            out.clone(),
            status.clone(),
        );
        assert!(until(|| out.borrow().len() == 1, 3).await);
        let record = android::record_path(&state, std::process::id());
        assert!(record.exists(), "the held forward is recorded");
        let children = w.children.pids();
        assert_eq!(
            children.len(),
            2,
            "track-devices and one logcat: {children:?}"
        );

        let started = Instant::now();
        let shut = tokio::task::spawn_blocking(move || {
            w.shutdown();
            w
        })
        .await
        .unwrap();
        assert!(started.elapsed() < SHUTDOWN_CAP, "{:?}", started.elapsed());
        assert!(
            dir.path().join("term_track").exists(),
            "track-devices got SIGTERM"
        );
        assert!(
            dir.path().join("term_logcat").exists(),
            "logcat got SIGTERM"
        );
        assert!(children.iter().all(|p| follow::has_exited(*p)));
        let removed = std::fs::read_to_string(dir.path().join("removed")).unwrap();
        assert_eq!(
            removed.trim(),
            format!("-s emulator-5554 forward --remove tcp:{port}")
        );
        assert!(!record.exists(), "the record goes with the forward");
        assert!(out.borrow().is_empty());
        assert_eq!(*status.borrow(), WatchStatus::default());

        // Once is enough: a second call, and the drop, do nothing more.
        shut.shutdown();
        drop(shut);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let again = std::fs::read_to_string(dir.path().join("removed")).unwrap();
        assert_eq!(again, removed);
    }

    #[tokio::test]
    async fn spawn_stops_watchers_an_earlier_run_orphaned() {
        use std::os::unix::process::ExitStatusExt;
        let mut orphan = Command::new("sleep").arg("30").spawn().unwrap();
        let procs = FakeProcesses::default();
        *procs.orphans.lock().unwrap() = vec![(
            orphan.id(),
            vec!["/sdk/platform-tools/adb".into(), "track-devices".into()],
        )];
        let out = Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let status = Arc::new(watch::channel(WatchStatus::default()).0);
        let _w = DeviceWatch::spawn(
            &Handle::current(),
            DeviceWatchConfig {
                adb: None,
                xcrun: None,
                simulator_dir: None,
                processes: Arc::new(procs),
                state_dir: None,
                timing: timing(),
            },
            out,
            status,
        );
        assert!(until(|| follow::has_exited(orphan.id()), 3).await);
        assert_eq!(orphan.wait().unwrap().signal(), Some(libc::SIGTERM));
    }
}
