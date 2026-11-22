//! Live discovery of running Flutter apps: watches the DTD registry that every `flutter run`
//! writes to, asks each DTD for its VM services, probes each VM once, and publishes the list.
//!
//! Only addresses found in the registry (or the DTD's own answers) are ever connected to.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use serde_json::Value;
use tokio::sync::watch;

use crate::devices::Candidate;
use crate::dtd::{DtdClient, VmServiceEntry, parse_app_label};
use crate::events::{StreamEvent, decode_notification};
use crate::rpc::{ConnectOptions, connect};
use crate::service::{Mode, VmService, detect_mode};
use crate::uri::VmServiceUri;

/// Default for `DiscoveryConfig::retry_after`.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How long a probe waits for the `flutterVersion` service to be announced.
const VERSION_WAIT: Duration = Duration::from_secs(1);
/// Default for `DiscoveryConfig::candidate_grace`.
const CANDIDATE_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// Where the Dart tooling daemon writes its registry files. Pure: the environment and home
/// directory are passed in.
pub fn registry_dir_for(
    os: Os,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(d) = set("DART_DATA_HOME") {
        return Some(PathBuf::from(d).join("dtd"));
    }
    match os {
        Os::MacOs => Some(
            home?
                .join("Library")
                .join("Application Support")
                .join("Dart")
                .join("dtd"),
        ),
        Os::Windows => {
            let base = match set("LOCALAPPDATA") {
                Some(l) => PathBuf::from(l),
                None => home?.join("AppData").join("Local"),
            };
            Some(base.join("Dart").join("dtd"))
        }
        Os::Linux => {
            let base = match set("XDG_STATE_HOME") {
                Some(s) => PathBuf::from(s),
                None => home?.join(".local").join("state"),
            };
            Some(base.join("Dart").join("dtd"))
        }
    }
}

pub fn registry_dir() -> Option<PathBuf> {
    registry_dir_for(Os::current(), &|k| std::env::var(k).ok(), dirs::home_dir())
}

/// One DTD, as written to the registry (or printed by `dart tooling-daemon --list --machine`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtdEntry {
    pub ws_uri: String,
    pub epoch: i64,
    /// The pid of the process hosting the DTD (the DDS process), not the app's.
    pub pid: u32,
    pub dart_version: Option<String>,
    pub workspace_root: Option<String>,
}

fn entry_from_value(v: &Value) -> Option<DtdEntry> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(DtdEntry {
        ws_uri: s("wsUri")?,
        epoch: v.get("epoch").and_then(Value::as_i64).unwrap_or(0),
        pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
        dart_version: s("dartVersion"),
        workspace_root: s("workspaceRoot"),
    })
}

/// Reads every registry file (names made of digits), skipping unreadable and partial ones.
/// Newest `epoch` first.
pub fn read_registry(dir: &Path) -> Vec<DtdEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<DtdEntry> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            entry_from_value(&serde_json::from_str::<Value>(&text).ok()?)
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.epoch));
    out
}

/// How long `dart tooling-daemon --list` may run before it is killed.
const CLI_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `dart tooling-daemon --list --machine`. Used where the registry directory is unknown.
/// The child is killed if it takes longer than five seconds.
pub fn list_via_cli(dart: &Path) -> Vec<DtdEntry> {
    use std::io::Read;
    let Ok(mut child) = std::process::Command::new(dart)
        .args(["tooling-daemon", "--list", "--machine"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let out = rx.recv_timeout(CLI_TIMEOUT);
    if out.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let Ok(out) = out else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&out) else {
        return Vec::new();
    };
    let mut v: Vec<DtdEntry> = items.iter().filter_map(entry_from_value).collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.epoch));
    v
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // kill(0, _) would signal the whole process group.
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // Signal 0 checks existence only; nothing is sent.
    // SAFETY: plain syscall with no pointers.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: the handle is opened, queried and closed here; `code` outlives the call.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE.0 as u32
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Identifies "the same app" across restarts, so a capture can reconnect when it reappears.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey {
    pub package: String,
    pub device: String,
    pub workspace: String,
}

impl AppKey {
    /// Whether two keys name the same app, launched by different means: package and device are
    /// equal, and the workspaces are equal or either is unknown (empty).
    pub fn same_app(&self, other: &AppKey) -> bool {
        self.package == other.package
            && self.device == other.device
            && (self.workspace == other.workspace
                || self.workspace.is_empty()
                || other.workspace.is_empty())
    }
}

