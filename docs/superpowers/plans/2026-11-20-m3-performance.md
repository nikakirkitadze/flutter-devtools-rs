# M3 Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fdt` records each frame's phases, the VM timeline and CPU samples from a connected app. The Performance tool explains slow frames in one sentence and shows CPU, rebuild and raw-trace views. A Timeline page puts frames, network, logs, navigation and GC on shared lanes. Cross-links tie frames, requests and log lines together. All of it stays within the §10.2 budgets on a 72,000-frame, 5-million-event session.

**Architecture:**
- **`fdt_store`** gains three record streams: frame phases, a timeline slice store with level-of-detail summaries, and a CPU sample store (a call-stack trie).
- **`fdt_vm`** gains:
  - the timeline RPCs, decoded from JSON;
  - Perfetto CPU samples, decoded by a small hand-written protobuf reader;
  - Flutter's profiling extensions and refresh-rate lookup.
- **`fdt_capture`** polls the timeline every 500 ms. While widget-build tracing is on, it switches to the `Timeline` stream instead. It assembles slices, matches them to frames by `frame_number`, and fetches CPU samples every 5 s while the app draws.
- **A new `fdt_analysis` crate** (no GPUI) computes phase breakdowns, rebuild counts, CPU call trees, correlation and the slow-frame sentence.
- **`fdt_ui`** gains a pure viewport (zoom/pan) and batched canvas painters.
- **`fdt_app`** adds the Performance tool (Frames, CPU, Rebuilds and Raw trace tabs), the Timeline page, and "Related" cross-links in every details pane.

**Tech stack:**
- Rust 1.100.0, edition 2024.
- `gpui-kit` 0.10.0 (UI crates only).
- tokio, `tokio-tungstenite`, serde/serde_json (`raw_value`), base64, rayon, memchr, regex and criterion — all already in the workspace. **No new dependencies.**
- Flutter 3.41–3.50 for `jank_lab`.

**Spec:** `docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md`. M3 is the third row of §13. This plan implements:
- §5.1 and §5.2, the timeline, CPU and widget-build rows;
- §8.1, the debug-mode banner in Performance;
- §8.2, the Timeline page, cross-links and the "Related" sections;
- §8.3, the Timeline page and Performance;
- the §8.4 shortcuts and palette items for M3 data;
- §8.7, the "Trace widget builds" default;
- §9.1 `fdt_analysis` and §9.3 level-of-detail summaries for lanes;
- the M3 rows of §10.2 and §10.3;
- §10.5;
- §11.

**Inputs carried in:**
- `docs/milestones/m2-review.md` open decisions. They stay with the author, except decision 8 (a live-publishing bench), which Task 1 implements. Task 26 carries the rest forward.
- `docs/milestones/m2-deferred-findings.md`, "Should fix soon (early M3)", in Task 1. Also the final-review minors "profiling stays on after disconnect" and "the capture host needs a drawn frame".

## Scope

**M3 delivers** the M3 row of §13:
- frame ingest with phases;
- timeline ingest and level-of-detail summaries;
- the phase breakdown and slow-frame explanation;
- the CPU tab;
- profile-mode rebuild tracing;
- the Timeline page with lanes;
- cross-links.

It also delivers the rest of §8.3 Performance: the Raw trace tab, the refresh-rate budget line and the debug-mode banner.

**Deferred on purpose.** Each item is owned by a later milestone or blocked by the platform:
- **The State lane** (M6, needs the probe).
- **The memory chart, GC and memory samples from the `GC` stream, and "held by HTTP recording"** (M4). The Timeline page's GC lane in M3 comes from the GC slices the timeline already records.
- **Debug-mode rebuild counts with source locations, and "Open in editor"** (M5). In profile and debug mode, widget-build timeline slices carry no location (fact Q5.2). So the Rebuilds tab shows widget types only.
- **MCP's `explain_frame` and `get_slow_frames`, and export from the Timeline page** (M7). The analysis API is shaped so that M7 can wrap it.
- **"The widget that loaded an image"** cross-link. Nothing in the VM service data links an image request to a widget.
- **Layout and paint tracing toggles** (`profileRenderObjectLayouts` / `Paints`). The spec names only "Trace widget builds". The capture code passes any of these extension names, so adding them later is copy plus a toggle.
- **The CPU sampling period setting.** It is not in the spec. The profiler default (1 ms, about 750 samples/s) is used.
- **Thread names and raster slice names on Windows, Linux, iOS and Android.** They were checked only on macOS with Impeller. Frames are matched by `frame_number`, not thread names, so other platforms still work. The author checks them on R2 and R3.
- **The >10 % CI regression gate (§10.3).** M2's reasons still hold.

## Calls made in this plan (please check)

1. **The timeline is polled as JSON (`getVMTimeline`), not Perfetto.**
   - Every `getPerfettoVMTimeline` response repeats every track descriptor the VM has ever created, and the VM never frees them (Q3.4). Poll cost therefore grows for the life of the app: after 14 minutes, a 100 ms window cost 0.84 MB.
   - JSON carries the same events plus counters, with no growth (Q3.7).
   - Perfetto is used only for CPU samples, where JSON costs 23 MB per call against 0.8 MB (Q7.6).
   - It is decoded by a ~150-line protobuf wire reader instead of `prost`, so M3 adds no dependencies. Spec §9.4 lists `prost` only as a starting point.
2. **Polling every 500 ms by default; the `Timeline` stream only while widget-build tracing is on.**
   - Polling adds no measurable frame cost (−2 µs, Q6.4). The stream adds about 30 µs per frame, which breaks the 2 % budget for builds under about 1.5 ms.
   - With tracing on, though, the VM's 32 Ki-event ring holds only about 0.18 s (Q3.6), so a poll would lose most events. Tracing already costs about +90 % build time, so the stream's cost doesn't matter then.
3. **fdt leaves the app's recorded timeline streams alone, apart from adding `Dart` and `Embedder` if they're missing.**
   - The engine records `Dart, Embedder, GC, Microtask` from launch (Q1.1). Recording already costs the app about 20 µs per frame whether or not fdt connects.
   - An invalid stream name fails the whole call (Q1.2), so fdt only sends names from `availableStreams`.
4. **A frame is slow when `max(build, raster)` exceeds the frame budget, and janky beyond twice the budget.**
   - The budget is `1 / refresh rate` (8.3 ms at 120 Hz), read once per run through `_flutter.listViews` and `_flutter.getDisplayRefreshRate`. If either call fails, the budget is 16.7 ms.
   - Build and raster overlap in Flutter's pipeline, so `elapsed` overstates jank. **This changes M2's strip**, which coloured bars by `elapsed` against 16.7 ms.
5. **Replayed and duplicate frames are dropped.**
   - The VM replays old `Flutter.Frame` events to a new subscriber. M2 stored them at session time 0 and stored them again on a reconnect.
   - M3 drops frames that started before the run's connect time, and frames that don't start after the run's last stored frame.
6. **CPU samples are fetched every 5 s, but only while the app draws frames.**
   - The VM keeps only about 40 s of samples (Q7.5). Fetching on demand would leave nothing for older slow frames or for later export (M7).
   - Each fetch costs the VM 40–110 ms and about 1 MB, mostly the function-name table (Q7.6).
   - Task 10 measures the cost to the app. If it exceeds 1 % of frame build time, the interval becomes 15 s.
7. **Raw widget-build slices are capped at 8,000,000 per session** (about 190 MB).
   - With tracing on, the app emits about 180,000 events/s (Q5.3), which is 4 MB/s of store.
   - Past the cap, per-frame counts are still recorded and the Rebuilds tab says that older detail was dropped. Rolling caps for everything arrive in M7.
8. **Async timeline slices are grouped by name into packed rows.** The VM creates about 1.5 new async ids per frame (Q3.4), so one row per id would mean 100,000 rows.
9. **The Raw trace tab is in M3.** It is in §8.3 Performance but not named in any §13 row. It reuses the slice store and the canvas painters, so it costs one task here and avoids a gap later.
10. **The frame chart is indexed by frame (equal-width bars that scroll), while the Timeline lanes are time-based.** Individual frames stay clickable even when frames are milliseconds apart.
11. **"User code" in CPU stacks is any Dart library that isn't the SDK, Flutter or a pub-cache package**, judged by the library path the VM sends. The slow-frame sentence names user code first.
12. **Phase colours are shades of `accent` plus `lane_network` for raster, always labelled with text.** That means no new colour tokens and no change to the contrast tests; colour is never the only signal (§10.4).
13. **fdt switches profiling back to the state it found on a user disconnect or window close:** widget-build tracing, HTTP logging and socket profiling. It does nothing on an unexpected drop. Other tools attached to the same app keep whatever they turned on.
14. **`jank_lab` floats above other windows when `JANK_LAB_FLOAT=1`.** A covered macOS window draws no frames, in profile mode too (S2), so automated runs need it.

## Global constraints

**Carried from M1 and M2 (unchanged)**
- **Toolchain.** Rust `1.100.0`, edition 2024. Every crate sets `license = "Apache-2.0"` and `publish = false`.
- **UI dependency.** UI crates depend only on `gpui-kit = "=0.10.0"`. Import GPUI items as `gpui_kit::…`, components as `gpui_kit::component::…` and base items as `gpui_kit::base::…`. Never use `use gpui_kit::*`.
- **Zed code.** Never copy from Zed's GPL-3.0 crates.
- **Design tokens.** Views take every colour, size, radius and duration from `fdt_ui::tokens`. `crates/fdt_ui/tests/token_usage.rs` enforces this. The only escape hatch is a same-line `// token-check: allow: <reason>`. Unit tests go at the bottom of each file in `#[cfg(test)] mod tests`.
- **Shortcuts.**
  - Use `keymap::platform("cmd-…", "ctrl-…")`; never `secondary-…`.
  - Elements that tests find get `.id("…").test_support()`.
  - Shell bindings that must not fire in text inputs use `SHELL_NOT_INPUT`.
- **Copy.** User-facing copy is sentence case and plain. It never mentions milestones or `fdt` versions.
- **Commits.** Use conventional prefixes, and end each message with the executing session's `Co-Authored-By` line.
- **Core crates have no GPUI.** `fdt_store`, `fdt_vm`, `fdt_capture` and the new `fdt_analysis` must not depend on `gpui-kit`, directly or through another crate.
- **The UI thread.**
  - No I/O, JSON or protobuf decoding, or work proportional to session size runs on the UI thread.
  - Analysis, filtering and aggregation run in `cx.background_spawn` or on the capture runtime.
  - A view renders only what is visible.
- **Snapshots.** Views read the latest `Arc<fdt_store::Snapshot>`. The capture publishes at most one snapshot per 16 ms. An idle connected app causes no publishes, apart from the 1 Hz recording clock.
- **Ids.** Stored records have `EntryId { run, seq }`. Views key caches on `(snapshot.session, EntryId)`, because run ids restart in every session.
- **Network access.** Only the discovered or pasted DTD and VM-service addresses. No telemetry.
- **Recordings and privacy.** Committed recordings contain no tokens and no home-directory paths. This now includes paths inside base64 Perfetto blobs (Task 7).
- **States (§10.5).** Every view designs these states:
  - loading: a skeleton only after `motion::LOADING_DELAY`;
  - empty, with the one action that fills it;
  - error;
  - disconnected: data stays;
  - mode mismatch: the debug banner.
- **Keyboard.** Every new action is in `palette::registry::commands()`, so it appears in the palette, menus and cheat sheet.
- **`cargo test --workspace`** (including `token_usage`), `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` pass at the end of every task.

**New in M3**
- **No new crates.io dependencies.** `cargo deny check licenses` must pass unchanged.
- **Canvases (facts V1, V11).**
  - Every custom chart, lane or track is its own `Entity`, rendered with `.cached(..)`.
  - It paints all its quads and labels inside one `window.paint_layer(bounds, …)`.
  - It notifies only when its data, viewport or hover changes.
  - At most 20,000 quads and 500 labels per canvas per frame.
  - Tooltips live in the parent view, not in the cached entity.
- **Canvas input (V8, V9).**
  - Mouse and scroll listeners act only in `DispatchPhase::Bubble`.
  - Mouse-down checks `hitbox.is_hovered(window)`. Move and up listeners stay unguarded during a drag, which uses `window.capture_pointer`.
  - ⌘/Ctrl+scroll and `PinchEvent` zoom about the pointer.
  - Horizontal scroll, or shift+scroll, pans time.
  - Plain vertical scroll pans time in views with no vertical content (the frame chart, Timeline lanes and flame chart). In the Raw trace it scrolls between tracks.
  - `ScrollDelta::Lines` is converted with `pixel_delta(line height)`.
- **Frame budget.** `budget_us = round(1_000_000 / refresh_hz)`. It is 16,667 when the rate is unknown.
  - Status: `Ok` when `max(build_us, raster_us) ≤ budget`, `Slow` when it is ≤ 2 × budget, `Janky` beyond.
  - Colours: `success`, `warning`, `danger`. A slow or janky bar is also taller and labelled in the table (§10.4).
- **Times.** Timeline and CPU-sample timestamps are on the VM timeline clock, and are mapped with `RunClock::from_timeline_us` (CPU samples: `ts_ns / 1000`).
- **The 2 % overhead budget.** At default settings fdt subscribes to no extra stream. It adds a recorded timeline stream only when `Dart` or `Embedder` is missing. Widget-build tracing is off by default, and is labelled "Adds about 90 % to build time while on".
- **Never pass a `viewId` that didn't come from `_flutter.listViews`.** A malformed one aborts the app (Q8.1).
- **New keyboard actions:**

  | Keys | Action | Context |
  |---|---|---|
  | ⌘= / Ctrl+= | Zoom in | `Lanes` (Timeline page, Raw trace, flame chart) |
  | ⌘- / Ctrl+- | Zoom out | `Lanes` |
  | ⌘0 / Ctrl+0 | Zoom to fit (the range if set, else the session) | `Lanes` |
  | ← / → | Previous / next frame | `FrameChart` |
  | J / K, ↑ / ↓, Enter | Rows and details | `DataTable` (frames table), `Tree` (CPU) |
  | ← / → | Collapse / expand | `Tree` (built in, V12) |

  Palette-only commands:
  - "Show frames", "Show CPU profile", "Show rebuilds", "Show raw trace";
  - "Trace widget builds" (toggle);
  - "Next slow frame", "Previous slow frame";
  - "Fetch CPU samples now".

**Budgets that apply in M3 (§10.2), on R1 unless noted**

| Measure | Budget | Checked by |
|---|---|---|
| Pan and zoom the frame chart, Timeline lanes, Raw trace and flame chart on the stress session | frame p99 ≤ 8.3 ms, no frame > 16.7 ms (R2/R3 p99 ≤ 16.7 ms) | `fdt bench-ui` (Task 24) |
| UI-thread work per frame | draw p99 ≤ 4 ms | `fdt bench-ui` |
| Open slow-frame details, including the sentence | ≤ 50 ms | bench-ui `frame-details` and a release budget test (Task 14) |
| Switch tools (fully drawn), including Performance and Timeline | ≤ 50 ms | bench-ui `tool-switch` |
| Timeline ingest: decode and assemble | ≥ 1,000,000 events/s on one core | release budget test (Task 8) |
| CPU call tree for 450,000 samples | ≤ 100 ms | release budget test (Task 13) |
| Overhead on the profiled app at default settings | ≤ 2 % added to frame build time | Task 26, with `frame_build_sampler` |
| Idle memory (connected, nothing selected) | ≤ 150 MB RSS | Task 26 |
| Idle CPU (connected, app idle) | ≤ 1 % | Task 26 |
| Attach to first live data | ≤ 1 s (unchanged) | Task 26 |

