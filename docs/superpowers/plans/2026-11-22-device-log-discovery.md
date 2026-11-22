# Device-log discovery (simulator and Android) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List Flutter apps opened on a booted iOS simulator or an Android device/emulator without the Flutter tool, on the Connect page, next to the apps DTD already finds; plus fix Return on a fresh Connect page.

**Architecture:** A new `fdt_vm::devices` module runs two watchers (simulator, Android). They follow device logs for the line "The Dart VM service is listening on <url>". They forward the port on Android, check that each app is still alive, and publish the current set of `Candidate`s on a `watch` channel. `Discovery` reads that channel, waits a 3 s grace period, and drops candidates the Flutter tool owns (same OS and pid as a DTD-listed app). It probes the rest with the existing probe and lists them with `AppSource::Device`. `fdt_app` starts the watcher (a Settings switch, on by default), shows "opened on device" rows, and updates the empty-state copy.

**Tech Stack:** Rust 2024, tokio (`process`, `net`, `time` features already on), the existing `regex` crate, GPUI via `gpui-kit` 0.10.0. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-22-device-log-discovery-design.md` (the design), which amends `docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md` §5.1 and §8.1. The iPhone watcher (design §6.2, spike S1) is **out of scope** for this plan.

## Global Constraints

- Marker text: `The Dart VM service is listening on ` followed by an `http://` URL (design F8). Nothing else counts as a hit.
- Candidate URLs are loopback only (`127.0.0.1` / `localhost`); a non-loopback URL from a log is ignored (design §6.7).
- Grace period before a candidate is first contacted: **3 s** (`candidate_grace`, design §6.4).
- Liveness checks every **2 s**; a log-found row leaves within about 5 s of its app quitting (design §6.4).
- Simulator liveness: `pid_alive(pid)` from `fdt_vm::discovery`. Android liveness: a TCP connect to the forwarded port that is **not closed within 400 ms** means alive (F3).
- Device lists are never refreshed by running a command on a timer (F11). Simulators: `stat` of `device.plist` files every 2 s, re-read only on mtime change (a builder choice the design allows, since reading files runs no process). Android: `adb track-devices`, with `adb devices -l` run only when it reports a change.
- Simulator follower command: `xcrun simctl spawn <udid> log stream --style ndjson --predicate 'eventMessage CONTAINS "Dart VM service is listening"'`. Backfill: the same with `log show --last 1h`.
- Android follower: `adb -s <serial> logcat -v brief -T 1 flutter:I *:S`. Backfill: `adb -s <serial> logcat -d -v brief flutter:I *:S`. Forward: `adb -s <serial> forward tcp:0 tcp:<device port>` (prints the local port). Remove: `adb -s <serial> forward --remove tcp:<local port>`.
- Android device names use Flutter's `cleanAdbDeviceName` on the `model:` field: `___` → `, `, `_` → space, collapse whitespace, trim (`sdk_gphone64_arm64` → `sdk gphone64 arm64`), so they match DTD's labels.
- URLs are only logged through `VmServiceUri::redacted()` or with the token path replaced by `<token>`.
- Every child process is started with `kill_on_drop(true)`; forwards are removed when their candidate goes, when the device goes, and on `Drop` of `DeviceWatch`.
- Settings key `find_device_apps`, default `true`.
- Copy (exact):
  - Empty state: "No running Flutter apps found. Start one with `flutter run`, or open a debug or profile build on a simulator or a connected device. Release builds can't be connected. Or paste a VM service URL below."
  - Empty state, extra line on macOS: "Apps on a connected iPhone appear only when started with `flutter run` or `flutter attach`."
  - Row folder column for device-log apps: "opened on device"
  - Settings switch: "Find apps opened on simulators and devices"; description: "Watches booted iOS simulators and Android devices for Flutter apps started without `flutter run`."
- Tests that spawn real processes use `sh -c` scripts and are `#[cfg(unix)]`; nothing in the default test run touches a real simulator, emulator or adb.

## Review Focus

