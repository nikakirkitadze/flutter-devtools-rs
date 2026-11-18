# Flutter DevTools replacement in Rust: design

- **Date:** 2026-11-18
- **Status:** Draft, awaiting approval
- **Working name:** `fdt` (a placeholder for crates, the binary and the file extension; the product name is still open)

---

## 1. Intent

**Why this exists.** Official Flutter DevTools lets developers down when they need it most:

- It lags on big timelines.
- It is slow and hard to get around in.
- Network logs often go missing.
- Debugging memory leaks and performance problems is hard.

DevTools shows raw data and leaves the developer to work out the answer.

**What we are building.** `fdt` is a native desktop app for macOS, Windows and Linux, written in Rust. It connects to a running Flutter app and answers three questions faster than DevTools can:

- Why did this frame jank?
- What is leaking?
- What is rebuilding too much?

It also provides network and log views that capture everything. All of this is tied together by time.

**The quality bar.** The app must feel as polished as Linear, Raycast or Zed. UI quality is part of what "done" means, not a later pass. Section 10 turns that into measurable requirements.

**Who it is for.** Flutter developers in general, not just the author. It ships as signed installers.

## 2. Goals

| # | Goal | What it means in practice |
|---|------|---------------------------|
| G1 | **Instant at any size** | Scrolling, panning, zooming and searching stay at the display refresh rate (120 fps on 120 Hz screens). This holds for 10-minute recordings, a million log lines and widget trees with 50k nodes. |
| G2 | **Answers, not raw data** | A slow frame explains itself: which phase ran over, what rebuilt, and which functions were busy. A leak shows what is holding it. A failed request links to what happened around it. The raw trace is always available, one step away. |
| G3 | **Never lose your place** | A time range selected in one tool applies to every tool. Back and forward history works across tools. Anything related is one click away. |
| G4 | **Capture everything** | Network and log capture starts the moment we connect. With the optional probe package (§8.8), capture starts at app launch. |
| G5 | **Premium and native** | Consistent typography, spacing and colour across carefully designed dark and light themes. Hover, loading, empty and error states are all designed. Window behaviour, shortcuts and menus follow each OS's conventions. |
| G6 | **Keyboard-first** | A command palette reaches every action. Shortcuts cover all frequent actions. Every tool has instant search. |
| G7 | **Shareable sessions** | Export a recording, either the last 60 seconds or a chosen range, as one file a teammate can open. |
| G8 | **Readable by AI agents** | Coding agents can query the findings (slow frames, leaks, failed requests) over MCP. |

## 3. Non-goals (v1)

The following are out of scope for v1:

- **A debugger with breakpoints and stepping.** IDEs already do this well.
- **App size analysis.**
- **Deep link validation.**
- **DevTools extensions.** Packages ship these as Flutter web apps. Running them would mean embedding a browser.
- **Flutter web targets.** Their VM service surface differs, and the Flutter docs point web network debugging at Chrome DevTools. v1 targets Flutter apps running on mobile devices, simulators and emulators, and desktop.
- **A widget sandbox or GenUI playground.** This is a separate future project, to be brainstormed on its own.
- **A multi-device layout matrix.** This is a separate future project.
- **Our own AI layer**, such as plain-language error diagnosis, one-click patches or an LLM integration with API keys. Coding agents do that job; we feed them good data (G8).
- **Rewinding or rolling back the live app's state.** The time-travel timeline shows the past. It does not change the running app.
- **Local database query capture** (drift, sqflite). This could come later through the probe.
- **A Provider state adapter.** Provider has no global observer hook. BLoC and Riverpod adapters come first.
- **Release-mode apps.** They expose no VM service, so this is an inherent limit.

## 4. Anti-goals

Each of these would make the project a failure even if every feature worked:

- **It feels like DevTools.** Lag on big data, spinners that flash, raw dumps without answers, or a tool switch that loses your place.
- **The UI thread stalls.** No I/O, parsing or work proportional to session size may ever happen on the UI thread. One dropped frame caused by data ingest is a bug.
- **Clutter.** No panels added for parity's sake. Every screen earns its place by answering a question.
- **Setup before value.** The app must be useful without the probe package. The probe only adds extras.
- **Distorting what we measure.** Default observation settings must not noticeably slow the app being profiled. High-overhead options are off by default and labelled.
- **Data leaving the machine.** No telemetry about the user's app, and no cloud dependency.
- **A web app in disguise.** Non-native shortcuts, hover lag or generic-looking widgets.

## 5. Constraints

These are facts about the platform. They were checked against Flutter 3.47.9 / Dart 3.13.7 and `vm_service` 15.4.0 on the author's machine.

### 5.1 All data comes from the Dart VM service

The Dart VM service is JSON-RPC over WebSocket, exposed by debug and profile builds. Flutter adds service extensions and extension events on top of it.

