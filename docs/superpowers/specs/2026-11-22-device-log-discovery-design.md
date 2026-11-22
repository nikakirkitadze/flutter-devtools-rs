# Finding apps opened on simulators and devices: design

- **Date:** 2026-11-22
- **Status:** Draft for the author's review.
- **Amends:** the main design, §5.1 ("Finding running apps") and §8.1 ("How apps are found",
  "Disconnects").

---

## 1. Why

The author usually starts apps with `flutter run` (debug, profile or release), and those already
appear on the Connect page. The gap is an app that is **already installed** on a simulator or a
phone and is opened from its icon (or from Xcode). Today nothing announces such an app, so `fdt`
does not list it, even though it is running and has a VM service that `fdt` can record from.

This was found on 2026-11-22:

- **Simulator.** `crocobet` was opened from the iPhone 17 simulator's home screen. The Connect page
  stayed empty. Pasting the VM service URL from the simulator log connected and recorded normally.
- **iPhone.** A Flutter app was opened on Nika's iPhone (USB). No Flutter tool or Xcode was attached,
  and the Connect page stayed empty.

**What success looks like:** with `fdt` open, the author opens an installed debug or profile app on
a simulator or a USB-connected phone, and within about four seconds it appears on the Connect page
like any other app. One click (or Enter) connects. (The wait is deliberate; see §6.4.)

## 2. Goals, non-goals, anti-goals

**Goals**
- List debug and profile Flutter apps that were opened without the Flutter tool, on:
  1. booted iOS simulators (macOS);
  2. Android emulators and devices that `adb` sees (every OS);
  3. iPhones connected by USB (macOS), if spike S1 (§7) succeeds.
- Show them in the existing list, with the same row layout, the same connect action and the same
  reconnect behaviour ("same app" opened again on the same device).
- Work when `fdt` is launched from Finder or the Dock, where `PATH` is minimal.

**Non-goals**
- **Release builds.** They have no VM service; no tool can attach. The empty state says so.
- **Launching apps from `fdt`.** The author opens the app; `fdt` notices it. Launching can come later.
- **Desktop apps** opened from Finder or Explorer. They print the VM service URL only to their own
  stdout, which nothing else can read.
- **iPhones on Windows and Linux**, and iPhones over Wi-Fi only. Later, if asked for.
- **Hot reload.** `fdt` is not the Flutter tool.

**Anti-goals** (failure even if it technically works)
- **Making `flutter run` worse.** An app the Flutter tool started must still appear exactly once, as
  it does today, with its workspace folder. A device-log entry never replaces it.
- **Connecting on its own.** Finding an app only lists it. `fdt` connects when the user asks, as now.
- **Noise or cost when idle.** Watching must keep the §10 idle budget (≤ 1 % CPU when idle) with one
  simulator and one phone attached, counting the processes `fdt` starts (F11). No dialogs,
  permission prompts, installs or `sudo`.
- **Leaking tokens.** VM service URLs carry an auth token. They are never logged unredacted.

## 3. How it works today

`fdt_vm::discovery::Discovery` runs a loop every second:

1. Read the DTD registry directory (`~/Library/Application Support/Dart/dtd/<pid>` on macOS) and keep
   entries whose pid is alive.
2. Connect to each DTD and call `ConnectedApp.getVmServices`.
3. Probe each VM service once (`getVM`, `getIsolate`, the `flutterVersion` service) for package, mode,
   Flutter version and the app's pid.
4. Publish a `Vec<DiscoveredApp>` on a `watch` channel. The Start page shows it. Duplicates are
   removed by VM URI.

`DiscoveredApp` carries `key: AppKey { package, device, workspace }`, `vm_uri`, `name`, `device`,
`mode`, `flutter_version`, `workspace` and `pid`. Connecting sends `Target::Discovered(app)`; on a
disconnect, `lifecycle.rs` waits for an app with the same `AppKey` to be listed again.

A security rule from M2 stays: `fdt` only opens WebSockets to addresses it discovered or the user
pasted.