/// How a discovered app was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AppSource {
    /// Listed by a Dart tooling daemon (the app runs under `flutter run` or similar).
    #[default]
    Dtd,
    /// Found in a device log, with no tooling daemon involved.
    Device,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredApp {
    pub key: AppKey,
    pub vm_uri: String,
    pub name: String,
    pub device: String,
    pub mode: Option<Mode>,
    pub flutter_version: Option<String>,
    pub workspace: Option<String>,
    /// The app's own pid, from `getVM`.
    pub pid: Option<u32>,
    pub source: AppSource,
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub interval: Duration,
    pub probe_timeout: Duration,
    /// Overrides [`registry_dir`] (tests use a temp dir).
    pub registry_dir: Option<PathBuf>,
    /// The `dart` executable, used only when no registry directory is known.
    pub dart_cli: Option<PathBuf>,
    /// A failed probe, a failed DTD or a closed DTD is not retried for this long.
    pub retry_after: Duration,
    /// Apps found in device logs, as published by `DeviceWatch`. `None` lists none.
    pub candidates: Option<watch::Receiver<Arc<[Candidate]>>>,
    /// A candidate is first contacted this long after it was first seen, so the Flutter tool's
    /// own connection (and DTD listing) gets there first.
    pub candidate_grace: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        DiscoveryConfig {
            interval: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(2),
            registry_dir: None,
            dart_cli: None,
            retry_after: RETRY_AFTER,
            candidates: None,
            candidate_grace: CANDIDATE_GRACE,
        }
    }
}

#[derive(Clone, Debug)]
struct ProbeInfo {
    pid: Option<u32>,
    os: String,
    mode: Mode,
    package: Option<String>,
    flutter_version: Option<String>,
}