| Need | Source | Modes |
|------|--------|-------|
| Per-frame timings | `Flutter.Frame` events on the `Extension` stream. Fields, all in µs: `number`, `startTime`, `elapsed`, `build`, `raster`, `vsyncOverhead`. | debug, profile |
| Phase detail (build, layout, paint, raster slices) | VM timeline. Set recorded streams with `setVMTimelineFlags`, then fetch incrementally with `getPerfettoVMTimeline` (time origin/extent). `getVMTimeline` returns JSON as a fallback. | profile (debug timings are not representative) |
| Widget builds per frame, profile mode | `ext.flutter.profileWidgetBuilds` / `ext.flutter.profileUserWidgetBuilds`. These add a timeline event per widget build. There are equivalents for layouts and paints. | debug, profile; **adds overhead** |
| Rebuild counts with source locations | `ext.flutter.inspector.trackRebuildDirtyWidgets` emits `Flutter.RebuiltWidgets` events. Locations come from `widgetLocationIdMap`. Repaint counts use `trackRepaintWidgets` and `Flutter.RepaintWidgets`. | **debug only** (registered inside `assert`) |
| CPU hot functions | `getPerfettoCpuSamples` / `getCpuSamples` for a time range | profile |
| Widget tree | `ext.flutter.inspector.*`, including `getRootWidgetTree`, `getDetailsSubtree`, `getProperties`, `setSelectionById`, `getLayoutExplorerNode`, `screenshot`, `show` | **debug only** |
| Memory overview | `getMemoryUsage`, `getAllocationProfile`, `GC` stream | debug, profile |
| Heap graph | `requestHeapSnapshot`, which sends binary chunks on the `HeapSnapshot` stream | debug, profile |
| Retaining paths | Computed locally from the heap graph. `getRetainingPath` / `getInboundReferences` work for live object ids. | debug, profile |
| HTTP | `ext.dart.io.httpEnableTimelineLogging`, `ext.dart.io.getHttpProfile` (with `updatedSince`), `getHttpProfileRequest`, `clearHttpProfile` | debug, profile |
| Sockets, WebSockets | `ext.dart.io` socket profiling extensions (the builder verifies the exact names) | debug, profile |
| Logs and errors | `Logging` stream (`dart:developer log`), `Stdout`/`Stderr`, `Flutter.Error` (structured errors), `Flutter.Navigation` (route changes) | debug, profile |
| Finding running apps | **Step 1, find the DTD instances.** Each running Dart Tooling Daemon (DTD) writes a registry file named after its pid into the Dart support directory. On macOS that is `~/Library/Application Support/Dart/dtd/<pid>`, a JSON file with `wsUri`, `epoch`, `pid`, `dartVersion` and `workspaceRoot`. `dart tooling-daemon --list --machine` prints the same list. **Step 2, find the apps.** Call `ConnectedApp.getVmServices` on each DTD. Flutter apps register automatically in debug and profile mode unless run with `--no-dds`. | — |

### 5.2 Consequences that shape the design

- **Jank and rebuilds are measured in different modes.** Jank must be measured in profile mode. The precise rebuild counter and widget inspector exist only in debug mode. In profile mode we get per-widget build events from the timeline, which lets us attribute rebuilds to a specific slow frame. That combination is a key differentiator. In debug mode we additionally get source locations.
- **HTTP recording is off by default in Dart** (`HttpClient.enableTimelineLogging = false`). Anything sent before a tool turns it on is lost. When recording is on, the profile covers `dart:io` traffic plus clients that report through `package:http_profile`: `cupertino_http`, `cronet_http` and `ok_http`. *Correction to what was said in conversation: native clients are not invisible. The real gap is requests sent before recording starts.*
- **Flutter reports object creation and disposal in debug mode** (`FlutterMemoryAllocations`, used by about 20 framework classes such as controllers and notifiers). Code inside the app can use this to catch objects that were disposed but never freed. The `leak_tracker` package does exactly this.
- **The Dart SDK already ships an MCP server** (`dart mcp-server`). It gives agents hot reload, hot restart, runtime errors, the widget tree, logs and app launching. We must not duplicate it.
- **The VM service limits some speeds.** We can make the UI instant, but transfer times for heap snapshots and very large timeline pulls are partly set by the VM. The UI must stay responsive and show progress while data streams in.
- **Recorded HTTP bodies live in the profiled app's heap.** While HTTP logging is on, `dart:io` keeps every request and response body in the app's memory until `clearHttpProfile` is called. Over a long session this inflates the app's memory numbers. See §8.3 Network for how we handle it.

### 5.3 Supported versions (design call)

- **Flutter.** The current stable release plus the three before it, roughly a year of releases. Recorded VM service fixtures and the CI `jank_lab` matrix cover the oldest and newest of that range.
- **Fallback code paths.** Paths such as the JSON timeline and `getCpuSamples` exist only if a supported release needs them. The builder checks this against the range.
- **Probe package.** Its minimum SDK matches the oldest supported Flutter. It targets current majors: `bloc` 9+ and `flutter_riverpod` 3+. Riverpod 3 changed `ProviderObserver`, and Riverpod 2 is not supported.
- **SDK-internal fields.** The versioned table of fields used for leak detection (§8.3 Memory) has one entry per supported release where the fields differ.

## 6. Approaches considered

### 6.1 UI framework (decided: GPUI)