## 4. Facts

Verified on the author's machine on 2026-11-22 (Flutter 3.47.9, macOS 27.2, Xcode with iOS 26.5 and
27 simulators, iPhone 16 Pro on iOS 27.2):

- **F1. Simulator log.** A debug app opened from the simulator's home screen logs, through the
  simulator's unified log:
  `Runner[94685] (Flutter) flutter: The Dart VM service is listening on http://127.0.0.1:52932/IPR4lN_nQH0=/`
  - `xcrun simctl spawn <udid> log show --predicate 'eventMessage CONTAINS "Dart VM service is listening"'`
    finds it, including for apps opened before the query.
  - The app is a process on the Mac, and its VM service listens on the Mac's loopback, so the URL
    works as is.
  - A pasted copy of that URL connected `fdt` and recorded frames and logs.
- **F2. Booted simulators.** `xcrun simctl list devices booted -j` lists them with name and udid.
- **F3. Android (spike S2, emulator `sdk_gphone64_arm64`, API 36).** `adb` is at
  `~/Library/Android/sdk/platform-tools/adb` and `/opt/homebrew/bin/adb`. With a debug `jank_lab`
  installed and started from its launcher activity (no Flutter tool):
  - `adb logcat -v brief flutter:I *:S` shows
    `I/flutter ( 9677): The Dart VM service is listening on http://127.0.0.1:40421/<token>/`. The pid
    is space-padded inside the brackets, and is the app's pid (`pidof` and `getVM` agree).
  - `adb forward tcp:0 tcp:40421` prints the local port (`54918`). `getVM` through
    `ws://127.0.0.1:54918/<token>/ws` answers.
  - A TCP connection to the forwarded port stays open (400 ms) while the app runs, and is closed by
    adb after 3 ms once the app is killed (`am force-stop`).
  - `adb logcat -v brief -T 1 flutter:I *:S` follows new lines: the relaunched app's line (new pid)
    arrived.
  - `adb shell monkey -p <package> -c android.intent.category.LAUNCHER 1` did not start the app on
    this emulator; `am start -n <package>/.MainActivity` did (test scripts only).
- **F4. iPhone.** The Flutter SDK ships `idevicesyslog` and `iproxy` in
  `<flutter>/bin/cache/artifacts/{libimobiledevice,usbmuxd}`. With `DYLD_LIBRARY_PATH` set to the
  sibling artifact folders, `idevicesyslog -u <udid>` streamed Nika's iPhone (about 35 lines a
  second, unfiltered). Not yet verified: that a Flutter app's VM service line appears in that stream
  on iOS 27, and that `iproxy` forwards to it.
- **F5. iOS rules.** Since iOS 14, a **debug** build cannot start from a phone's home screen without
  a debugger attached. Profile builds can. Debug builds started from Xcode can.
