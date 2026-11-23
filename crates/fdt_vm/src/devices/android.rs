//! Watching Android devices through `adb`: `track-devices` for the device list, `logcat` for the
//! VM service line, and `adb forward` to reach the VM service from this machine.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::follow::{Backoff, Children, Follower, follow, run_once, sigterm, spawn_tracked};
use super::marker::{android_hit, clean_adb_device_name};
use super::processes::Processes;
use super::{Candidate, Platform, PlatformState, SHUTDOWN_CAP, WatchTiming};
use crate::discovery::pid_alive;

const ADB_TIMEOUT: Duration = Duration::from_secs(5);
const REMOVE_TIMEOUT: Duration = Duration::from_secs(2);
const STABLE_RUN: Duration = Duration::from_secs(30);

/// Drains complete `adb track-devices` frames (4 hex digits of length, then that many bytes of
/// `serial\tstate\n` lines) from `buf`. A partial frame stays in `buf`.
pub fn parse_track_frames(buf: &mut Vec<u8>) -> Vec<Vec<(String, String)>> {
    let mut frames = Vec::new();
    loop {
        if buf.len() < 4 {
            break;
        }
        let len = std::str::from_utf8(&buf[..4])
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok());
        let Some(len) = len else {
            // Not a frame header: nothing after it can be trusted, so start over.
            buf.clear();
            break;
        };
        if buf.len() < 4 + len {
            break;
        }
        let body = String::from_utf8_lossy(&buf[4..4 + len]).into_owned();
        buf.drain(..4 + len);
        let devices = body
            .lines()
            .filter_map(|l| {
                let (serial, state) = l.split_once('\t')?;
                Some((serial.to_string(), state.trim().to_string()))
            })
            .collect();
        frames.push(devices);
    }
    frames
}

/// `(serial, cleaned model)` for every `device`-state line of `adb devices -l`.
pub fn parse_devices_l(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            if parts.next()? != "device" {
                return None;
            }
            let name = parts
                .find_map(|p| p.strip_prefix("model:"))
                .map(clean_adb_device_name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| serial.to_string());
            Some((serial.to_string(), name))
        })
        .collect()
}

/// Whether `127.0.0.1:port` accepts a connection that is still open after `wait`. Sends nothing.
pub async fn tcp_alive(port: u16, wait: Duration) -> bool {
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    else {
        return false;
    };
    let mut byte = [0u8; 1];
    match tokio::time::timeout(wait, stream.read(&mut byte)).await {
        Err(_) => true,
        Ok(Ok(0)) | Ok(Err(_)) => false,
        Ok(Ok(_)) => true,
    }
}

enum Ev {
    Frame(Vec<(String, String)>),
    TrackDown(String),
    Line { serial: String, line: String },
}

/// The arguments of the long-lived `logcat` follower for one device.
fn follow_args(serial: &str) -> Vec<String> {
    args(&[
        "-s",
        serial,
        "logcat",
        "-v",
        "brief",
        "-T",
        "1",
        "flutter:I",
        "*:S",
    ])
}

/// Whether `argv` is exactly one of the long-lived commands this watcher runs:
/// `<adb> track-devices`, or `<adb> -s <serial> logcat -v brief -T 1 flutter:I *:S`.
pub(crate) fn is_watcher_argv(argv: &[String]) -> bool {
    let Some((exe, rest)) = argv.split_first() else {
        return false;
    };
    if Path::new(exe).file_name().is_none_or(|n| n != "adb") {
        return false;
    }
    match rest {
        [only] => only == "track-devices",
        [flag, serial, ..] => flag == "-s" && !serial.is_empty() && rest == follow_args(serial),
        _ => false,
    }
}

/// Sends SIGTERM to watcher processes an earlier fdt left behind when it died without stopping
/// them: orphans (parent pid 1) whose argv is exactly a watcher command. Returns their pids.
pub(crate) fn sweep_orphans(processes: &dyn Processes) -> Vec<u32> {
    let pids: Vec<u32> = processes
        .orphans("adb")
        .into_iter()
        .filter(|(_, argv)| is_watcher_argv(argv))
        .map(|(pid, _)| pid)
        .collect();
    for pid in &pids {
        sigterm(*pid);
    }
    if !pids.is_empty() {
        log::info!(
            "stopped {} adb watcher process(es) an earlier run left behind",
            pids.len()
        );
    }
    pids
}

/// The adb forwards the watcher holds. With a record file, every change is written to it, so
/// a later run can remove the forwards if this one dies without doing so.
pub(crate) struct Held {
    list: Mutex<Vec<(String, u16)>>,
    record: Option<PathBuf>,
}

impl Held {
    pub(crate) fn new(record: Option<PathBuf>) -> Held {
        Held {
            list: Mutex::new(Vec::new()),
            record,
        }
    }