| Option | Upside | Trade-offs | Verdict |
|--------|--------|------------|---------|
| **GPUI (Zed's framework) + `gpui-component` (Longbridge)** | Built for exactly the feel we want: GPU-drawn, sharp text, 120 fps in Zed. Zed 1.0 reportedly shipped on macOS, Windows and Linux in April 2026. `gpui-component` (Apache-2.0) provides a virtualized DataTable for hundreds of thousands of rows, trees, docked and split panels, charts, themes and Lucide icons. Longbridge ships a trading app on it. | The crates.io release reportedly lags Zed's main branch by about a year, so we pin a git revision and absorb API churn on upgrades. Docs are thin, so we learn from Zed's source. Smaller community. | **Chosen** |
| egui | The most mature Rust UI library and the largest community. Proven on huge timelines by Rerun. Fastest route to a prototype. | Looks like egui unless you fight it. Linear/Raycast-level polish in typography, animation and native window feel is uphill. | Rejected: wrong side of the polish requirement |
| Tauri (Rust core + web UI) | Best tooling for polished UI work. | A different browser engine on each OS, and WebKitGTK on Linux is slow. Huge timelines need WebGL, and all data must cross from Rust to JavaScript. Hardest place to guarantee 120 fps. | Rejected |
| Iced, Slint, Makepad, Xilem | — | Iced trails GPUI on large tables and animation. Slint targets embedded. Makepad and Xilem are too young. | Dropped early |

### 6.2 How data is captured (decided: attach, plus an optional in-app package)

| Option | Trade-offs | Verdict |
|--------|-----------|---------|
| Attach only | Zero setup. Loses requests sent before connecting. Leak detection is limited to heap analysis. State changes are not visible at all. | Rejected |
| **Attach + optional probe package** | Everything works by attaching. One debug-only dependency (`fdt_probe`) unlocks capture from app launch, confirmed leak detection, and state-management events (BLoC, Riverpod). The cost is maintaining a Dart package alongside the Rust app. | **Chosen** |
| Probe required | Simplest to build, but nobody can try the app without setting it up first. That is a poor first impression for a tool meant to beat DevTools. | Rejected |

### 6.3 Main window layout (decided: B, separate tools, with four additions)

Three layouts were mocked up:

- **A:** one timeline across the top of every view
- **B:** a sidebar of separate tools
- **C:** a findings-first inbox

The user chose **B**. Plain B loses your place when you switch tools, which is one of the original complaints. Four additions fix that: a shared time range, back/forward history, a Timeline page and cross-links (§8.2). The mockup is saved next to this document as `2026-11-18-flutter-devtools-rs-layout-mockup.html`.

## 7. Decisions made in conversation

- Rust and native desktop, on macOS, Windows and Linux.
- v1 tools: Performance (frames and jank), Memory (leaks), Network, Logs, Widget tree with rebuild hotspots, and State. All are tied together by a shared session timeline, and sessions can be exported.
- Data capture: attach with zero setup, plus an optional probe package.
- AI: expose our findings to coding agents over MCP. Do not rebuild the Dart MCP server. Do not build our own AI diagnosis.
- Out of scope for this project: the widget sandbox and the device matrix. Each is a separate later project.
- Layout: B with the four additions.
- UI framework: GPUI with `gpui-component`.
- Process: Sonnet subagents implement and review each task. Opus does the final review.
- **Open source** in a public repository. The licence was left to the designer; see §12.1.
- The author has **Windows and Linux machines** and checks every milestone on them.
- The author has, or will have, an **Apple Developer ID and a Windows code-signing certificate** for release builds.

## 8. Product design

### 8.1 Connecting

- **Start screen.** It lists running Flutter apps with no setup.
  - **How apps are found.** `fdt` watches the DTD registry directory (§5.1) and connects to each DTD listed there. Registry entries whose pid is no longer running are ignored. It then lists every app each DTD knows about: app name, device, mode (debug or profile), Flutter version and workspace folder. The list updates live as apps start and stop.
  - **Locations on each OS.** The builder confirms the registry directory on Windows and Linux (the Dart support directory under `%APPDATA%` and the XDG locations). If a location can't be determined, `fdt` falls back to running `dart tooling-daemon --list --machine` with the `dart` found on `PATH` or in a configured SDK.
  - **Connecting.** Press Enter on an app to connect.
  - **Manual options.** A field accepts a pasted VM service URL in either the `http://…/token=/` or `ws://…/ws` form, which covers `--no-dds` and remote cases. Recent session files are listed too.
- **One connected app per window.** Multiple windows are allowed.
- **On connect**, the app does the following:
  1. Subscribes to the streams it needs.
  2. Turns on HTTP and socket profiling immediately.
  3. Starts recording frames, logs, GC and memory samples.
  4. Detects the mode and the probe version.
- **Mode banner.** In debug mode, the Performance tool shows a persistent, non-blocking banner saying debug timings are not representative, with a one-line hint on running in profile mode. In profile mode, the Widget tree tool explains that it needs debug mode.
- **Disconnects** (app restarted, device unplugged) keep all recorded data. A banner offers automatic reconnection to the same app. "Same app" means the same app id, device and workspace, found again through DTD.
- **Runs within a session.**
  - **Hot reload and hot restart** continue the current run.
  - **A full restart or a reconnect** to a new process continues the same session as a new **run**.
  - **What a run carries.** Each run has its own VM clock, isolate ids and object ids. The store, time axis, cross-link ids, MCP ids and `.fdtsession` format all carry a run id from the start.
  - **On screen.** Timelines place runs one after another, with a labelled divider. The time range can span runs.
  - **Starting fresh.** "New session" (⌘N) starts with no prior data.

### 8.2 Main window

```
┌───────────────────────────────────────────────────────────────────────────────────────┐
│ ● ● ●  ‹ ›  shop_app · iPhone 16 Pro · profile  ● Rec 2:14  [◷ 0:51–1:07 ✕]   [⌕ ⌘K]  │
├──────────────┬────────────────────────────────────────────────────────────────────────┤
│ ⧉ Timeline   │ SESSION ▁▂▁▃▁▁▂▇█▅▃▁▂▁▁▁▂▁▁▃▁   [▓▓▓ selected range ▓▓▓]                │
│──────────────│────────────────────────────────────────────────────────────────────────│
│ TOOLS        │ Requests   WebSockets   Errors 1                                        │
│ ▤ Perf     6 │ ┌──────────────────── table (virtualized) ─────────────┬─ details ─────┐ │
│ ◎ Memory   2 │ │ GET  /v2/products?page=3  200  182 ms  ▬▬           │ GET /img/…   │ │
│ ⇅ Network  9 │ │ GET  /img/hero_banner.jpg 200  1.4 s   ▬▬▬▬▬▬▬▬     │ Related:     │ │
│ ≡ Logs   214 │ │ POST /v2/orders           500  310 ms      ▬▬▬      │ Frame #4,820→│ │
│ ⌗ Widgets    │ │ …                                                    │ CachedImage →│ │
│ ◇ State   31 │ └──────────────────────────────────────────────────────┴──────────────┘ │
│ ⇪ Export…    │                                                                         │
│ ⚙ Settings   │                                                                         │
└──────────────┴────────────────────────────────────────────────────────────────────────┘
```

- **Title bar.** It shows back/forward buttons, the connected app with device, mode and recording state, the time-range chip when a range is active, and the command palette entry point.
- **Sidebar.** The Timeline page comes first, then the tools. Each tool shows a count of findings or items within the current time range; the count is red when something needs attention. Export and Settings sit at the bottom.
- **Session strip.** A thin frame-time chart of the whole session sits at the top of every tool. Drag on it to set or change the shared time range. Double-click it, or press Esc in the strip, to clear the range.
- **Tool body.** Each tool has its own tabs, a virtualized main list or chart, and a details pane on the right. The details pane includes a **Related** section of cross-links.
- **The four additions to layout B:**
  1. **Back and forward.** ⌘[ and ⌘] (Ctrl on Windows and Linux), plus mouse back/forward buttons, move through a history of (tool, selection, time range).
  2. **A time range that follows you.** A range set in any tool filters every tool until it is cleared, and the chip shows it.
  3. **A Timeline page.** It shows all lanes together: frames, network, logs, state, GC and navigation. Session export starts here.
  4. **Cross-links.** Each one is a single click: frame ↔ requests and log lines at that moment ↔ rebuilt widgets; leak ↔ the screen visit that caused it; request ↔ the frame that decoded its image.

### 8.3 Tools

**Timeline page**
- Lanes for frames (bars coloured by status), network (spans), logs (ticks coloured by level), state events, GC, navigation (route changes as labelled markers), and findings (markers).
- Scroll to zoom and drag to pan. Drag on the lanes, or on the session strip, to set the range.
- Hovering an item shows a compact tooltip. Clicking jumps to that item in its tool, with the range kept.

**Performance**
- **Frames tab.** A frame chart with an 8.3 ms or 16.7 ms budget line, matched to the device's refresh rate (from `Flutter.Frame` data and device info). Below it is a frames table: number, total, build, raster, widgets rebuilt.
- **Slow frame details.** Selecting a slow frame opens its explanation:
  - a phase breakdown (build, layout, paint, raster) with the phase that ran over highlighted
  - **the reason in one sentence**, for example "setState in CartBadge rebuilt the whole ProductGrid (386 widgets)", produced by heuristics in `fdt_analysis`
  - the most-rebuilt widgets in that frame
  - the busiest Dart functions in that frame (from CPU samples)
  - related requests and log lines
- **CPU tab.** Bottom-up, top-down and flame chart views for the current time range.
- **Rebuilds tab.**
  - In profile mode: per-widget build counts per frame and per range, from timeline build events. This needs "Trace widget builds" on, which is off by default and labelled with an overhead note.
  - In debug mode: rebuild counts with source file:line from `trackRebuildDirtyWidgets`. Each has an "Open in editor" action through the IDE's URL scheme or a configured command.
- **Raw trace tab.** A full Perfetto-style track view of the timeline for the range, drawn natively (not an embedded Perfetto UI).

**Memory**
- A memory chart (heap used and capacity, external, RSS) with GC markers on the shared time axis.
- **Classes tab.** The allocation profile with instance counts and sizes, and deltas when a range is selected.
- **Leaks tab.** It uses three levels of evidence:
  1. **Single-snapshot scan (any app).** Takes a heap snapshot and flags lifecycle objects that are dead but still reachable. Examples: `State` objects that are no longer mounted, defunct `Element`s, and disposed `ChangeNotifier`s or controllers where the snapshot can tell. Each leak shows its **retaining path** as a readable chain from the object to its GC root, with retained size and source locations where known. *The exact private fields used to detect "dead" are SDK-internal; the builder validates them against the SDK and keeps them in one versioned table.*
  2. **"Go there and back" diff (any app).** Snapshot, visit a screen and leave it, snapshot again. The diff shows classes from the user's own packages whose live counts grew, with retaining paths for samples.
  3. **Confirmed leaks (probe, debug mode).** Objects reported as disposed through `FlutterMemoryAllocations` that are still alive after several GCs, with the creation location where available.
- **Snapshot tab.** A dominator tree browser with retained sizes and search by class or library.

**Network**
- **Requests tab.** Method, path, status, duration, size and a waterfall. Filters cover method, status class, host and text. Bodies are shown pretty-printed (JSON, form, text, image preview), along with request and response headers, timing breakdown and errors.
- **Body capture (design call).**
  - **Copied when they arrive.** Bodies are copied into the session as soon as each request completes, not on click. They therefore survive disconnects, appear in exports and work offline.
  - **Size caps.** Each body is stored up to 1 MB and truncated beyond that, with the full size noted. While still connected, a "Load full body" action fetches the rest. All bodies share a 200 MB budget inside the recording cap, evicted oldest first. Both caps are configurable.
  - **The app's own copy.** By default `fdt` does **not** clear the app-side HTTP profile, because that would wipe data for DevTools or an IDE attached at the same time. Two measures limit the distortion:
    - The Memory tool shows how much of the app's heap the HTTP profile holds, where the snapshot lets us attribute it, and labels it "held by HTTP recording".
    - A setting, "Free app memory after copying requests", turns on periodic `clearHttpProfile` after each copy.
- **WebSockets tab** and **Errors tab** (failed or 4xx/5xx requests).
- **Related links:** the frame during which the response was processed, log lines at that time, and the widget that loaded an image when it can be determined.

**Logs**
- Merges `Logging`, `Stdout`/`Stderr`, `Flutter.Error` (rendered as structured errors with the widget summary) and navigation events.
- Filters for level, logger name and source. Search supports substring and regex. A follow mode tails new lines, and selecting a line pauses following.
- Multi-line records collapse to one line and expand on Space or click.
- Each line has a timestamp that links to the frame at that moment.

**Widget tree** (debug only)
- A summary tree with a full-tree toggle. The tree is virtualized for 50k+ nodes and depth over 200, with expand and collapse by keyboard.
- Selection syncs both ways with on-device select mode.
- The details pane shows properties, the render object and the layout explorer for flex widgets. A rebuild-count badge appears per node when rebuild tracking is on.
- Search by widget type or text, with matches highlighted and expanded.

**State** (requires the probe)
- A table of BLoC and Riverpod events: time, source (the bloc or provider name), event or transition, and the new state as a summary that expands to its full value.
- Grouped by source. Each event links to the frames and requests around it.

### 8.4 Command palette and keyboard

- **⌘K / Ctrl+K** opens the palette. It holds commands (every action in the app), navigation (tools, frames by number, requests by path, widgets by type, log search) and settings. Matching is fuzzy, and recently used items rank first. Each result shows its shortcut.
- **Global shortcuts.** On Windows and Linux, Ctrl replaces ⌘.

  | Shortcut | Action |
  |----------|--------|
  | ⌘1–⌘7 | Switch tools (Timeline first) |
  | ⌘F | Search in the current tool |
  | ⌘⇧F | Search everywhere |
  | ⌘[ / ⌘] | Back / forward |
  | Esc | Clear the selection, then the range |
  | J / K or ↑ / ↓ | Move through rows |
  | ← / → | Collapse / expand |
  | Space | Expand a record |
  | Enter | Open details |
  | ⌘E | Export the session |
  | ⌘⇧S | Take a heap snapshot |
  | ⌘R | Start or stop recording |
  | ⌘, | Settings |

- **Every list** supports type-to-filter.
- **A shortcut cheat sheet** opens with `?`.

### 8.5 Session recording and export

- **Rolling buffer.** Recording continues while connected, capped by time and memory. The default is 30 minutes or 1 GB of in-memory data, whichever comes first, and both are configurable. When the cap is reached, the oldest data is evicted. Heap snapshots and anything the user pins are kept until removed.
- **Export.** The user can export one of three things:
  - the last N seconds (60 by default)
  - the selected range
  - the whole session

  Heap snapshots are included only if the user chooses, because they are large. The format is a single `.fdtsession` file: a versioned, zstd-compressed container with one section per stream plus metadata (app, device, mode, Flutter and Dart versions, probe version, time base).
- **Opening a session file** shows the same UI, read-only, with an "Offline" badge. All tools, cross-links and searches work.
- **Forward compatibility.** The format carries a version, and newer apps open older files.

### 8.6 Agent access (MCP)

- The running app hosts a **read-only** MCP server bound to localhost with a per-run token. Agents launch `fdt mcp`, a stdio bridge in the same binary, which finds the running app through a per-user discovery file and proxies to it.
- **Tools exposed:**
  - `list_sessions`
  - `get_findings(range?)`
  - `get_slow_frames(range?)`
  - `explain_frame(id)`
  - `get_rebuild_hotspots(range?)`
  - `get_leaks()`
  - `get_retaining_path(id)`
  - `get_requests(filter, range?)`
  - `get_request(id)`
  - `search_logs(query, range?)`
  - `export_session(range?, path)`
- Responses are compact structured JSON designed for agents: summaries first, with ids for drill-down.
- There are no actions on the app. Hot reload and similar actions remain the Dart MCP server's job.

### 8.7 Settings (small on purpose)

- Theme: System, Dark or Light.
- Recording caps.
- Default for "Trace widget builds".
- Editor integration ("Open in editor" command or URL scheme).
- Probe status and help.
- MCP on or off.
- Update check on or off.

### 8.8 The optional probe package (`fdt_probe`, Dart)

- **Setup.** Add a dependency and call `FdtProbe.init()` at the start of `main`. In release mode it compiles to a no-op and is tree-shaken out.
- **What it adds:**
  - **Capture from app launch.** It turns on HTTP and socket profiling before the first request. It also buffers navigation and state events in a bounded ring (5,000 events by default) until a tool connects; the tool drains the ring through a service extension.
  - **State events.** A `BlocObserver` adapter and a Riverpod `ProviderObserver` adapter send events through `developer.postEvent`. Values are summarised (type name plus truncated `toString`) with a size cap.
  - **Confirmed leaks (debug).** Built on `FlutterMemoryAllocations` disposal events with weak references and GC checks. The builder should evaluate depending on `package:leak_tracker` before reimplementing this.
- **Protocol.** Service extensions under `ext.fdt.*` and events named `fdt.*`, with a protocol version. The app shows when the probe is present, its version, and a clear message on a version mismatch.
- **Discovery.** Each tool's empty state mentions the probe only where it would help, for example State, and Network when the session started late.

## 9. Architecture

### 9.1 Workspace layout

```
crates/
  fdt_vm/        VM service and DTD clients: JSON-RPC over WebSocket, typed subset, streams, reconnect
  fdt_store/     time-indexed session store: append-only chunks per stream, immutable snapshots for readers
  fdt_analysis/  frame explanation, rebuild attribution, CPU aggregation, heap graph,
                 dominators and retaining paths, leak scans, cross-stream correlation
  fdt_session/   .fdtsession export and import
  fdt_mcp/       MCP server and stdio bridge
  fdt_ui/        design system: tokens, themes, core components, custom canvases (charts, lanes, flame)
  fdt_app/       GPUI application: windows, tools, palette, navigation history; builds the `fdt` binary
probe/           Dart package fdt_probe
fixtures/
  jank_lab/      Flutter app with deliberate jank, leaks, failing requests, state churn, deep trees
  recordings/    recorded VM service transcripts and stress datasets for tests and benchmarks
```

The core crates (`fdt_vm`, `fdt_store`, `fdt_analysis`, `fdt_session`, `fdt_mcp`) have **no GPUI dependency**. They can be tested headless, and they isolate us from GPUI churn.

### 9.2 Threads and data flow

```
VM service / DTD ──WebSocket──► fdt_vm (async runtime, own threads)
                                  │ decoded events
                                  ▼
                               fdt_store (append, index)  ──► analysis jobs (worker pool)
                                  │ new generation published                 │ results
                                  ▼                                          ▼
                     UI thread: reads an immutable snapshot of the current generation;
                     at most one redraw per display frame; work is proportional to what's visible
```

- **The UI thread** never does I/O, decoding or work proportional to session size. All of that happens on the async runtime or the worker pool.
- **The store** publishes generations. The UI coalesces change notifications into at most one redraw per display frame.
- **Long jobs** (heap snapshot analysis, big searches) are cancellable, report progress, and stream partial results.

### 9.3 Rendering large data

- **Tables, trees and logs** are virtualized. Only visible rows are built (`gpui-component` DataTable, GPUI uniform lists). Row heights are fixed per view so virtualization stays O(1).
- **Charts, lanes, flame charts and the session strip** are custom GPUI elements that paint batched primitives. Each stream keeps incremental **level-of-detail summaries** (per-bucket min/max/count at power-of-two zoom levels). Drawing cost therefore depends on pixel width, not event count. Labels are drawn only where they fit.
- **Search.** Log text lives in contiguous arenas. Substring search uses SIMD `memchr::memmem` across cores, and regex uses the `regex` crate. Results stream in, so typing never waits on the full scan.

### 9.4 Dependencies (starting points; the builder can substitute equivalents)

| Purpose | Crates |
|---------|--------|
| UI | `gpui` (pinned git revision), `gpui-component` |
| Async, networking, JSON | `tokio`, `tokio-tungstenite`, `serde`, `serde_json` |
| Perfetto protobuf decoding | `prost` |
| Parallel analysis | `rayon` |
| Search | `memchr`, `regex` |
| Compression | `zstd` |
| MCP | `rmcp` (official Rust MCP SDK) |
| Benchmarks | `criterion` |

GPUI and `gpui-component` versions are pinned in `Cargo.lock` and upgraded only on purpose.

## 10. UI quality requirements

UI quality is part of done. A feature that works but misses these targets is not done.

### 10.1 Reference hardware and stress datasets

**Reference machines:**
- **R1:** Apple Silicon Mac (M1 class or newer) with a 120 Hz display
- **R2:** Windows 11 laptop with a 4-core CPU and integrated graphics, 60 Hz
- **R3:** Linux laptop of the same class as R2, Wayland and X11, 60 Hz

**Stress datasets**, kept in `fixtures/recordings`, generated synthetically and supplemented by real `jank_lab` recordings:

| Dataset | Size |
|---------|------|
| Timeline | 10-minute profile session at 120 Hz: 72,000 frames and about 5 million timeline events |
| Logs | 1,000,000 log lines (~150 MB) |
| Network | 100,000 HTTP requests |
| Widget tree | 50,000 nodes, depth 200 |
| Heap snapshot | 5,000,000 objects |

### 10.2 Performance budgets

| Interaction | Budget |
|-------------|--------|
| Scroll, pan, zoom and resize on stress datasets, R1 | Frame time p99 ≤ 8.3 ms; no frame > 16.7 ms |
| Same, R2 and R3 | p99 ≤ 16.7 ms |
| UI-thread CPU per frame (layout and paint preparation) | p99 ≤ 4 ms on R1 |
| Cold start to first window paint | ≤ 200 ms on R1; ≤ 400 ms on R2/R3 |
| Cold start to interactive (palette usable) | ≤ 400 ms on R1; ≤ 700 ms on R2/R3 |
| Attach to a local app to first live data on screen | ≤ 1 s |
| Hover, press and selection feedback | The next frame |
| Command palette open | ≤ 1 frame |
| Palette results update per keystroke | ≤ 8 ms over 5,000 items |
| Switch tools (fully drawn) | ≤ 50 ms |
| Open slow-frame details | ≤ 50 ms |
| Log search over 1M lines | First results ≤ 16 ms per keystroke; full scan ≤ 100 ms on R1 |
| Network filter over 100k requests | ≤ 16 ms |
| Heap snapshot of 5M objects: parse, dominators, leak scan | ≤ 3 s on R1, with the UI fully responsive throughout |
| Open a 200 MB session file | First view ≤ 1.5 s |
| Idle memory (connected, nothing selected) | ≤ 150 MB RSS |
| Idle CPU (connected, app idle) | ≤ 1 % |
| Overhead on the profiled app at default settings | ≤ 2 % added to frame build time. Heavier options are off by default and labelled. |

### 10.3 How the budgets are enforced

- **Frame-time HUD.** A command palette action, "Show frame times", overlays live p50/p95/p99 frame times, a frame-time graph and dropped-frame counts.
- **UI benchmark harness** (`fdt bench-ui`). It replays scripted scroll, zoom, search and tool-switch sequences over the stress datasets and prints frame-time percentiles as JSON.
- **Core benchmarks** (`criterion`) cover ingest, level-of-detail building, search, dominators and session I/O.
- **CI.** Runs on macOS, Windows and Linux. It gates on core benchmarks and on whatever headless UI-thread metric the builder can measure reliably. A regression over 10 % fails the build.
- **Release checklist.** GPU frame times are verified manually on R1–R3 before each release.

### 10.4 Design system

Everything visual comes from tokens in `fdt_ui::theme`. Views never hard-code colours, sizes, radii or durations; the builder adds a check that enforces this.

**Colour.** Semantic tokens, each defined for dark and light. Starting values come from the approved mockups; the builder tunes them and verifies contrast.

| Token | Dark | Light |
|-------|------|-------|
| `bg` | `#0D0E11` | `#FBFBFC` |
| `surface` | `#131519` | `#F4F5F7` |
| `elevated` | `#1A1C22` | `#FFFFFF` |
| `border` | `#23262D` | `#E3E5E9` |
| `text` | `#E7E8EB` | `#16181D` |
| `text-muted` | `#8A8F99` | `#5C6370` |
| `text-faint` | `#5B606A` | `#8B919C` |
| `accent` | `#7B8CFF` | `#5B6CF0` |
| `success` / frame ok | `#3FB27F` | `#1F9D63` |
| `warning` / frame slow | `#E2A336` | `#B7791F` |
| `danger` / frame janky | `#E5534B` | `#D23B33` |
| `lane-network` | `#4EA1FF` | `#2F80ED` |
| `lane-state` | `#B07CFF` | `#8B5CF6` |

- **Contrast** meets WCAG AA in both themes: 4.5:1 for body text and 3:1 for UI elements and chart marks.
- **Colour is never the only signal.** Janky frames are also taller and labelled, and statuses carry text or icons.
- **Lane colours** stay distinguishable under deuteranopia and protanopia.

**Typography.**
- Fonts are bundled so metrics match on every OS: **Inter** for UI text and **JetBrains Mono** for code, paths, logs and numbers. Tables use tabular figures.
- Size scale: 11 (labels and captions), **12 (default dense body)**, 13 (body), 14 (section titles), 16 (page titles), 20 (empty-state headings).
- Weights: 400, 500 and 600.
- Line height: 1.4 for UI text, 1.5 for logs.

**Spacing, shape and density.**
- Spacing uses a 4-point grid: 2, 4, 6, 8, 12, 16, 20, 24, 32.
- Corner radii: 4 for chips, 6 for controls and rows, 8 for panels and popovers, 12 for dialogs.
- Row heights: 26 px for tables and trees, 22 px for log lines.
- Separation comes from borders rather than shadows; the dark theme in particular relies on them. Popovers and the palette get one soft shadow.

**Motion.**
- 100 ms for hover and press, 160 ms for panel and selection transitions, 200 ms for overlays (palette, dialogs). All use ease-out.
- Data redraws are never animated, and nothing animates while the user scrubs, pans or zooms.
- When the OS asks for reduced motion, transitions are replaced with instant changes.

**Icons and focus.**
- Lucide icons (bundled with `gpui-component`) at 14 and 16 px with a consistent stroke.
- Keyboard focus shows a 2 px accent ring. Mouse focus shows no ring.

### 10.5 States every view must design

- **Loading.** A skeleton appears only if loading takes longer than 150 ms, so spinners never flash. Long operations show determinate progress and can be cancelled.
- **Empty.** Says what will appear here and the single action that gets it, including the shortcut. For example: "No requests yet. Requests appear as your app makes them. Started recording late? Add fdt_probe to capture from launch."
- **Error.** Says in plain language what happened and what to do next. Raw details are collapsible and copyable.
- **Disconnected.** Data stays, a banner shows automatic reconnection, and export is still available.
- **Mode mismatch.** The debug or profile banners from §8.1.

### 10.6 Native feel per OS

- **macOS.** Unified title bar with traffic lights, a standard app menu (About, Settings ⌘, Hide, Quit), ⌘ shortcuts, native open and save panels, momentum scrolling and full-screen support.
- **Windows.** A custom title bar with standard caption buttons, plus Snap Layouts on hovering maximize where GPUI allows. Ctrl shortcuts and native file dialogs.
- **Linux.** Wayland and X11. Client-side decorations that follow GNOME and KDE conventions, Ctrl shortcuts, and file dialogs through the XDG portal.
- **All platforms.** Follow the OS light or dark setting by default, with a manual override. Crisp at any display scale. Right-click context menus on rows: copy as text, copy as JSON, open in editor, and pin.

### 10.7 Accessibility

- Every action is reachable by keyboard and listed in the palette.
- Contrast and colour rules are as in §10.4.
- Screen reader support depends on GPUI's accessibility support. The builder records its current state; gaps are a known limitation, not a blocker for v1.

## 11. Testing strategy

- **Core crates** are built test-first. VM service behaviour is tested against **recorded transcripts**: real JSON-RPC traffic captured from `jank_lab` in debug and profile mode, replayed deterministically.
- **Integration tests** run `jank_lab` as a desktop app in profile and debug mode on CI, connect `fdt` headless (core crates only), and assert on findings. Examples: the deliberate jank produces a slow frame attributed to the right widget; the deliberate leak shows up with the expected retaining path; the failing endpoint appears in Errors.
- **The probe** gets Dart unit tests plus an integration test through `jank_lab`.
- **UI** uses GPUI's test context for view logic: navigation history, range propagation, palette filtering and keyboard handling.
- **Performance** gates are as in §10.3.
- **Visual review.** Each milestone ends with screenshots of its key screens in both themes on all three OSes, checked against the design system checklist (§10.4–10.6). The author takes the Windows and Linux screenshots on their own machines.
- **CI coverage.** CI builds and runs the automated tests on macOS, Windows and Linux. It does not need screenshot infrastructure.

## 12. Distribution

### 12.1 Licence and repository

The project is open source in a public GitHub repository.

- **The app and its Rust crates: Apache-2.0.**
  - It matches `gpui` and `gpui-component`.
  - It includes an explicit patent grant.
  - Companies can adopt it without legal review.
- **The `fdt_probe` Dart package: BSD-3-Clause**, the Dart and Flutter ecosystem convention. It is compiled into other people's apps, so it must stay permissive. It is published on pub.dev; the publisher is chosen at first release.
- **Zed's GPL-3.0 crates must not be copied.** Zed's `gpui` crate is Apache-2.0, but its application crates (`ui`, `picker`, `title_bar`, `workspace` and others) are GPL-3.0. They can be read for ideas but never copied. The command palette, title bar and other chrome are built on `gpui` and `gpui-component`.
- **Licence check in CI.** `cargo-deny` enforces an allow-list of compatible dependency licences: Apache-2.0, MIT, BSD, ISC, Zlib, Unicode, and OFL for fonts.
- **Bundled assets:** Inter (OFL), JetBrains Mono (OFL) and Lucide icons (ISC).

### 12.2 Packages and releases

- **macOS:** universal DMG (arm64 and x86_64), signed with the author's Developer ID and notarized.
- **Windows:** MSI or MSIX, signed with the author's code-signing certificate.
- **Linux:** AppImage and `.deb`. Flatpak comes later.
- **CI release jobs** build all three, using signing secrets stored in CI.
- **Updates:** an update check reads the GitHub Releases feed and notifies the user of new versions. It can be turned off. There is no silent auto-update in v1, and there is no server of our own.
- **Privacy:** no telemetry. There is no crash-report server. Crashes write a local report, and on the next launch the app offers to open a pre-filled GitHub issue for the user to review and submit. Recorded data never leaves the machine unless the user exports it.

## 13. Delivery milestones

Each milestone gets its own implementation plan and ends shippable to the author for daily use.

| # | Milestone | Contents |
|---|-----------|----------|
| M1 | Foundation | Cargo workspace; GPUI shell (window, sidebar, per-OS title bar); design system tokens and both themes; command palette; navigation history plumbing; frame-time HUD; CI on all three OSes; `jank_lab` fixture app |
| M2 | Connect, Logs, Network | DTD discovery and pasted URLs; connection lifecycle and reconnect; session store; Logs tool; Network tool; session strip; shared time range |
| M3 | Performance | Frame ingest; timeline ingest and level-of-detail summaries; phase breakdown and slow-frame explanation; CPU tab; profile-mode rebuild tracing; Timeline page with lanes; cross-links |
| M4 | Memory | Memory chart; allocation profile; heap snapshot pipeline; dominators; retaining paths; single-snapshot leak scan; "go there and back" diff |
| M5 | Widget tree | Debug-mode inspector tree, details and layout explorer; rebuild counts with source links; open in editor |
| M6 | Probe | `fdt_probe` package: capture from launch, BLoC and Riverpod adapters, confirmed leaks; State tool and lane |
| M7 | Sessions and agents | Rolling buffer caps; `.fdtsession` export and import; offline mode; MCP server and stdio bridge |
| M8 | Ship | Packaging and signing on all three OSes; update check; full polish pass against §10; release performance verification on R1–R3 |

**Definition of done for every milestone:**
- Its views meet the §10.2 budgets on the stress datasets.
- Both themes are finished.
- Loading, empty, error and disconnected states are designed.
- Every action is keyboard-reachable and listed in the palette.
- It passes visual review.

## 14. Calls made in this design (please check)

These were not discussed explicitly. Each is a proposal with a reason:

1. **Working name `fdt`**, a placeholder until a product name is chosen.
2. **Bundled Inter and JetBrains Mono** instead of system fonts. Dense tables need identical metrics everywhere; native feel comes from window chrome, menus and shortcuts.
3. **BLoC and Riverpod adapters first.** Both have global observer hooks. Provider comes later.
4. **Flutter web is out of v1.** Its VM service surface and network story differ.
5. **One app per window**, with multiple windows allowed.
6. **Recording caps of 30 minutes / 1 GB by default**, with the last 60 seconds as the default export.
7. **"Trace widget builds" is off by default** because of its overhead on the profiled app.
8. **MCP is read-only.** Actions on the app stay with the Dart MCP server.
9. **No telemetry and no servers of our own.** The update check uses GitHub Releases. Crash reports go through a pre-filled GitHub issue the user chooses to submit.
10. **Leak detection without the probe** relies on SDK-internal field names, kept in one versioned table that is checked per Flutter release.
11. **Supported versions** are current stable Flutter plus the three releases before it, with `bloc` 9+ and Riverpod 3+ (§5.3).
12. **HTTP bodies are copied into the session when they arrive**, at up to 1 MB each and 200 MB in total. The app-side profile is not cleared by default, so tools attached at the same time are not affected (§8.3).
13. **A full restart continues the same session as a new run.** Run ids are built in from the start (§8.1).
14. **Licences: Apache-2.0 for the app and BSD-3-Clause for the probe.** Zed's GPL code is not copied (§12.1).

## 15. Left to the builder

- Exact crate versions, and the GPUI revision to pin (match what `gpui-component` pins).
- Perfetto protobuf compared with the JSON timeline, and the polling cadence for timeline and HTTP profile updates.
- The concrete design of the level-of-detail summaries and the store's chunk format.
- The slow-frame explanation heuristics, iterated against `jank_lab` scenarios.
- Whether to depend on `package:leak_tracker` in the probe.
- The headless UI-thread metric used for CI gates.
- How "Open in editor" is wired: VS Code and JetBrains URL schemes, plus a custom command.
- The installer tooling for each OS.
