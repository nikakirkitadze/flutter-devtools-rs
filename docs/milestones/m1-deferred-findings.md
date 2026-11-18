# M1 deferred and parked findings

These are review findings that were deliberately not fixed in M1. They were triaged as "can wait" by the final whole-branch review, or parked after the single fix round. Use them as input when planning M2.

## Parked after the final fix round

- HUD rows overflow the 232 px panel after relabelling ("Frame (anim)") — Ruling: real and user-visible; deferred because T16 is measuring on this exact code and there is no second wave; fix = HUD_WIDTH ≈ 280 or shorter labels; surface to user as recommended pre-merge one-liner.
- flush_settings writes on every quit (overwrites a hand-edited invalid settings.json with defaults; startup-trace runs rewrite the file; main-thread write at quit) — Ruling: real, regression from M3; fix = skip when generation unchanged since last write; surface to user as recommended pre-merge one-liner.
- logger uses eprintln! (panics on broken stderr pipe) — Ruling: real, trivial (`let _ = writeln!(stderr)`); deferred.
- I6 jank test doesn't exercise the no-toggle dispose path, so it can't go RED on 3.41.9 against the old code — Ruling: real; fix is controller-eager already, test gap only; deferred.
- I5 has no test though bounds-based line-height test is possible — Ruling: deferred, visual review covers it.
- `log` crate's std feature relied on transitively — Ruling: declare features=["std"] later; builds today; deferred.
- ⌘W (and Minimize/Zoom/FullScreen) do nothing while a dialog has focus (dialogs are Root siblings) — Ruling: known pattern; M2 should add a dialog-wrapper re-dispatch; deferred.

## Deferred minor findings by task

- **Task 1:** CI release budget step does a full release build while no budget tests exist (until T7) — plan-mandated, cost only.
- **Task 1:** cargo "unused dependency" manifest warnings until T2/T3/T7 use gpui_kit/fdt_ui/criterion — re-check at final review that none remain.
- **Task 2:** token_usage walker can pass vacuously if read_dir fails / no files found — add assert!(!files.is_empty()).
- **Task 2:** token check stops at first `#[cfg(test)]` line, so a mid-file cfg(test) item hides the rest of the file (brief-mandated) — document or narrow to `#[cfg(test)]` followed by `mod`.
- **Task 2:** detector test has one example per regex; rgba/hsla/text_xl/rounded_md/from_secs/px(0.5) uncovered.
- **Task 3:** theme.rs radius f32→usize cast truncates fractional radius tokens; mono_font.size==12 never asserted; FdtAssets list-union/dedup untested; fonts::load untested.
- **Task 3:** FdtAssets miss path does O(catalog) Assets.list scan per lookup (gpui caches results; cache the path set if it shows in a profile).
- **Task 4:** overlapping sync_reduced_motion calls could apply out of order; no log::debug on gsettings failure (KDE not read); off-main-thread read not test-verified.
- **Task 5:** no test for hide-then-release / repeated set_visible(true); stats not reset on re-show (stale numbers mixed); graph scale divides by refresh without zero clamp; per-render Vec copies; "Draw  "/"Frame " alignment relies on trailing spaces (use fixed-width label cell); redundant test imports.
- **Task 6:** capacity 0/1 semantics untested/undocumented; selection-only change creating an entry unpinned by test; NavHistory has no Debug derive.
- **Task 7:** per-item Vec allocations (~7/item); tests miss camelCase boundary, far-gap path, multi-term sums, multi-char lowercase; tie-break uses byte length; eprintln noise in budget test; seen/bonus allocated when recent empty.
- **Task 7:** many_short_terms_are_capped may not prove the cap (use haystacks that all match "a").
- **Task 8:** crate glob `use crate::actions::*` in keymap::tool_binding; DIGITS zip silently truncates if a Tool is added (compile-time length assert); tests don't assert key strings/contexts/mac-only exclusions; tool.* registry entries duplicate show_tool mapping.
- **Task 9:** disabled Export icon not dimmed; Tool::ALL[1..] slice assumes Timeline first; row::TABLE / alpha::ACTIVE_ROW / border colour reused for sidebar row height + hover (consider sidebar tokens); weak tests (empty_state ids, count colour, Export disabled, active mark); sidebar rows not keyboard-focusable (⌘1–7 cover it); empty states have no action button (plan-mandated for M1).
- **Task 10:** failed rename leaves settings.json.tmp, no sync_all before rename; selected-state of theme control untested; one invalid field discards whole file (undocumented); Keystroke::parse("?") error → silent empty div.
- **Task 10:** rapid_updates test can't exercise real thread interleaving (deterministic executor).
- **Task 11:** stale-index window — on_confirm maps ix.row against live model.ranked, so query-change + Enter inside one frame could run the wrong command (fix: capture ranked snapshot in the on_confirm closure built per render); typing_and_enter test checks model not rendered rows; no direct assertion that confirm records the recent id; MainWindow.palette keeps last model after close; set_query rescans specs per recent id; escape test lacks run_until_parked.
- **Task 2:** token-check regex `\.(..|min_h|..)_\d` flags zero-valued helpers like `.min_h_0()` / `.p_0()` — should exclude `_0()`; same family as the `.hsla(` false positive (R9).
- **Task 12:** overflow_hidden on page row clips tall pages (pages must own scroll); row::TABLE reused for title-bar control height and border colour as hover fill (title-bar tokens); menu test doesn't pin order/separators/mac-only items; disabled buttons still show shortcut tooltip; no tests for disabled styling or window actions; Hide/HideOthers/ShowAll menu items mac-only (registry has them mac-only).
- **Task 13:** RowId trait indirection; unused `shortcuts-dialog` id; group_name/GROUPS could live on Group; About handler inline vs method; unbound test could assert a Kbd renders.
- **Task 13:** shortcut_hint uses fully-qualified paths + local use (cosmetic).
- **Task 14:** take WindowHandle from open_window instead of cx.windows().first(); {:.1} binary rounding of x.x5; no unit test for after_next_paint (Window::simulate_next_frame); println! on closed pipe.
- **Task 15:** lazy jank AnimationController created in dispose; unawaited response.close(); WebSocket double-open race / unhandled connect failure / ping after close / sequential close in server; doc comment on AppEvents not LeakyScreen; placeholder pubspec description; hero image fetched once; home route under initialRoute.
- **Task 14b:** seeded-vs-plain theme test shares one TestAppContext (order-dependent diff not caught); defer_menus untested and waits for 3 frames (delayed if window occluded); watchdog not cancelled after success; window-open/update failure paths only log::error (no stderr); floor harness ≠ literal hello_world and floor median > app median (noise) — rerun on quiet machine in T16.
- **Task 14b:** report line 214 A-variant medians should read 392.3/410.7 (not cited in body); after-phase table mixes per-column medians.
- **Task 16:** unmeasured "10–20 % worse" estimate; fonts_loaded per-run wording; resize rows overlap (collapse to one Draw p99 row, R2/R3 budgets); idle run descriptions (run 3 looks cherry-picked); author checklist lacks per-OS steps / Ctrl variants / where to record; 3.41.9 re-resolve command not given.