    /// Applies `change`, and rewrites the record if the list changed.
    fn update<R>(&self, change: impl FnOnce(&mut Vec<(String, u16)>) -> R) -> R {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        let before = list.clone();
        let r = change(&mut list);
        if *list != before
            && let Some(path) = &self.record
        {
            save_record(path, &list);
        }
        r
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, u16)> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn push(&self, serial: &str, port: u16) {
        self.update(|l| l.push((serial.to_string(), port)));
    }

    fn remove(&self, serial: &str, port: u16) {
        self.update(|l| l.retain(|(s, p)| !(s == serial && *p == port)));
    }

    fn retain(&self, keep: impl FnMut(&(String, u16)) -> bool) {
        self.update(|l| l.retain(keep));
    }

    fn replace(&self, new: Vec<(String, u16)>) {
        self.update(|l| *l = new);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecordedForward {
    serial: String,
    port: u16,
}

/// `forwards-<pid>.json` in `dir`: the forwards process `pid` holds.
pub(crate) fn record_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("forwards-{pid}.json"))
}

/// Writes `list` to `path` (through a temporary file), or deletes `path` when `list` is empty.
fn save_record(path: &Path, list: &[(String, u16)]) {
    if list.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let entries: Vec<RecordedForward> = list
        .iter()
        .map(|(serial, port)| RecordedForward {
            serial: serial.clone(),
            port: *port,
        })
        .collect();
    let Ok(text) = serde_json::to_string(&entries) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub(crate) fn read_record(path: &Path) -> Option<Vec<(String, u16)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let entries: Vec<RecordedForward> = serde_json::from_str(&text).ok()?;
    Some(entries.into_iter().map(|e| (e.serial, e.port)).collect())
}