**Stress session (§10.1)** is M2's `synth::stress_session(seed)`, which Task 4 extends. It holds:
- 72,000 frames at 120 Hz with phases;
- about 5,000,000 timeline slices on 12 thread tracks and 4 async rows;
- 450,000 CPU samples over a trie of about 3,000 call sites;
- M2's 1,000,000 logs and 100,000 requests.

## Review focus

These are the inputs most likely to hurt a real user that feature tests would not exercise by default. Each one has a test in the task that owns the code.

1. **A ragged timeline.** The input includes:
   - events lost when the ring overflowed between polls;
   - an `E` with no `B`, or a `B` that never closes;
   - events out of order within a response (Q2.5);
   - an async id equal to a thread id (Q2.6);
   - phase names in another case (`Build` instead of `BUILD`);
   - a frame whose `Flutter.Frame` event arrives seconds before or after its slices.

   Expected:
   - Nothing panics, and nothing is attributed to the wrong frame.
   - A frame with no matching slices shows "No timeline detail for this frame" while its build and raster times still show.
   - *(Tasks 8, 18)*
2. **Hot restart, reconnect or disconnect in the middle of a frame or a trace.** Expected:
   - Open slices are dropped and frames are not duplicated.
   - Tracing is re-applied to the new isolate.
   - A user disconnect turns tracing back off.
   - Old frames keep their phases.
   - *(Tasks 9, 11)*
3. **Pathological frames.** The input includes:
   - a 10 s frame (an app hang);
   - a frame with 100,000 widget builds;
   - nesting 600 levels deep;
   - a 4 KB widget type name.

   Expected:
   - The explanation finishes within 50 ms.
   - Rows clip, labels elide, and depth is shown up to the store's cap.
   - The table and chart stay within budget.
   - *(Tasks 3, 14, 17, 21)*
4. **CPU data that is missing, huge or strange.** The input includes:
   - the profiler off in the app;
   - a fetch that times out;
   - 30,000 samples;
   - 128-deep stacks;
   - 10 KB mangled Swift names;
   - a function with no library.

   Expected:
   - The CPU tab and the details pane say why there is no data.
   - Names elide.
   - The call tree builds within its budget.
   - Native frames never count as user code.
   - *(Tasks 10, 13, 19)*
5. **Extreme zoom and pan.**
   - Input:
     - zoom to below 1 µs or beyond the whole session;
     - pinch with a delta of ±5;
     - a wheel with `Lines` deltas;
     - a zero-width range drag;
     - an empty session;
     - a session that is still growing while zoomed in.
   - Expected:
     - The viewport clamps to [1 µs, session span + 10 %].
     - There are no NaN or negative widths.
     - A zero-width drag is a click.
     - The view stays within budget.
   - *(Tasks 15, 21, 22)*

## Facts verified while planning (read before starting)

Two spikes ran on this machine on 2026-11-20 (Flutter 3.47.9, Dart 3.13.7, VM service protocol 4.21, macOS arm64, Impeller):
- **Protocol facts (Q)** come from `jank_lab` in profile and debug mode.
- **UI facts (V)** come from compiled, measured release builds against `gpui-kit` 0.10.0.

M2's facts P1–P17 and U1–U12 (in `docs/superpowers/plans/2026-11-19-m2-connect-logs-network.md`) still hold. Anything below marked *unverified* was not observed.

### Protocol

- **Q1. Timeline flags.**
  - `getVMTimelineFlags` → `{"availableStreams":["API","Compiler","CompilerVerbose","Dart","Debugger","Embedder","GC","Isolate","Microtask","VM"],"recordedStreams":["Dart","Embedder","GC","Microtask"],"recorderName":"Ring","type":"TimelineFlags"}`. This is the engine default from launch, in profile and debug mode.
  - `setVMTimelineFlags {recordedStreams:[…]}` replaces the whole set.
  - **Any unknown or wrong-case name fails the whole call** with `-32602`, and nothing changes.
  - DevTools sends `["GC","Dart","Embedder"]`.
- **Q2. `getVMTimeline {timeOriginMicros, timeExtentMicros}`** → `{"type":"Timeline","traceEvents":[…],"timeOriginMicros":…,"timeExtentMicros":…}`.
  - The response's origin and extent are the earliest event and the span actually returned.
  - Trace events look like `{"name":"LAYOUT","cat":"Dart","ph":"B","pid":20931,"tid":259,"ts":420817589524,"args":{…}}`, with `ts` in µs on the `getVMTimelineMicros` clock (the same clock as `Flutter.Frame.startTime`).
  - Phases seen:
    - `B`/`E`: sync slices; both carry `name`;
    - `b`/`e`: async; `id` is a hex string, e.g. `"1fdb"`;
    - `C`: counters;
    - `s`/`t`/`f`: flows;
    - `M` with `name:"thread_name"`, `args.name`: thread names, repeated in every response;
    - `X` (complete, with `dur`) is legal, but none was seen.
- **Q3. Windows and ordering.**
  - The window is inclusive on the event start.
  - Polling `[next, now]` and then setting `next = now + 1` gave **zero duplicates and zero out-of-window events** over 4–10 s runs at 50, 100, 500 and 1,000 ms intervals.
  - Slices straddle polls (`B` in one response, `E` in the next), so keep per-track stacks across polls.
  - A few late-recorded async begins (`SceneDisplayLag`) are never returned. Their `e` arrives alone; ignore it.
  - Events are **not** time-sorted, with dozens of inversions per second and up to 10 per track. **Stable-sort each response by `ts` before matching.**
  - Async ids can equal a thread id: key async tracks separately from threads.
- **Q4. Ring buffer:** 32,768 events.
  - It holds about 4.5 s under raster-heavy jank and about 6 s or more for build-heavy jank.
  - **With widget-build tracing on, it holds 0.18 s** (about 180,000 events/s).
  - A 1 s JSON window is about 250–510 KB and 4–30 ms per call. That is about 93 bytes per event, and 2.7–8.6 k events/s with the default streams.
- **Q5. `Timeline` stream push.** `streamListen {streamId:"Timeline"}` delivers `{"kind":"TimelineEvents","timelineEvents":[64 JSON trace events]}` blocks.
  - Latency: median 25 ms, p90 53 ms, up to 10 s for quiet threads.
  - It costs about +30 µs per frame while subscribed.
  - `setVMTimelineFlags` also posts `TimelineStreamSubscriptionsUpdate {updatedStreams}` on this stream.
  - Unsubscribe with `streamCancel {streamId:"Timeline"}`.
- **Q6. One frame** on macOS, where the UI thread is named `io.flutter.platform` and raster runs on `io.flutter.raster`:
  ```
  UI thread:  VsyncProcessCallback {StartTime, TargetTime: ns}
                Animator::BeginFrame {frame_number:"2778"}
                  BUILD > [widget slices when tracing]
                  LAYOUT (root) > LAYOUT
                  UPDATING COMPOSITING BITS (root) > UPDATING COMPOSITING BITS
                  PAINT (root) > PAINT
                  COMPOSITING > Animator::Render {frame_number}
                  FINALIZE TREE
                  POST_FRAME
  raster:     GPURasterizer::Draw > Rasterizer::DoDraw {frame_number} > Rasterizer::DrawToSurfaces > …
                (Canvas::saveLayer slices appear under LayerTree::Paint)
  async rows: Frame > Animate, PipelineItem > PipelineProduce, Frame Request Pending, SceneDisplayLag
  GC:         slices with cat "GC" on DartWorker threads (CollectNewGeneration, Scavenge, MarkSweep, …)
  ```
  - **`Flutter.Frame.number` equals the `frame_number` arg** (a string) on `Animator::BeginFrame`, `Animator::Render` and `Rasterizer::DoDraw`, and on no other slice.
  - `startTime` equals `BeginFrame.ts` (±1 µs). `build` ≈ `Render.ts − startTime`. `raster` ≈ `DoDraw.dur`.
  - **Phase names are upper case in 3.47.** Other versions are unverified, so match case-insensitively.
  - Child slices do **not** sum to their parent: about 28 ms of a 31 ms `DoDraw` had no child.
  - There are about 75 events per frame with the default streams.
- **Q7. Widget-build tracing.**
  - `ext.flutter.profileWidgetBuilds` and `ext.flutter.profileUserWidgetBuilds` (and `…RenderObjectLayouts`/`…Paints`) are registered in profile **and** debug mode.
  - Set with `{isolateId, enabled:"true"|"false"}` (string values). Read with `{isolateId}`. The reply is `{"enabled":"false","method":"…","type":"_extensionType"}`.
  - All are off at launch. Whether hot restart resets them is unverified, so re-apply after `IsolateStart`.
  - Each rebuilt widget becomes a `Dart` slice named by its `runtimeType` (e.g. `HeavyBuildPanel`, `NotificationListener<LayoutChangedNotification>`). The slices carry **no location**, nest by element tree under `BUILD`, and the direct children of `BUILD` are the dirty roots.
  - `/rebuilds` produced 1,329 slices per frame. Median build went from 0.90 to 1.82 ms (+88 %).
- **Q8. CPU samples.**
  - The profiler is on by default: `profiler=true`, `profile_period=1000`, about 750 samples/s on a busy UI thread.
  - Idle time is not sampled. Only the isolate's mutator thread appears.
  - The buffer holds about 29.8 k samples, **about 40 s** of a fully busy thread. The response's origin can be much older than real coverage.
  - **`getPerfettoCpuSamples {isolateId, timeOriginMicros, timeExtentMicros}`** → `{"type":"PerfettoCpuSamples","samplePeriod":1000,"maxStackDepth":128,"sampleCount":739,"pid":…,"timeOriginMicros":…,"timeExtentMicros":…,"samples":"<base64 protobuf>"}`.
  - A 1 s window costs about 0.84 MB and 41–60 ms; 30 s costs 1.63 MB and 334 ms. The fixed part is the function-name table, which is sent again every call. `getCpuSamples` (JSON) costs 23 MB.
  - If the profiler is off, the call is expected to fail or return zero samples (unverified). Treat both as "CPU sampling is off in this app".
- **Q9. Perfetto CPU-sample wire format.**
  - A `Trace` is repeated field 1 (`TracePacket`, length-delimited).
  - `TracePacket`:
    - 8 `timestamp` (varint ns, clock 3 = monotonic = `getVMTimelineMicros` × 1000)
    - 12 `interned_data`
    - 13 `sequence_flags` (bit 0 = incremental state cleared)
    - 66 `perf_sample`
    - 6 `clock_snapshot` (ignore)
  - `InternedData`:
    - 5 `function_names` {1 iid, 2 str}
    - 6 `frames` {1 iid, 2 function_name_id, 3 mapping_id}
    - 7 `callstacks` {1 iid, 2 frame_ids, **unpacked** repeated varints, **root first**}
    - 17 `mapping_paths` {1 iid, 2 str}
    - 19 `mappings` {1 iid, 7 path_string_ids, repeated}
  - `PerfSample`: 2 pid, 3 tid, 4 callstack_iid.
  - Layout:
    - The first data packet has `sequence_flags=3` and **all** names, frames and mappings.
    - Each sample packet may carry `interned_data{callstacks}` for a new stack.
    - **iids restart in every response.**
    - Samples are not time-ordered.
  - Names:
    - Dart frames are `Class.method`, with a mapping path that is the resolved library URI (`org-dartlang-sdk:///…`, or a `file:///…` path that may contain the home directory).
    - Native frames (C++, ObjC, mangled Swift) have `mapping_id` 0.
    - There are no line numbers.
- **Q10. Refresh rate.**
  - First `_flutter.listViews {}` → `{"views":[{"id":"_flutterView/0xc50c95420","type":"FlutterView",…}]}`.
  - Then `_flutter.getDisplayRefreshRate {viewId}` → `{"type":"DisplayRefreshRate","fps":120.0}`.
  - `fps == 0` means 60.
  - **A malformed `viewId` aborts the app** (`stoull`).
  - Both calls go through DDS and take about 1 ms.
- **Q11. A covered macOS `jank_lab` window draws no frames, in profile mode too.** Automated runs need a floating window. `flutter run` prints "Failed to foreground app".
- **Q12. Versions.**
  - Every RPC above exists from protocol 4.6 (`getPerfettoCpuSamples`), and Dart 3.11 ships 4.20. So all of Flutter 3.41–3.50 has them.
  - Event names, the default streams and the platform-thread merge are verified only on 3.47.

### UI (`gpui-kit` 0.10.0, release, measured)

- **V1. Batch quads in a layer.**
  - Without one, `paint_quad` cost grows with n squared: 10k quads take 6.6 ms and 50k take 125 ms.
  - Inside `window.paint_layer(bounds, |w| …)` it is linear, about 40 ns per quad: 10k take 0.39 ms, 72k take 3.1 ms.
  - `PathBuilder` is a trap: 2,000 rects take 10 ms.
  - Text in a layer draws above that layer's quads whatever the call order (V2).
- **V3. Labels in canvases.**
  - Shape with `window.text_system().shape_line(text: SharedString, font_size, &[TextRun{len: text.len(), font, color, background_color: None, underline: None, strikethrough: None}], None) -> ShapedLine`.
  - Paint with `.paint(origin, line_height, TextAlign::Left, None, window, cx)`. Width comes from `.width()`.
  - The shape cache keeps the current and previous frame, so the same strings every frame are cheap: 300 labels take 0.3–0.6 ms, and 0.32 ms when clipped with `window.with_content_mask(Some(ContentMask{bounds}), …)`. New strings cost about 9 µs each.
  - Fits-check: `window.text_system().line_wrapper(font, size).should_truncate_line(&str, width, "…", TruncateFrom::End) -> Option<usize>`, 300 checks in 35 µs. Elide with `.truncate_line(…)`.
- **V4. Input.**
  - `ScrollEvent { position, delta: ScrollDelta::{Pixels(Point<Pixels>), Lines(Point<f32>)}, modifiers, touch_phase, source }`, with `delta.pixel_delta(line_height)`.
  - `PinchEvent { position, delta: f32 /* 0.1 = +10 % */, modifiers, phase }` exists on macOS.
  - Register both in the canvas paint closure with `window.on_mouse_event(…)`. **Both fire in the Capture and Bubble phases.**
  - `window.capture_pointer(hitbox.id)` keeps `is_hovered` true for the rest of a drag.
  - `window.set_cursor_style(CursorStyle::…, &hitbox)` works in paint only.
  - Trackpad momentum arrives as `Moved` phases (unverified).
- **V5. Tooltips.**
  - Keep `hover: Option<(item, Point<Pixels>)>` in the parent view.
  - Render `deferred(anchored().position(pos + offset).snap_to_window_with_margin(m).child(…)).with_priority(1)`.
  - It costs about +0.08 ms per frame while following the mouse.
- **V6. Caching.**
  - A `.cached(StyleRefinement)` entity skips its render and paint closures when it was not notified, and replays its primitives.
  - With a layer, a cached 72k-bar chart costs 0.23 ms. Without one it costs 6.8 ms either way.
  - The cache style must give a definite size.
- **V7. Tree.**
  - `gpui_kit::component::tree::{tree, TreeItem, TreeState, TreeEntry, TreeEvent}`.
  - `TreeItem::new(id, label).expanded(bool).children(…)`. Ids must be unique.
  - `TreeState::new(cx).items(vec)`, then `set_items`, `selected_index`, `set_selected_index`, `index_of(&id)`, `scroll_to_item`, `focus`.
  - Render with `tree(&state, |ix, entry, selected, window, cx| ListItem::new(ix).pl(depth indent).child(…))`.
  - The key context is `"Tree"`, with ↑/↓, ←/→ collapse/expand and Enter built in.
  - 20,220 nodes cost 0.34 ms per scrolling frame; building them costs 3.4–3.9 ms; a toggle costs 0.47 ms.
  - Expansion lives on the items, so `set_items` must carry `.expanded(..)` through.
