# M3 verification results and review checklist

Branch `m3-performance`, built from the commit that precedes this record (`c145993`, plus the `cpu_poll` default change described under "CPU polling" and the `p99` column added to `frame_build_sampler`). R1: Apple M3 Max, macOS, 14 cores, 120 Hz display, release build. Run on the night of 21 November 2026 (00:59 to 01:55 local). Corrected after its review the same day: the miss count, the load during the second overhead run, which statements are hypotheses, the memory units and the attach comparison. Nothing was re-measured; the fix round added two unit tests.

**Final fix wave (after the whole-branch review, 21 November 2026).** The three-reviewer whole-branch review found no critical issue; its important findings were fixed in one wave (commits after `d77def3`). Where a fix changes a figure in this record, the figure is updated and marked "(final fix wave)"; a figure the wave could change but did not re-measure is marked "pending re-measure". The rest of this record is the Task 26 verification as it was. After the wave: `cargo test --workspace` 902 passed, 0 failed, 12 ignored; `FDT_BUDGETS=strict cargo test --workspace --release -- budget` 28 passed (the two new ones are a contrast test and a capture test whose names contain "budget"); the live `jank_lab` test passed on the second of two profile runs (the first judged 0 frames with tracing on: no janky frame with phases arrived in its window).

**Timeline clicks (ruling R22, after the wave's re-review).** A click on a Timeline lane opens its target at once, on mouse-up. A double-click clears the range only on the Timeline's ruler and on the session strip, as the plan and spec say; on a lane it is two clicks. (The wave had delayed every lane click by 500 ms so that a double-click on a lane could clear the range instead.)

Legend: met, missed, not resolvable (the method cannot see the budget's margin), not measured (and why).

## Summary and status

M3 is built: frame ingest with phases, timeline ingest with level-of-detail lanes, CPU samples, widget-build tracing, the analysis crate (phase breakdown, rebuilds, correlation, the slow-frame sentence), the Performance tool (Frames, CPU, Rebuilds and Raw trace tabs), the Timeline page with lanes, cross-links between tools, and the bench-ui scenarios for all of it. All automated checks are green.

What this verification found:

- **Met:** every release budget test (26 tests, strict); frame p99 and no-frame-over-16.7 ms for all pan and zoom scenarios; the overhead on the profiled app on `/jank` (no increase detectable: +0.22 % and -0.01 % in two runs); idle CPU (max 0.5 %, from a window that was not drawing); the Timeline ingest rate (3.41 M events/s against 1 M; that test timed a typed decoder the poller did not use, and after the final fix wave the poller uses it and the test times it: 3.52 M events/s); the CPU tree (1.4 ms against 100 ms).
- **Not resolvable:** the overhead on `/rebuilds`, whose build is about 1 ms and varies by 10 % between rounds, so 2 % cannot be seen on this machine.
- **Missed, with the dominant cost:** draw p99 on five scenarios, by the median of three runs: `logs-search` and `tool-switch` (M2's open decision 1, unchanged), `strip-drag` (3.4 ms in M2), and the new `frame-details` and `trace-zoom` (`net-filter` missed in one run of three and is met on the median); the Logs search keystroke latency in milliseconds (19.6 against 16, still inside 2 frames); idle memory of the connected window, 153.3 MiB (160.8 MB) against 150 MB, a miss in either unit (measured with a window that was not presenting frames).
- **Changed in code:** the default `cpu_poll` is now 15 s (ruling R6, below), as a precaution. One of two overhead runs crossed the 20 µs median line, and that run had an 80 to 110 ms frame in every connected sample. That a CPU-sample fetch causes that frame is a hypothesis: the other run had such a frame in 1 of 6 samples, and no run logged fetch times. The re-measure at 15 s is pending.
- **Not measured, because the Mac's screen locked partway through** (`CGSSessionScreenIsLocked = 1` at some point between 01:30 and 01:33, checked again at 01:35, 01:42, 01:45, 01:54 and 01:55; the display had gone to sleep, and `jank_lab` stopped drawing, so no frames reached it or `fdt`): the 15 s re-measurement of the CPU poll, the 600-frame CPU-poll run (one of four rounds got its bare half before the lock), the widget-build tracing overhead, the quiet-machine re-run of the draw misses, and the five attach-to-data runs, along with the in-window part of attach to data (only `attach_ms` was taken, once). Nothing is estimated; see "Not measured".

## Automated checks (all green)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass, no warnings |
| `cargo test --workspace` | 862 passed, 0 failed, 12 ignored (the ignored ones include the live `jank_lab` test) |
| `FDT_BUDGETS=strict cargo test --workspace --release -- budget --nocapture` | 26 passed (4 `fdt_analysis`, 12 `fdt_app`, 1 `fdt_capture`, 9 `fdt_store`), 0 failed |
| `jank_lab` live test | run once against a profile app with the screen locked: connected (`attach_ms=126`), then failed at its first `goto_logs` because the app was not drawing. Not counted. It passed in Task 25 on an unlocked machine (profile and debug). |

## Measured budgets

### Release budget tests (Step 1)

`uptime` before: load 11.82 12.28 13.87; after: 13.88 12.75 13.99. The machine was busy, as it was for most of the earlier tasks; these tests are single-threaded CPU work and have wide margins. Raw output: appendix A.

| Measure | Budget | Measured | Verdict |
|---|---|---|---|
| Timeline ingest: decode and assemble (199,990 events) | >= 1,000,000 events/s | 3.41 M events/s (decode and feed p50 58.7 ms). Final fix wave: the poller now decodes with that typed pass (`decode_timeline_reply`; before, it parsed a `serde_json::Value` tree, which reviewer A measured at 1.20 M events/s), and the test times the shipped decoder: 3.52 M events/s (p50 56.8 ms; strict release run, load 2.3) | met |
| CPU call tree, 450,000 samples (3,000 call sites) | <= 100 ms | top-down p50 0.23 ms, bottom-up p50 1.41 ms; flame chart, 10 s range, 100,612 spans, p50 4.6 ms | met |
| Open slow-frame details, including the sentence | <= 50 ms | `explain`, 100 slowest frames: p50 1.21 ms, p99 1.68 ms, max 3.65 ms (analysis only; the pane's draw is in the bench-ui table). Final fix wave (every BUILD in the frame is walked now; strict release run, load 2.3): p50 1.20 ms, p99 1.51 ms, max 5.71 ms | met |
| Timeline lane layout, 6 lanes, 72k frames | (internal) | median 0.09 ms, worst 0.26 ms | met |
| Lane query over 6 UI lanes, 5M slices | (internal) | p90 0.021 ms, worst 0.053 ms | met |
| 10,000 slices plus publish | (internal) | p90 0.79 ms, worst 0.85 ms | met |
| `strip_bars`, 72k frames | (internal) | worst 0.0067 ms | met |
| Log search, first results over 1M lines | <= 16 ms | worst 0.2 to 3.1 ms | met |
| Log search, full scan | <= 100 ms | worst 2.4 to 40.7 ms | met |
| Network filter over 100k requests | <= 16 ms | worst 0.36 to 1.02 ms | met |
| Palette per keystroke, 5,000 items | <= 8 ms | worst 1.71 ms | met |

### `fdt bench-ui` (Step 2): every scenario, 3 runs

`uptime` before each batch: 1:00 load 11.37 12.23 13.76; 1:01 load 11.14 12.02 13.57; 1:02 load 9.22 11.34 13.20. The display refreshes at 8.333 ms (120 Hz, reported by the display); the interval budget is therefore 8.833 ms (ruling R16 of M2: the larger of the plan's and one refresh period, plus 0.5 ms). The screen was unlocked for all three batches. The load average was already 11 to 14 before I started anything, and a `top` sample taken right after the batches showed no process above 0.1 % CPU, so I could not identify its source; the numbers may carry noise from other activity on the machine (9 login sessions).

Medians over the 3 runs of draw p99 (budget 4 ms), interval p99 (budget 8.833 ms), and the latency p99 for the interactive scenarios. "Pass" is the report's own `pass` field, which also checks the maximum interval (16.7 ms) and, for search and switch, the latency in ms and frames.

| Scenario | Draw p99 ms (runs) | Interval p99 ms | Latency p99 ms (budget) | Pass | Verdict |
|---|---|---|---|---|---|
| logs-scroll | 3.13 (2.90, 3.13, 3.17) | 8.41 | | 3/3 | met |
| logs-search | **6.33** (6.63, 6.33, 6.08) | 8.56 | first result 19.6 (16; 2 frames, budget 2), full result 42.4 ms, 5 frames (budget 100 ms, 12 frames) | 0/3 | **missed** (draw and ms latency) |
| net-scroll | 3.29 (3.35, 3.21, 3.30) | 8.41 | | 3/3 | met |
| net-filter | 3.40 (4.21, 3.40, 3.40) | 8.47 | 11.8 (16) | 2/3 | met on the median; run 1 missed on draw (4.21) |
| tool-switch | **7.08** (13.30, 7.08, 6.63) | **11.51** (33.4, 8.39, 11.51) | 16.8 (50) | 0/3 | **missed** (draw; interval p99 in 2 of 3 runs: run 1 ran at about 30 Hz throughout, run 3 had one slow interval) |
| strip-drag | **4.28** (error, 4.17, 4.38) | 8.66 | | 0/3 | **missed**; run 1 errored with "the drag did not select a time range on the strip" |
| live-logs (new, Task 1) | 2.78 (2.62, 2.92, 2.78) | 8.44 | | 3/3 | met |
| frame-chart | 3.05 (3.01, 3.05, 3.12) | 8.65 | | 3/3 | met |
| frame-details | **4.51** (5.23, 4.31, 4.51) | 8.40 | 15.4 (50; 1 frame) | 0/3 | **missed** (draw, by 0.5 ms) |
| timeline-zoom | 2.26 (2.26, 2.13, 2.34) | 8.73 | | 3/3 | met |
| trace-zoom | **4.30** (3.39, 4.30, 4.33) | 8.61 | | 1/3 | **missed** (2 of 3 runs, by 0.3 ms) |
| flame-zoom | 2.66 (2.58, 2.66, 2.77) | 8.63 | | 3/3 | met |
| cpu-tree | 3.70 (3.81, 3.43, 3.70) | 8.42 | | 3/3 | met |
| live-perf (new, Task 24) | 3.83 (3.64, 3.86, 3.83) | 8.42 | | 3/3 | met |

Notes:

- The pan and zoom budget (frame p99 <= 8.3 ms, no frame over 16.7 ms) holds for every scroll and zoom scenario: interval p99 8.4 to 8.7 ms against the 8.833 ms line, and the largest single interval in those scenarios was 16.6 ms (net-scroll run 1). `tool-switch` is not a pan or zoom scenario; its first run is described below.
- **The latency scenarios have 18 to 90 samples**, so their "p99" is close to the maximum. Treat them as indicative.
- **`logs-search`:** the draw p99 is M2's open decision 1, unchanged at about 6.3 ms (M2: 6.5). The full-result latency is met (5 frames against 12). The first-result latency in milliseconds is 19.6 ms against 16, but 2 frames against a budget of 2 (R17: keystroke latency is judged in frames), so the report fails only on the ms line. M2's record called this met (first results within 2 frames); the ms line is stricter than what M2 reported.
- **`tool-switch`** now includes Performance and Timeline in the switch list. Draw p99 7.1 ms is the same figure M2 recorded (6.9) and the same cause (full-page text reshaping). The interval-p99 misses in 2 of 3 runs have different shapes. Run 1 ran at about 30 Hz throughout: interval p50 33.3 ms and p99 33.4 ms, so every frame took about four refresh periods, and its draw p99 (13.3 ms) and latency p99 (46.7 ms) are also far from runs 2 and 3 (cause not investigated). Run 3 had one slow interval (11.5 ms) in a short scenario (41 frames; its p95 is 8.36 ms).
- **`frame-details`** was 4.68 ms median at the end of Task 24b on a machine with load 10 to 15 (5.37 before R10). On this run it is 4.51 ms median (5.23, 4.31, 4.51), at load 9 to 11, a little better but still over. Dominant cost, as Task 24b measured it: the frame where the pane's full draw (about 1.5 ms after R10, text shaping of the Related rows) coincides with the frames table's scrollbar-animation re-render (about 1.8 ms); prepaint (layout plus shaping) is the bulk (p99 2.9 ms). **The "quietest run" re-measure was not possible:** by the time the machine's load had fallen to 2 to 5 (01:07 onward) every `bench-ui` batch had finished, and when I wanted to repeat the misses at that load the screen was locked (see "Summary"). So this is an open decision (number 2 below), not an accepted pass.
- **`trace-zoom`** is a new miss, not seen by earlier tasks: 3.39, 4.30, 4.33. By 0.3 ms; the same load caveat applies.
- **`strip-drag`:** run 1's error is intermittent and is a harness result, not a draw time. Cause not investigated. Runs 2 and 3 are 4.17 and 4.38 ms against 4 (M2 recorded 3.4 ms after Task 21b), so the strip's draw time is also up since M2. The strip changed in M3 only in how it judges its bars (against the run's refresh-rate budget instead of a fixed 16.7 ms); its budget line is M2's. The cause of the rise is not investigated.

### Overhead on the profiled app (Step 3, <= 2 % added to frame build time)

Setup: `jank_lab` built in profile mode (`flutter run -d macos --profile`, `JANK_LAB_FLOAT=1`), driven to `/jank` and `/rebuilds` with `ext.jank_lab.run` (`goto_jank`, `goto_rebuilds`). `frame_build_sampler` (a second VM client on the `Extension` stream only) collects 150 `Flutter.Frame` build times per sample. "conn" is the real `fdt --connect <vm uri>` window, default settings (tracing off, `cpu_poll` 5 s at the time of this run), connected for 10 s before sampling; "bare" is the sampler alone. Six interleaved rounds (bare first on odd rounds, conn first on even rounds), the window started and killed each round. Raw: appendix C.

Run 2 (the recorded run; a second `jank_lab` instance; 1-minute load 11.3 falling to 5.5 during the `/jank` rounds, and 3.8 falling to 2.0 during the `/rebuilds` rounds):

| Page | bare median of medians (us) | conn median of medians (us) | bare / conn median p90 (us) | paired median ratio conn / bare | Verdict |
|---|---|---|---|---|---|
| `/jank` (24 ms build) | 24,060 | 24,072 | 24,104 / 24,112 | **0.9999** (rounds 0.9954 to 1.0010) | met (no increase detectable) |
| `/rebuilds` (about 1 ms build) | 974 | 886 | 1,735 / 1,450 | 0.9415 (rounds 0.88 to 1.09) | **not resolvable at 2 % on this machine** (see the note) |

Run 1 (`/jank` only, a first `jank_lab` instance, load 2.8 to 3.2): bare 24,083, conn 24,140, paired median ratio **1.0022** (+0.22 %; median delta +52 us), and each connected round had one build of 79 to 112 ms (appendix C, first run). That run is why the CPU-poll default changed (below). On `/jank`, both runs are well under 2 %.

Notes:

- The `/jank` build is long and steady (p90 within 0.4 % of the median), so the 2 % budget is resolved there to a few tenths of a percent. On `/rebuilds` the build is about 1 ms and varies by 10 % between rounds, so 2 % (20 us) cannot be resolved; the connected samples were faster (ratio 0.94), which is noise, or the Mac running faster with more going on. This says only that no increase was seen, so the verdict is "not resolvable", not "met".
- **The single long frame in connected rounds may come from a CPU-sample fetch; that is a hypothesis, not a finding.** It appeared in 6 of 6 connected rounds in run 1 (79 to 112 ms) and in 1 of 6 in run 2 (94 ms), with the same 5 s poll and the same procedure. A 150-frame sample spans about 3.8 s, so if every 5 s fetch cost such a frame, most samples in both runs would show one; run 2 does not. No run logged when fetches happened, so no long frame is tied to a poll, and no other cause was looked for. In bare rounds the maximum was 24.1 to 27.4 ms. The long frame does not move the median or p90; if it is a fetch, it is one visibly dropped frame every poll.
- Two overhead runs in the same configuration disagreeing (median +52 us, then -2 us) shows the noise floor of this method; the long frames, whatever causes them, may also depend on how long the app has been running (the second instance had been up about 2 minutes).
- **Two run-2 samples took far longer than the others:** `/jank` connected round 6 took 35.6 s for its 150 frames and `/rebuilds` bare round 3 took 65.4 s, against 2.3 to 4.7 s for every other sample (and 8.4 s for run 1's first bare sample). Their frame numbers are consecutive (5614 to 5763 and 9028 to 9177), so the app drew no frames for about 32 s and 63 s inside those samples; the sampler did not miss frames. Their build times are in line with the other rounds. The cause was not investigated (possibly the not-drawing behaviour described under call 14). Both samples are kept in the medians.
- Widget-build tracing on (R12, `profileUserWidgetBuilds`, "about +29 %"): **not measured here.** The +29 % is from the planning spike (Q5.3), measured on a different screen. The measurement was planned for after the CPU-poll runs and the screen locked first. The figure must be labelled "from the spike, not re-measured in M3 verification".

### CPU polling (ruling R6, Task 10 Step 3)

Rule: measure the `/jank` build median and p99 with `cpu_poll` at 5 s against not connected; if the median delta is over 20 us or the p99 delta is over 1 ms, make 15 s the default, re-measure, and record both runs.

| Run | median delta (conn - bare) | p99 delta | max in connected rounds |
|---|---|---|---|
| Run 1, 6 x 150 frames (no p99 column yet; p90 delta +34 us) | **+52 us** (rounds +59, +59, +12, -14, +122, +46) | n/a | 79 to 112 ms in every connected round |
| Run 2, 6 x 150 frames | -2 us | -3 us | 94 ms in 1 round |
| Planned 600-frame run, 4 rounds | not completed: round 1's bare half only (median 24,527 us, p99 26,199 us, max 36,981 us, 600 frames in 16.2 s); the screen locked before its connected half | | |

Run 1 crosses the median line, so under R6's rule the default is now **15 s** (`crates/fdt_capture/src/config.rs`, pinned by a unit test). This is a precaution: run 2 crossed neither line, and that the long frames come from fetches is a hypothesis (see the notes above). **The re-measurement at 15 s was not possible** (screen locked), so it is open: decision 4, and the "Pending verification" list. If the hypothesis holds, a hitch comes at most every 15 s while frames are drawn. The VM keeps about 40 s of samples, so nothing is lost; the cost is that the CPU samples for a slow frame arrive up to 15 s late (the Frame details pane already shows its "Pending" state and "Fetch now").

### Idle memory and CPU, and attach (Step 4)

**Real window, connected, app idle on the home screen: measured, but with the screen locked** (`CGSSessionScreenIsLocked = 1`), so the window was not presenting frames. `fdt --connect <vm uri>` (the capture host installs from a timer since Task 1, so it connects without frames; the 54 MiB difference from the unconnected window below says it did), 30 s warm-up, 30 one-second `ps -o rss=,%cpu=` samples. `uptime` before 1:40, load 5.62 4.65 5.40. Raw: appendix E.

Units: `ps` reports RSS in KiB (1,024 bytes), and the figures here are KiB / 1,024 = MiB, with decimal MB where the budget is compared. The spec writes the budget as "150 MB" without saying which; the miss holds under either reading. (The first version of this record divided KiB by 1,000, as M2's record did.)

| Measure | Budget | Measured | Verdict |
|---|---|---|---|
| Idle RSS, connected | <= 150 MB | 152.7 to 153.3 MiB (156,352 to 156,992 KiB; 160.1 to 160.8 MB), flat across the 30 samples | **missed**, by 3.3 MiB (2.2 %) if the budget is 150 MiB, or by 10.8 MB (7.2 %) if it is 150 decimal MB; with a window that drew no frames, and a window that draws can only add to it |
| Idle CPU, connected | <= 1 % | 0.0 to 0.5 %, mean 0.11 % | met for a window that was not drawing (a locked window draws nothing, so a drawn idle window may cost more; GPUI redraws only on change) |

Where the 153 MiB comes from (same night, same machine, release builds, 15 s settle):

| Process | RSS |
|---|---|
| `fdt` at M2 (`1b9cd1f`), window only, not connected | 69.1 MiB (70,784 KiB) |
| `fdt` at M3, window only, not connected | 98.9 MiB (101,232 KiB; 98.7 MiB on a second run), +29.7 MiB since M2 |
| `attach_hold` (the capture without a window), connected to the idle app | 50.5 MiB at 1 s, 59.0 MiB at 6 s, 59.5 to 60.2 MiB over the next 20 s |
| `fdt` at M3, connected | 152.7 to 153.3 MiB |

M2's record said 54 to 56 MB for an unconnected window (54,336 to 56,288 KiB, which is 53.1 to 55.0 MiB); the M2 build run here gave 69.1 MiB (the machine and the window size differ), so about +30 MiB is the comparable growth. Two parts to explain, **neither investigated**: the window alone grew by about 30 MiB (M3 views, fonts or caches created at startup), and the capture, which M2's record gave as 3.2 MB (3,216 KiB, 3.1 MiB), now holds about 50 to 60 MiB with nothing captured (5 logs, 0 frames). Candidates are the Perfetto and timeline decode buffers, the rayon and tokio pools, and the store's pre-sized chunks. This is open decision 3.

**Attach to first live data, 5 runs: not measured.** `attach_ms` was 126 ms in the one run I could take (the live test against the idle profile-mode app, screen locked). M2 recorded 5 ms in its one profile-mode run (Task 22) and 10 to 11 ms in its three debug-mode runs, so profile to profile it is 5 ms against 126 ms, one run each. The M3 connect sequence also reads the refresh rate and the timeline flags, which may account for the difference; that is untested and not broken down. The live test fails at its next step (`goto_logs`, which needs a drawing app), so `attach_to_data_ms` was not reached. The budget is <= 1 s to first live data; 126 ms leaves a wide margin even before the first log. **Status: `attach_ms` measured once, `attach_to_data_ms` not measured, the five runs not taken.**

## Not measured

| Item | Why |
|---|---|
| Widget-build tracing overhead (labelled, once) | the screen locked before it; the "+29 %" is the Q5.3 spike figure |
| CPU-poll re-measurement at 15 s, and the 600-frame 5 s run | the screen locked |
| `frame-details` and the other draw misses on the quietest run | by the time load was low, the batches were done and the screen then locked |
| `fdt` window drawing while connected to a live app | `jank_lab` stops drawing when the display sleeps; the window idle numbers above are for a window that drew nothing |
| Attach to first live data, 5 runs | `jank_lab_live` needs a drawing app after connecting; one `attach_ms` only |
| Refresh-rate budget on a 60 Hz display | the only display is 120 Hz |
| Cold start (first paint <= 200 ms, interactive <= 400 ms) and palette open (one frame) | not run in this verification; M3 builds the Performance and Timeline views when the window opens, so both can have moved since M2 |
| Windows and Linux (thread and raster slice names, fact Q12, paths, shortcuts) | no such machine |
| The first CI run | no GitHub remote |

## Calls made and their outcomes

These are the plan's 14 calls, with what the verification can say about each.

1. **Timeline polled as JSON, Perfetto only for CPU samples.** Kept. Ingest runs at 3.41 M events/s (3.52 M with the decoder the poller ships, after the final fix wave, which also caps one poll at 2 s of timeline and catches up in steps).
2. **Poll every 500 ms; the `Timeline` stream only while tracing is on.** Kept. Not re-measured here.
3. **fdt leaves the app's timeline streams alone, adding `Dart` and `Embedder` if missing.** Kept.
4. **A slow frame is `max(build, raster)` over the budget, janky beyond twice the budget.** Kept; the strip's colouring changed from M2 as planned. The budget comes from the display (8.333 ms here).
5. **Replayed and duplicate frames are dropped.** Kept; pinned by the replay tests.
6. **CPU samples are fetched every 5 s while the app draws.** **Changed to 15 s** as a precaution after the measurement above: one of two runs crossed R6's median line; the re-measure at 15 s is pending. The brief of Task 10 required this check; it moved here by ruling R6.
7. **Raw widget-build slices are capped at 8,000,000.** Kept; the Rebuilds tab says when older detail was dropped.
8. **Async slices are grouped by name into packed rows.** Kept.
9. **The Raw trace tab is in M3.** Kept; `trace-zoom` is the one that now misses the draw budget by 0.3 ms.
10. **The frame chart is indexed by frame, the Timeline lanes by time.** Kept.
11. **User code is any Dart library that isn't the SDK, Flutter or a pub-cache package.** Kept; see R11 to R13 below for how the user widget is named.
12. **Phase colours are shades of `accent` plus `lane_network`, always labelled.** Kept.
13. **Profiling is put back as found on a user disconnect or window close.** Kept; ruling R7 made a switch to another app count.
14. **`jank_lab` floats with `JANK_LAB_FLOAT=1`.** Kept, with a limit found in this run: floating does not help when the user's Space changes or the display sleeps; the window then draws nothing until the app is brought forward (`System Events` `frontmost`) and, once the session is locked, not at all.

### Rulings that changed the plan

- R1: `synth::stress_writer(seed)` returns the writer so the live scenarios can append to a session.
- R2: the CPU sample period is stored per run (`set_cpu_period`), so call trees convert samples to milliseconds from the run.
- R3: the strip's `MIN_DRAG_PX` is shared rather than duplicated.
- R4: "Fetch now" shows only while connected; otherwise the copy says the samples were not fetched before the app disconnected.
- R5: the `live-logs` bench-ui numbers moved to this task (Task 1's run was environmental): measured above, 2.78 ms draw p99, 3 of 3 passes.
- R6: the CPU-poll cost measurement moved to this task: measured above at 5 s, where one of two runs crossed the median line; default now 15 s, its re-measure pending.
- R7: connecting to another target while connected is a user disconnect from the first app, so profiling is restored.
- R8: the flame chart scrolls vertically when its deepest visible stack exceeds the rows; PanY pans time only when everything fits.
- R9: Task 24b was added for the frame-details draw time; it did not reach 4 ms (open decision 2).
- R10: Related in the frame details shows at most 5 requests and 5 log lines, plus "Show all" rows that open the tool with the range set.
- R11 to R13: the user widget named under a rebuild root is the largest-subtree user widget; user widgets are told apart from framework widgets by a generated list of the Flutter SDK's `Widget` subclasses (950 names, Flutter 3.47.9; regenerate with `scripts/gen-framework-widgets.sh`). Final fix wave: 906 names, after the generator stopped reading the sample classes in doc comments (`MyApp`, `HomePage` and 42 more).
- R12: "Trace widget builds" uses `profileUserWidgetBuilds`, about +29 % build time (spike figure) against +88 % for all widgets.
- R14: the live jank checks judge every build- or raster-dominated janky frame in the window (at least 5, at least 80 % attributed), not just the slowest.

## Open decisions for the author

Numbers 1 to 7 are carried from M2's record, with the numbering made consecutive; 8 onward are new in M3.

1. **Draw p99 for Logs search and tool switch is 6.3 and 7.1 ms against 4 ms** (M2: 6.5 and 6.9). Cost: reshaping a full page of text per frame. Meeting it needs visible changes (fewer text runs per row, no ligatures). Decide: change the design, or accept about 8 ms for these two interactions.
2. **Draw p99 for the M3 pages: `frame-details` 4.5, `trace-zoom` 4.3, `strip-drag` 4.3 against 4 ms** (and `net-filter` once at 4.2). Final fix wave: `frame-details` re-run three times at load 3.8 to 4.9: 5.03, 4.30, 4.59 ms (median 4.59), still over. The wave stopped the pane being rebuilt on every snapshot publish while data streams (reviewer B measured 21 rebuilds for 10 unrelated publishes; now 0), but this scenario uses a fixed snapshot and a new frame per step, so it does not exercise that; a streaming variant of `frame-details` would. `trace-zoom` and `strip-drag`: pending re-measure. `frame-details`: as Task 24b measured it, the pane draw (text shaping of Related rows, about 1.5 ms) lands on the table's scrollbar-animation re-render (about 1.8 ms). Options Task 24b listed and did not apply: a visible scrollbar mode for the frames table, applying the pane result one frame later (+8 ms latency), or virtualising Related below the fold. The quiet-machine re-run could not be taken (see above); re-run first (`./target/release/fdt bench-ui frame-details`, three times).
3. **Idle memory of the connected window is 153.3 MiB (160.8 MB) against 150 MB**, a miss in either unit, measured with a window that was not drawing. The window alone grew from 69.1 to 98.9 MiB since M2 and the capture alone is about 50 to 60 MiB with nothing captured (M2: 3.1 MiB). Decide whether to find and trim the growth (the likely places are listed above) or to move the budget; repeat the measurement with a drawing window first. Final fix wave: reviewer A traced the capture's 50 to 60 MiB to one ring-sized `getVMTimeline` reply decoded through a `Value` tree, whose peak the allocator keeps. The poller now decodes with the typed pass (events converted as they are read) and asks for at most 2 s of timeline per call. Against reviewer A's fake idle VM service (`attach_hold`, release, RSS after 7 s), a first reply of 33,000 events now leaves 26.9 MiB (59.9 MiB before), one of 4,762 events 11.0 MiB (15.7), and the idle feed alone 7.0 MiB (7.3). The connected window's 153.3 MiB: pending re-measure against a live app.
4. **The CPU-poll default and a possible hitch.** In one of two overhead runs every connected sample had one 80 to 110 ms frame. The hypothesis is that each CPU-sample fetch causes it, but the other run had such a frame in 1 of 6 samples and nothing tied the frames to a fetch. The default is now 15 s as a precaution; the re-measurement is pending, and it should record when each fetch is sent so long frames can be matched to fetches. If the hypothesis holds, decide whether that is acceptable for a tool that must add under 2 %, or whether the fetch should be made on demand (when a slow frame is opened) with a longer interval in the background. The VM keeps only about 40 s of samples.
5. **The first CI run.** No GitHub remote yet; `ci.yml` (including the live `jank_lab` job and the release budget step) has never run.
6. **Recent URLs keep the VM token in `settings.json`** (M2 decision 3). Store it, or store only host and port and ask again.
7. **Bodies of unknown length are auto-fetched with a 10 s bound** (M2 decision 4), **new session (⌘N) discards the stored session with no confirmation and fires inside inputs** (M2 decision 7), **`fdt --connect` skips recent URLs** (M2 decision 6), and **Windows and Linux paths are read from source only** (M2 decision 5). All unchanged. M2 decision 8 (a live-publishing bench) is closed: `live-logs` and `live-perf` exist and pass.
8. **Widget-build tracing cost is a planning figure.** "Trace widget builds" says "about 30 %" in its copy; that number is from the spike (Q5.3), not from the finished app. Measure it once, labelled, with `profileUserWidgetBuilds` on `/rebuilds` (command in "Pending verification").
9. **Known flaky tests** (see below). Decide whether to fix them before CI starts. Final fix wave: the first one, `bench_ui::live_perf_checks_that_the_session_grew_and_the_chart_followed`, had a deterministic cause (its growth expectation was in wall-clock time against a fixed two frames per step) and is fixed; the check counts steps, and a test runs it with 200 ms per step.

### Known flaky tests

- ~~`bench_ui::live_perf_checks_that_the_session_grew_and_the_chart_followed` failed under load in 2 workspace runs and passes alone.~~ Final fix wave: the expected growth was `elapsed / 80 ms` frames against a test feed of 2 frames per step, so any run slower than about 160 ms per step failed; it now counts steps (`live_perf_passes_on_a_slow_machine` sleeps 200 ms per step).
- `lifecycle::new_session_starts_empty_and_keeping_the_connection` hit a settle-helper timeout once under load (Task 11) and passed 6 re-runs.
- Also from the ledger: `fdt_capture` `replay_debug` failed intermittently under load on the M2 baseline (not reproduced in 80 runs), and one workspace run of Task 22 had 3 failures not reproduced in 4 runs.

## Author review checklist

Save screenshots next to this file as `m3-<os>-<theme>-<view>.png`.

- [ ] Visual review of the Performance tabs (Frames, CPU, Rebuilds, Raw trace) and the Timeline page, in light and dark, on macOS, Windows and Linux: the frame chart with its budget line, the frames table, frame details with a slow-frame sentence and the Related block, the CPU bottom-up, top-down and flame views, the Rebuilds table, the Raw trace (with the UI and raster threads), the Timeline lanes (frames, requests, logs, navigation, GC, findings), the debug-mode banner, and the "Trace widget builds" setting and its copy.
- [ ] On Windows and Linux, check the thread and raster slice names (fact Q12: event names, the default streams and the platform-thread merge were verified only on Flutter 3.47 on macOS; confirm the Raw trace tab and the phase matching find the UI and raster threads on the other two).
- [ ] Check the refresh-rate budget on a 60 Hz display: the strip's bar colours and heights, the frame chart's budget line and its label, and `fdt bench-ui` reporting `refresh_ms` near 16.7 and `refresh_source` `display`.
- [ ] Keyboard-only pass over the new commands: the tool shortcuts for Performance and Timeline, next and previous slow frame, J and K in the frames table, the palette items for frames and CPU functions, the cross-links ("Show all in Network", and from a log or request to its frame).
- [ ] A real app (not `jank_lab`): the slow-frame sentence on a janky frame in profile mode, the Rebuilds tab with "Trace widget builds" on, and the debug-mode banner in a debug run.
- [ ] A hot restart and a full restart in profile mode: the run ends or continues, frames and CPU samples of the old run stay, profiling is put back on a user disconnect.
- [ ] Repeat the draw-time misses and idle memory on a quiet, awake R1 (commands below).

## Pending verification

All on an awake, unlocked R1, with `uptime` before each batch:

- **bench-ui, the misses again:** `cargo build --release -p fdt_app` then, three times each, `./target/release/fdt bench-ui <logs-search|tool-switch|strip-drag|frame-details|trace-zoom|net-filter>`.
- **CPU poll at 15 s:** start a profile `jank_lab` (`JANK_LAB_FLOAT=1 flutter run -d macos --profile`), go to `/jank` (`ext.jank_lab.run` with `goto_jank`), and run 4 interleaved rounds of `frame_build_sampler --uri <ws uri> --frames 600` with and without `fdt --connect <ws uri>` (now 15 s). Pass: median delta <= 20 us, p99 delta <= 1 ms, no build over 40 ms in connected rounds. Record when each CPU fetch is sent (the capture does not log successful fetches today), so that a long frame can be matched to a fetch or ruled out as one.
- **Tracing overhead, labelled:** the same on `/rebuilds`, with and without the "Trace widget builds" switch on in the connected window.
- **Idle memory and CPU of the connected window while drawing:** `fdt --connect`, home screen, 30 s warm-up, then 30 samples of `ps -o rss=,%cpu= -p <pid>` one second apart (`ps` reports KiB; divide by 1,024 for MiB). Budgets: RSS <= 150 MB, CPU <= 1 %.
- **Attach to first live data, 5 runs:** `scripts/jank-lab-it.sh profile` five times (it prints `attach_ms`, `attach_to_data_ms`, `first_log_ms`).
- **The first CI run** on macOS, Windows and Linux.
- **Windows and Linux** code paths and slice names.

## Appendices: raw runs


### A. Release budget tests (Step 1)

`FDT_BUDGETS=strict cargo test --workspace --release -- budget --nocapture`, measurement lines and results only (the "test ... ok" lines are elided):

```
 0:59  up 5 days,  9:16, 9 users, load averages: 11.82 12.28 13.87
    Finished `release` profile [optimized + debuginfo] target(s) in 17.51s
running 4 tests
call_tree_budget: top_down p50 229.583µs, bottom_up p50 1.413208ms, nodes 3000
flame_budget: 10 s range p50 4.623875ms, 100612 spans, truncated false
explain_budget: 100 slowest frames p50 1.2145ms, p99 1.678416ms, max 3.651708ms
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 42 filtered out; finished in 1.38s
running 12 tests
lane layout, 6 lanes, 72k frames: median 0.09 ms, worst 0.26 ms
render of the 50 MB body's stored MiB: 266.458µs
budget query len 1 worst 1.713375ms p90 1.462333ms
pretty-print of 1 MiB JSON: 19.574292ms
budget query len 2 worst 1.401917ms p90 1.383041ms
budget query len 3 worst 1.616791ms p90 1.509833ms
budget query len 4 worst 1.592875ms p90 1.572917ms
budget query len 6 worst 1.595917ms p90 1.58ms
budget query len 7 worst 1.650083ms p90 1.57925ms
budget query len 7 worst 1.548792ms p90 1.448666ms
budget query len 8 worst 1.558291ms p90 1.543209ms
budget query len 3 worst 1.397625ms p90 1.367167ms
budget query len 100000 worst 648.583µs p90 590.667µs
budget query len 79 worst 29.625µs p90 23.125µs
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 454 filtered out; finished in 0.69s
running 1 test
assemble_budget: 199990 events, decode+feed p50 58.729542ms (decode alone p50 54.658125ms), runs [57.586042ms, 57.938667ms, 58.729542ms, 58.994667ms, 59.07125ms], 3.41 M events/s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 42 filtered out; finished in 0.31s
running 9 tests
strip_bars 72k frames, width 1000: p90 0.0016 ms, worst 0.0067 ms
synth::perf(72_000): 4998740 slices (119 MB) + 3282703 cells (52 MB), drops TimelineDrops { depth: 0, overlap: 0, build_cap: 0, lod_coarse: 0 }
lane query over 6 UI lanes: p90 0.0208 ms, worst 0.0532 ms
synthetic 1M logs: 150 MB of text
full scan timeout: p90 28.7 ms, worst 32.9 ms
full scan Timeout: p90 2.3 ms, worst 2.4 ms
full scan user_42: p90 39.1 ms, worst 40.6 ms
full scan regex status=5\d\d: p90 40.7 ms, worst 40.7 ms
first results timeout: p90 2.0 ms, worst 2.0 ms
first results Timeout: p90 0.1 ms, worst 0.2 ms
first results user_42: p90 3.0 ms, worst 3.1 ms
first results regex status=5\d\d: p90 2.7 ms, worst 2.8 ms
net filter text products: p90 0.88 ms, worst 1.02 ms
net filter 4xx and 5xx: p90 0.27 ms, worst 0.36 ms
net filter host and POST: p90 0.61 ms, worst 0.81 ms
net filter errors tab: p90 0.28 ms, worst 0.38 ms
10,000 slices + publish: p90 0.7937 ms, worst 0.8454 ms
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 96 filtered out; finished in 2.39s
 0:59  up 5 days,  9:16, 9 users, load averages: 13.88 12.75 13.99
```

### B. bench-ui, every scenario x 3 (Step 2)

One JSON line per run with the per-phase and per-view render fields removed (they are in the report output); the uptime line is before each batch.

```
## batch 1  1:00  up 5 days,  9:17, 9 users, load averages: 11.37 12.23 13.76
RUN 1 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.192,"p95":2.694,"p99":2.898,"max":4.66},"interval_ms":{"p50":8.333,"p95":8.38,"p99":8.419,"max":8.503},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.53,"p95":2.889,"p99":6.627,"max":6.627},"interval_ms":{"p50":8.328,"p95":8.505,"p99":8.559,"max":8.559},"latency_ms":{"p50":8.927,"p95":19.567,"p99":19.567,"max":19.567},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.7,"p95":35.155,"p99":35.155,"max":35.155},"full_frames":{"p50":3.0,"p95":4.0,"p99":4.0,"max":4.0},"samples":{"first":18,"full":18},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0,"full_p99_ms":100.0,"full_budget_frames":12.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 1 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.628,"p95":3.057,"p99":3.349,"max":4.195},"interval_ms":{"p50":8.332,"p95":8.38,"p99":8.408,"max":16.645},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.684,"p95":3.679,"p99":4.211,"max":4.211},"interval_ms":{"p50":8.332,"p95":8.455,"p99":8.456,"max":8.456},"latency_ms":{"p50":11.594,"p95":12.572,"p99":12.572,"max":12.572},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 1 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":5.38,"p95":8.59,"p99":13.298,"max":13.298},"interval_ms":{"p50":33.326,"p95":33.405,"p99":33.431,"max":33.431},"latency_ms":{"p50":38.813,"p95":41.986,"p99":46.72,"max":46.72},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 1 strip-drag
{"scenario":"strip-drag","error":"the drag did not select a time range on the strip","pass":false}
RUN 1 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.73,"p95":2.409,"p99":2.621,"max":2.971},"interval_ms":{"p50":8.332,"p95":8.394,"p99":8.449,"max":8.487},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":1.278,"p95":2.8,"p99":3.013,"max":3.134},"interval_ms":{"p50":8.328,"p95":8.602,"p99":8.643,"max":8.746},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 frame-details
{"scenario":"frame-details","frames":100,"draws":101,"draw_ms":{"p50":2.648,"p95":3.831,"p99":5.231,"max":7.064},"interval_ms":{"p50":8.332,"p95":8.379,"p99":8.4,"max":8.525},"latency_ms":{"p50":10.769,"p95":13.039,"p99":15.423,"max":15.423},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":50},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 1 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":0.925,"p95":2.091,"p99":2.264,"max":2.586},"interval_ms":{"p50":8.327,"p95":8.663,"p99":8.746,"max":8.867},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.675,"p95":2.288,"p99":3.394,"max":4.558},"interval_ms":{"p50":8.333,"p95":8.376,"p99":8.528,"max":8.663},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":0.616,"p95":2.142,"p99":2.584,"max":2.595},"interval_ms":{"p50":8.332,"p95":8.385,"p99":8.618,"max":8.684},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":1.76,"p95":2.619,"p99":3.811,"max":3.932},"interval_ms":{"p50":8.333,"p95":8.37,"p99":8.382,"max":8.445},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 1 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":0.985,"p95":3.355,"p99":3.639,"max":4.336},"interval_ms":{"p50":8.333,"p95":8.38,"p99":8.419,"max":11.395},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
## batch 2  1:01  up 5 days,  9:18, 9 users, load averages: 11.14 12.02 13.57
RUN 2 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.419,"p95":2.864,"p99":3.129,"max":4.672},"interval_ms":{"p50":8.332,"p95":8.379,"p99":8.41,"max":8.515},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.525,"p95":2.95,"p99":6.328,"max":6.328},"interval_ms":{"p50":8.336,"p95":8.518,"p99":8.966,"max":8.966},"latency_ms":{"p50":8.886,"p95":19.582,"p99":19.582,"max":19.582},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.618,"p95":42.472,"p99":42.472,"max":42.472},"full_frames":{"p50":3.0,"p95":5.0,"p99":5.0,"max":5.0},"samples":{"first":18,"full":18},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0,"full_p99_ms":100.0,"full_budget_frames":12.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 2 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.668,"p95":2.99,"p99":3.207,"max":3.592},"interval_ms":{"p50":8.332,"p95":8.378,"p99":8.407,"max":8.428},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.378,"p95":3.202,"p99":3.403,"max":3.403},"interval_ms":{"p50":8.325,"p95":8.448,"p99":8.466,"max":8.466},"latency_ms":{"p50":11.207,"p95":11.769,"p99":11.769,"max":11.769},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.415,"p95":5.774,"p99":7.083,"max":7.083},"interval_ms":{"p50":8.333,"p95":8.384,"p99":8.392,"max":8.392},"latency_ms":{"p50":11.88,"p95":14.134,"p99":15.416,"max":15.416},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 2 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":2.188,"p95":2.569,"p99":4.171,"max":4.915},"interval_ms":{"p50":8.332,"p95":8.381,"p99":8.619,"max":8.939},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 2 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.925,"p95":2.543,"p99":2.915,"max":3.352},"interval_ms":{"p50":8.332,"p95":8.383,"p99":8.405,"max":8.509},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":1.502,"p95":2.82,"p99":3.049,"max":3.356},"interval_ms":{"p50":8.329,"p95":8.599,"p99":8.654,"max":8.717},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 frame-details
{"scenario":"frame-details","frames":100,"draws":101,"draw_ms":{"p50":2.729,"p95":3.764,"p99":4.314,"max":7.01},"interval_ms":{"p50":8.336,"p95":8.383,"p99":8.402,"max":8.411},"latency_ms":{"p50":10.698,"p95":12.465,"p99":15.418,"max":15.418},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":50},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 2 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.43,"p95":1.958,"p99":2.126,"max":2.46},"interval_ms":{"p50":8.324,"p95":8.663,"p99":8.702,"max":8.825},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":2.018,"p95":3.229,"p99":4.299,"max":4.712},"interval_ms":{"p50":8.333,"p95":8.427,"p99":8.609,"max":8.64},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 2 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.816,"p95":2.515,"p99":2.663,"max":2.698},"interval_ms":{"p50":8.328,"p95":8.578,"p99":8.632,"max":8.648},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":2.596,"p95":2.977,"p99":3.432,"max":3.691},"interval_ms":{"p50":8.333,"p95":8.372,"p99":8.422,"max":8.551},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 2 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":1.008,"p95":3.535,"p99":3.855,"max":5.546},"interval_ms":{"p50":8.332,"p95":8.383,"p99":8.444,"max":11.908},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
## batch 3  1:02  up 5 days,  9:19, 9 users, load averages: 9.22 11.34 13.20
RUN 3 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.383,"p95":2.914,"p99":3.165,"max":4.458},"interval_ms":{"p50":8.332,"p95":8.377,"p99":8.41,"max":8.918},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 logs-search
{"scenario":"logs-search","frames":91,"draws":92,"draw_ms":{"p50":0.503,"p95":2.902,"p99":6.078,"max":6.078},"interval_ms":{"p50":8.337,"p95":8.51,"p99":8.538,"max":8.538},"latency_ms":{"p50":8.819,"p95":19.536,"p99":19.536,"max":19.536},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.608,"p95":42.397,"p99":42.397,"max":42.397},"full_frames":{"p50":3.0,"p95":5.0,"p99":5.0,"max":5.0},"samples":{"first":18,"full":18},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0,"full_p99_ms":100.0,"full_budget_frames":12.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 3 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.757,"p95":3.058,"p99":3.295,"max":3.839},"interval_ms":{"p50":8.333,"p95":8.375,"p99":8.404,"max":8.447},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.139,"p95":3.144,"p99":3.401,"max":3.401},"interval_ms":{"p50":8.327,"p95":8.472,"p99":8.499,"max":8.499},"latency_ms":{"p50":10.958,"p95":11.773,"p99":11.773,"max":11.773},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":16.0,"latency_budget_frames":2.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.37,"p95":5.901,"p99":6.628,"max":6.628},"interval_ms":{"p50":8.336,"p95":8.363,"p99":11.506,"max":11.506},"latency_ms":{"p50":11.727,"p95":14.41,"p99":16.844,"max":16.844},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 3 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":2.092,"p95":3.8,"p99":4.381,"max":4.756},"interval_ms":{"p50":8.331,"p95":8.609,"p99":8.702,"max":8.882},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 3 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.862,"p95":2.489,"p99":2.779,"max":3.969},"interval_ms":{"p50":8.333,"p95":8.382,"p99":8.436,"max":8.631},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":1.26,"p95":2.8,"p99":3.123,"max":3.297},"interval_ms":{"p50":8.33,"p95":8.606,"p99":8.651,"max":8.71},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 frame-details
{"scenario":"frame-details","frames":100,"draws":101,"draw_ms":{"p50":2.778,"p95":3.789,"p99":4.514,"max":6.871},"interval_ms":{"p50":8.333,"p95":8.387,"p99":8.397,"max":8.402},"latency_ms":{"p50":10.696,"p95":12.811,"p99":15.151,"max":15.151},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":50},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7,"latency_p99_ms":50.0,"latency_budget_frames":6.0},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 3 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":0.991,"p95":2.031,"p99":2.335,"max":2.584},"interval_ms":{"p50":8.326,"p95":8.655,"p99":8.728,"max":8.863},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":2.097,"p95":3.813,"p99":4.333,"max":4.414},"interval_ms":{"p50":8.331,"p95":8.396,"p99":8.637,"max":8.654},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":false}
RUN 3 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.197,"p95":2.625,"p99":2.769,"max":3.062},"interval_ms":{"p50":8.327,"p95":8.593,"p99":8.632,"max":9.016},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":2.396,"p95":2.91,"p99":3.699,"max":3.924},"interval_ms":{"p50":8.332,"p95":8.378,"p99":8.432,"max":9.506},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
RUN 3 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":1.106,"p95":3.566,"p99":3.831,"max":4.305},"interval_ms":{"p50":8.332,"p95":8.377,"p99":8.411,"max":8.565},"budget":{"draw_p99_ms":4.0,"interval_p99_ms":8.833,"interval_max_ms":16.7},"interval_budget_ms":8.833,"interval_plan_budget_ms":8.3,"refresh_ms":8.333,"refresh_source":"display","pass":true}
## done  1:03  up 5 days,  9:20, 9 users, load averages: 12.96 12.08 13.36
```

### C. Overhead runs (Step 3)

Line format: mode, then the sampler output (`build_us` in microseconds). Run 2 (recorded):

```
--- /jank
## round 1 goto_jank  1:24  up 5 days,  9:41, 9 users, load averages: 11.29 8.37 8.66
bare frames=150 first_number=568 last_number=717 seconds=3.8 build_us median=24147 mean=24184.7 p90=24263 p99=24534 min=24073 max=26729
conn frames=150 first_number=1240 last_number=1389 seconds=3.8 build_us median=24068 mean=24072.2 p90=24107 p99=24144 min=24037 max=24165
## round 2 goto_jank  1:24  up 5 days,  9:41, 9 users, load averages: 10.93 8.59 8.73
conn frames=150 first_number=1954 last_number=2103 seconds=3.8 build_us median=24075 mean=24079.7 p90=24116 p99=24163 min=24040 max=24166
bare frames=150 first_number=2275 last_number=2424 seconds=3.8 build_us median=24054 mean=24062.8 p90=24091 p99=24158 min=24034 max=24172
## round 3 goto_jank  1:24  up 5 days,  9:41, 9 users, load averages: 8.53 8.22 8.59
bare frames=150 first_number=2553 last_number=2702 seconds=3.8 build_us median=24054 mean=24064.8 p90=24097 p99=24196 min=24033 max=24328
conn frames=150 first_number=3227 last_number=3376 seconds=3.8 build_us median=24053 mean=24059.4 p90=24090 p99=24120 min=24034 max=24121
## round 4 goto_jank  1:25  up 5 days,  9:42, 9 users, load averages: 7.57 8.03 8.52
conn frames=150 first_number=3941 last_number=4090 seconds=3.8 build_us median=24053 mean=24061.0 p90=24088 p99=24116 min=24033 max=24196
bare frames=150 first_number=4260 last_number=4409 seconds=3.8 build_us median=24056 mean=24059.7 p90=24089 p99=24127 min=24032 max=24129
## round 5 goto_jank  1:25  up 5 days,  9:42, 9 users, load averages: 6.61 7.80 8.42
bare frames=150 first_number=4540 last_number=4689 seconds=3.8 build_us median=24063 mean=24070.9 p90=24110 p99=24212 min=24032 max=24367
conn frames=150 first_number=5214 last_number=5363 seconds=4.7 build_us median=24086 mean=24563.4 p90=24147 p99=24471 min=24039 max=94260
## round 6 goto_jank  1:26  up 5 days,  9:43, 9 users, load averages: 5.47 7.42 8.26
conn frames=150 first_number=5614 last_number=5763 seconds=35.6 build_us median=24083 mean=24104.0 p90=24213 p99=24279 min=24036 max=24312
bare frames=150 first_number=5897 last_number=6046 seconds=3.7 build_us median=24194 mean=24197.3 p90=24231 p99=24273 min=24134 max=24298
--- /rebuilds
## round 1 goto_rebuilds  1:27  up 5 days,  9:44, 9 users, load averages: 3.81 6.58 7.88
bare frames=150 first_number=6453 last_number=6602 seconds=2.3 build_us median=1004 mean=1003.5 p90=1128 p99=1210 min=658 max=1257
conn frames=150 first_number=7418 last_number=7567 seconds=2.3 build_us median=883 mean=1028.0 p90=1432 p99=3824 min=545 max=4434
## round 2 goto_rebuilds  1:27  up 5 days,  9:44, 9 users, load averages: 3.29 6.28 7.75
conn frames=150 first_number=8253 last_number=8402 seconds=2.3 build_us median=1062 mean=1165.2 p90=1607 p99=3425 min=616 max=3863
bare frames=150 first_number=8672 last_number=8821 seconds=2.4 build_us median=975 mean=1197.9 p90=1783 p99=4328 min=544 max=4564
## round 3 goto_rebuilds  1:27  up 5 days,  9:44, 9 users, load averages: 3.08 6.00 7.60
bare frames=150 first_number=9028 last_number=9177 seconds=65.4 build_us median=1015 mean=1112.3 p90=1597 p99=2336 min=98 max=2562
conn frames=150 first_number=9816 last_number=9965 seconds=2.3 build_us median=922 mean=994.2 p90=1244 p99=2751 min=444 max=4378
## round 4 goto_rebuilds  1:29  up 5 days,  9:46, 9 users, load averages: 2.12 4.94 7.04
conn frames=150 first_number=10052 last_number=10201 seconds=2.3 build_us median=875 mean=1035.1 p90=1833 p99=2977 min=354 max=3917
bare frames=150 first_number=10466 last_number=10615 seconds=2.3 build_us median=973 mean=1226.5 p90=2294 p99=2823 min=468 max=2823
## round 5 goto_rebuilds  1:29  up 5 days,  9:46, 9 users, load averages: 2.04 4.73 6.91
bare frames=150 first_number=10818 last_number=10967 seconds=2.3 build_us median=901 mean=1191.7 p90=2412 p99=3040 min=455 max=3389
conn frames=150 first_number=11780 last_number=11929 seconds=2.3 build_us median=889 mean=1011.5 p90=1468 p99=2687 min=478 max=2699
## round 6 goto_rebuilds  1:30  up 5 days,  9:47, 9 users, load averages: 1.98 4.49 6.77
conn frames=150 first_number=12811 last_number=12960 seconds=2.3 build_us median=848 mean=910.2 p90=1260 p99=1504 min=522 max=2766
bare frames=150 first_number=13229 last_number=13378 seconds=2.3 build_us median=870 mean=1014.5 p90=1687 p99=2840 min=467 max=2955
```

Run 1, `/jank` (older sampler, no p99 column):

```
## round 1 goto_jank  1:07  up 5 days,  9:24, 9 users, load averages: 2.83 7.48 11.00
bare frames=150 first_number=5 last_number=154 seconds=8.4 build_us median=24090 mean=24132.5 p90=24187 min=24037 max=27358
conn frames=150 first_number=637 last_number=786 seconds=3.8 build_us median=24149 mean=24655.1 p90=24210 min=24062 max=99272
## round 2 goto_jank  1:08  up 5 days,  9:25, 9 users, load averages: 2.85 7.10 10.76
conn frames=150 first_number=1304 last_number=1453 seconds=3.8 build_us median=24131 mean=24498.8 p90=24172 min=24054 max=79526
bare frames=150 first_number=1577 last_number=1726 seconds=3.9 build_us median=24072 mean=24080.9 p90=24120 min=24038 max=24280
## round 3 goto_jank  1:08  up 5 days,  9:25, 9 users, load averages: 3.15 6.84 10.56
bare frames=150 first_number=1812 last_number=1961 seconds=3.8 build_us median=24085 mean=24090.1 p90=24146 min=24036 max=24341
conn frames=150 first_number=2442 last_number=2591 seconds=3.9 build_us median=24097 mean=24542.9 p90=24152 min=24046 max=88378
## round 4 goto_jank  1:09  up 5 days,  9:26, 9 users, load averages: 2.89 6.54 10.36
conn frames=150 first_number=3115 last_number=3264 seconds=3.9 build_us median=24067 mean=24492.3 p90=24099 min=24037 max=87008
bare frames=150 first_number=3389 last_number=3538 seconds=3.7 build_us median=24081 mean=24094.0 p90=24169 min=24037 max=24209
## round 5 goto_jank  1:09  up 5 days,  9:26, 9 users, load averages: 2.99 6.27 10.15
bare frames=150 first_number=3621 last_number=3770 seconds=3.7 build_us median=24069 mean=24081.7 p90=24139 min=24035 max=24405
conn frames=150 first_number=4248 last_number=4397 seconds=3.9 build_us median=24191 mean=24779.2 p90=24231 min=24122 max=111763
## round 6 goto_jank  1:09  up 5 days,  9:26, 9 users, load averages: 3.17 6.09 10.00
conn frames=150 first_number=4919 last_number=5068 seconds=3.8 build_us median=24189 mean=24693.6 p90=24234 min=24125 max=98965
bare frames=150 first_number=5192 last_number=5344 seconds=3.9 build_us median=24143 mean=24160.9 p90=24189 min=24073 max=25746
```

A third attempt on a first `jank_lab` instance returned "0 of 150 frames" for every sample (the window was on another Space and not drawing) and was discarded; its file had only these lines:

```
## round 1 goto_jank  1:10  up 5 days,  9:27, 9 users, load averages: 5.27 6.18 9.80
bare only 0 of 150 frames arrived
conn only 0 of 150 frames arrived
## round 2 goto_jank  1:15  up 5 days,  9:31, 9 users, load averages: 4.23 6.25 9.04
```

### D. CPU poll, planned 600-frame run (incomplete)

```
## round 1 goto_jank  1:30  up 5 days,  9:47, 9 users, load averages: 2.59 4.39 6.63
bare frames=600 first_number=14389 last_number=14988 seconds=16.2 build_us median=24527 mean=24542.6 p90=24789 p99=26199 min=24040 max=36981
conn only 0 of 600 frames arrived
## round 2 goto_jank  1:33  up 5 days,  9:50, 9 users, load averages: 3.29 3.81 6.01
```

### E. Idle memory and CPU (Step 4)

Connected `fdt`, locked screen. First line is `uptime`, then samples as `rss_kb cpu%` (`ps` RSS, in KiB), one per second after a 30 s warm-up, then `uptime`.

```
 1:40  up 5 days,  9:57, 9 users, load averages: 5.62 4.65 5.40
fdt pid 893; warm-up 30 s; samples (rss_kb cpu%)
156352   0.0
156352   0.4
156352   0.1
156352   0.3
156352   0.1
156368   0.0
156368   0.1
156896   0.0
156896   0.0
156896   0.0
156896   0.0
156896   0.0
156912   0.0
156928   0.0
156928   0.2
156928   0.0
156928   0.0
156928   0.0
156928   0.0
156960   0.0
156960   0.0
156960   0.0
156960   0.0
156960   0.0
156960   0.3
156960   0.3
156992   0.4
156992   0.3
156992   0.5
156992   0.3
 1:41  up 5 days,  9:58, 9 users, load averages: 3.35 4.16 5.16
```

Decomposition runs (single samples, same night): M2 `fdt` (1b9cd1f) unconnected `70784 0.0` (KiB), M3 `fdt` unconnected `101232 0.0` (101024 on a second run), `attach_hold` connected: 51,696 KiB at 1 s rising to 51,936 at 5 s and 60,432 at 6 s; 60,896 to 61,600 KiB over the following 20 s with CPU 0.0 to 0.1 %. The M2 `fdt --connect` run at 70,720 KiB cannot have been connected (the capture host installs from a frame there, and the screen was locked); only its unconnected number is used.

Attach: `FDT_JANK_LAB=1 cargo test --release -p fdt_capture --test jank_lab_live -- --ignored --nocapture` against the idle profile app: `attach_ms=126 debug=false`, then `panicked at crates/fdt_capture/tests/jank_lab_live.rs:74:17` (the `goto_logs` action never saw its screen mount).