- **F6. Finder launches.** An app started from Finder or the Dock gets
  `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so `adb` and `flutter` are not on it. `xcrun` is at
  `/usr/bin/xcrun`.
- **F7. Bonjour.** No `_dartVmService._tcp` service was visible on the network during the iPhone test.
- **F8. One log line to match.** The supported range is Flutter 3.41 to 3.50 (main design §5.3). All
  of it prints "The Dart VM service is listening on <url>". The older "Observatory listening on" text
  is out of range.

- **F10. The log line carries the app's pid.** The simulator line is prefixed `Runner[94685]`, and
  94685 is the app's host pid (its VM service port was found listening under that pid with `lsof`).
  Android's `brief` format puts the pid in `I/flutter (<pid>):`, and the iPhone syslog in
  `Runner(Flutter)[<pid>]` (S2 and S1 confirm these two).
- **F11. How idle CPU is measured.** The M2 and M3 reviews sample `ps -o %cpu= -p <fdt pid>`, which
  is one process, as a percentage of one core. This design also counts the processes `fdt` starts,
  added together. A reviewer timed the device-list commands: `xcrun simctl list devices booted -j`
  costs about 80 ms of CPU per call, and `xcrun devicectl list devices` about 100 ms with no phone
  attached. Polling both every 2 s would cost about 9 % of a core, so device lists must come from
  change notifications (§6.2).

- **F9. DDS (spike S3, 2026-11-22, iPhone 18 Pro simulator, iOS 27).** `flutter run` of `jank_lab`
  while a script connected to the raw VM service URL 60 ms after its log line and held the socket:
  - The script's `getVM` answered (pid 20182, the same as the log line's pid). About 590 ms after the
    log line, when DDS connected, the VM service **closed the script's socket** (close code 1005).
    `flutter run` went on normally (`app.started`). So a direct client does not stop DDS: DDS ends
    it. An early probe cannot break `flutter run`.
  - The DTD registry file appeared 607 ms after the log line.
  - With DDS attached, a new direct client to the raw URL is refused (WebSocket error, close 1006).
    So the log line of an app the Flutter tool is running never yields a working candidate.
  - Consequence: if `fdt` is connected directly to an app and the user then runs `flutter attach`,
    `fdt`'s connection is closed, and it reconnects through the DTD entry (§6.6).
- **F12. Device-list sources (spike S4).**
  - Simulators: each `~/Library/Developer/CoreSimulator/Devices/<udid>/device.plist` has `name` and
    `state` (3 booted, 1 shut down), and its modification time changes when the state does. Reading
    these files needs no `simctl` call.
  - Android: `adb track-devices` printed `0015emulator-5554\tdevice` (a 4-digit hex length, then the
    `adb devices` lines) and stays open, printing again on every change.
  - iPhone: a `Listen` request on `/var/run/usbmuxd` (plist protocol, version 1, message type 8)
    answered `Result 0`, then pushed `Attached` for Nika's iPhone (`ConnectionType USB`).

## 5. Approaches

| Approach | Trade-offs | Verdict |
|----------|-----------|---------|
| **A. Watch device logs.** Follow the log of each booted simulator and each connected device, pick out the VM service line, forward the port if needed, and list the app. | Matches the request: nothing to click or set up. Runs external tools (`xcrun simctl`, `adb`, the Flutter SDK's `idevicesyslog` and `iproxy`). On an iPhone, only apps opened after watching starts are found (the syslog has no history). While `fdt` is connected directly, a later `flutter attach` to the same app can fail (F9). | **Chosen by the author** |
| B. Run `flutter attach` for the user. The user picks a project and a device and opens the app; `flutter attach` waits for it, and it joins the list through DTD. | The Flutter tool handles every device quirk and keeps hot reload. But it needs the app's project folder, and it is a button pressed beforehand, not a listener. | Rejected |
| C. Browse Bonjour for `_dartVmService._tcp`. | Passive, but nothing was announced in the test (F7). iOS announces only with Local Network permission. Phones still need port forwarding. | Rejected |

## 6. Design

### 6.1 Overview

A new **device watcher** runs next to the DTD scan. It produces **candidates**: a VM service URL
reachable from this machine, the app's pid from the log line, and where it came from (platform,
device id, device name). After a grace period (§6.4), Discovery probes each candidate as it probes
DTD-listed services, and publishes the ones that answer in the same list. A candidate is never shown
before its probe succeeds, so it can't be clicked while the Flutter tool may still be attaching.

```
DTD registry ──► DTD clients ──► getVmServices ──┐
                                                  ├──► probe ──► merge ──► list on Connect page
