# M2 verification results and review checklist

Branch `m2-connect-logs-network`, verified on the commit that adds `fdt --connect` (`1c669b6`) plus the measurement examples. R1: Apple M3 Max, macOS, release build.

Legend: met, missed, not measured (and why).

## Summary and status

M2 is built: the `fdt_store`, `fdt_vm` and `fdt_capture` crates, the start page with discovery and URL connect, connection status, the session strip with a shared time range, the Logs and Network tools, the rebuilt palette, `fdt bench-ui`, and the live test against `jank_lab`. All automated checks are green. The live attach, the stress budget tests and the overhead on the profiled app meet their budgets.

Two kinds of measurement could **not** be taken in this verification run, because the Mac's screen was locked for the whole session (`CGSSessionScreenIsLocked = true`, checked at the start and again at the end). A locked session presents no frames to a GPUI window:

- **`fdt bench-ui`**: every scenario times out with no frames (`{"scenario":"logs-scroll","error":"timed out","pass":false}` after 120 s). No draw p99, frame p99 or latency numbers were produced here.
- **Idle memory and CPU of the real window while connected**: `fdt --connect` opens its window, but the capture host is installed from a next-frame callback, which never fires without frames, so it never connected (no TCP connection to the VM was open). The numbers below for idle use are for the capture alone, without a window.

The author needs to re-run those two on an unlocked R1 (commands under "Pending verification"). The last bench-ui numbers I have are the ones the Task 21b implementer took on an unlocked, loaded machine; they are shown separately and marked as such.

Not-met budgets known from those earlier runs: Logs search and tool switch draw p99 (see "Open decisions").

## Automated checks (all green)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass, no warnings |
| `cargo test --workspace` | 478 passed, 0 failed, 7 ignored (including the live `jank_lab` test, which needs a running app) |
| `cargo test --workspace --release -- budget --nocapture` | 12 passed (7 in `fdt_app`, 5 in `fdt_store`) |
| `cargo deny check licenses` | licenses ok |
| `flutter analyze` (fixtures/jank_lab) | No issues found |
| `flutter test` (fixtures/jank_lab) | 18 passed |
| `scripts/jank-lab-it.sh debug` x3 | 3 of 3 passed, hot restart ok each time |

## Measured budgets

Load: `uptime` before each batch is recorded. The machine was shared and busy (Chrome, Android Studio and its Dart processes; 1-minute load average 124.38 at 19:05 when the first idle attempt started, 88.33 at 19:06; later batches 4.3 to 8.2, listed per batch below). The overhead numbers have a noise of about 1 % run to run (see the spread between the two bare runs), so only differences larger than that mean anything.

### Release budget tests (one run, load not recorded for this batch)

| Measure | Budget | Measured | Verdict |
|---|---|---|---|
| Log search, first results over 1M lines (4 queries) | <= 16 ms | p90 0.2 to 3.8 ms, worst 5.3 ms | met |
| Log search, full scan over 1M lines | <= 100 ms | p90 2.0 to 49.2 ms, worst 50.5 ms | met |
| Network filter over 100k requests (4 filters) | <= 16 ms | p90 0.29 to 0.92 ms, worst 1.37 ms | met |
| Palette results per keystroke, 5,000 items | <= 8 ms | worst 1.79 ms | met |
| `strip_bars`, 72k frames at width 1000 | (internal) | worst 0.0064 ms | met |
| Pretty-print of 1 MiB JSON (off the UI thread) | (internal) | 20.5 ms | met |

### Attach: `scripts/jank-lab-it.sh debug`, 3 runs, 1-minute load 4.48, 4.64, 4.26

| Run | attach_ms | attach_to_data_ms | first_log_ms |
|---|---|---|---|
| 1 | 11 | 124 | 106 |
| 2 | 10 | 122 | 106 |
| 3 | 10 | 125 | 108 |

`attach_ms` is connect until the status is Connected. `attach_to_data_ms` is connect until the first live log record is in a published snapshot. Budget: attach to first live data on screen <= 1 s. Snapshots publish at most every 16 ms and the UI draws on the next frame, so add about one frame (8.3 to 16.7 ms): about 140 ms. **Met**, with a wide margin. The UI part (a frame) was not timed in a window; it is an estimate. These runs had a healthy app. If the main isolate is paused, the clock calibration now waits at most 1.5 s before its fallback, and the whole connect sequence at most 5 s. A profile-mode pass during Task 22 gave attach_ms 5, attach_to_data_ms 110, first_log_ms 103 (one run; not repeated here).