- **V8. gpui-component charts** (`BarChart` etc.) are category-band charts with no level of detail or selection, so they are not used.

## File structure

```
Cargo.toml                                   members += fdt_analysis
crates/fdt_store/src/
  frames.rs                                  FramePhases, FrameStatus, budget, range/at/by_number, slow index
  timeline.rs        (new)                   names, tracks, lanes of slices, lane level-of-detail, TimelineSnapshot
  cpu.rs             (new)                   function table, call-stack trie, samples by time, CpuSnapshot
  lanes.rs           (new)                   level-of-detail for log and request lanes, navigation index
  run.rs, session.rs                         refresh rate and tracing per run; new snapshot fields
  synth.rs                                   timeline, cpu, perf_session
  benches/store.rs                           + timeline lanes, cpu
crates/fdt_vm/src/
  timeline.rs        (new)                   flags, getVMTimeline, TraceEvent decode, streamCancel
  perfetto.rs        (new)                   protobuf wire reader, CPU-sample decode
  flutter.rs         (new)                   bool profiling extensions, refresh rate
  events.rs, testing.rs, service.rs
  examples/record_jank_lab.rs                + `perf` recording mode
  tests/decode_recordings.rs, tests/recordings_privacy.rs
crates/fdt_capture/src/
  ingest/assemble.rs (new)                   pure timeline assembler: tracks, B/E, async rows, frame phases
  ingest/perf.rs     (new)                   PerfHub: timeline poller/stream, CPU poller, tracing, cleanup
  ingest/frames.rs, ingest/mod.rs, connect.rs, capture.rs, lifecycle.rs, config.rs
  tests/perf.rs      (new)                   replay and scripted tests
crates/fdt_analysis/ (new, no GPUI)
  src/lib.rs, breakdown.rs, rebuilds.rs, correlate.rs, cpu_tree.rs, user_code.rs, explain.rs
crates/fdt_ui/src/
  tokens.rs                                  layout, table and alpha tokens for M3
  viewport.rs        (new, pure)             time viewport: zoom, pan, clamp, x↔time
  canvas.rs          (new)                   layered bar/span/label painters, label fitting, zoom/pan input
crates/fdt_app/src/
  nav.rs                                     Selection::Frame
  tools/performance/{mod,view,frame_chart,frames_table,details,cpu_tab,flame,rebuilds_tab,trace_tab}.rs  (new)
  tools/timeline/{mod,view,lanes}.rs         (new)
  tools/logs/details.rs, tools/network/details.rs     Related sections
  shell/{mod,strip,counts,tool_page,banner}.rs
  palette/{items,registry,view}.rs, actions.rs, keymap.rs, settings.rs, capture_host.rs,
  test_support.rs, perf.rs, bench_ui.rs
fixtures/jank_lab/lib/{ext.dart,scenarios/jank.dart}, macos/Runner/MainFlutterWindow.swift
fixtures/recordings/jank_lab-perf.jsonl (new)
scripts/record-jank-lab.sh, scripts/jank-lab-it.sh, .github/workflows/ci.yml
docs/milestones/m3-review.md (new)
```

Task order is sequential:
- Task 1 carries M2's early fixes.
- Tasks 2–5 build the store.
- Tasks 6–7 build the protocol layer and the recordings.
- Tasks 8–11 build capture.
- Tasks 12–14 build analysis.
- Tasks 15–23 build the UI.
- Task 24 adds benchmarks, Task 25 the live test and Task 26 the measurements.

Task 7 needs Flutter and a macOS or Linux desktop.

---
### Task 1: Early fixes carried from M2

**Files:**
- Modify:
  - `crates/fdt_store/src/logs.rs` (tail bound by bytes, `level_counts_from`)
  - `crates/fdt_app/src/tools/logs/view.rs` (incremental counts, match count)
  - `crates/fdt_vm/src/discovery.rs` (tests)
  - `crates/fdt_app/src/app.rs` (capture host fallback)
  - `crates/fdt_app/src/shell/start_page.rs` (runtime failure copy)
  - `crates/fdt_app/src/bench_ui.rs` (`live-logs`)

**Interfaces:**
- **Consumes:** M2 as merged at `aff8ccf`.
- **Produces:**
  - **The log store's tail.**
    - `LOG_TAIL_MAX_BYTES: usize = 256 * 1024`. The log store seals its tail chunk when it reaches the chunk's record count **or** when the tail's text bytes reach this value.
    - Test hook: `LogsSnapshot::tail_text_bytes() -> usize`.
  - **`LogsSnapshot::level_counts_from(from_seq: usize, range: Option<TimeRange>) -> [u64; 4]`** counts only records at or after `from_seq`. The Logs job's incremental path adds it to the prior counts instead of recounting.
  - **The Logs match count** (`logs-match-count`) shows whenever `!self.search_pending()` and a result has been applied. It no longer requires `settled()`.
  - **The capture host has a timer fallback.** It still installs from the first-frame chain, and also from a `cx.spawn` timer 500 ms after the window opens if it isn't installed by then (`// token-check: allow: startup fallback`). If the runtime fails to start, the Start page shows "Couldn't start the background recorder." with the error in the collapsible details.
  - **A new `bench-ui` scenario, `live-logs`.**
    - A background thread publishes a new snapshot every 16 ms: the stress session plus 50 new log lines per publish, built with a `SessionWriter` the scenario owns.
    - The Logs view follows. The report is the usual JSON plus per-view render counts.
    - Budgets: draw p99 ≤ 4 ms and the interval budget.
- [ ] **Step 1: Write the failing tests.**
  - `a_stream_of_large_records_seals_by_bytes` (`fdt_store`): push 100 records of 64 KiB each, publish after each one, and assert `tail_text_bytes() <= LOG_TAIL_MAX_BYTES + 64 * 1024` every time.
  - `level_counts_from_matches_a_full_count`: on `synth::logs` with 10,000 records, `level_counts_from(k, r)` plus the counts of `[0, k)` equals `level_counts(r)` for 5 values of `k` and 3 ranges.
  - `incremental_and_full_results_match` (Logs view): 20 append steps with a level filter, a text filter and a range. After each step the rows and counts equal a fresh full job's.
  - `match_count_stays_during_an_incremental_job`: with search text settled, set `running = true` through the test hook and the count element still exists.
  - `capture_host_installs_without_a_frame`: open the shell in a test window that never draws, `advance_clock(600 ms)`, and the host is installed.
- [ ] **Step 2: Make the discovery tests deterministic.**
  - Tests that wait on real time with 350–500 ms margins move to `#[tokio::test(start_paused = true)]` with `tokio::time::advance`.
  - Tests that must touch the file system use a `wait_until(cond, 5 s)` poll helper instead of fixed sleeps.
  - Run `cargo test -p fdt_vm discovery` 20 times in a loop. Expected: 20 passes.
- [ ] **Step 3: Implement until green, then add `live-logs`.**
  - Run `cargo run --release -p fdt_app -- bench-ui live-logs` once and paste its JSON into the report. It is report-only here; Task 26 judges it.
- [ ] **Step 4: Verify.** `cargo test --workspace`, clippy and fmt all pass.
- [ ] **Step 5: Commit.** `fix(store): bound the log tail by bytes and count incrementally`, `fix(app): keep the match count while streaming and install the capture host without a frame`, and `test(vm): make discovery tests deterministic`.

### Task 2: Frame budget, status, phases and queries

