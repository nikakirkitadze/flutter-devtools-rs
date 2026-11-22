//! Finding Flutter apps opened on iOS simulators and Android devices without `flutter run`, by
//! watching device logs for the VM service line the engine prints at startup.

pub mod android;
pub mod follow;
pub mod marker;
pub mod simulator_watch;
pub mod simulators;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::discovery::Os;
use android::AndroidWatch;
use simulator_watch::SimulatorWatch;

/// How often the watchers check things, so tests can run them fast.
#[derive(Clone, Copy, Debug)]
pub struct WatchTiming {
    /// Interval between liveness checks of found apps.
    pub liveness: Duration,
    /// How long a connection to a forwarded port must stay open to count as alive.
    pub alive_wait: Duration,
    /// Interval between simulator device-file scans.
    pub sim_scan: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            liveness: Duration::from_secs(2),
            alive_wait: Duration::from_millis(400),
            sim_scan: Duration::from_secs(2),
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
            timing: WatchTiming::default(),
        }
    }
}

/// Runs the simulator and Android watchers and publishes the union of what they find. Dropping
/// it stops both, removes their forwards, and publishes an empty set and the default status.
pub struct DeviceWatch {
    merge: JoinHandle<()>,
    // Dropped after `merge` is aborted, in field order, each emptying its own set.
    _simulator: Option<SimulatorWatch>,
    _android: Option<AndroidWatch>,
    candidates: Arc<watch::Sender<Arc<[Candidate]>>>,
    status: Arc<watch::Sender<WatchStatus>>,
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

        let sim_out = new_set();
        let and_out = new_set();
        let (sim_state, simulator) = match (config.xcrun, config.simulator_dir) {
            (Some(xcrun), Some(dir)) => {
                let st = new_state(PlatformState::Off);
                let w = SimulatorWatch::spawn(
                    handle,
                    xcrun,
                    dir,
                    config.timing,
                    sim_out.clone(),
                    st.clone(),
                );
                (st, Some(w))
            }
            _ if Os::current() != Os::MacOs => (new_state(PlatformState::Off), None),
            _ => (
                new_state(PlatformState::Unavailable("xcrun not found".into())),
                None,
            ),
        };
        let (and_state, android) = match config.adb {
            Some(adb) => {
                let st = new_state(PlatformState::Off);
                let w =
                    AndroidWatch::spawn(handle, adb, config.timing, and_out.clone(), st.clone());
                (st, Some(w))
            }
            None => (
                new_state(PlatformState::Unavailable("adb not found".into())),
                None,
            ),
        };

        let merge = {
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
                        if cur[..] == all[..] {
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
                        if *cur == now {
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
            _simulator: simulator,
            _android: android,
            candidates,
            status,
        }
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        self.merge.abort();
        // The watchers drop after this, stopping their processes; the final values are sent here
        // so no merge can race them.
        self.candidates.send_replace(Arc::from(Vec::new()));
        self.status.send_replace(WatchStatus::default());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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

    fn fake_xcrun(dir: &std::path::Path, stream: &str, show: &str) -> PathBuf {
        let script = format!(
            "#!/bin/sh\nDIR=\"{}\"\ncase \"$*\" in\n  *\"log stream\"*) echo $$ > \"$DIR/stream_pid\"; {stream};;\n  *\"log show --last 1h\"*) {show};;\nesac\n",
            dir.display()
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

    struct Rig {
        _dir: tempfile::TempDir,
        sims: PathBuf,
        out: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<WatchStatus>>,
        child: std::process::Child,
        watch: Option<DeviceWatch>,
    }

    fn rig(stream_hit: bool, show_pid: Option<u32>) -> Rig {
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
        let stream = if stream_hit {
            format!("echo '{}'; exec sleep 30", marker(child.id()))
        } else {
            "exec sleep 30".to_string()
        };
        let show = show_pid.map_or("true".to_string(), |p| format!("echo '{}'", marker(p)));
        let xcrun = fake_xcrun(dir.path(), &stream, &show);
        let out = Arc::new(watch::channel(Arc::<[Candidate]>::from(Vec::new())).0);
        let status = Arc::new(watch::channel(WatchStatus::default()).0);
        let watch = DeviceWatch::spawn(
            &Handle::current(),
            DeviceWatchConfig {
                adb: None,
                xcrun: Some(xcrun),
                simulator_dir: Some(sims.clone()),
                timing: timing(),
            },
            out.clone(),
            status.clone(),
        );
        Rig {
            _dir: dir,
            sims,
            out,
            status,
            child,
            watch: Some(watch),
        }
    }

    #[tokio::test]
    async fn fake_simulator_end_to_end() {
        let mut r = rig(true, None);
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
    async fn stale_backfill_line_is_skipped() {
        let r = rig(false, Some(999_999));
        let st = r.status.clone();
        assert!(
            until(
                || st.borrow().simulator == PlatformState::Watching { devices: 1 },
                3
            )
            .await
        );
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(r.out.borrow().is_empty());
    }

    #[tokio::test]
    async fn shutting_down_a_simulator_stops_its_follower() {
        let r = rig(false, None);
        let st = r.status.clone();
        assert!(
            until(
                || st.borrow().simulator == PlatformState::Watching { devices: 1 },
                3
            )
            .await
        );
        let pid_file = r._dir.path().join("stream_pid");
        assert!(until(|| pid_file.exists(), 3).await);
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::fs::write(
            r.sims.join("U1/device.plist"),
            plist("U1", "iPhone 18 Pro", 1),
        )
        .unwrap();
        assert!(
            until(
                || st.borrow().simulator == PlatformState::Watching { devices: 0 },
                3
            )
            .await
        );
        assert!(until(|| !crate::discovery::pid_alive(pid), 3).await);
    }

    #[tokio::test]
    async fn dropping_device_watch_empties_the_set() {
        let mut r = rig(true, None);
        assert!(until(|| r.out.borrow().len() == 1, 3).await);
        drop(r.watch.take());
        assert!(r.out.borrow().is_empty());
        assert_eq!(*r.status.borrow(), WatchStatus::default());
        r.child.kill().unwrap();
        r.child.wait().unwrap();
    }
}