### Idle memory and CPU, connected

**Real window: not measured** (screen locked, see the summary). Two attempts: `fdt --connect` with the app on its home screen, 30 s wait, 30 samples 2 s apart. All 30 samples were 0.0 % CPU, with RSS 56,128 to 56,288 KB for the first 16 and 54,336 KB for the last 14 (about 54 to 56 MB), but it was not connected (no socket to the VM), so these numbers describe an idle, unconnected window and do not count.

**Capture alone, no window (a lower bound, not the budget measurement).** `attach_hold` example (the real `Capture` connected to `jank_lab` in debug mode on its home screen), 30 s warm-up, then 30 samples 2 s apart, 1-minute load 7.11 before and 4.88 after. The sampled PID was the built `attach_hold` binary itself (started directly, not through `cargo run`):

- RSS: 3,168 KB at the first sample, 3,216 KB for the other 29. Maximum 3.2 MB.
- CPU: 0.0 on 24 samples, 0.1 on 6. Maximum 0.1 %.

The window adds its own cost (about 55 MB unconnected, from the idle runs above, plus snapshot rendering). The 150 MB and 1 % budgets are **not verified** for the whole app.

### Overhead on the profiled app (<= 2 % added to frame build time)

`jank_lab` in profile mode on the Jank screen (`--profile --dart-define=SCENARIO=jank`; its `HeavyBuildPanel` burns 24 ms in `build` on each animation frame). `frame_build_sampler` collects 600 `Flutter.Frame` `build` values from a second VM client subscribed to the `Extension` stream only, counting only frames that started after it subscribed (the VM replays older events on subscribe; the first version of the sampler counted those and gave four identical runs, so it was fixed). "Attached" means the real `Capture` (the same code `fdt` uses) is connected as a third client; the window was not involved because it could not connect (locked screen), and the app only sees the capture's connection. Order: bare, attached, bare, attached. 1-minute load 8.16 before the first pair and 7.82 before the second.

| Run | Median build (us) | Mean (us) | p90 (us) | Frames per second observed |
|---|---|---|---|---|
| bare 1 | 24,497 | 25,107 | 25,778 | 35.1 |
| attached 1 | 24,784 | 25,445 | 26,546 | 33.0 |
| bare 2 | 24,770 | 25,285 | 26,468 | 33.3 |
| attached 2 | 24,730 | 25,490 | 26,282 | 33.1 |

Mean of medians: bare 24,634 us, attached 24,757 us. Difference +123 us, **+0.5 %**. The two bare runs differ from each other by 273 us (1.1 %), so the difference is inside the noise. Budget <= 2 %: **met** (for the capture; a window cannot change what the app sees). Only this one app and screen were tried; an app with many requests per second was not.

### `fdt bench-ui`: not measured here

Required: every scenario 3 times, medians against budgets. **Not run**, because the screen was locked (see the summary). The table shows what the Task 21b implementer recorded on this branch on an unlocked but loaded machine (load 5 to 70 because of an ffmpeg job), draw p99 medians. These are not part of this verification.

| Scenario | Draw p99 before Task 21b (ms) | After (ms) | Budget | State after |
|---|---|---|---|---|
| logs-scroll | 11.8 | 3.0 | 4 | met |
| logs-search | 7.0 | 6.5 | 4 | **missed** (latency: first results within 2 frames, full result within 12: met) |
| net-scroll | 4.0 | 3.4 | 4 | met |
| net-filter | 3.7 | 3.7 | 4 | met |
| tool-switch | 6.8 | 6.9 | 4 | **missed** (switch latency within budget) |
| strip-drag | 3.8 | 3.4 | 4 | met |

The scroll interval budget is `max(plan budget, refresh period) + 0.5 ms` (ruling R16), printed with the report.

## Calls made and their outcomes

