# M2 Connect, Logs and Network Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fdt` finds running Flutter apps through DTD, or connects to a pasted VM service URL. It records the app's logs, HTTP traffic and frame timings into a session store. It shows them in the Logs and Network tools with a session strip and a shared time range, within the §10.2 budgets on 1,000,000 log lines and 100,000 requests.

**Architecture:** M2 adds three crates with no GPUI dependency.
- **`fdt_store`** is the time-indexed session store: append-only chunks, immutable snapshots, log search, request filtering and level-of-detail summaries.
- **`fdt_vm`** is the JSON-RPC-over-WebSocket client for the VM service and DTD, plus app discovery.
- **`fdt_capture`** runs a tokio runtime. It connects, ingests events into the store and handles hot restarts, disconnects, reconnects and runs.

`fdt_app` receives store snapshots over `tokio::sync::watch` channels and renders the start page, the strip, the Logs tool and the Network tool. All filtering, searching, decoding and pretty-printing happens off the UI thread.

**Tech stack:**
- Rust 1.100.0, edition 2024
- `gpui-kit` 0.10.0 (UI crates only)
- tokio 1 (already in the lockfile at 1.54.1) and `tokio-tungstenite` with no TLS
- serde/serde_json, `url`, `base64`
- `rayon`, `memchr` and `regex`
- `criterion` 0.8
- Flutter 3.41–3.50 for `jank_lab`

**Spec:** `docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md`. M2 is the second row of §13. This plan implements:
- §5.1 and §5.2 as they apply
- §8.1
- §8.2: the session strip, the time range and its chip, sidebar counts and the title bar
- §8.3 Logs and Network
- the §8.4 shortcuts that act on M2 data
- §9
- the M2 rows of §10.2 and §10.3, including `fdt bench-ui`
- §10.5
- §11

**Inputs carried from M1:** `docs/milestones/m1-review.md` ("Carry into M2") and `docs/milestones/m1-deferred-findings.md` (the parked items).

## Scope

**M2 delivers** the M2 row of §13:
- DTD discovery and pasted URLs
- the connection lifecycle and reconnect
- the session store
- the Logs tool and the Network tool
- the session strip
- the shared time range

It also delivers the M1 carry-overs:
- a palette item model with parameters, on a virtualised list
- `Shell && !Input` key contexts
- pages that own their scrolling
- settings that tolerate one bad field
- run ids in selections and on the time axis
- window actions that work while a dialog is open
- the logger's broken-pipe fix

**Deferred on purpose.** Each item is owned by a later milestone or blocked by the platform:
- **Cross-links and the details pane's "Related" section** (M3 row). M2 stores the run ids and times that cross-links need.
- **The Timeline page's lanes** (M3).
- **Refresh-rate detection** (M3). Until then the strip measures against a 16.7 ms budget.
- **Rolling recording caps, `.fdtsession` export/import, and recent session files on the start page** (M7). The start page lists recent pasted URLs instead.
- **"Held by HTTP recording" in the Memory tool, and recording GC and memory samples on connect** (M4).
- **The debug-mode banner in Performance** (M3) **and the profile-mode explanation in Widget tree** (M5).
- **Probe detection and the probe hint in empty states** (M6). The probe does not exist yet.
- **WebSocket message contents.** The VM service does not expose them (fact P13). M2 shows the handshake, open/closed state and byte counters.
- **The >10 % CI regression gate (§10.3).** Hosted runners vary by more than 10 % between runs. M2 keeps M1's approach: absolute budget tests in release mode, plus `fdt bench-ui` runs recorded in the review.
- **"Open in editor" and "Pin" row actions** (M5, M7). M2 adds Copy and Copy as JSON.

## Calls made in this plan (please check)

1. **A new crate, `fdt_capture`**, which is not in spec §9.1. It holds the connection lifecycle and the VM-event-to-store ingest. Keeping it out of `fdt_vm` (protocol) and `fdt_store` (data) leaves both independent and lets integration tests run headless.
2. **Stress datasets are generated in code** (`fdt_store::synth`, fixed seed) instead of being stored in `fixtures/recordings`. A million log lines is about 150 MB and does not belong in git. Real `jank_lab` recordings are committed, as the spec asks.
3. **The session time axis is the host's clock.** `SessionTime` is nanoseconds since the session started, meaning the first successful connect or ⌘N. Each run maps the app's three clocks onto it through a calibration taken at connect (fact P6). Events from before the session started are clamped to 0 and marked.
4. **Discovery polls the DTD registry folder once a second** and keeps one DTD connection per entry. DTD's own events do not report `flutter run` apps (fact P3).
5. **Bodies whose `Content-Length` exceeds 4 MiB are not copied automatically.** Fetching one costs the app a JSON frame about 3.6 times the body size (fact P11). "Load full body" fetches it on demand. Smaller bodies are copied when the request completes, capped at 1 MiB each and 200 MiB in total, as the spec says.
6. **Multi-line log records expand inline as extra 22 px rows,** so the list keeps a uniform row height and O(1) layout at 1M lines. The details pane shows the full record.
7. **The `flutter: ` prefix that Flutter adds to `print` output is removed** from Stdout lines.
8. **Connecting lands on the Logs tool.** The Timeline page has no lanes until M3.
9. **Tools that M2 does not build** (Timeline, Performance, Memory, Widgets, State) show a plain "not available yet" state while an app is connected, instead of a misleading "connect an app".
10. **⌘⇧F (search everywhere) opens the palette.** Its data items (requests by path, "Search logs for …") make it a search over the session.

## Global constraints

**Carried from M1 (unchanged)**
- **Toolchain.** Rust `1.100.0`, edition 2024. Every crate sets `license = "Apache-2.0"` and `publish = false`.
- **UI dependency.** UI crates depend only on `gpui-kit = "=0.10.0"`. Import GPUI items as `gpui_kit::…`, components as `gpui_kit::component::…` and base items as `gpui_kit::base::…`.
- **Glob imports.** Never `use gpui_kit::*`; import names explicitly.
- **Zed code.** Never copy from Zed's GPL-3.0 crates.
- **Design tokens.** Views take every colour, size, radius and duration from `fdt_ui::tokens`. `crates/fdt_ui/tests/token_usage.rs` enforces this. The only escape hatch is a same-line `// token-check: allow: <reason>`. Unit tests go at the bottom of each file in `#[cfg(test)] mod tests`.
- **Shortcuts.**
  - Shortcuts use ⌘ on macOS and Ctrl elsewhere, written as `keymap::platform("cmd-r", "ctrl-r")`. Never use `secondary-…`.
  - Elements that tests find get `.id("…").test_support()`.
  - An action run from inside a dialog must close the dialog, focus the shell, then `window.dispatch_action`.
- **Copy.** User-facing copy is sentence case and plain. It never mentions milestones or versions of `fdt`. The product name is `fdt`.
- **Commits.** Use conventional prefixes, and end each message with the executing session's `Co-Authored-By` line.

**New in M2**
- **Core crates have no GPUI.** `fdt_store`, `fdt_vm` and `fdt_capture` must not depend on `gpui-kit`, directly or through another crate. They are tested with `#[test]` and `#[tokio::test]`.
- **The UI thread.**
  - No I/O, JSON decoding or work proportional to session size runs on the UI thread.
  - Filtering, searching, counting, pretty-printing and hex formatting run in `cx.background_spawn` or on the capture runtime.
  - A view renders only visible rows.
- **Snapshots.**
  - Views read the latest `Arc<fdt_store::Snapshot>`; they never hold a lock on the writer.
  - The capture publishes at most one snapshot per 16 ms.
  - An idle connected app causes no publishes and no UI work, apart from a 1 Hz recording clock.
- **Ids carry runs.** Every stored record has an `EntryId { run, seq }`, and selections in `Location` use it. A VM-side id (isolate, request or object) is never used as a session id.
- **Network access.**
  - `fdt` only opens WebSockets to DTD and VM-service addresses it discovered or that the user pasted.
  - It makes no other network calls.
  - It sends no telemetry.
- **Recordings and privacy.** Committed recordings and test fixtures contain no auth tokens and no home-directory paths. Task 6 adds a test that enforces this.
- **Row heights.** Log rows are 22 px and use JetBrains Mono with line height 1.5. Table rows are 26 px.
- **Displayed log text.** A log row shows at most `LOG_ROW_MAX_CHARS = 512` characters of the record's first line. The store keeps up to 1 MiB of each record, and search covers all of it.
- **States (§10.5) every M2 view designs:** loading (a skeleton or spinner only after `motion::LOADING_DELAY` = 150 ms), empty, error (plain message plus collapsible, copyable details), disconnected (data stays; banner) and "not available yet".
- **Keyboard.** Every new action is in `palette::registry::commands()`, so it appears in the palette and the cheat sheet, and has the §8.4 shortcut where one exists:

  | Keys | Action |
  |---|---|
  | ⌘F | Search in the current tool |
  | ⌘⇧F | Search everywhere |
  | ⌘R | Start or stop recording |
  | ⌘N | New session |
  | J / K / ↑ / ↓ | Move through rows |
  | ← / → | Collapse / expand |
  | Space | Expand a record |
  | Enter | Open details |
  | Esc | Clear the selection, then the range |

  ⌘⇧N opens a new window; spec §8.1 allows multiple windows.

**Budgets that apply in M2 (§10.2), on R1 unless noted**

| Measure | Budget | Checked by |
|---|---|---|
| Scroll on stress datasets | frame p99 ≤ 8.3 ms, no frame > 16.7 ms (R2/R3 p99 ≤ 16.7 ms) | `fdt bench-ui` (Task 21) |
| UI-thread work per frame | Draw p99 ≤ 4 ms | `fdt bench-ui` |
| Attach to a local app → first live data on screen | ≤ 1 s | Task 23 |
| Log search over 1M lines | first results ≤ 16 ms per keystroke; full scan ≤ 100 ms | release budget tests (Task 2) + bench-ui |
| Network filter over 100k requests | ≤ 16 ms | release budget test (Task 3) + bench-ui |
| Switch tools (fully drawn) | ≤ 50 ms | bench-ui |
| Palette results per keystroke | ≤ 8 ms over 5,000 items | existing budget test (kept green in Task 20) |
| Idle memory, connected | ≤ 150 MB RSS | Task 23 |
| Idle CPU, connected, app idle | ≤ 1 % | Task 23 |
| Overhead on the profiled app | ≤ 2 % added to frame build time | Task 23 |

## Review focus

These are the inputs most likely to hurt a real user that feature tests would not exercise by default. Each one has a test in the task that owns the code.

1. **A pathological log record**, either one 200 KB line (minified JSON) or 10,000 lines.
   - The list keeps scrolling smoothly. The row shows at most 512 characters, and expanding shows at most 200 continuation rows plus "Show all in details".
   - Search still finds text past the 512th character.
   - *(Tasks 2, 16)*
2. **A huge, binary or lying response body**: 50 MB, a PNG, invalid UTF-8, or invalid JSON served as `application/json`.
   - There is no stall and no crash. Over-limit bodies are not fetched.
   - Binary bodies show a hex preview, and invalid JSON shows as text with a note.
   - *(Tasks 10, 19)*
3. **Pasting a URL in an unexpected form**: surrounding whitespace, a missing trailing slash, `ws://…/ws`, a DevTools URL with `?uri=`, `https`, garbage, a port with nothing listening, or a web server that is not a VM service.
   - Each gives a specific plain message.
   - Connecting never waits more than 5 s.
   - *(Tasks 5, 14)*
4. **The app dies at any point**: during the connect sequence, mid-request or mid-body-fetch (socket closes with 1006).
   - Recorded data stays and the run ends.
   - The status becomes Disconnected, pending body fetches become "unavailable", and nothing panics.
   - *(Tasks 10, 11)*
5. **Fast or hostile search input**: typing quickly, an invalid regex `(`, a catastrophic-looking regex `(a+)+$`.
   - An invalid regex shows an inline error and leaves the rows as they were.
   - The `regex` crate's linear time bounds every search.
   - Each keystroke cancels the previous scan, so work never queues.
   - *(Tasks 2, 17)*

## Facts verified while planning (read before starting)

Two spikes on this machine (Flutter 3.47.9 / Dart 3.13.7, `gpui-kit` 0.10.0) produced these facts.
- **Protocol facts (P1–P17)** come from captures of `jank_lab` in debug, profile and `--no-dds` mode.
- **UI facts (U1–U12)** come from compiled tests against our pinned crates.

### Protocol

- **P1. Every `flutter run` starts its own DTD,** hosted by its DDS process.
  - The DTD writes a registry file `<registry dir>/<DDS pid>` containing:
    ```json
    {"wsUri":"ws://127.0.0.1:51569/<token>","epoch":1795074978912,"pid":77473,"dartVersion":"3.13.7 …","workspaceRoot":"<cwd of flutter run>"}
    ```
  - `pid` is the DDS pid, not the app's pid.
  - The registry directory is `$DART_DATA_HOME/dtd` if that variable is set. Otherwise:
    - macOS: `~/Library/Application Support/Dart/dtd`
    - Windows: `%LOCALAPPDATA%\Dart\dtd`
    - Linux: `${XDG_STATE_HOME:-~/.local/state}/Dart/dtd`
  - **Files go stale when the app quits.** A reader must tolerate dead pids and dead `wsUri`s.
  - `--no-dds` runs have no DTD and no registry file; they are reachable only by URL.
  - `dart tooling-daemon --list --machine` prints the same entries as a JSON array, newest first.
- **P2. Reading DTD needs no secret.** The call and its response:
  ```json
  -> {"jsonrpc":"2.0","id":"1","method":"ConnectedApp.getVmServices"}
  <- {"result":{"type":"VmServicesResponse","vmServices":[{"uri":"ws://127.0.0.1:51570/<token>/ws","name":"Kind: Flutter - Device: macOS - Package: jank_lab"}]},"id":"1"}
  ```
  - `uri` is the DDS WebSocket URL, ready to connect to.
  - The `name` is the only source of device and package information. Mode, Flutter version and pid come from the VM service (P4, P5).
- **P3. DTD events report nothing useful about `flutter run` apps.**
  - The app registers inside DDS startup, before anyone can listen.
  - On quit, the DTD and VM sockets both close with code **1006**, no close frame, about 10 ms after "Application finished". No unregister event arrives.
  - DTD notifications use `params.eventKind`/`params.eventData`; VM notifications use `params.event`.
- **P4. VM service basics.**
  - `getVM` returns `pid`, `startTime` (epoch ms), `operatingSystem`, `targetCPU` and `isolates: [{id, name, isSystemIsolate, isolateGroupId}]`. System isolates are listed separately. The main isolate is the one with `name == "main"`.
  - `getIsolate` returns `extensionRPCs` and `rootLib.uri` (e.g. `package:jank_lab/main.dart`).
  - The Flutter version comes from the `Service` stream:
    1. Its `ServiceRegistered` events (replayed to late subscribers) name `method` values such as `s1.flutterVersion`. The prefix is `s1.` through DDS and `s0.` without it, so always read it from the event.
    2. Calling that method with `{isolateId}` returns `{"frameworkVersion":"3.47.9","flutterVersion":"3.47.9","dartSdkVersion":"3.13.7",…}`.
  - Duplicate `streamListen` without DDS returns error `103 "Stream already subscribed"`. Treat that as success.
- **P5. Mode.** The app is in debug mode exactly when `ext.flutter.reassemble` is in the main isolate's `extensionRPCs`; otherwise it is in profile mode. `getVM._features` says `release` in both modes, so do not use it.
- **P6. Three clocks.**
  - **Event `timestamp` and `LogRecord.time`** are epoch milliseconds.
  - **HTTP profile times** (`startTime`, `endTime`, `events[].timestamp`, `response.startTime/endTime`, `HttpProfile.timestamp`, `updatedSince`) are epoch microseconds.
  - **`Flutter.Frame.startTime` and socket profile times** are on the `getVMTimelineMicros` clock (monotonic µs).
  - **Calibration.** Call `getVMTimelineMicros` (`{"type":"Timestamp","timestamp":332217802517}`) and then `ext.dart.io.getHttpProfile` with `updatedSince: 9000000000000000`. That call returns no requests but carries `timestamp` in epoch µs. Take the two back to back, and bracket them with host-clock reads.
- **P7. `Logging` events.**
  ```json
  {"kind":"Logging","timestamp":…,"logRecord":{"sequenceNumber":0,"time":…,"level":500,
   "loggerName":{"kind":"String","id":"objects/9/0","valueAsString":"jank_lab.net"},
   "message":{"kind":"String","id":"objects/10/0","length":410,"count":128,"valueAsString":"line 1 of 30\n…","valueAsStringIsTruncated":true},
   "error":{"kind":"Null"},"stackTrace":{"kind":"Null"}}}
  ```
  - **Truncation.** Messages longer than 128 characters arrive truncated. `valueAsStringIsTruncated` is **absent**, not false, when nothing was cut. `getObject {isolateId, objectId}` returns the full `valueAsString`.
  - **Level.** It is 0 when the app gave none.
- **P8. `Stdout`/`Stderr` events.**
  - `{"kind":"WriteEvent","bytes":"<base64>","timestamp":…}`. A `print` arrives as two events: `flutter: text`, then `\n`.
  - A 1,000-line burst is 2,000 events in the same millisecond, in order.
  - `Stderr` never fired in any run.
  - `developer.log` output does not appear on Stdout.