/// Removes the forwards recorded by fdt processes that are no longer running, then deletes
/// their records. Forwards no record lists are never touched.
async fn sweep_records(adb: &Path, dir: &Path, children: &Children) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    let dead: Vec<PathBuf> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let pid: u32 = name
                .to_str()?
                .strip_prefix("forwards-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            (pid != me && !pid_alive(pid)).then(|| e.path())
        })
        .collect();
    for path in dead {
        let forwards = read_record(&path).unwrap_or_default();
        let removals = forwards.iter().map(|(serial, port)| async move {
            let spec = format!("tcp:{port}");
            run_once(
                adb,
                &["-s", serial, "forward", "--remove", &spec],
                REMOVE_TIMEOUT,
                children,
            )
            .await
        });
        futures_util::future::join_all(removals).await;
        if !forwards.is_empty() {
            log::info!(
                "removed {} adb forward(s) an earlier run left behind",
                forwards.len()
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Handle to the Android watcher. Dropping it stops everything and removes its forwards.
pub(crate) struct AndroidWatch {
    task: JoinHandle<()>,
    adb: PathBuf,
    held: Arc<Held>,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
}

impl AndroidWatch {
    /// With a `state_dir`, the held forwards are recorded in it, and forwards recorded there by
    /// fdt processes that died are removed before watching starts.
    pub(crate) fn spawn(
        handle: &Handle,
        adb: PathBuf,
        cfg: WatchTiming,
        out: Arc<watch::Sender<Arc<[Candidate]>>>,
        status: Arc<watch::Sender<PlatformState>>,
        children: Children,
        state_dir: Option<PathBuf>,
    ) -> AndroidWatch {
        let record = state_dir
            .as_deref()
            .map(|d| record_path(d, std::process::id()));
        let held = Arc::new(Held::new(record));
        let task = handle.spawn(
            Watcher {
                handle: handle.clone(),
                adb: adb.clone(),
                cfg,
                out: out.clone(),
                status,
                held: held.clone(),
                children,
                state_dir,
                found: HashMap::new(),
                devices: HashMap::new(),
                names: HashMap::new(),
            }
            .run(),
        );
        AndroidWatch {
            task,
            adb,
            held,
            out,
        }
    }

    /// Stops the watcher, removes its forwards in parallel until `deadline`, and empties its
    /// set. A forward whose removal failed or ran out of time stays in the record, for the next
    /// run to remove. Safe to call more than once.
    pub(crate) fn shutdown(&self, deadline: std::time::Instant) {
        // Aborting drops the followers and the track child (all `kill_on_drop`).
        self.task.abort();
        let held = self.held.snapshot();
        if !held.is_empty() {
            let failed = remove_forwards_blocking(&self.adb, &held, deadline);
            self.held.replace(failed);
        }
        self.out.send_replace(Arc::from(Vec::new()));
    }
}

impl Drop for AndroidWatch {
    fn drop(&mut self) {
        self.shutdown(std::time::Instant::now() + SHUTDOWN_CAP);
    }
}

/// Runs `adb forward --remove` for every forward at once and waits until all finished or
/// `deadline` passed. Returns the forwards not confirmed removed.
fn remove_forwards_blocking(
    adb: &Path,
    forwards: &[(String, u16)],
    deadline: std::time::Instant,
) -> Vec<(String, u16)> {
    let mut failed = Vec::new();
    let mut running = Vec::new();
    for (serial, port) in forwards {
        let spawned = std::process::Command::new(adb)
            .args(["-s", serial, "forward", "--remove", &format!("tcp:{port}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => running.push(((serial.clone(), *port), child)),
            Err(_) => failed.push((serial.clone(), *port)),
        }
    }
    while !running.is_empty() && std::time::Instant::now() < deadline {
        running.retain_mut(|(forward, child)| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if !status.success() {
                    failed.push(forward.clone());
                }
                false
            }
            Err(_) => {
                failed.push(forward.clone());
                false
            }
        });
        if !running.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    for (forward, mut child) in running {
        let _ = child.kill();
        let _ = child.wait();
        failed.push(forward);
    }
    failed
}

/// Per-device log followers, stopped when the device goes away.
struct DeviceTasks {
    _follower: Follower,
    pump: JoinHandle<()>,
    backfill: JoinHandle<()>,
}

impl Drop for DeviceTasks {
    fn drop(&mut self) {
        self.pump.abort();
        self.backfill.abort();
    }
}

struct Found {
    candidate: Candidate,
    /// The device-side URL the hit named, to ignore the same line seen twice.
    device_url: String,
    local_port: u16,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Watcher {
    handle: Handle,
    adb: PathBuf,
    cfg: WatchTiming,
    out: Arc<watch::Sender<Arc<[Candidate]>>>,
    status: Arc<watch::Sender<PlatformState>>,
    held: Arc<Held>,
    children: Children,
    /// Where forwards are recorded; earlier runs' records there are swept first.
    state_dir: Option<PathBuf>,
    found: HashMap<(String, u32), Found>,
    devices: HashMap<String, DeviceTasks>,
    names: HashMap<String, String>,
}

impl Watcher {
    async fn run(mut self) {
        if let Some(dir) = &self.state_dir {
            sweep_records(&self.adb, dir, &self.children).await;
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _track = AbortOnDrop(self.handle.spawn(track_devices(
            self.adb.clone(),
            tx.clone(),
            self.children.clone(),
        )));
        let mut tick = tokio::time::interval(self.cfg.liveness);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                Some(ev) = rx.recv() => match ev {
                    Ev::Frame(devices) => self.on_frame(devices, &tx).await,
                    Ev::TrackDown(why) => {
                        self.status.send_replace(PlatformState::Unavailable(
                            format!("adb stopped: {why}"),
                        ));
                    }
                    Ev::Line { serial, line } => self.on_line(&serial, &line).await,
                },
                _ = tick.tick() => self.check_liveness().await,
            }
        }
    }

    async fn on_frame(&mut self, devices: Vec<(String, String)>, tx: &mpsc::UnboundedSender<Ev>) {
        let live: HashSet<String> = devices
            .into_iter()
            .filter(|(_, state)| state == "device")
            .map(|(serial, _)| serial)
            .collect();
        self.status.send_replace(PlatformState::Watching {
            devices: live.len(),
        });
        if let Some(text) =
            run_once(&self.adb, &["devices", "-l"], ADB_TIMEOUT, &self.children).await
        {
            self.names.extend(parse_devices_l(&text));
        }
        let gone: Vec<String> = self
            .devices
            .keys()
            .filter(|s| !live.contains(*s))
            .cloned()
            .collect();
        for serial in gone {
            self.devices.remove(&serial);
            let keys: Vec<_> = self
                .found
                .keys()
                .filter(|(s, _)| *s == serial)
                .cloned()
                .collect();
            for key in keys {
                self.found.remove(&key);
            }
            // adb drops a detached device's forwards itself, and a `--remove` would fail with
            // "device not found" and be retried every tick, so just forget them.
            self.held.retain(|(s, _)| *s != serial);
        }
        for serial in live {
            if !self.devices.contains_key(&serial) {
                let tasks = self.start_device(&serial, tx);
                self.devices.insert(serial, tasks);
            }
        }
        self.publish();
    }

    fn start_device(&self, serial: &str, tx: &mpsc::UnboundedSender<Ev>) -> DeviceTasks {
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let follower = follow(
            &self.handle,
            self.adb.clone(),
            follow_args(serial),
            Backoff::default(),
            line_tx,
            self.children.clone(),
        );
        let pump = {
            let (tx, serial) = (tx.clone(), serial.to_string());
            self.handle.spawn(async move {
                while let Some(line) = line_rx.recv().await {
                    let ev = Ev::Line {
                        serial: serial.clone(),
                        line,
                    };
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
            })
        };
        let backfill = {
            let (tx, serial, adb) = (tx.clone(), serial.to_string(), self.adb.clone());
            let children = self.children.clone();
            self.handle.spawn(async move {
                let cmd = [
                    "-s",
                    &serial,
                    "logcat",
                    "-d",
                    "-v",
                    "brief",
                    "flutter:I",
                    "*:S",
                ];
                let Some(text) = run_once(&adb, &cmd, ADB_TIMEOUT, &children).await else {
                    return;
                };
                for line in text.lines() {
                    let ev = Ev::Line {
                        serial: serial.clone(),
                        line: line.to_string(),
                    };
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
            })
        };
        DeviceTasks {
            _follower: follower,
            pump,
            backfill,
        }
    }

    async fn on_line(&mut self, serial: &str, line: &str) {
        if !self.devices.contains_key(serial) {
            return;
        }
        let Some(hit) = android_hit(line) else { return };
        let Ok(url) = url::Url::parse(&hit.url) else {
            return;
        };
        let Some(device_port) = url.port() else {
            return;
        };
        let key = (serial.to_string(), hit.pid);
        if self
            .found
            .get(&key)
            .is_some_and(|f| f.device_url == hit.url)
        {
            return;
        }
        let spec = format!("tcp:{device_port}");
        // A `forward tcp:0` that times out, is cancelled, or prints something unparsable may leave
        // a forward we have no port for, so we cannot track or remove it. We accept leaking that
        // one forward rather than listing `forward --list` and guessing, which could remove
        // forwards other tools own.
        let Some(printed) = run_once(
            &self.adb,
            &["-s", serial, "forward", "tcp:0", &spec],
            ADB_TIMEOUT,
            &self.children,
        )
        .await
        else {
            return;
        };
        let Ok(local) = printed.trim().parse::<u16>() else {
            return;
        };
        self.held.push(serial, local);
        if !tcp_alive(local, self.cfg.alive_wait).await {
            self.remove_forward(serial, local).await;
            return;
        }
        let candidate = Candidate {
            platform: Platform::Android,
            device_id: serial.to_string(),
            device_name: self
                .names
                .get(serial)
                .cloned()
                .unwrap_or_else(|| serial.to_string()),
            pid: hit.pid,
            vm_uri: format!("http://127.0.0.1:{local}{}", url.path()),
        };
        let old = self.found.insert(
            key,
            Found {
                candidate,
                device_url: hit.url,
                local_port: local,
            },
        );
        if let Some(old) = old {
            self.remove_forward(serial, old.local_port).await;
        }
        self.publish();
    }

    async fn check_liveness(&mut self) {
        let wait = self.cfg.alive_wait;
        let checks = self
            .found
            .iter()
            .map(|(key, f)| (key.clone(), f.local_port))
            .map(|(key, port)| async move { (key, tcp_alive(port, wait).await) });
        let dead: Vec<_> = futures_util::future::join_all(checks)
            .await
            .into_iter()
            .filter(|(_, alive)| !alive)
            .map(|(key, _)| key)
            .collect();
        for key in dead {
            if let Some(f) = self.found.remove(&key) {
                self.remove_forward(&key.0, f.local_port).await;
            }
        }
        self.publish();
        // Forwards whose removal failed earlier stay in `held`; try them again.
        let ports: HashSet<u16> = self.found.values().map(|f| f.local_port).collect();
        let orphans: Vec<(String, u16)> = self
            .held
            .snapshot()
            .into_iter()
            .filter(|(_, p)| !ports.contains(p))
            .collect();
        for (serial, port) in orphans {
            self.remove_forward(&serial, port).await;
        }
    }

    async fn remove_forward(&self, serial: &str, port: u16) {
        let spec = format!("tcp:{port}");
        let removed = run_once(
            &self.adb,
            &["-s", serial, "forward", "--remove", &spec],
            REMOVE_TIMEOUT,
            &self.children,
        )
        .await;
        // `held` is the record of live forwards: it only forgets one once adb confirmed the
        // removal, so a failed removal is retried on the next tick and at `Drop`.
        if removed.is_some() {
            self.held.remove(serial, port);
        }
    }

    fn publish(&self) {
        let mut all: Vec<Candidate> = self.found.values().map(|f| f.candidate.clone()).collect();
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

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Runs `adb track-devices`, sending each frame's device list. Restarts with backoff, and tells
/// the watcher when the child could not start or ended. Stops once `children` was terminated.
async fn track_devices(adb: PathBuf, tx: mpsc::UnboundedSender<Ev>, children: Children) {
    let backoff = Backoff::default();
    let mut delay = backoff.first;
    loop {
        let started = Instant::now();
        let why = track_once(&adb, &tx, &children).await;
        if children.is_stopped() || tx.send(Ev::TrackDown(why)).is_err() {
            return;
        }
        if started.elapsed() >= STABLE_RUN {
            delay = backoff.first;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(backoff.max);
    }
}

async fn track_once(adb: &Path, tx: &mpsc::UnboundedSender<Ev>, children: &Children) -> String {
    let mut cmd = tokio::process::Command::new(adb);
    cmd.arg("track-devices")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let (mut child, _tracked) = match spawn_tracked(&mut cmd, children).await {
        Ok(Some(started)) => started,
        Ok(None) => return "stopping".to_string(),
        Err(e) => return format!("could not start adb ({e})"),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return "no output".to_string();
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        for frame in parse_track_frames(&mut buf) {
            if tx.send(Ev::Frame(frame)).is_err() {
                return String::new();
            }
        }
    }
    let _ = child.wait().await;
    "track-devices exited".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn track_frames_are_split_and_partials_kept() {
        let mut buf = b"0015emulator-5554\tdevice\n0000".to_vec();
        assert_eq!(
            parse_track_frames(&mut buf),
            vec![
                vec![("emulator-5554".to_string(), "device".to_string())],
                vec![]
            ]
        );
        assert!(buf.is_empty());
        let mut buf = b"0015emulator-5554\tdev".to_vec();
        assert!(parse_track_frames(&mut buf).is_empty());
        assert_eq!(buf, b"0015emulator-5554\tdev");
    }

    #[test]
    fn devices_l_names_are_cleaned() {
        let text = "List of devices attached\nemulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\nR5CT unauthorized usb:1-1 transport_id:2\nplain device\n";
        assert_eq!(
            parse_devices_l(text),
            vec![
                (
                    "emulator-5554".to_string(),
                    "sdk gphone64 arm64".to_string()
                ),
                ("plain".to_string(), "plain".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn tcp_alive_tells_open_from_closed() {
        let wait = Duration::from_millis(100);
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = holder.local_addr().unwrap().port();
        let hold = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = holder.accept().await {
                keep.push(s);
            }
        });
        assert!(tcp_alive(port, wait).await);
        hold.abort();

        let dropper = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dropper.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((s, _)) = dropper.accept().await {
                drop(s);
            }
        });
        assert!(!tcp_alive(port, wait).await);

        let unused = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(!tcp_alive(unused, wait).await);
    }

    #[test]
    fn watcher_argv_must_match_exactly() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let logcat = [
            "-s",
            "emulator-5554",
            "logcat",
            "-v",
            "brief",
            "-T",
            "1",
            "flutter:I",
            "*:S",
        ];
        let with = |exe: &str, rest: &[&str]| {
            let mut a = vec![exe];
            a.extend_from_slice(rest);
            v(&a)
        };
        assert!(is_watcher_argv(&v(&[
            "/opt/homebrew/bin/adb",
            "track-devices"
        ])));
        assert!(is_watcher_argv(&v(&["adb", "track-devices"])));
        assert!(is_watcher_argv(&with("/sdk/platform-tools/adb", &logcat)));
        // Another program, extra or missing arguments, or a one-shot command: never.
        assert!(!is_watcher_argv(&v(&["/usr/bin/fakeadb", "track-devices"])));
        assert!(!is_watcher_argv(&v(&["adb", "track-devices", "-l"])));
        assert!(!is_watcher_argv(&with("adb", &logcat[..8])));
        assert!(!is_watcher_argv(&with(
            "adb",
            &[&logcat[..], &["-d"]].concat()
        )));
        assert!(!is_watcher_argv(&v(&[
            "adb",
            "-s",
            "emulator-5554",
            "logcat",
            "-d",
            "-v",
            "brief",
            "flutter:I",
            "*:S"
        ])));
        assert!(!is_watcher_argv(&v(&["adb", "-s", "", "logcat"])));
        assert!(!is_watcher_argv(&v(&["adb", "logcat"])));
        assert!(!is_watcher_argv(&v(&["adb"])));
        assert!(!is_watcher_argv(&[]));
    }

    #[test]
    fn held_forwards_are_recorded_and_the_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = record_path(dir.path(), 4242);
        assert_eq!(path, dir.path().join("forwards-4242.json"));
        let held = Held::new(Some(path.clone()));
        held.push("emulator-5554", 50001);
        held.push("R5CT", 50002);
        assert_eq!(
            read_record(&path),
            Some(vec![
                ("emulator-5554".to_string(), 50001),
                ("R5CT".to_string(), 50002)
            ])
        );
        held.remove("R5CT", 50002);
        assert_eq!(
            read_record(&path),
            Some(vec![("emulator-5554".to_string(), 50001)])
        );
        held.retain(|(s, _)| s != "emulator-5554");
        assert!(!path.exists(), "an empty record is deleted");
        held.replace(vec![("X".to_string(), 1)]);
        assert_eq!(read_record(&path), Some(vec![("X".to_string(), 1)]));
        // Without a record path nothing is written.
        let unrecorded = Held::new(None);
        unrecorded.push("Y", 2);
        assert_eq!(unrecorded.snapshot(), vec![("Y".to_string(), 2)]);
    }

    #[cfg(unix)]
    mod fake {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        const LINE: &str =
            "I/flutter ( 4242): The Dart VM service is listening on http://127.0.0.1:40421/tok=/";

        fn fake_adb(dir: &Path, backfill: &str) -> PathBuf {
            let script = format!(
                r#"#!/bin/sh
DIR="{dir}"
case "$*" in
  "track-devices")
    if [ -e "$DIR/flaky" ] && [ ! -e "$DIR/started" ]; then touch "$DIR/started"; exit 1; fi
    printf '0015emulator-5554\tdevice\n'
    while [ ! -e "$DIR/leave" ]; do sleep 0.1; done
    printf '0000'
    exec sleep 30;;
  "devices -l") echo "emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1";;
  *"logcat -d"*) {backfill};;
  *"logcat -v brief -T 1"*)
    echo $$ > "$DIR/logcat_pid"
    if [ ! -e "$DIR/silent" ]; then echo "{LINE}"; fi
    if [ -e "$DIR/line2" ]; then sleep 1; cat "$DIR/line2"; fi
    exec sleep 30;;
  *"forward --remove"*)
    if [ -e "$DIR/remove_fails" ]; then exit 1; fi
    echo "$@" >> "$DIR/removed";;
  *"forward tcp:0 tcp:40422"*) cat "$DIR/port2";;
  *"forward tcp:0 tcp:"*) cat "$DIR/port";;
  "version") echo ok;;
esac
"#,
                dir = dir.display()
            );
            // Write under another name and rename, so no writer still holds the file open when it
            // is executed (ETXTBSY on Linux).
            let tmp = dir.join("adb.tmp");
            std::fs::write(&tmp, script).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = dir.join("adb");
            std::fs::rename(&tmp, &path).unwrap();
            path
        }

        fn timing() -> WatchTiming {
            WatchTiming {
                liveness: Duration::from_millis(200),
                alive_wait: Duration::from_millis(100),
                ..WatchTiming::default()
            }
        }

        type Channels = (
            Arc<watch::Sender<Arc<[Candidate]>>>,
            Arc<watch::Sender<PlatformState>>,
        );

        fn channels() -> Channels {
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

        #[tokio::test]
        async fn fake_adb_end_to_end() {
            let dir = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            std::fs::write(dir.path().join("port"), format!("{port}\n")).unwrap();
            let held = tokio::spawn(async move {
                let mut keep = Vec::new();
                while let Ok((s, _)) = listener.accept().await {
                    keep.push(s);
                }
            });
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let _watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status.clone(),
                Children::default(),
                None,
            );

            assert!(
                until(|| out.borrow().len() == 1, 3).await,
                "candidate appears"
            );
            let c = out.borrow()[0].clone();
            assert_eq!(c.device_name, "sdk gphone64 arm64");
            assert_eq!(c.pid, 4242);
            assert_eq!(c.device_id, "emulator-5554");
            assert_eq!(c.vm_uri, format!("http://127.0.0.1:{port}/tok=/"));
            assert_eq!(*status.borrow(), PlatformState::Watching { devices: 1 });

            // Closing the listener's side makes the forwarded port dead.
            held.abort();
            assert!(
                until(|| out.borrow().is_empty(), 3).await,
                "candidate leaves"
            );
            let removed = dir.path().join("removed");
            assert!(
                until(
                    || std::fs::read_to_string(&removed)
                        .is_ok_and(|t| t.contains(&format!("tcp:{port}"))),
                    3
                )
                .await
            );
        }

        #[tokio::test]
        async fn dropping_the_watch_removes_held_forwards() {
            let dir = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            std::fs::write(dir.path().join("port"), format!("{port}\n")).unwrap();
            let held = tokio::spawn(async move {
                let mut keep = Vec::new();
                while let Ok((s, _)) = listener.accept().await {
                    keep.push(s);
                }
            });
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status,
                Children::default(),
                None,
            );
            assert!(until(|| out.borrow().len() == 1, 3).await);
            drop(watch);
            assert!(out.borrow().is_empty());
            let removed = std::fs::read_to_string(dir.path().join("removed")).unwrap();
            assert!(removed.contains("-s emulator-5554 forward --remove"));
            assert!(removed.contains(&format!("tcp:{port}")));
            held.abort();
        }

        fn removed_has(dir: &Path, port: u16) -> bool {
            std::fs::read_to_string(dir.join("removed"))
                .is_ok_and(|t| t.contains(&format!("tcp:{port}")))
        }

        /// A listener that accepts and holds connections; aborting the task closes it.
        async fn holder() -> (u16, tokio::task::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                let mut keep = Vec::new();
                while let Ok((s, _)) = listener.accept().await {
                    keep.push(s);
                }
            });
            (port, task)
        }

        #[tokio::test]
        async fn dead_backfill_lines_leave_no_candidate_and_no_forward() {
            let dir = tempfile::tempdir().unwrap();
            let closed = {
                let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                l.local_addr().unwrap().port()
            };
            std::fs::write(dir.path().join("port"), format!("{closed}\n")).unwrap();
            // Only the backfill reports the line; the live follower stays silent.
            std::fs::write(dir.path().join("silent"), "").unwrap();
            let adb = fake_adb(dir.path(), &format!("echo \"{LINE}\""));
            let (out, status) = channels();
            let rx = out.subscribe();
            let watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status,
                Children::default(),
                None,
            );

            assert!(until(|| removed_has(dir.path(), closed), 3).await);
            // Keep watching: nothing may ever be published, and nothing stays forwarded.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(!rx.has_changed().unwrap(), "a candidate was published");
            assert!(out.borrow().is_empty());
            assert!(watch.held.snapshot().is_empty());
        }

        #[tokio::test]
        async fn device_leaving_removes_its_candidates_and_forwards() {
            let dir = tempfile::tempdir().unwrap();
            let (port, _task) = holder().await;
            std::fs::write(dir.path().join("port"), format!("{port}\n")).unwrap();
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status.clone(),
                Children::default(),
                None,
            );
            assert!(until(|| out.borrow().len() == 1, 3).await);
            let pid = std::fs::read_to_string(dir.path().join("logcat_pid"))
                .unwrap()
                .trim()
                .to_string();

            std::fs::write(dir.path().join("leave"), "").unwrap();
            assert!(
                until(|| out.borrow().is_empty(), 3).await,
                "candidate leaves"
            );
            assert_eq!(*status.borrow(), PlatformState::Watching { devices: 0 });
            assert!(watch.held.snapshot().is_empty());
            // adb drops a detached device's forwards itself: no remove must be run for it.
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert!(!removed_has(dir.path(), port), "forward --remove was run");
            // The logcat follower for that device is gone (`kill -0` fails for a dead process).
            let dead = || {
                !std::process::Command::new("kill")
                    .args(["-0", &pid])
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            };
            assert!(until(dead, 3).await, "follower stopped");
        }

        #[tokio::test]
        async fn new_url_for_the_same_pid_replaces_and_removes_the_old_forward() {
            let dir = tempfile::tempdir().unwrap();
            let (port1, _a) = holder().await;
            let (port2, _b) = holder().await;
            std::fs::write(dir.path().join("port"), format!("{port1}\n")).unwrap();
            std::fs::write(dir.path().join("port2"), format!("{port2}\n")).unwrap();
            let line2 = LINE.replace("40421/tok=", "40422/tok2=");
            std::fs::write(dir.path().join("line2"), format!("{line2}\n")).unwrap();
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status,
                Children::default(),
                None,
            );

            assert!(until(|| out.borrow().len() == 1, 3).await);
            assert_eq!(
                out.borrow()[0].vm_uri,
                format!("http://127.0.0.1:{port1}/tok=/")
            );
            let replaced = || {
                let cur = out.borrow();
                cur.len() == 1 && cur[0].vm_uri == format!("http://127.0.0.1:{port2}/tok2=/")
            };
            assert!(until(replaced, 4).await, "candidate replaced");
            assert!(until(|| removed_has(dir.path(), port1), 3).await);
            assert!(!removed_has(dir.path(), port2));
            assert_eq!(
                watch.held.snapshot(),
                vec![("emulator-5554".to_string(), port2)]
            );
        }

        #[tokio::test]
        async fn unavailable_recovers_to_watching() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("flaky"), "").unwrap();
            std::fs::write(dir.path().join("silent"), "").unwrap();
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let _watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out,
                status.clone(),
                Children::default(),
                None,
            );
            assert!(
                until(
                    || matches!(*status.borrow(), PlatformState::Unavailable(_)),
                    3
                )
                .await
            );
            assert!(
                until(
                    || *status.borrow() == PlatformState::Watching { devices: 1 },
                    4
                )
                .await,
                "recovers after the backoff"
            );
        }

        #[tokio::test]
        async fn missing_adb_reports_unavailable() {
            let (out, status) = channels();
            let _watch = AndroidWatch::spawn(
                &Handle::current(),
                PathBuf::from("/nonexistent/adb"),
                timing(),
                out.clone(),
                status.clone(),
                Children::default(),
                None,
            );
            assert!(
                until(
                    || matches!(*status.borrow(), PlatformState::Unavailable(_)),
                    2
                )
                .await
            );
            assert!(out.borrow().is_empty());
        }

        #[test]
        fn sweep_orphans_stops_only_orphaned_watchers() {
            use crate::devices::processes::FakeProcesses;
            use std::os::unix::process::ExitStatusExt;
            let spawn = || {
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap()
            };
            let (mut track, mut oneshot, mut other) = (spawn(), spawn(), spawn());
            let procs = FakeProcesses::default();
            let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            *procs.orphans.lock().unwrap() = vec![
                (track.id(), v(&["/sdk/adb", "track-devices"])),
                (
                    oneshot.id(),
                    v(&["/sdk/adb", "-s", "e", "logcat", "-d", "flutter:I", "*:S"]),
                ),
                (other.id(), v(&["/usr/bin/notadb", "track-devices"])),
            ];
            assert_eq!(sweep_orphans(&procs), vec![track.id()]);
            assert_eq!(track.wait().unwrap().signal(), Some(libc::SIGTERM));
            assert_eq!(oneshot.try_wait().unwrap(), None);
            assert_eq!(other.try_wait().unwrap(), None);
            for c in [&mut oneshot, &mut other] {
                c.kill().unwrap();
                c.wait().unwrap();
            }
        }

        #[tokio::test]
        async fn records_of_dead_runs_are_swept_and_others_kept() {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state");
            std::fs::create_dir_all(&state).unwrap();
            let dead = {
                let mut c = std::process::Command::new("true").spawn().unwrap();
                let pid = c.id();
                c.wait().unwrap();
                pid
            };
            let mut live = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            save_record(
                &record_path(&state, dead),
                &[("emulator-5554".into(), 61001), ("R5CT".into(), 61002)],
            );
            save_record(
                &record_path(&state, live.id()),
                &[("emulator-5554".into(), 61003)],
            );
            std::fs::write(dir.path().join("silent"), "").unwrap();
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let _watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out,
                status.clone(),
                Children::default(),
                Some(state.clone()),
            );
            assert!(
                until(
                    || *status.borrow() == PlatformState::Watching { devices: 1 },
                    3
                )
                .await
            );
            let removed = std::fs::read_to_string(dir.path().join("removed")).unwrap();
            assert!(removed.contains("-s emulator-5554 forward --remove tcp:61001"));
            assert!(removed.contains("-s R5CT forward --remove tcp:61002"));
            assert!(
                !removed.contains("61003"),
                "a live run's forward was removed"
            );
            assert!(!record_path(&state, dead).exists());
            assert!(record_path(&state, live.id()).exists());
            live.kill().unwrap();
            live.wait().unwrap();
        }

        #[test]
        fn forwards_are_removed_in_parallel_within_the_deadline() {
            let dir = tempfile::tempdir().unwrap();
            let slow = dir.path().join("slow-adb");
            let tmp = dir.path().join("slow.tmp");
            std::fs::write(&tmp, "#!/bin/sh\nexec sleep 5\n").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &slow).unwrap();
            let forwards: Vec<(String, u16)> = (1..=3).map(|p| ("e".to_string(), p)).collect();
            let start = std::time::Instant::now();
            let failed =
                remove_forwards_blocking(&slow, &forwards, start + Duration::from_millis(500));
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(500), "{took:?}");
            assert!(
                took < Duration::from_millis(1500),
                "not one after another: {took:?}"
            );
            assert_eq!(failed, forwards, "unconfirmed removals stay recorded");

            let adb = fake_adb(dir.path(), "true");
            let start = std::time::Instant::now();
            let failed = remove_forwards_blocking(&adb, &forwards, start + Duration::from_secs(5));
            assert!(failed.is_empty());
            assert!(start.elapsed() < Duration::from_secs(2));
            let removed = std::fs::read_to_string(dir.path().join("removed")).unwrap();
            assert_eq!(removed.lines().count(), 3);
        }

        #[tokio::test]
        async fn shutdown_keeps_unremoved_forwards_in_the_record() {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state");
            let (port, _task) = holder().await;
            std::fs::write(dir.path().join("port"), format!("{port}\n")).unwrap();
            let adb = fake_adb(dir.path(), "true");
            let (out, status) = channels();
            let watch = AndroidWatch::spawn(
                &Handle::current(),
                adb,
                timing(),
                out.clone(),
                status,
                Children::default(),
                Some(state.clone()),
            );
            assert!(until(|| out.borrow().len() == 1, 3).await);
            let record = record_path(&state, std::process::id());
            assert_eq!(
                read_record(&record),
                Some(vec![("emulator-5554".to_string(), port)])
            );
            // Removal now fails: the forward stays recorded for the next run.
            std::fs::write(dir.path().join("remove_fails"), "").unwrap();
            watch.shutdown(std::time::Instant::now() + Duration::from_secs(2));
            assert!(out.borrow().is_empty());
            assert_eq!(
                read_record(&record),
                Some(vec![("emulator-5554".to_string(), port)])
            );
            std::fs::remove_file(dir.path().join("remove_fails")).unwrap();
            drop(watch);
            assert!(!record.exists(), "removed at drop, and the record with it");
        }
    }
}
