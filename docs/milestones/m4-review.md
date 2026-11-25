# M4 verification results and review checklist

Branch `m4-memory` (38 commits on master `775a908`, 76 files, +25,818 / -154 lines). R1: Apple M3 Max, macOS, 14 cores, 120 Hz display, release build. Run on 25 November 2026 between 15:35 and 18:18 local (+0400). Tasks 1 to 18 were each reviewed on their own; the whole-branch review is still to come.

**The machine was not quiet, and this record says so with every number.** Load average was 3 to 5 for most of the run (the author's own apps: a virtual machine, an Android emulator, Chrome), 12.8 right after the release compile, and 1.4 to 2.5 late in the afternoon. The plan asks for "an awake, quiet R1". No budget below has been loosened; where one missed and load is a plausible cause, it is recorded as a miss with the load and "re-measure on a quiet machine" is on the author checklist. No app of the author's was closed. The display went to sleep or the session locked twice (15:52:14 and 18:10:10); what that cost is under "Not measured".

Legend: met, missed, not resolvable (the method cannot see the budget's margin), not measured (and why).

## Summary and status

M4 is built: the Memory tool (a live chart of heap used, capacity, external and RSS with GC marks; Classes, Leaks and Snapshot tabs), heap snapshots taken on request and analysed in the capture (dominators, retained sizes, shortest retaining paths, a level-1 leak scan, a diff between two snapshots, the size of the HTTP recording), the GC lane on the Timeline, the findings lane, cross-links, palette and keyboard actions, two bench-ui scenarios for the chart and the tree plus one for analysis, and a live `jank_lab` test of the whole path. The static checks and unit tests are green; **the live `jank_lab` test is not**: 2 of 5 profile runs failed (see below), and the cause of neither is known.

What this verification found:

- **Met:** every release budget test (31 strict, 0 failed); the 5M-object heap snapshot (1.30 s against 3 s on the synthetic heap, 0.89 s on the 4M-object replicated real heap), peak analysis memory (66.6 and 77.0 bytes per object against 80, so the replicated heap has 3 bytes of headroom) and the held index (28.09 and 28.08 bytes per reachable object against 32); the Classes table (p50 1.17 ms against 4); the keystroke latency of the Snapshot search (p99 7.2 ms against 16); the overhead on the profiled app at the median (+10.5 us, about +0.04 %, against 2 %); the tool-switch latency in milliseconds (18.2 against 50); attach to first live data (126 ms or less in 3 of 4 runs, against 1 s).
- **Missed, with the dominant cost:**
  - **A CPU-sample fetch costs one 130 to 168 ms frame**, every 15 s, as shown by the timestamps of 4 long frames in two runs (below) and by 6 of 6 connected overhead rounds. The median and p99 budgets hold, but no build over 40 ms in connected rounds is the rule M3 set, and 6 of 6 connected rounds broke it. This closes M3's open decision 4 as a confirmed cause, not a hypothesis. The decision it needs is the author's.
  - **Idle memory of the connected window is about 172 MiB (181 MB) against 150 MB**, with a window that draws (M3: 153.3 MiB with a window that did not).
  - **Idle CPU is 1.8 to 2.0 % in `top`** for the plain binary and the bundle on the Logs page, against 1 % (0.8 % after a snapshot, 11 % on one Memory page run). The `sample` tool saw the threads waiting almost all the time, so the two do not agree; cause not found (a ticking "Rec" clock in the title bar is a candidate).
  - **`memory-chart` interval p99 is 8.99 ms against 8.833** (3 of 3 runs, 8.75 / 8.99 / 9.04; the chart's own layout p99 is 0.87 ms, so our layout is not the cost) and one run had a 17.58 ms interval.
  - **`heap-tree` draw p99 is 4.31 ms against 4** (4.31, 3.45, 4.63), with 2 of 3 runs at a 13.3 ms frame interval (a display cadence of about 75 Hz), which is not ours.
  - **Draw p99 on the earlier scenarios** (by the median of 3, in either of two passes): `logs-search` 7.19 / 6.89, `tool-switch` 9.88 / 9.98 (M3: 7.08), `frame-details` 5.11 (M3: 4.51), `live-perf` 4.15, `logs-scroll` 4.20 (pass 2 only; new), and `net-scroll`, `net-filter`, `live-logs` in one pass of two.
  - **Two of five live tests failed, cause unknown for both**  (attach runs): step 6's unmount check once, and "only 0 judged frames" with tracing on once (the same as M3's first run).
- **Not measured:** idle with the window drawing on a quiet screen for every page; bench-ui pass 2 for `frame-chart` through `heap-analyse` (the display went off at 15:52:14, again at 18:10:10); the first CI run on Flutter 3.50; Windows and Linux. See "Not measured".
- **Closed by this record:** M3's pending re-measure of the six draw misses (done: `trace-zoom` and `strip-drag` now meet; `net-filter` meets on the median; `logs-search`, `tool-switch`, `frame-details` still miss), the 15 s CPU poll (done), tracing overhead (done, labelled, unresolved), and attach (done, 5 runs). M3's "there is no CI remote" is closed: CI has run since 2026-11-23 (its first run on Flutter 3.50 is still on the checklist).

## Automated checks (the live `jank_lab` test is not green)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass, no warnings |
| `cargo clippy --lib --target x86_64-pc-windows-msvc -p fdt_store -p fdt_vm -p fdt_capture -p fdt_analysis -- -D warnings` | pass |
| `cargo test --workspace` | 1,251 passed, 0 failed, 16 ignored (the ignored ones include the live `jank_lab` test); baseline at the start of M4 was 988 |
| `FDT_BUDGETS=strict cargo test --workspace --release -- budget --nocapture` | 31 passed (4 `fdt_analysis` lib, 2 `fdt_analysis` `heap_budget`, 14 `fdt_app`, 1 `fdt_capture` lib, 1 `fdt_capture` `perf`, 9 `fdt_store`), 0 failed |
| `jank_lab` live test, profile, `scripts/jank-lab-it.sh profile`, 5 runs | 3 passed, 2 failed (run 2: "one unmounted _LeakyScreenState" left 0, right 1, at step 6; run 3: "jank (tracing): only 0 judged frames"). Debug and profile both passed in Task 18; see "Open decisions" |

## Measured budgets

### Release budget tests (Step 1)

`uptime` before: load 5.44 5.15 5.29 (15:36); after: 12.82 7.55 6.19 (the release compile had just finished; the tests are single-threaded CPU work). Raw: appendix A.

| Measure | Budget | Measured | Verdict |
|---|---|---|---|
| Heap snapshot, 5M objects (synthetic): parse, dominators, leak scan | <= 3 s (p50 of 3) | 1.30 s total (parse 308 ms, reachability and scan 152 ms, dominators and index 837 ms) | met |
| Heap snapshot, 4.0M objects replicated from the real fixture (24 copies) | <= 3 s | 0.89 s (263 + 55 + 572 ms) | met |
| Peak analysis memory | <= 80 B per object | 66.6 (synthetic), **77.0** (replicated; 3 B of headroom, deterministic: any new allocation on the analysis path trips it) | met |
| Held index | <= 32 B per reachable object | 28.09 and 28.08 (92.8 MB for 3.30 M reachable objects) | met |
| Classes table rows, worst p50 | <= 4 ms | 1.17 ms | met |
| Call tree, flame, explain (M3) | 100 ms, 100 ms, 50 ms | 0.28 / 1.86 ms, 5.5 ms, p99 2.05 ms | met |
| Timeline ingest | >= 1 M events/s | 3.40 M events/s | met |
| Log search first results / full scan | <= 16 ms / <= 100 ms | worst 4.1 ms / 61.5 ms | met |
| Network filter over 100k requests | <= 16 ms | worst 5.09 ms (the first text query; the others 0.35 to 0.56 ms) | met |
| Palette per keystroke, 5,000 items | <= 8 ms | worst 2.81 ms | met |
| Timeline lane layout, strip bars, lane query, 10,000 slices plus publish (internal) | internal | 0.16 ms, 0.0067 ms, 0.096 ms, 1.08 ms | met |

The 5M-object tests ran at load 5 and passed with their margins; the memory limits are byte counts and do not depend on load.

### `fdt bench-ui` (Step 2): every scenario, 3 runs, two passes

Pass 1 ran all 17 scenarios from 15:37 to 15:47 (uptime before each scenario is in appendix B1; load 12.5 for the first, 3.4 to 4.5 for the rest, 7.2 and 7.6 for the last two). Pass 2 ran from 15:49 and was cut short: the display went off at 15:52:14 and `frame-chart` through `heap-analyse` timed out. A third attempt at 18:11 was also cut short (the session locked at 18:10:10), so its output is discarded (kept as `B3-bench-locked-discarded.txt` in the raw folder). So every scenario has pass 1, and `logs-scroll` through `live-logs` also have pass 2.

The display refreshes at 8.333 ms (120 Hz); the interval budget is 8.833 ms (M2's ruling R16). Draw p99 budget 4 ms. Cells are the median of the 3 runs, with the runs in brackets. "Pass" counts the report's own `pass`, which also checks the largest interval (16.7 ms) and, for latency scenarios, the latency lines.

**A cadence problem that is not ours.** In pass 1, 10 of the 51 runs had an interval p50 above 20 ms (the whole run at about 30 Hz, or 33 ms every other frame), and 3 of the pass 2 runs (`tool-switch` once, `live-logs` twice). They are spread over `logs-search`, `net-scroll`, `strip-drag`, `live-logs`, `frame-chart`, `live-perf`, `heap-analyse` and `tool-switch`, so they are not tied to a view. Two `heap-tree` runs ran at a steady 13.3 ms interval. M3 saw the same thing once (`tool-switch` run 1). It makes interval p99 and "pass" noisy and leaves draw p99 mostly intact.

| Scenario | Pass 1 draw p99 ms | Pass 2 draw p99 ms | Interval p99 ms (P1 / P2) | Latency p99 ms (budget) | Pass (P1 / P2) | Verdict (met only if met in every pass) |
|---|---|---|---|---|---|---|
| logs-scroll | timed out 3 of 3 (load 12.5, right after the compile) | **4.20** (3.25, 4.34, 4.20) | n/a / 8.78 | | 0/3 / 0/3 | **missed** (pass 2, by 0.2 ms; M3: 3.13) |
| logs-search | **7.19** (7.46, 7.17, 7.19) | **6.89** (6.89, 6.76, 6.91) | 34.4 / 8.78 | first 38.5 / 19.8 (16; P2 is 2 frames); full 38.5 / 35.6 (100) | 0/3 / 0/3 | **missed** (M2 open decision 1, unchanged) |
| net-scroll | **4.47** (4.64, 4.47, 3.08) | 3.30 (3.30, 3.28, 3.33) | 33.4 / 8.42 | | 1/3 / 3/3 | **missed in pass 1** (two runs at 30 Hz), met in pass 2 |
| net-filter | **4.38** (4.70, 4.38, 4.12) | 3.96 (3.98, 3.87, 3.96) | 8.46 / 8.52 | 12.7 / 12.4 (16) | 0/3 / 3/3 | **missed in pass 1** (0.4 ms), met in pass 2 |
| tool-switch | **9.88** (10.81, 9.51, 9.88) | **9.98** (9.24, 13.55, 9.98) | 10.1 / 10.3 | 18.2 / 18.4 (50) | 0/3 / 0/3 | **missed** (M3: 7.08; the list now includes Memory) |
| strip-drag | 2.78 (2.78, 3.07, 1.86) | 3.63 (4.14, 3.12; run 1 errored) | 33.4 / 8.78 | | 1/3 / 1/3 | met on the median; pass 2 run 3 errored "the drag did not select a time range on the strip" (M3 saw it once too) |
| live-logs | 2.73 (2.27, 4.55, 2.73) | **6.03** (6.03, 6.18, 5.63) | 8.84 / 33.7 | | 0/3 / 0/3 | **missed in pass 2** (all three runs at 30 Hz, where each draw is longer) |
| frame-chart | 2.33 (2.33, 3.35, 1.99) | not run (display off) | 8.75 | | 1/3 | met on draw; interval met on the median |
| frame-details | **5.11** (6.77, 5.11, 4.88) | not run | 8.48 | 21.6 (50) | 0/3 | **missed** (M3: 4.51, 4.59) |
| timeline-zoom | 2.09 (1.77, 2.09, 2.52) | not run | 8.66 | | 2/3 | met |
| trace-zoom | 3.49 (3.49, 3.85, 3.43) | not run | 8.59 | | 2/3 | met (M3 missed at 4.30) |
| flame-zoom | 2.98 (2.70, 3.02, 2.98) | not run | 8.74 | | 2/3 | met |
| cpu-tree | 3.69 (3.69, 3.63, 4.21) | not run | 8.53 | | 2/3 | met |
| live-perf | **4.15** (4.15, 3.81, 8.70) | not run | 8.68 | | 1/3 | **missed** by 0.15 ms (run 3 ran at 30 Hz; M3: 3.83) |
| memory-chart (new) | 2.61 (2.46, 2.61, 2.83) | not run | **8.99** (8.75, 8.99, 9.04) | `layout_ms` p99 0.87 (0.55, 0.87, 0.93) | 0/3 | **missed** (interval, by 0.16 ms; draw met) |
| heap-tree (new) | **4.31** (4.31, 3.45, 4.63) | not run | **13.37** (14.21, 8.69, 13.37) | keystroke 7.16 (7.16, 7.33, 4.74) vs 16 | 1/3 | **missed** (draw by 0.3 ms; two runs at a 13.3 ms cadence) |
| heap-analyse (new) | 2.15 (2.15, 5.38, 1.46) | not run | 33.35 (8.70, 33.71, 33.35) | analysis 1.32 s, 1.17 s, 1.33 s | 1/3 | draw met; interval met in run 1 and 30 Hz in runs 2 and 3 |

Notes:

- **`memory-chart`.** Interval p99 misses the 8.833 ms line by 0.16 ms in all three runs, with draw p99 at 2.6 ms and the chart's layout p99 at 0.87 ms (p50 0.31 to 0.40 ms). Our two costs add up to well under one frame, so the misses are the machine at load 3.8: the machine, not our layout, is the cost. Run 1 had one 17.58 ms interval (over the 16.7 ms line), which is why "pass" is 0 of 3. At Task 17's runs (load 6 to 7) the interval p99 was 9.02 and the layout p99 0.70 ms. Not measured: this scenario on a quiet machine.
- **`heap-tree`.** Draw p99 over 4 ms in 2 of 3 runs (4.31, 4.63), the same two runs that ran at a 13.3 ms interval, so the whole run was paced differently. Run 2 (8.33 ms cadence) had draw p99 3.45 and passed. Phases of run 1: prepaint p99 1.70 ms, paint p99 1.88 ms, with a single 8.47 ms paint. The keystroke latency (4.7 to 7.3 ms against 16) is met in all three.
- **`heap-analyse`.** The analysis took 1.17 to 1.33 s with the window drawing. In run 1, with a normal cadence, the interval p99 is 8.70 ms and draw p99 2.16 ms: the UI stays responsive while the analysis runs. Runs 2 and 3 had 33 ms intervals (a 30 Hz run) and draw p99 of 5.38 and 1.46 ms.
- **`tool-switch`.** 9.9 ms against M3's 7.1; its switch list gained Memory. The first-run phase split (prepaint p99 4.4 ms, paint p99 6.5 ms) points at paint, as in M2 and M3 (full-page text reshaping). Whether Memory is the added cost was not isolated. The switch latency in milliseconds (18.2) is met.
- **`frame-details`.** 5.11 ms median (6.77, 5.11, 4.88) against M3's final-wave 4.59 (and 4.51 earlier), at load 3.8, worse than M3 on a lighter machine. Nothing in M4 should touch this pane, but nothing was investigated. It stays open decision 2.
- **M3's six pending re-runs, answered.** `logs-search` (6.9 to 7.2) and `tool-switch` (9.9) and `frame-details` (5.1) still miss. `trace-zoom` (3.49) and `strip-drag` (2.78 and 3.63) now meet, `net-filter` meets in pass 2 only (3.96 and 4.38). `logs-scroll` newly misses in pass 2 (4.20) and was not measured in pass 1.
- The latency scenarios have 8 to 90 samples, so their "p99" is close to the maximum.

### Overhead on the profiled app (Step 3, <= 2 % added to frame build time)

Setup, as in M3: `jank_lab` in profile mode (`flutter run -d macos --profile`, `JANK_LAB_FLOAT=1`), `frame_build_sampler` (a second VM client on the `Extension` stream) collecting 150 `Flutter.Frame` build times per sample. "conn" is the real `fdt --connect <vm uri>` window at default settings (memory poll every 500 ms, the GC stream, CPU samples every 15 s, tracing off), started 10 s before sampling; "bare" is the sampler alone. Six interleaved rounds (bare first on odd rounds), the window started and killed each round. The sampler now runs `goto_jank` itself and waits 2 s before it counts frames (`--act`, `--trace`, `--long-ms` are new optional flags; a first attempt that took `since` before the 2 s wait counted the transition frames and is discarded). Load 4.1 falling to 2.5 over the run, 17:57 to 17:59. Raw: appendix C.

| Page | bare median of medians (us) | conn median of medians (us) | paired median delta | p99 delta (median) | Builds >= 40 ms | Verdict |
|---|---|---|---|---|---|---|
| `/jank` (24 ms build) | 24,078 | 24,090 | **+10.5 us (+0.04 %)**; rounds +21, -1, +6, +15, +6, +42 | +21 us (rounds +33, +21, +4, +21, +34, +16) | bare 0 of 6 rounds; **conn 6 of 6 rounds, one frame of 155.6 to 167.6 ms each** | the 2 % budget is **met** at the median and p99; the "no build over 40 ms" rule from M3's R6 is **missed** |

- The long frames come from the CPU-sample fetch (next section). The mean of a connected sample rises by about 1 ms (24,958 to 25,062 us against 24,062 to 24,101 bare), all of it from that one frame.
- Raw per-sample statistics (median, mean, p90, p99, min, max, frame range, seconds) are in appendix C.

### CPU polling: when is each fetch sent? (M3 open decision 4)

The capture does not log successful fetches, and adding a logger was not in the task. The fetch is a timer (`interval_at(start + cpu_poll, cpu_poll)`, 15 s, skipped when no frame was drawn since the last), so its times are known from the connect time. I ran the capture alone (`attach_hold`, which prints the epoch time at which it reports `Connected`, from the same `Capture` code the window uses) next to a 1,500-frame sampler that prints the wall-clock time of every frame whose build is at least 40 ms. Load 2.3 to 2.9. Raw: appendix D.

| Run | connected at (epoch ms) | long frames (epoch ms, build) | after connect |
|---|---|---|---|
| 1 | 1795615191932 | 1795615207174 (137.1 ms); 1795615222181 (151.2 ms) | **15.24 s; 30.25 s** |
| 2 | 1795615233732 | 1795615248966 (129.0 ms); 1795615263973 (156.8 ms) | **15.23 s; 30.24 s** |

All four long frames land 0.23 to 0.25 s after a multiple of 15 s from `Connected` (the offset is the status change and the event delivery), and there are no others in 3,000 frames. The two 150-frame rounds of the overhead run, started about 15 s after the window opened, each caught the first one. So the hypothesis of M3 is now a finding: **each CPU-sample fetch costs the app one 129 to 168 ms frame, every 15 s while frames are drawn.** The 600-frame run M3 planned was not repeated; this is its replacement. The median and p99 of the build time are not moved. The decision (on-demand fetch for a slow frame, a longer background interval, or accepting about 9 dropped frames per 15 s) stays with the author: open decision 4.

### Widget-build tracing overhead, once, labelled (M3 open decision 8)

Measured on `/rebuilds` by toggling `ext.flutter.profileUserWidgetBuilds` directly from the sampler (what the "Trace widget builds" switch does in fdt), tracing on against off, six interleaved rounds of 150 frames, no fdt connected, load 4.0 to 5.4. Raw: appendix F.

| Round (first, second) | off median (us) | on median (us) | on / off |
|---|---|---|---|
| 1 (off, on) | 2,121 | 1,551 | 0.73 |
| 2 (on, off) | 1,152 | 1,485 | 1.29 |
| 3 (off, on) | 1,411 | 2,771 | 1.96 |
| 4 (on, off) | 2,551 | 3,077 | 1.21 |
| 5 (off, on) | 2,956 | 4,469 | 1.51 |
| 6 (on, off) | 3,152 | 4,583 | 1.45 |

**Not resolvable.** The app's own build time on `/rebuilds` drifts from about 1.2 ms to 3.2 ms over the run (the "off" medians rise 1,152 to 3,152 us), more than any effect, so only the sign is believable: tracing on is slower in 5 of 6 rounds, with ratios of 1.21 to 1.96 in them (median of the six 1.37). That is consistent with "about 30 %" only loosely and does not confirm it; the copy still carries the spike figure. A fixed workload (not `/rebuilds`) or a quiet machine would resolve it. The switch was left off at the end.

### Idle memory and CPU, and attach (Step 4)

**Idle, connected, no snapshot.** The plain release binary (`./target/release/fdt --connect <vm uri>`) against an idle profile `jank_lab` on its home screen; a window that draws; the screen unlocked (checked before and after); load 4.1 at the start, 2.3 at the end. 30 s warm-up, then 30 one-second `ps -o rss=,%cpu=` samples. Raw: appendix G. The page the window opened on was not recorded; it restores the last tool.

| Measure | Budget | Measured | Verdict |
|---|---|---|---|
| Idle RSS, connected | <= 150 MB | 176,272 to 176,720 KiB = **172.1 to 172.6 MiB (180.5 to 181.0 MB)**, flat across the 30 samples | **missed**, by 22 MiB (15 %) against 150 MiB, or by 30.5 MB (20 %) against 150 decimal MB; M3 measured 153.3 MiB with a window that drew nothing |
| Idle CPU, connected, memory poll included | <= 1 % | `ps` shows 1.0 to 3.7 % (mean 2.0) but it decays from the start of the process, so it is not a rate; `top` over 31 one-second samples straight after: mean 2.4 % over all 31, max 8.5 % (the fifth sample; samples 1 to 5 are 1.6, 3.6, 5.7, 6.3 and 8.5 %, higher than the band after them: samples 6 to 31 are 1.1 to 2.5 %, mean 1.9 %) | **missed**, about 2 %; the `sample` tool (5 s) shows the threads in `__psynch_cvwait` and `mach_msg2_trap` nearly all the time (62,575 and 7,314 of about 88,000 samples), so the burst work it caught was tiny. The two do not agree; unresolved |

**Which RSS.** The budget is judged on `ps` RSS, as M3 did (172.1 to 172.6 MiB). `top -stats rsize` on the same pid, run right after, showed 366 to 367 MB in its first four samples and 300 to 304 MB after (appendix G), 1.7 to 2.1 times the `ps` figure. The reason is not known: `top` and `ps` count RSS differently on macOS (shared, compressed or purgeable pages are candidates), and I did not check which. If the `top` figure is the right one, the miss is larger. The `top` memory column is not used anywhere else in this record.

A second set came from `fdt.app` (the bundle from `scripts/bundle-macos.sh`) started and driven through the `argent` Mac-app tools, which add their own library to the process. Because of that it is not the primary number. On the Logs page: RSS 222,432 to 222,720 KiB (217.2 MiB, 30 samples), `top` CPU 1.69 % mean, 2.7 % max. On the Memory page as the window restored it (Classes tab, a range selected): RSS 165,504 to 188,384 KiB, `top` CPU mean 11.3 %, max 25 %. A window that finds itself on the Memory page with a range, redrawing, is therefore the expensive case; the idle budget is for "nothing selected" and this was not.

**After snapshots (not the idle budget; call 3).** The bundle again, Memory page, after two snapshots of `jank_lab` (#1 at 1:36 of the session, 204,291 objects; #2 at 3:48, 460,818 objects; #2 held, 55 suspects shown, a diff against #1): `ps` RSS 173,568 to 212,368 KiB (169.5 to 207.4 MiB, 30 samples, varying with the OS compressing pages), `top` CPU mean 0.78 %, max 1.1 %. The held index of #2 is about 28 bytes per reachable object, so a few MB; it is not the difference between the sets. The RSS "before" and "after" figures are therefore too close to separate on this machine, because the bundle's own number moved from 165 to 222 MiB without any snapshot. The window was left on the Memory page with the snapshot #1 present: that snapshot was not taken by this record's steps (it was already in the session at the first screenshot, and the session's origin is unknown); it is listed as an oddity.

**Attach to first live data, 5 runs** (`scripts/jank-lab-it.sh profile`, load 2.6 to 4.4; the machine's session was locked, but the app drew its frames anyway). Raw: appendix E.

| Run | `attach_ms` | `attach_to_data_ms` | `first_log_ms` | Test result |
|---|---|---|---|---|
| 1 | 8 | 130 | 106 | passed |
| 2 | 8 | not reached | | failed at step 6 (unmount check, left 0 right 1) |
| 3 | 10 | 126 | 104 | failed: tracing judged 0 frames |
| 4 | 8 | 36 | 15 | passed |
| 5 | 7 | 126 | 104 | passed |

`attach_ms` median 8 ms (M3: 126 ms once, M2: 5 ms). `attach_to_data_ms` median 126 ms over the four runs that reached it (36 to 130 ms) against <= 1 s: **met**. The run-4 figure is about a third of the others; not investigated. Snapshot transfer (reported, not budgeted): `received_ms` 12 to 34 ms for 5.1 to 6.4 MB; request to `Ready` 307 to 398 ms; the diff lands 26 ms after `Ready`.

## Not measured

| Item | Why |
|---|---|
| bench-ui pass 2 for `frame-chart` through `heap-analyse` | the display went to sleep at 15:52:14 and every run timed out. A repeat at 18:11 was cut off when the session locked at 18:10:10 ; its output is discarded |
| Idle RSS and CPU of a window on one fixed page, with and without a snapshot, without the `argent` library | no way to send keystrokes to a process started outside `argent` (`osascript` is not allowed), so the "after snapshot" figures come from the bundle driven through `argent` |
| Why idle CPU is about 2 % while `sample` shows an idle process | not investigated |
| Tracing overhead to better than a sign | the app's build time drifts more than the effect |
| Refresh-rate budget on a 60 Hz display | the only display is 120 Hz |
| Cold start (first paint <= 200 ms, interactive <= 400 ms) and palette open (one frame) | not run; M3 and M4 build more views at window open, so both may have moved since M2 |
| The pause copy, the progress and the tree on a real app with a large heap | `jank_lab` only (heaps of 5 to 20 MB); the 5M-object numbers are synthetic and a replicated real heap |
| Windows and Linux (including the 250-level reveal limit and the heap directory) | no such machine |
| The first CI run on Flutter 3.50 | not run here; the first leak scan against `package:listen` |

## Calls made and their outcomes

These are the plan's 15 calls, with what the verification can say about each.

1. **Heap snapshots are taken only on request** (⌘⇧S, the button, the palette). Kept. The app's pause was 307 to 398 ms from request to `Ready` including analysis (transfer 12 to 34 ms); the pause copy and the cancellable progress are built. Not seen on a large real heap.
2. **Binary frames go to a chunk channel**, tried as snapshot frames first. Kept. Five live runs in profile mode, plus the debug and profile runs of Task 18, produced no unreadable snapshot.
3. **Memory for the parsed graph.** Kept, and the budgets hold: peak 66.6 and 77.0 B per object (80), held index 28.1 B per reachable object (32). The replicated heap has only 3 B of headroom. Files are deleted on remove, new session or window close; a disconnect keeps them (a ruling, below). Idle RSS (172 MiB) is over budget before any snapshot, and the held index is not the cause.
4. **Memory chart data** polled every 500 ms, unchanged samples not stored, GC events filtered to the app's group. Kept; the poll is inside the idle CPU figure that missed (cause not found) and the overhead on the app at the median is +0.04 %.
5. **The allocation profile is fetched on demand only.** Kept; automatic fetches use `gc: false` (a ruling). Its pause was not measured here (15 to 17 ms in the planning facts).
6. **Dominator-based retained sizes, BFS shortest retaining paths.** Kept. Asserted by the live test's `path_text` (`Root -> static instance -> AppEvents._listeners -> ... -> _LeakyScreenState`).
7. **Unreachable objects are left out of every snapshot view.** Kept.
8. **Level-1 leak scan** with the versioned field table. Kept; in profile mode the notifier check is `Undecidable("profile")` as planned. The scan of Flutter 3.41 and 3.50 snapshots is on the checklist.
9. **The diff**, with lifecycle classes from any package. Kept; the diff found the leaked `ScrollController` and `TextEditingController` in the live test, 26 ms after `Ready`.
10. **"Held by HTTP recording".** Kept; checked live: 32 B and 0 requests in the baseline, 20,608 B and 1 request after `load_products`.
11. **Fixtures:** one real redacted profile snapshot (6.04 MB, 167,031 objects), rebuilt byte for byte by the encoder, with a privacy test. Kept.
12. **⌘⇧S from any page opens Memory > Leaks with a Back step.** Kept; shown working in the screenshot taken during this run (a snapshot taken from the Logs page landed on Leaks with "Receiving snapshot", then the result).
13. **`jank_lab` gains `goto_leaks` and `pump_frames`.** Kept; the memory checks ran with a locked session in these runs.
14. **Analysis in the capture, results in the store.** Kept; `heap-analyse` ran with the UI responsive (draw p99 2.2 ms, interval p99 8.7 ms at a normal cadence).
15. **Chart colours from existing tokens**, always with text labels. Kept; no new tokens.

### Rulings that changed the plan

The ledger's rulings, with their reasons and costs.

- **R1 (pre-flight):** Task 11 calls `request_heap_snapshot(main, cfg.request_timeout)`, because Task 2's signature needs a timeout and the config already has `heap_request_timeout`. Cost if wrong: one argument.
- **R2 (pre-flight):** Task 14 sets a pending class query and switches to Snapshot; Task 16 makes the search consume it. Task 15's "Show in snapshot" only emits `Selection::HeapNode`; Task 16 makes the tree reveal it. Reason: the consumer did not exist yet. Cost if wrong: Task 16 adds the consuming half.
- **Task 1:** "drawn == false (an empty page) clears" means no data to show; a result still computing keeps the request pending. Reason: the plan's parenthetical; M3 focused the tree once computed. Cost: a late result could steal focus from the palette (the dialog check still guards it).
- **Task 3:** `parse` and `decode_full` accept zero objects (well-formed); the "root" error is Task 6's. Cost: one more `Err` path.
- **Task 5 (1):** the stress memory series spans the stress session's existing 10-minute span, with 1,200 samples and 20,000 GCs; `latest_time` unchanged. Reason: spec section 10.1 (a 10-minute Timeline dataset) and keeping the M2 and M3 bench baselines comparable. Cost: the memory chart bench is lighter on samples (1,200 against the plan's 3,600).
- **Task 5 (2):** a 200-profile by 6,666-class test (rows at most 24 B, the class table interned once); `MAX_PROFILES` stays 1,000 (worst case about 160 MB debug, 75 MB profile). Cost if wrong: an M7 cap.
- **Task 7:** `unmount()` sets `_widget = null` and `_lifecycleState = defunct` together (H15), so either signal decides; the Element check is `Done` when at least one signal can run, `Undecidable` only when neither can; an I/O failure fails the analysis. Cost: a defunct Element whose `_widget` is non-null is missed when the enum is tree-shaken.
- **Task 8:** the hash-0 rule is deferred (H12 makes it unreachable for the classes tracked). Cost: a class grown only by hash-0 objects is not listed.
- **Task 9:** drop the stored per-node `idom` array; derive it from the contiguous child ranges (`partition_point` on `first_child`). Reason: the held index was 32.09 B per object, 0.3 % over; it is 28.1 now with the 32 B target still a hard assert. Cost: an O(log n) idom lookup in the tree reveal.
- **Task 11:** the post-snapshot allocation profile is for fresh takes only (a re-opened old snapshot would be stamped now, not then). Cancelling or failing the analysis of an `Open` restores `Ready`; a cancelled or failed fresh take drops its file; the hub stops at the first `last` chunk and the parser rejects trailing bytes. Cost: a rare snapshot fails and must be retaken; the user cannot retry analysis of a failed fresh take's file.
- **Task 11 (carried rulings):** a diff I/O error leaves the record `Ready` without a diff (a debug log, no paths); heap files are created with mode 0600 under `cfg(unix)`; no paths or snapshot bytes are logged. Reason: snapshot privacy (global constraints) and a failed diff should not lose a good analysis. Cost if wrong: a user sees no diff and no error for an unreadable baseline.
- **Task 12:** command labels follow the existing Title Case convention; other copy is sentence case. Cost: relabel 8 commands.
- **Task 14:** automatic fetches (tab open, after a snapshot) always use `gc: false`; `gc: true` only on an explicit Refresh with "Live objects only" on. Reason: a full GC is visible to every client and should follow a user act. Cost: an opened stale tab shows garbage-inclusive counts until Refresh.
- **Task 15 (1):** a new `Selection::LeakSnapshot(HeapId)` shown on Leaks; `Heap(HeapId)` stays Snapshot. Cost: one more variant to match. **Task 15 (copy):** the empty-state shortcut renders the platform's keys; the growth pane's `new_bytes` is labelled the new objects' size, not "Retains".
- **Task 16:** `MAX_REVEAL_DEPTH` is 250 on Windows (1 MB main-thread stack) and 500 elsewhere; past the limit the reveal degrades with a note. Cost: Windows users see the note sooner; never run on Windows (CI will).
- **Task 17:** the analysis thread sets `finished` from a drop guard; `memory-chart` reports its chart layout time per step. Cost: a few lines of instrumentation. (Used above: layout p99 0.87 ms.)
- **Task 18 (step 9):** the plan's "disconnect empties the heap directory" conflicts with call 3; the test checks files survive disconnect, then New Session empties the directory. Cost: none. **Task 18 (3):** the plan's one-second unmount sequence is kept; risk recorded for CI. **Task 18 (4):** wait up to 10 s for 3 new stored samples, each with RSS; no wall-clock span assert.
- **Task 19 (this record):** the measurement scripts gained optional flags (`--act`, `--trace`, `--long-ms` on `frame_build_sampler`; the connect time on `attach_hold`) so the fetch times and the tracing toggle could be measured; no product code changed. Cost: two small example edits.

**Observed in this verification, and bearing on a ruling:** the Task 18 ruling (3) risk came true once in five live runs (run 2, "one unmounted _LeakyScreenState", left 0, right 1, at the 1 s unmount sequence). **The cause is unknown.** The raw output shows only the assertion at `crates/fdt_capture/tests/jank_lab_live.rs:810` (left 0, right 1). It may be the 1 s unmount timing the ruling accepted, or a real unmount or leak-detection problem in the app or the capture; one failure in five cannot tell them apart. It happened on a Mac at load 4. Run 3's "jank (tracing): only 0 judged frames" (`jank_lab_live.rs:192`) is the same: cause unknown (M3's first live run failed the same way and passed on its second; that is not an explanation).

## Open decisions for the author

Numbers 1 to 4, 8 and 10 carry M3's unresolved decisions (M3's 1 to 4, 8, 6 and 7, and 9 as 9); M4's misses are 3 to 7. M3's decision 5 is closed.

1. **Draw p99 for Logs search and tool switch** is 6.9 to 7.2 and 9.9 ms against 4 ms (M2: 6.5 and 6.9; M3: 6.3 and 7.1). `tool-switch` grew by about 2.8 ms since M3 and its list now includes Memory. Cost: full-page text reshaping. Decide: change the design, or accept about 7 to 10 ms for these two interactions.
2. **Draw p99 for the M3 pages:** `frame-details` 5.11 ms (M3: 4.51, 4.59) against 4 ms; `trace-zoom` and `strip-drag` now meet; `net-filter` meets in one of two passes; `live-perf` 4.15. Dominant cost as M3 found it: the pane draw lands on the frames table's scrollbar-animation re-render. The options Task 24b listed are still unapplied.
3. **Idle memory of the connected window is about 172 MiB (181 MB) against 150 MB**, up from M3's 153.3 MiB (which was a locked, non-drawing window). With a drawing window the figure is higher, as M3 warned. M3's candidates (the window alone, +30 MiB since M2; the capture's 50 to 60 MiB) were partly addressed in M3's final wave (a typed timeline decode) but the connected window is still 22 MiB over. Decide whether to trim (and where: measure window-only, capture-only and M4's memory state separately) or to move the budget.
4. **The CPU-sample fetch costs one 129 to 168 ms frame every 15 s** (confirmed: 4 of 4 fetches in two runs at 15.2 and 30.2 s after connect; 6 of 6 connected overhead rounds). Decide: fetch on demand when a slow frame is opened with a longer background interval, accept about 9 dropped frames per 15 s, or fetch only while the Performance tool is open. The 2 % budget on the median is met either way.
5. **Idle CPU is about 2 % in `top` against 1 %** while `sample` shows an idle process; and on a Memory page with a range selected it was 11 % (mean, one run). Find the cause (a ticking "Rec" timer redrawing the title bar every second is a candidate) or move the budget. Not found here.
6. **`memory-chart` interval p99 (8.99 against 8.833) and `heap-tree` draw p99 (4.31 against 4)** on a loaded machine; our layout is 0.87 ms. Re-measure on a quiet, awake R1 before deciding anything.
7. **Heap analysis has 3 B per object of headroom on the replicated heap** (77.0 against 80). Decide whether to leave it (a new allocation on the analysis path then fails the strict budget test) or to find the margin.
8. **Tracing overhead is not resolved** (above); the copy's "about 30 %" is still the spike figure.
9. **Known flaky tests** (below), including one M4 test (`memory::hot_restart_moves_polling_to_the_new_group`) the ledger asked the final review to fix, the live test's unmount step (`jank_lab_live.rs:810`, run 2, cause unknown), and its tracing step (`jank_lab_live.rs:192`, "only 0 judged frames", run 3, cause unknown; it was also M3's first-run failure).
10. **Recent URLs keep the VM token in `settings.json`** (M2 decision 3), **bodies of unknown length are auto-fetched with a 10 s bound** (M2 decision 4), **new session (⌘N) discards the stored session with no confirmation and fires inside inputs** (M2 decision 7), **`fdt --connect` skips recent URLs** (M2 decision 6) and **Windows and Linux paths are read from source only** (M2 decision 5). All unchanged. M3's decision 5 (there is no CI remote) is closed: CI has run since 2026-11-23.
11. **Heap snapshot files live in `dirs::cache_dir()/fdt/heap`** (macOS `~/Library/Caches/fdt/heap`); the Windows and Linux locations are unchecked.

### Known flaky tests

- `fdt_capture` `memory::hot_restart_moves_polling_to_the_new_group` failed once in a full workspace run (Task 18), passed 3 reruns. M4 code; not fixed. It passed in this run's full workspace run.
- `fdt_capture` `load_full_body_after_the_app_stopped_changes_nothing` failed once in a full workspace run, passed 2 reruns (pre-existing).
- `fdt_store` `log_query::hostile_queries_are_bounded` failed a timing check under load once (547 ms); a timing assert in the normal suite, a candidate for `budget::check` (pre-existing).
- `lifecycle::new_session_starts_empty_and_keeping_the_connection` hit a settle-helper timeout once under load (M3, Task 11).
- `fdt_capture` `replay_debug` failed intermittently under load on the M2 baseline.
- The live `jank_lab` test: step 6's unmount check failed once in five profile runs here (above), and "only 0 judged frames" with tracing on failed once (as in M3).

## Author review checklist

Save screenshots next to this file as `m4-<os>-<theme>-<view>.png`.

- [ ] Visual review of the Memory tabs (Classes, Leaks, Snapshot) and the chart (with its GC marks, the heap and RSS lines, the selection and the snapshot markers), and the Timeline's GC and findings lanes, in light and dark, on macOS, Windows and Linux: the pause copy, the progress, the empty states, the "Profile mode can't tell..." copy, the diff list, the Snapshot tree and its details pane.
- [ ] A real app (not `jank_lab`) with a large heap: the pause copy, the progress while receiving and analysing, cancel, the tree and retaining paths, the time to Ready, and the idle memory after the snapshot.
- [ ] Flutter 3.41 and 3.50 snapshots through the leak scan (H17), to confirm the field table.
- [ ] **The first CI run on Flutter 3.50**, which is the first leak scan against `package:listen`; check that the live test's steps (the unmount at step 6, the 3 stored samples) hold on the hosted runners.
- [ ] **Windows:** the 250-level reveal limit (a snapshot with a deep chain: the "too deep" note appears and nothing crashes) and the heap directory's location; **Linux:** the heap directory's location.
- [ ] Keyboard-only pass: ⌘⇧S (Ctrl+Shift+S), Back, the chart's zoom keys (⌘= ⌘- ⌘0), the Classes/Leaks/Snapshot tabs, the tree's arrow keys and search, the palette's heap items.
- [ ] A hot restart and a full restart in profile mode: the chart's run boundary, polling moving to the new isolate group, the diff refusing snapshots of different runs, GC marks continuing.
- [ ] **Re-measure on a quiet, awake R1** (commands below): `memory-chart`, `heap-tree`, `tool-switch`, `frame-details`, `live-perf`, `logs-scroll`, the idle RSS and CPU of a drawing window on one page, and the remaining bench-ui scenarios in pass 2.
- [ ] **Investigate the two live-test failures** before relying on CI: step 6 "one unmounted _LeakyScreenState" (`jank_lab_live.rs:810`, left 0, right 1, run 2) and "jank (tracing): only 0 judged frames" (`jank_lab_live.rs:192`, run 3). Run the profile live test repeatedly, with a longer unmount wait, to tell timing from a real unmount or leak-detection fault.
- [ ] Fix or accept the other flaky tests listed above before relying on CI.

## Pending verification

All on an awake, unlocked R1, with `uptime` before each batch:

- **bench-ui, again, on a quiet machine:** `cargo build --release -p fdt_app` then, three times each, `./target/release/fdt bench-ui <scenario>` for every scenario, `memory-chart` and `heap-tree` first.
- **Idle:** `fdt --connect <ws uri>`, one named page, 30 s warm-up, then 30 samples of `ps -o rss=,%cpu= -p <pid>` and a 30-sample `top -l 31 -s 1 -pid <pid> -stats pid,cpu,rsize`; repeat after one snapshot. `ps` CPU decays from the start of the process, use `top`.
- **The CPU fetch:** repeat the two-run timing (appendix D) after any change to the poll; `frame_build_sampler --long-ms 40` prints the long frames' wall-clock times and `attach_hold` the connect time.
- **Tracing:** on a fixed workload, `frame_build_sampler --trace on|off`.
- **Attach, 5 runs:** `scripts/jank-lab-it.sh profile` five times.
- **The first CI run** on macOS, Windows and Linux, with Flutter 3.41.9 and 3.50.0.

## Appendix: deferred minors from the ledger, by area

Summarised from the SDD ledger (the full list is in `.superpowers/sdd/2026-11-21-m4-memory/progress.md`). None blocks the milestone; each is a small fix or a comment.

- **Heap format and parser** (Tasks 3, 4): a hostile header with length 0 skips a count bound and could abort on a large allocation; `refs.len()` is never checked against the declared reference count; `Graph.classes` has a `[0]` sentinel and `FullSnapshot.classes` does not; the object count is not bounded in `read_object`; the fixture's UTF-16 masker leaves one odd trailing byte unmasked at a buffer end; the recorder writes the fixture with default permissions.
- **Analysis and index** (Tasks 5 to 9): `HeapIndex::path` keeps the 200 hops nearest the node, so a leading ellipsis is needed when `path[0] != 0`; `HeapFile::new` is public and unchecked (one owner per path); the tracked-hash vector (12 B per tracked object) is alive through the dominator peak and unbounded for hash-heavy apps; `object_details` is a linear pass (0.1 to 0.2 s at 5M) with no cancel; the leak scan opens the snapshot file once per finding; `labelled_path` opens it per sample; `HeapIndex::idom` returns `n-1` silently for an out-of-range node; hash-0 objects never count as new in the diff (dormant, H12); the replicated peak has 3 B of headroom.
- **Capture** (Tasks 2, 10, 11): a GC event without a group id logs a warning each time; a missing `memoryUsage` becomes zeros; the profile fetch is a detached task and repeated fetches run concurrently; poll RPCs have no timeout; group state is duplicated in the router and the hub; a panicking transfer leaves its record busy until disconnect; the idle timer starts at the request; a hot restart between `take_heap` and `Msg::Take` can mislabel the isolate; diff jobs are not serialised or cancellable; `DISCONNECTED` is duplicated.
- **Views** (Tasks 12 to 16): `set_tab` can leave a selection that disagrees with the tab; the palette's heap item only navigates; the "Live objects only" note is only a tooltip; details pane code lives in `classes_tab.rs` (1,649 lines) rather than `details.rs`; `leaks_tab.rs` (about 2,100 lines with tests) and `snapshot_tab.rs` (about 2,500) are large and could split model from view; the baseline dropdown offers incomparable snapshots; "Show in snapshot" while disconnected opens a snapshot that cannot be loaded; the pin-invalidation path in the tree reveal and the Windows constant have no test that ran on Windows; the levels cache is uncapped.
- **Bench and live test** (Tasks 17, 18): the heap-tree scroll check is skipped when the tree has 50 rows or fewer; the `memory-chart` scenario has no Ready heap, so snapshot markers are unexercised; "Ready" means any heap Ready; the network-settle loop before `goto_network` has no deadline.
- **Tests:** several tests leak temp directories (`mem::forget`); some test helpers have unused parameters; no RED evidence for a few tasks (the ledger records which).

## Appendices: raw runs

The raw files are under `.superpowers/sdd/2026-11-21-m4-memory/task19-raw/`. Excerpts follow.


### A. Release budget tests (Step 1)

`FDT_BUDGETS=strict cargo test --workspace --release -- budget --nocapture`, measurement lines and results (the "test ... ok" lines are elided):

```
15:36  up 2 days, 22:51, 12 users, load averages: 5.44 5.15 5.29
    Finished `release` profile [optimized + debuginfo] target(s) in 36.15s
running 4 tests
call_tree_budget: top_down p50 277.167µs, bottom_up p50 1.855833ms, nodes 3000
flame_budget: 10 s range p50 5.545417ms, 100612 spans, truncated false
explain_budget: 100 slowest frames p50 1.357959ms, p99 2.048709ms, max 2.641208ms
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 114 filtered out; finished in 1.44s
running 2 tests
{"test":"heap_budget_5m_synthetic","objects":5000000,"reachable":3301406,"file_bytes":116530897,"parse_ms":308,"reach_leaks_ms":152,"dominators_index_ms":837,"total_ms":1298,"peak_bytes":333044033,"peak_per_object":66.6,"held_bytes":92752406,"held_per_reachable":28.09,"findings":2,"stage_peak_bytes":[187250752, 227613746, 333044033],"stage_start_live_bytes":[0, 186988704, 219571798]}
{"test":"heap_budget_5m_replicated","objects":4008721,"reachable":3294265,"file_bytes":143871633,"parse_ms":263,"reach_leaks_ms":55,"dominators_index_ms":572,"total_ms":890,"peak_bytes":308782167,"peak_per_object":77.0,"held_bytes":92507324,"held_per_reachable":28.08,"findings":24,"stage_peak_bytes":[161920006, 194232335, 308782167],"stage_start_live_bytes":[0, 161657958, 178096901]}
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 7.57s
running 14 tests
class_rows_budget: worst p50 1.166542ms
lane layout, 6 lanes, 72k frames: median 0.08 ms, worst 0.16 ms
render of the 50 MB body's stored MiB: 182.708µs
budget query len 1 worst 2.24625ms p90 1.629209ms
pretty-print of 1 MiB JSON: 22.067041ms
budget query len 2 worst 2.81325ms p90 1.590666ms
budget query len 3 worst 1.761708ms p90 1.672292ms
budget query len 4 worst 1.7255ms p90 1.661875ms
budget query len 6 worst 1.78125ms p90 1.670584ms
budget query len 7 worst 2.068125ms p90 1.6965ms
budget query len 7 worst 1.728833ms p90 1.640708ms
budget query len 8 worst 1.922875ms p90 1.769125ms
budget query len 3 worst 1.732584ms p90 1.616583ms
budget query len 100000 worst 593µs p90 475.375µs
budget query len 79 worst 24µs p90 23.833µs
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 603 filtered out; finished in 0.74s
running 1 test
assemble_budget: 199990 events, decode+feed p50 58.812458ms (decode alone p50 54.396583ms), runs [58.002083ms, 58.574875ms, 58.812458ms, 58.838792ms, 60.681625ms], 3.40 M events/s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 48 filtered out; finished in 0.32s
running 1 test
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 23 filtered out; finished in 0.11s
running 9 tests
strip_bars 72k frames, width 1000: p90 0.0018 ms, worst 0.0067 ms
synthetic 1M logs: 150 MB of text
full scan timeout: p90 36.2 ms, worst 41.7 ms
full scan Timeout: p90 2.5 ms, worst 2.6 ms
full scan user_42: p90 53.7 ms, worst 53.9 ms
full scan regex status=5\d\d: p90 52.9 ms, worst 61.5 ms
first results timeout: p90 2.7 ms, worst 2.9 ms
first results Timeout: p90 0.2 ms, worst 0.2 ms
first results user_42: p90 4.0 ms, worst 4.1 ms
first results regex status=5\d\d: p90 3.5 ms, worst 3.6 ms
net filter text products: p90 0.99 ms, worst 5.09 ms
net filter 4xx and 5xx: p90 0.32 ms, worst 0.41 ms
net filter host and POST: p90 0.52 ms, worst 0.56 ms
net filter errors tab: p90 0.28 ms, worst 0.35 ms
10,000 slices + publish: p90 0.8915 ms, worst 1.0841 ms
synth::perf(72_000): 4998740 slices (119 MB) + 3282703 cells (52 MB), drops TimelineDrops { depth: 0, overlap: 0, build_cap: 0, lod_coarse: 0 }
lane query over 6 UI lanes: p90 0.0262 ms, worst 0.0963 ms
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 120 filtered out; finished in 2.86s
15:36  up 2 days, 22:52, 12 users, load averages: 12.82 7.55 6.19
```

### B1. bench-ui, pass 1 (every scenario x 3)

One JSON line per run with the per-phase and per-view render fields removed; the uptime line is before each scenario.

```
## batch logs-scroll 15:37  up 2 days, 22:52, 12 users, load averages: 12.45 7.63 6.23
RUN 1 logs-scroll
{"scenario":"logs-scroll","error":"timed out","pass":false}
RUN 2 logs-scroll
{"scenario":"logs-scroll","error":"timed out","pass":false}
RUN 3 logs-scroll
{"scenario":"logs-scroll","error":"timed out","pass":false}
## batch logs-search 15:43  up 2 days, 22:58, 12 users, load averages: 3.73 5.33 5.61
RUN 1 logs-search
{"scenario":"logs-search","frames":87,"draws":88,"draw_ms":{"p50":1.005,"p95":4.094,"p99":7.465,"max":7.465},"interval_ms":{"p50":33.336,"p95":33.719,"p99":34.36,"max":34.36},"latency_ms":{"p50":34.71,"p95":38.635,"p99":38.635,"max":38.635},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"full_ms":{"p50":34.725,"p95":68.684,"p99":68.684,"max":68.684},"full_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.684,"p95":3.23,"p99":7.17,"max":7.17},"interval_ms":{"p50":8.331,"p95":8.541,"p99":8.596,"max":8.596},"latency_ms":{"p50":9.003,"p95":19.902,"p99":19.902,"max":19.902},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.863,"p95":36.042,"p99":36.042,"max":36.042},"full_frames":{"p50":3.0,"p95":4.0,"p99":4.0,"max":4.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 logs-search
{"scenario":"logs-search","frames":87,"draws":88,"draw_ms":{"p50":0.746,"p95":3.416,"p99":7.19,"max":7.19},"interval_ms":{"p50":33.087,"p95":33.607,"p99":37.297,"max":37.297},"latency_ms":{"p50":34.219,"p95":38.476,"p99":38.476,"max":38.476},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":34.254,"p95":38.476,"p99":38.476,"max":38.476},"full_frames":{"p50":1.0,"p95":4.0,"p99":4.0,"max":4.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch net-scroll 15:43  up 2 days, 22:58, 11 users, load averages: 3.83 5.30 5.60
RUN 1 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":3.069,"p95":4.273,"p99":4.64,"max":5.391},"interval_ms":{"p50":33.325,"p95":33.378,"p99":33.435,"max":37.5},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":3.42,"p95":4.271,"p99":4.47,"max":4.596},"interval_ms":{"p50":33.326,"p95":33.388,"p99":33.425,"max":37.525},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.568,"p95":2.91,"p99":3.076,"max":3.767},"interval_ms":{"p50":8.333,"p95":8.376,"p99":8.41,"max":11.662},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch net-filter 15:44  up 2 days, 22:59, 11 users, load averages: 4.18 5.23 5.56
RUN 1 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.598,"p95":3.101,"p99":4.7,"max":4.7},"interval_ms":{"p50":8.338,"p95":8.432,"p99":8.46,"max":8.46},"latency_ms":{"p50":10.989,"p95":13.055,"p99":13.055,"max":13.055},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.628,"p95":3.338,"p99":4.378,"max":4.378},"interval_ms":{"p50":8.332,"p95":8.438,"p99":8.448,"max":8.448},"latency_ms":{"p50":11.111,"p95":12.699,"p99":12.699,"max":12.699},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.537,"p95":3.197,"p99":4.121,"max":4.121},"interval_ms":{"p50":8.332,"p95":8.446,"p99":8.458,"max":8.458},"latency_ms":{"p50":11.068,"p95":12.455,"p99":12.455,"max":12.455},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch tool-switch 15:44  up 2 days, 22:59, 11 users, load averages: 4.32 5.24 5.56
RUN 1 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.67,"p95":6.035,"p99":10.806,"max":10.806},"interval_ms":{"p50":8.331,"p95":8.38,"p99":11.119,"max":11.119},"latency_ms":{"p50":12.028,"p95":14.392,"p99":19.214,"max":19.214},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.859,"p95":6.046,"p99":9.511,"max":9.511},"interval_ms":{"p50":8.334,"p95":8.365,"p99":9.735,"max":9.735},"latency_ms":{"p50":12.274,"p95":14.444,"p99":17.866,"max":17.866},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.821,"p95":5.701,"p99":9.878,"max":9.878},"interval_ms":{"p50":8.329,"p95":8.443,"p99":10.103,"max":10.103},"latency_ms":{"p50":12.209,"p95":14.33,"p99":18.223,"max":18.223},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch strip-drag 15:44  up 2 days, 22:59, 11 users, load averages: 4.51 5.25 5.56
RUN 1 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":2.088,"p95":2.619,"p99":2.777,"max":2.878},"interval_ms":{"p50":33.334,"p95":33.404,"p99":33.44,"max":34.363},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":2.153,"p95":2.655,"p99":3.066,"max":4.202},"interval_ms":{"p50":33.332,"p95":33.401,"p99":33.448,"max":33.975},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":1.458,"p95":1.764,"p99":1.857,"max":2.348},"interval_ms":{"p50":8.333,"p95":8.369,"p99":8.431,"max":9.116},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch live-logs 15:44  up 2 days, 23 hrs, 11 users, load averages: 3.99 5.07 5.48
RUN 1 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.592,"p95":2.16,"p99":2.272,"max":2.552},"interval_ms":{"p50":8.332,"p95":8.38,"p99":8.435,"max":16.712},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":2.273,"p95":3.229,"p99":4.547,"max":5.791},"interval_ms":{"p50":33.326,"p95":33.39,"p99":33.463,"max":37.498},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.687,"p95":2.4,"p99":2.73,"max":2.895},"interval_ms":{"p50":8.332,"p95":8.38,"p99":8.836,"max":12.124},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch frame-chart 15:45  up 2 days, 23 hrs, 11 users, load averages: 3.64 4.86 5.39
RUN 1 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":1.108,"p95":1.616,"p99":2.334,"max":2.711},"interval_ms":{"p50":8.332,"p95":8.392,"p99":8.52,"max":10.009},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":1.06,"p95":2.791,"p99":3.35,"max":3.998},"interval_ms":{"p50":33.309,"p95":33.412,"p99":33.528,"max":37.411},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 frame-chart
{"scenario":"frame-chart","frames":720,"draws":721,"draw_ms":{"p50":0.807,"p95":1.298,"p99":1.987,"max":2.815},"interval_ms":{"p50":8.333,"p95":8.388,"p99":8.754,"max":17.72},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch frame-details 15:45  up 2 days, 23:01, 11 users, load averages: 3.80 4.77 5.34
RUN 1 frame-details
{"scenario":"frame-details","frames":101,"draws":102,"draw_ms":{"p50":2.857,"p95":4.092,"p99":6.772,"max":10.768},"interval_ms":{"p50":8.336,"p95":8.383,"p99":8.48,"max":11.03},"latency_ms":{"p50":10.637,"p95":13.209,"p99":27.518,"max":27.518},"latency_frames":{"p50":1.0,"p95":1.0,"p99":2.0,"max":2.0},"samples":{"latency":50},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 frame-details
{"scenario":"frame-details","frames":100,"draws":101,"draw_ms":{"p50":2.968,"p95":4.13,"p99":5.107,"max":10.861},"interval_ms":{"p50":8.336,"p95":8.401,"p99":10.704,"max":14.28},"latency_ms":{"p50":10.705,"p95":13.134,"p99":21.617,"max":21.617},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":50},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 frame-details
{"scenario":"frame-details","frames":100,"draws":101,"draw_ms":{"p50":2.878,"p95":4.293,"p99":4.881,"max":11.218},"interval_ms":{"p50":8.33,"p95":8.419,"p99":8.454,"max":11.629},"latency_ms":{"p50":10.662,"p95":12.93,"p99":19.578,"max":19.578},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":50},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch timeline-zoom 15:45  up 2 days, 23:01, 11 users, load averages: 3.50 4.69 5.31
RUN 1 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":0.829,"p95":1.455,"p99":1.77,"max":2.718},"interval_ms":{"p50":8.331,"p95":8.517,"p99":8.98,"max":10.016},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":0.882,"p95":1.863,"p99":2.089,"max":2.844},"interval_ms":{"p50":8.329,"p95":8.555,"p99":8.645,"max":11.349},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 timeline-zoom
{"scenario":"timeline-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.032,"p95":1.886,"p99":2.515,"max":2.786},"interval_ms":{"p50":8.329,"p95":8.571,"p99":8.656,"max":9.425},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch trace-zoom 15:46  up 2 days, 23:01, 11 users, load averages: 3.85 4.71 5.30
RUN 1 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.551,"p95":2.313,"p99":3.488,"max":3.862},"interval_ms":{"p50":8.332,"p95":8.402,"p99":8.566,"max":9.036},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.929,"p95":3.198,"p99":3.854,"max":4.016},"interval_ms":{"p50":8.33,"p95":8.462,"p99":8.664,"max":17.701},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 trace-zoom
{"scenario":"trace-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.763,"p95":2.777,"p99":3.433,"max":4.099},"interval_ms":{"p50":8.331,"p95":8.425,"p99":8.591,"max":9.232},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch flame-zoom 15:46  up 2 days, 23:01, 11 users, load averages: 3.41 4.59 5.25
RUN 1 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.252,"p95":2.44,"p99":2.704,"max":2.912},"interval_ms":{"p50":8.331,"p95":8.516,"p99":8.662,"max":9.261},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.246,"p95":2.593,"p99":3.018,"max":3.774},"interval_ms":{"p50":8.333,"p95":8.545,"p99":8.737,"max":9.161},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 flame-zoom
{"scenario":"flame-zoom","frames":300,"draws":301,"draw_ms":{"p50":1.311,"p95":2.75,"p99":2.979,"max":3.213},"interval_ms":{"p50":8.329,"p95":8.545,"p99":8.868,"max":16.649},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch cpu-tree 15:46  up 2 days, 23:01, 11 users, load averages: 3.42 4.55 5.23
RUN 1 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":2.476,"p95":2.938,"p99":3.689,"max":3.891},"interval_ms":{"p50":8.333,"p95":8.404,"p99":8.722,"max":9.498},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":2.495,"p95":2.772,"p99":3.632,"max":3.882},"interval_ms":{"p50":8.334,"p95":8.378,"p99":8.453,"max":9.35},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 cpu-tree
{"scenario":"cpu-tree","frames":300,"draws":301,"draw_ms":{"p50":2.237,"p95":3.058,"p99":4.211,"max":4.794},"interval_ms":{"p50":8.334,"p95":8.398,"p99":8.531,"max":17.55},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch live-perf 15:46  up 2 days, 23:02, 11 users, load averages: 3.94 4.62 5.24
RUN 1 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":1.078,"p95":3.717,"p99":4.148,"max":6.508},"interval_ms":{"p50":8.334,"p95":8.387,"p99":8.676,"max":17.63},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":1.067,"p95":3.541,"p99":3.814,"max":4.372},"interval_ms":{"p50":8.334,"p95":8.384,"p99":8.412,"max":9.246},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 live-perf
{"scenario":"live-perf","frames":600,"draws":601,"draw_ms":{"p50":3.412,"p95":7.359,"p99":8.703,"max":10.825},"interval_ms":{"p50":33.263,"p95":33.403,"p99":33.57,"max":37.923},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch memory-chart 15:47  up 2 days, 23:02, 11 users, load averages: 3.83 4.51 5.18
RUN 1 memory-chart
{"scenario":"memory-chart","frames":300,"draws":301,"draw_ms":{"p50":1.241,"p95":1.633,"p99":2.455,"max":3.164},"interval_ms":{"p50":8.332,"p95":8.423,"p99":8.751,"max":17.581},"layout_ms":{"p50":0.314,"p95":0.469,"p99":0.546,"max":0.602},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 memory-chart
{"scenario":"memory-chart","frames":300,"draws":301,"draw_ms":{"p50":1.67,"p95":2.275,"p99":2.608,"max":2.767},"interval_ms":{"p50":8.329,"p95":8.675,"p99":8.986,"max":10.361},"layout_ms":{"p50":0.403,"p95":0.526,"p99":0.874,"max":1.967},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 memory-chart
{"scenario":"memory-chart","frames":300,"draws":301,"draw_ms":{"p50":1.436,"p95":2.43,"p99":2.825,"max":2.898},"interval_ms":{"p50":8.332,"p95":8.64,"p99":9.038,"max":9.712},"layout_ms":{"p50":0.366,"p95":0.477,"p99":0.932,"max":1.024},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch heap-tree 15:47  up 2 days, 23:02, 11 users, load averages: 7.20 5.23 5.43
RUN 1 heap-tree
{"scenario":"heap-tree","frames":329,"draws":330,"draw_ms":{"p50":2.283,"p95":3.286,"p99":4.309,"max":10.469},"interval_ms":{"p50":13.314,"p95":13.375,"p99":14.209,"max":21.874},"latency_ms":{"p50":2.677,"p95":7.157,"p99":7.157,"max":7.157},"latency_frames":{"p50":0.0,"p95":0.0,"p99":0.0,"max":0.0},"samples":{"latency":5},"interval_budget_ms":17.2,"refresh_ms":13.333,"pass":false}
RUN 2 heap-tree
{"scenario":"heap-tree","frames":329,"draws":330,"draw_ms":{"p50":2.289,"p95":2.779,"p99":3.447,"max":7.295},"interval_ms":{"p50":8.332,"p95":8.385,"p99":8.692,"max":9.735},"latency_ms":{"p50":2.527,"p95":7.332,"p99":7.332,"max":7.332},"latency_frames":{"p50":0.0,"p95":0.0,"p99":0.0,"max":0.0},"samples":{"latency":5},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 heap-tree
{"scenario":"heap-tree","frames":329,"draws":330,"draw_ms":{"p50":2.171,"p95":2.827,"p99":4.631,"max":9.805},"interval_ms":{"p50":8.335,"p95":13.34,"p99":13.371,"max":21.137},"latency_ms":{"p50":2.281,"p95":4.739,"p99":4.739,"max":4.739},"latency_frames":{"p50":0.0,"p95":0.0,"p99":0.0,"max":0.0},"samples":{"latency":5},"interval_budget_ms":17.2,"refresh_ms":13.333,"pass":false}
## batch heap-analyse 15:47  up 2 days, 23:03, 11 users, load averages: 7.56 5.43 5.49
RUN 1 heap-analyse
{"scenario":"heap-analyse","frames":162,"draws":163,"draw_ms":{"p50":0.964,"p95":1.313,"p99":2.155,"max":4.804},"interval_ms":{"p50":8.333,"p95":8.392,"p99":8.703,"max":8.931},"analysis_ms":1318.398,"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 heap-analyse
{"scenario":"heap-analyse","frames":39,"draws":40,"draw_ms":{"p50":0.94,"p95":1.301,"p99":5.38,"max":5.38},"interval_ms":{"p50":33.329,"p95":33.694,"p99":33.706,"max":33.706},"analysis_ms":1171.29,"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 heap-analyse
{"scenario":"heap-analyse","frames":146,"draws":147,"draw_ms":{"p50":0.949,"p95":1.189,"p99":1.461,"max":5.895},"interval_ms":{"p50":8.333,"p95":33.276,"p99":33.35,"max":33.368},"analysis_ms":1326.901,"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## end 15:47  up 2 days, 23:03, 11 users, load averages: 6.92 5.36 5.47
```

### B2. bench-ui, pass 2 (every scenario x 3; the display went off at 15:52 and `frame-chart` onward timed out)

One JSON line per run with the per-phase and per-view render fields removed; the uptime line is before each scenario.

```
## batch logs-scroll 15:49  up 2 days, 23:04, 11 users, load averages: 3.58 4.68 5.19
RUN 1 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.295,"p95":2.882,"p99":3.247,"max":5.377},"interval_ms":{"p50":8.332,"p95":8.389,"p99":8.85,"max":10.9},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.218,"p95":2.972,"p99":4.341,"max":5.322},"interval_ms":{"p50":8.333,"p95":8.398,"p99":8.563,"max":11.011},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 logs-scroll
{"scenario":"logs-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.356,"p95":2.981,"p99":4.196,"max":5.289},"interval_ms":{"p50":8.333,"p95":8.399,"p99":8.778,"max":17.523},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch logs-search 15:49  up 2 days, 23:05, 11 users, load averages: 3.54 4.60 5.15
RUN 1 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.53,"p95":3.358,"p99":6.888,"max":6.888},"interval_ms":{"p50":8.337,"p95":8.509,"p99":10.264,"max":10.264},"latency_ms":{"p50":8.85,"p95":13.901,"p99":13.901,"max":13.901},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"full_ms":{"p50":25.617,"p95":35.528,"p99":35.528,"max":35.528},"full_frames":{"p50":3.0,"p95":4.0,"p99":4.0,"max":4.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.569,"p95":3.213,"p99":6.764,"max":6.764},"interval_ms":{"p50":8.327,"p95":8.516,"p99":8.546,"max":8.546},"latency_ms":{"p50":8.944,"p95":19.854,"p99":19.854,"max":19.854},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.679,"p95":35.547,"p99":35.547,"max":35.547},"full_frames":{"p50":3.0,"p95":4.0,"p99":4.0,"max":4.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 logs-search
{"scenario":"logs-search","frames":89,"draws":90,"draw_ms":{"p50":0.535,"p95":3.142,"p99":6.905,"max":6.905},"interval_ms":{"p50":8.332,"p95":8.505,"p99":8.783,"max":8.783},"latency_ms":{"p50":8.866,"p95":19.777,"p99":19.777,"max":19.777},"latency_frames":{"p50":1.0,"p95":2.0,"p99":2.0,"max":2.0},"full_ms":{"p50":25.639,"p95":46.358,"p99":46.358,"max":46.358},"full_frames":{"p50":3.0,"p95":5.0,"p99":5.0,"max":5.0},"samples":{"first":18,"full":18},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch net-scroll 15:50  up 2 days, 23:05, 11 users, load averages: 4.54 4.79 5.21
RUN 1 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.717,"p95":3.129,"p99":3.303,"max":3.904},"interval_ms":{"p50":8.333,"p95":8.385,"p99":8.508,"max":9.42},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.787,"p95":3.094,"p99":3.277,"max":4.028},"interval_ms":{"p50":8.333,"p95":8.384,"p99":8.418,"max":10.06},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 net-scroll
{"scenario":"net-scroll","frames":600,"draws":601,"draw_ms":{"p50":2.819,"p95":3.136,"p99":3.33,"max":4.618},"interval_ms":{"p50":8.333,"p95":8.381,"p99":8.421,"max":9.397},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch net-filter 15:50  up 2 days, 23:05, 11 users, load averages: 4.41 4.75 5.19
RUN 1 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.43,"p95":3.165,"p99":3.98,"max":3.98},"interval_ms":{"p50":8.334,"p95":8.464,"p99":8.52,"max":8.52},"latency_ms":{"p50":11.204,"p95":12.413,"p99":12.413,"max":12.413},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 2 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.29,"p95":2.978,"p99":3.866,"max":3.866},"interval_ms":{"p50":8.33,"p95":8.44,"p99":8.627,"max":8.627},"latency_ms":{"p50":10.879,"p95":12.223,"p99":12.223,"max":12.223},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
RUN 3 net-filter
{"scenario":"net-filter","frames":37,"draws":38,"draw_ms":{"p50":2.52,"p95":3.55,"p99":3.965,"max":3.965},"interval_ms":{"p50":8.333,"p95":8.473,"p99":8.505,"max":8.505},"latency_ms":{"p50":11.429,"p95":12.391,"p99":12.391,"max":12.391},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":8},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch tool-switch 15:50  up 2 days, 23:05, 11 users, load averages: 5.62 4.99 5.27
RUN 1 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.572,"p95":6.129,"p99":9.242,"max":9.242},"interval_ms":{"p50":8.327,"p95":8.411,"p99":9.474,"max":9.474},"latency_ms":{"p50":11.941,"p95":14.607,"p99":17.531,"max":17.531},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":5.188,"p95":10.664,"p99":13.555,"max":13.555},"interval_ms":{"p50":33.338,"p95":33.388,"p99":34.116,"max":34.116},"latency_ms":{"p50":38.728,"p95":44.038,"p99":47.17,"max":47.17},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 tool-switch
{"scenario":"tool-switch","frames":41,"draws":42,"draw_ms":{"p50":3.47,"p95":6.106,"p99":9.98,"max":9.98},"interval_ms":{"p50":8.332,"p95":8.723,"p99":10.316,"max":10.316},"latency_ms":{"p50":11.755,"p95":14.705,"p99":18.382,"max":18.382},"latency_frames":{"p50":1.0,"p95":1.0,"p99":1.0,"max":1.0},"samples":{"latency":40},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch strip-drag 15:50  up 2 days, 23:05, 11 users, load averages: 5.41 4.96 5.26
RUN 1 strip-drag
{"scenario":"strip-drag","error":"the drag did not select a time range on the strip","pass":false}
RUN 2 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":2.142,"p95":2.548,"p99":4.137,"max":4.537},"interval_ms":{"p50":8.335,"p95":8.404,"p99":8.926,"max":16.824},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 strip-drag
{"scenario":"strip-drag","frames":300,"draws":301,"draw_ms":{"p50":1.777,"p95":2.631,"p99":3.122,"max":4.413},"interval_ms":{"p50":8.332,"p95":8.407,"p99":8.638,"max":9.201},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":true}
## batch live-logs 15:50  up 2 days, 23:06, 11 users, load averages: 4.81 4.85 5.21
RUN 1 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":3.171,"p95":5.677,"p99":6.029,"max":7.005},"interval_ms":{"p50":33.322,"p95":33.524,"p99":33.661,"max":37.603},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 2 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":3.104,"p95":5.814,"p99":6.177,"max":6.59},"interval_ms":{"p50":33.315,"p95":33.511,"p99":33.76,"max":37.927},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
RUN 3 live-logs
{"scenario":"live-logs","frames":600,"draws":601,"draw_ms":{"p50":1.994,"p95":3.282,"p99":5.626,"max":6.349},"interval_ms":{"p50":8.341,"p95":33.357,"p99":33.484,"max":33.743},"interval_budget_ms":8.833,"refresh_ms":8.333,"pass":false}
## batch frame-chart 15:51  up 2 days, 23:06, 11 users, load averages: 4.76 4.91 5.22
RUN 1 frame-chart
{"scenario":"frame-chart","error":"timed out","pass":false}
RUN 2 frame-chart
{"scenario":"frame-chart","error":"timed out","pass":false}
RUN 3 frame-chart
{"scenario":"frame-chart","error":"timed out","pass":false}
## batch frame-details 15:57  up 2 days, 23:12, 11 users, load averages: 1.91 2.97 4.18
RUN 1 frame-details
{"scenario":"frame-details","error":"timed out","pass":false}
RUN 2 frame-details
{"scenario":"frame-details","error":"timed out","pass":false}
RUN 3 frame-details
{"scenario":"frame-details","error":"timed out","pass":false}
## batch timeline-zoom 16:03  up 2 days, 23:19, 11 users, load averages: 1.78 2.35 3.47
RUN 1 timeline-zoom
{"scenario":"timeline-zoom","error":"timed out","pass":false}
RUN 2 timeline-zoom
{"scenario":"timeline-zoom","error":"timed out","pass":false}
RUN 3 timeline-zoom
{"scenario":"timeline-zoom","error":"timed out","pass":false}
## batch trace-zoom 16:09  up 2 days, 23:25, 11 users, load averages: 1.38 1.74 2.79
RUN 1 trace-zoom
{"scenario":"trace-zoom","error":"timed out","pass":false}
RUN 2 trace-zoom
{"scenario":"trace-zoom","error":"timed out","pass":false}
RUN 3 trace-zoom
{"scenario":"trace-zoom","error":"timed out","pass":false}
## batch flame-zoom 16:15  up 2 days, 23:31, 11 users, load averages: 2.05 1.81 2.45
RUN 1 flame-zoom
{"scenario":"flame-zoom","error":"timed out","pass":false}
RUN 2 flame-zoom
{"scenario":"flame-zoom","error":"timed out","pass":false}
RUN 3 flame-zoom
{"scenario":"flame-zoom","error":"timed out","pass":false}
## batch cpu-tree 16:21  up 2 days, 23:37, 11 users, load averages: 3.53 2.88 2.73
RUN 1 cpu-tree
{"scenario":"cpu-tree","error":"timed out","pass":false}
RUN 2 cpu-tree
{"scenario":"cpu-tree","error":"timed out","pass":false}
RUN 3 cpu-tree
{"scenario":"cpu-tree","error":"timed out","pass":false}
## batch live-perf 16:27  up 2 days, 23:43, 11 users, load averages: 2.19 2.76 2.75
RUN 1 live-perf
{"scenario":"live-perf","error":"timed out","pass":false}
RUN 2 live-perf
{"scenario":"live-perf","error":"timed out","pass":false}
RUN 3 live-perf
{"scenario":"live-perf","error":"timed out","pass":false}
## batch memory-chart 16:34  up 2 days, 23:49, 11 users, load averages: 2.69 3.05 2.90
RUN 1 memory-chart
{"scenario":"memory-chart","error":"timed out","pass":false}
RUN 2 memory-chart
{"scenario":"memory-chart","error":"timed out","pass":false}
RUN 3 memory-chart
{"scenario":"memory-chart","error":"timed out","pass":false}
## batch heap-tree 16:40  up 2 days, 23:55, 11 users, load averages: 3.04 2.91 2.87
RUN 1 heap-tree
{"scenario":"heap-tree","error":"timed out","pass":false}
RUN 2 heap-tree
{"scenario":"heap-tree","error":"timed out","pass":false}
RUN 3 heap-tree
{"scenario":"heap-tree","error":"timed out","pass":false}
## batch heap-analyse 16:46  up 3 days, 1 min, 11 users, load averages: 2.56 2.67 2.76
RUN 1 heap-analyse
{"scenario":"heap-analyse","error":"timed out","pass":false}
RUN 2 heap-analyse
{"scenario":"heap-analyse","error":"timed out","pass":false}
RUN 3 heap-analyse
{"scenario":"heap-analyse","error":"timed out","pass":false}
## end 16:52  up 3 days, 7 mins, 11 users, load averages: 2.63 3.12 3.02
```

### C. Overhead runs on `/jank` (Step 3)

Each line: mode, then the sampler's output (`build_us` in microseconds); `long` lines are frames of 40 ms or more with their epoch time. The discarded first attempt is `C-overhead-jank-discarded-settle-bug.txt` in the raw folder.

```
17:57  up 3 days,  1:12, 11 users, load averages: 3.47 3.74 3.57
--- jank
## round 1 jank 17:57  up 3 days,  1:12, 11 users, load averages: 3.47 3.74 3.57
bare frames=150 first_number=5523 last_number=5672 seconds=3.8 build_us median=24054 mean=24062.1 p90=24093 p99=24162 min=24033 max=24227
conn long number=6278 build_us=155606 at_epoch_ms=1795615061127
frames=150 first_number=6147 last_number=6296 seconds=3.8 build_us median=24075 mean=24958.4 p90=24119 p99=24195 min=24039 max=155606
## round 2 jank 17:57  up 3 days,  1:13, 11 users, load averages: 2.98 3.60 3.52
conn long number=6977 build_us=160367 at_epoch_ms=1795615079061
frames=150 first_number=6847 last_number=6996 seconds=3.9 build_us median=24077 mean=24990.5 p90=24129 p99=24202 min=24034 max=160367
bare frames=150 first_number=7151 last_number=7300 seconds=3.8 build_us median=24078 mean=24083.5 p90=24133 p99=24181 min=24035 max=24247
## round 3 jank 17:58  up 3 days,  1:13, 11 users, load averages: 2.95 3.54 3.50
bare frames=150 first_number=7375 last_number=7524 seconds=3.8 build_us median=24082 mean=24085.4 p90=24131 p99=24196 min=24037 max=24260
conn long number=8129 build_us=164737 at_epoch_ms=1795615108928
frames=150 first_number=7999 last_number=8148 seconds=3.9 build_us median=24088 mean=25033.8 p90=24154 p99=24200 min=24040 max=164737
## round 4 jank 17:58  up 3 days,  1:13, 11 users, load averages: 2.67 3.44 3.46
conn long number=8834 build_us=162619 at_epoch_ms=1795615127010
frames=150 first_number=8703 last_number=8852 seconds=3.8 build_us median=24092 mean=25025.5 p90=24157 p99=24234 min=24043 max=162619
bare frames=150 first_number=9003 last_number=9152 seconds=3.8 build_us median=24077 mean=24088.5 p90=24144 p99=24213 min=24036 max=24217
## round 5 jank 17:58  up 3 days,  1:14, 11 users, load averages: 2.54 3.35 3.43
bare frames=150 first_number=9227 last_number=9376 seconds=3.8 build_us median=24093 mean=24101.4 p90=24171 p99=24207 min=24036 max=24215
conn long number=9982 build_us=167633 at_epoch_ms=1795615156760
frames=150 first_number=9851 last_number=10000 seconds=3.8 build_us median=24099 mean=25061.0 p90=24164 p99=24241 min=24039 max=167633
## round 6 jank 17:59  up 3 days,  1:14, 11 users, load averages: 2.52 3.27 3.40
conn long number=10683 build_us=163004 at_epoch_ms=1795615174753
frames=150 first_number=10551 last_number=10700 seconds=3.8 build_us median=24109 mean=25037.6 p90=24168 p99=24222 min=24039 max=163004
bare frames=150 first_number=10852 last_number=11001 seconds=3.7 build_us median=24067 mean=24076.1 p90=24113 p99=24206 min=24034 max=24206
17:59  up 3 days,  1:15, 11 users, load averages: 2.20 3.14 3.35
```

### D. CPU fetch timing (Step 3)

`attach_hold` (prints `connected at_epoch_ms`) and a 1,500-frame sampler at once; the line starting `1795...N` is a stray `date` call that this macOS does not support, ignore it.

```
## fetch-timing run 1 17:59  up 3 days,  1:15, 11 users, load averages: 2.27 3.14 3.35
17956151953N
long number=11963 build_us=137074 at_epoch_ms=1795615207174
long number=12558 build_us=151163 at_epoch_ms=1795615222181
frames=1500 first_number=11519 last_number=13018 seconds=37.6 build_us median=24095 mean=24265.5 p90=24180 p99=24233 min=24032 max=151163
connected at_epoch_ms=1795615191932
done logs=21 frames=1589 cpu_samples=19874
## fetch-timing run 2 18:00  up 3 days,  1:15, 11 users, load averages: 2.88 3.18 3.35
17956152373N
long number=13625 build_us=128972 at_epoch_ms=1795615248966
long number=14220 build_us=156834 at_epoch_ms=1795615263973
frames=1500 first_number=13181 last_number=14680 seconds=37.7 build_us median=24085 mean=24254.8 p90=24168 p99=24219 min=24033 max=156834
connected at_epoch_ms=1795615233732
done logs=16 frames=1589 cpu_samples=19711

18:01  up 3 days,  1:16, 11 users, load averages: 4.49 3.50 3.45
```

### E. Attach, 5 runs (Step 4)

`scripts/jank-lab-it.sh profile`, relevant lines (the full output is in the raw folder).

```
--- run 1
17:43  up 3 days, 58 mins, 11 users, load averages: 3.15 3.10 3.05
attach_ms=8 debug=false
memory: samples ok heap_used=11070960 rss=Some(148635648)
memory: gc ok
heap[0]: wait_ms=315 bytes=5083240 received_ms=Some(13) objects=138599 reachable=137526
heap[1]: wait_ms=356 bytes=6254158 received_ms=Some(22) objects=181444 reachable=137771
heap[1]: analysis checks=LeakChecks { states: Done, elements: Done, notifiers: Undecidable("profile") } findings=1 http=Some(HttpHeld { bytes: 32, requests: 0 })
diff_ms=26
heap[2]: wait_ms=355 bytes=5748497 received_ms=Some(12) objects=163918 reachable=146949
memory: cleanup ok
attach_to_data_ms=130
first_log_ms=106
jank: after 3 s frames=113 with_phases=113 timeline_slices=21
jank: frames=113 with_phases=113 wait_ms=3003
exit=0
17:44  up 3 days, 59 mins, 11 users, load averages: 2.60 2.99 3.01
--- run 2
17:46  up 3 days,  1:01, 11 users, load averages: 4.16 3.56 3.24
attach_ms=8 debug=false
memory: samples ok heap_used=11121152 rss=Some(157581312)
memory: gc ok
heap[0]: wait_ms=324 bytes=5083142 received_ms=Some(12) objects=138600 reachable=137527
heap[1]: wait_ms=390 bytes=5411846 received_ms=Some(16) objects=154094 reachable=144075
heap[1]: analysis checks=LeakChecks { states: Done, elements: Done, notifiers: Undecidable("profile") } findings=0 http=Some(HttpHeld { bytes: 32, requests: 0 })
thread 'live_capture_of_jank_lab' (6403575) panicked at crates/fdt_capture/tests/jank_lab_live.rs:810:5:
assertion `left == right` failed: one unmounted _LeakyScreenState
  left: 0
 right: 1
exit=101
17:46  up 3 days,  1:02, 11 users, load averages: 4.43 3.66 3.28
--- run 3
17:46  up 3 days,  1:02, 11 users, load averages: 4.43 3.66 3.28
attach_ms=10 debug=false
memory: samples ok heap_used=11127520 rss=Some(157925376)
memory: gc ok
heap[0]: wait_ms=333 bytes=5083260 received_ms=Some(12) objects=138600 reachable=137527
heap[1]: wait_ms=385 bytes=6255362 received_ms=Some(33) objects=181647 reachable=137771
heap[1]: analysis checks=LeakChecks { states: Done, elements: Done, notifiers: Undecidable("profile") } findings=1 http=Some(HttpHeld { bytes: 32, requests: 0 })
diff_ms=26
heap[2]: wait_ms=398 bytes=6071627 received_ms=Some(34) objects=164207 reachable=146626
memory: cleanup ok
attach_to_data_ms=126
first_log_ms=104
jank: after 3 s frames=117 with_phases=102 timeline_slices=20
jank: frames=117 with_phases=102 wait_ms=3003
thread 'live_capture_of_jank_lab' (6407081) panicked at crates/fdt_capture/tests/jank_lab_live.rs:192:5:
budget missed: jank (tracing): only 0 judged frames
exit=101
17:47  up 3 days,  1:03, 11 users, load averages: 3.78 3.61 3.28
--- run 4
17:47  up 3 days,  1:03, 11 users, load averages: 3.78 3.61 3.28
attach_ms=8 debug=false
memory: samples ok heap_used=11217712 rss=Some(166133760)
memory: gc ok
heap[0]: wait_ms=316 bytes=5091232 received_ms=Some(12) objects=138936 reachable=137863
heap[1]: wait_ms=358 bytes=6339900 received_ms=Some(16) objects=186402 reachable=138106
heap[1]: analysis checks=LeakChecks { states: Done, elements: Done, notifiers: Undecidable("profile") } findings=1 http=Some(HttpHeld { bytes: 32, requests: 0 })
diff_ms=26
heap[2]: wait_ms=386 bytes=6417315 received_ms=Some(13) objects=174811 reachable=147285
memory: cleanup ok
attach_to_data_ms=36
first_log_ms=15
jank: after 3 s frames=112 with_phases=112 timeline_slices=20
jank: frames=112 with_phases=112 wait_ms=3002
exit=0
17:48  up 3 days,  1:03, 11 users, load averages: 3.87 3.65 3.31
--- run 5
17:48  up 3 days,  1:03, 11 users, load averages: 3.87 3.65 3.31
attach_ms=7 debug=false
memory: samples ok heap_used=11100864 rss=Some(155353088)
memory: gc ok
heap[0]: wait_ms=335 bytes=5085930 received_ms=Some(12) objects=138705 reachable=137527
heap[1]: wait_ms=347 bytes=6238092 received_ms=Some(17) objects=180506 reachable=137771
heap[1]: analysis checks=LeakChecks { states: Done, elements: Done, notifiers: Undecidable("profile") } findings=1 http=Some(HttpHeld { bytes: 32, requests: 0 })
diff_ms=26
heap[2]: wait_ms=349 bytes=5591295 received_ms=Some(13) objects=157304 reachable=146950
memory: cleanup ok
attach_to_data_ms=126
first_log_ms=104
jank: after 3 s frames=116 with_phases=96 timeline_slices=20
jank: frames=116 with_phases=96 wait_ms=3001
exit=0
17:49  up 3 days,  1:04, 11 users, load averages: 4.04 3.69 3.35
```

### F. Widget-build tracing on `/rebuilds` (Step 3)

```
18:01  up 3 days,  1:16, 11 users, load averages: 4.03 3.43 3.43
--- rebuilds, tracing off/on via ext.flutter.profileUserWidgetBuilds (sampler only, no fdt)
## round 1 18:01  up 3 days,  1:16, 11 users, load averages: 4.03 3.43 3.43
trace-off frames=150 first_number=15272 last_number=15421 seconds=2.3 build_us median=2121 mean=2110.4 p90=3510 p99=4677 min=479 max=5392
trace-on frames=150 first_number=15631 last_number=15780 seconds=1.5 build_us median=1551 mean=1943.4 p90=3255 p99=4848 min=641 max=5744
## round 2 18:01  up 3 days,  1:16, 11 users, load averages: 4.02 3.45 3.43
trace-on frames=150 first_number=16024 last_number=16173 seconds=1.1 build_us median=1485 mean=1692.0 p90=2516 p99=4467 min=682 max=4734
trace-off frames=150 first_number=16419 last_number=16568 seconds=1.2 build_us median=1152 mean=1215.3 p90=1860 p99=2720 min=461 max=2720
## round 3 18:01  up 3 days,  1:16, 11 users, load averages: 4.02 3.46 3.44
trace-off frames=150 first_number=16821 last_number=16970 seconds=1.1 build_us median=1411 mean=1387.3 p90=2122 p99=2464 min=464 max=2622
trace-on frames=150 first_number=17214 last_number=17363 seconds=1.2 build_us median=2771 mean=2852.5 p90=4074 p99=5180 min=717 max=5454
## round 4 18:01  up 3 days,  1:17, 11 users, load averages: 4.27 3.52 3.46
trace-on frames=150 first_number=17617 last_number=17766 seconds=1.2 build_us median=3077 mean=3180.3 p90=4801 p99=5551 min=920 max=6247
trace-off frames=150 first_number=18017 last_number=18166 seconds=1.2 build_us median=2551 mean=2589.4 p90=3352 p99=3941 min=1064 max=4657
## round 5 18:01  up 3 days,  1:17, 11 users, load averages: 4.33 3.55 3.47
trace-off frames=150 first_number=18421 last_number=18570 seconds=1.2 build_us median=2956 mean=2917.3 p90=3717 p99=4582 min=1545 max=4765
trace-on frames=150 first_number=18822 last_number=18971 seconds=1.2 build_us median=4469 mean=4494.5 p90=6027 p99=6997 min=1628 max=7028
## round 6 18:02  up 3 days,  1:17, 11 users, load averages: 5.42 3.79 3.55
trace-on frames=150 first_number=19226 last_number=19375 seconds=1.2 build_us median=4583 mean=4525.5 p90=6129 p99=7912 min=2193 max=8057
trace-off frames=150 first_number=19627 last_number=19776 seconds=1.1 build_us median=3152 mean=3192.1 p90=4303 p99=5412 min=1457 max=6320
18:02  up 3 days,  1:17, 11 users, load averages: 5.12 3.78 3.55
```

### G. Idle memory and CPU (Step 4)

First the plain binary (`ps` lines are `rss_kb cpu%`, `ps` RSS in KiB; `ps` CPU decays from process start), then three bundle sets (driven through `argent`).

```
locked? IOConsoleLocked" = No
18:02  up 3 days,  1:18, 11 users, load averages: 4.14 3.71 3.54
pid 30649
warm-up 30 s done; samples (rss_kb cpu%)
176272   1.9
176272   3.7
176288   2.4
176304   3.0
176304   3.3
176304   3.2
176304   2.5
176304   2.4
176336   2.3
176352   2.4
176352   1.4
176368   1.9
176368   2.0
176368   1.4
176368   2.0
176368   1.0
176368   1.6
176416   1.1
176416   1.7
176416   1.2
176416   1.7
176432   1.0
176448   1.4
176448   1.3
176480   1.7
176480   1.2
176704   2.1
176704   1.9
176720   3.3
176720   2.7
18:03  up 3 days,  1:19, 11 users, load averages: 2.25 3.24 3.38
locked after? IOConsoleLocked" = No
--- take one snapshot via script? (see G-after)

--- top, 31 samples (pid cpu% rsize)
18:04  up 3 days,  1:19, 11 users, load averages: 2.21 3.20 3.36
30649  1.6  366M 366M
30649  3.6  366M+ 366M+
30649  5.7  367M+ 367M+
30649  6.3  367M+ 367M+
30649  8.5  300M- 300M-
30649  2.0  304M+ 304M+
30649  1.9  304M+ 304M+
30649  1.5  304M  304M 
30649  1.1  304M+ 304M+
30649  2.5  304M+ 304M+
30649  2.4  304M  304M 
30649  2.1  304M+ 304M+
30649  1.8  304M+ 304M+
30649  2.2  304M  304M 
30649  2.1  304M  304M 
30649  2.1  304M  304M 
30649  1.7  304M  304M 
30649  1.9  304M  304M 
30649  2.0  304M  304M 
30649  2.1  304M  304M 
30649  2.0  304M+ 304M+
30649  1.8  304M  304M 
30649  1.5  304M  304M 
30649  1.5  304M  304M 
30649  2.0  304M+ 304M+
30649  2.2  304M  304M 
30649  1.4  304M+ 304M+
30649  1.3  304M  304M 
30649  1.8  304M+ 304M+
30649  2.0  304M  304M 
30649  1.9  304M+ 304M+
18:04  up 3 days,  1:19, 11 users, load averages: 2.47 3.16 3.34

=== bundle fdt.app, Memory page as found ===
bundle fdt.app pid 31618; before snapshot; warm-up 30 s
18:05  up 3 days,  1:21, 11 users, load averages: 1.82 2.80 3.18
ps rss_kb cpu% (30 x 1 s)
165504   2.1
165536   3.2
165536   3.4
165552   1.8
165552   1.4
165552   2.7
165584   2.1
165840   3.0
165840   1.6
165840   2.2
165840   2.9
165840   2.2
165856   2.3
165888   1.2
165888   1.9
165888   1.4
165904   1.9
165920   2.0
165920   0.7
166048   3.9
177008   7.5
177504   4.7
177680   7.0
177728  15.8
177728   9.8
178304   5.8
184384   6.7
184400   7.0
184672   3.8
188384   8.2
18:06  up 3 days,  1:21, 11 users, load averages: 2.50 2.86 3.19

18:06  up 3 days,  1:21, 11 users, load averages: 2.50 2.86 3.19
31618  4.3  317M+
31618  10.9 325M+
31618  3.8  325M+
31618  4.5  325M+
31618  6.5  325M 
31618  9.0  325M-
31618  12.1 335M+
31618  16.3 335M 
31618  11.5 335M+
31618  6.3  335M+
31618  5.0  335M+
31618  5.2  335M 
31618  15.4 335M+
31618  13.4 336M+
31618  10.1 336M+
31618  18.7 336M+
31618  20.8 336M+
31618  8.5  336M+
31618  12.5 336M+
31618  25.0 336M 
31618  11.3 336M 
31618  17.9 336M 
31618  15.6 336M 
31618  11.8 336M 
31618  9.5  336M+
31618  11.9 336M+
31618  6.4  336M 
31618  9.3  349M+
31618  13.7 349M 
31618  12.6 349M+
18:07  up 3 days,  1:22, 11 users, load averages: 2.54 2.82 3.16

=== bundle fdt.app, Logs page ===
bundle fdt.app pid 31618; Logs page, connected to idle jank_lab (profile); no snapshot; 30 s after navigating
18:08  up 3 days,  1:23, 11 users, load averages: 2.13 2.66 3.08
222432   2.1
222432   0.5
222432   1.4
222432   1.8
222432   0.9
222432   0.2
222432   0.7
222640   1.1
222640   1.3
222640   3.7
222640   3.4
222640   2.4
222640   1.4
222640   2.2
222656   3.7
222720   3.2
222720   2.3
222720   3.1
222720   2.4
222720   1.9
222720   2.1
222720   1.2
222720   1.2
222720   1.0
222720   2.0
222720   1.8
222720   0.9
222720   1.0
222720   1.1
222720   0.4
18:08  up 3 days,  1:23, 11 users, load averages: 2.07 2.58 3.03

18:08  up 3 days,  1:23, 11 users, load averages: 2.07 2.58 3.03
31618  0.9  330M-
31618  2.0  330M 
31618  1.2  330M+
31618  1.7  330M 
31618  2.1  330M 
31618  1.9  330M 
31618  1.2  330M-
31618  1.5  330M+
31618  1.2  330M 
31618  1.1  330M 
31618  1.6  330M 
31618  0.6  330M 
31618  1.9  330M 
31618  1.5  330M 
31618  1.9  330M 
31618  2.1  330M 
31618  2.7  330M 
31618  2.2  330M 
31618  1.5  330M+
31618  1.5  330M 
31618  1.3  330M 
31618  2.3  330M 
31618  1.5  330M+
31618  1.9  330M 
31618  2.3  330M 
31618  2.0  330M+
31618  2.1  330M 
31618  1.9  330M 
31618  2.1  330M 
31618  0.9  330M 
18:09  up 3 days,  1:24, 11 users, load averages: 2.01 2.53 2.99

=== bundle fdt.app, after snapshots #1 and #2 ===
bundle fdt.app pid 31618; Memory page after snapshots #1 and #2 (#2 held, 460,818 objects); 30 s later
18:09  up 3 days,  1:25, 11 users, load averages: 2.06 2.46 2.94
211920   6.6
211920   3.9
211920   4.1
211936   5.8
211936   7.2
211936   5.3
211936   6.0
211936   4.2
211936   4.0
211936   4.4
211936   4.8
212176   3.1
212176   3.9
212176   4.0
212176   1.7
212368   1.8
173568   0.2
173568   0.5
173568   0.5
173568   0.1
173584   0.2
173584   0.0
173584   0.0
173584   0.0
173584   0.2
173584   0.7
173584   1.1
173584   0.7
173648   1.1
173648   0.8
18:10  up 3 days,  1:25, 11 users, load averages: 1.57 2.30 2.87

18:10  up 3 days,  1:25, 11 users, load averages: 1.57 2.30 2.87
31618  0.5  155M
31618  0.6  155M
31618  1.1  155M
31618  0.7  155M
31618  0.9  156M+
31618  0.8  156M 
31618  0.7  156M 
31618  1.1  156M 
31618  0.7  156M 
31618  0.6  156M 
31618  0.7  156M 
31618  0.8  156M 
31618  0.6  156M 
31618  0.9  156M 
31618  1.0  156M 
31618  0.8  156M 
31618  0.8  156M 
31618  1.0  156M 
31618  0.6  156M 
31618  0.6  156M 
31618  0.5  156M 
31618  1.0  156M 
31618  0.8  156M 
31618  0.8  156M 
31618  0.9  156M 
31618  0.7  156M 
31618  0.8  156M 
31618  0.6  156M 
31618  0.8  156M+
31618  0.9  156M 
18:11  up 3 days,  1:26, 11 users, load averages: 1.75 2.26 2.83
```

### I. Automated checks

`uptime` lines and `cargo test --workspace` results (the first `cargo fmt --check` of this run flagged the new `--act` code in `frame_build_sampler`; it was formatted, and `fmt --check` and `clippy -D warnings` pass on the committed tree).

```
18:16  up 3 days,  1:32, 11 users, load averages: 1.46 1.80 2.39
test result: ok. 115 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 1.53s
test result: ok. 2 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 1.93s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.44s
test result: ok. 614 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 19.61s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.76s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.46s
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.84s
test result: ok. 31 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.18s
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.24s
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.69s
test result: ok. 123 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out; finished in 9.87s
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s
test result: ok. 154 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.51s
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.79s
test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.46s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.44s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
18:18  up 3 days,  1:33, 11 users, load averages: 2.90 2.44 2.59
```