device watcher ──► candidates (forwarded URLs) ───┘
```

### 6.2 The three watchers

Each watcher keeps a list of its devices, starts one log follower per device, and stops it when the
device goes away. When a line containing `The Dart VM service is listening on ` arrives, the
watcher takes the URL after it and the pid before it, forwards the port if needed, and emits a
candidate.

**Device lists are not polled** (F11). Each watcher learns about devices from a source that pushes
changes (F12), and runs a listing command only when something changed.

**iOS simulator (macOS only)**
- Devices: watch the CoreSimulator device folder (`~/Library/Developer/CoreSimulator/Devices`) and
  its `*/device.plist` files with file-system events, and read `name` and `state` from the plists
  (F12). No `simctl` call is needed to list them.
- Follower: `xcrun simctl spawn <udid> log stream --style ndjson --predicate 'eventMessage CONTAINS "Dart VM service is listening"'`.
  The predicate filters inside the log daemon, so `fdt` sees only matching lines.
- Apps already running: when a simulator is first seen, run the same predicate with `log show` over
  the last hour. Stale URLs are dropped by the probe.
- Forwarding: none (F1).

**Android (every OS)**
- Devices: `adb track-devices` (one long-lived process that prints the list whenever it changes),
  then `adb devices -l` for names; the name is the `model:` field, or the AVD name for an emulator.
- Follower: `adb -s <serial> logcat -v brief -T 1 flutter:I *:S` (only the `flutter` tag).
- Apps already running: `adb -s <serial> logcat -d -v brief flutter:I *:S` when the device is first
  seen.
- Forwarding: `adb -s <serial> forward tcp:0 tcp:<device port>` prints the local port; the candidate
  URL uses `127.0.0.1:<local port>` with the same token path. The forward is removed
  (`adb forward --remove`) when the candidate's probe fails for good, the device goes away, or `fdt`
  quits.

**iPhone over USB (macOS only; depends on spike S1)**
- Devices: usbmuxd's attach and detach events (a `Listen` request on `/var/run/usbmuxd`), then
  `xcrun devicectl list devices --json-output <file>` once per change for names and pairing state,
  keeping wired, paired, available devices.
- Follower: the Flutter SDK's `idevicesyslog -u <udid>`, with `DYLD_LIBRARY_PATH` set as the Flutter
  tool sets it. The stream is unfiltered, so `fdt` keeps only lines containing the marker text.
- Apps already running: not found (no history). The empty state says to reopen the app.
- Forwarding: the Flutter SDK's `iproxy <local>:<device port> -u <udid>`, one process per candidate,
  killed with the candidate.
- If S1 fails, the iPhone watcher is left out of this work, and the empty state tells iPhone users to
  start apps with `flutter run` or `flutter attach`.

### 6.3 Finding the tools

Each platform turns itself off quietly when its tool is missing.

- `xcrun`: `/usr/bin/xcrun` (macOS).
- `adb`, the first that exists: `PATH`; `$ANDROID_HOME/platform-tools`;
  `$ANDROID_SDK_ROOT/platform-tools`; the default SDK folder (`~/Library/Android/sdk` on macOS,
  `%LOCALAPPDATA%\Android\Sdk` on Windows, `~/Android/Sdk` on Linux).
- The Flutter SDK (for `idevicesyslog` and `iproxy`), the first that exists: `$FLUTTER_ROOT`; the
  `flutter` on `PATH` with symlinks resolved; the `flutterRoot` in
  `<workspace>/.dart_tool/package_config.json` of any workspace a DTD entry has reported;
  `~/development/flutter`, `~/flutter`, `~/fvm/default`.
- Settings can override the `adb` path and the Flutter SDK path.

### 6.4 Staying out of the Flutter tool's way, and leaving the list

The same app can be found both ways: `flutter run` prints the log line too, and reads it at about
the same moment `fdt` does. An early probe cannot break `flutter run` (F9: DDS closes it), but
without care the app would flash up as a log row and then be replaced by its DTD row, and `fdt`
would contact an app the Flutter tool is about to own. `flutter run` is how the author usually works,
so its rows must look exactly as they do today.

- **Identity:** device plus the app's pid. A candidate's pid comes from its log line (F10), so it is
  known without touching the app. A DTD-listed app's pid comes from its probe's `getVM`, as today.
- **Grace period.** A candidate is not probed until 3 s after its log line. If, by then, a DTD-listed
  app on the same device has the same pid, the candidate is dropped without ever being contacted.
  Otherwise it is probed and listed. In S3 the registry file appeared 0.6 s after the log line; the
  DTD app's pid is known once its own probe finishes, which can take up to about 1.5 s more. 3 s
  covers both with room on a loaded machine.
- **The DTD row wins** at any later point too: if the Flutter tool attaches to an app after it was
  listed from a log (`flutter attach`), the log row is removed.
- **Repeated lines** (the app restarted, the log replayed) are deduplicated by URL.

**Leaving the list.** A log-found row disappears within about 5 s of its app quitting:
- Simulator: the app's host pid (F10) is checked with `kill(pid, 0)`, every 2 s, at no cost.
- Android and iPhone: every 2 s, a plain TCP connection to the forwarded port. If the forwarder
  closes it at once, the app has gone. This is not a VM service client, so it can't get in the way of
  DDS (S1 and S2 confirm that the forwarders behave this way).
- When a row goes, its port forward goes too, unless a window is connected through it.

This also keeps reconnect correct: `lifecycle.rs` connects to the first listed app whose key
matches, so a dead row must not linger next to the reopened app.

### 6.5 What the user sees

The rows look as they do today. The last column, which shows the workspace folder for apps from
`flutter run`, says "opened on device" for these. The device column says which simulator or phone.

```
Connect to a running app
  jank_lab   iPhone 17           debug    Flutter 3.47.9   fixtures/jank_lab
  crocobet   iPhone 18 Pro       debug    Flutter 3.47.9   opened on device
  crocobet   Nika's iPhone       profile  Flutter 3.47.9   opened on device