**Files:**
- Modify:
  - `crates/fdt_store/src/frames.rs`, `run.rs`, `session.rs`, `synth.rs`
  - `crates/fdt_app/src/shell/strip.rs` (field renames only; the strip's new colouring is Task 16)
- Test: unit tests in `frames.rs`

**Interfaces:**
- **Consumes:** M2 `FrameStore`, `LodSeries`, `RunInfo`.
- **Produces:**
  - **Budget.**
    - `pub const DEFAULT_BUDGET_US: u32 = 16_667`.
    - `pub fn budget_us(refresh_hz: Option<f32>) -> u32` is `round(1e6 / hz)` when `hz` is finite and ≥ 1, else `DEFAULT_BUDGET_US`.
    - `RunInfo.refresh_hz: Option<f32>`, set with `SessionWriter::set_refresh_rate(run, hz: f32)`.
    - `Snapshot::frame_budget_us(run) -> u32` and `Snapshot::latest_budget_us() -> u32` (the last run's budget, or the default).
  - **Status.**
    - `pub enum FrameStatus { Ok, Slow, Janky }`.
    - `pub fn frame_status(time_us: u32, budget_us: u32) -> FrameStatus`: `Ok` when `time_us ≤ budget_us`, `Slow` when it is ≤ 2 × budget, else `Janky`.
    - `FrameRecord::time_us() -> u32` is `max(build_us, raster_us)`. `FrameRecord::end() -> SessionTime` is `start + elapsed`.
  - **Phases.**
    - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub struct UiPhases { begin: SessionTime, ui_us: u32, build_us: u32, layout_us: u32, paint_us: u32, composite_us: u32, finalize_us: u32, widgets_built: Option<u32> }`.
    - `pub struct RasterPhase { start: SessionTime, raster_us: u32 }`.
    - `FrameRecord.ui: Option<UiPhases>` and `FrameRecord.raster: Option<RasterPhase>`.
    - `SessionWriter::set_frame_ui(run, number: u64, UiPhases)` and `set_frame_raster(run, number, RasterPhase)`.
      - If the frame is stored, it updates the frame copy-on-write.
      - Otherwise it parks the value in a pending map keyed by `(run, number)` that `push_frame` drains.
      - The pending map holds at most 4,096 entries and drops the oldest.
  - **Duplicates.** `SessionWriter::push_frame(FrameInput) -> Option<EntryId>` returns `None`, and stores nothing, when `start` is not after the last stored frame of the same run.
  - **Queries on `FramesSnapshot`.** Frames are stored sorted by start.
    - `range(r: TimeRange) -> std::ops::Range<usize>`: frames whose start is in `r`.
    - `at(t) -> Option<usize>`: the frame whose `[start, end]` contains `t`.
    - `first_after(t) -> Option<usize>`.
    - `by_number(run, number) -> Option<usize>`.
    - `slow() -> &ChunkedSnapshot<u32>`: the seqs of `Slow` and `Janky` frames, judged with the run's budget. `set_refresh_rate` rebuilds that run's entries.
  - **Level of detail.**
    - The frames level-of-detail series stores `time_us()` instead of `elapsed_us`.
    - `StripBar { start, end, max_us, count }`, renamed from `max_elapsed_us` and `frames`.
    - `pub fn lod_cells(lod: &LodSnapshot, range, width_px) -> Vec<StripBar>`. `strip_bars(frames, …)` now delegates to it.
  - **`synth::frames`** also fills `ui` and `raster` consistently:
    - build is split 60/10/15/10/5 % into build, layout, paint, composite and finalize;
    - `raster.start` is `start + build`;
    - `widgets_built: None`.
- [ ] **Step 1: Write the failing tests.**
  - `budget_from_refresh_rate`: 120.0 → 8,333; 59.94 → 16,683; 60.0 → 16,667; 0.0, NaN and `None` → 16,667.
  - `status_thresholds`: with budget 8,333, 8,333 is `Ok`, 8,334 is `Slow`, 16,666 is `Slow` and 16,667 is `Janky`.
  - `status_uses_build_or_raster_not_elapsed`: build 5 ms, raster 6 ms and elapsed 20 ms at budget 8,333 → `Ok`.
  - `phases_set_before_and_after_the_frame_arrive`.
  - `pending_phases_are_bounded`.
  - `replayed_and_duplicate_frames_are_dropped`: the same start twice, and an earlier start, give `None`. A later run's earlier number is accepted.
  - `range_at_first_after_and_by_number` on `synth::frames(…, 10_000, 3, 120)`.
  - `slow_index_follows_the_refresh_rate`: 60 → 120 Hz grows the index.
  - `lod_keeps_max_of_build_and_raster`.
  - The existing strip tests, updated for the renames.
- [ ] **Step 2: Implement, then verify.** Run `cargo test -p fdt_store -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(store): frame budget, status, phases and time queries`.

### Task 3: Timeline slice store with level-of-detail lanes

**Files:**
- Create: `crates/fdt_store/src/timeline.rs`
- Modify: `session.rs`, `lib.rs`, `synth.rs`, `benches/store.rs`

**Interfaces:**
- **Consumes:** `Chunked`, `LodSeries`, `RunId`, `SessionTime` and Task 2's phase setters.
- **Produces:**
  - **Types.**
    - `pub struct NameId(pub u32)` and `pub struct TrackId(pub u32)`, both `Copy + Eq + Hash`.
    - `pub enum TrackKind { Thread, Async }`.
    - `#[repr(u8)] pub enum SliceCat { Dart, Embedder, Gc, Other }`.
    - `pub struct TrackInfo { id: TrackId, run: RunId, kind: TrackKind, name: Arc<str>, order: u32 }`. `order` is 0 for the UI thread, 1 for raster and 10 for others; tracks sort by `(run, order, name)`.
    - `#[derive(Clone, Copy)] pub struct Slice { start: SessionTime, dur_ns: u64, name: NameId, cat: SliceCat }`. A test asserts `size_of::<Slice>() == 24`.
  - **Constants.**
    - `MAX_DEPTH: usize = 256`.
    - `LANE_LOD_BASE_SHIFT: u32 = 22`: level-0 cells are about 4.2 ms.
    - `LANE_LOD_LEVELS: usize = 20`.
    - `RAW_BUILD_CAP: u64 = 8_000_000`.
  - **Writer.**
    - `intern_name(&str) -> NameId`.
    - `timeline_track(run, kind, key: u64, name: &str) -> TrackId` is idempotent per `(run, kind, key)`. A non-empty name replaces an empty one.
    - `set_track_order(TrackId, u32)`.
    - `push_slice(track, depth: u16, s: Slice, widget_build: bool) -> bool`. It returns `false`, stores nothing and counts the drop when any of these holds:
      - `depth ≥ MAX_DEPTH`;
      - `s.start` is before the end of that lane's last slice (lanes never overlap);
      - `widget_build` is set and the session already holds `RAW_BUILD_CAP` widget-build slices.
    - It calls `touch(end)`.
  - **`Snapshot.timeline: TimelineSnapshot`.**
    - `name(NameId) -> &str`.
    - `tracks() -> &[TrackInfo]`, sorted.
    - `lanes(TrackId) -> &[LaneSnapshot]`, indexed by depth.
    - `drops() -> TimelineDrops { depth: u64, overlap: u64, build_cap: u64 }`.
    - `gc_lod() -> &LodSnapshot`: `insert(start, dur_us)` for every `Gc` slice at depth 0.
    - `slice_count() -> u64`.
  - **`LaneSnapshot`.**
    - `len()` and `get(i) -> &Slice`.
    - `range_indices(r: TimeRange) -> Range<usize>`: every slice that overlaps `r`. Binary search on start, then include the one before it if it ends after `r.start`.
    - `cells(r: TimeRange, width_px: usize) -> Vec<LaneCell>`.
      - `LaneCell { start: SessionTime, end: SessionTime, covered_ns: u64, count: u32 }`.
      - It uses the smallest level whose cell count over `r` is ≤ `width_px`.
  - **Lane level-of-detail algorithm** (not determined by the signatures):
    - For each lane and level k, keep a sparse `Chunked<(u64 cell_index, u64 covered_ns, u32 count)>` in ascending index order.
    - A pushed slice adds its overlap with each cell it covers, at every level. Because a lane's slices arrive in start order, each update either touches the last cell or appends a new one.
  - **`synth::perf(w, run, frames: usize, seed, hz)`** pushes frames, their phases and about 70 slices per frame:
    - The UI track (`io.flutter.ui`, order 0) gets `VsyncProcessCallback > Animator::BeginFrame > {BUILD, LAYOUT (root) > LAYOUT, PAINT (root) > PAINT, COMPOSITING > Animator::Render, FINALIZE TREE, POST_FRAME}`. 10 % of frames get 3 levels of 4 widget slices under BUILD.
    - The raster track (`io.flutter.raster`, order 1) gets `GPURasterizer::Draw > Rasterizer::DoDraw > Rasterizer::DrawToSurfaces > {LayerTree::Preroll, LayerTree::Paint > n × Canvas::saveLayer}`.
    - Two async rows hold `Frame > Animate` and `PipelineItem > PipelineProduce`.
    - A `DartWorker` track gets a `Gc` slice every 50 frames.
    - Durations match the frame's phases.
- [ ] **Step 1: Write the failing tests.**
  - `slice_is_24_bytes`.
  - `tracks_are_idempotent_and_named_late`.
  - `range_indices_include_a_straddling_slice`.
  - `overlapping_and_too_deep_slices_are_dropped_and_counted`.
  - `widget_build_cap_drops_only_build_slices`: use a test-only `with_build_cap(10)`.
  - `cells_fit_the_width_and_conserve_coverage`: over 10,000 random non-overlapping slices, the summed `covered_ns` over the whole span equals the summed durations, and the cell count is ≤ width.
  - `sealed_chunks_are_shared_between_publishes` (`Arc::ptr_eq`).
  - `gc_lod_counts_gc_slices`.
  - `synth_perf_makes_about_70_slices_per_frame`: 1,000 frames → 60k–80k slices; every frame's `ui` and `raster` are set.
- [ ] **Step 2: Write the budget tests** (release, names contain `budget`).
  - `lane_query_budget_5m`: `synth::perf` with 72,000 frames. Then `range_indices` plus `cells(whole span, 2_000)` for every lane of the UI track, p90 ≤ 0.5 ms over 50 runs.
  - `timeline_publish_budget`: append 10,000 slices, then publish, p90 ≤ 2 ms.
- [ ] **Step 3: Implement, verify, and add a criterion bench** `timeline_cells`. Run `cargo test -p fdt_store` and the release budget run. Expected: PASS.
- [ ] **Step 4: Commit.** `feat(store): timeline slices with level-of-detail lanes`.

### Task 4: CPU sample store and the full stress session

**Files:**
- Create: `crates/fdt_store/src/cpu.rs`
- Modify: `session.rs`, `lib.rs`, `synth.rs`, `benches/store.rs`

**Interfaces:**
- **Consumes:** Task 3's store patterns.
- **Produces:**
  - **Types.**
    - `pub struct FunctionId(pub u32)`.
    - `pub enum FunctionKind { Dart, Native }`. A function is `Native` when it has no library.
    - `pub struct FunctionInfo { name: Arc<str>, library: Option<Arc<str>>, kind: FunctionKind }`.
    - `pub struct NodeId(pub u32)`. Node 0 is the trie root and has no function.
    - `pub struct CpuNode { parent: NodeId, function: FunctionId, depth: u16 }`.
    - `#[derive(Clone, Copy)] pub struct CpuSample { time: SessionTime, leaf: NodeId }`.
    - `pub enum CpuStatus { Unknown, On, Off(Arc<str>) }`.
    - `MAX_STACK_DEPTH: usize = 256`. Deeper stacks are cut at the root side, keeping the leaf end.
  - **Writer.**
    - `intern_function(name: &str, library: Option<&str>) -> FunctionId`, keyed by `(name, library)`.
    - `intern_stack(root_first: &[FunctionId]) -> NodeId`, through a writer-side `HashMap<(NodeId, FunctionId), NodeId>`.
    - `push_cpu_samples(run, batch: Vec<CpuSample>)` sorts the batch by time and drops samples at or before the last stored sample's time.
    - `set_cpu_coverage(run, end: SessionTime)` and `set_cpu_status(run, CpuStatus)`.
  - **`Snapshot.cpu: CpuSnapshot`.**
    - `function(FunctionId) -> &FunctionInfo`.
    - `node(NodeId) -> &CpuNode` and `node_count()`.
    - `samples() -> &ChunkedSnapshot<CpuSample>`.
    - `range(r) -> Range<usize>`.
    - `stack(leaf) -> impl Iterator<Item = FunctionId>`, leaf first.
    - `coverage_end(run) -> Option<SessionTime>` and `status(run) -> &CpuStatus`.
  - **`synth::cpu(w, run, samples: usize, seed, span: TimeRange)`.**
    - About 300 functions. 20 % have library `file:///app/lib/screen.dart`, 50 % have `file:///sdk/flutter/packages/flutter/lib/src/widgets/framework.dart`, and 30 % are native.
    - A trie of about 3,000 nodes, at most 40 deep.
    - Samples are spread evenly at about 750/s.
  - **`synth::stress_session(seed)`** now also runs `synth::perf(…, 72_000, seed, 120)`, `set_refresh_rate(run, 120.0)` and `synth::cpu(…, 450_000, …)` over the frames' span. M2's bench scenarios keep using it.
- [ ] **Step 1: Write the failing tests.**
  - `stacks_share_prefixes`.
  - `stack_walks_leaf_to_root`.
  - `batches_are_sorted_and_overlaps_dropped`.
  - `deep_stacks_keep_the_leaf`: 300 frames → depth 256, and the leaf is kept.
  - `range_by_time`.
  - `coverage_and_status_per_run`.
  - `stress_session_has_perf_data`: 72,000 frames, ≥ 4,500,000 slices, 450,000 samples, budget 8,333.
- [ ] **Step 2: Implement, verify** (`cargo test -p fdt_store`), and confirm the M2 bench scenarios still build (`cargo build --release -p fdt_app`).
- [ ] **Step 3: Commit.** `feat(store): CPU samples on a call-stack trie and the M3 stress session`.

### Task 5: Lane summaries for logs and requests, and the navigation index

**Files:**
- Create: `crates/fdt_store/src/lanes.rs`
- Modify: `logs.rs`, `net.rs`, `frames.rs` (`LodSeries::raise`)

**Interfaces:**
- **Consumes:** `LodSeries` and `lod_cells` (Task 2).
- **Produces:**
  - `LodSeries::raise(t, value)` raises a bucket's max without counting.
  - `LogsSnapshot::lod() -> &LodSnapshot` holds `insert(time, level_index)`, where `Debug=0, Info=1, Warning=2, Error=3`.
  - `LogsSnapshot::navigation() -> &ChunkedSnapshot<u32>` holds the seqs of `LogSource::Navigation` records.
  - `NetSnapshot::lod() -> &LodSnapshot`.
    - It does `insert(start, 0)` on a request's first upsert.
    - It does `raise(start, 1)` when the request becomes an error (`is_error()`).
    - So `max_us == 1` marks a cell with an error.
  - `lanes.rs` re-exports `lod_cells` and adds `pub fn lane_cells(lod: &LodSnapshot, range, width_px) -> Vec<StripBar>`, an alias with lane docs.
- [ ] **Step 1: Write the failing tests.**
  - `log_lod_keeps_the_worst_level`.
  - `navigation_index_lists_route_changes`.
  - `request_lod_marks_errors_without_double_counting`: a request upserted 3 times, then failing → count 1, max 1.
  - `lane_cells_fit_the_width` on the stress session's logs and requests.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_store`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(store): level-of-detail summaries for log and request lanes`.

### Task 6: `fdt_vm`: timeline, Perfetto CPU samples, Flutter profiling extensions and refresh rate

**Files:**
- Create: `crates/fdt_vm/src/timeline.rs`, `perfetto.rs`, `flutter.rs`
- Modify: `events.rs`, `lib.rs`, `testing.rs` (helpers)
- Test: unit tests in each file

**Interfaces:**
- **Consumes:** `VmService`, `RpcClient::call_raw_with_timeout`, `decode_notification`.
- **Produces:**
  - **`timeline`**
    - `TimelineFlags { available: Vec<String>, recorded: Vec<String> }`.
    - `VmService::get_vm_timeline_flags()` and `set_vm_timeline_flags(&[&str])`.
    - `VmService::stream_cancel(stream)`. Error 104, "Stream not subscribed", counts as OK.
    - `pub fn wanted_streams(flags) -> Option<Vec<String>>`: the recorded set plus whichever of `Dart` and `Embedder` are available and missing, in that order. It is `None` when nothing is missing, and it never adds a name that isn't in `available`.
    - `VmService::get_vm_timeline(origin_us: i64, extent_us: i64, timeout) -> Result<TimelineChunk, RpcError>`.
      - It uses the raw call and decodes off the reader task.
      - `TimelineChunk { events: Vec<TraceEvent>, skipped: u32 }`.
    - `pub fn decode_trace_events(json: &str) -> (Vec<TraceEvent>, u32 /* skipped */)`.
    - `TraceEvent { ph: Phase, name: Box<str>, cat: Category, pid: i64, tid: i64, ts_us: i64, dur_us: Option<i64>, id: Option<u64>, frame_number: Option<u64>, thread_name: Option<Box<str>> }`.
      - `id` is parsed from a hex string, a decimal string or a number.
      - `frame_number` comes from `args.frame_number`.
      - `thread_name` comes from `M` events' `args.name`.
    - `Phase { Begin, End, Complete, AsyncBegin, AsyncEnd, Instant, Counter, Metadata, Other }`.
    - `Category { Dart, Embedder, Gc, Other }`.
  - **`events`.** New variants:
    - `StreamEvent::TimelineEvents { events: Vec<TraceEvent> }`;
    - `StreamEvent::TimelineSubscriptions { recorded: Vec<String> }`, from `TimelineStreamSubscriptionsUpdate`.
  - **`perfetto`**
    - `pub fn decode_cpu_samples(bytes: &[u8]) -> Result<CpuProfile, String>`.
    - `CpuProfile { functions: Vec<PerfFunction>, stacks: Vec<Vec<u32>> /* root-first indices into functions */, samples: Vec<PerfSampleRec>, }`.
    - `PerfFunction { name: String, library: Option<String> }`.
    - `PerfSampleRec { ts_ns: u64, tid: u32, stack: u32 }`.
    - The field numbers are in fact Q9.
      - Reset interning whenever `sequence_flags & 1` is set.
      - Functions are deduplicated by `(name, library)` within one response.
      - The library is the mapping's first path string.
      - Unknown fields are skipped.
      - Truncated input returns `Err`.
    - `VmService::get_perfetto_cpu_samples(isolate, origin_us, extent_us, timeout) -> Result<(CpuProfile, CpuWindow { origin_us, extent_us, period_us }), RpcError>`. Base64 and protobuf are decoded in `spawn_blocking`.
    - `#[cfg(any(test, feature = "testing"))] pub mod encode { pub fn cpu_samples(…) -> Vec<u8> }` is a minimal encoder used by the tests here and in `fdt_capture`.
  - **`flutter`**
    - `pub const PROFILE_WIDGET_BUILDS: &str = "ext.flutter.profileWidgetBuilds"`.
    - `VmService::get_bool_extension(method, isolate) -> Result<bool, _>` and `set_bool_extension(method, isolate, on) -> Result<bool, _>`. Both send `enabled` as `"true"`/`"false"` and read `enabled` back, accepting a string or a bool.
    - `VmService::display_refresh_rate(isolate) -> Result<Option<f32>, RpcError>`.
      - It calls `_flutter.listViews` and takes the first view with `type == "FlutterView"`.
      - **Only then** does it call `_flutter.getDisplayRefreshRate {viewId: <that id verbatim>}`.
      - `fps == 0` maps to 60. No view gives `Ok(None)`, with no second call.
- [ ] **Step 1: Write the failing tests.**
  - `wanted_streams_cases`:
    - the engine default → `None`;
    - `["GC"]` → `["GC","Dart","Embedder"]`;
    - `Embedder` not available → only `Dart` is added;
    - an unknown recorded name is kept as is.
  - `trace_event_phases_decode`: one event of each phase, from Q2 excerpts.
  - `async_ids_parse_hex_decimal_and_numbers`.
  - `frame_number_comes_from_args`.
  - `bad_events_are_skipped_and_counted`.
  - `timeline_events_notification_decodes`.
  - `cpu_samples_round_trip`:
    - Encode 2 responses whose iids restart.
    - Decode them: stacks are root-first, functions are deduplicated and native frames have no library.
  - `truncated_protobuf_is_an_error`.
  - `refresh_rate_passes_only_the_listed_view_id` (`ScriptedServer`): assert the exact `viewId` param, and assert that no `getDisplayRefreshRate` is sent when `views` is empty.
  - `bool_extension_sends_strings`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_vm` and clippy. Expected: PASS. `cargo deny check licenses` is unchanged.
- [ ] **Step 3: Commit.** `feat(vm): timeline, Perfetto CPU samples, profiling extensions and refresh rate`.

### Task 7: `jank_lab` automation, the perf recording, replay and privacy

**Files:**
- Modify:
  - `fixtures/jank_lab/lib/ext.dart`
  - `lib/scenarios/jank.dart`
  - `macos/Runner/MainFlutterWindow.swift`
  - `scripts/record-jank-lab.sh`, `scripts/jank-lab-it.sh`
  - `crates/fdt_vm/examples/record_jank_lab.rs`
  - `src/tap.rs`, `src/testing.rs`
  - `tests/decode_recordings.rs`, `tests/recordings_privacy.rs`
  - `fixtures/recordings/README.md`
- Create: `fixtures/recordings/jank_lab-perf.jsonl`

**Interfaces:**
- **Consumes:** Task 6.
- **Produces:**
  - **`ext.jank_lab.run` built-ins.**
    - `goto_jank` and `goto_rebuilds` push `/jank` and `/rebuilds`.
    - `home` pops until the first route.
    - `JankScreen` registers `raster_on` and `raster_off`, which set the "Raster heavy" switch.
    - Every action still ends with `scheduleFrame()`.
  - **`MainFlutterWindow`** sets `level = .floating` when the environment has `JANK_LAB_FLOAT=1`. Both scripts export it.
  - **`record_jank_lab --mode perf`** (profile mode only). The sequence:
    1. Connect, `getVMTimelineFlags`, `_flutter.listViews` and the refresh rate. Subscribe to `Extension` and `Isolate`.
    2. `goto_jank`, wait 1 s, then 2 timeline polls (`getVMTimelineMicros` and a `getVMTimeline` of about 500 ms each). Then `getPerfettoCpuSamples` for the jank window.
    3. `raster_on`, wait 1 s, 2 polls, `raster_off`.
    4. `home`, `goto_rebuilds`, then turn `profileWidgetBuilds` on.
    5. `streamListen Timeline` for 0.5 s, then turn `profileWidgetBuilds` off and `streamCancel Timeline`.

    `scripts/record-jank-lab.sh perf` writes `fixtures/recordings/jank_lab-perf.jsonl`. The file must be ≤ 12 MiB; a test checks this.
  - **Redaction.** `tap::redact` also rewrites the home directory inside base64 strings of 1 KB or more, under the keys `samples` and `trace`.
    - It decodes them, replaces each character of the user name with `x` (same length, so protobuf lengths stay valid), and re-encodes.
    - `recordings_privacy` decodes every such blob and fails on any `/Users/<name>/` or `/home/<name>/` whose `<name>` is not all `x`.
  - **`ReplayServer`.** Once recorded answers run out, `getVMTimeline` returns `{"type":"Timeline","traceEvents":[],"timeOriginMicros":0,"timeExtentMicros":0}`. `getPerfettoCpuSamples` returns the same shape with `sampleCount: 0` and an empty `samples`. They never repeat the last answer.
- [ ] **Step 1: Write the failing tests.**
  - `perf_recording_decodes` (`fdt_vm/tests/decode_recordings.rs`):
    - Every `getVMTimeline` result and every `TimelineEvents` notification decodes with `skipped == 0`.
    - At least one `Animator::BeginFrame` carries a `frame_number` equal to a recorded `Flutter.Frame.number`.
    - The CPU response decodes to ≥ 100 samples, and some stack contains a function named `_HeavyBuildPanelState.build`.
    - The `TimelineEvents` blocks contain a slice named `ProductTile`.
  - `perf_recording_is_small`.
  - `base64_blobs_are_redacted`.
  - `replay_exhausts_to_empty_timeline`.
  - `jank_lab` Dart test: `goto_jank` and `raster_on`/`raster_off` change the route and the switch (`fixtures/jank_lab/test/ext_test.dart`).
- [ ] **Step 2: Record.**
  - Run `scripts/record-jank-lab.sh perf` with Flutter on macOS.
  - Check `rg -c "Users/[^x]" fixtures/recordings/jank_lab-perf.jsonl` → 0.
  - Re-run `flutter analyze` and `flutter test` in `fixtures/jank_lab`.
- [ ] **Step 3: Verify.** Run `cargo test -p fdt_vm`. Expected: PASS.
- [ ] **Step 4: Commit.** `feat(jank_lab): perf automation and a redacted profile recording of timeline and CPU samples`.

### Task 8: Timeline assembler

**Files:**
- Create: `crates/fdt_capture/src/ingest/assemble.rs`
- Test: unit tests there, plus a release budget test

**Interfaces:**
- **Consumes:** Task 6 `TraceEvent`.
- **Produces:**
  - **The pure assembler.**
    - `pub(crate) struct Assembler` and `Assembler::new()`.
    - `feed(&mut self, events: Vec<TraceEvent>, out: &mut AssembledBatch)` stable-sorts the events by `ts_us` first.
    - `reset(&mut self)` drops open slices, async rows and pending frame accumulators.
    - `stats() -> AssemblerStats { unmatched_end, unclosed_dropped, async_overflow, too_deep }`.
  - **`AssembledBatch`.** All times are timeline µs.
    - `tracks: Vec<(TrackKeyRaw, Box<str> /* name, may be empty */, u32 /* order */)>`.
    - `slices: Vec<RawSlice>`, where `RawSlice { track: TrackKeyRaw, depth: u16, start_us: i64, dur_us: i64, name: Box<str>, cat: Category, widget_build: bool }`.
    - `ui: Vec<(u64 /* frame number */, UiPhasesUs)>` and `raster: Vec<(u64, i64 /* start_us */, i64 /* dur_us */)>`.
    - `TrackKeyRaw = (TrackKind, u64)`. For a thread the key is the tid. For an async row it is a stable hash of `(name, row)`.
  - **Rules** (not determined by the signatures):
    - **Sync slices.** `B`/`E` use a stack per `(pid, tid)`. Depth is the stack length at `B`.
      - An `E` whose name doesn't match the top pops to the nearest match within 4 levels; otherwise it is dropped and counted.
      - An `E` on an empty stack is dropped and counted.
      - An `X` event is a slice at the current depth.
    - **Async slices.** `b`/`e` are matched on `(cat, name, id)`.
      - A closed async slice goes to the row track named after the slice. It takes the first row whose last end ≤ its start; otherwise a new row opens.
      - At most 16 rows per name. Beyond that the slice is dropped and counted.
      - Async keys never mix with thread keys (Q3).
    - **Thread names** come from `M` events.
    - **The UI thread** is the track whose `Animator::BeginFrame` carries a `frame_number`; it gets order 0. **The raster thread** is the track whose `Rasterizer::DoDraw` carries one; it gets order 1.
    - **Phases.**
      - When a `BeginFrame` with `frame_number` closes, its direct children are summed by normalised name:
        - normalise by upper-casing and stripping a trailing ` (ROOT)`;
        - `BUILD` → build;
        - `LAYOUT` → layout;
        - `PAINT` → paint;
        - `UPDATING COMPOSITING BITS` and `COMPOSITING` → composite;
        - `FINALIZE TREE`, `POST_FRAME` and `SEMANTICS` → finalize.
      - The result is emitted as `ui`, with `ui_us = BeginFrame.dur` and `widgets_built = Some(count)` when any slice nested inside `BUILD` closed during it.
      - A `DoDraw` with `frame_number` emits `raster`.
    - **Widget builds.** Every slice that closes while a `BUILD` is open below it on the same stack is a widget build.
    - **Depth** ≥ 256 is dropped and counted.
- [ ] **Step 1: Write the failing tests** (Review focus 1).
  - `sync_slices_nest_by_depth`.
  - `end_without_begin_is_dropped`.
  - `mismatched_end_pops_to_match`.
  - `unsorted_events_are_sorted_first`.
  - `slices_straddle_feeds`: `B` in feed 1 and `E` in feed 2.
  - `reset_drops_open_slices`.
  - `async_id_equal_to_tid_stays_separate`.
  - `async_rows_pack_and_cap`.
  - `lowercase_phase_names_still_count`.
  - `recorded_frames_match_flutter_frame`: feed the perf recording's timeline responses. For every frame present in both, `ui_us − Flutter.Frame.build` is between 0 and 300 µs (`BeginFrame` runs 50–250 µs longer than `build`, fact Q6) and `|raster_us − Flutter.Frame.raster| ≤ 50`. Assert this for at least 5 frames.
  - `widget_builds_are_counted`: the recording's `TimelineEvents` part, where a frame has `widgets_built ≥ 100`.
  - `too_deep_is_dropped`.
- [ ] **Step 2: Write the budget test** `assemble_budget` (release): JSON-encode 200,000 synthetic events (70 per frame), then `decode_trace_events` plus `feed` takes ≤ 200 ms (p50 of 5 runs). Add the same workload as a criterion bench, `crates/fdt_capture/benches/ingest.rs` (§10.3 "ingest").
- [ ] **Step 3: Implement and verify.** Run `cargo test -p fdt_capture assemble` and the release budget run. Expected: PASS.
- [ ] **Step 4: Commit.** `feat(capture): assemble timeline slices and frame phases`.

### Task 9: Timeline capture: flags, polling, refresh rate and frame rules

**Files:**
- Create: `crates/fdt_capture/src/ingest/perf.rs`, `crates/fdt_capture/tests/perf.rs`
- Modify:
  - `ingest/mod.rs` (`Ingest::Timeline`, `Ingest::PerfReady`, Router forwarding)
  - `ingest/frames.rs`, `connect.rs`, `capture.rs`, `lifecycle.rs`, `config.rs`

**Interfaces:**
- **Consumes:** Tasks 2, 3, 6 and 8.
- **Produces:**
  - **`CaptureConfig`:** `timeline_poll: Duration` (500 ms), `timeline_call_timeout` (5 s) and `refresh_timeout` (1 s).
  - **The connect sequence** (inside the existing 5 s deadline), after the calibration:
    1. `display_refresh_rate(main)` with `refresh_timeout`; an error means `None`. The result travels in `Ingest::StartRun { …, refresh_hz: Option<f32>, connect_timeline_us: i64 }`, and `start_run` calls `set_refresh_rate`.
    2. `get_vm_timeline_flags`, and if `wanted_streams` returns `Some(list)`, `set_vm_timeline_flags(list)`. Errors are logged and ignored.
  - **Frame rules.** `frame_input` returns `None` when `startTime < connect_timeline_us`. The store drops non-increasing starts (Task 2).
  - **`PerfHub`.** Spawned after `NetHub` and aborted on drop. It owns one task that `select!`s on:
    - **the poll tick**, which is skipped while in stream mode:
      1. `now = get_vm_timeline_micros()`.
      2. `get_vm_timeline(next, now - next)`, then `next = now + 1`. On an error other than `Closed`, `next` stays put.
      3. Feed the assembler, map µs to `SessionTime` with `shared.get().clock`, and send `Ingest::Timeline(TimelineBatch)`.
    - **events forwarded by the Router** from `StreamEvent::TimelineEvents`;
    - **commands from `PerfControl`**, which Tasks 10 and 11 extend.

    If `shared.get().conn` differs from the last batch's, the assembler resets: a new session or reconnect.
  - **`TimelineBatch`.** Session times, plus `run`:
    - `tracks: Vec<(TrackKind, u64, Box<str>, u32)>`;
    - `slices: Vec<BatchSlice>`, with `BatchSlice { track: (TrackKind, u64), depth: u16, start: SessionTime, dur_ns: u64, name: Box<str>, cat: SliceCat, widget_build: bool }`. `Category` maps to `SliceCat` one to one;
    - `ui: Vec<(u64, UiPhases)>` and `raster: Vec<(u64, RasterPhase)>`.

    `Session::ingest` writes it only while recording, using `intern_name`, `timeline_track`, `set_track_order`, `push_slice`, `set_frame_ui` and `set_frame_raster`.
  - **`Ingest::PerfReady(Arc<PerfControl>)`** mirrors `NetReady`. The session keeps it for commands. `on_stale` handles every new variant.
- [ ] **Step 1: Write the failing tests** (`tests/perf.rs`).
  - `flags_add_missing_streams_once`:
    - with `["GC"]` recorded, exactly one `setVMTimelineFlags ["GC","Dart","Embedder"]`;
    - with the engine default, none.
  - `refresh_rate_lands_in_the_run`: 120 → budget 8,333. `listViews` failing → 16,667, and the connect still succeeds.
  - `poll_windows_are_contiguous`: each origin is the previous `now + 1`.
  - `polled_slices_reach_the_store_with_phases` (`ScriptedServer` serving 2 polls of the recording's events).
  - `replayed_frames_before_connect_are_dropped`.
  - `reconnect_to_the_same_vm_does_not_duplicate_frames`.
  - `new_session_resets_the_assembler`: no slice in the new session starts before 0.
  - `paused_recording_drops_batches`.
  - `closed_connection_ends_the_poller`.
  - `replay_profile_perf` (`ReplayServer`, `jank_lab-perf.jsonl`): at least 5 frames have `ui` and `raster` set. For each, `ui.ui_us − FrameRecord.build_us` is between 0 and 300 µs.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_capture`. Expected: PASS, with the M2 tests unchanged.
- [ ] **Step 3: Commit.** `feat(capture): poll the VM timeline, match phases to frames and read the refresh rate`.

### Task 10: CPU sample capture

**Files:**
- Modify: `crates/fdt_capture/src/ingest/perf.rs`, `ingest/mod.rs`, `capture.rs`, `config.rs`, `crates/fdt_app/src/capture_host.rs` (trait), `crates/fdt_app/src/test_support.rs` (fake)
- Test: `crates/fdt_capture/tests/perf.rs`

**Interfaces:**
- **Consumes:** Tasks 4, 6 and 9.
- **Produces:**
  - **`CaptureConfig`:** `cpu_poll: Duration` (5 s) and `cpu_call_timeout` (5 s).
  - **The `PerfHub` CPU tick.**
    - It fetches only if the Router counted at least one `Flutter.Frame` since the last fetch (`Arc<AtomicU64>`), or a fetch-now was requested.
    - Window: `[cpu_next, now]` on the main isolate, which is shared with the Router so it follows hot restart. Then `cpu_next = now + 1`.
    - It decodes, maps `ts_ns / 1000` with the clock, and sends `Ingest::Cpu(CpuBatch { functions: Vec<(String, Option<String>)>, stacks: Vec<Vec<u32>>, samples: Vec<(SessionTime, u32)>, coverage_end: SessionTime })`.
    - A server error marks the profiler off: it sends `Ingest::CpuStatus(CpuStatus::Off("CPU sampling is off in this app."))` once and stops CPU ticks for this connection. A timeout is logged and retried on the next tick.
  - **The session** interns functions and stacks, then pushes the samples, the coverage and `CpuStatus::On`.
  - **`Capture::fetch_cpu_samples()`** sends `Command::FetchCpuNow`, which reaches `PerfControl::fetch_now()`.
  - **`CaptureControl::fetch_cpu_samples(&self)`**. `FakeCapture` logs it as `"fetch_cpu"`.
- [ ] **Step 1: Write the failing tests.**
  - `cpu_fetch_waits_for_frames`.
  - `fetch_now_fetches_immediately`.
  - `cpu_windows_are_contiguous`.
  - `profiler_off_marks_status_and_stops`.
  - `cpu_timeout_retries_on_the_next_tick`: the window is not advanced, and the status stays `On`.
  - `hot_restart_moves_cpu_to_the_new_isolate`.
  - `replay_cpu_has_the_heavy_build_function`: the CPU store holds `_HeavyBuildPanelState.build` with library `Some(..)`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_capture -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Measure the cost to the app** (Calls 6). Run `jank_lab` in profile mode on `/jank`, using `JANK_LAB_FLOAT=1` and `goto_jank`.
  1. Use `frame_build_sampler` in 6 interleaved rounds of 150 frames, alternating these two setups:
     - `attach_hold`, extended with a `--cpu-poll <secs>` flag, connected with `cpu_poll` 5 s;
     - not connected.
  2. Report the median and p99 `build` deltas.
  3. If the median delta is over 20 µs or the p99 delta is over 1 ms, set the default `cpu_poll` to 15 s, re-measure, and record both runs.
- [ ] **Step 4: Commit.** `feat(capture): fetch CPU samples while the app draws`.

### Task 11: Widget-build tracing, and restoring profiling on disconnect

**Files:**
- Modify:
  - `crates/fdt_capture/src/ingest/perf.rs`, `ingest/http.rs` (remember prior state), `ingest/mod.rs` (Router)
  - `capture.rs`, `lifecycle.rs`, `config.rs`
  - `crates/fdt_store/src/run.rs`, `session.rs` (`tracing`)
  - `crates/fdt_app/src/settings.rs`, `capture_host.rs`, `test_support.rs`

**Interfaces:**
- **Consumes:** Tasks 6 and 9, and M2's `NetHub`.
- **Produces:**
  - **Settings and config.**
    - `Settings.trace_widget_builds: bool` (default `false`, §8.7).
    - `CaptureConfig.trace_widget_builds: bool`, set by `config_from_settings`.
    - `RunInfo.tracing: bool`, set with `SessionWriter::set_tracing(run, bool)`.
  - **The command.** `Capture::set_trace_widget_builds(on)` → `Command::SetTraceWidgetBuilds(on)`. The session keeps the wanted state across connects. While connected, `PerfControl::set_tracing(on)` runs this sequence:
    1. `set_bool_extension(PROFILE_WIDGET_BUILDS, main, on)`.
    2. On success with `on`: `stream_listen("Timeline")` and stream mode.
    3. On `off`: `stream_cancel("Timeline")`, then `next = get_vm_timeline_micros() + 1` and back to polling.
    4. `Ingest::Tracing(actual)` reaches `set_tracing`.
  - **Connect and hot restart.**
    - On connect, a wanted `true` is applied after `StartRun`.
    - On hot restart it is re-applied when `ServiceExtensionAdded(ext.flutter.profileWidgetBuilds)` arrives for the new main isolate.
  - **Restore.** `PerfControl::restore(timeout 500 ms)` and `NetHub::restore(timeout 500 ms)` put back what fdt changed:
    - tracing goes off if fdt turned it on;
    - HTTP logging and socket profiling go off only if they were off before fdt enabled them. `NetHub` reads the state first by calling the extension without `enabled`.

    They run on `Command::Disconnect` and `Command::Shutdown` (window close), before the connection is aborted. An unexpected close skips them.
  - **`CaptureControl::set_trace_widget_builds(&self, on: bool)`**. `FakeCapture` logs it as `"trace_builds true|false"`.
- [ ] **Step 1: Write the failing tests.**
  - `tracing_on_sets_the_extension_and_streams`: `enabled:"true"`, then `streamListen Timeline`; forwarded `TimelineEvents` reach the store.
  - `tracing_off_cancels_and_resumes_polling`.
  - `wanted_tracing_applies_on_connect`.
  - `hot_restart_reapplies_tracing`.
  - `user_disconnect_restores_what_fdt_changed`: tracing off, and HTTP logging off when it was off at connect.
  - `user_disconnect_keeps_http_on_when_it_was_on_before`.
  - `unexpected_drop_makes_no_restore_calls`.
  - `settings_default_reaches_capture_config`.
- [ ] **Step 2: Implement and verify.** Run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(capture): widget-build tracing, and restore profiling state on disconnect`.

### Task 12: `fdt_analysis`: phase breakdown, rebuilds, correlation and user code

**Files:**
- Create: `crates/fdt_analysis/Cargo.toml` (deps: `fdt_store`, `rayon`), `src/lib.rs`, `src/breakdown.rs`, `src/rebuilds.rs`, `src/correlate.rs`, `src/user_code.rs`
- Modify: root `Cargo.toml` (members)

**Interfaces:**
- **Consumes:** Tasks 2–5 and M2's `filter_requests`/`filter_logs`.
- **Produces:**
  - **`breakdown`**
    - `pub enum Phase { Build, Layout, Paint, Composite, Finalize, OtherUi, Raster }`.
    - `pub struct Segment { phase: Phase, us: u32 }`.
    - `pub struct Breakdown { segments: Vec<Segment>, ui_us: u32, raster_us: u32, budget_us: u32, status: FrameStatus, over: Option<Phase>, has_timeline: bool }`.
    - `pub fn breakdown(frame: &FrameRecord, budget_us: u32) -> Breakdown`. The rules:
      - With `frame.ui`, the segments are Build, Layout, Paint, Composite, Finalize, then `OtherUi = ui_us − sum` (shown only when > 0), then Raster.
      - Without it, the segments are Build (`build_us`) and Raster only, with `has_timeline: false`.
      - `over` is `None` when the status is `Ok`.
      - Otherwise `over` is `Raster` when `raster_us > budget_us && raster_us ≥ ui time`, else the largest UI segment.
  - **`rebuilds`**
    - `pub struct WidgetCount { name: Arc<str>, builds: u32, frames: u32, max_in_frame: u32, self_ns: u64, total_ns: u64 }`.
    - `pub struct RootRebuild { name: Arc<str>, widgets: u32, total_ns: u64, largest_child: Option<(Arc<str>, u32)> }`.
    - `pub struct FrameRebuilds { total: u32, roots: Vec<RootRebuild>, widgets: Vec<WidgetCount> }`. Widgets are sorted by builds, then total time, both descending.
    - `pub fn frame_rebuilds(snap: &Snapshot, frame_seq: usize) -> Option<FrameRebuilds>`.
      - Find the run's UI track (order 0) and the `BUILD` slice inside `[ui.begin, ui.begin + ui_us]`.
      - Walk deeper lanes inside BUILD's span. Each slice's parent is the enclosing slice one lane up.
      - The roots are BUILD's direct children. `largest_child` is the root's direct child with the largest subtree.
      - `None` when the frame has no widget-build slices.
    - `pub fn range_rebuilds(snap, range: Option<TimeRange>, cancel: &AtomicBool) -> Option<Vec<WidgetCount>>` merges `frame_rebuilds` over frames with `widgets_built > 0` in range, using rayon. `None` when cancelled.
  - **`correlate`**
    - `pub const RELATED_LIMIT: usize = 20`.
    - `pub struct Related { frames: Vec<usize>, requests: Vec<u32>, logs: Vec<u32> }`.
    - `pub fn frame_window(frame, budget_us) -> TimeRange` is `[start, start + max(elapsed, budget)]`.
    - `related_to_frame(snap, frame_seq) -> Related` lists requests overlapping the window and logs inside it.
    - `related_to_log(snap, log_seq) -> Related` lists the frame at the log's time, else the first frame starting within 100 ms after it, plus the requests in flight at that instant.
    - `related_to_request(snap, req_seq) -> Related` lists the first frame starting at or after the response end, within 1 s ("the frame that processed the response"), plus the logs inside `[start, end]`.
    - Each list is capped at `RELATED_LIMIT`, keeping those nearest the anchor time.
  - **`user_code`**
    - `pub fn is_user_library(library: Option<&str>) -> bool` is `false` for:
      - `None` (native code);
      - anything starting `dart:`, `org-dartlang-sdk:` or `package:flutter/`;
      - any path containing `/packages/flutter/`, `/flutter/packages/`, `/.pub-cache/`, `/pub-cache/hosted/` or `/flutter/bin/cache/`.

      It is `true` otherwise.
- [ ] **Step 1: Write the failing tests.**
  - `breakdown_cases`:
    - raster over;
    - build over;
    - layout over;
    - other-UI over;
    - an `Ok` frame has no `over`;
    - with no phases, two segments and `has_timeline == false`.
  - `frame_rebuilds_counts_roots_and_children`: a writer-built frame with `BUILD > CartScreen > {CartBadge, ProductGrid > 64 × ProductTile > Card > Text}` gives root `CartScreen`, `largest_child == ("ProductGrid", 193)`, and `ProductTile` builds 64.
  - `range_rebuilds_merges_frames_and_cancels`.
  - `related_windows_and_limits`.
  - `log_links_to_the_frame_at_its_time_or_just_after`.
  - `request_links_to_the_frame_after_its_response`.
  - `user_code_cases`:
    - `file:///Users/xxxx/src/jank_lab/lib/scenarios/jank.dart` → true;
    - `file:///Users/xxxx/flutter/packages/flutter/lib/src/widgets/framework.dart` → false;
    - `org-dartlang-sdk:///sdk/lib/core/stopwatch.dart` → false;
    - `None` → false.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_analysis`, then `cargo tree -p fdt_analysis | rg gpui`, which must print nothing. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(analysis): phase breakdown, rebuild attribution and correlation`.

### Task 13: CPU call trees, busiest functions and flame spans

**Files:**
- Create: `crates/fdt_analysis/src/cpu_tree.rs`, `benches/analysis.rs` (criterion)
- Test: unit tests and release budget tests

**Interfaces:**
- **Consumes:** Task 4 `CpuSnapshot`.
- **Produces:**
  - **Types.**
    - `pub struct CallNode { function: FunctionId, self_count: u64, total_count: u64, depth: u16, children: Vec<u32> }`. Children are sorted by `total_count`, descending.
    - `pub struct CallTree { nodes: Vec<CallNode>, roots: Vec<u32>, samples: u64 }`.
  - **`pub fn top_down(cpu: &CpuSnapshot, range: Option<TimeRange>, cancel: &AtomicBool) -> Option<CallTree>`**:
    1. Count samples per trie leaf, finding the range by binary search.
    2. Accumulate up the trie.
    3. Emit the nodes that have samples.
    - It is O(samples + nodes).
  - **`pub fn bottom_up(…) -> Option<CallTree>`**:
    - Roots are the functions with self samples.
    - A node's children are its callers. Each leaf-to-root path is inserted with its count.
    - A function that recurses is counted once per path at each level.
  - **`pub struct FunctionCost { function: FunctionId, self_count: u64, total_count: u64 }`** and **`pub fn busiest(cpu, range, limit: usize) -> Vec<FunctionCost>`**.
    - Ordered by `self_count`.
    - `total_count` counts each function at most once per sample.
  - **`pub struct FlameSpan { depth: u16, start: SessionTime, end: SessionTime, function: FunctionId }`** and **`pub fn flame_spans(cpu, range: TimeRange, max_spans: usize) -> (Vec<FlameSpan>, bool /* truncated */)`**:
    - Walk the samples in time order, keeping one open span per depth.
    - Consecutive samples with the same function at depth d, and the same frames above it, extend the span when their gap is ≤ 2 × the sample period. The period comes from the run, at 1 ms by default.
    - A span ends at the next sample, or at its own sample time plus the period.
    - Output stops at `max_spans`.
- [ ] **Step 1: Write the failing tests.** Use a hand-built trie with 3 roots, recursion and 2 time ranges.
  - `top_down_counts_self_and_total`.
  - `bottom_up_lists_callers`.
  - `recursion_counts_once_per_sample`.
  - `range_limits_samples`.
  - `cancel_returns_none`.
  - `flame_spans_merge_and_split`.
  - `flame_truncates_at_the_cap`.
  - `deep_stacks_and_long_names` (Review focus 4): 128-deep stacks with 10 KB names.
- [ ] **Step 2: Write the budget tests** (release).
  - `call_tree_budget_450k`: on the stress session, the whole range, `top_down` and `bottom_up` each take ≤ 100 ms (p50 of 5).
  - `flame_budget`: a 10 s range takes ≤ 50 ms.
- [ ] **Step 3: Implement and verify.** Run `cargo test -p fdt_analysis` and the release budget run. Expected: PASS.
- [ ] **Step 4: Commit.** `feat(analysis): CPU call trees and flame spans`.

### Task 14: Slow-frame explanation

**Files:**
- Create: `crates/fdt_analysis/src/explain.rs`
- Test: unit tests and a release budget test

**Interfaces:**
- **Consumes:** Tasks 12 and 13.
- **Produces:**
  - **`pub enum CpuState { Covered, Pending, Off(Arc<str>), NoSamples }`**:
    - `Off` when the run's status is `Off`.
    - `Pending` when `coverage_end < frame end`.
    - `NoSamples` when it is covered but no sample falls in the frame's UI window.
  - **`pub enum Cause`**:
    - `WithinBudget`
    - `Raster { save_layers: u32 }`
    - `Gc { gc_ns: u64 }`
    - `WidgetBuild { widget: Arc<str>, self_ns: u64, share: f32 }`
    - `Rebuilt { root: Arc<str>, widgets: u32, child: Option<(Arc<str>, u32)> }`
    - `HotFunction { function: Arc<str>, phase: Phase, share: f32 }`
    - `Phase(Phase)`
  - **`pub struct Explanation { sentence: String, cause: Cause, breakdown: Breakdown, rebuilds: Option<FrameRebuilds>, functions: Vec<FunctionCost>, cpu: CpuState, related: Related }`**.
    - `functions` lists at most 10 entries from the frame's UI window, user code first.
  - **`pub fn explain(snap: &Snapshot, frame_seq: usize) -> Explanation`.**
  - **Rules.** The first rule that matches wins. Let `t` be `format_ms(time_us)`, `b` the budget in ms, and `x` = time / budget formatted `{:.1}×`. `format_ms` prints `{:.1} ms` under 10 ms and `{:.0} ms` otherwise. Counts use comma grouping (`1,345`). Names longer than 80 characters are cut to 79 plus `…`.

    | # | When | Cause | Sentence (exact copy) |
    |---|---|---|---|
    | 1 | status `Ok` | `WithinBudget` | `Within budget ({t} of {b}).` |
    | 2 | over = Raster and ≥ 10 `Canvas::saveLayer` slices on the raster track in the raster window | `Raster` | `Raster took {r}, {x} the budget: the scene used {n} saveLayer calls (opacity, blurs or clips).` |
    | 3 | over = Raster | `Phase(Raster)` | `Raster took {r}, {x} the budget.` |
    | 4 | `Gc` slices overlapping the UI window add up to ≥ 25 % of `ui_us` | `Gc` | `Garbage collection ran for {g} during this frame's {u} of UI work.` |
    | 5 | over = Build, rebuilds present, and one widget's self time ≥ 50 % of build | `WidgetBuild` | `{Widget} took {s} to build ({p}% of the build phase).` |
    | 6 | over = Build and rebuilds present | `Rebuilt` (the root with the most widgets) | `{Root} rebuilt {n} widgets, including {Child} ({m}).`, or `{Root} rebuilt {n} widgets.` |
    | 7 | samples cover the over phase's window | `HotFunction` | `{function} ran for most of the {phase} ({p}% of samples).` |
    | 8 | otherwise | `Phase(over)` | `{Phase} took {d}, {x} the budget.` For Build, add ` Turn on Trace widget builds to see which widgets rebuilt.` |

    - **Rule 7's function.** Among user-code functions, take those whose total samples in the window are within 90 % of the best. Pick the deepest by trie depth. If there is no user code, take the busiest function overall.
    - **Phase names in copy:** `build`, `layout`, `paint`, `compositing`, `frame finalization`, `UI work outside build, layout and paint`. Capitalise the first letter at the start of a sentence.
- [ ] **Step 1: Write the failing tests.**
  - `rule_1` through `rule_8`, one for each row, using writer-built snapshots.
  - `rules_are_ordered`: raster beats GC, and a widget beats rebuilt.
  - `cpu_state_cases`.
  - `huge_frame_and_long_names`: a 10 s frame with 100,000 widget builds and a 4 KB widget name finishes in under 50 ms (release) and cuts the name.
  - `spec_example_shape`: the Task 12 CartScreen fixture, with every widget slice taking 50 µs of self time, plus build over budget, gives `CartScreen rebuilt 195 widgets, including ProductGrid (193).`
- [ ] **Step 2: Write the budget test** `explain_budget` (release): `explain` on the stress session's 100 slowest frames, with p99 ≤ 50 ms. That includes `related` over 1M logs and 100k requests.
- [ ] **Step 3: Implement and verify.** Run `cargo test -p fdt_analysis` and the release budget run. Expected: PASS.
- [ ] **Step 4: Commit.** `feat(analysis): explain slow frames in one sentence`.

### Task 15: `fdt_ui`: viewport, batched canvas painters and M3 tokens

**Files:**
- Create: `crates/fdt_ui/src/viewport.rs` (pure), `crates/fdt_ui/src/canvas.rs`
- Modify: `crates/fdt_ui/src/tokens.rs`, `src/lib.rs`

**Interfaces:**
- **Consumes:** facts V1–V6.
- **Produces:**
  - **`viewport`.** Everything is in plain `u64` nanoseconds; `fdt_ui` does not depend on `fdt_store`.
    - `pub struct Viewport { start_ns: u64, span_ns: u64 }`.
    - `pub const MIN_SPAN_NS: u64 = 1_000`.
    - `Viewport::fit(start_ns, end_ns) -> Self`.
    - `x_of(t_ns, width) -> f32` and `time_at(x, width) -> u64`.
    - `zoom_about(x, width, factor: f32, bounds: (u64, u64))`.
      - A factor above 1 zooms in, and the time under `x` stays put.
      - The span is clamped to `[MIN_SPAN_NS, (bounds span) × 1.1]`.
      - `start` is clamped so the view keeps at least 5 % overlap with the bounds.
      - A factor that isn't finite or is ≤ 0 is ignored.
    - `pan_px(dx, width, bounds)` and `range() -> (u64, u64)`.
    - `pub fn scroll_zoom_factor(delta_y_px: f32) -> f32` is `exp(delta_y_px * 0.004)`. Positive means scrolling up, which zooms in.
    - `pub fn pinch_zoom_factor(delta: f32) -> f32` is `(1.0 + delta).clamp(0.2, 5.0)`.
  - **`canvas`**
    - `CANVAS_MAX_QUADS: usize = 20_000` and `CANVAS_MAX_LABELS: usize = 500`.
    - `pub struct Quad { bounds: Bounds<Pixels>, color: Hsla }`.
    - `pub fn paint_quads(window: &mut Window, layer: Bounds<Pixels>, quads: &[Quad])` paints one `paint_layer`. Anything past the cap is ignored and counted under `debug_assert`.
    - `pub struct Label { text: SharedString, origin: Point<Pixels>, max_width: Pixels, color: Hsla }`.
    - `pub fn paint_labels(window, cx, clip: Bounds<Pixels>, labels: &[Label])`:
      - It uses the mono font at `font::LABEL`.
      - It skips a label whose `max_width < layout::LABEL_MIN_WIDTH`.
      - It elides with `truncate_line(…, "…", TruncateFrom::End)` when the label doesn't fit.
      - It paints inside `with_content_mask(clip)`.
    - `pub fn zoom_pan_listeners(window: &mut Window, hitbox: &Hitbox, line_height: Pixels, on: impl Fn(ZoomPan, &mut Window, &mut App) + 'static)`.
      - `ZoomPan::{Zoom { x: Pixels, factor: f32 }, PanX(Pixels), PanY(Pixels)}`.
      - It registers `ScrollEvent` and `PinchEvent` listeners that act only in the Bubble phase while the hitbox is hovered.
      - ⌘/Ctrl+scroll and pinch → `Zoom`; horizontal delta or shift → `PanX`; vertical delta → `PanY`.
      - `Lines` deltas go through `pixel_delta(line_height)`.
  - **Tokens.**
    - `layout`:
      - `LANE_HEIGHT 24`, `LANE_LABEL_WIDTH 112`, `TRACK_ROW 18`, `FLAME_ROW 18`;
      - `FRAME_CHART_HEIGHT 140`, `FRAME_BAR_WIDTH 6`, `FRAME_BAR_MIN_WIDTH 1`;
      - `PHASE_BAR_HEIGHT 12`, `LABEL_MIN_WIDTH 32`, `TOOLTIP_OFFSET 12`, `TREE_INDENT 12`, `RULER_HEIGHT 20`.
    - `table`: `COL_FRAME 80`, `COL_MS 72`, `COL_COUNT 72`, `COL_PERCENT 64`, `COL_FUNCTION 360`, `COL_WIDGET 280`.
    - `alpha::PHASE: [u8; 5] = [0xFF, 0xC0, 0x99, 0x73, 0x4D]`, for build, layout, paint, composite and finalize/other, applied to `accent`.
- [ ] **Step 1: Write the failing tests.**
  - Viewport property tests over 1,000 random cases:
    - zooming in and back out by the same factor round-trips within 1 ns per 1 ms;
    - the time under `x` is fixed;
    - the span is always within its clamps;
    - factors of 0, ∞, NaN, 1e9 and 1e-9 never produce a NaN or zero span;
    - empty bounds `(0, 0)` keep `MIN_SPAN_NS`.
  - `pan_clamps_to_bounds`.
  - `scroll_and_pinch_factors`.
  - `paint_quads_smoke` and `labels_elide_or_skip` (`#[gpui::test]` with a test window; assert through a test hook counting painted labels).
  - `zoom_pan_listeners_bubble_only`: dispatch one `ScrollEvent`, and the callback fires once.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_ui` (token check included). Expected: PASS.
- [ ] **Step 3: Commit.** `feat(ui): time viewport, batched canvas painters and M3 tokens`.

### Task 16: App plumbing: routing, frame selection, strip budget, counts, banner, palette and actions

**Files:**
- Create: `crates/fdt_app/src/tools/performance/mod.rs`, `view.rs`, `crates/fdt_app/src/tools/timeline/mod.rs`, `view.rs`
- Modify:
  - `nav.rs`, `tools/mod.rs`
  - `shell/mod.rs`, `shell/tool_page.rs`, `shell/strip.rs`, `shell/counts.rs`, `shell/chrome.rs`
  - `palette/items.rs`, `palette/registry.rs`, `palette/view.rs`
  - `actions.rs`, `keymap.rs`, `perf.rs`, `test_support.rs`

**Interfaces:**
- **Consumes:** Tasks 2, 10, 11 and 15.
- **Produces:**
  - **`Selection::Frame(EntryId)`.** Every exhaustive match on `Selection` is updated.
  - **`pub struct OpenLocation(pub Location)`** is an event that any tool view can emit. `MainWindow` subscribes for the Logs, Network, Performance and Timeline views and calls `navigate`, which keeps a Back step.
  - **`PerformanceView`.**
    - `PerformanceView::new(session, window, cx)`.
    - `pub enum PerfTab { Frames, Cpu, Rebuilds, Trace }`, with `set_tab(PerfTab, cx)` and `set_location(range, selection: Option<EntryId>, cx)`.
    - The tab bar is `perf-tabs` (fact U7). Tab bodies are placeholders until Tasks 17–21.
    - The **debug banner** (`perf-debug-banner`) shows while the latest run's mode is `Debug`: "Debug mode timings aren't representative. Run the app with flutter run --profile for real numbers." It is persistent and non-blocking.
  - **`TimelineView`.** `TimelineView::new(session, window, cx)` and `set_location(range, cx)`. Its body is a placeholder until Task 22.
  - **Routing.**
    - `MainWindow` renders both views when they are routed: the store has frames, or an app is connected.
    - Timeline and Performance leave `tool_page`'s "not available yet" list, and its test is updated.
    - `snapshot_changed` resets both views on a new session.
  - **Counts.**
    - `range_counts` also returns the Performance count: the slow frames in range.
    - `attention` is set when any of those frames is `Janky`.
  - **Strip.**
    - Bars are coloured by `frame_status(max_us, latest_budget_us)`.
    - Height is `max_us / (2 × budget)`, capped at 1.
    - `strip_key` includes the budget.
  - **Palette.**
    - `items::frame_items(snap, query) -> Vec<PaletteItem>`. A query matching `^(frame\s*)?#?(\d+)$` gives "Frame #N" for the newest run that has that number.
    - `items::slow_frame_items(snap, limit = 20)` lists the slowest frames as "Frame #N · {t} · {status}".
    - Both are built in the existing background job, with `Navigate(Location { page: Tool(Performance), selection: Some(Frame(id)), range: None })`.
    - New commands are in the registry: the actions below plus the tab commands.
  - **Actions.**
    - `ShowFramesTab, ShowCpuTab, ShowRebuildsTab, ShowTraceTab, ToggleTraceWidgetBuilds, NextSlowFrame, PreviousSlowFrame, FetchCpuSamples, ZoomIn, ZoomOut, ZoomToFit, PreviousFrame, NextFrame`.
    - Keymap contexts: `LANES_CONTEXT = "Lanes"` and `FRAME_CHART_CONTEXT = "FrameChart"`, with the bindings in Global constraints.
  - **`perf::Counter`** gains `PerformanceView, FrameChart, TimelineView, Lanes`.
  - **`test_support::frame_at(run, number, start_ms, build_ms, raster_ms) -> FrameInput`.**
- [ ] **Step 1: Write the failing tests.**
  - `performance_and_timeline_route_when_connected`.
  - `not_built_tools_say_so_when_connected`, updated: only Memory, Widgets and State remain.
  - `new_session_resets_performance_and_timeline`.
  - `performance_count_counts_slow_frames_in_range`.
  - `strip_uses_the_refresh_budget`: at 120 Hz, a 10 ms frame is `Slow`; at 60 Hz the same frame is `Ok`.
  - `debug_banner_shows_only_in_debug`.
  - `frame_number_query_finds_the_frame`.
  - `slow_frame_items_are_slowest_first`.
  - `open_location_navigates_with_a_back_step`.
  - `registry_lists_m3_commands`: the commands are in the palette, menus and cheat sheet.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): route Performance and Timeline, frame selection, refresh-rate strip and M3 commands`.

### Task 17: Frames tab: the frame chart and the frames table

**Files:**
- Create: `crates/fdt_app/src/tools/performance/frame_chart.rs`, `frames_table.rs`
- Modify: `tools/performance/view.rs`

**Interfaces:**
- **Consumes:** Tasks 2, 15 and 16.
- **Produces:**
  - **`pub fn chart_bars(frames: &FramesSnapshot, rows: &[u32], first: usize, bar_px: f32, width_px: f32, budget_us: u32) -> Vec<BarGeom>`** (pure).
    - `BarGeom { x: f32, w: f32, h_frac: f32, status: FrameStatus, row: usize }`.
    - The y scale tops out at 3 × budget, and a taller bar is clipped and flagged.
    - When `bar_px < 1`, each pixel column shows the max of its frames. That keeps it to at most `width_px` bars.
  - **`FrameChart`.** A cached entity, `frame-chart`, in context `FrameChart`. It shows:
    - the bars, painted with `paint_quads`;
    - the budget line, labelled "{b} budget";
    - the selected frame, outlined in `accent`;
    - the hover tooltip, which lives in `PerformanceView`: "Frame #N · {t} · build {b} · raster {r}".

    Interaction:
    - A click selects a frame and emits `OpenLocation` with `Selection::Frame`.
    - A drag of at least `MIN_DRAG_PX` sets the shared range from the first to the last dragged frame.
    - Plain scroll pans by frames. ⌘/Ctrl+scroll and pinch change the bar width, between `FRAME_BAR_MIN_WIDTH` and 24 px.
    - ←/→ move the selection.
    - It follows the newest frame while recording until the user pans back. A "Latest" button resumes following.
  - **`FramesTable`.** A `DataTable` delegate, `frames-table`.
    - Columns: Frame, Time, Total, Build, Raster, Layout, Paint, Widgets, Status (text plus colour).
    - Rows are the frames in range, computed off the UI thread.
    - Toolbar toggles: "Slow only" and "Slowest first". The slowest-first sort runs in the background.
    - A filter input, `frames-filter`, keeps frames whose number contains the typed digits. This is §8.4's type-to-filter.
    - J/K and Enter work, and Enter opens details.
  - **Selection** syncs between the chart, the table and `Location`.
  - **States.**
    - Empty: "No frames yet. Frames appear as your app draws. Use the app to record some."
    - Disconnected: data stays.
- [ ] **Step 1: Write the failing tests.**
  - `chart_bars_cases`:
    - one bar per frame at 6 px;
    - pixel-max level of detail at 0.1 px, with ≤ `width_px` bars;
    - clipping at 3 × budget;
    - an empty input.
  - `click_selects_and_drag_sets_range` (simulated mouse on the chart bounds).
  - `arrow_keys_move_between_frames`.
  - `table_follows_range_and_slow_only`.
  - `filter_by_frame_number`.
  - `slowest_first_orders_by_time_us`.
  - `table_scroll_does_not_render_the_chart` (render counter = 0).
  - `follow_latest_stops_after_panning_back`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): frame chart and frames table`.

### Task 18: Frame details: phase breakdown, explanation, rebuilt widgets, busiest functions and Related

**Files:**
- Create: `crates/fdt_app/src/tools/performance/details.rs`
- Modify: `tools/performance/view.rs`

**Interfaces:**
- **Consumes:** Tasks 12–14, 16 and 17. `CaptureControl::fetch_cpu_samples` (Task 10).
- **Produces:**
  - **A resizable details pane, `frame-details`** (U6, `layout::DETAILS_*`), opened by selecting a frame.
    - The header is "Frame #N", a status chip and Close.
    - The pane is computed by `fdt_analysis::explain` in `cx.background_spawn`. It is keyed by `(snapshot.session, EntryId)` and recomputed when the snapshot changes the frame (its phases or the CPU coverage).
    - A skeleton shows only after `LOADING_DELAY`.
  - **Sections, in order:**
    1. **Breakdown.**
       - A phase bar (`PHASE_BAR_HEIGHT`). Segment widths are proportional to time.
       - Colours: `accent` with `alpha::PHASE[i]` for the UI phases, and `lane_network` for raster.
       - A text legend underneath: "Build 24.2 ms · Layout 0.0 ms · … · Raster 31.6 ms".
       - The over phase's legend entry reads "{Phase} {t}, over budget" in `danger`, with a 2 px `danger` underline on its segment.
       - With `has_timeline == false`, it adds: "No timeline detail for this frame."
    2. **Explanation:** `explanation.sentence`, at `font::BODY`, medium weight.
    3. **Most rebuilt widgets:** the top 10 (name, builds, self time). With no rebuilds, it shows "Turn on Trace widget builds to see which widgets rebuilt." and a "Trace widget builds" button.
    4. **Busiest functions:** the top 10, with a "your code" chip on user code.
       - `Pending` → "CPU samples for this frame haven't been fetched yet." and a "Fetch now" button.
       - `Off` → the reason.
       - `NoSamples` → "No CPU samples were taken during this frame."
    5. **Related:**
       - requests ("METHOD path · status · duration");
       - log lines ("time · level · first line").

       Each row emits `OpenLocation` with its tool and selection. The current range is kept.
- [ ] **Step 1: Write the failing tests.**
  - `selecting_a_janky_frame_shows_its_sentence` (stress-session frame).
  - `over_phase_is_labelled_in_text`.
  - `frame_without_timeline_says_so`.
  - `pending_cpu_offers_fetch_now`: a click logs `fetch_cpu` on the fake capture.
  - `rebuilds_button_turns_tracing_on`: logs `trace_builds true`.
  - `related_rows_navigate_with_the_range_kept`.
  - `new_session_clears_the_pane`.
  - `long_widget_names_elide`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): frame details with the slow-frame explanation`.

### Task 19: CPU tab: bottom-up, top-down and flame chart

**Files:**
- Create: `crates/fdt_app/src/tools/performance/cpu_tab.rs`, `flame.rs`
- Modify: `tools/performance/view.rs`

**Interfaces:**
- **Consumes:** Tasks 13, 15 and 16, and fact V7 (Tree).
- **Produces:**
  - **The toolbar, `cpu-toolbar`.** It holds:
    - a segmented switch: "Bottom up", "Top down", "Flame chart" (`cpu-view-*`);
    - the scope: "Selected range" or "Whole session", plus "{n} samples";
    - a filter input that keeps nodes whose name contains the text, plus their ancestors (off-thread);
    - "Fetch now".
  - **Trees** (`cpu-tree`): the `Tree` component, with items built off the UI thread from `top_down`/`bottom_up`.
    - Each row: indent `TREE_INDENT × depth`, name with library basename, a "your code" chip, then Self % and Total % (`COL_PERCENT`), then Self and Total time in ms (`COL_MS`), where ms = count × the sample period.
    - Item ids are the node path, so they are unique and stable.
    - Expansion is carried through `set_items`.
    - Bottom-up children are built on `TreeEvent::Expanded`.
    - At most 500 children per node, then a "{n} more" row.
  - **`FlameChart`.** A cached entity, `cpu-flame`, in context `Lanes`.
    - It draws `flame_spans` for the scope, with rows of `FLAME_ROW`.
    - Colours: user code `accent`, Dart `text_muted`, native `text_faint`. Labels go wherever they fit.
    - The tooltip shows the function, library, duration and samples. A click shows the same in a footer.
    - Zoom and pan come from the shared listeners and the zoom actions.
  - **Recompute** happens when the range or snapshot generation changes, throttled to 250 ms like the counts, and cancelling the previous job.
  - **States.**
    - No samples: "No CPU samples in this range. Samples are taken while your app's UI thread is busy."
    - Off: the reason.
    - Loading: a skeleton after `LOADING_DELAY`.
- [ ] **Step 1: Write the failing tests.**
  - `top_down_shows_roots_then_expands_with_arrows`.
  - `bottom_up_roots_are_sorted_by_self`.
  - `filter_keeps_matches_and_ancestors`.
  - `range_change_recomputes_and_cancels`.
  - `flame_paints_at_most_the_quad_cap` (pure layout fn `flame_layout(spans, viewport, width, rows) -> (quads, labels)`).
  - `profiler_off_shows_the_reason`.
  - `fetch_now_logs_fetch_cpu`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): CPU tab with call trees and a flame chart`.