- **P9. Flutter errors.**
  - **Debug only**, as `Extension` events with `extensionKind: "Flutter.Error"`. `flutter run` turns structured errors on by default.
  - `extensionData` carries:
    - `description` (e.g. "Exception caught by widgets library")
    - `errorsSinceReload`
    - `renderedErrorText`: the full text only when `errorsSinceReload == 0`, otherwise a one-liner
    - `properties: [...]`, which is always complete. It contains:
      - `{type:"ErrorSummary", description}`
      - `{type:"DiagnosticsBlock", name:"The relevant error-causing widget was", children:[{description:"_Thrower _Thrower:file:///…/logs.dart:103:29"}]}`
      - `{type:"DiagnosticsStackTrace", properties:[{description:"#0 _Thrower.build (package:…)"},…]}`
  - **Profile mode** sends no `Flutter.Error`. The error text arrives as `flutter: …` Stdout lines.
- **P10. Other `Extension` events.**
  - `Flutter.Frame` is `{"number":2,"startTime":<timeline µs>,"elapsed":48615,"build":3180,"raster":15895,"vsyncOverhead":29501}`, all in µs.
  - `Flutter.Navigation` is `{"route":{"description":"MaterialPageRoute<void>(/logs)","settings":{"name":"/logs"}}}`. Prefer `settings.name`.
  - `Isolate` stream kinds: `IsolateStart`, `IsolateRunnable`, `IsolateExit`, `ServiceExtensionAdded` (field `extensionRPC`) and `IsolateReload`.
- **P11. HTTP profiling.**
  - **RPCs.** `ext.dart.io.httpEnableTimelineLogging {isolateId, enabled}` and `ext.dart.io.socketProfilingEnabled {isolateId, enabled}`. Both are **off by default and reset to off on hot restart**. Requests started while profiling is off are never recorded.
  - **Polling.** `getHttpProfile {isolateId, updatedSince?}` returns `{"type":"HttpProfile","timestamp":…,"requests":[…]}` with no bodies. `updatedSince` is inclusive (`lastUpdateTime >= updatedSince`), so the same request can come back twice. Upsert by id.
  - **Request entry.** `id` is a decimal string that can be negative. Fields: `method`, `uri`, `startTime`, `endTime` (only "request sent"), `events: [{timestamp, event}]`, and `request: {headers: {name: [values]}, connectionInfo: {localPort, remoteAddress, remotePort}, contentLength, error?}`.
  - **Response.** `response: {startTime, endTime?, statusCode, reasonPhrase, headers, contentLength, error?}` appears once the headers arrive.
  - **Completion** means `response.endTime` exists, or `request.error` or `response.error` exists.
  - **In flight**, an entry has no `response` key.
  - **Failed:** `{"events":[],"request":{"error":"SocketException: Connection refused …"}}`, with no headers.
  - **Bodies.** `getHttpProfileRequest {isolateId, id}` returns the same fields plus `requestBody` and `responseBody` as **JSON arrays of byte integers**. A 256 KiB body becomes a 937 KB frame. The VM applies no cap.
  - **Clearing.** `clearHttpProfile` empties the profile, including in-flight requests, whose later updates are then lost.
- **P12. Sockets.** `getSocketProfile {isolateId}` returns every TCP socket as `{"id","startTime","endTime"?,"address","port","socketType","readBytes","writeBytes","lastReadTime"?,"lastWriteTime"?}`, with times on the timeline clock. It has no link to HTTP requests and no local port.
- **P13. WebSockets** appear in the HTTP profile only as the handshake.
  - It is a `GET http://…/ws` with `statusCode: 101`, request header `upgrade: ["websocket"]`, and `response.error: "Socket has been detached"`.
  - Its socket shows only byte counters.
  - Message frames are not observable.
- **P14. Hot reload** keeps the isolate id and sends `IsolateReload`. HTTP profiling stays on.
- **P15. Hot restart.**
  - Event order: `IsolateExit` (old) → `IsolateStart` (new id **and** new group id) → about 75 `ServiceExtensionAdded` → `IsolateRunnable`.
  - HTTP and socket profiling are **off** again, and the profile is empty.
  - The VM's `pid` and `startTime` do not change.
  - DDS keeps the stream subscriptions.
- **P16. Quitting.** The VM WebSocket closes with code 1006 and no close frame.
- **P17. `jank_lab` scheduling.** Debug builds on macOS draw no frames while the window is not in front, so a pushed route doesn't build. Any automation entry point must call `WidgetsBinding.instance.scheduleFrame()`.

### UI (`gpui-kit` 0.10.0; spike tests passed)

- **U1. Network table: `DataTable` with `TableState`** (`gpui_kit::component::table`).
  - **`TableDelegate` required methods:** `columns_count`, `rows_count`, `column(ix, &App) -> Column` and `render_td(row, col, &mut Window, &mut Context<TableState<Self>>)`.
  - **Optional methods:** `render_tr -> Stateful<Div>` (gives row ids for tests: `click(("row", 3usize))`), `context_menu(row, PopupMenu, …)`, `render_empty` and `visible_rows_changed`.
  - **`TableState`:**
    - `TableState::new(delegate, window, cx)`.
    - `.loop_selection(false)` (the default is true).
    - `selected_row()`, `set_selected_row` (scrolls the row into view), `scroll_to_row(ix, cx)` and `refresh(cx)`.
  - **Columns:** `Column::new(key, name).width(px).text_right()`.
  - **Events:** `TableEvent::{SelectRow(usize), DoubleClickedRow, RightClickedRow(Option<usize>), ColumnWidthsChanged(Vec<Pixels>), ClearSelection}`, via `cx.subscribe`.
  - **Keys.** The key context is `"DataTable"`. Add J/K by binding `gpui_kit::base::actions::{SelectDown, SelectUp}` in that context.
  - **Row height** is set only by `DataTable::with_size(Size::Size(px))`.
  - **Behaviour.** No built-in sort. At 100k rows a frame makes 54 `render_td` calls.
- **U2. Logs list: raw `uniform_list`.**
  - **Signature:** `uniform_list(id, count, f(Range<usize>, &mut Window, &mut App) -> Vec<R>)`; use `cx.processor`.
  - **Layout.** It needs `.size_full()` and an explicit row `.h(row::LOG)`.
  - **Scrolling.** `UniformListScrollHandle` offers `scroll_to_item(ix, ScrollStrategy::{Top,Center,Bottom,Nearest})`, `scroll_to_bottom()` and `is_scrolled_to_end() -> Option<bool>`. The last becomes `Some(false)` once the user scrolls up. Attach the handle with `.track_scroll(&handle)`.
  - **Rendered range.** `logical_scroll_top_index` is test-only and unreliable; record the rendered range inside the closure instead.
  - **Cost.** 1M items make 21 item-closure calls per frame, because item 0 is rendered twice more to measure it.
- **U3. Inline-expanding rows.** `gpui::list` builds 1M rows in about 220 ms (debug), so it is not used. Inline expansion uses continuation rows in the `uniform_list` (Task 16).
- **U4. Text input** (`gpui_kit::component::input::{Input, InputState, InputEvent}`).
  - **Creation:** `cx.new(|cx| InputState::new(window, cx).placeholder("…"))`, rendered with `Input::new(&state).cleanable(true)`.
  - **Value:** read with `value() -> SharedString`. `set_value(v, window, cx)` emits **no** `Change`.
  - **Focus:** `focus(window, cx)`.
  - **Events:** `InputEvent::{Change, PressEnter{secondary, shift}, Focus, Blur}` via `cx.subscribe_in(&state, window, …)`. Enter emits `PressEnter` plus a repeated `Change`, so dedupe by value.
  - **Key context `"Input"`.**
    - A binding in context `Shell` still fires inside a focused input and swallows the key. `Shell && !Input` was verified to work.
    - Escape propagates to the shell unless the input sets `clean_on_escape`.
- **U5. Highlights.** `StyledText::new(text).with_highlights(iter of (Range<usize>, HighlightStyle))`. Ranges must be sorted, non-overlapping and on char boundaries.
- **U6. Split pane.** `h_resizable(id).with_state(&Entity<ResizableState>).child(resizable_panel().size(px).size_range(min..max).child(…))`.
  - `ResizableState` emits `Resized`, and `sizes()` are pixels that can be persisted.
  - Panels must not use `overflow_hidden`.
- **U7. Tabs.** `TabBar::new(id).underline().with_size(Size::Small).selected_index(n).on_click(|ix: &usize, w, cx| …).child(Tab::new().label("Requests"))`. A `Tab` cannot take an id; tests click tabs by index (`window.click(2usize, cx)`) inside `within("net-tabs")`.
- **U8. Strip drag.** Build the strip as a `canvas`.
  - **Prepaint:** `window.insert_hitbox(bounds, HitboxBehavior::Normal)`.
  - **Paint:** `paint_quad`, then `window.on_mouse_event` listeners, re-registered every frame.
  - **Mouse-down:** guard with `phase == DispatchPhase::Bubble && hitbox.is_hovered(window)`.
  - **Move and up:** leave unguarded so a drag continues outside the strip.
  - **Coordinates.** Keep the bounds in an `Rc<RefCell<Bounds<Pixels>>>`; local x is `(pos.x - b.origin.x) / b.size.width`.
  - **Double-click** is `MouseDownEvent.click_count >= 2`. In tests, `simulate_mouse_down` always sends `click_count: 1`; use `simulate_event(MouseDownEvent { click_count: 2, … })`.
- **U9. Background to UI.** `cx.spawn(async move |this, cx| …)` can await `tokio::sync::{watch, mpsc}` receivers on GPUI's executor without a tokio runtime on that side.
  - Keep the `Task` in a field; dropping it cancels the loop.
  - `this.update(cx, …)` returns `Err` once the view is gone; stop the loop then.
  - `notify()` calls coalesce to one frame.
  - In tests, a real OS thread that wakes a foreground task needs `cx.executor().allow_parking()`.
- **U10. Mouse back/forward** is already handled by the shell.
- **U11. Clipboard and context menus.**
  - Write to the clipboard with `cx.write_to_clipboard(ClipboardItem::new_string(s))`.
  - Context menus: `use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};`, then `el.id(…).context_menu(|menu, _w, _cx| menu.item(PopupMenuItem::new("Copy").on_click(|_, _w, cx| …)))`. Use `on_click` closures, not action items.
  - Tables use `TableDelegate::context_menu`.
- **U12. Loading states.**
  - `Spinner::new()` and `Skeleton::new()`.
  - Delayed loading: `cx.spawn` a task that awaits `cx.background_executor().timer(motion::LOADING_DELAY)`, then sets `show_skeleton` if still loading. Tests advance time with `advance_clock`.

## File structure

```
Cargo.toml                                  members += fdt_store, fdt_vm, fdt_capture; new workspace deps
crates/fdt_store/                           no GPUI
  src/lib.rs
  src/time.rs                               SessionTime, TimeRange (moved from fdt_app::nav), RunId, EntryId
  src/clock.rs                              RunClock, calibrate
  src/chunked.rs                            Chunked<T>, ChunkedSnapshot<T>
  src/run.rs                                RunInfo, AppInfo, Mode
  src/session.rs                            SessionWriter, Snapshot
  src/logs.rs                               log records, text arenas, LogsSnapshot
  src/log_query.rs                          LogFilter, TextQuery, filtering and two-phase search, match_ranges
  src/net.rs                                HttpRecord, BodyState, SocketRecord, NetSnapshot
  src/net_query.rs                          NetFilter, filter_requests
  src/frames.rs                             FrameRecord, LodSeries, strip_bars
  src/synth.rs                              synthetic stress datasets (fixed seed)
  benches/store.rs                          criterion: log search, net filter, strip bars
crates/fdt_vm/                              no GPUI
  src/lib.rs
  src/uri.rs                                VmServiceUri parsing
  src/rpc.rs                                JSON-RPC over WebSocket
  src/tap.rs                                transcript recorder with redaction
  src/types.rs, src/events.rs               typed subset of the VM service protocol
  src/service.rs                            VmService (typed calls), Mode, detect_mode
  src/dart_io.rs                            ext.dart.io HTTP and socket profiling
  src/dtd.rs                                DtdClient, app label parsing
  src/discovery.rs                          registry directory, pid checks, Discovery
  src/testing.rs                            ScriptedServer, ReplayServer (feature "testing")
  examples/record_jank_lab.rs               records fixtures/recordings
  tests/decode_recordings.rs
crates/fdt_capture/                         no GPUI
  src/lib.rs
  src/runtime.rs                            CaptureRuntime (tokio)
  src/config.rs                             CaptureConfig
  src/status.rs                             ConnStatus
  src/capture.rs                            Capture handle and the session task
  src/connect.rs                            connect sequence and clock calibration
  src/ingest/{mod,logs,stdout,flutter_error,frames,http,sockets}.rs
  src/lifecycle.rs                          disconnect, reconnect, runs, hot restart
  tests/replay_debug.rs, tests/lifecycle.rs, tests/jank_lab_live.rs
crates/fdt_ui/src/tokens.rs                 new layout, table and motion tokens
crates/fdt_app/
  src/main.rs                               adds `bench-ui`
  src/capture_host.rs                       app-wide runtime + discovery; CaptureControl trait
  src/session_model.rs                      SessionModel entity (snapshot + status)
  src/shell/start_page.rs                   discovered apps and URL field
  src/shell/strip.rs                        session strip element and pure layout
  src/shell/banner.rs                       disconnected banner
  src/shell/title_bar.rs                    connection chip, recording clock, range chip
  src/tools/mod.rs
  src/tools/logs/{mod,model,view,toolbar,details}.rs
  src/tools/network/{mod,model,table,toolbar,details,body}.rs
  src/palette/{items,view}.rs               rewritten: item model with parameters, uniform_list
  src/bench_ui.rs                           `fdt bench-ui`
fixtures/jank_lab/lib/ext.dart              ext.jank_lab.run automation extension
fixtures/recordings/                        jank_lab-debug.jsonl, jank_lab-profile.jsonl, README.md
scripts/record-jank-lab.sh, scripts/jank-lab-it.sh
.github/workflows/ci.yml                    adds the jank_lab integration job
docs/milestones/m2-review.md                Task 23
```

Task order is sequential:
- Tasks 1–4 build `fdt_store`.
- Tasks 5–8 build `fdt_vm`.
- Tasks 9–11 build `fdt_capture`.
- Tasks 12–21 build the app.
- Task 22 adds the live integration test and Task 23 measures.

Task 12 depends only on Task 1. Task 6 needs Flutter and a macOS or Linux desktop.

---
### Task 1: `fdt_store` foundation: time, ids, chunked storage, runs and snapshots

**Files:**
- Create: `crates/fdt_store/Cargo.toml`, `src/lib.rs`, `src/time.rs`, `src/clock.rs`, `src/chunked.rs`, `src/run.rs`, `src/session.rs`
- Modify: `Cargo.toml` (members; workspace deps `rayon = "1"`, `memchr = "2"`), `crates/fdt_app/Cargo.toml` (dependency `fdt_store`), `crates/fdt_app/src/nav.rs` (remove the `SessionTime`/`TimeRange` definitions and re-export them)

**Interfaces:**
- **Consumes:** nothing new.
- **Produces:**
  - **`fdt_store::time`**
    - `SessionTime(pub u64)`: nanoseconds since the session origin. It moves here from `nav`, with the same derives plus `Default`.
    - `TimeRange { start, end }`:
      - `new(a, b)` orders the endpoints.
      - `contains(t) -> bool` is inclusive at both ends.
      - `overlaps(start, end: SessionTime) -> bool` is inclusive.
      - `duration_ns() -> u64`.
    - `RunId(pub u32)`, starting at 1.
    - `EntryId { run: RunId, seq: u64 }`, where `seq` is unique per stream within a session. All three derive `Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord`.
  - **`fdt_store::clock`**
    - `RunClock { origin_host_us: i64, skew_us: i64, timeline_to_wall_us: i64 }`.
    - `RunClock::new(origin_host_us, cal: Calibration)`.
    - `from_epoch_ms(i64)`, `from_epoch_us(i64)` and `from_timeline_us(i64) -> Stamp`.
    - `Stamp { time: SessionTime, clamped: bool }`, where `clamped` means the event happened before the origin.
    - `Calibration { skew_us, timeline_to_wall_us }`.
    - `calibrate(host_before_us: i64, host_after_us: i64, device_wall_us: i64, timeline_us: i64) -> Calibration`:
      - `skew = (before + after) / 2 - device_wall`
      - `timeline_to_wall = device_wall - timeline`
    - `host_now_us() -> i64` (`SystemTime` epoch µs).
  - **`fdt_store::chunked`**
    - `Chunked<T: Clone>` and `Chunked::with_chunk(n: usize)`.
    - `push(T) -> usize` returns the index.
    - `len()`, `get(i) -> Option<&T>`.
    - `get_mut(i) -> Option<&mut T>` is copy-on-write: only the touched sealed chunk is cloned when a snapshot shares it.
    - `snapshot() -> ChunkedSnapshot<T>`.
    - `ChunkedSnapshot<T>` is `Clone`. It offers `len()`, `get(i) -> Option<&T>`, `chunks() -> impl Iterator<Item = &[T]>` in order, and `chunk_size()`.
  - **`fdt_store::run`**
    - `Mode { Debug, Profile, Unknown }`.
    - `AppInfo { name: String, device: Option<String>, mode: Mode, flutter_version: Option<String>, workspace: Option<String>, vm_uri: String, pid: Option<u32> }`.
    - `RunInfo { id: RunId, app: AppInfo, started: SessionTime, ended: Option<SessionTime>, clock: RunClock }`.
  - **`fdt_store::session`**
    - `SessionWriter::new(origin_host_us: i64)`.
    - `origin_host_us()`.
    - `start_run(app, clock, at) -> RunId`.
    - `update_run_app(run, f: impl FnOnce(&mut AppInfo))`.
    - `end_run(run, at)`.
    - `is_dirty()`.
    - `publish() -> Arc<Snapshot>`.
    - `Snapshot { generation: u64, origin_host_us: i64, runs: Arc<[RunInfo]>, … }` with `run(RunId) -> Option<&RunInfo>`, `latest_time() -> SessionTime` (the latest stored event time or run boundary), and `Snapshot::empty()`. Tasks 2–4 add the `logs`, `net` and `frames` fields.