1. **The `log` tool's own line matches the predicate.** `log stream` prints "log run noninteractively, … args: … 'eventMessage CONTAINS "Dart VM service is listening"'". It must never become a candidate: the parser requires `listening on http://` (Task 1 test `simulator_ignores_the_log_tools_own_invocation_line`).
2. **Old log history.** Backfill returns lines from apps that quit long ago. Simulator: dead pids are skipped before emitting. Android: a forward is kept only if its first TCP check says alive, and is removed otherwise (Task 4 `dead_backfill_lines_leave_no_candidate_and_no_forward`, Task 5 `stale_backfill_line_is_skipped`).
3. **`flutter run` on a simulator or emulator** prints the same line. The candidate must never appear as a second row: claimed by pid within the grace period, never probed (Task 6 `candidate_claimed_by_a_dtd_app_is_never_contacted`).
4. **adb or xcrun missing, or the follower process dies** (adb server restart, simulator shut down). The watcher reports `Unavailable`, or restarts the follower with backoff, and never spins (Task 3 `follower_restarts_with_backoff_and_stops_on_drop`, Task 4 `missing_adb_reports_unavailable`).
5. **Toggling the Settings switch off** must kill followers, remove every forward, and empty the list's device rows at once (Task 8 `switching_off_removes_device_rows`).

---

### Task 1: Log-line parsing and device-name cleaning

**Files:**
- Create: `crates/fdt_vm/src/devices/mod.rs` (module root: declares submodules and the shared types below)
- Create: `crates/fdt_vm/src/devices/marker.rs`
- Modify: `crates/fdt_vm/src/lib.rs` (add `pub mod devices;`)

**Interfaces:**
- Produces (in `devices/mod.rs`):
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub enum Platform { IosSimulator, Android }` with `pub fn os(self) -> &'static str` returning `"ios"` / `"android"` (the VM's `getVM.operatingSystem`).
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Candidate { pub platform: Platform, pub device_id: String, pub device_name: String, pub pid: u32, pub vm_uri: String }`. `vm_uri` is an `http://127.0.0.1:<port>/<token>/` URL reachable from this machine.
- Produces (in `devices/marker.rs`):
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Hit { pub pid: u32, pub url: String }`
  - `pub fn simulator_hit(ndjson_line: &str) -> Option<Hit>`: parses one `log … --style ndjson` line; takes `processID` and the URL after the marker in `eventMessage`.
  - `pub fn android_hit(brief_line: &str) -> Option<Hit>`: parses `I/flutter ( 9677): The Dart VM service is listening on http://127.0.0.1:40421/abc=/`.
  - `pub fn is_loopback_url(url: &str) -> bool`
  - `pub fn clean_adb_device_name(model: &str) -> String`

- [ ] **Step 1: Write the failing tests** in `marker.rs`:
  - `simulator_reads_pid_and_url`: the line `{"processID":62710,"processImagePath":"/…/Runner.app/Runner","eventMessage":"flutter: The Dart VM service is listening on http://127.0.0.1:61865/8JqAbc=/","timestamp":"2026-11-22 21:22:19.396005+0400"}` gives `Hit { pid: 62710, url: "http://127.0.0.1:61865/8JqAbc=/" }`.
  - `simulator_ignores_the_log_tools_own_invocation_line`: `{"processID":18471,"processImagePath":"/…/usr/bin/log","eventMessage":"log run noninteractively, parent: 61916 (launchd_sim), args: 'log' 'stream' '--predicate' 'eventMessage CONTAINS \"Dart VM service is listening\"'"}` gives `None`.
  - `simulator_ignores_non_json_and_other_messages`: `Filtering the log data using "…"` gives `None`; a JSON line whose `eventMessage` is `flutter: hello` gives `None`.
  - `android_reads_space_padded_pid`: `I/flutter ( 9677): The Dart VM service is listening on http://127.0.0.1:40421/AbC-_=/` gives `Hit { pid: 9677, url: "http://127.0.0.1:40421/AbC-_=/" }`; `I/flutter (12345): …` gives pid 12345.
  - `android_ignores_other_flutter_lines`: the Impeller line `I/flutter ( 9739): [IMPORTANT:flutter/shell/…] Using the Impeller rendering backend (OpenGLES).` gives `None`.
  - `only_loopback_urls_count`: `is_loopback_url` is true for `http://127.0.0.1:1/a=/` and `http://localhost:1/a=/`, false for `http://192.168.1.5:1/a=/`, and both `*_hit` functions return `None` when the URL is not loopback.
  - `adb_names_are_cleaned_like_flutter`: `sdk_gphone64_arm64` → `sdk gphone64 arm64`; `Pixel___Tablet_x` → `Pixel, Tablet x`; `  a__b ` → `a b`.

- [ ] **Step 2: Run them and see them fail**
  Run: `cargo test -p fdt_vm devices::marker`
  Expected: compile errors for the missing items.

- [ ] **Step 3: Implement** the types in `devices/mod.rs` and the five functions in `marker.rs`. Use one `regex` (compiled once with `std::sync::LazyLock`) for `The Dart VM service is listening on (http://\S+)`, and `serde_json` for the ndjson line.