1. **A new crate, `fdt_capture`** (not in the spec). Kept. It holds the connection lifecycle and ingest, and made headless integration tests possible: the live `jank_lab` test and the replay tests need no window.
2. **Stress datasets generated in code** (`fdt_store::synth`, fixed seed). Kept. 1M log lines and 100k requests are produced in a few seconds; real `jank_lab` recordings are committed.
3. **Session time axis is the host clock**, with a per-run calibration at connect. Kept; the calibration is unit-tested by hand-checked numbers, and a hot restart in the live test stays in one run.
4. **Discovery polls the DTD registry folder once a second**, one DTD connection per entry. Kept. Found `jank_lab` within 5 s in the live test. Only read from source on Windows and Linux.
5. **Bodies over 4 MiB are not copied automatically; "Load full body" fetches them.** Kept, with changes: a request is not fetched when either side's known length is over 4 MiB (one fetch returns both bodies, so each over-limit side is marked too large, and a known in-limit side is offered through "Load full body"). Bodies of unknown length are still auto-fetched with a 10 s bound (R11), but the reply is decoded from text under a 4 MiB cap and anything longer is marked too large with its real length. "Load full body" is disabled for bodies known to be over 64 MiB, the most one 256 MiB frame can carry as the VM's integer array (four characters a byte).
5a. **The connect sequence is bounded.** Everything up to the start of the run gives up after 5 s with a plain message, and the clock calibration's call into the app waits 1.5 s before the fallback, so a paused app or a server that is not a VM service no longer holds the window for the 30 s call default. Isolates already running at connect that expose the HTTP profile are polled, not only the main one.
6. **Multi-line log records expand inline as 22 px rows.** Kept; the pathological record tests (one 200 KB line, 10,000 lines) pass.
7. **The `flutter: ` print prefix is removed from Stdout lines.** Kept.
8. **Connecting lands on Logs.** Kept; navigation happens on the transition to connected only, not on every status event (fixed in review).
9. **Unbuilt tools show "not available yet" while connected.** Kept.
10. **Search everywhere (cmd-shift-F) opens the palette** with request and "Search logs for" items. Kept; the palette builds its data items on the background executor (R15).

### Rulings that changed the plan

- R1: requests and sockets are keyed `(run, isolate, vm_id)`, because a hot restart keeps the run and the new isolate's profile ids can repeat.
- R2: windows open detached and attach a capture when the capture host exists; the start page is created lazily.
- R3: a request's end time is the response end time, with fallbacks for failed requests.
- R4: clearing the app's HTTP profile waits until no request or body fetch is in flight.
- R5: discovery publishes after its first scan even when empty; the start page shows "Looking for running apps" for up to 2 s.
- R6: tests use an empty temporary registry directory; none reads the real DTD registry.
- R7: Task 18 creates the Network `view.rs`.
- R8: a truncated JSON body shows as text without the "not valid JSON" note.
- R9: `match_ranges("Ünïcode", "ünï")` is 0..5 bytes, not the plan's 0..6 (arithmetic error in the plan).
- R10: the store gets `store_full_body`, which bypasses the per-body cap and counts toward the total.
- R11: bodies of unknown length are auto-fetched, bounded by a 10 s timeout.
- R12: loading the full body of a record from a previous run leaves the stored body unchanged; the UI disables the button.
- R13: a finished Logs filter job is dropped only when the filter changed, not when the snapshot moved on.
- R14: keyboard row movement replaces the history entry instead of pushing.
- R15: palette data items are built on the background executor.
- R16: the scroll interval budget is the larger of the plan's and one refresh period, plus 0.5 ms.
- R17: keystroke latency budgets are judged in frames after the keystroke's frame.
- R18: the logs-search scenario types the next character after 5 frames or once the previous full result is drawn.
- R19: an added draw-time pass (Task 21b) because bench-ui showed misses against the 4 ms draw budget; it cached static chrome and fixed benchmark artefacts.
- R20: `fdt --connect <vm-service-url>` was added so a connected window can be started without clicks. A bad URL exits 2 with the parse message. It connects once the first window's capture is attached, which needs the window to draw frames (the capture host installs from a next-frame callback), so a locked or occluded session does not connect until frames resume. It does not save the URL in recent URLs.

## Open decisions for the author

1. **Draw p99 for Logs search and tool switch is about 6.5 to 7 ms against 4 ms.** The cost is reshaping the full page of text each frame (CoreText and layout). Meeting it needs visible changes: fewer text runs per row, or no ligatures. The budget was not loosened. Decide whether to change the design or accept 8 ms for these two interactions.
2. **The first CI run.** There is no GitHub remote yet, so the workflow (`ci.yml`, including the live `jank_lab` job) has never run.
3. **Recent URLs keep the VM token in `settings.json`.** The token is needed to reconnect. Decide whether to store it, or only the host and port and ask again.
4. **Bodies of unknown length are auto-fetched with a 10 s bound (R11).** An app that serves huge chunked bodies makes the VM send a JSON frame about 3.6 times the body size per request before the bound applies. The reply is no longer built into a JSON tree, and a body over 4 MiB is dropped while it is decoded, but the frame still crosses the socket and is held as text (up to the 256 MiB frame limit). Skipping unknown-length bodies would leave most real responses empty.
7. **New session (⌘N) discards the stored session with no confirmation or undo, and fires inside text inputs.** A product decision: ask first, offer undo, or only fire outside inputs. Navigation history, range and selection now reset with it.
8. **`fdt bench-ui` uses a fixed snapshot.** It cannot see the cost of the capture publishing a new snapshot while the window draws (about 60 Hz while capturing), so its draw and scroll numbers are for a quiet session. Decide whether a live-publishing scenario is needed.
5. **Windows and Linux discovery and paths** (registry directory, process-alive checks, ctrl shortcuts) are only read from source; none of it has been compiled or run there.
6. **`fdt --connect` skips recent URLs.** Choose whether a command-line connect should be remembered like a pasted one (it would also write the token).