### Task 20: Rebuilds tab and the "Trace widget builds" setting

**Files:**
- Create: `crates/fdt_app/src/tools/performance/rebuilds_tab.rs`
- Modify: `tools/performance/view.rs`, `shell/settings_page.rs`

**Interfaces:**
- **Consumes:** Tasks 11, 12 and 16.
- **Produces:**
  - **The toolbar.**
    - A switch, `rebuilds-trace-toggle`, labelled "Trace widget builds", with the note "Adds about 90 % to build time while on." beside it.
    - It reflects `RunInfo.tracing`, the actual state, and calls `CaptureControl::set_trace_widget_builds`.
    - It is disabled when no app is connected, and its tooltip then says "Connect to an app to trace widget builds."
    - `ToggleTraceWidgetBuilds` does the same.
  - **The scope** is "Selected frame" when a frame is selected (`frame_rebuilds`), otherwise "Selected range" or "Whole session" (`range_rebuilds`, off-thread and cancellable, with a skeleton).
  - **The table**, `rebuilds-table` (DataTable).
    - Columns: Widget (`COL_WIDGET`), Builds, Frames, Max in a frame, Self time (`COL_COUNT`/`COL_MS`).
    - Sorted by Builds, descending.
    - A filter input, `rebuilds-filter`, keeps widgets whose name contains the text, ignoring case.
  - **The cap note.** When `timeline.drops().build_cap > 0`, it shows "Build detail stopped after 8,000,000 widget builds. Counts per frame are kept."
  - **States.**
    - Tracing off and no data: "Widget builds aren't traced. Turn on Trace widget builds to count rebuilds per widget.", plus the switch.
    - On with none in scope: "No widgets rebuilt in this range."
  - **Settings page.** A "Trace widget builds by default" switch with the same overhead note, bound to `Settings.trace_widget_builds`.
