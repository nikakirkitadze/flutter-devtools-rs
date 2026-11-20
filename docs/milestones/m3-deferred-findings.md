# M3 deferred and parked findings

These are review findings the per-task reviews (Sonnet) triaged as "can wait" and the controller parked in the M3 ledger. Tasks 1 to 25 are covered; the minors listed per task are the ones left after each task's fix rounds. Items marked "carried" were handed to a later task and are listed here only if that task did not close them. M2's list is in `m2-deferred-findings.md`.

## Flaky tests (seen under load, not reproduced alone)

- `bench_ui::live_perf_checks_that_the_session_grew_and_the_chart_followed` failed in 2 workspace runs under load and passes alone. Task 24 notes its growth checks (`at_end`, wall-clock growth) "may flake on live runs".
- `lifecycle::new_session_starts_empty_and_keeping_the_connection` hit a settle-helper timeout once (Task 11), then passed 6 re-runs.
- `fdt_capture` `replay_debug` failed intermittently under load, also on the baseline (Task 7); not reproduced in 80 runs (Task 9).
- One unexplained workspace run with 3 failed tests (Task 22), not reproduced in 4 runs.
- The `jank_lab` window can stop drawing after a tracing toggle; the live test nudges it with `list` (Task 25).

## By task

- **Task 1:** no mixed chunk-size test for `level_counts_from` and `filter_logs` from a sequence number; `synth::writer` shadows `logs::testkit::writer`; the live-logs feed thread never stops; the DTD retry upper bound check (2..=3) was dropped.
- **Task 2:** `set_budget` rebuilds the slow index even when the budget is unchanged; `by_number` binary search plus the writer's `HashMap` look up twice; `pending_order.retain` is O(4096); missing tests (multi-run budget, unknown run, overwriting a parked phase); `RunInfo.refresh_hz` keeps raw junk values (`budget_us` guards them).
- **Task 3:** the cell-end overflow guard does not cover starts near `u64::MAX` (hostile timestamps only; panics in debug); `lod_coarse` counts level insertions, not slices; fine levels under-report slices longer than 17 s (documented).
- **Task 4:** `push_cpu_samples` ignores the run and uses one global last sample (fine within one writer); `CpuSnapshot::function` and `node` panic on foreign ids; `stress_session_has_perf_data` takes about 9 s in debug.
- **Task 5:** the request raise uses the current start, not the insert's first start (cell mismatch if the start changes); log `seq` as `u32` truncates; `NAV_CHUNK` duplicates `LOG_CHUNK`; the raise re-runs on every upsert of a failed request.
- **Task 6:** `decode_cpu_samples` fails the whole response on one unknown id (carried to Task 10); per-packet `Vec` allocation in the Perfetto field reader; missing tests for packed frame ids and incremental interning.
- **Task 7:** the user-name mask may overrun when `/Users/<name>` has no trailing slash; metadata events carry `ts_us = 0`; the recorder ignores call errors; Dart tests were not red-first; blobs under 1,024 characters are not scanned for home paths.
- **Task 8:** the fast path treats array events as metadata (unreachable from the VM); one odd element sends the whole response to the slow path; args keys allocate; equal timestamps keep VM order; async rows are keyed by name only.
- **Task 9:** no test for the retry origin after a failing `getVMTimeline`; `get_vm_timeline_micros` has no explicit timeout in the poll loop; the NaN and infinite fps guard is untested.
- **Task 10:** repeated timeouts grow the fetch window without a cap; a timeout does not reset the error streak; samples between the last fetch and a restart are lost; "Off" is never re-enabled, even by "Fetch now".
- **Task 11:** if the prior-state read fails on every "on", tracing is not restored; restore blocks the session loop up to 500 ms; stream coalescing (up to 16,384 events per feed) was not load-benchmarked.
- **Task 12:** the rebuilds root-by-node child scan is quadratic with many roots; `name()` allocates an `Arc<str>` per widget per frame; no raster-over-but-smaller-than-UI breakdown case.
- **Task 13:** flame truncation drops still-open outer spans; an unchecked `t - o.last`; the bottom-up cancel check can be skipped; `busiest` cannot be cancelled.
- **Task 14:** `gc_ns` reads only the depth-0 lane; rule 8's hint is shown even with rebuilds present (verbatim brief); `phase_window` assumes phases run back to back; `explain_budget` is empty in debug builds.
- **Task 15:** every handled scroll stops propagation, including PanY; the over-cap case logs a warning instead of a `debug_assert`; real trackpad direction is untested on hardware.
- **Task 16:** the strip-key test is weak; the strip cache is not budget-keyed (the generation bump covers it); the trace toggle does not write the Settings default (Task 20 added the Settings switch).
- **Task 17:** Enter adds no Back step after J/K when there is no details pane; only "Latest" resumes follow; no disconnected-data test; visuals unverified on a display.
- **Task 18:** the delayed skeleton is untested; no Esc-to-close binding (the shell's clear selection covers it); a per-snapshot clone and compare cost; about 4 background re-analyses per second while streaming (bounded); a session change mid-flight is untested.
- **Task 19:** `row_offset` is not re-clamped when the canvas grows; hover is not cleared on a row scroll; the flame outline is dropped at the quad cap; the tooltip does not follow the pointer; the CPU tab is unreachable when a session has CPU samples but no frames; the "Off" reason shows only when there are no samples.
- **Task 20:** the cap test pushes 8M slices (about 6 s in debug; a store test hook would be instant); an On/Off/On race within one RPC from a non-toolbar caller can leave it on; "Max in a frame" may truncate at the column count.
- **Task 21:** `tracks_match` ignores the track kind; the label cap is spent in row order; `slice_at` scans; no pointer capture during a ruler drag.
- **Task 22:** findings are sampled when slow frames outnumber the lane width (a janky frame can be missed in a dense view); `worst_frame` has no hard cap per cell; `lane_hit` is recomputed per pointer move; a click on a GC slice opens Performance with no selection.
- **Task 23:** the landing guard is not isolated by tests; related links are not keyboard-focusable; the Related block shifts the layout when it loads; the first 5 of 20 related items are in time order, not nearest.
- **Task 24 and 24b:** the cpu-tree check is gated on `tree_len > 50`; the theme test comment is false and does not isolate the key; the table scrollbar-animation rerender is not counted; the details pane result is keyed by `Arc` pointer.
- **Task 25:** zero-duration edge cases; descendants under a non-Dart slice are dropped; a nine-argument helper; rule 7 is not asserted in the boundary test; the fixture's `raster_on` draws 160 tiles.

## Carried from M2 and still open

See `m2-deferred-findings.md`. Items closed in M3 (Task 1 and the plan's "Inputs carried in"): the streaming bench scenario (`live-logs`), the tail log chunk copy, the incremental level counts, the capture host's dependence on a drawn frame, and profiling left on after disconnect. The rest (Windows and Linux paths, the token in `settings.json`, decompression-bomb PNGs) is unchanged.