## Author review checklist

Save screenshots next to this file as `m2-<os>-<theme>-<view>.png`.

- [ ] Visual review in light and dark, on macOS, Windows and Linux: the start page (empty, apps found, connecting, failed), title-bar states, the disconnected banner, the strip with a range selected, Logs (list, expanded record, search with highlights, details pane, a Flutter error) and Network (each tab; details for a JSON, an image, a binary, a truncated and a too-large body).
- [ ] Keyboard-only pass: cmd-1 to cmd-7, cmd-F, cmd-shift-F, j/k/arrows, left/right, space, enter, escape (selection, then range), cmd-R, cmd-N, cmd-shift-N, palette.
- [ ] Discovery on Windows and Linux: confirm the registry directory contains the app's DTD entry (fact P1) and `fdt` lists it within a couple of seconds.
- [ ] A hot restart and a full restart of a real app (not `jank_lab`): the run ends or continues as expected, data stays, the banner and status are correct, and bodies pending at the time become "unavailable".
- [ ] Draw time and scroll on R1 with `fdt bench-ui` (see below), and frame p99 on R2 and R3.
- [ ] Idle memory and CPU of the connected window (see below).
- [ ] A pasted URL in each form: with spaces, no trailing slash, `ws://.../ws`, a DevTools URL with `?uri=`, `https`, garbage, a closed port, a web server that is not a VM service.

## Pending verification

- **bench-ui, all six scenarios, 3 runs each, on an unlocked R1:**
  `cargo build --release -p fdt_app` then `./target/release/fdt bench-ui <logs-scroll|logs-search|net-scroll|net-filter|tool-switch|strip-drag>`. Record medians and `uptime`.
- **Idle memory and CPU of the connected window, on an unlocked R1:** start `jank_lab` (`flutter run -d macos`), run `./target/release/fdt --connect <vm uri>`, check the title bar says Connected, wait 30 s, then `ps -o rss=,%cpu= -p <pid>` every 2 s for 60 s. Budgets: RSS <= 150 MB, every CPU sample <= 1 %.
- **The UI part of attach to data on screen** (one frame after the snapshot), timed in a window.
- **The first CI run** on macOS, Windows and Linux, including the live job and the release budget step (report-only on hosted runners; `FDT_BUDGETS=strict` enforces it).
- **Windows and Linux** code paths read only from source (discovery, process checks, shortcuts, client-side frame).
- **Profile-mode attach** was measured once (Task 22), not three times.

## Carry into M3

- **Refresh-rate detection** for the strip budget line and the bench-ui interval budget (the display is assumed from the window).
- **Cross-links from stored ids:** jump from a log or request to the frame or time it happened in, using `EntryId` rather than VM ids.
- **Draw-time work** if the author decides to meet 4 ms for full-page text changes (open decision 1).
- **Deferred minors from the review ledger, summarised:**
  - Capture: the tail log chunk is copied per publish and bounded by count, not bytes; the writer waits 1 s after sending Close; `id: null` error replies are dropped.
  - Discovery: the CLI fallback can block the scan loop up to 5 s; a missed Flutter version is cached for good; Windows treats access-denied as dead.
  - Logs: details lines are cut at 4,096 characters and not wrapped; search compiles its matcher per call.
  - Network: the Errors count costs a second pass off-thread; the registry ids `logs.next` and `logs.previous` also serve Network.
  - App: the whole shell re-renders on every snapshot (about 60 Hz while capturing), `install()` checks `PATH` on the UI thread (a GUI-launched macOS app has a minimal `PATH`, so `dart` may not be found), recent URLs also remember failed URLs, and the start-connect error is not cleared by URL or recent connects.
  - Live test: the 10-minute wait for the VM URI is long for local runs; the discovery fallback can pick another `jank_lab`.