- [ ] **Step 1: Write the failing tests.**
  - `toggle_calls_capture_and_reflects_actual_state`.
  - `toggle_disabled_when_disconnected`.
  - `frame_scope_lists_that_frames_widgets` (the Task 12 fixture).
  - `range_scope_aggregates`.
  - `filter_by_widget_name`.
  - `cap_note_shows_after_drops`.
  - `settings_switch_persists`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): Rebuilds tab and the trace widget builds setting`.

### Task 21: Raw trace tab

**Files:**
- Create: `crates/fdt_app/src/tools/performance/trace_tab.rs`
- Modify: `tools/performance/view.rs`

**Interfaces:**
- **Consumes:** Tasks 3, 15 and 16.
- **Produces:**
  - **`pub fn trace_layout(snap: &Snapshot, viewport: Viewport, rows: &[TraceRow], scroll_y: f32, size: (f32, f32)) -> TraceGeometry`** (pure).
    - `TraceRow { track: TrackId, depth: u16 }`, listed for the expanded tracks.
    - `TraceGeometry { quads: Vec<(Rect, ColorKey)>, labels: Vec<(Rect, NameId)>, ruler: Vec<(f32, String)> }`.
    - Only visible rows are drawn. A row draws raw slices when `range_indices(...).len() ≤ CANVAS_MAX_QUADS / visible_rows`, and otherwise draws `cells(...)`, whose alpha is scaled by coverage.
    - Labels go on raw slices of at least `LABEL_MIN_WIDTH`, at most 500 in all.
    - Colour keys: `Dart` → `accent`, `Embedder` → `lane_network`, `Gc` → `text_muted`, `Other` → `text_faint`.
  - **`TraceView`.** A cached canvas entity, `trace-canvas`, in context `Lanes`.
    - On the left, a fixed label column (`LANE_LABEL_WIDTH`) with track names. A click on a name collapses or expands that track's deeper rows.
    - A ruler of `RULER_HEIGHT`. Dragging on the ruler sets the shared range; double-clicking clears it.
    - Input:
      - `PanY` scrolls the rows;
      - `PanX` and shift pan time;
      - `Zoom`, ⌘=/⌘-/⌘0 zoom.
    - The hover tooltip shows the name, duration, start offset, track and category.
    - A click selects a slice and shows a footer with the same fields plus the depth.
    - The viewport fits the shared range whenever the range changes, and the whole session otherwise.
  - **The empty state:** "No timeline recorded yet. Timeline events appear while your app draws frames."
- [ ] **Step 1: Write the failing tests.**
  - `layout_switches_to_cells_when_dense`: the stress session at full zoom out stays ≤ 20,000 quads and ≤ 500 labels.
  - `layout_draws_raw_slices_when_zoomed_in`.
  - `deep_rows_stop_at_the_store_cap` (Review focus 3).
  - `ruler_drag_sets_range_and_zero_width_drag_selects`.
  - `zoom_keys_change_the_viewport`.
  - `tooltip_text`.
  - `empty_state`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): raw trace tab`.