```

**Empty state**, replacing today's copy:

> No running Flutter apps found. Start one with `flutter run`, or open a debug or profile build on a
> simulator or a connected device. Release builds can't be connected. Or paste a VM service URL below.

When the iPhone watcher is unavailable (no Flutter SDK found, or left out after S1), one more line
says how to make iPhone apps appear (`flutter run` or `flutter attach`).

**Settings** gets one switch, "Find apps opened on simulators and devices", on by default. Next to it,
each platform shows whether it is watching or why not, for example "Android: adb not found".

### 6.6 Connecting and reconnecting

- Connecting is unchanged: `Target::Discovered(app)` with the candidate's (possibly forwarded) URL.
- `AppKey` for these apps is `{ package, device, workspace: "" }`. Reopening the app on the same
  device produces a new candidate with the same key, so the existing reconnect banner and
  `waiting_for` logic work.
- **Across launch methods.** When either key has an empty workspace, "same app" means the same
  package on the same device. So a capture started from `flutter run` reconnects when the app is
  reopened from its icon, and the other way round. Two projects with the same package name on one
  device could be confused; that is accepted.
- The device names must match across the two sources for this. DTD's label for a simulator
  ("iPhone 17") and the watcher's name come from the same device name; the builder normalises both
  (for example, no "(simulator)" suffix in the key).
- A forward stays alive while a window is connected through it, even if a later probe fails.

### 6.7 Cost and safety

- One follower process per booted simulator and per connected device, plus one `iproxy` per iPhone
  candidate, plus the long-lived device-list sources (§6.2). Device lists are not polled; the only
  2 s timers are the liveness checks in §6.4, which are a `kill(pid, 0)` or one local TCP connect.
- The idle CPU budget (≤ 1 % of one core, `fdt` plus its child processes added together) is measured
  with `fdt` open, one simulator booted and one phone attached. The unfiltered iPhone stream is the
  risk; S1 measures it.
- Candidate URLs are always loopback (directly, or through a forward). `fdt` never connects to a
  non-loopback address found in a log.
- URLs are logged with `VmServiceUri::redacted()` only.
- All child processes are killed when `fdt` quits, including on a crash path where possible
  (process group on Unix, job object on Windows).

## 7. Spikes before planning

Each is a throwaway check whose result goes into §4 as a fact. S2, S3 and S4 ran on 2026-11-22 (F3,
F9, F12). S1 needs the author's iPhone and is still open; the iPhone watcher is built only after it.

- **S1. iPhone (decides whether the iPhone watcher is in).** On Nika's iPhone (iOS 27.2, USB):
  1. Open a **profile** build of `jank_lab` from the home screen, and separately run a **debug** build
     from Xcode. Does the VM service line appear in `idevicesyslog`, and with the pid?
  2. Does `iproxy <local>:<port> -u <udid>` forward to it, and does `fdt` connect through it?
  3. After the app quits, does a TCP connection to the forwarded port close at once?
  4. What does an always-on `idevicesyslog` cost in CPU on the Mac?
- **S2. Android.** On the emulator: the exact logcat line (with pid) for a debug build opened from
  the launcher, that `-T 1` follows from now, that `forward tcp:0` prints the port, and that a TCP
  connection to the forward closes at once after the app quits.
- **S3. DDS (F9).** With `flutter run` active: does the raw URL refuse a direct client? With `fdt`
  connected directly: does `flutter attach` fail, and with what message? And the race: how long after
  the log line does the DTD registry entry appear in `flutter run` (simulator and Android), and does
  an `fdt` probe in that window make `flutter run` fail? This sets the grace period in §6.4.
- **S4. Device-list sources.** That a simulator booting or shutting down changes a file under the
  CoreSimulator device folder; that `adb track-devices` reports an emulator starting and stopping;
  that a usbmuxd `Listen` reports a phone being plugged in and out. Measure each source's idle CPU.

## 8. Testing

- **Unit tests:** line parsing (each platform's format, URL and pid, tokens with `=`); tool lookup
  with a fake filesystem and environment; merging rules (DTD wins, deduplication, pid identity);
  the grace period (a candidate whose pid a DTD app claims within 3 s is never contacted: the
  scripted VM service records no connection); liveness (a dead pid or a closed forward removes the
  row within 5 s); cross-launch reconnect keys; candidate lifecycle with a fake process runner and
  scripted VM services (`fdt_vm::testing::ScriptedServer`).
- **Budget test:** idle CPU of `fdt` plus its children with one simulator booted and one phone
  attached, recorded like the M2 and M3 idle measurements.
- **Live test (simulator):** like `scripts/jank-lab-it.sh`, install `jank_lab` on a booted simulator,
  launch it with `xcrun simctl launch` (no Flutter tool), and assert it is listed and connectable
  within 3 s. Ignored unless an environment variable is set, like the existing live test.
- **Manual:** Android emulator and the iPhone, recorded in the milestone review.

## 9. Changes to the main design

- §5.1 "Finding running apps": add a third step, device logs, for apps not started by the Flutter
  tool.
- §8.1 "How apps are found": add the device watcher, the grace period and the merge rule.
  "Disconnects": "found again through DTD or a device log"; when one side has no workspace, the same
  app is the same package on the same device.
- §8.1 security rule unchanged: only discovered or pasted addresses, now also only loopback for
  log-found ones.

## 10. Calls made in this design (the author did not decide these)

- Android is included although the request named simulators and real devices, because it uses the
  same mechanism at low cost and an emulator is set up on the author's machine.
- The iPhone watcher depends on spike S1 and is cut from this work if S1 fails.
- The switch in Settings is on by default.
- Apps already running when `fdt` starts are found on simulators and Android (log history), not on
  iPhones.
- Protecting `flutter run` comes before speed: log-found apps wait 3 s before being contacted, so they
  appear in about 4 s rather than 2 (§6.4).
- The idle CPU budget counts `fdt`'s child processes, so device lists come from change
  notifications, not polling (F11, §6.2).
- A log-found row leaves within about 5 s of its app quitting, using a pid check or a plain TCP
  check, never a VM service connection (§6.4).
- Reconnect works across launch methods by matching package and device when one side has no
  workspace (§6.6).

## 11. Left to the builder

- Where the watcher lives (a module in `fdt_vm` next to `discovery`, or its own), and how it hands
  candidates to the scan loop.
- Process supervision details: restart a follower that exits, with backoff.
- How Settings lays out the per-platform status.
- Which milestone this lands in (to agree with the author; M4's plan is waiting for review on
  `m4-memory`).