fn package_of(root_lib_uri: &str) -> Option<String> {
    let rest = root_lib_uri.strip_prefix("package:")?;
    let name = rest.split('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Connects once, reads what identifies the app, and drops the connection.
async fn probe(uri: &str, timeout: Duration) -> Result<ProbeInfo, String> {
    tokio::time::timeout(timeout, probe_inner(uri))
        .await
        .map_err(|_| "probe timed out".to_string())?
}

async fn probe_inner(uri: &str) -> Result<ProbeInfo, String> {
    // Candidates come as `http://` URLs; the socket needs the `ws://.../ws` form.
    let url = match VmServiceUri::parse(uri) {
        Ok(u) => u.ws_url().clone(),
        Err(_) => url::Url::parse(uri).map_err(|e| e.to_string())?,
    };
    let (client, mut notes) = connect(&url, ConnectOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    let vm = VmService::new(client);
    let info = vm.get_vm().await.map_err(|e| e.to_string())?;
    let main = info
        .isolates
        .iter()
        .find(|i| i.name == "main" && !i.is_system_isolate)
        .ok_or("no main isolate yet")?;
    let isolate = vm.get_isolate(&main.id).await.map_err(|e| e.to_string())?;
    vm.stream_listen("Service")
        .await
        .map_err(|e| e.to_string())?;
    let method = tokio::time::timeout(VERSION_WAIT, async {
        while let Some(n) = notes.recv().await {
            if let Some(StreamEvent::ServiceRegistered { method, .. }) = decode_notification(&n)
                && method.ends_with("flutterVersion")
            {
                return Some(method);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let flutter_version = match method {
        Some(m) => vm
            .flutter_version(&m, &main.id)
            .await
            .ok()
            .and_then(|v| v.flutter_version.or(v.framework_version)),
        None => None,
    };
    Ok(ProbeInfo {
        pid: info.pid,
        os: info.operating_system.clone(),
        mode: detect_mode(&isolate),
        package: isolate.root_lib.as_ref().and_then(|l| package_of(&l.uri)),
        flutter_version,
    })
}

/// Handle to the background scan. Dropping it stops the scan and closes its connections.
pub struct Discovery {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Discovery {
    /// The receiver holds an empty list until the first scan finishes, which always publishes
    /// (even when nothing is found). Later scans publish only when the list changed.
    pub fn spawn(
        handle: &tokio::runtime::Handle,
        config: DiscoveryConfig,
    ) -> (Discovery, watch::Receiver<Arc<[DiscoveredApp]>>) {
        let (tx, rx) = watch::channel::<Arc<[DiscoveredApp]>>(Arc::from(Vec::new()));
        let task = handle.spawn(run(config, tx));
        (Discovery { task }, rx)
    }
}

enum Event {
    Connected(DtdEntry, Option<DtdClient>),
    Listed(String, Option<Vec<VmServiceEntry>>),
    Probed(String, Result<ProbeInfo, String>),
}

struct Dtd {
    entry: DtdEntry,
    client: DtdClient,
    services: Vec<VmServiceEntry>,
    listing: bool,
}

enum ProbeState {
    Running,
    Done(ProbeInfo),
    Failed(Instant),
}

/// All state of the scan loop. Every network step (connect, list, probe) runs as its own task
/// in `tasks` and reports back through `events`, so a slow or hung peer never delays the
/// others, nor the publishing of what is already known. Dropping the scanner aborts the tasks.
struct Scanner {
    config: DiscoveryConfig,
    clients: HashMap<String, Dtd>,
    connecting: HashSet<String>,
    /// ws uris that must not be (re)connected to until the instant passes.
    backoff: HashMap<String, Instant>,
    probes: HashMap<String, ProbeState>,
    live: HashSet<String>,
    /// The latest candidate set, read each tick.
    candidates: Vec<Candidate>,
    /// When each candidate's `vm_uri` was first seen in the current stretch of the set.
    first_seen: HashMap<String, Instant>,
    cli_cache: (Option<Instant>, Vec<DtdEntry>),
    tasks: tokio::task::JoinSet<()>,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
}

async fn run(config: DiscoveryConfig, tx: watch::Sender<Arc<[DiscoveredApp]>>) {
    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let interval = config.interval;
    let mut s = Scanner {
        config,
        clients: HashMap::new(),
        connecting: HashSet::new(),
        backoff: HashMap::new(),
        probes: HashMap::new(),
        live: HashSet::new(),
        candidates: Vec::new(),
        first_seen: HashMap::new(),
        cli_cache: (None, Vec::new()),
        tasks: tokio::task::JoinSet::new(),
        events,
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = Instant::now();
    let mut first_published = false;
    loop {
        tokio::select! {
            _ = ticker.tick() => s.tick().await,
            Some(ev) = inbox.recv() => s.apply(ev),
            _ = s.any_closed() => {}
        }
        while s.tasks.try_join_next().is_some() {}
        s.prune_closed();
        let apps: Arc<[DiscoveredApp]> = Arc::from(s.apps());
        if !first_published {
            // The first scan publishes even an empty list, but waits for its in-flight work
            // (at most one interval) so the start page does not flash "none found".
            if s.idle() || started.elapsed() >= interval {
                first_published = true;
                tx.send_replace(apps);
            }
        } else {
            tx.send_if_modified(|cur| {
                if **cur == *apps {
                    false
                } else {
                    *cur = apps;
                    true
                }
            });
        }
    }
}

impl Scanner {
    /// Resolves when any DTD connection closes. Owns clones, so it does not borrow `self`.
    fn any_closed(&self) -> impl std::future::Future<Output = ()> + use<> {
        let clients: Vec<DtdClient> = self.clients.values().map(|d| d.client.clone()).collect();
        async move {
            if clients.is_empty() {
                std::future::pending::<()>().await;
            }
            futures_util::future::select_all(clients.iter().map(|c| Box::pin(c.closed()))).await;
        }
    }

    fn idle(&self) -> bool {
        self.connecting.is_empty()
            && self.clients.values().all(|d| !d.listing)
            && self
                .probes
                .values()
                .all(|p| !matches!(p, ProbeState::Running))
    }

    /// A DTD that closed means its app quit; its registry file may linger until the pid dies,
    /// so it is not reconnected to for `retry_after`.
    fn prune_closed(&mut self) {
        let until = Instant::now() + self.config.retry_after;
        let backoff = &mut self.backoff;
        self.clients.retain(|k, d| {
            let keep = !d.client.is_closed();
            if !keep {
                backoff.insert(k.clone(), until);
            }
            keep
        });
    }

    async fn entries(&mut self) -> Vec<DtdEntry> {
        let dir = self.config.registry_dir.clone().or_else(registry_dir);
        if let Some(dir) = dir {
            return tokio::task::spawn_blocking(move || read_registry(&dir))
                .await
                .unwrap_or_default();
        }
        let Some(dart) = self.config.dart_cli.clone() else {
            return Vec::new();
        };
        // Spawning a process every tick would be wasteful.
        let stale = self
            .cli_cache
            .0
            .is_none_or(|t| t.elapsed() >= self.config.retry_after);
        if stale {
            let list = tokio::task::spawn_blocking(move || list_via_cli(&dart))
                .await
                .unwrap_or_default();
            self.cli_cache = (Some(Instant::now()), list);
        }
        self.cli_cache.1.clone()
    }

    async fn tick(&mut self) {
        let entries: Vec<DtdEntry> = self
            .entries()
            .await
            .into_iter()
            .filter(|e| pid_alive(e.pid))
            .collect();
        self.live = entries.iter().map(|e| e.ws_uri.clone()).collect();
        let live = &self.live;
        self.clients.retain(|k, _| live.contains(k));
        self.backoff
            .retain(|k, until| live.contains(k) && *until > Instant::now());

        for e in &entries {
            if self.clients.contains_key(&e.ws_uri)
                || self.connecting.contains(&e.ws_uri)
                || self.backoff.contains_key(&e.ws_uri)
            {
                continue;
            }
            self.connecting.insert(e.ws_uri.clone());
            let (e, timeout, events) = (e.clone(), self.config.probe_timeout, self.events.clone());
            self.tasks.spawn(async move {
                let attempt = async {
                    let url = url::Url::parse(&e.ws_uri).ok()?;
                    DtdClient::connect(&url).await.ok()
                };
                let client = tokio::time::timeout(timeout, attempt).await.ok().flatten();
                let _ = events.send(Event::Connected(e, client));
            });
        }

        let ws_uris: Vec<String> = self.clients.keys().cloned().collect();
        for ws in ws_uris {
            self.spawn_list(&ws);
        }

        self.refresh_candidates();

        // Forget probe results for VMs no DTD lists and no candidate offers any more.
        let seen: HashSet<&str> = self
            .clients
            .values()
            .flat_map(|d| d.services.iter().map(|s| s.uri.as_str()))
            .chain(self.candidates.iter().map(|c| c.vm_uri.as_str()))
            .collect();
        self.probes.retain(|k, _| seen.contains(k.as_str()));
        self.spawn_probes();
    }

    /// Reads the latest candidate set, stamping new URIs and forgetting those that left (so one
    /// that returns starts a fresh grace period).
    fn refresh_candidates(&mut self) {
        let now = Instant::now();
        self.candidates = match &self.config.candidates {
            Some(rx) => rx.borrow().to_vec(),
            None => Vec::new(),
        };
        let current: HashSet<&str> = self.candidates.iter().map(|c| c.vm_uri.as_str()).collect();
        self.first_seen.retain(|k, _| current.contains(k.as_str()));
        for c in &self.candidates {
            self.first_seen.entry(c.vm_uri.clone()).or_insert(now);
        }
    }

    /// Whether a DTD-listed app with a finished probe has this candidate's pid and OS, meaning
    /// the Flutter tool owns it.
    fn claimed(&self, c: &Candidate) -> bool {
        self.clients
            .values()
            .flat_map(|d| d.services.iter())
            .any(|svc| {
                matches!(self.probes.get(&svc.uri),
                    Some(ProbeState::Done(i)) if i.pid == Some(c.pid) && i.os == c.platform.os())
            })
    }

    fn spawn_list(&mut self, ws: &str) {
        let Some(d) = self.clients.get_mut(ws) else {
            return;
        };
        if d.listing {
            return;
        }
        d.listing = true;
        let (client, ws, timeout, events) = (
            d.client.clone(),
            ws.to_string(),
            self.config.probe_timeout,
            self.events.clone(),
        );
        self.tasks.spawn(async move {
            let r = tokio::time::timeout(timeout, client.get_vm_services())
                .await
                .ok()
                .and_then(Result::ok);
            let _ = events.send(Event::Listed(ws, r));
        });
    }

    fn spawn_probes(&mut self) {
        let now = Instant::now();
        let mut todo: Vec<String> = Vec::new();
        let mut offered: Vec<&str> = self
            .clients
            .values()
            .flat_map(|d| d.services.iter().map(|s| s.uri.as_str()))
            .collect();
        for c in &self.candidates {
            let due = self
                .first_seen
                .get(&c.vm_uri)
                .is_some_and(|t| now >= *t + self.config.candidate_grace);
            if due && !self.claimed(c) {
                offered.push(&c.vm_uri);
            }
        }
        for uri in offered {
            let needs = match self.probes.get(uri) {
                None => true,
                Some(ProbeState::Running | ProbeState::Done(_)) => false,
                Some(ProbeState::Failed(until)) => *until <= now,
            };
            if needs && !todo.iter().any(|u| u == uri) {
                todo.push(uri.to_string());
            }
        }
        for uri in todo {
            self.probes.insert(uri.clone(), ProbeState::Running);
            let (timeout, events) = (self.config.probe_timeout, self.events.clone());
            self.tasks.spawn(async move {
                let r = probe(&uri, timeout).await;
                let _ = events.send(Event::Probed(uri, r));
            });
        }
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Connected(entry, client) => {
                self.connecting.remove(&entry.ws_uri);
                let ws = entry.ws_uri.clone();
                match client {
                    Some(client) if self.live.contains(&ws) => {
                        self.clients.insert(
                            ws.clone(),
                            Dtd {
                                entry,
                                client,
                                services: Vec::new(),
                                listing: false,
                            },
                        );
                        self.spawn_list(&ws);
                    }
                    Some(_) => {}
                    None => {
                        self.backoff
                            .insert(ws, Instant::now() + self.config.retry_after);
                    }
                }
            }
            Event::Listed(ws, result) => match result {
                Some(services) => {
                    if let Some(d) = self.clients.get_mut(&ws) {
                        d.listing = false;
                        d.services = services;
                        self.spawn_probes();
                    }
                }
                None => {
                    if self.clients.remove(&ws).is_some() {
                        self.backoff
                            .insert(ws, Instant::now() + self.config.retry_after);
                    }
                }
            },
            Event::Probed(uri, result) => {
                let Some(slot) = self.probes.get_mut(&uri) else {
                    return;
                };
                *slot = match result {
                    Ok(info) => ProbeState::Done(info),
                    Err(e) => {
                        log::debug!("probe of a VM service failed: {e}");
                        ProbeState::Failed(Instant::now() + self.config.retry_after)
                    }
                };
            }
        }
    }

    fn apps(&self) -> Vec<DiscoveredApp> {
        let mut apps = Vec::new();
        for d in self.clients.values() {
            for svc in &d.services {
                let Some(ProbeState::Done(info)) = self.probes.get(&svc.uri) else {
                    continue;
                };
                let label = svc.name.as_deref().map(parse_app_label).unwrap_or_default();
                let name = label
                    .package
                    .clone()
                    .or_else(|| info.package.clone())
                    .unwrap_or_else(|| "Dart app".to_string());
                let device = label.device.clone().unwrap_or_else(|| info.os.clone());
                apps.push(DiscoveredApp {
                    key: AppKey {
                        package: name.clone(),
                        device: device.clone(),
                        workspace: d.entry.workspace_root.clone().unwrap_or_default(),
                    },
                    vm_uri: svc.uri.clone(),
                    name,
                    device,
                    mode: Some(info.mode),
                    flutter_version: info.flutter_version.clone(),
                    workspace: d.entry.workspace_root.clone(),
                    pid: info.pid,
                    source: AppSource::Dtd,
                });
            }
        }
        for c in &self.candidates {
            let Some(ProbeState::Done(info)) = self.probes.get(&c.vm_uri) else {
                continue;
            };
            if self.claimed(c) {
                continue;
            }
            let name = info
                .package
                .clone()
                .unwrap_or_else(|| "Dart app".to_string());
            apps.push(DiscoveredApp {
                key: AppKey {
                    package: name.clone(),
                    device: c.device_name.clone(),
                    workspace: String::new(),
                },
                vm_uri: c.vm_uri.clone(),
                name,
                device: c.device_name.clone(),
                mode: Some(info.mode),
                flutter_version: info.flutter_version.clone(),
                workspace: None,
                pid: Some(c.pid),
                source: AppSource::Device,
            });
        }
        apps.sort_by(|a, b| (&a.name, &a.device, &a.vm_uri).cmp(&(&b.name, &b.device, &b.vm_uri)));
        apps.dedup_by(|a, b| a.vm_uri == b.vm_uri);
        apps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Platform;
    use crate::testing::ScriptedServer;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::time::Duration;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn registry_dir_for_each_os() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            registry_dir_for(Os::MacOs, &env_of(&[]), home.clone()),
            Some(PathBuf::from(
                "/home/u/Library/Application Support/Dart/dtd"
            ))
        );
        assert_eq!(
            registry_dir_for(
                Os::Windows,
                &env_of(&[("LOCALAPPDATA", "C:\\Users\\u\\AppData\\Local")]),
                None
            ),
            Some(
                PathBuf::from("C:\\Users\\u\\AppData\\Local")
                    .join("Dart")
                    .join("dtd")
            )
        );
        assert_eq!(
            registry_dir_for(
                Os::Linux,
                &env_of(&[("XDG_STATE_HOME", "/x/state")]),
                home.clone()
            ),
            Some(PathBuf::from("/x/state/Dart/dtd"))
        );
        assert_eq!(
            registry_dir_for(Os::Linux, &env_of(&[]), home.clone()),
            Some(PathBuf::from("/home/u/.local/state/Dart/dtd"))
        );
        assert_eq!(registry_dir_for(Os::Linux, &env_of(&[]), None), None);
        for os in [Os::MacOs, Os::Windows, Os::Linux] {
            assert_eq!(
                registry_dir_for(
                    os,
                    &env_of(&[("DART_DATA_HOME", "/data"), ("XDG_STATE_HOME", "/x")]),
                    home.clone()
                ),
                Some(PathBuf::from("/data").join("dtd"))
            );
        }
    }

    #[test]
    fn read_registry_skips_garbage_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let w = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        w(
            "100",
            r#"{"wsUri":"ws://127.0.0.1:1/a","epoch":5,"pid":100,"dartVersion":"3.13.7","workspaceRoot":"/w"}"#,
        );
        w(
            "200",
            r#"{"wsUri":"ws://127.0.0.1:2/b","epoch":9,"pid":200}"#,
        );
        w("300", r#"{"wsUri":"ws://127.0.0.1:3/c","epoc"#); // partial write
        w("400", "not json");
        w(
            "notes.txt",
            r#"{"wsUri":"ws://127.0.0.1:4/d","epoch":1,"pid":4}"#,
        );
        w("500", r#"{"epoch":1,"pid":5}"#); // no wsUri
        let got = read_registry(dir.path());
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].pid, 200); // newest epoch first
        assert_eq!(got[1].workspace_root.as_deref(), Some("/w"));
        assert_eq!(got[1].dart_version.as_deref(), Some("3.13.7"));
        assert!(read_registry(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn dead_pids_are_ignored() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }

    fn vm_server() -> Arc<ScriptedServer> {
        vm_server_with("jank_lab", 0)
    }

    /// A VM whose first `fail_first` `getVM` calls fail.
    fn vm_server_with(package: &'static str, fail_first: usize) -> Arc<ScriptedServer> {
        vm_server_with_identity(package, fail_first, 4242, "macos")
    }

    /// Like `vm_server_with`, with the pid and operating system `getVM` reports.
    fn vm_server_with_identity(
        package: &'static str,
        fail_first: usize,
        pid: u32,
        os: &'static str,
    ) -> Arc<ScriptedServer> {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let server = Arc::new(ScriptedServer::start(move |method, _| match method {
            "getVM" => {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < fail_first {
                    return Err((-32000, "not ready".into()));
                }
                Ok(json!({"pid": pid, "operatingSystem": os,
                    "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]}))
            }
            "getIsolate" => Ok(json!({"id": "isolates/1", "name": "main",
                "extensionRPCs": ["ext.flutter.reassemble"],
                "rootLib": {"uri": format!("package:{package}/main.dart")}})),
            "streamListen" => Ok(json!({"type": "Success"})),
            "s1.flutterVersion" => Ok(json!({"flutterVersion": "3.47.9"})),
            _ => Err((-32601, "nope".into())),
        }));
        let s = server.clone();
        tokio::spawn(async move {
            loop {
                if s.received().iter().any(|(m, _)| m == "streamListen") {
                    s.notify(
                        "streamNotify",
                        json!({"streamId": "Service", "event": {"kind": "ServiceRegistered",
                            "timestamp": 1, "service": "flutterVersion", "method": "s1.flutterVersion"}}),
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        server
    }

    fn dtd_server(vm: &ScriptedServer) -> ScriptedServer {
        dtd_server_listing(Arc::new(std::sync::Mutex::new(vec![(
            vm.ws_uri().to_string(),
            "jank_lab".to_string(),
        )])))
    }

    /// A DTD that lists whatever `(uri, package)` pairs are in `vms` at the time of the call.
    fn dtd_server_listing(vms: Arc<std::sync::Mutex<Vec<(String, String)>>>) -> ScriptedServer {
        ScriptedServer::start(move |method, _| match method {
            "ConnectedApp.getVmServices" => {
                let list: Vec<Value> = vms
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(uri, pkg)| {
                        json!({"uri": uri, "name": format!("Kind: Flutter - Device: macOS - Package: {pkg}")})
                    })
                    .collect();
                Ok(json!({"type": "VmServicesResponse", "vmServices": list}))
            }
            _ => Err((-32601, "nope".into())),
        })
    }

    fn write_entry(dir: &Path, ws: &url::Url) {
        let body = json!({"wsUri": ws.to_string(), "epoch": 1, "pid": std::process::id(),
            "workspaceRoot": "/work/jank_lab"});
        std::fs::write(dir.join(std::process::id().to_string()), body.to_string()).unwrap();
    }

    fn config(dir: &Path) -> DiscoveryConfig {
        DiscoveryConfig {
            interval: Duration::from_millis(50),
            probe_timeout: Duration::from_secs(2),
            registry_dir: Some(dir.to_path_buf()),
            dart_cli: None,
            retry_after: Duration::from_secs(5),
            candidates: None,
            candidate_grace: Duration::from_millis(300),
        }
    }

    async fn next(rx: &mut watch::Receiver<Arc<[DiscoveredApp]>>, ms: u64) -> Arc<[DiscoveredApp]> {
        tokio::time::timeout(Duration::from_millis(ms), rx.changed())
            .await
            .expect("no publish in time")
            .unwrap();
        rx.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn discovery_finds_probes_and_drops_an_app() {
        let vm = vm_server();
        let dtd = dtd_server(&vm);
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let (_disc, mut rx) =
            Discovery::spawn(&tokio::runtime::Handle::current(), config(dir.path()));
        let apps = loop {
            let a = next(&mut rx, 5000).await;
            if !a.is_empty() {
                break a;
            }
        };
        assert_eq!(apps.len(), 1);
        let a = &apps[0];
        assert_eq!(a.name, "jank_lab");
        assert_eq!(a.device, "macOS");
        assert_eq!(a.mode, Some(Mode::Debug));
        assert_eq!(a.flutter_version.as_deref(), Some("3.47.9"));
        assert_eq!(a.workspace.as_deref(), Some("/work/jank_lab"));
        assert_eq!(a.pid, Some(4242));
        assert_eq!(a.key.package, "jank_lab");
        // The probe connection is not kept open.
        wait_until("probe connection closed", || vm.open_connections() == 0).await;
        // And the VM was probed exactly once despite several ticks.
        assert_eq!(
            vm.received().iter().filter(|(m, _)| m == "getVM").count(),
            1
        );

        dtd.close_abruptly();
        wait_for(&mut rx, 5000, "app gone", |a| a.is_empty()).await;
    }

    // Real time: the scan runs against real files. The negative check below can only pass
    // spuriously, never fail, however slow the machine is.
    #[tokio::test]
    async fn first_scan_publishes_even_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (_disc, mut rx) =
            Discovery::spawn(&tokio::runtime::Handle::current(), config(dir.path()));
        let apps = next(&mut rx, 5000).await;
        assert!(apps.is_empty());
        // Later identical scans do not publish.
        let again = tokio::time::timeout(Duration::from_millis(300), rx.changed()).await;
        assert!(again.is_err());
    }

    #[tokio::test]
    async fn dropping_discovery_closes_its_dtd_connection() {
        let vm = vm_server();
        let dtd = dtd_server(&vm);
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let (disc, mut rx) =
            Discovery::spawn(&tokio::runtime::Handle::current(), config(dir.path()));
        while next(&mut rx, 5000).await.is_empty() {}
        assert_eq!(dtd.open_connections(), 1);
        drop(disc);
        wait_until("DTD connection closed", || dtd.open_connections() == 0).await;
    }

    async fn wait_for(
        rx: &mut watch::Receiver<Arc<[DiscoveredApp]>>,
        ms: u64,
        what: &str,
        ok: impl Fn(&[DiscoveredApp]) -> bool,
    ) -> Arc<[DiscoveredApp]> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            let cur = rx.borrow_and_update().clone();
            if ok(&cur) {
                return cur;
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
                .unwrap();
        }
    }

    /// Polls `cond` until it holds or 5 s pass, for state that changes on another thread. The
    /// deadline is wall-clock on purpose: these tests run in real time.
    async fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn count(server: &ScriptedServer, method: &str) -> usize {
        server
            .received()
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }

    // Real time, because the VM is a real socket: a paused clock would jump ahead while the
    // runtime waits for it. Only monotonic facts are asserted: the retry cannot have happened
    // before `retry_after` passed, and it did happen.
    #[tokio::test]
    async fn probe_failure_is_retried_after_the_retry_interval() {
        let vm = vm_server_with("jank_lab", 1);
        let dtd = dtd_server(&vm);
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let mut cfg = config(dir.path());
        cfg.retry_after = Duration::from_millis(700);
        let started = std::time::Instant::now();
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        let apps = wait_for(&mut rx, 10_000, "app after retry", |a| a.len() == 1).await;
        let seen = std::time::Instant::now();
        assert_eq!(apps[0].name, "jank_lab");
        // The first probe failed, so the app could only appear after the retry interval.
        assert!(seen.duration_since(started) >= Duration::from_millis(700));
        assert_eq!(count(&vm, "getVM"), 2);
    }

    #[tokio::test]
    async fn app_leaves_when_its_uri_leaves_get_vm_services() {
        let vm = vm_server();
        let list = Arc::new(std::sync::Mutex::new(vec![(
            vm.ws_uri().to_string(),
            "jank_lab".to_string(),
        )]));
        let dtd = dtd_server_listing(list.clone());
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let (_disc, mut rx) =
            Discovery::spawn(&tokio::runtime::Handle::current(), config(dir.path()));
        wait_for(&mut rx, 3000, "app", |a| a.len() == 1).await;
        list.lock().unwrap().clear();
        wait_for(&mut rx, 5000, "app gone", |a| a.is_empty()).await;
    }

    #[tokio::test]
    async fn hung_vm_does_not_block_other_apps() {
        let hung = vm_server_with("hung_app", 0);
        hung.set_latency(Duration::from_secs(30));
        let ok = vm_server_with("ok_app", 0);
        let dtd = dtd_server_listing(Arc::new(std::sync::Mutex::new(vec![
            (hung.ws_uri().to_string(), "hung_app".to_string()),
            (ok.ws_uri().to_string(), "ok_app".to_string()),
        ])));
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let mut cfg = config(dir.path());
        cfg.probe_timeout = Duration::from_secs(10);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        let apps = wait_for(&mut rx, 5000, "healthy app", |a| !a.is_empty()).await;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "ok_app");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_dtd_does_not_block_other_apps() {
        let vm = vm_server();
        let ok_dtd = dtd_server(&vm);
        let hung_dtd = dtd_server(&vm);
        hung_dtd.set_latency(Duration::from_secs(30));
        let dir = tempfile::tempdir().unwrap();
        // Two registry files with live pids: this process and its parent.
        let body = |ws: &url::Url, epoch: i64, pid: u32| {
            json!({"wsUri": ws.to_string(), "epoch": epoch, "pid": pid}).to_string()
        };
        let me = std::process::id();
        let parent = std::os::unix::process::parent_id();
        std::fs::write(
            dir.path().join(me.to_string()),
            body(&hung_dtd.ws_uri(), 2, me),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(parent.to_string()),
            body(&ok_dtd.ws_uri(), 1, parent),
        )
        .unwrap();
        let mut cfg = config(dir.path());
        cfg.probe_timeout = Duration::from_secs(10);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        let apps = wait_for(&mut rx, 5000, "app via the healthy DTD", |a| !a.is_empty()).await;
        assert_eq!(apps[0].name, "jank_lab");
    }

    // Real time, for the same reason. The DTD is asked again only after `retry_after`, so the
    // second ask cannot come earlier than that, however many 50 ms ticks pass.
    #[tokio::test]
    async fn failing_dtd_is_backed_off() {
        let dtd = ScriptedServer::start(|_, _| Err((-32000, "broken".into())));
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let mut cfg = config(dir.path());
        cfg.retry_after = Duration::from_millis(600);
        let started = std::time::Instant::now();
        let (_disc, _rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        wait_until("a retry", || count(&dtd, "ConnectedApp.getVmServices") >= 2).await;
        assert!(started.elapsed() >= Duration::from_millis(600));
    }

    fn candidate(platform: Platform, pid: u32, vm: &ScriptedServer) -> Candidate {
        Candidate {
            platform,
            device_id: "dev-1".into(),
            device_name: "iPhone 17".into(),
            pid,
            vm_uri: vm.ws_uri().to_string(),
        }
    }

    fn publish(tx: &watch::Sender<Arc<[Candidate]>>, v: Vec<Candidate>) {
        tx.send_replace(Arc::from(v));
    }

    #[tokio::test]
    async fn device_candidate_is_listed_after_the_grace_period() {
        let vm = vm_server_with_identity("shop", 0, 77, "ios");
        let dir = tempfile::tempdir().unwrap();
        let (_tx, crx) = watch::channel::<Arc<[Candidate]>>(Arc::from(vec![candidate(
            Platform::IosSimulator,
            77,
            &vm,
        )]));
        let mut cfg = config(dir.path());
        cfg.candidates = Some(crx);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(rx.borrow().is_empty());
        assert_eq!(count(&vm, "getVM"), 0);
        let apps = wait_for(&mut rx, 1500, "candidate listed", |a| a.len() == 1).await;
        let a = &apps[0];
        assert_eq!(a.source, AppSource::Device);
        assert_eq!(a.name, "shop");
        assert_eq!(a.device, "iPhone 17");
        assert_eq!(a.workspace, None);
        assert_eq!(a.key.workspace, "");
        assert_eq!(a.pid, Some(77));
    }

    #[tokio::test]
    async fn candidate_claimed_by_a_dtd_app_is_never_contacted() {
        let dtd_vm = vm_server_with_identity("shop", 0, 77, "ios");
        let dtd = dtd_server(&dtd_vm);
        let cand_vm = vm_server_with_identity("shop", 0, 77, "ios");
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let (_tx, crx) = watch::channel::<Arc<[Candidate]>>(Arc::from(vec![candidate(
            Platform::IosSimulator,
            77,
            &cand_vm,
        )]));
        let mut cfg = config(dir.path());
        cfg.candidates = Some(crx);
        cfg.candidate_grace = Duration::from_millis(800);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        wait_for(&mut rx, 1500, "dtd app", |a| a.len() == 1).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let apps = rx.borrow().clone();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].source, AppSource::Dtd);
        assert_eq!(count(&cand_vm, "getVM"), 0);
    }

    #[tokio::test]
    async fn same_pid_on_another_os_is_not_claimed() {
        let dtd_vm = vm_server_with_identity("shop", 0, 77, "ios");
        let dtd = dtd_server(&dtd_vm);
        let cand_vm = vm_server_with_identity("other", 0, 77, "android");
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), &dtd.ws_uri());
        let (_tx, crx) = watch::channel::<Arc<[Candidate]>>(Arc::from(vec![candidate(
            Platform::Android,
            77,
            &cand_vm,
        )]));
        let mut cfg = config(dir.path());
        cfg.candidates = Some(crx);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        let apps = wait_for(&mut rx, 1500, "both listed", |a| a.len() == 2).await;
        assert!(apps.iter().any(|a| a.source == AppSource::Dtd));
        assert!(apps.iter().any(|a| a.source == AppSource::Device));
    }

    #[tokio::test]
    async fn candidate_leaves_when_its_set_drops_it() {
        let vm = vm_server_with_identity("shop", 0, 77, "ios");
        let dir = tempfile::tempdir().unwrap();
        let (tx, crx) = watch::channel::<Arc<[Candidate]>>(Arc::from(vec![candidate(
            Platform::IosSimulator,
            77,
            &vm,
        )]));
        let mut cfg = config(dir.path());
        cfg.candidates = Some(crx);
        let (_disc, mut rx) = Discovery::spawn(&tokio::runtime::Handle::current(), cfg);
        wait_for(&mut rx, 1500, "candidate listed", |a| a.len() == 1).await;
        publish(&tx, Vec::new());
        wait_for(&mut rx, 1500, "candidate gone", |a| a.is_empty()).await;
    }

    #[test]
    fn same_app_matches_across_launch_methods() {
        let k = |p: &str, d: &str, w: &str| AppKey {
            package: p.into(),
            device: d.into(),
            workspace: w.into(),
        };
        assert!(k("shop", "iPhone 17", "/w").same_app(&k("shop", "iPhone 17", "")));
        assert!(k("shop", "iPhone 17", "/w").same_app(&k("shop", "iPhone 17", "/w")));
        assert!(!k("shop", "iPhone 17", "/w").same_app(&k("shop", "iPhone 17", "/x")));
        assert!(!k("shop", "iPhone 17", "").same_app(&k("shop", "Pixel", "")));
    }
}