### Task 22: Timeline page with lanes

**Files:**
- Create: `crates/fdt_app/src/tools/timeline/lanes.rs`
- Modify: `tools/timeline/view.rs`

**Interfaces:**
- **Consumes:** Tasks 2, 3, 5, 15 and 16.
- **Produces:**
  - **Pure lane layouts.** Each takes `(snap, viewport, width_px, budget)` and returns quads, labels and hit targets `Vec<(Rect, LaneHit)>`:
    - `frames_lane`: bars per frame when zoomed in, otherwise `lod_cells` of the frames level of detail; status colours; height ∝ time / (2 × budget).
    - `network_lane`: `lane_cells(net.lod())` in `lane_network`, or `danger` when `max_us == 1`. When the cells' summed count is ≤ 2,000, it draws request spans from a background `filter_requests` over the viewport instead, packed into 3 rows.
    - `logs_lane`: `lane_cells(logs.lod())`, coloured by level index (0–1 `text_muted`, 2 `warning`, 3 `danger`).
    - `navigation_lane`: `accent` markers with the route name label where it fits.
    - `gc_lane`: `lod_cells(timeline.gc_lod())` in `text_muted`.
    - `findings_lane`: `danger` markers for janky frames, labelled "Jank".
    - `LaneHit::{Frame(usize), Request(u32), LogCell(TimeRange), Navigation(u32), Gc(TimeRange), Finding(usize)}`.
  - **`TimelineView`.**
    - It stacks a ruler and the lanes, with names in the label column. Lanes with no data are hidden, except Frames.
    - Each lane is its own cached entity (V6). The tooltip lives in `TimelineView`.
    - Input: zoom, pan, `Lanes` keys, drag-to-range and double-click-to-clear.
    - **A click** emits `OpenLocation` with the current range kept:
      - a frame or finding → `Performance` with `Selection::Frame`;
      - a request → `Network` with `Selection::Request`;
      - a log cell → `Logs` with the first record at or after the cell start, found in a background `filter_logs` over the cell's range;
      - navigation → `Logs` with that record.
    - **Tooltips:**
      - a frame: "Frame #N · {t} · {status}";
      - a request cell: "{n} requests ({e} failed)";
      - a request span: "GET /path · 200 · 182 ms";
      - a log cell: "{n} lines, {e} errors";
      - navigation: the route;
      - GC: "Garbage collection · {d}";
      - a finding: "Janky frame #N".
  - **The empty state** (no app and no data): "Nothing recorded yet. Connect to an app to fill the timeline."
