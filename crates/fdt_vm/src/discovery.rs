//! Live discovery of running Flutter apps: watches the DTD registry that every `flutter run`
//! writes to, asks each DTD for its VM services, probes each VM once, and publishes the list.
//!
//! Only addresses found in the registry (or the DTD's own answers) are ever connected to.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;

use crate::dtd::{DtdClient, parse_app_label};
use crate::events::{StreamEvent, decode_notification};
use crate::rpc::{ConnectOptions, connect};
use crate::service::{Mode, VmService, detect_mode};

/// A failed probe or DTD connection is retried after this long.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How long a probe waits for the `flutterVersion` service to be announced.
const VERSION_WAIT: Duration = Duration::from_secs(1);

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

/// Runs `dart tooling-daemon --list --machine`. Used where the registry directory is unknown.
pub fn list_via_cli(dart: &Path) -> Vec<DtdEntry> {
    let Ok(out) = std::process::Command::new(dart)
        .args(["tooling-daemon", "--list", "--machine"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&out.stdout) else {
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
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub interval: Duration,
    pub probe_timeout: Duration,
    /// Overrides [`registry_dir`] (tests use a temp dir).
    pub registry_dir: Option<PathBuf>,
    /// The `dart` executable, used only when no registry directory is known.
    pub dart_cli: Option<PathBuf>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        DiscoveryConfig {
            interval: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(2),
            registry_dir: None,
            dart_cli: None,
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
    let url = url::Url::parse(uri).map_err(|e| e.to_string())?;
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

struct Scanner {
    config: DiscoveryConfig,
    clients: HashMap<String, (DtdEntry, DtdClient)>,
    /// DTD connections that failed: retried after `RETRY_AFTER`.
    connect_failed: HashMap<String, Instant>,
    probes: HashMap<String, Result<ProbeInfo, Instant>>,
    cli_cache: (Option<Instant>, Vec<DtdEntry>),
}

async fn run(config: DiscoveryConfig, tx: watch::Sender<Arc<[DiscoveredApp]>>) {
    let mut s = Scanner {
        config,
        clients: HashMap::new(),
        connect_failed: HashMap::new(),
        probes: HashMap::new(),
        cli_cache: (None, Vec::new()),
    };
    let mut first = true;
    loop {
        let apps: Arc<[DiscoveredApp]> = Arc::from(s.scan().await);
        if first {
            first = false;
            tx.send_replace(apps);
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
        s.wait_for_next_tick().await;
    }
}

impl Scanner {
    /// Sleeps for the interval, but wakes early when a DTD connection closes.
    async fn wait_for_next_tick(&self) {
        let sleep = tokio::time::sleep(self.config.interval);
        if self.clients.is_empty() {
            sleep.await;
            return;
        }
        let closes = self.clients.values().map(|(_, c)| Box::pin(c.closed()));
        tokio::select! {
            _ = sleep => {}
            _ = futures_util::future::select_all(closes) => {}
        }
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
        let stale = self.cli_cache.0.is_none_or(|t| t.elapsed() >= RETRY_AFTER);
        if stale {
            let list = tokio::task::spawn_blocking(move || list_via_cli(&dart))
                .await
                .unwrap_or_default();
            self.cli_cache = (Some(Instant::now()), list);
        }
        self.cli_cache.1.clone()
    }

    async fn scan(&mut self) -> Vec<DiscoveredApp> {
        let entries: Vec<DtdEntry> = self
            .entries()
            .await
            .into_iter()
            .filter(|e| pid_alive(e.pid))
            .collect();
        let live: HashSet<&str> = entries.iter().map(|e| e.ws_uri.as_str()).collect();
        // A DTD that closed means its app quit; its registry file may linger until the pid dies,
        // so it is not reconnected to for a while.
        let failed = &mut self.connect_failed;
        self.clients.retain(|k, (_, c)| {
            let keep = live.contains(k.as_str()) && !c.is_closed();
            if !keep {
                failed.insert(k.clone(), Instant::now());
            }
            keep
        });
        self.connect_failed.retain(|k, _| live.contains(k.as_str()));

        for e in &entries {
            if self.clients.contains_key(&e.ws_uri) {
                continue;
            }
            if self
                .connect_failed
                .get(&e.ws_uri)
                .is_some_and(|t| t.elapsed() < RETRY_AFTER)
            {
                continue;
            }
            let attempt = async {
                let url = url::Url::parse(&e.ws_uri).ok()?;
                DtdClient::connect(&url).await.ok()
            };
            match tokio::time::timeout(self.config.probe_timeout, attempt).await {
                Ok(Some(c)) => {
                    self.connect_failed.remove(&e.ws_uri);
                    self.clients.insert(e.ws_uri.clone(), (e.clone(), c));
                }
                _ => {
                    self.connect_failed.insert(e.ws_uri.clone(), Instant::now());
                }
            }
        }

        // One getVmServices per DTD; a failing DTD is dropped and reconnected later.
        let timeout = self.config.probe_timeout;
        let mut listed: Vec<(DtdEntry, Vec<crate::dtd::VmServiceEntry>)> = Vec::new();
        let mut broken = Vec::new();
        for (ws, (entry, client)) in &self.clients {
            match tokio::time::timeout(timeout, client.get_vm_services()).await {
                Ok(Ok(v)) => listed.push((entry.clone(), v)),
                _ => broken.push(ws.clone()),
            }
        }
        for ws in broken {
            self.clients.remove(&ws);
        }
        listed.sort_by_key(|a| std::cmp::Reverse(a.0.epoch));

        // Probe uncached VMs concurrently.
        let seen: HashSet<&str> = listed
            .iter()
            .flat_map(|(_, v)| v.iter().map(|s| s.uri.as_str()))
            .collect();
        self.probes.retain(|k, _| seen.contains(k.as_str()));
        let mut todo: Vec<String> = Vec::new();
        for uri in &seen {
            let needs = match self.probes.get(*uri) {
                None => true,
                Some(Ok(_)) => false,
                Some(Err(at)) => at.elapsed() >= RETRY_AFTER,
            };
            if needs && !todo.iter().any(|t| t == uri) {
                todo.push(uri.to_string());
            }
        }
        let results = futures_util::future::join_all(todo.iter().map(|u| probe(u, timeout))).await;
        for (uri, r) in todo.into_iter().zip(results) {
            match r {
                Ok(info) => {
                    self.probes.insert(uri, Ok(info));
                }
                Err(e) => {
                    log::debug!("probe of a VM service failed: {e}");
                    self.probes.insert(uri, Err(Instant::now()));
                }
            }
        }

        let mut apps = Vec::new();
        for (entry, services) in &listed {
            for svc in services {
                let Some(Ok(info)) = self.probes.get(&svc.uri) else {
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
                        workspace: entry.workspace_root.clone().unwrap_or_default(),
                    },
                    vm_uri: svc.uri.clone(),
                    name,
                    device,
                    mode: Some(info.mode),
                    flutter_version: info.flutter_version.clone(),
                    workspace: entry.workspace_root.clone(),
                    pid: info.pid,
                });
            }
        }
        apps.sort_by(|a, b| (&a.name, &a.device, &a.vm_uri).cmp(&(&b.name, &b.device, &b.vm_uri)));
        apps.dedup_by(|a, b| a.vm_uri == b.vm_uri);
        apps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedServer;
    use serde_json::json;
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
        let server = Arc::new(ScriptedServer::start(|method, _| match method {
            "getVM" => Ok(json!({"pid": 4242, "operatingSystem": "macos",
                "isolates": [{"id": "isolates/1", "name": "main", "isSystemIsolate": false}]})),
            "getIsolate" => Ok(json!({"id": "isolates/1", "name": "main",
                "extensionRPCs": ["ext.flutter.reassemble"],
                "rootLib": {"uri": "package:jank_lab/main.dart"}})),
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
        let uri = vm.ws_uri().to_string();
        ScriptedServer::start(move |method, _| match method {
            "ConnectedApp.getVmServices" => {
                Ok(json!({"type": "VmServicesResponse", "vmServices": [
                {"uri": uri, "name": "Kind: Flutter - Device: macOS - Package: jank_lab"}]}))
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
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(vm.open_connections(), 0);
        // And the VM was probed exactly once despite several ticks.
        assert_eq!(
            vm.received().iter().filter(|(m, _)| m == "getVM").count(),
            1
        );

        dtd.close_abruptly();
        let apps = next(&mut rx, 200).await;
        assert!(apps.is_empty());
    }

    #[tokio::test]
    async fn first_scan_publishes_even_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (_disc, mut rx) =
            Discovery::spawn(&tokio::runtime::Handle::current(), config(dir.path()));
        let apps = next(&mut rx, 1000).await;
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
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(dtd.open_connections(), 0);
    }
}