- [ ] **Step 1: Write the failing tests** (in the modules' `tests`).
  - `time_range_new_orders_endpoints`, `contains_and_overlaps_are_inclusive`.
  - `calibrate_maps_device_clocks_onto_the_host_axis`. With origin 1_000_000 µs, `calibrate(1_000_000, 1_000_200, 2_000_000, 500)` gives:
    - `from_epoch_us(2_000_000)` → `SessionTime(100_000)`
    - `from_timeline_us(500)` → `SessionTime(100_000)`
    - `from_epoch_ms(2_000)` → `SessionTime(100_000)`
    - `from_epoch_us(1_000_000)` → `SessionTime(0)` with `clamped == true`
  - `chunked_snapshot_is_unchanged_by_later_pushes` (chunk 4, push 10, snapshot, push 5 → the snapshot still has 10).
  - `get_mut_copies_only_the_touched_sealed_chunk`. Use a `#[cfg(test)] fn chunk_ptr(&self, c) -> *const T` on the snapshot to show that untouched chunks are pointer-equal across two snapshots.
  - `chunks_cover_every_item_in_order`.
  - `publish_bumps_generation_only_when_dirty` (generation 1, publish again unchanged → same `Arc`).
  - `runs_are_numbered_from_one_and_record_end`.
- [ ] **Step 2: Confirm they fail.**
  - Run: `cargo test -p fdt_store`
  - Expected: FAIL (compile errors: items missing).
- [ ] **Step 3: Implement.**
  - **`Chunked` storage.** Store sealed chunks as `Vec<Arc<Vec<T>>>` plus a `tail: Vec<T>`. Cache the `Arc<[Arc<Vec<T>>]>` handed to snapshots and rebuild it only when a chunk seals or `get_mut` touches a sealed chunk. A snapshot copies the tail.
  - **`nav.rs`.** Replace the two definitions with `pub use fdt_store::time::{SessionTime, TimeRange};`. Every existing `nav` test must pass unchanged.
- [ ] **Step 4: Verify.**
  - Run: `cargo test -p fdt_store && cargo test -p fdt_app`
  - Expected: PASS.
  - Also run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo deny check licenses`.
- [ ] **Step 5: Commit** `feat(store): add the session store foundation with run clocks and snapshots`.

### Task 2: Log records, text arenas, filtering and search

**Files:**
- Create: `crates/fdt_store/src/logs.rs`, `src/log_query.rs`, `src/synth.rs` (the logs part), `benches/store.rs`
- Modify: `src/session.rs` (`push_log`, `Snapshot.logs`), `Cargo.toml` (deps `regex`, `memchr`, `rayon`; dev-dep `criterion`; `[[bench]] name = "store" harness = false`)

**Interfaces:**
- **Consumes:** Task 1.
- **Produces:**
  - **Records**
    - `LogLevel { Debug, Info, Warning, Error }` with `label() -> &'static str` ("Debug", "Info", "Warning", "Error") and `short() -> &'static str` ("DBG", "INF", "WRN", "ERR").
    - `level_from_dart(i64) -> LogLevel`:
      - `0` → Info
      - `1..=799` → Debug
      - `800..=899` → Info
      - `900..=999` → Warning
      - `>= 1000` → Error
    - `LogSource { Log, Stdout, Stderr, FlutterError, Navigation }` with `label()` ("Log", "Stdout", "Stderr", "Flutter error", "Navigation").
    - `ErrorDetail { description: String, summary: String, widget: Option<String>, location: Option<String>, stack: Vec<String>, rendered: String }`.
    - `LogInput { run: RunId, time: SessionTime, clamped: bool, level: LogLevel, source: LogSource, logger: String, text: String, error: Option<ErrorDetail> }`. An empty `logger` means none.
    - `SessionWriter::push_log(LogInput) -> EntryId`.
  - **`Snapshot.logs: LogsSnapshot`**
    - `len()`.
    - `row(seq: usize) -> LogRow<'_>`, where `LogRow { id: EntryId, time, clamped, level, source, logger: &str, text: &str, line_count: u32, error: Option<&ErrorDetail> }`.
    - `loggers() -> &[Arc<str>]`.
    - `level_counts(range: Option<TimeRange>) -> [u64; 4]`. It is O(n) and called only off the UI thread.
  - **`log_query`**
    - `LevelSet` and `SourceSet` are `u8` bitsets with `ALL`, `contains` and `toggle`.
    - `LogFilter { levels: LevelSet, sources: SourceSet, loggers: Option<Arc<HashSet<Arc<str>>>>, text: Option<TextQuery>, range: Option<TimeRange> }`, with `Default` showing everything.
    - `TextQuery` is `Plain { needle: String, case_sensitive: bool }` or `Regex(regex::Regex)`.
    - `QueryError { message: String }`.
    - `compile_query(input: &str, regex_mode: bool) -> Result<Option<TextQuery>, QueryError>`:
      - Trimmed empty input gives `None`.
      - **Smart case:** the search is case-insensitive unless the input contains an uppercase letter.
      - Regex mode uses `(?i)` under the same rule.
    - `filter_logs(snap: &LogsSnapshot, f: &LogFilter, from_seq: usize) -> Vec<u32>` returns ascending seqs ≥ `from_seq`, scanned in parallel over chunks.
    - `filter_logs_two_phase(snap, f, cancel: &AtomicBool, on_first: impl FnOnce(Vec<u32>)) -> Option<Vec<u32>>`:
      - Phase one scans the newest `FIRST_PHASE_CHUNKS = 64` chunks and calls `on_first`.
      - Phase two scans everything.
      - It returns `None` if `cancel` was set.
    - `match_ranges(text: &str, q: &TextQuery) -> Vec<Range<usize>>`: sorted, non-overlapping, on char boundaries.
  - **Constants**
    - `LOG_CHUNK = 1024` records per chunk.
    - `MAX_RECORD_BYTES = 1 << 20`. Longer text is cut and gets `"… (cut, N bytes)"` appended.
  - **`synth::logs(w: &mut SessionWriter, run: RunId, n: usize, seed: u64)`**
    - Deterministic xorshift with no `rand` dependency.
    - Levels: 30 % Debug, 50 % Info, 12 % Warning, 8 % Error.
    - 24 logger names, 5 % multi-line records (2–30 lines), 40–300 characters per line, about 1 ms apart.
    - About 150 bytes per record on average.

- [ ] **Step 1: Write the failing tests.**
  - `dart_levels_map_at_the_boundaries` (0, 1, 799, 800, 899, 900, 999, 1000, 1200).
  - `multi_line_text_counts_lines_and_keeps_all_text` ("a\nb\nc\n" → `line_count` 3).
  - `loggers_are_interned`.
  - `oversized_records_are_cut_with_a_note`.
  - `filter_by_level_source_logger_and_range`.
  - `smart_case_plain_search` ("error" matches "Error"; "Error" does not match "error").
  - `regex_search_and_invalid_regex_reports_message` (`compile_query("(", true)` is `Err` with a non-empty message).
  - `pathological_regex_finishes_quickly`: `(a+)+$` over 1,000 records of 30 × "a" + "b" finishes in < 1 s in debug.
  - `search_finds_text_beyond_the_display_cap` (a 200 KB single-line record whose match is at byte 150,000).
  - `two_phase_reports_newest_first_then_everything` (3,000 chunks' worth with `LOG_CHUNK` overridden in tests via a `#[cfg(test)]` constructor, or with enough records; phase one contains only seqs from the newest 64 chunks).
  - `cancel_returns_none`.
  - `match_ranges_handle_case_and_multibyte` ("Ünïcode" with query "ünï" → `0..6` in bytes).
- [ ] **Step 2: Write the budget tests** (`#[cfg_attr(debug_assertions, ignore)]`; the names contain `budget`).
  - Over `synth::logs(…, 1_000_000, 7)`, first assert that total text bytes are within 120–180 MB.
  - `log_search_budget_full_scan_1m`: queries `"timeout"`, `"Timeout"`, `"user_42"` and regex `status=5\d\d`. Ten runs each; p90 of `filter_logs` ≤ 100 ms.
  - `log_search_budget_first_results_1m`: the `on_first` latency for the same queries has p90 ≤ 16 ms.
  - Print the worst and p90 with `eprintln!`.
- [ ] **Step 3: Confirm the tests fail.**
  - Run: `cargo test -p fdt_store`
  - Expected: FAIL.
- [ ] **Step 4: Implement.**
  - **Storage.** A `LogChunk { metas: Vec<LogMeta>, text: String }` (`LogMeta` holds offsets into `text`). Sealed chunks are `Arc<LogChunk>`; the snapshot copies the tail chunk.
  - **Plain search.** Case-sensitive uses `memchr::memmem::Finder` (built once). Case-insensitive uses `regex::RegexBuilder::new(&regex::escape(needle)).case_insensitive(true)`.
  - **Parallelism.** Use `rayon` over chunks and keep results in order.
- [ ] **Step 5: Verify.**
  - Run: `cargo test -p fdt_store` (PASS), then `cargo test -p fdt_store --release -- budget --nocapture`.
  - Expected: both budget tests PASS.
  - Add `log_search_100k` and `log_search_1m` groups to `benches/store.rs` and run `cargo bench -p fdt_store --bench store -- --quick log_search`.
- [ ] **Step 6: Commit** `feat(store): add log records with arena text, filters and two-phase search`.

### Task 3: Network records, body budget and request filtering

**Files:**
- Create: `crates/fdt_store/src/net.rs`, `src/net_query.rs`
- Modify: `src/session.rs`, `src/synth.rs` (requests), `benches/store.rs`

**Interfaces:**
- **Consumes:** Task 1.
- **Produces:**
  - **`HttpUpdate`**
    - Identity and request line: `run`, `isolate: String`, `vm_id: String`, `method`, `url`.
    - Times: `start: SessionTime`, `start_clamped: bool`, `request_sent: Option<SessionTime>`, `response_start: Option<SessionTime>`, `end: Option<SessionTime>`. `end` is set when the request is complete (fact P11).
    - Response: `status: Option<u16>`, `reason: Option<String>`.
    - Headers: `request_headers` and `response_headers`, both `Vec<(String, Vec<String>)>` and sorted by name.
    - Lengths: `request_content_length` and `response_content_length`, both `Option<i64>`.
    - `events: Vec<(String, SessionTime)>`, `error: Option<String>`, `websocket: bool`, `remote: Option<(String, u16)>`.
  - **`SessionWriter` methods**
    - `upsert_request(HttpUpdate) -> RequestUpsert { id: EntryId, newly_completed: bool }`. It is keyed by `(run, vm_id)`, and `newly_completed` is true exactly once.
    - `set_body_state(id, BodyKind, BodyState)`.
    - `store_body(id, BodyKind, bytes: Vec<u8>)`. It applies `BodyLimits` and evicts the oldest bodies first.
    - `set_body_limits(BodyLimits)`.
    - `upsert_socket(SocketUpdate) -> EntryId`. It is keyed by `(run, vm_id)` and links a websocket request (see Step 1).
  - **`HttpRecord`** has all `HttpUpdate` fields plus:
    - `id: EntryId`.
    - `host: String`: authority with the port.
    - `path: String`: path plus query.
    - `request_body` and `response_body`, both `BodyState`.
    - `before_capture: bool`, which is `start_clamped`.
  - **`HttpRecord` methods**
    - `status_class() -> StatusClass`, one of `Informational`, `Success`, `Redirect`, `ClientError`, `ServerError`, `Failed` or `Pending`.
    - `is_error()`: Failed, 4xx or 5xx, but never a websocket upgrade.
    - `duration_ns() -> Option<u64>`.
    - `size() -> Option<u64>`: the response content length, else the stored body's full length.
    - `content_type() -> Option<&str>`.
  - **Bodies**
    - `BodyKind { Request, Response }`.
    - `BodyState`: `NotCaptured`, `Pending`, `Captured(Body)`, `TooLarge { len: u64 }`, `Evicted { len: u64 }` or `Unavailable { reason: String }`.
    - `Body { bytes: Arc<[u8]>, full_len: u64 }` with `truncated()`.
    - `BodyLimits { per_body: usize, total: usize }`. The defaults are `1 << 20` and `200 << 20`.
  - **Sockets**
    - `SocketUpdate { run, vm_id, address, port: u16, kind: String, start: SessionTime, end: Option<SessionTime>, read_bytes: u64, write_bytes: u64, last_read: Option<SessionTime>, last_write: Option<SessionTime> }`.
    - `SocketRecord` has the same fields plus `id` and `websocket: Option<EntryId>`.
  - **`Snapshot.net: NetSnapshot`**
    - `len()`, `request(seq) -> &Arc<HttpRecord>`, `sockets() -> &ChunkedSnapshot<SocketRecord>`.
    - `socket_for(request: EntryId) -> Option<&SocketRecord>`.
    - `hosts() -> Arc<[String]>` (distinct, in first-seen order).
    - `body_bytes()`.
  - **`net_query`**
    - `NetTab { Requests, WebSockets, Errors }`.
    - `MethodSet`: a bitset over `GET POST PUT PATCH DELETE OTHER`.
    - `ClassSet`: a bitset over the `StatusClass` values.
    - `NetFilter { tab, methods, classes, host: Option<String>, text: String, range: Option<TimeRange> }`. Its `text` is matched as a case-insensitive substring of the URL.
    - `filter_requests(&NetSnapshot, &NetFilter) -> Vec<u32>`.
    - The Requests tab excludes nothing. WebSockets keeps `websocket` requests. Errors keeps `is_error()`.
    - A range keeps requests whose `[start, end or latest]` overlaps it.
  - **`synth::requests(w, run, n, seed)`**
    - 12 hosts.
    - Methods: 70 % GET, 20 % POST, 10 % other.
    - Statuses: 80 % 2xx, 5 % 3xx, 8 % 4xx, 4 % 5xx, 2 % failed, 1 % pending.
    - Durations of 5 ms–3 s, sizes of 0–2 MB, and no bodies stored.
- [ ] **Step 1: Write the failing tests.**
  - `upserting_the_same_request_updates_in_place` (same seq; `newly_completed` true once across three upserts).
  - `host_and_path_split` (`"http://127.0.0.1:53452/v2/products?page=3"` → `"127.0.0.1:53452"`, `"/v2/products?page=3"`; `"not a url"` → host `""`, path `"not a url"`).
  - `status_classes_and_errors` (101 websocket with `error: Some("Socket has been detached")` → not an error).
  - `bodies_over_the_cap_are_truncated` (2 MiB with a 1 MiB cap → 1 MiB stored, `full_len` 2 MiB).
  - `oldest_bodies_are_evicted_to_stay_in_budget` (total 3 MiB, four 1 MiB bodies → the first becomes `Evicted { len }`).
  - `filters_by_method_class_host_text_range_and_tab`.
  - `websocket_socket_is_linked_by_remote_address`. A socket whose `address`/`port` equal the request's `remote`, starting within 2 s of the request, is linked both ways. When several qualify, the nearest start wins.
- [ ] **Step 2: Write the budget test** `net_filter_budget_100k` (release only) over `synth::requests(…, 100_000, 7)`.
  - Filters:
    - `text = "products"`
    - `classes = ClientError | ServerError`
    - `host = Some(<a synth host>)` with `methods = POST`
    - the Errors tab
  - 50 runs each; p90 ≤ 16 ms. Print the worst and p90.
- [ ] **Step 3: Confirm the tests fail, then implement.**
  - Requests are `Chunked<Arc<HttpRecord>>` with `with_chunk(1024)` and a `HashMap<(RunId, String), usize>` index in the writer.
  - Body accounting is a `VecDeque<(EntryId, BodyKind, usize)>`.
- [ ] **Step 4: Verify.**
  - Run: `cargo test -p fdt_store` (PASS), then `cargo test -p fdt_store --release -- budget --nocapture` (PASS).
  - Add a `net_filter_100k` bench group.
- [ ] **Step 5: Commit** `feat(store): add HTTP and socket records with body budget and filters`.

### Task 4: Frames and level-of-detail summaries for the strip

**Files:**
- Create: `crates/fdt_store/src/frames.rs`
- Modify: `src/session.rs`, `src/synth.rs` (frames), `benches/store.rs`

**Interfaces:**
- **Consumes:** Task 1.
- **Produces:**
  - **Frames**
    - `FrameInput { run, number: u64, start: SessionTime, elapsed_us: u32, build_us: u32, raster_us: u32, vsync_overhead_us: u32 }`.
    - `SessionWriter::push_frame(FrameInput) -> EntryId`.
    - `FrameRecord` has the same fields plus `id`.
  - **`Snapshot.frames: FramesSnapshot`** with `len()`, `get(i)` and `lod() -> &LodSnapshot`.
  - **`LodSeries`** (writer side)
    - `BASE_SHIFT = 24`: level-0 buckets are 2^24 ns ≈ 16.8 ms.
    - `LEVELS = 20`.
    - Level k buckets are 2^(24+k) ns wide.
    - `insert(t: SessionTime, value_us: u32)` is O(LEVELS) and accepts any order.
    - Each level is a `Chunked<Bucket>` with chunk 1024, and `Bucket { max_us: u32, count: u32 }`.
  - **`strip_bars(frames: &FramesSnapshot, range: TimeRange, width_px: usize) -> Vec<StripBar>`**
    - `StripBar { start: SessionTime, end: SessionTime, max_elapsed_us: u32, frames: u32 }`.
    - It picks the smallest level whose bucket count over `range` is ≤ `width_px`, returns only non-empty buckets in time order, and visits at most `2 * width_px + 2` buckets.
  - **`synth::frames(w, run, n, seed, hz: u32)`**
    - Frames at `1/hz` spacing.
    - 3 % slow (budget to 2× budget) and 1 % janky (> 2× budget).
- [ ] **Step 1: Write the failing tests.**
  - `a_frame_lands_in_one_bucket_per_level`.
  - `bucket_keeps_the_maximum`.
  - `out_of_order_inserts_are_counted`.
  - `strip_bars_fit_the_width` (`synth::frames(…, 72_000, 7, 120)` with width 1,000 → ≤ 1,000 bars, the bars are ordered, and their `frames` sum to 72,000).
  - `strip_bars_for_a_sub_range`.
- [ ] **Step 2: Write the budget test** `strip_bars_budget_72k` (release): width 1,000 over the whole span, p90 ≤ 1 ms over 50 runs.
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_store` and the release budget run.
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(store): add frame records and level-of-detail summaries`.

### Task 5: `fdt_vm`: URL parsing and the JSON-RPC client

**Files:**
- Create: `crates/fdt_vm/Cargo.toml`, `src/lib.rs`, `src/uri.rs`, `src/rpc.rs`, `src/tap.rs`, `src/testing.rs`
- Modify: `Cargo.toml` (members; workspace deps):
  - `tokio = { version = "1", features = ["rt-multi-thread", "net", "sync", "time", "macros", "io-util", "process", "fs"] }`
  - `tokio-tungstenite`: run `cargo add -p fdt_vm tokio-tungstenite --no-default-features --features connect` to pick the latest version, then move it into the workspace deps
  - `futures-util = "0.3"`, `url = "2"`, `base64 = "0.22"`
- Cargo features: `testing = []` gates `src/testing.rs`. `fdt_vm` lists itself as a dev-dependency with `features = ["testing"]`.

**Interfaces:**
- **Consumes:** nothing.
- **Produces:**
  - **`uri`**
    - `VmServiceUri`, with `parse(input: &str) -> Result<VmServiceUri, UriError>`, `ws_url() -> &url::Url`, `host_port() -> String` and `redacted() -> String` (the token segment becomes `…`).
    - `UriError` has a `message()` with this exact copy:
      - `Empty`: "Paste a VM service URL, for example http://127.0.0.1:50300/abc=/"
      - `NotVmService`: "That doesn't look like a VM service URL. It looks like http://127.0.0.1:50300/abc=/ or ws://127.0.0.1:50300/abc=/ws"
      - `Secure`: "Secure (https or wss) VM service URLs aren't supported yet."
  - **`rpc`**
    - `ConnectOptions { connect_timeout: Duration /* 5 s */, call_timeout: Duration /* 30 s */, max_message_bytes: usize /* 256 MiB */, tap: Option<Tap> }`, with `Default`.
    - `connect(url: &url::Url, opts) -> Result<(RpcClient, NotificationRx), ConnectError>`.
    - `NotificationRx = tokio::sync::mpsc::UnboundedReceiver<Notification>`, and `Notification { method: String, params: serde_json::Value }`.
    - `RpcClient` is `Clone`:
      - `call(method, params: Value) -> Result<Value, RpcError>`
      - `call_with_timeout(method, params, Duration)`
      - `closed() -> impl Future<Output = CloseReason>`
      - `is_closed()`
      - `close()`
    - `RpcError`: `Server { code: i64, message: String, data: Option<Value> }`, `Timeout`, `Closed` or `Decode(String)`.
    - `CloseReason { code: Option<u16>, reason: String }`. `code: None` means the socket dropped with no close frame (shown as 1006).
    - `ConnectError` has a `message()` with this exact copy:
      - `Refused`: "Nothing is listening at {host_port}. Is the app still running?"
      - `Timeout`: "No answer from {host_port} within 5 seconds."
      - `NotVmService`: "{host_port} answered, but it isn't a Dart VM service. Check the token at the end of the URL."
      - `Other(String)`
  - **`tap`**
    - `Tap::create(path) -> io::Result<Tap>`. It writes one JSON object per line, `{"t_us":<µs since tap start>,"dir":"send"|"recv","msg":<frame>}`.
    - `redact(text: &str) -> String`:
      - Replaces URL path segments of 8 or more characters that end in `=` with `<token>`.
      - Replaces the absolute home directory with `~` (matched literally) on every OS.
      - Rewrites `file:///Users/<name>/` and `C:\\Users\\<name>\\` forms.
  - **`testing::ScriptedServer`**
    - `start(handler: impl Fn(&str, &Value) -> Result<Value, (i64, String)> + Send + Sync + 'static) -> ScriptedServer`.
    - `ws_uri() -> url::Url`, `notify(method, params)`, `close_abruptly()`, `received() -> Vec<(String, Value)>`.
    - `set_latency(Duration)` delays every response.
- [ ] **Step 1: Write the failing URL tests** (table-driven, `uri_parse_cases`):

  | Input | Expected |
  |---|---|
  | `http://127.0.0.1:50300/abc=/` | `ws://127.0.0.1:50300/abc=/ws` |
  | `http://127.0.0.1:50300/abc=` | `ws://127.0.0.1:50300/abc=/ws` |
  | `"  ws://127.0.0.1:50300/abc=/ws \n"` | `ws://127.0.0.1:50300/abc=/ws` |
  | `ws://127.0.0.1:50300/abc=/ws/` | `ws://127.0.0.1:50300/abc=/ws` |
  | `http://127.0.0.1:9100/?uri=http://127.0.0.1:50300/abc=/` | `ws://127.0.0.1:50300/abc=/ws` |
  | `127.0.0.1:50300/abc=/` | `ws://127.0.0.1:50300/abc=/ws` |
  | `http://localhost:50300/` | `ws://localhost:50300/ws` |
  | `https://h:443/abc=/` | `Secure` |
  | `""` | `Empty` |
  | `hello` | `NotVmService` |
  | `ftp://x/` | `NotVmService` |

  Also test `redacted()` (`ws://127.0.0.1:50300/…/ws`).
- [ ] **Step 2: Write the failing client tests** (`#[tokio::test]` against `ScriptedServer`):
  - `call_round_trip`.
  - `concurrent_calls_match_ids` (100 parallel calls; the server answers in reverse order).
  - `server_error_maps_to_rpc_error`.
  - `notifications_arrive_in_order`.
  - `abrupt_close_reports_no_code_and_fails_pending_calls`: a pending call ends with `RpcError::Closed` within 100 ms, and `closed()` yields `code: None`.
  - `connect_refused_message`: bind a `std::net::TcpListener` on port 0, take its port and drop it.
  - `connect_timeout_message`: a listener that accepts and never answers; `connect_timeout` is 300 ms here.
  - `plain_http_server_is_not_a_vm_service`: a tiny server that answers `HTTP/1.1 404`.
  - `tap_writes_redacted_jsonl`.
  - `large_results_are_accepted`: a 5 MB result string.
- [ ] **Step 3: Confirm the tests fail, then implement.**
  - The reader task routes responses by id through a `HashMap<String, oneshot::Sender>`, and routes notifications to the channel.
  - Ids are strings "1", "2", ….
  - On read EOF or an error, every pending call fails with `Closed` and the close watch is set.
  - `max_message_size` and `max_frame_size` come from `max_message_bytes`.
- [ ] **Step 4: Verify.**
  - Run: `cargo test -p fdt_vm`
  - Expected: PASS. Also run clippy and `cargo deny check licenses`; the new dependency tree must pass the existing allow-list unchanged.
- [ ] **Step 5: Commit** `feat(vm): add VM service URL parsing and a JSON-RPC WebSocket client`.

### Task 6: `jank_lab` automation, the recorder, recorded transcripts and replay

**Files:**
- Create:
  - `fixtures/jank_lab/lib/ext.dart`, `fixtures/jank_lab/test/ext_test.dart`
  - `crates/fdt_vm/examples/record_jank_lab.rs`
  - `scripts/record-jank-lab.sh`
  - `fixtures/recordings/README.md`, `fixtures/recordings/jank_lab-debug.jsonl`, `fixtures/recordings/jank_lab-profile.jsonl`
  - `crates/fdt_vm/tests/recordings_privacy.rs`
- Modify:
  - `fixtures/jank_lab/lib/main.dart`
  - `lib/scenarios/logs.dart`, `lib/scenarios/network.dart`
  - `lib/fixture_server.dart`
  - `crates/fdt_vm/src/testing.rs` (`ReplayServer`)

**Interfaces:**
- **Consumes:** Task 5 (`connect`, `Tap`, `ScriptedServer`).
- **Produces:**
  - **`ext.jank_lab.run`**, a service extension with param `action`. It returns `{"ok":true,"available":[…]}`, or the extension error `-32602` with message "Unknown action: <name>".
    - **Built-in actions:** `goto_logs`, `goto_network` (`pushNamed` on a global `navKey`), `pop` and `list`.
    - **Logs screen** (registered in `initState`, removed in `dispose`): `emit_levels`, `print_burst`, `multi_line`, `report_error` and `throw_in_build`.
    - **Network screen:** `load_products`, `place_order`, `download`, `burst`, `open_socket`, `hero_image`, `close_socket`, plus:
      - `refused`: GET `http://127.0.0.1:1/`
      - `slow`: GET `/slow`, which the fixture server answers after 8 s
    - **Handler.** Every call runs the action unawaited (`unawaited(Future.sync(fn))`) and calls `WidgetsBinding.instance.scheduleFrame()` (fact P17).
  - **`testing::ReplayServer::start(transcript: &Path) -> ReplayServer`**
    - It serves recorded responses. `ws_uri()` and `received()` work as on `ScriptedServer`.
    - **Matching by key:**
      - `getObject` by `objectId`
      - `getHttpProfileRequest` by `id`
      - `ext.jank_lab.run` by `action`
    - **Matching by method:** every other method is answered with the recorded responses for that method in order, then the last one repeated. `isolateId` is ignored when matching.
    - **Unknown method:** `-32601 "Method not found"`.
    - **Notifications.** After answering `streamListen` for stream S, it sends every recorded notification of S in recorded order.
- [ ] **Step 1: Flutter tests first** (`fixtures/jank_lab/test/ext_test.dart`):
  - The extension handler (called through the registered callback, made testable by exporting `handleJankLabRun(String action)`) returns `available` containing the built-ins.
  - An unknown action returns the error.
  - The Logs screen's actions appear while it is mounted and disappear after it is popped.
  - Run: `cd fixtures/jank_lab && flutter test`. Expected: FAIL first, then PASS after implementing.
- [ ] **Step 2: Implement `ext.dart` and wire the screens.**
  - Register in `main()` before `runApp` and give `MaterialApp` the `navigatorKey: navKey`.
  - Add `/slow` to the fixture server.
  - Run `flutter analyze` (clean) and `flutter test` (PASS).
- [ ] **Step 3: Write the recorder** (`cargo run -p fdt_vm --example record_jank_lab -- --uri <url> --out <file>`). It runs against a live app with a `Tap`, in this order:
  1. `getVersion`, `getVM`, `getIsolate(main)`, `getFlagList`.
  2. `streamListen` for each of `Logging Stdout Stderr Extension Isolate Service Debug`.
  3. `getVMTimelineMicros`, `ext.dart.io.getVersion`, `httpEnableTimelineLogging {enabled:true}`, `socketProfilingEnabled {enabled:true}`, and `getHttpProfile {updatedSince: 9000000000000000}`.
  4. Wait up to 2 s for a `ServiceRegistered` whose method ends in `flutterVersion`, then call it.
  5. Run the actions with 300 ms gaps:
     1. `goto_logs`, `emit_levels`, `print_burst`, `multi_line`, `report_error`
     2. `throw_in_build`, only if `ext.flutter.reassemble` is present
     3. `pop`, `goto_network`, `load_products`, `place_order`, `refused`, `download`, `hero_image`
     4. `open_socket` (then wait 5 s), `close_socket`, `slow`
  6. Poll `getHttpProfile` with `updatedSince` every 500 ms for 12 s, and call `getHttpProfileRequest` for each completed request (fact P11).
  7. Call `getSocketProfile`.
  8. Call `getObject` for every truncated Logging message.
  9. Close.
- [ ] **Step 4: Write the recording script** `scripts/record-jank-lab.sh <debug|profile>`.
  - It runs `flutter run -d macos` (`-d linux` on Linux) `--machine` in `fixtures/jank_lab` (adding `--profile` for profile), reads the `app.debugPort` event's `wsUri`, and runs the recorder into `fixtures/recordings/jank_lab-<mode>.jsonl`.
  - It stops the app with the daemon's `app.stop` and leaves no processes running.
  - Record both transcripts. Then write `fixtures/recordings/README.md`: how the recordings were made, the Flutter and Dart versions, and that they are redacted.
- [ ] **Step 5: Write the failing privacy test** `recordings_privacy.rs` and make it pass.
  - Every file under `fixtures/recordings/` must contain no `/Users/`, `/home/` or `C:\\Users`.
  - No URL path segment may match `[A-Za-z0-9_-]{8,}=`.
- [ ] **Step 6: Write the failing replay tests and implement `ReplayServer`.**
  - `replay_answers_get_vm_from_the_recording`.
  - `stream_listen_replays_that_streams_events_in_order`.
  - `get_object_matches_by_object_id` (the full 410-character string).
  - `unknown_method_is_not_found`.
- [ ] **Step 7: Verify.**
  - Run: `cargo test -p fdt_vm`, `flutter test` and `flutter analyze`.
  - Expected: PASS and clean.
- [ ] **Step 8: Commit** `feat(fixtures): add jank_lab automation, recorded VM transcripts and a replay server`.

### Task 7: Typed VM service API

**Files:**
- Create: `crates/fdt_vm/src/types.rs`, `src/events.rs`, `src/service.rs`, `src/dart_io.rs`, `tests/decode_recordings.rs`

**Interfaces:**
- **Consumes:** Tasks 5 and 6.
- **Produces:**
  - **`types`** (all `serde` with `#[serde(rename_all = "camelCase")]`; unknown fields ignored; conditional fields are `Option`):
    - `Vm { pid: Option<u32>, start_time: Option<i64>, operating_system: String, target_cpu: Option<String>, isolates: Vec<IsolateRef> }`.
    - `IsolateRef { id, name, is_system_isolate: bool, isolate_group_id: Option<String> }`.
    - `Isolate { id, name, extension_rpcs: Vec<String>, root_lib: Option<LibRef { uri }> }`.
    - `InstanceRef { id: Option<String>, kind: String, value_as_string: Option<String>, value_as_string_is_truncated: bool /* default false */, length: Option<u64>, class_name: Option<String> /* from class.name */ }`.
    - `FlutterVersion { flutter_version, framework_version, dart_sdk_version }`, all `Option<String>`.
  - **`events`**
    - `decode_notification(&Notification) -> Option<StreamEvent>`.
    - `StreamEvent` is one of:
      - `Logging { isolate, timestamp_ms, sequence, time_ms, level, logger: InstanceRef, message: InstanceRef, error: InstanceRef, stack_trace: InstanceRef }`
      - `Write { stream: WriteStream /* Stdout | Stderr */, bytes: Vec<u8> /* base64-decoded */, timestamp_ms }`
      - `Extension { isolate, kind: String, data: Value, timestamp_ms }`
      - `IsolateStart { isolate: IsolateRef }`, `IsolateRunnable { isolate }`, `IsolateExit { isolate }`
      - `ServiceExtensionAdded { isolate, rpc: String }`, `IsolateReload { isolate_group: Option<String> }`
      - `ServiceRegistered { service: String, method: String }`
      - `Other { stream: String, kind: String }`
  - **`service`**
    - `VmService::new(RpcClient)`.
    - `get_version`, `get_vm`, `get_isolate(id)`.
    - `stream_listen(&str)`, which treats error 103 as success.
    - `get_vm_timeline_micros() -> i64`.
    - `get_object_string(isolate, object_id) -> String`.
    - `get_flag_list`.
    - `call_service(method, isolate) -> Value`.
    - `flutter_version(method, isolate) -> FlutterVersion`.
    - `call_extension(method, isolate, params: Value) -> Value`, which adds `isolateId`.
    - `Mode { Debug, Profile }` and `detect_mode(&Isolate) -> Mode` (fact P5).
  - **`dart_io`**
    - `http_enable(isolate, bool) -> bool`.
    - `get_http_profile(isolate, updated_since: Option<i64>) -> HttpProfile { timestamp: i64, requests: Vec<HttpProfileEntry> }`.
    - `HttpProfileEntry { id, isolate_id, method, uri, start_time, end_time: Option<i64>, events: Vec<HttpEvent { timestamp, event }>, request: Option<HttpRequestData>, response: Option<HttpResponseData> }`.
    - `HttpRequestData { headers: Option<BTreeMap<String, Vec<String>>>, connection_info: Option<ConnectionInfo { local_port, remote_address, remote_port }>, content_length: Option<i64>, error: Option<String> }`.
    - `HttpResponseData { start_time, end_time: Option<i64>, status_code: Option<u16>, reason_phrase, headers, content_length, error, connection_info }`.
    - `HttpProfileEntry::is_complete()` (fact P11) and `is_websocket_upgrade()`.
    - `get_http_profile_request(isolate, id) -> HttpProfileRequestFull { entry: HttpProfileEntry /* flattened */, request_body: Option<Vec<u8>>, response_body: Option<Vec<u8>> }`. The bodies are byte arrays from JSON integer arrays.
    - `clear_http_profile(isolate)`.
    - `socket_profiling_enable(isolate, bool)`.
    - `get_socket_profile(isolate) -> Vec<SocketStat { id, address, port, socket_type, start_time, end_time: Option<i64>, read_bytes, write_bytes, last_read_time: Option<i64>, last_write_time: Option<i64> }>`.
    - `HTTP_CALIBRATION_SINCE: i64 = 9_000_000_000_000_000`.
- [ ] **Step 1: Write the failing decode tests** (`tests/decode_recordings.rs`) over both recordings.
  - Every `recv` result frame decodes into its type, keyed by the method of the matching `send`.
  - Every `streamNotify` decodes to a `StreamEvent`. `Other` is allowed only for the kinds the recording also shows as unhandled; list them in the test.
  - **Counts in the debug recording:**
    - Logging ≥ 5
    - Write (Stdout) ≥ 2,000
    - Flutter.Error ≥ 1
    - Flutter.Frame ≥ 1
    - Flutter.Navigation ≥ 1
  - **Profile recording:** zero Flutter.Error.
  - `detect_mode`: the debug recording gives Debug and the profile recording gives Profile.
  - The 30-line message's `InstanceRef` has `value_as_string_is_truncated == true` and `length == Some(410)`.
  - **HTTP:**
    - GET `/v2/products?page=3` → 200.
    - `refused` has `request.error`.
    - The `/ws` entry `is_websocket_upgrade()` with status 101.
    - The 262,144-byte response body decodes to 262,144 bytes.
- [ ] **Step 2: Write the failing service tests** (`#[tokio::test]`).
  - `VmService` against `ReplayServer`: `get_vm` and `get_object_string` (the full 410 characters).
  - `stream_listen_accepts_already_subscribed`: `ScriptedServer` answering `(103, "Stream already subscribed")`.
  - `flutter_version` called with the recorded method name.
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_vm`
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(vm): add the typed VM service and dart:io profiling API`.

### Task 8: DTD client and app discovery

**Files:**
- Create: `crates/fdt_vm/src/dtd.rs`, `src/discovery.rs`
- Modify: `crates/fdt_vm/Cargo.toml`:
  - `dirs` (workspace)
  - `libc` on unix
  - `windows` with `Win32_Foundation` and `Win32_System_Threading` on windows (same `windows` version as `fdt_ui`)

**Interfaces:**
- **Consumes:** Tasks 5 and 7.
- **Produces:**
  - **`dtd`**
    - `DtdClient::connect(ws: &url::Url) -> Result<DtdClient, ConnectError>`.
    - `get_vm_services() -> Result<Vec<VmServiceEntry { uri: String, name: Option<String>, exposed_uri: Option<String> }>, RpcError>`.
    - `closed()`.
    - `AppLabel { kind: Option<String>, device: Option<String>, package: Option<String> }` and `parse_app_label(&str) -> AppLabel`. It parses `"Kind: Flutter - Device: macOS - Package: jank_lab"` and tolerates missing or reordered parts.
  - **`discovery`**
    - `Os { MacOs, Windows, Linux }`.
    - `registry_dir_for(os, env: &dyn Fn(&str) -> Option<String>, home: Option<PathBuf>) -> Option<PathBuf>` (fact P1, including `DART_DATA_HOME`).
    - `registry_dir() -> Option<PathBuf>`.
    - `DtdEntry { ws_uri: String, epoch: i64, pid: u32, dart_version: Option<String>, workspace_root: Option<String> }`.
    - `read_registry(dir) -> Vec<DtdEntry>`. It reads only files whose names are digits, skips unreadable or partial JSON, and sorts newest `epoch` first.
    - `pid_alive(pid) -> bool`:
      - unix: `kill(pid, 0) == 0 || errno == EPERM`
      - windows: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` plus `GetExitCodeProcess == STILL_ACTIVE`
    - `list_via_cli(dart: &Path) -> Vec<DtdEntry>` runs `dart tooling-daemon --list --machine`. It is used when `registry_dir()` is `None`.
    - `AppKey { package: String, device: String, workspace: String }`.
    - `DiscoveredApp { key: AppKey, vm_uri: String, name: String, device: String, mode: Option<Mode>, flutter_version: Option<String>, workspace: Option<String>, pid: Option<u32> }`.
    - `DiscoveryConfig { interval: Duration /* 1 s */, probe_timeout: Duration /* 2 s */, registry_dir: Option<PathBuf> /* override */, dart_cli: Option<PathBuf> }`.
    - `Discovery::spawn(handle: &tokio::runtime::Handle, config) -> (Discovery, watch::Receiver<Arc<[DiscoveredApp]>>)`. Dropping the `Discovery` stops it.
  - **The loop, every `interval`:**
    1. Read the registry and keep entries whose pid is alive.
    2. Keep one `DtdClient` per entry and drop it when it closes.
    3. Call `get_vm_services` on each client.
    4. Probe each VM `uri` once, cached by uri, within `probe_timeout`:
       - `getVM`: pid
       - `getIsolate(main)`: mode and `rootLib` package
       - `streamListen("Service")`, then the first `ServiceRegistered` ending in `flutterVersion`, called within 1 s
    5. Publish the list, sorted by `(name, device)`.
  - **Field sources:**
    - `name`: the label's package, else the `rootLib` package, else "Dart app".
    - `device`: the label's device, else `getVM.operatingSystem`.
    - `workspace`: the entry's `workspace_root`.
  - **Removal.** An app disappears when its DTD closes, its uri leaves `getVmServices`, or its probe fails. A probe failure is retried after 5 s.
- [ ] **Step 1: Write the failing tests.**
  - `registry_dir_for_each_os`:
    - macOS `~/Library/Application Support/Dart/dtd`
    - Windows `%LOCALAPPDATA%\Dart\dtd`
    - Linux `XDG_STATE_HOME` set and unset
    - `DART_DATA_HOME` overrides all
  - `read_registry_skips_garbage_and_partial_files`.
  - `dead_pids_are_ignored`: spawn `std::process::Command::new(if windows {"cmd"} else {"true"})`, wait, then use its pid.
  - `parse_app_label_cases`.
  - `discovery_finds_probes_and_drops_an_app` (`#[tokio::test]`):
    - A temp registry dir holds a file named after `std::process::id()`, pointing at a `ScriptedServer` DTD.
    - That DTD answers `ConnectedApp.getVmServices` with a second `ScriptedServer` VM.
    - The VM answers `getVM`, `getIsolate` (extensionRPCs include `ext.flutter.reassemble`; rootLib `package:jank_lab/main.dart`) and `streamListen`, then sends `ServiceRegistered { method: "s1.flutterVersion" }` and answers it.
    - With `interval` 50 ms, the receiver yields one app: name "jank_lab", device "macOS", mode Debug, version "3.47.9", and the workspace from the file.
    - After `close_abruptly()` on the DTD, the list is empty within 200 ms.
- [ ] **Step 2: Confirm they fail, implement, verify.**
  - Run: `cargo test -p fdt_vm`
  - Expected: PASS.
- [ ] **Step 3: Commit** `feat(vm): add DTD client and live app discovery`.

### Task 9: `fdt_capture`: connect, ingest logs and frames, publish snapshots

**Files:**
- Create:
  - `crates/fdt_capture/Cargo.toml` (deps `fdt_store`, `fdt_vm`, `tokio`, `serde_json`, `base64`, `log`; dev-dep `fdt_vm` with `testing`)
  - `src/lib.rs`, `src/runtime.rs`, `src/config.rs`, `src/status.rs`, `src/capture.rs`, `src/connect.rs`
  - `src/ingest/{mod,logs,stdout,flutter_error,frames}.rs`
  - `tests/replay_debug.rs`
- Modify: the root `Cargo.toml` (members)

**Interfaces:**
- **Consumes:** Tasks 1, 2, 4, 5 and 7.
- **Produces:**
  - **Runtime.** `CaptureRuntime::new() -> io::Result<CaptureRuntime>`: a multi-thread tokio runtime with 2 workers named `fdt-capture`. `handle() -> tokio::runtime::Handle`.
  - **`CaptureConfig`** (with `Default`):

    | Field | Default |
    |---|---|
    | `publish_interval` | 16 ms |
    | `http_poll` | 500 ms |
    | `socket_poll` | 2 s |
    | `body_limits: BodyLimits` | as in Task 3 |
    | `auto_fetch_limit: u64` | 4 MiB |
    | `free_app_http_memory: bool` | false |
    | `stdout_flush` | 100 ms |
    | `string_fetch_timeout` | 2 s |
    | `reconnect_uri_retry` | 1 s |
    | `reconnect_uri_give_up` | 30 s |

  - **`Target`**: `Discovered(DiscoveredApp)` or `Uri(VmServiceUri)`.
  - **`ConnStatus`** (Clone, PartialEq):
    - `Idle`
    - `Connecting { label: String }`
    - `Connected { run: RunId, app: AppInfo, recording: bool }`
    - `Disconnected { app: Option<AppInfo>, reason: String, waiting_for: Option<AppKey> }`
    - `Failed { message: String, details: String }`
  - **`Capture`**
    - `start(handle, config, discovery: Option<watch::Receiver<Arc<[DiscoveredApp]>>>) -> Capture`. It starts with an empty session and status `Idle`.
    - Commands: `connect(Target)`, `disconnect()`, `stop_waiting()`, `set_recording(bool)`, `new_session()`, `load_full_body(EntryId, BodyKind)`, `set_free_app_http_memory(bool)` and `set_body_limits(BodyLimits)`. Tasks 10 and 11 implement the later ones.
    - `status() -> watch::Receiver<ConnStatus>` and `snapshots() -> watch::Receiver<Arc<Snapshot>>`.
  - **Internal (`pub(crate)`, unit-tested)**
    - `LineAssembler { push(&[u8]) -> Vec<String>, flush() -> Option<String> }`. It strips a leading `flutter: ` from each complete line and handles `\r\n`.
    - `parse_flutter_error(&Value) -> ErrorDetail` (fact P9):
      - `summary` is the first `ErrorSummary.description`, else `description`.
      - `widget` and `location` come from the first child of the "The relevant error-causing widget was" block. The first whitespace-separated token is the widget name, and the location is what follows the `<name>:` prefix.
      - `stack` is the `DiagnosticsStackTrace` property descriptions.
      - `rendered` is `renderedErrorText` when `errorsSinceReload` is 0 or absent. Otherwise every property description is joined with `\n`.
- **The session task** owns the `SessionWriter`. Connection tasks send it `Ingest` messages over an unbounded mpsc channel. It applies them and publishes whenever it is dirty and at least `publish_interval` has passed since the last publish. Otherwise it sets one timer for the remainder. It never publishes when nothing changed.
- **The connect sequence.** Run independent calls together with `tokio::join!`:
  1. `connect` with `ConnectOptions::default()`.
  2. `getVM` and pick the main isolate (fact P4).
  3. `getIsolate(main)` for the mode and package. At the same time:
     - `streamListen` for `Logging Stdout Stderr Extension Isolate Service`
     - calibration: `host_now_us` → `getVMTimelineMicros` → `getHttpProfile(main, HTTP_CALIBRATION_SINCE)` → `host_now_us`, then `calibrate`
  4. Start the run. The session origin is `host_now_us()` at the first connect of the session.
  5. Set status `Connected`.
  6. The Flutter version is filled in later via `update_run_app` when `ServiceRegistered` arrives.
- **Ingest rules**
  - **Logging events** are processed in order by one task.
    - If the message (or logger name) is truncated, it awaits `get_object_string` with `string_fetch_timeout`. On failure it uses the truncated text plus "…".
    - `level_from_dart`.
    - `time` is `from_epoch_ms(time_ms)`.
    - If `error` or `stack_trace` is not `Null`, their `value_as_string` (else `class_name`) is appended on new lines prefixed "Error: " and "Stack trace:".
  - **Write events** go through one `LineAssembler` per stream, flushed after `stdout_flush` of quiet. Stdout lines are `Info`; Stderr lines are `Error`.
  - **`Flutter.Error`**: an `Error` record, source `FlutterError`, text `"<description>: <summary>"`, with `error: Some(detail)`.
  - **`Flutter.Navigation`**: an `Info` record, source `Navigation`, text `"Navigated to <settings.name or description>"`.
  - **`Flutter.Frame`**: `push_frame` with `from_timeline_us(startTime)`.
- [ ] **Step 1: Write the failing unit tests.**
  - `line_assembler_joins_split_writes_and_strips_prefix`: `"flutter: a"` then `"\n"`, then `"flutter: b\nflutter: c"`, then `"\n"` gives a, b, c.
  - `partial_line_flushes_after_quiet` (`tokio::time::pause`).
  - `crlf_lines`.
  - `parse_flutter_error_from_recording`: load the first `Flutter.Error` from `jank_lab-debug.jsonl`. Expect widget `_Thrower`, a location ending `logs.dart:<line>:<col>`, stack non-empty, and summary "Bad state: Deliberate failure in build".
- [ ] **Step 2: Write the failing replay test** (`tests/replay_debug.rs`). Start a `Capture` on a `CaptureRuntime`, connect with `Target::Uri(<ReplayServer of jank_lab-debug.jsonl>)`, and wait until status is `Connected` and no new generation has appeared for 300 ms (5 s timeout). Assert:
  - Records with source `Log` include loggers `jank_lab.net` and `jank_lab.ui` at Debug, Info, Warning and Error.
  - Exactly 1,000 Stdout records match `^burst line \d+$` (prefix removed) and appear consecutively.
  - One record has `line_count == 30` with the full text ending in "line 30 of 30".
  - One `FlutterError` record has `error.widget == Some("_Thrower")`.
  - One `Navigation` record reads "Navigated to /logs".
  - `frames.len() >= 1`.
  - The run's `app.mode == Mode::Debug`, `app.name == "jank_lab"`, and `flutter_version` is `Some`.
- [ ] **Step 3: Write the failing coalescing test.** A `ScriptedServer` sends 2,000 `Stdout` write notifications in one burst after `streamListen`. Count the generations seen by a `snapshots()` receiver over the 300 ms after the burst. Expect ≤ 25, with the final snapshot holding 1,000 lines.
- [ ] **Step 4: Implement, then verify.**
  - Run: `cargo test -p fdt_capture`
  - Expected: PASS.
- [ ] **Step 5: Commit** `feat(capture): connect to apps and ingest logs, errors, navigation and frames`.

### Task 10: `fdt_capture`: HTTP and socket capture

**Files:**
- Create: `crates/fdt_capture/src/ingest/http.rs`, `src/ingest/sockets.rs`
- Modify: `src/capture.rs`, `src/connect.rs`, `tests/replay_debug.rs`
- Test: `tests/network.rs`

**Interfaces:**
- **Consumes:** Tasks 3, 7 and 9.
- **Produces:**
  - `to_update(entry: &HttpProfileEntry, clock: &RunClock, run: RunId) -> HttpUpdate`, `pub(crate)` and unit-tested:
    - `end` comes from `response.endTime`, else from the error time.
    - Otherwise `end` is `None`.
    - For a failed request, `end` is `entry.end_time`.
    - `websocket` is `is_websocket_upgrade()`.
    - `remote` comes from `request.connectionInfo`.
  - The `Capture` commands `load_full_body`, `set_free_app_http_memory` and `set_body_limits` become functional.
- **The poller** runs one task per isolate that exposes `ext.dart.io.getHttpProfile`.
  - **Enabling.** Profiling (HTTP and sockets) is enabled on connect. On a new isolate, it is enabled when `ServiceExtensionAdded { rpc: "ext.dart.io.httpEnableTimelineLogging" }` arrives (fact P15).
  - **Cadence.** Every `http_poll`, call `get_http_profile(isolate, updated_since)`, where `updated_since` is the previous response's `timestamp`. Upsert each entry.
  - **Newly completed requests:**
    - A websocket upgrade gets no fetch, and both bodies are `NotCaptured`.
    - If `response.contentLength > auto_fetch_limit`, the response body is `TooLarge { len }`.
    - Otherwise mark both bodies `Pending` and fetch `get_http_profile_request`, with at most 4 fetches in flight per run.
    - Store the bodies with `store_body`; an empty body becomes `NotCaptured`.
    - On error, both become `Unavailable { reason }`.
  - **Clearing.** After a poll, if `free_app_http_memory` is set and no request in this isolate is incomplete in the store, call `clear_http_profile` and reset `updated_since` to `None`.
  - **Stopping.** The isolate's poller stops on `IsolateExit`.
- **The socket poller** runs every `socket_poll`, but only while the run has at least one websocket request. It upserts each `SocketStat`, with times from `from_timeline_us`.
- **`load_full_body(id, kind)`.** If connected to the record's run, fetch again and store the full bytes, bypassing `per_body` but counting toward `total`. Otherwise reply `Unavailable { reason: "Reconnect to the app to load the rest." }`.
- [ ] **Step 1: Write the failing `to_update` unit tests**, using entries from the debug recording: a completed GET, the failed request, an in-flight `/slow` sample if recorded (else a hand-made entry with no `response`), and the 101 upgrade.
- [ ] **Step 2: Write the failing replay assertions** (extend `tests/replay_debug.rs`):
  - GET `/v2/products?page=3` is 200, with a JSON response body `Captured` and `size() == Some(1039)` or the recorded length.
  - POST `/v2/orders` is 500, with request body bytes `{"cart":1}`.
  - `refused` is `Failed` and `is_error()`.
  - The `/ws` request is `websocket`, not an error, with both bodies `NotCaptured`.
  - The 262,144-byte blob is `Captured` and not truncated with default limits. With `per_body` set to 100 KiB before connecting, it is truncated: 102,400 bytes of 262,144.
- [ ] **Step 3: Write the failing `ScriptedServer` tests** (`tests/network.rs`):
  - **`hot_restart_reenables_profiling`.** Send `IsolateExit(old)`, `IsolateStart(new)` and `ServiceExtensionAdded(new, "ext.dart.io.httpEnableTimelineLogging")`. The server must then receive `httpEnableTimelineLogging {isolateId: new, enabled: true}` and `socketProfilingEnabled` for the new isolate, and polls must move to the new isolate.
  - **`oversized_bodies_are_not_fetched`.** `contentLength` 50,000,000 gives `TooLarge`, and `received()` contains no `getHttpProfileRequest`.
  - **`duplicate_entries_from_inclusive_updated_since_do_not_duplicate`.**
  - **`free_app_memory_clears_only_when_nothing_is_in_flight`.**
  - **`app_dies_during_body_fetch`.** `close_abruptly()` while `getHttpProfileRequest` is pending makes the body `Unavailable`, with no panic.
  - **`sockets_are_polled_only_after_a_websocket_and_linked`.**
- [ ] **Step 4: Implement, then verify.**
  - Run: `cargo test -p fdt_capture`
  - Expected: PASS.
- [ ] **Step 5: Commit** `feat(capture): record HTTP requests, bodies and websocket sockets`.

### Task 11: `fdt_capture`: disconnects, reconnects, runs, pause and new session

**Files:**
- Create: `crates/fdt_capture/src/lifecycle.rs`, `tests/lifecycle.rs`
- Modify: `src/capture.rs`, `src/connect.rs`

**Interfaces:**
- **Consumes:** Tasks 8–10.
- **Produces:** the behaviour of `disconnect`, `stop_waiting`, `set_recording` and `new_session`.
- **The rules:**
  - **The socket closes on its own** (any code, including none):
    1. End the run at `host_now`.
    2. Stop the pollers.
    3. Mark `Pending` bodies `Unavailable { reason: "The app stopped before the body was copied." }`.
    4. Set status `Disconnected { app, reason: "<name> stopped.", waiting_for }`. `waiting_for` is the `AppKey` for a discovered target. A URL target keeps retrying the same URI every `reconnect_uri_retry` until `reconnect_uri_give_up`; `waiting_for` stays `None`.
  - **Automatic reconnect.** While `waiting_for` is `Some(key)`, the first discovered app with that key gets connected. It is the **same run** if `getVM.pid` and `startTime` equal the ended run's values (then reopen the run: `ended = None`). Otherwise it is a **new run**, with a new `RunClock` calibration and the same origin.
  - **`stop_waiting()`.** Set `waiting_for = None` and stop retrying.
  - **`disconnect()`.** Close the connection, end the run, and set `Disconnected { reason: "Disconnected.", waiting_for: None }`.
  - **Hot restart.** `IsolateExit` and then `IsolateStart` of a new main isolate keep the **same run**. The main isolate id updates (Task 10 re-enables profiling).
  - **`set_recording(false)`.**
    - Ingest drops every log, frame, request and socket message.
    - Pollers keep advancing `updated_since`, so requests made while paused are never shown later.
    - The status shows `recording: false`.
    - `set_recording(true)` resumes.
  - **`new_session()`.** Replace the writer with `SessionWriter::new(host_now_us())`. If connected, start run 1 of the new session for the same VM, with a new clock origin and the same calibration. Publish immediately.
  - **A failed connect** gives `Failed { message: ConnectError::message(), details: <debug text> }`. A protocol error during the sequence gives the message "Connected, but the app didn't answer as expected." with the RPC error as details.
- [ ] **Step 1: Write the failing tests** (`tests/lifecycle.rs`, using `ScriptedServer` and a hand-driven `watch::Sender<Arc<[DiscoveredApp]>>`):
  - `app_exit_keeps_data_and_waits_for_the_same_app`.
  - `same_key_new_vm_starts_run_two` (two runs; the run 2 start is after the run 1 end; logs from both runs are present).
  - `same_vm_reconnect_continues_the_run`.
  - `stop_waiting_ignores_the_app_when_it_returns`.
  - `url_target_retries_then_gives_up` (`tokio::time::pause`).
  - `hot_restart_keeps_the_run`.
  - `paused_recording_drops_events_and_resumes`.
  - `new_session_starts_empty_and_keeps_the_connection`.
  - `disconnect_command`.
  - `connect_to_nothing_reports_failed_with_message` (the refused port; the message starts "Nothing is listening at").
  - `app_dies_during_the_connect_sequence` (close after `getVM`): status is `Failed` or `Disconnected`, with no panic and no run left open.
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_capture`
  - Expected: PASS. Also run clippy and confirm `cargo tree -p fdt_capture | grep -c gpui` prints 0.
- [ ] **Step 3: Commit** `feat(capture): handle disconnects, reconnects, runs, pause and new sessions`.

### Task 12: Shell changes carried from M1

**Files:**
- Modify:
  - `crates/fdt_app/src/nav.rs`, `keymap.rs`, `settings.rs`, `logging.rs`, `Cargo.toml` (`log = { workspace = true, features = ["std"] }`)
  - `src/shell/{mod,tool_page,settings_page,shortcuts,about}.rs`, `src/palette/view.rs`
  - `crates/fdt_ui/src/tokens.rs`

**Interfaces:**
- **Consumes:** Task 1.
- **Produces:**
  - **`nav`**
    - `Page::Start`.
    - `Selection` is `Log(EntryId)`, `Request(EntryId)` or `Socket(EntryId)`. It replaces `SelectionKey`.
    - `Location.selection: Option<Selection>`.
    - `NavHistory`'s initial location is `Location::at(Page::Start)`.
  - **`keymap`**
    - `SHELL_NOT_INPUT: &str = "Shell && !Input"`. It is used for `?`, `escape`, `cmd-[`/`ctrl-[` and `cmd-]`/`ctrl-]`.
    - `bindings()` keeps every other binding.
  - **`settings`**
    - New fields:
      - `free_app_http_memory: bool` (false)
      - `body_cap_kib: u32` (1024)
      - `body_budget_mib: u32` (200)
      - `recent_urls: Vec<String>` (newest first, at most 5)
    - **Tolerant parsing.** Parse into a `serde_json::Map`, then read each known field on its own. A field with the wrong type keeps its default and logs one warning naming the field. Unknown fields are ignored.
    - `record_recent_url(&mut Vec<String>, &str)`: dedupe, then put the URL first and cap at 5.
  - **`logging`.** Lines are written with `writeln!(io::stderr().lock(), …)`, and write errors are ignored.
  - **`shell::dialog_window_actions(el: Div, shell: FocusHandle) -> Div`.** It registers `CloseWindow`, `Minimize`, `Zoom` and `ToggleFullScreen` handlers that close the dialog, focus the shell, then re-dispatch the action. It is used by the palette, the cheat sheet and About.
  - **`tool_page::empty_copy(tool, connected: bool)`.** When `connected` is true, Timeline, Performance, Memory, Widgets and State return:
    - heading "Not available yet"
    - body "<Tool label> isn't built yet. Logs and Network are recording this app."
  - **Pages own scrolling.** The settings page's content is `div().id("settings-scroll").overflow_y_scroll()`. The page container keeps `.min_h_0()`.
  - **`Page::Start` placeholder.** Until Task 14 replaces it, `Page::Start` renders an `EmptyState` (`id: "start-page"`) with heading "Connect to a running app" and body "Running Flutter apps appear here." The sidebar is unchanged.
  - **Tokens.**
    - `layout`:
      - `STRIP_HEIGHT = 32`, `TOOLBAR_HEIGHT = 40`, `BANNER_HEIGHT = 32`
      - `DETAILS_WIDTH = 380`, `DETAILS_MIN = 280`, `DETAILS_MAX = 640`
      - `START_WIDTH = 720`, `WATERFALL_WIDTH = 160`, `STATUS_DOT = 8`
    - `table` (new module): `COL_METHOD = 72`, `COL_STATUS = 64`, `COL_DURATION = 80`, `COL_SIZE = 80`, `COL_TIME = 96`, `COL_LEVEL = 48`, `COL_LOGGER = 140`, `COL_HOST = 180`.
    - `motion::LOADING_DELAY = 150 ms`.
- [ ] **Step 1: Write the failing tests.**
  - `selection_round_trips_through_history`.
  - `shell_starts_on_the_start_page`. Update M1 tests that assumed Timeline: they navigate explicitly.
  - `question_mark_types_into_a_focused_input`: a test view with an `InputState` inside the shell; typing `?` puts "?" in the input, and the cheat sheet is absent.
  - `back_shortcut_does_not_fire_in_an_input`.
  - `one_bad_settings_field_keeps_the_others` (`{"theme":"dark","recent_commands":5}` → theme Dark, `recent_commands` empty).
  - `new_settings_fields_default`.
  - `recent_urls_dedupe_and_cap`.
  - `logger_ignores_write_errors`: factor `write_record(&mut impl Write, &Record)` and pass a writer that always errors.
  - `close_window_works_while_the_palette_is_open` (press ⌘W with the palette open → `cx.windows().is_empty()`).
  - `settings_page_scrolls_in_a_short_window` (the window is `WINDOW_MIN` height; `settings-scroll` exists).
  - `not_built_tools_say_so_when_connected`.
- [ ] **Step 2: Confirm they fail, implement, verify.**
  - Run: `cargo test --workspace`
  - Expected: PASS, including `token_usage`.
- [ ] **Step 3: Commit** `feat(app): add start page routing, input-safe shortcuts, tolerant settings and dialog window actions`.

### Task 13: Capture host, session model, title bar status and the disconnected banner

**Files:**
- Create: `crates/fdt_app/src/capture_host.rs`, `src/session_model.rs`, `src/shell/banner.rs`
- Modify:
  - `src/app.rs`, `src/actions.rs`, `src/keymap.rs`, `src/menus.rs`
  - `src/palette/registry.rs`
  - `src/shell/{mod,title_bar}.rs`
  - `Cargo.toml` (deps `fdt_capture`, `fdt_vm`, `fdt_store`, `tokio` with `sync`)

**Interfaces:**
- **Consumes:** Tasks 9–12.
- **Produces:**
  - **`capture_host::CaptureControl`**: a trait, object-safe and `'static`. It has every `Capture` command plus `status()` and `snapshots()` receivers. `impl CaptureControl for fdt_capture::Capture`.
  - **`CaptureHost` (Global)**
    - It holds one `CaptureRuntime`, one `Discovery` and its receiver.
    - `CaptureHost::install(cx)` runs after the first frame, the same way `defer_menus` does, so startup does not pay for it.
    - `new_capture(&self, config) -> Box<dyn CaptureControl>` and `apps() -> watch::Receiver<Arc<[DiscoveredApp]>>`.
  - **`SessionModel`** (entity, one per window):
    - Constructors: `new(control: Box<dyn CaptureControl>, cx)`, and `fixed(snapshot: Arc<Snapshot>, status: ConnStatus)` for tests and bench-ui.
    - Accessors: `snapshot() -> &Arc<Snapshot>`, `status() -> &ConnStatus`, `control() -> Option<&dyn CaptureControl>`.
    - `recording_elapsed() -> Option<Duration>`.
    - `subscribe`-able `SessionEvent::{Snapshot, Status}`.
    - Two `cx.spawn` loops await `changed()` on the watch receivers, update and `cx.notify()`. A 1 Hz timer runs only while `Connected { recording: true }`.
  - **`MainWindow::new(window, cx, session: Entity<SessionModel>)`.** `open_main_window` builds the model from `CaptureHost`; `test_support::open_shell` uses `SessionModel::fixed(Snapshot::empty(), ConnStatus::Idle)`.
  - **Actions and bindings** (added to the registry, the cheat sheet and a new File menu on macOS, all with context `SHELL_CONTEXT` unless noted):

    | Action | Binding | Notes |
    |---|---|---|
    | `ToggleRecording` | `cmd-r` / `ctrl-r` | "Start or Stop Recording", group View |
    | `NewSession` | `cmd-n` / `ctrl-n` | "New Session", group App |
    | `NewWindow` | `cmd-shift-n` / `ctrl-shift-n`, context None | "New Window", group Window |
    | `ConnectToApp` | none | "Connect to an App…"; goes to `Page::Start` |
    | `Disconnect` | none | |
    | `StopWaiting` | none | |

  - **Title-bar chip** (`id: "connection-chip"`; clicking it goes to `Page::Start`):

    | Status | Chip text |
    |---|---|
    | `Idle` | "No app connected" |
    | `Connecting` | "Connecting to {label}…" |
    | `Connected` | "{name} · {device} · {mode}" (mode in lowercase), plus `id: "rec-chip"`: a filled dot in `danger` followed by "Rec {m:ss}", or "Paused" in `text_muted` |
    | `Disconnected` | "{name} · disconnected" with a warning icon and `warning` text |
    | `Failed` | "Couldn't connect" |

  - **Banner** (`id: "disconnected-banner"`, `layout::BANNER_HEIGHT`, below the title bar, only on tool pages):
    - **Disconnected and waiting:** "{name} stopped. Waiting for it to start again. Your recorded data is kept." with a button `id: "banner-stop-waiting"` reading "Stop waiting".
    - **Disconnected, not waiting:** "Disconnected from {name}. Your recorded data is kept." with a button `id: "banner-connect"` reading "Connect to an app".
  - **Navigation on status changes.** When the status becomes `Connected` while on `Page::Start`, navigate to `Page::Tool(Tool::Logs)`. Sidebar counts stay `None` here (Task 15 fills them).
  - **Settings wiring.** `CaptureConfig` is built from the settings:
    - `body_limits` = `body_cap_kib` and `body_budget_mib`
    - `free_app_http_memory`

    The Settings page gains a "Network" section with a switch `id: "settings-free-http-memory"`, labelled "Free app memory after copying requests". Its hint reads "Clears the app's own HTTP profile after fdt copies each request. Other tools attached to the app lose those requests." Toggling it updates the setting and calls `set_free_app_http_memory` on every window's capture.
  - **Discovery fallback.** `DiscoveryConfig.dart_cli` is the first `dart` (or `dart.bat` on Windows) found on `PATH`, searched with `std::env::split_paths`.
- [ ] **Step 1: Write the failing tests** (`#[gpui_kit::test]`, using a `FakeCapture: CaptureControl` that records commands and exposes `watch::Sender`s):
  - `title_shows_each_status` (all five strings).
  - `recording_clock_ticks_once_per_second` (`advance_clock`).
  - `cmd_r_toggles_recording`.
  - `connected_status_moves_start_page_to_logs`.
  - `banner_offers_stop_waiting_and_sends_it`.
  - `new_window_opens_a_second_window_on_the_start_page`.
  - `registry_and_menus_list_the_new_commands` (the M1 cross-check tests are extended).
  - `idle_model_does_no_work`: no renders are triggered over 5 s of `advance_clock` with status `Idle`.
  - `free_http_memory_setting_reaches_the_capture`.
  - `capture_host_installs_after_the_first_frame`: `cx.has_global::<CaptureHost>()` is false when the window first opens and true once the next frames have run.
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
  - Then `cargo build --release -p fdt_app` and run `./target/release/fdt --startup-trace` 7 times before this task's changes (`git stash`) and 7 times after. Report both medians and the machine load (`uptime`). This is informational; the structural test above is the gate.
- [ ] **Step 3: Commit** `feat(app): host the capture runtime and show connection status`.

### Task 14: Start page

**Files:**
- Create: `crates/fdt_app/src/shell/start_page.rs`
- Modify: `src/shell/mod.rs`, `src/shell/sidebar.rs` (the start page is reachable from a sidebar item "Connect" above Timeline, `id: "sidebar-connect"`)

**Interfaces:**
- **Consumes:** Tasks 12 and 13 (`CaptureHost::apps`, `SessionModel`), and `fdt_vm::{DiscoveredApp, VmServiceUri}`.
- **Produces:** `StartPage` (entity), created by `MainWindow`, with `StartPage::new(apps: watch::Receiver<Arc<[DiscoveredApp]>>, session: Entity<SessionModel>, window, cx)`.
- **Layout.** A centred column, `layout::START_WIDTH` wide.
  1. Heading "Connect to a running app".
  2. The app list, then the "Or paste a VM service URL" input (`id: "start-url"` on its wrapper div), then "Recent URLs".
- **App list.** Rows `id: "start-app-{i}"` are 26 px each and show:
  - the name
  - the device
  - the mode as a text badge ("debug" / "profile")
  - "Flutter {version}"
  - the last two workspace path components
- **List keys.** The list is focusable with key context `"AppList"`. `up`/`down`/`j`/`k` move, `enter` connects, and a click connects.
- **Empty list:** "No running Flutter apps found. Start one with `flutter run`, or paste its VM service URL below."
- **The URL field.**
  - Enter parses with `VmServiceUri::parse`.
  - On error it shows `id: "start-url-error"` with `UriError::message()` in `danger` text.
  - On success it calls `connect(Target::Uri(..))` and `record_recent_url`.
- **Recent URLs.** Rows `id: "start-recent-{i}"` show `redacted()` text, and a click connects.
- **Status display.**
  - **Connecting:** the row or field shows a `Spinner` only after `motion::LOADING_DELAY` (U12).
  - **Failed:** `id: "start-error"` shows the message, and a "Show details" toggle (`id: "start-error-details-toggle"`) reveals the details with a Copy button (`id: "start-error-copy"`).
- [ ] **Step 1: Write the failing tests.**
  - `lists_discovered_apps` (2 apps; texts asserted).
  - `arrow_down_and_enter_connects_the_second_app` (FakeCapture got `Target::Discovered(app 1)`).
  - `invalid_url_shows_message` (exact `NotVmService` copy).
  - `valid_url_connects_and_is_remembered`.
  - `spinner_appears_only_after_the_delay` (100 ms → absent; 200 ms → present).
  - `failure_shows_message_and_copyable_details`.
  - `empty_state_copy`.
  - `list_updates_live` (send a new app list → the row appears).
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
- [ ] **Step 3: Commit** `feat(app): add the start page with live app discovery and URL connect`.

### Task 15: Session strip, shared time range, range chip and sidebar counts

**Files:**
- Create: `crates/fdt_app/src/shell/strip.rs`, `src/tools/mod.rs` (the `ToolHeader` used by tool pages: page title, then the strip)
- Modify: `src/shell/{mod,title_bar,sidebar,tool_page}.rs`

**Interfaces:**
- **Consumes:** Tasks 4, 12 and 13.
- **Produces:**
  - **Pure functions** (plain `#[test]`):
    - `format_offset(SessionTime) -> String`: `m:ss` below 1 h, else `h:mm:ss`.
    - `format_range(TimeRange) -> String` ("0:51–1:07", with an en dash).
    - `session_span(snap: &Snapshot, now: Option<SessionTime>) -> TimeRange`. It runs from 0 to the max of `latest_time()` and `now`, with at least 10 s.
    - `strip_layout(snap, span, width_px) -> StripLayout { bars: Vec<(f32 /*x*/, f32 /*w*/, f32 /*h frac*/, BarKind)>, dividers: Vec<(f32, RunId)> }`:
      - `BarKind`: `Ok` ≤ 16.7 ms, `Slow` ≤ 33.4 ms, else `Janky`.
      - The height fraction is `min(max_elapsed / 33.4 ms, 1.0)`. The budget line sits at half height. Janky bars are always full height.
    - `x_to_time(x_frac, span) -> SessionTime`.
  - **`SessionStrip` element** (`id: "session-strip"`, height `layout::STRIP_HEIGHT`; it is a canvas, U8).
    - It paints the bars (`success`, `warning`, `danger`), the budget line (`border`) and the run dividers (`text_faint`, plus a "Run N" label where it fits).
    - It paints the range overlay (`accent` at `alpha::SELECTION`) and a live preview while dragging.
    - **Drag.** A drag commits one `navigate` with the new `range` on mouse up. A plain click, moving less than 3 px, does nothing.
    - **Double-click** clears the range.
  - **Range chip** (`id: "range-chip"`, title bar). It is shown when `current().range.is_some()` and reads `format_range`. Its clear button `id: "range-chip-clear"` navigates with `range: None`.
  - **Sidebar counts.** On each snapshot or range change, a background job (`cx.background_spawn`; the result is applied only if the generation and range still match) computes:
    - **Logs:** records in range; `attention` if any are Error-level in range.
    - **Network:** requests in range; `attention` if any are errors in range.
    - Results go through `set_counts`.
  - **Esc.** Esc clears the selection first, then the range. This is M1 behaviour, now also when focus is inside the strip.
- [ ] **Step 1: Write the failing pure tests.**
  - `format_offset_cases` (0 → "0:00"; 65 s → "1:05"; 3,725 s → "1:02:05").
  - `format_range_uses_an_en_dash`.
  - `span_has_a_minimum_and_follows_now`.
  - `strip_layout_classifies_bars_and_places_dividers` (two runs; one 40 ms frame → `Janky`, height 1.0).
  - `x_to_time_maps_edges`.
- [ ] **Step 2: Write the failing view tests.**
  - `dragging_the_strip_sets_one_history_entry` (a drag from 25 % to 75 % gives `range ≈ [25 %, 75 %]` of the span within one bucket, and `history` back depth +1).
  - `drag_past_the_edge_clamps`.
  - `double_click_clears_the_range` (U8 `simulate_event`).
  - `range_chip_shows_and_clears`.
  - `counts_follow_the_range` (a fixed snapshot with 10 logs, 3 in range, 1 of them Error → Logs count 3 with attention).
  - `escape_clears_selection_then_range`.
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(app): add the session strip, shared time range and range-aware counts`.

### Task 16: Logs tool: the virtualised list, follow mode, inline expansion and filters

**Files:**
- Create: `crates/fdt_app/src/tools/logs/{mod,model,view,toolbar}.rs`
- Modify: `src/shell/{mod,tool_page}.rs` (route `Tool::Logs` to `LogsView` when the snapshot has data or the app is connected), `src/keymap.rs`

**Interfaces:**
- **Consumes:** Tasks 2, 12, 13 and 15.
- **Produces:**
  - **`tools::logs::model`** (pure, plain `#[test]`)
    - `LOG_ROW_MAX_CHARS = 512` and `MAX_INLINE_LINES = 200`.
    - **`RowMap`**
      - `RowMap::new(filtered: Arc<[u32]>)`.
      - `toggle(seq, extra_lines: u32)`.
      - `row_count() -> usize`.
      - `resolve(row) -> RowRef`, where `RowRef` is `Record { seq }`, `Continuation { seq, line: u32 }` or `ShowAll { seq }`.
      - `row_of(seq) -> Option<usize>`.
      - An expanded record contributes `1 + min(lines - 1, MAX_INLINE_LINES)` rows, plus one `ShowAll` row if it was cut.
      - `resolve` is O(log e) with e expanded records.
    - `display_line(text: &str) -> (&str, bool)`: the first line, cut to `LOG_ROW_MAX_CHARS` chars on a char boundary, and whether it was cut.
    - **`Follow`**
      - States: `following: bool` and `unseen: usize`.
      - `on_new_rows(n, at_end: Option<bool>)`. It returns whether to scroll to the bottom: true when `following && at_end != Some(false)`. Otherwise it adds to `unseen`.
      - `on_select()` pauses following.
      - `on_user_scrolled_up()` pauses following.
      - `resume()` sets following and clears `unseen`.
  - **`LogsView`** (entity)
    - It holds the `SessionModel`, a `LogFilter`, `filtered: Arc<[u32]>`, a `RowMap`, `Follow`, a `UniformListScrollHandle`, the selected seq and the job state.
    - **Recomputing the filter.** On a snapshot change it recomputes off the UI thread:
      - **Incremental:** when only new records arrived and the filter is unchanged, filter from the last seq and append.
      - **Full:** otherwise.
      - **Stale results:** a result is dropped if the generation or filter changed in the meantime.
    - **Row layout** (`row::LOG` = 22 px, `font::MONO`, `font::LINE_HEIGHT_LOG`, `font::DENSE`, tabular figures). Row id `log-row-{row}`.
      - Columns: time `format_offset` with `.mmm` (width `table::COL_TIME`); a level text badge `short()` (width `COL_LEVEL`) in the level colour (Debug `text_muted`, Info `text`, Warning `warning`, Error `danger`); the logger, or the source label when there is no logger (`COL_LOGGER`, `text_muted`); then the message from `display_line`.
      - A record with more lines gets a badge `id: "log-more-{seq}"` reading "+{n} lines". Clicking it toggles expansion.
      - A `Continuation` row is indented and `text_muted`.
      - A `ShowAll` row reads "Show all {n} lines in details", and clicking it selects the record.
    - **Toolbar** (`layout::TOOLBAR_HEIGHT`):
      - Level chips `id: "logs-level-{debug|info|warning|error}"`, each labelled with its count in the current range.
      - A source menu button `id: "logs-source"`: a popup with check items for each `LogSource`.
      - A logger menu button `id: "logs-logger"`: check items for the seen loggers, plus "All loggers".
      - A follow toggle `id: "logs-follow"` reading "Follow".
      - When paused with unseen rows, a pill `id: "logs-unseen"` reads "{n} new lines ↓" and clicking it resumes.
    - **Keys** (key context `"LogsList"`):

      | Keys | Action |
      |---|---|
      | `j` / `down` | `SelectNext` |
      | `k` / `up` | `SelectPrevious` |
      | `space` | `ToggleExpand` |
      | `right` | `Expand` |
      | `left` | `Collapse` |
      | `enter` | `OpenDetails` (Task 17 shows the pane) |
      | `end` | `FollowLatest` |
      | `home` | `GoToFirst` |

      The actions are defined in `actions.rs` and listed in the registry under group View with labels such as "Next Row", "Expand Record" and "Follow Latest Logs".
    - **Selection** syncs with `Location.selection = Some(Selection::Log(id))`, and navigating to such a location scrolls to the row and selects it.
    - **Range.** The shared range is part of the `LogFilter`.
  - **States:**
    - **Connected with no logs:** the M1 Logs empty copy, heading "No logs yet", body "Logs, print output and Flutter errors appear here as your app writes them."
    - **Filters exclude everything:** "No lines match your filters." with a button `id: "logs-clear-filters"` reading "Clear filters".
    - **Filtering a large change:** after `LOADING_DELAY`, a thin `Skeleton` sits at the toolbar's bottom edge.
- [ ] **Step 1: Write the failing pure tests.**
  - `row_map_without_expansion_is_identity`.
  - `expanding_inserts_continuation_rows`.
  - `expansion_is_capped_with_a_show_all_row` (a 10,000-line record → 200 continuation rows plus `ShowAll`).
  - `resolve_and_row_of_round_trip_with_many_expansions` (property-style over 1,000 random toggles, deterministic seed).
  - `display_line_cuts_on_a_char_boundary` (a 200 KB line of "é").
  - `follow_state_machine`.
- [ ] **Step 2: Write the failing view tests.**
  - `renders_only_visible_rows_at_one_million` (`SessionModel::fixed` with `synth::logs(1_000_000)`; count item-closure calls in one frame via a `#[cfg(test)]` counter: ≤ 40).
  - `new_lines_scroll_to_the_bottom_while_following`.
  - `selecting_pauses_follow_and_shows_unseen_count`.
  - `end_resumes_following`.
  - `space_expands_a_multi_line_record` (`log-row-…` count grows by `lines - 1`).
  - `error_chip_filters_to_errors`.
  - `range_filters_rows`.
  - `clear_filters_button`.
  - `selection_round_trips_through_location`.
  - `filter_work_runs_off_the_ui_thread`: the filter job is spawned with `background_spawn`; assert the view shows the old rows until `run_until_parked`.
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(logs): add the virtualised log list with follow, inline expansion and filters`.

### Task 17: Logs tool: search and the details pane

**Files:**
- Create: `crates/fdt_app/src/tools/logs/details.rs`
- Modify: `tools/logs/{view,toolbar,model}.rs`, `src/actions.rs`, `src/keymap.rs`

**Interfaces:**
- **Consumes:** Tasks 2 and 16.
- **Produces:**
  - **The search field.** An `Input` with `id: "logs-search"` on its wrapper, placeholder "Search logs". Next to it a regex toggle `id: "logs-regex"` labelled ".*", with tooltip "Regular expression".
    - **`FocusSearch` (⌘F, context `Shell`).** It focuses the current tool's search field and is registered as "Search in Current Tool".
    - **Each change of query** (deduped by value, U4):
      1. Run `compile_query`.
      2. On `Err`, show `id: "logs-search-error"` with the message in `danger` and keep the previous rows.
      3. On `Ok`, set the cancel flag of the running job, start `filter_logs_two_phase` on the background executor, and show the first-phase rows, then the full result.
    - **While a search runs,** a "Searching…" label (`id: "logs-searching"`) appears after `LOADING_DELAY`. The match count `id: "logs-match-count"` reads "{n} matches".
    - `LogsView::set_search(query: &str, regex: bool, window, cx)`, used by the palette in Task 20.
    - **Highlights.** `match_ranges` is computed only for rendered rows, then clipped and shifted onto the displayed (possibly cut) line and applied with `StyledText::with_highlights` (U5), using background `accent` at `alpha::SELECTION`.
  - **The details pane** (`id: "logs-details"`; a resizable right panel, U6, default `layout::DETAILS_WIDTH`, range `DETAILS_MIN..DETAILS_MAX`).
    - It opens on Enter, a double-click or `OpenDetails`, and on selection once it is open. Esc closes it before clearing the selection.
    - **Metadata:** time, level, source, logger and run ("Run 2"), plus "Before fdt connected" when `clamped`.
    - **Message:** the full text in a virtualised mono list of lines (`uniform_list`, 22 px), so a 10,000-line record stays O(visible).
    - **Flutter errors:** sections for Summary, "Caused by widget" (widget plus location), and "Stack trace" (one row per frame).
    - **Buttons:** `id: "logs-copy-text"` ("Copy") and `id: "logs-copy-json"` ("Copy as JSON"). The JSON is `{"time":"0:12.345","level":"Error","source":"Flutter error","logger":"","run":1,"message":"…"}`.
  - **Row context menu** (U11): "Copy" and "Copy as JSON".
- [ ] **Step 1: Write the failing tests.**
  - `cmd_f_focuses_search`.
  - `typing_filters_rows_and_counts_matches` (`"burst"` over a fixed snapshot).
  - `uppercase_makes_search_case_sensitive`.
  - `regex_toggle_enables_patterns`.
  - `invalid_regex_shows_error_and_keeps_rows`.
  - `typing_quickly_cancels_the_previous_scan`: the model keeps the last job's `Arc<AtomicBool>`; after two quick `set_search` calls the first flag is true.
  - `highlights_cover_matches_in_cut_lines` (pure: a match past char 512 gives no highlight, and one inside does).
  - `details_show_full_text_and_metadata`.
  - `details_show_flutter_error_sections`.
  - `copy_and_copy_as_json_write_the_clipboard`.
  - `context_menu_copies`.
  - `escape_closes_details_first`.
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
- [ ] **Step 3: Commit** `feat(logs): add streaming search with highlights and a details pane`.

### Task 18: Network tool: the requests table, tabs and filters

**Files:**
- Create: `crates/fdt_app/src/tools/network/{mod,model,table,toolbar}.rs`
- Modify: `src/shell/{mod,tool_page}.rs`, `src/keymap.rs` (`j`/`k` → `gpui_kit::base::actions::{SelectDown, SelectUp}` in context `"DataTable"`), `src/actions.rs`

**Interfaces:**
- **Consumes:** Tasks 3, 13, 15 and 16 (the shared patterns).
- **Produces:**
  - **`tools::network::model`** (pure)
    - `format_bytes(u64) -> String`: "0 B", "999 B", "1.0 KB", "1.4 MB", "2.0 GB" (base 1024, one decimal from KB).
    - `format_duration_ns(u64) -> String`: "850 µs", "182 ms", "1.4 s", "1:05" (≥ 60 s).
    - `status_text(&HttpRecord) -> String`: "200", "Failed", "Pending" or "101".
    - `waterfall(record, span: TimeRange, now: SessionTime) -> (f32, f32)`: x and width fractions. Pending requests extend to `now`, and the width is at least 1 px-equivalent (0.002).
  - **`NetworkView`** (entity)
    - **Tabs** (U7, `TabBar` `id: "net-tabs"`): "Requests", "WebSockets" and "Errors {n}". The error count is shown only when non-zero.
    - **Table** (`DataTable`, U1, `with_size(Size::Size(row::TABLE))`, `loop_selection(false)`).
    - **Requests and Errors columns:**
      - Method (`COL_METHOD`)
      - Host (`COL_HOST`, `text_muted`)
      - Path (flex)
      - Status (`COL_STATUS`, coloured `success` 2xx, `text_muted` 1xx/3xx, `warning` 4xx, `danger` 5xx/Failed, plus a spinner icon for Pending)
      - Duration (`COL_DURATION`, right-aligned)
      - Size (`COL_SIZE`, right-aligned)
      - Waterfall (`layout::WATERFALL_WIDTH`, a bar in the status colour)
    - **WebSockets columns:**
      - URL
      - State ("Open" / "Closed", from the linked socket's `end`, else "Unknown")
      - Sent and Received (from `write_bytes` / `read_bytes` via `format_bytes`)
      - Last activity (`format_offset`)
    - **Toolbar:**
      - A filter `Input` (`id: "net-filter"`, placeholder "Filter by URL"). ⌘F focuses it.
      - Method chips `id: "net-method-{get|post|put|patch|delete|other}"`.
      - Status chips `id: "net-status-{2xx|3xx|4xx|5xx|failed|pending}"`.
      - A host menu `id: "net-host"` (popup with "All hosts" plus each host).
    - **Filtering.** `filter_requests` runs on the background executor for every filter, tab, range or snapshot change. Results are stale-checked like the Logs filter.
    - **Selection.** `TableEvent::SelectRow` selects and sets `Location.selection = Selection::Request(id)`; navigating to such a location selects and scrolls (`set_selected_row`).
    - **Following.** New requests do not move the selection. When nothing is selected and the table is at the bottom, it stays at the bottom.
    - **Row context menu** (`TableDelegate::context_menu`): "Copy URL" and "Copy as JSON" (`{"method","url","status","duration_ms","size","start":"m:ss.mmm"}`).
  - **Empty states:**
    - **Requests:** "No requests yet. Requests appear as your app makes them. Requests sent before fdt connected aren't captured."
    - **WebSockets:** "No WebSockets yet. fdt shows each connection's handshake and traffic volume; message contents aren't available to debugging tools."
    - **Errors:** "No failed requests."
    - **Filters exclude everything:** "No requests match your filters." with `id: "net-clear-filters"`.
- [ ] **Step 1: Write the failing pure tests.**
  - `format_bytes_cases`.
  - `format_duration_cases`.
  - `status_text_cases`.
  - `waterfall_positions_and_pending_extends_to_now`.
- [ ] **Step 2: Write the failing view tests.**
  - `renders_only_visible_rows_at_100k` (count `render_td` calls: ≤ 100 per frame).
  - `filter_text_narrows_rows`.
  - `status_chips_filter`.
  - `errors_tab_shows_only_errors_and_its_count`.
  - `websockets_tab_columns`.
  - `j_and_k_move_the_selection`.
  - `selection_round_trips_through_location`.
  - `range_filters_requests`.
  - `copy_url_from_context_menu`.
  - `empty_states_per_tab`.
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(network): add the requests table with tabs, filters and waterfall`.

### Task 19: Network tool: the request details pane

**Files:**
- Create: `crates/fdt_app/src/tools/network/{details,body}.rs`
- Modify: `tools/network/view.rs`, `src/actions.rs` (`LoadFullBody`)

**Interfaces:**
- **Consumes:** Tasks 3, 10 and 18.
- **Produces:**
  - **`body::BodyKindGuess`**: `Json`, `Form`, `Text`, `Image(ImageFormat)` or `Binary`.
    - `guess(content_type: Option<&str>, bytes: &[u8]) -> BodyKindGuess`:
      - `application/json`, `+json` → Json
      - `application/x-www-form-urlencoded` → Form
      - `text/*`, `application/xml`, `application/javascript` → Text
      - `image/png|jpeg|gif|webp` → Image
      - anything else is sniffed: valid UTF-8 without NUL in the first 4 KiB → Text, otherwise Binary
  - **`body::render_lines(kind, bytes) -> RenderedBody { lines: Vec<String>, note: Option<String> }`**. It runs on the background executor.
    - **Json** is pretty-printed with `serde_json::to_string_pretty`. Invalid JSON falls back to Text with the note "Not valid JSON. Shown as text."
    - **Form** gives `name = value` lines, percent-decoded.
    - **Text** with invalid UTF-8 is decoded lossily, with the note "Contains bytes that aren't valid UTF-8."
    - **Binary** gives a hex dump of the first 4 KiB, 16 bytes per line (`00000000  89 50 4e 47 …  .PNG…`), with the note "Binary data, {format_bytes(len)}. Showing the first 4 KB."
  - **The details pane** (`id: "net-details"`; resizable like Logs).
    - **Header:** the method, the full URL (`id: "net-details-url"`), the status text and the duration.
    - **Sections**, each collapsible with ids `net-sec-{overview|request-headers|response-headers|request-body|response-body|timing|error}`:
      - **Overview:** start, duration, size and run, plus "Started before fdt connected" when `before_capture`.
      - **Headers:** sorted, multi-value headers joined with ", ".
      - **Timing:** each event with its offset from the start (`+182 ms`) and a total.
      - **Error:** shown when present.
    - **Body section by state:**
      - `Pending`: a `Skeleton` after `LOADING_DELAY`.
      - `NotCaptured`: "No body."
      - `Captured`: a virtualised list of the rendered lines. Images use the GPUI `img` element with `Arc<gpui_kit::Image>` built from the bytes and format, scaled to fit the pane width (max height 320 px via a `layout` token `IMAGE_PREVIEW_MAX = 320`).
      - **Truncated:** a notice `id: "net-body-truncated"` reads "Showing the first {cap} of {full}." with a `LoadFullBody` button (`id: "net-load-full"`).
      - `TooLarge { len }`: "This body wasn't copied automatically ({format_bytes(len)})." with the same button.
      - `Evicted { len }`: "This body was removed to stay within the {budget} body budget."
      - `Unavailable { reason }`: the reason.
      - **Disconnected:** the button is disabled, with the tooltip "Reconnect to the app to load the rest."
    - **Copy buttons:** "Copy URL", "Copy body" (the full stored bytes as UTF-8, lossy) and "Copy headers".
- [ ] **Step 1: Write the failing pure tests.**
  - `guess_cases` (including JSON served as `text/plain` → Text, and a PNG with no content type → Binary by sniff).
  - `json_is_pretty_printed`.
  - `invalid_json_falls_back_with_a_note`.
  - `form_is_decoded`.
  - `invalid_utf8_is_lossy_with_a_note`.
  - `binary_hex_dump_format` (first line exact for `[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]`).
  - `a_50_mb_json_body_renders_in_bounded_time`. This is a release `budget` test: rendering is applied to the ≤ 1 MiB stored bytes, and the test asserts that only those are rendered.
- [ ] **Step 2: Write the failing view tests.**
  - `details_open_on_selection_with_sections`.
  - `pending_body_shows_skeleton_after_delay`.
  - `truncated_body_offers_load_full_body_and_sends_the_command`.
  - `too_large_and_evicted_messages`.
  - `load_full_body_disabled_when_disconnected`.
  - `copy_buttons_write_the_clipboard`.
  - `image_body_renders_an_image_element` (`id: "net-body-image"`).
- [ ] **Step 3: Implement, then verify.**
  - Run: `cargo test -p fdt_app` and `cargo test --workspace --release -- budget`
  - Expected: PASS.
- [ ] **Step 4: Commit** `feat(network): add request details with headers, timing and body previews`.

### Task 20: Palette with parameterised items on a virtualised list

**Files:**
- Create: `crates/fdt_app/src/palette/items.rs`
- Modify: `src/palette/{view,registry,mod}.rs`, `src/shell/mod.rs`, `src/actions.rs` (`SearchEverywhere`), `src/keymap.rs`

**Interfaces:**
- **Consumes:** Tasks 12, 13, 16, 17 and 18. M1's `rank()` and `record_recent()` are unchanged.
- **Produces:**
  - **`palette::items`**
    - `PaletteItem { id: String, label: String, detail: Option<String>, keywords: Vec<String>, group: Group, kind: ItemKind }`, where `Group` gains `Data`.
    - `ItemKind`: `Action(fn() -> Box<dyn Action>)`, `Navigate(Location)` or `SearchLogs(String)`.
    - `command_items() -> Vec<PaletteItem>` maps M1's `commands()`.
    - `request_items(snap: &Snapshot, limit: usize /* 2,000 */) -> Vec<PaletteItem>`. It keeps the latest request per `(method, path)`, newest first, with label "GET /v2/products?page=3", detail "{host} · {status_text}", group Data, and `Navigate(Location { page: Network, selection: Request(id), range: None })`.
    - `search_logs_item(query) -> PaletteItem` (label "Search logs for “{query}”").
  - **`PaletteModel`**
    - `PaletteModel::new(items: Vec<PaletteItem>, recent_ids)`, `set_query`, `ranked()` and `item(ix)`. Ranking runs over `label + keywords + detail`.
    - When the query is non-empty, the `SearchLogs` item is appended **after** the ranked results. It is not ranked.
    - The ranked list is computed in `set_query`, synchronously, under the existing 8 ms / 5,000-item budget.
  - **The view.** It is still opened with `window.open_dialog` (M1 fact), but the content is our own:
    - an `Input` (`id: "palette-input"` on its wrapper)
    - a `uniform_list` of rows `id: "palette-row-{i}"`, `row::TABLE` high, showing the label, a `text_muted` detail and the shortcut hint for `Action` items only
    - key context `"Palette"`:

      | Keys | Action |
      |---|---|
      | `up` / `down` | move |
      | `enter` | run |
      | `escape` | close |
      | `cmd-k` / `ctrl-k` | close (existing) |

    - Running an item closes the dialog, focuses the shell, then:
      - `Action`: dispatch the action.
      - `Navigate`: call `MainWindow::navigate` through a `WeakEntity<MainWindow>`.
      - `SearchLogs`: navigate to Logs and call `LogsView::set_search(q, false, …)`.
    - Commands record recents as before.
  - **`SearchEverywhere`** (⌘⇧F / Ctrl+Shift+F, context `Shell`) opens the palette. It is registered as "Search Everywhere".
- [ ] **Step 1: Update the M1 palette tests to the new view and confirm they fail.** Run `cargo test -p fdt_app palette`. Then write the new failing tests:
  - `only_visible_rows_render_at_5000_items` (≤ 30 row builds per frame).
  - `typing_a_path_finds_the_request_and_enter_navigates_to_it`.
  - `search_logs_item_appears_last_and_runs_a_log_search`.
  - `cmd_shift_f_opens_the_palette`.
  - `recent_commands_rank_first` (kept from M1).
  - `question_mark_types_into_the_palette` (kept).
  - `cmd_k_closes` (kept).
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_app` and `cargo test -p fdt_app --release -- budget`
  - Expected: PASS. The M1 ranking budget test is still green.
- [ ] **Step 3: Commit** `feat(palette): rebuild the palette on parameterised items and a virtualised list`.

### Task 21: `fdt bench-ui` and the stress-dataset scenarios

**Files:**
- Create: `crates/fdt_app/src/bench_ui.rs`
- Modify: `src/main.rs`, `src/app.rs`, `crates/fdt_store/src/synth.rs` (`stress_session(seed) -> Arc<Snapshot>`), `crates/fdt_app/tests/cli.rs`

**Interfaces:**
- **Consumes:** Tasks 2–4 and 13–20.
- **Produces:**
  - **`synth::stress_session(seed: u64) -> Arc<Snapshot>`**: one run, 1,000,000 logs, 100,000 requests and 72,000 frames at 120 Hz.
  - **CLI.** `fdt bench-ui <scenario> [--frames N]`:
    - It builds the stress session first (not timed), opens the main window with `SessionModel::fixed(snapshot, ConnStatus::Connected { … })`, runs the scenario, prints one JSON line to stdout and exits.
    - **Exit codes:** 0 when every budget passes, 1 otherwise, 2 for a usage error.
  - **Scenarios.** One step per frame, driven by chained `window.on_next_frame` plus `window.refresh()`:

    | Scenario | Script |
    |---|---|
    | `logs-scroll` | `scroll_to_item(i, Top)`, `i += 3` per frame, 600 frames |
    | `logs-search` | type "connection timeout" one character every 5 frames; record per keystroke the frames until the first-phase rows are drawn and until the full result is drawn |
    | `net-scroll` | `scroll_to_row(i)`, `i += 2` per frame, 600 frames |
    | `net-filter` | type "products" one character every 5 frames; record per keystroke the ms until results are drawn |
    | `tool-switch` | alternate `ShowLogs` / `ShowNetwork` 40 times; record the ms from dispatch to the end of the frame that drew the new page |
    | `strip-drag` | move a simulated drag across the strip, 300 frames |

  - **Measurement.** Draw durations come from `FrameTimingCollector` (`draw_end - draw_start`). Frame intervals are the deltas between consecutive `draw_start` values while the scenario runs.
  - **Output:** `{"scenario":"logs-scroll","frames":600,"draw_ms":{"p50":…,"p95":…,"p99":…,"max":…},"interval_ms":{…},"latency_ms":{…},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.3,"latency_p99_ms":<16 for searches and filters, 50 for tool-switch>},"pass":true}`.
    - `latency_ms` is present only for `logs-search`, `net-filter` and `tool-switch`.
    - `interval_ms` is checked against 8.3 ms only when the display refresh interval is ≤ 8.4 ms; otherwise against 16.7 ms. Record which one was used as `"interval_budget_ms"`.
- [ ] **Step 1: Write the failing tests.**
  - `cli.rs`: `bench-ui` without a scenario prints usage and exits 2.
  - `bench_ui::percentiles` unit tests (reuse `fdt_ui::hud::stats` if it fits, otherwise a small sorted-percentile function with tests).
  - `scenario_scripts_complete_headless` (`#[gpui_kit::test]` with a small synth session, 100 logs and 100 requests, and `--frames 10` equivalents): every scenario reaches its end state, for example the last row index reached and every keystroke recorded.
- [ ] **Step 2: Implement, then verify.**
  - Run: `cargo test -p fdt_app`
  - Expected: PASS.
  - Then `cargo build --release -p fdt_app` and, with the screen unlocked, `./target/release/fdt bench-ui logs-scroll`. Paste the JSON into the report.
- [ ] **Step 3: Commit** `feat(app): add the bench-ui harness and stress scenarios`.

### Task 22: Live `jank_lab` integration test and CI job

**Files:**
- Create: `crates/fdt_capture/tests/jank_lab_live.rs`, `scripts/jank-lab-it.sh`
- Modify: `.github/workflows/ci.yml`, `README.md` (how to run it locally)

**Interfaces:**
- **Consumes:** Tasks 6, 8 and 9–11.
- **Produces:**
  - **`jank_lab_live.rs`.** It is `#[ignore]`d unless `FDT_JANK_LAB=1`, and reads the app's VM URI from `FDT_JANK_LAB_VM_URI`.
    - **Discovery:** `Discovery` (real registry directory) lists an app named "jank_lab" within 5 s.
    - **Capture:** connect with `Target::Discovered`, then drive the actions with `call_extension("ext.jank_lab.run", …)` through a separate `VmService`:
      1. `goto_logs`, `emit_levels`, `print_burst`, `multi_line`, `report_error`
      2. `pop`, `goto_network`, `load_products`, `place_order`, `refused`, `open_socket`
    - **Assertions within 10 s:**
      - all four log levels are present
      - 1,000 burst lines
      - the full 30-line record
      - a `FlutterError` record, debug only
      - GET 200 with its body
      - POST 500 with request body `{"cart":1}`
      - a failed `refused` request in the Errors filter
      - a websocket request linked to a socket
    - **Lifecycle:** hot restart through `flutter run`'s stdin is too indirect for CI. Instead call `ext.jank_lab.run` `load_products` after `s1.hotRestart` (if registered), and assert the request is captured in the same run.
    - **Timings:** print `attach_ms` (from `connect()` to the first snapshot with `Connected`) and `first_log_ms` (from `emit_levels` to the record appearing).
  - **`scripts/jank-lab-it.sh [debug|profile]`.**
    1. Run `flutter pub get`.
    2. Start `flutter run -d <macos|linux> --machine` in the background (with `--profile` for profile).
    3. Wait for `app.debugPort` (60 s timeout).
    4. Export `FDT_JANK_LAB=1` and `FDT_JANK_LAB_VM_URI`.
    5. Run `cargo test -p fdt_capture --test jank_lab_live -- --ignored --nocapture`.
    6. Stop the app (`app.stop`, then kill on timeout) and exit with the test's status.
  - **CI job `jank_lab_live`** (ubuntu-24.04, Flutter 3.50.0):
    - Install Linux desktop dependencies (`clang cmake ninja-build pkg-config libgtk-3-dev`) plus the Rust dependencies from the `rust` job.
    - Run `xvfb-run -a scripts/jank-lab-it.sh debug`, then `profile`.
- [ ] **Step 1: Write the test and the script, then run locally on macOS.**
  - Run: `scripts/jank-lab-it.sh debug` and `scripts/jank-lab-it.sh profile`.
  - Expected: PASS, with `attach_ms` and `first_log_ms` printed. Paste both runs' output into the report.
- [ ] **Step 2: Add the CI job.** It cannot run until the repository has a GitHub remote; the report says so.
- [ ] **Step 3: Commit** `test(capture): add the live jank_lab integration test and CI job`.

### Task 23: M2 verification and review record

**Files:**
- Create: `docs/milestones/m2-review.md`
- Modify: `docs/milestones/m1-deferred-findings.md` (mark the carried items done, with the M2 commit that did each)

**Interfaces:**
- **Consumes:** everything.
- **Produces:** the record of measured budgets, the author's checklist and "Carry into M3".

- [ ] **Step 1: Run the full suite.**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `cargo test --workspace --release -- budget --nocapture`
  - `cargo deny check licenses`
  - `cd fixtures/jank_lab && flutter analyze && flutter test`

  Paste the summary lines.
- [ ] **Step 2: Run bench-ui.** Run every scenario 3 times on R1 (screen unlocked, other apps closed). Record the medians of p99 against the budgets in a table, and note the system load (`uptime`).
- [ ] **Step 3: Measure attach, idle use and overhead.**
  - **Attach:** run `scripts/jank-lab-it.sh debug` three times. Record `attach_ms` and `first_log_ms`; the budget is ≤ 1 s from connect to the first data on screen, and one frame is added for the UI.
  - **Idle while connected:**
    - Launch `fdt`, connect to `jank_lab` on its home screen (no animation), and wait 30 s.
    - Sample `ps -o rss=,%cpu= -p <pid>` every 2 s for 60 s.
    - Record max RSS (≤ 150 MB) and every CPU sample (≤ 1 %).
  - **Overhead on the profiled app:**
    - Run `jank_lab` in profile mode on the Jank screen with its animation running.
    - Collect 600 `Flutter.Frame` `build` values first with a bare listener (a `VmService` subscribed to `Extension` only), then with `fdt` connected. Run them in alternating order, twice.
    - Record the median build-time difference (≤ 2 %).
- [ ] **Step 4: Write `docs/milestones/m2-review.md`.** Use the same sections as `m1-review.md`:
  - **Summary and status.**
  - **Measurements:** tables with run counts and conditions.
  - **Calls made and their outcomes.**
  - **Author review checklist:**
    - visual review in both themes on all three OSes of the start page, title-bar states, banner, strip with range, Logs (list, expanded, search with highlights, details, Flutter error) and Network (each tab, details for JSON, image, binary, truncated and too-large bodies)
    - keyboard-only pass
    - discovery on Windows and Linux (registry directory per fact P1)
    - a hot restart and a full restart of a real app
  - **Pending verification:** the first CI run, and Windows/Linux discovery paths that have only been read from source.
  - **Carry into M3:** for example refresh-rate detection for the strip budget line, cross-links from the stored ids, and whatever the measurements show.
- [ ] **Step 5: Commit** `docs: add M2 verification results and review checklist`.