- [ ] **Step 1: Write the failing tests.**
  - `lane_layouts_fit_the_width` (the stress session, each lane ≤ width cells).
  - `network_lane_shows_spans_when_sparse`.
  - `clicks_map_to_locations_with_range_kept`, one per `LaneHit`.
  - `empty_lanes_hide`.
  - `drag_sets_range_and_double_click_clears`.
  - `extreme_zoom_is_clamped_and_finite` (Review focus 5: pinch ±5, `Lines` deltas, zoom past 1 µs and past the session).
  - `growing_session_keeps_the_viewport_when_zoomed_in`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): Timeline page with frames, network, logs, navigation, GC and findings lanes`.

### Task 23: Cross-links in Logs and Network

**Files:**
- Modify: `crates/fdt_app/src/tools/logs/details.rs`, `tools/logs/view.rs` (row timestamp link), `tools/network/details.rs`

**Interfaces:**
- **Consumes:** Task 12 `correlate` and Task 16 `OpenLocation`.
- **Produces:**
  - **Logs details** gain a "Related" block (`logs-related`) after the meta block.
    - "Frame #N · {status}" links to Performance.
    - Up to 5 "Requests in flight" link to Network.
    - "No frame at this moment." shows when there is none.
  - **A log row's timestamp** is a link (`logs-row-time-{seq}`, pointer cursor). It opens the frame at that time, and is plain text when there is none.
  - **Network details** gain a collapsible "Related" section (`net-section-related`) after Timing.
    - "Frame that processed the response: #N".
    - Up to 5 "Log lines during this request".
  - **All of it** is computed in `cx.background_spawn` and keyed by `(snapshot.session, EntryId)`. It never blocks the pane's first draw.
- [ ] **Step 1: Write the failing tests.**
  - `log_details_link_to_the_frame`.
  - `log_row_time_opens_the_frame`.
  - `request_details_link_to_the_frame_after_the_response`.
  - `links_keep_the_range_and_add_a_back_step`.
  - `no_frame_copy`.
- [ ] **Step 2: Implement and verify.** Run `cargo test -p fdt_app`. Expected: PASS.
- [ ] **Step 3: Commit.** `feat(app): cross-links from logs and requests to frames`.

### Task 24: `fdt bench-ui` scenarios for M3

**Files:**
- Modify: `crates/fdt_app/src/bench_ui.rs`, `crates/fdt_app/src/perf.rs`

**Interfaces:**
- **Consumes:** Tasks 1, 4 and 16–22.
- **Produces:** New `Scenario` variants, with `name()`, `home()`, `default_len()`, `latency_budget_ms()`, `ready()` cases and `USAGE` updated:
  - `frame-chart`: pan through all 72,000 frames, then zoom the bar width in and out.
  - `frame-details`: select the 50 slowest frames in turn. Latency is the time until the sentence is drawn, with a budget of 50 ms.
  - `timeline-zoom`: zoom from the whole session to a 10 ms span and back, then pan.
  - `trace-zoom`: the same on the Raw trace tab.
  - `flame-zoom`: the same on the CPU flame chart, scoped to a 60 s range.
  - `cpu-tree`: expand the top-down tree 6 levels, then scroll through it.
  - `live-perf`: like Task 1's `live-logs`, but each publish appends 2 frames with phases and their about 140 slices. The Frames tab follows.

  `tool-switch` now cycles through Logs, Network, Performance and Timeline.
- [ ] **Step 1: Write the failing tests.**
  - `scenario_names_round_trip`.
  - `ready_knows_performance_and_timeline`.
  - `frame_details_latency_is_measured_until_the_sentence_is_drawn`, using a test-mode driver over 3 frames.
- [ ] **Step 2: Run each new scenario 3 times** (`uptime` first) with `cargo run --release -p fdt_app -- bench-ui <name>`. Put the medians in the report. They are judged in Task 26, and a miss is reported, not hidden.
- [ ] **Step 3: Commit.** `feat(app): bench-ui scenarios for Performance and Timeline`.

### Task 25: Live `jank_lab` test for performance and the CI job

**Files:**
- Modify: `crates/fdt_capture/tests/jank_lab_live.rs`, `crates/fdt_capture/Cargo.toml` (dev-dep `fdt_analysis`), `scripts/jank-lab-it.sh`, `.github/workflows/ci.yml`

**Interfaces:**
- **Consumes:** Everything above, plus fact Q11.
- **Produces:** In profile mode, after M2's checks, these steps run:
  1. **Jank.** `goto_jank`, wait 3 s, then `fetch_cpu_samples`.
     - At least 20 frames have `ui` and `raster`.
     - On the slowest janky frame, `explain` gives `Cause::HotFunction` whose function contains `_HeavyBuildPanelState.build`.
  2. **Jank with tracing.** `set_trace_widget_builds(true)` and wait 2 s. A new janky frame gives `Cause::WidgetBuild` with widget `HeavyBuildPanel`.
  3. **Raster.** Turn tracing off, `raster_on`, wait 3 s. A janky frame with raster over budget gives `Cause::Raster { save_layers ≥ 10 }`.
  4. **Rebuilds.** `raster_off`, `home`, `goto_rebuilds`, tracing on, wait 2 s. Some frame's `frame_rebuilds` has root `CartScreen` with `largest_child.0 == "ProductGrid"`, and `ProductTile` builds = 64.
  5. **Restore.** Disconnect as the user would. A second plain `VmService` connection then reads `profileWidgetBuilds == false`.

  In debug mode the test only checks that frames get `ui` and `raster`. The script exports `JANK_LAB_FLOAT=1`. CI is unchanged apart from the new assertions running in its existing `jank_lab_live` matrix.
- [ ] **Step 1: Write the test.** Then run `scripts/jank-lab-it.sh profile` and `scripts/jank-lab-it.sh debug` on macOS. Expected: PASS. Paste the raw timings and the asserted sentences into the report.
- [ ] **Step 2: Commit.** `test(capture): live jank_lab checks for frame phases, explanations, rebuilds and restore`.

### Task 26: M3 verification and review record

**Files:**
- Create: `docs/milestones/m3-review.md`
- Modify: `docs/milestones/m2-review.md` (fill in the measurements it left open), `README.md` (what Performance and Timeline do, with one line each)

**Interfaces:**
- **Consumes:** everything.
- **Produces:** the record, in M2's format. It covers:
  - what shipped;
  - the budgets table with raw numbers;
  - the calls made;
  - open decisions;
  - the author checklist.
- [ ] **Step 1: Run the release budget tests.** `cargo test --workspace --release -- budget --nocapture` with `FDT_BUDGETS=strict`, on an unlocked, quiet R1.
- [ ] **Step 2: Run bench-ui.** Run every scenario (M2's and M3's) 3 times, with `uptime` before each batch, and record the medians of draw p99, interval p99 and latency. These include M2's never-measured runs.
- [ ] **Step 3: Measure the overhead** with `frame_build_sampler` on `/jank` and `/rebuilds` in profile mode, using `JANK_LAB_FLOAT=1`:
  - 6 interleaved rounds × 150 frames of fdt connected (`fdt --connect`, default settings) against not connected;
  - median, p90 and the paired median ratio for each;
  - judged against ≤ 2 %;
  - tracing on, measured once and labelled.
- [ ] **Step 4: Measure idle and attach.** With fdt connected to an idle `jank_lab` (home screen):
  - RSS and CPU over 30 one-second samples, after a 30 s warm-up;
  - attach to first live data, 5 runs.
- [ ] **Step 5: Write `m3-review.md`.** Every number traces to a raw run pasted in an appendix. Misses are stated with the dominant cost; budgets are not loosened. Open decisions carry M2's unresolved ones (⌘N, draw p99 for Logs search, the token in settings), plus any M3 misses. The checklist includes:
  - a visual review of the Performance tabs and the Timeline page in both themes, on macOS, Windows and Linux;
  - checking thread and raster slice names on Windows and Linux (fact Q12);
  - checking the refresh-rate budget on a 60 Hz display.
- [ ] **Step 6: Commit.** `docs: M3 verification record`.