- [ ] **Step 4: Run the tests and see them pass**
  Run: `cargo test -p fdt_vm devices::marker` → all pass. Then `cargo clippy -p fdt_vm --all-targets -- -D warnings` → clean.

- [ ] **Step 5: Commit**
  `git add crates/fdt_vm/src/lib.rs crates/fdt_vm/src/devices && git commit -m "feat(vm): parse VM service lines from simulator and Android logs"`

### Task 2: Finding adb and reading simulators

**Files:**
- Create: `crates/fdt_vm/src/devices/tools.rs`
- Create: `crates/fdt_vm/src/devices/simulators.rs`

**Interfaces:**
- Consumes: `fdt_vm::discovery::Os`.
- Produces:
  - `pub fn find_adb(os: Os, env: &dyn Fn(&str) -> Option<String>, home: Option<PathBuf>, exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf>`. Order: each `PATH` entry; `$ANDROID_HOME/platform-tools`; `$ANDROID_SDK_ROOT/platform-tools`; the default SDK (`~/Library/Android/sdk`, `%LOCALAPPDATA%\Android\Sdk` (falling back to `~/AppData/Local/Android/Sdk`), `~/Android/Sdk`). The file name is `adb.exe` on Windows, `adb` elsewhere. Empty variables are ignored.
  - `pub fn find_xcrun(exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf>`: `/usr/bin/xcrun` on macOS only.
  - `pub fn simulator_devices_dir(home: Option<PathBuf>) -> Option<PathBuf>`: `~/Library/Developer/CoreSimulator/Devices` on macOS.
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Simulator { pub udid: String, pub name: String, pub booted: bool }`
  - `pub fn parse_device_plist(xml: &str) -> Option<Simulator>`: reads the `UDID`, `name` and `state` keys of the XML plist (state 3 = booted, F12).
  - `pub struct SimulatorScan` with `pub fn new(dir: PathBuf) -> Self` and `pub fn booted(&mut self) -> Vec<Simulator>`. It `stat`s `<dir>/*/device.plist`, re-reads only files whose mtime or size changed since the last call, and returns booted simulators sorted by name then udid.

- [ ] **Step 1: Write the failing tests**
  - `adb_is_found_in_order` (tools.rs): with `exists` true for every candidate path, `PATH=/a:/b` gives `/a/adb`. With `exists` true only for `$ANDROID_SDK_ROOT/platform-tools/adb`, and `ANDROID_HOME` set but missing, gives the SDK_ROOT one. With nothing existing except `~/Library/Android/sdk/platform-tools/adb` on `Os::MacOs`, gives that. On `Os::Windows` the name is `adb.exe`.
  - `missing_adb_is_none`: `exists` always false gives `None`.
  - `device_plist_is_parsed` (simulators.rs): a copy of a real `device.plist` (XML with `<key>UDID</key><string>0B66…</string>`, `<key>name</key><string>iPhone 18 Pro</string>`, `<key>state</key><integer>3</integer>`, other keys between) gives `Some(Simulator { udid: "0B66…", name: "iPhone 18 Pro", booted: true })`; state 1 gives `booted: false`; garbage gives `None`.
  - `scan_rereads_only_changed_files`: in a `tempfile` dir with two device folders (one booted, one not), `booted()` returns one simulator. Rewrite the second with state 3 and a different file size (so the change shows even within the same mtime second), and `booted()` returns two. Delete the first folder, and `booted()` returns one.

- [ ] **Step 2: Run and see them fail**
  Run: `cargo test -p fdt_vm devices::tools devices::simulators` → compile errors.

- [ ] **Step 3: Implement** the functions. Parse the plist with a small `regex` over `<key>K</key>\s*<(string|integer)>V</…>`; no plist crate.

- [ ] **Step 4: Run and see them pass**
  Run: `cargo test -p fdt_vm devices::` → pass; clippy clean.

- [ ] **Step 5: Commit** `feat(vm): find adb and read booted simulators from their device files`

### Task 3: Child-process line follower

**Files:**
- Create: `crates/fdt_vm/src/devices/follow.rs`

**Interfaces:**
- Produces:
  - `pub struct Follower` (dropping it kills the child and stops restarting).
  - `pub fn follow(handle: &tokio::runtime::Handle, program: PathBuf, args: Vec<String>, restart: Backoff, lines: tokio::sync::mpsc::UnboundedSender<String>) -> Follower`. It spawns `program args` with stdout piped, stdin and stderr null, and `kill_on_drop(true)`. Every stdout line (without the newline) is sent to `lines`. When the child exits, it waits for the next backoff delay and starts it again.
  - `#[derive(Clone, Copy, Debug)] pub struct Backoff { pub first: Duration, pub max: Duration }`. The delay doubles from `first` up to `max`, and resets after the child has run for 30 s. Defaults: 1 s and 30 s.
  - `pub async fn run_once(program: &Path, args: &[&str], timeout: Duration) -> Option<String>`: runs a command to completion and returns its stdout. It returns `None` on a spawn error, a non-zero exit, or a timeout, in which case the child is killed.

- [ ] **Step 1: Write the failing tests** (all `#[cfg(unix)]`, `#[tokio::test]`, using `/bin/sh -c`):
  - `follower_sends_each_line`: `printf 'a\nb\n'; sleep 5` gives `"a"` then `"b"` on the channel within 1 s.
  - `follower_restarts_with_backoff_and_stops_on_drop`: script `echo x; exit 1`, with `Backoff { first: 50ms, max: 200ms }`. After 1 s, the channel has received between 4 and 12 `"x"` (restarts happen, with growing delays). After dropping the `Follower`, no line arrives for 500 ms.
  - `dropping_the_follower_kills_the_child`: script `echo $$; sleep 30`. Read the pid, drop the `Follower`, and within 1 s `pid_alive(pid)` is false.
  - `run_once_returns_stdout_and_times_out`: `echo hi` gives `Some("hi\n")`; `sleep 5` with a 200 ms timeout gives `None` within 1 s; a missing program gives `None`.

- [ ] **Step 2: Run and see them fail** `cargo test -p fdt_vm devices::follow` → compile errors.

- [ ] **Step 3: Implement** with `tokio::process::Command` and `BufReader::lines()`.

- [ ] **Step 4: Run and see them pass**; clippy clean.

- [ ] **Step 5: Commit** `feat(vm): supervised child processes that stream their output lines`

### Task 4: Android watcher

**Files:**
- Create: `crates/fdt_vm/src/devices/android.rs`

**Interfaces:**
- Consumes: `Candidate`, `Platform`, `android_hit`, `is_loopback_url`, `clean_adb_device_name` (Task 1); `follow`, `Follower`, `Backoff`, `run_once` (Task 3).
- Produces:
  - `pub fn parse_track_frames(buf: &mut Vec<u8>) -> Vec<Vec<(String, String)>>`. It drains complete `adb track-devices` frames (a 4-hex-digit length, then that many bytes of `serial\tstate\n` lines) from `buf` and returns each frame's `(serial, state)` list. A partial frame stays in `buf`.
  - `pub fn parse_devices_l(text: &str) -> Vec<(String, String)>`: `(serial, cleaned model)` for lines whose state is `device`. With no `model:` field, the name is the serial.
  - `pub async fn tcp_alive(port: u16, wait: Duration) -> bool`: connect to `127.0.0.1:port`; true when the connection is still open after `wait` (400 ms in production). False when it fails or is closed before then.
  - `pub(crate) struct AndroidWatch` with `pub(crate) fn spawn(handle: &Handle, adb: PathBuf, cfg: WatchTiming, out: Arc<watch::Sender<Arc<[Candidate]>>>, status: Arc<watch::Sender<PlatformState>>) -> AndroidWatch`. Dropping it stops everything and synchronously runs `adb -s <serial> forward --remove tcp:<port>` for each forward it holds (`std::process::Command`, 2 s timeout each).
  - `#[derive(Clone, Copy, Debug)] pub struct WatchTiming { pub liveness: Duration, pub alive_wait: Duration, pub sim_scan: Duration }`, defaults 2 s, 400 ms, 2 s. Put this in `devices/mod.rs`.
  - `#[derive(Clone, Debug, PartialEq, Eq, Default)] pub enum PlatformState { #[default] Off, Watching { devices: usize }, Unavailable(String) }`. Put this in `devices/mod.rs`.

  Behaviour:
  1. Follow `adb track-devices`. On each frame, run `adb devices -l` once (`run_once`, 5 s) for names. Start a logcat follower and a one-off backfill for each new serial in state `device`; stop followers and remove forwards for serials that went away.
  2. On a hit: `forward tcp:0 tcp:<port>` (`run_once`) gives the local port. If `tcp_alive(local)` is false, remove the forward and drop the hit. Otherwise publish `Candidate { platform: Android, device_id: serial, device_name, pid, vm_uri: "http://127.0.0.1:<local>/<token>/" }`. If a candidate with the same `(serial, pid)` already exists, replace it and remove the old forward.
  3. Every `liveness`, `tcp_alive` each forward. If one is dead, remove the candidate and the forward.
  4. Status: `Watching { devices }`, or `Unavailable("adb stopped: …")` when `track-devices` keeps failing (the follower's backoff runs meanwhile).

- [ ] **Step 1: Write the failing tests**
  - `track_frames_are_split_and_partials_kept`: `b"0015emulator-5554\tdevice\n0000"` gives `[[("emulator-5554","device")], []]` with `buf` empty; `b"0015emulator-5554\tdev"` gives `[]` with `buf` unchanged.
  - `devices_l_names_are_cleaned`: `List of devices attached\nemulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\nR5CT unauthorized usb:1-1 transport_id:2\n` gives `[("emulator-5554","sdk gphone64 arm64")]`.
  - `tcp_alive_tells_open_from_closed` (`#[tokio::test]`): a `TcpListener` that accepts and holds gives `true`. A listener that accepts and immediately drops the stream gives `false`. An unused port gives `false`.
  - `fake_adb_end_to_end` (`#[cfg(unix)]`, `#[tokio::test]`): an `adb` shell script in a temp dir that answers:
    - `track-devices`: prints one frame for `emulator-5554` and sleeps;
    - `devices -l`: the line above;
    - `logcat -d`: nothing;
    - `logcat -v brief -T 1 …`: one marker line for pid 4242 with port 40421, then sleeps;
    - `forward tcp:0 tcp:40421`: the port of a test `TcpListener` that holds connections;
    - `forward --remove …`: appends its arguments to a file.

    Within 3 s the published set is exactly one `Candidate` with `device_name "sdk gphone64 arm64"`, pid 4242 and the listener's port. Dropping the listener makes the candidate leave within 3 s, and the remove file names that port.
  - `dead_backfill_lines_leave_no_candidate_and_no_forward`: same fake, but `logcat -d` prints an old marker line and the forward points at a closed port. No candidate is ever published, and the remove file names the port.
  - `missing_adb_reports_unavailable`: an `adb` path that does not exist gives `PlatformState::Unavailable(_)` within 2 s, and no candidates.

- [ ] **Step 2: Run and see them fail** `cargo test -p fdt_vm devices::android`.

- [ ] **Step 3: Implement**, keeping `(serial, pid) → (Candidate, local_port)` as the watcher's state, owned by one task.

- [ ] **Step 4: Run and see them pass**; clippy clean.

- [ ] **Step 5: Commit** `feat(vm): find Flutter apps opened on Android devices from logcat`

### Task 5: Simulator watcher and `DeviceWatch`

**Files:**
- Create: `crates/fdt_vm/src/devices/simulator_watch.rs`
- Modify: `crates/fdt_vm/src/devices/mod.rs` (add `DeviceWatch`, `DeviceWatchConfig`, `WatchStatus`)

**Interfaces:**
- Consumes: Tasks 1–4; `fdt_vm::discovery::pid_alive`.
- Produces (in `mod.rs`):
  - `#[derive(Clone, Debug, PartialEq, Eq, Default)] pub struct WatchStatus { pub simulator: PlatformState, pub android: PlatformState }`
  - `#[derive(Clone, Debug)] pub struct DeviceWatchConfig { pub adb: Option<PathBuf>, pub xcrun: Option<PathBuf>, pub simulator_dir: Option<PathBuf>, pub timing: WatchTiming }`, with `pub fn detect() -> Self`, which uses `find_adb`, `find_xcrun` and `simulator_devices_dir` with the real environment.
  - `pub struct DeviceWatch` with `pub fn spawn(handle: &tokio::runtime::Handle, config: DeviceWatchConfig, candidates: Arc<watch::Sender<Arc<[Candidate]>>>, status: Arc<watch::Sender<WatchStatus>>) -> DeviceWatch`. Each platform's state is `Unavailable("xcrun not found")`, `Unavailable("adb not found")` or `Off` (simulators off macOS) as applicable. The published candidate set is the union of both watchers' sets, sorted by `vm_uri`, and is only sent when it changed. `Drop` stops both watchers, sends an empty set, and sends `WatchStatus::default()`.

  Simulator behaviour:
  1. Every `sim_scan`, `SimulatorScan::booted()`. Start a `log stream` follower plus a one-off `log show --last 1h` backfill for each newly booted simulator, and stop them for simulators that shut down.
  2. On a hit whose `pid_alive(pid)` is true, publish `Candidate { platform: IosSimulator, device_id: udid, device_name: name, pid, vm_uri: url }` (the URL as logged; F1). A dead pid is skipped.
  3. Every `liveness`, drop candidates whose pid is dead.
  4. Status: `Watching { devices: booted count }`.

- [ ] **Step 1: Write the failing tests**
  - `fake_simulator_end_to_end` (`#[cfg(unix)]`, `#[tokio::test]`): a temp devices dir with one booted `device.plist` (udid `U1`, name `iPhone 18 Pro`), and a fake `xcrun` script:
    - for `log stream`: one ndjson marker line with `processID` = the pid of a `sleep 30` child the test started, then sleeps;
    - for `log show`: nothing.

    Within 3 s the set is one `Candidate { platform: IosSimulator, device_id: "U1", device_name: "iPhone 18 Pro", pid, vm_uri }`. Killing the `sleep` child removes it within 3 s.
  - `stale_backfill_line_is_skipped`: the fake `log show` prints a marker line for pid 999999 (dead), and `log stream` prints nothing. No candidate within 3 s.
  - `shutting_down_a_simulator_stops_its_follower`: rewrite the plist with state 1. Within 3 s the status is `Watching { devices: 0 }` and the fake `log stream` process is gone (it writes its pid to a file; check `pid_alive`).
  - `dropping_device_watch_empties_the_set`: after the first test's candidate appears, drop the `DeviceWatch`. The receiver's latest value is empty and the status is `WatchStatus::default()`.

- [ ] **Step 2: Run and see them fail.** **Step 3: Implement.** **Step 4: Run and see them pass**, plus `cargo test -p fdt_vm` (everything) and clippy.

- [ ] **Step 5: Commit** `feat(vm): find Flutter apps opened on booted iOS simulators`

### Task 6: Discovery lists device candidates

**Files:**
- Modify: `crates/fdt_vm/src/discovery.rs`
- Modify: `crates/fdt_app/src/test_support.rs`, `crates/fdt_capture/tests/lifecycle.rs` (add the new `DiscoveredApp` field where they build one)

**Interfaces:**
- Consumes: `Candidate`, `Platform` (Task 1).
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum AppSource { #[default] Dtd, Device }`, and a new field `pub source: AppSource` on `DiscoveredApp`.
  - `DiscoveryConfig` gains `pub candidates: Option<watch::Receiver<Arc<[Candidate]>>>` (default `None`) and `pub candidate_grace: Duration` (default 3 s).
  - `impl AppKey { pub fn same_app(&self, other: &AppKey) -> bool }`: package and device equal, and either the workspaces are equal or either one is empty (design §6.6).

  Rules (design §6.4):
  - On each tick, read the latest candidate set. Remember when each `vm_uri` was first seen, and forget the ones that left.
  - A candidate is **claimed** when some DTD-listed service's finished probe has `pid == Some(c.pid)` and `os == c.platform.os()`.
  - A candidate is probed (same `probes` map, same `probe()`, same retry) only when `now ≥ first_seen + candidate_grace` and it is not claimed. A claimed candidate is never probed. A candidate that becomes claimed after being listed is no longer listed.
  - Each unclaimed candidate with a finished probe becomes a `DiscoveredApp`:
    - `name`: the probe's package, or `"Dart app"`;
    - `device`: `c.device_name`;
    - `key: AppKey { package: name, device: c.device_name, workspace: "" }`;
    - `workspace: None`, `source: AppSource::Device`, `pid: Some(c.pid)`;
    - `mode` and `flutter_version` from the probe.
    DTD apps get `source: AppSource::Dtd`. The existing sort and URI dedup run over both.
  - `probes.retain` keeps candidate URIs too.

- [ ] **Step 1: Write the failing tests** (in `discovery.rs` tests, with `ScriptedServer` VMs; the candidate channel is a `watch::channel` the test owns; `candidate_grace` 300 ms in tests):
  - `device_candidate_is_listed_after_the_grace_period`: a VM server for package `shop` with getVM pid 77 and os `ios`, published as a candidate with pid 77. It is not listed at 150 ms, and is listed by 1.5 s with `source Device`, `device` from the candidate, `workspace None`, and `key.workspace ""`.
  - `candidate_claimed_by_a_dtd_app_is_never_contacted`: a DTD app (registry + DTD server) whose VM answers pid 77, os `ios`, plus a candidate (a second scripted VM) with pid 77, platform `IosSimulator`. After 1.5 s, exactly one app is listed (the DTD one), and the candidate's server has received **zero** connections (`count(server, "getVM") == 0`).
  - `same_pid_on_another_os_is_not_claimed`: as above, but the candidate's platform is `Android`. Both are listed.
  - `candidate_leaves_when_its_set_drops_it`: after it is listed, publish an empty set. It is gone within 1.5 s.
  - `same_app_matches_across_launch_methods`: `AppKey{shop, iPhone 17, "/w"}.same_app(&AppKey{shop, iPhone 17, ""})` is true; `{shop, iPhone 17, "/w"}` vs `{shop, iPhone 17, "/x"}` is false; `{shop, iPhone 17, ""}` vs `{shop, Pixel, ""}` is false.

- [ ] **Step 2: Run and see them fail** `cargo test -p fdt_vm discovery`.

- [ ] **Step 3: Implement**; update the two other `DiscoveredApp` constructors with `source: AppSource::Dtd`.

- [ ] **Step 4: Run and see them pass**, plus `cargo test --workspace` and clippy.

- [ ] **Step 5: Commit** `feat(vm): list apps found in device logs, after a grace period, unless the Flutter tool owns them`

### Task 7: Reconnect across launch methods

**Files:**
- Modify: `crates/fdt_capture/src/lifecycle.rs:292` (`a.key == key` → `a.key.same_app(&key)`)
- Test: `crates/fdt_capture/tests/lifecycle.rs`

**Interfaces:**
- Consumes: `AppKey::same_app` (Task 6).

- [ ] **Step 1: Write the failing test** `reconnects_to_the_same_app_opened_from_its_icon`, in the style of that file's existing reconnect tests. Connect to a discovered app with key `{jank_lab, iPhone 17, "/w"}`, then close its VM. Publish a discovery list with an app keyed `{jank_lab, iPhone 17, ""}` (source `Device`) whose VM is live. The capture reconnects to it (status `Connected` within the test's usual timeout).
- [ ] **Step 2: Run and see it fail** `cargo test -p fdt_capture --test lifecycle reconnects_to_the_same_app_opened_from_its_icon`.
- [ ] **Step 3: Implement** the one-line change.
- [ ] **Step 4: Run and see it pass**, plus `cargo test -p fdt_capture`.
- [ ] **Step 5: Commit** `feat(capture): reconnect to the same app whether or not the Flutter tool started it`

### Task 8: App wiring, Connect page and Settings

**Files:**
- Modify: `crates/fdt_app/src/settings.rs` (field `find_device_apps: bool`, default `true`, JSON key `find_device_apps`, read with the existing `field` helper)
- Modify: `crates/fdt_app/src/capture_host.rs` (create the candidate and status senders; pass `candidates` to `DiscoveryConfig`; hold `Option<DeviceWatch>`; `pub fn set_find_device_apps(&mut self, on: bool)`; `pub fn device_status(&self) -> watch::Receiver<WatchStatus>`)
- Modify: `crates/fdt_app/src/shell/start_page.rs` (empty-state copy, iPhone line, "opened on device")
- Modify: `crates/fdt_app/src/shell/settings_page.rs` (new section "Connect" with the switch, its description, and two status lines)

**Interfaces:**
- Consumes: `DeviceWatch`, `DeviceWatchConfig::detect`, `WatchStatus`, `PlatformState` (Task 5); `AppSource` (Task 6).
- Produces:
  - `CaptureHost::install` starts a `DeviceWatch` when `settings(cx).find_device_apps` is true.
  - `CaptureHost::install_with(cx, config)` (tests) never starts one, so tests never touch real devices.
  - Status line text:
    - `"iOS simulators: watching N booted"` / `"Android: watching N devices"` for `Watching`;
    - `"iOS simulators: <reason>"` / `"Android: <reason>"` for `Unavailable`;
    - `"… off"` for `Off`.
    Only the iOS line is shown on macOS.

- [ ] **Step 1: Write the failing tests**
  - `settings.rs`: `find_device_apps_defaults_on_and_round_trips`.
  - `start_page.rs`: `empty_state_copy` is updated to the new exact copy, and on macOS (`cfg!(target_os = "macos")`) also finds the iPhone line. `device_rows_say_opened_on_device`: an app with `source Device` and `workspace None` shows the text `opened on device` in its row, and an app with `source Dtd` still shows its short path.
  - `settings_page.rs`: `switching_off_removes_device_rows`. With a host installed through a test helper that has a fake `DeviceWatch` (an `Arc<watch::Sender<Arc<[Candidate]>>>` the test publishes to), turning the switch off saves `find_device_apps = false` and calls `set_find_device_apps(false)`, which drops the watch and empties the candidate set. Add the test helper next to `install_with` in `capture_host.rs`, `#[cfg(test)]`.

- [ ] **Step 2: Run and see them fail** `cargo test -p fdt_app start_page settings`.

- [ ] **Step 3: Implement.** The switch calls `update_settings` then `CaptureHost::set_find_device_apps` (if the host exists) and notifies the shell, like the existing switches. The Settings page observes `device_status()` to redraw the status lines.

- [ ] **Step 4: Run and see them pass**, plus `cargo test --workspace` and clippy.

- [ ] **Step 5: Commit** `feat(app): show apps opened on simulators and devices, with a Settings switch`

### Task 9: Return on a fresh Connect page connects

Observed on 2026-11-22 in the real app (installed `fdt.app`, driven by keyboard events):
- A fresh window with one app listed and highlighted: Return did nothing.
- ⌘K "Connect to an App…" while already on Connect, then Return: nothing.
- ⌘1 then ⌘[ (a real page change back to Connect), then Return: connected.

`MainWindow::new` calls `window.focus(&focus, cx)` on the shell (`shell/mod.rs:132`). `focus_start_if_current` runs on page changes and in `set_apps_source`. The `ConnectToApp` handler only `navigate`s, which is a no-op when already on Start, and the palette returns focus to the shell when it closes.

**Files:**
- Modify: `crates/fdt_app/src/shell/mod.rs`
- Test: `crates/fdt_app/src/shell/start_page.rs` tests (they already build a real shell via `test_support`)

- [ ] **Step 1: Write the failing tests**
  - `enter_on_a_fresh_window_connects_the_first_app`: build the window the way `app::run` does (through `test_support`, with the start page created by the same path as production: the host's apps, not a direct `set_apps_source` call if production differs), list two apps, and `simulate_keystrokes("enter")` with no other input. The fake control received `Target::Discovered` for the first app.
  - `connect_to_app_while_on_connect_focuses_the_list`: open the palette, run "Connect to an App…", then `enter`. Connects the highlighted app.
- [ ] **Step 2: Run and see them fail.** If the first test passes in the harness, the harness differs from production. Find that difference first (`superpowers:systematic-debugging`): which focus call wins at startup in `app::run`, and when the start page is created relative to `shell/mod.rs:132`. Make the test reproduce it before fixing.
- [ ] **Step 3: Fix the root cause.** The `ConnectToApp` handler also calls `focus_start_if_current` after `navigate`, and the startup path ends with the list focused when Start is the current page.
- [ ] **Step 4: Run and see them pass**, plus `cargo test -p fdt_app`.
- [ ] **Step 5: Commit** `fix(app): Return on the Connect page connects as soon as the window opens`

### Task 10: Live simulator test, spec amendment, install

**Files:**
- Create: `crates/fdt_vm/tests/device_watch_live.rs`
- Modify: `docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md` (§5.1 "Finding running apps", §8.1 "How apps are found" and "Disconnects"; text from the design's §9)
- Modify: `README.md` (one line under Command line or Build: apps opened on a simulator or Android device appear on the Connect page; iPhone apps need `flutter run`/`flutter attach`)

- [ ] **Step 1: Write the live test** `simulator_app_opened_without_flutter_tool_is_listed`, ignored unless `FDT_DEVICE_IT=1`.
  - It needs a booted simulator with `jank_lab` installed (bundle id `dev.fdt.jankLab`). It uses `xcrun simctl launch <booted udid> dev.fdt.jankLab`.
  - It runs `DeviceWatch` plus `Discovery` (an empty temp registry dir) and asserts that a `DiscoveredApp` with `source Device` and name `jank_lab` is listed within 6 s.
  - Then `xcrun simctl terminate`, and it is gone within 7 s.
- [ ] **Step 2: Run it** (a simulator is booted and `jank_lab` is installed from spike S3):
  Run: `FDT_DEVICE_IT=1 cargo test -p fdt_vm --test device_watch_live -- --ignored --nocapture`
  Expected: PASS. Record the time to listed and the time to gone.
- [ ] **Step 3: Measure idle CPU**, `fdt` plus its children (Global Constraints):
  - `fdt` open, not connected, one simulator booted, the emulator running, 30 s warm-up;
  - then 30 samples one second apart of the summed `ps -o %cpu=` of `fdt` and its child processes (`pgrep -P <fdt pid>`, recursively).
  - Budget: every sample ≤ 1 %. Record the raw numbers in the commit message body.
- [ ] **Step 4: Amend the main spec and README**, then run `cargo test --workspace` and clippy.
- [ ] **Step 5: Commit** `docs: device-log discovery in the main design; live simulator test`
- [ ] **Step 6: Rebuild and install** `scripts/bundle-macos.sh --install` (quit fdt first). Then check by hand:
  - with the installed app open, launch `jank_lab` from the simulator's home screen. It appears within about 4 s with "opened on device", and Return connects;
  - same for the emulator (`adb shell am start -n dev.fdt.jank_lab/.MainActivity`).
