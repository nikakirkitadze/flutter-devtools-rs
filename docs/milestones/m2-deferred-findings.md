# M2 deferred and parked findings

These are review findings that were deliberately not fixed in M2. They were triaged as "can wait" by the final whole-branch review (Opus), or parked after its fix wave and the residual fix round. Items struck through were fixed before the merge (master `1b9cd1f`). Use them as input when planning M3.

## Should fix soon (early M3)

The final review's triage put these first:

- **Tail log chunk is copied on every publish and bounded by record count, not bytes** (T2 parked). A stream of large records means megabytes copied per publish.
- **The whole shell re-renders on every snapshot** (T13), about 60 Hz while capturing. Chrome is cached since T21b, but measure it under streaming: bench-ui uses a fixed snapshot and cannot see this.
- **Logs job cost per snapshot.** `level_counts` runs over every record (`logs/view.rs`) and the filtered list is copied twice per publish (T16); about 5–10 ms of background CPU per publish at 1M records while logs stream. Make counts incremental. There is no incremental-versus-full equivalence test.
- **Discovery tests use real 350–500 ms margins** (T8) and will flake once CI runs.
- **bench-ui has no streaming-snapshot scenario.** The fixed-snapshot harness cannot catch bugs that depend on live publishing (the root of the final review's Critical #1). Add one to bench-ui and the tests.

## Parked after the final fix wave and the residual fix round

- ~~Logs search during live capture panics (rows from a newer snapshot than `self.snap`)~~ — **Fixed:** rows and snapshot travel together; `get_row` on UI paths.
- ~~⌘N with a range set keeps Logs expansion and the details line cache → `line_at` panic~~ — **Fixed:** views key caches by session; first-phase results reset too (`a_new_sessions_first_phase_rows_reset_expansion_and_details`).
- ~~Bodies not fetched for requests that complete while disconnected (same-VM reconnect)~~ — **Fixed:** a locally aborted request counts as newly completed once.
- ~~Logs and Network job `Key` used `generation`, which restarts per session~~ — **Fixed:** `Key` carries `session`.
- ~~Network "Load full body" waits survived ⌘N~~ — **Fixed.**
- ~~"Searching…" shown for any running job / false "No lines match" during a search~~ — **Fixed.**
- **Logs match count disappears during every incremental job** (`logs/view.rs`, `match_count()` requires `settled()`); "N matches" can blink while logs stream. Gate on `!search_pending()`.
- **While recording is paused, a request aborted by a disconnect is dropped after the reconnect** (`isolate_exit` removes the open set, so `poll` skips it). Remember aborted `(isolate, vm_id)` pairs as `known_open`.
- **"Load full body" limit is checked per side.** `getHttpProfileRequest` returns both bodies in one frame; two large sides can exceed the 256 MiB frame and the reader closes the connection. A full load also decodes the unwanted side up to 64 MiB.
- **Item-1 test gap:** `apply_first_for_test` passes `self.snap`, so no test asserts the rows/snapshot coupling. Add a `debug_assert!` in `render_row`.
- **RPC envelope:** `Box<RawValue>` copies each result once (peak ≈ 2× text for big replies); every `call` parses twice; a malformed `error.code`/`message`/id drops the frame and the caller waits for its timeout.
- **`m2-review.md` open-decision numbering** reads 1, 2, 3, 4, 7, 8, 5, 6.

## Final review minors (not fixed)

- **Linked-socket lookup on the UI thread:** `socket_for` (`fdt_store/src/net.rs`) is a linear scan per WebSockets row, cache cleared every snapshot. Store the linked socket on the request record.
- **App-controlled log times can stretch the strip:** records use the app's `LogRecord.time`; a future timestamp stretches the span. Use the event timestamp for the axis, or clamp to now plus a margin.
- **Profiling stays on after disconnect:** HTTP and socket profiling stay enabled in the app after Disconnect or window close. Disable best-effort on a user disconnect.
- **The VM connection lingers** up to 10 s after Disconnect/close (body fetches and the log worker hold client clones).
- **The capture host needs a drawn frame:** it installs from an `on_next_frame` chain, so a locked/occluded launch never discovers or connects. Add a timer fallback. A runtime start failure leaves the Start page with only its heading.
- **Wrong body reason after reconnect:** bodies whose fetch was pending at the disconnect stay "The app stopped before the body was copied." although the app is alive.

## Deferred minor findings by task

- **Task 3:** socket nearest-wins only from the request side; linking is O(websockets × sockets).
- **Task 5:** the writer sleeps 1 s after Close; `id: null` errors are dropped; I/O errors show tungstenite's text.
- **Task 6:** replay re-sends events on a repeated listen; the recordings privacy test is not recursive; one `~/Documents` path remains.
- **Task 8:** a missed `flutterVersion` is cached for good; Windows `pid_alive` treats access-denied as dead (unix treats EPERM as alive); `dedup_by` relies on sort order; the CLI fallback `list_via_cli` can block the scan loop ≤ 5 s every `retry_after` (only with no registry dir); probe/list tasks are not aborted when their entry goes away (≤ 2 s).
- **Task 9:** an aborted connection skips the final stdout flush; the replay test doesn't pin the Flutter version; `stale_connection_cannot_overwrite_status` reimplements the guard instead of driving `Session::run`.
- **Task 10:** ~~isolates already running at connect are not polled~~ (fixed); ~~request body not fetched when the response is TooLarge~~ (fixed); free-memory race window ~2 RTT (accepted by R4); a never-completing request blocks clearing.
- **Task 11:** `url_target_retries_then_gives_up` mixes real sockets and paused time (loose 2..=6 range); `Failed` after `StartRun` doesn't set `waiting_for` for a discovered target.
- **Task 12:** the `token_usage` exemption is a manifest substring check; the settings scroll test checks only the id; `render_start` has an unused `cx`; Settings drops unknown fields on write.
- **Task 13:** `install()` stats `PATH` on the UI thread after the first frame (a GUI-launched macOS app has a minimal `PATH`, so `dart` is often not found); File menu items are always enabled; ⌘R/⌘N use `SHELL_CONTEXT` and fire inside inputs (see open decision on ⌘N).
- **Task 14:** recent URLs persist the VM token in `settings.json` (now 0600; open decision); failed URLs are remembered too; a double Enter connects twice; AppList actions are exempted from the palette rule by an inline list; the Disconnected state has no start-page copy; `start-connect-error` is not cleared on URL/recent connects.
- **Task 15:** the strip span grows in real time only while recording (frozen while paused); stale drag state if the page changes mid-drag; the Esc test name over-claims (the strip has no focus); the first run has no divider; no direct `count_requests == filter_requests().len()` test; no test that an unchanged `set_counts` skips the notify.
- **Task 16:** `logs-source`/`logs-logger` ids sit on wrapper divs (buttons are `*-button`); `filter_work_runs_off_the_ui_thread` doesn't prove `background_spawn`; the 1M test bound ≤ 40 is loose.
- **Task 17:** details lines are cut at 4,096 characters and not wrapped or horizontally scrollable (a match past the pane width is not visible); details clone `rec.error` per frame; `status_changed` can move focus from the sidebar to the list; the first data after an empty store can drop a location-restored selection; ~~phase-one delivery and append-during-query untested~~ (fixed).
- **Task 18:** the Errors count costs a second pass off-thread; registry ids `logs.next`/`logs.previous` also serve Network; the `SelectDown`/`SelectUp` exemption in the registry test.
- **Task 20:** Enter before the request merge selects "Search logs" (no loading hint); no stale-merge/cancel test; an unreachable "No matching results" branch; rows act on mouse-down with no hover selection; `set_query` compares recents × items.
- **Task 21c:** the palette merge test doesn't prove the kept row differs from a reset; the first-mount strip notify costs one extra render.
- **Task 22:** the 10-minute VM-URI wait vs the brief's 60 s (cold CI builds); fixed sleeps after pop/retry; the discovery fallback `.or(named.first())` may pick another `jank_lab`; the CI apt package list is not tailored.
- **Task 23:** "30 s wait/warm-up" and "two attempts" in the idle section are procedure statements without raw timestamps.

## Declined to judge (open with the author)

- ⌘N discards the session with no confirmation or undo, and fires inside inputs (spec §8.4 assigns ⌘N to New session).
- Windows and Linux code paths (discovery, `pid_alive`, shortcuts) have not been compiled or run.
- Draw p99 for Logs search and tool switch is about 6.5–7 ms against 4 ms (meeting it needs visible text changes).
- bench-ui and the connected window's idle RSS/CPU were not measured (screen locked); commands in `m2-review.md`.
- Decompression-bomb PNGs under 8 MiB rely on gpui/image decoder limits, not verified.
- Two windows connected to the same app double the overhead (spec §8.1 allows multiple windows).
- Visual design in light and dark (author's checklist).
