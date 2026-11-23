# M1 verification results and review checklist

Branch `m1-foundation`, measured on commit `eef6500` (R1: Apple M3 Max, macOS, release build).

Legend: met, missed, not measured (needs the author on a machine with accessibility access to the UI).

## Automated checks (all green on eef6500)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass, no warnings |
| `cargo test --workspace --locked` | 78 passed + 1 ignored (fdt_app), 2 (cli), 28 (fdt_ui), 3 (token_usage); 0 failed |
| `cargo test --workspace --release --locked -- budget` | 1 passed (palette ranking, 5,000 items per keystroke) |
| `cargo deny check licenses` | licenses ok |
| `flutter analyze` (fixtures/jank_lab) | No issues found |
| `flutter test` (fixtures/jank_lab) | 14 passed |

## Measured budgets

Machine state: the Mac was never quiet. 1-minute load was 8.07 at the start; I polled every 60 s for 10 minutes (8.71, 7.41, 5.67, 7.55, 5.12, 12.52, 7.11, 6.40, 7.09, 5.65, 7.71) and it never fell to 3, so the measurements below ran at load about 7 (1-minute load 7.11 before startup, 6.87 after; 6.09 around idle). Treat absolute numbers as 10 to 20 percent worse than a quiet machine. The display was kept awake with `caffeinate -d`.

### Startup (7 runs of `fdt --startup-trace`)

`main()`-based numbers come from the trace; exec-based numbers add `pre_main_ms = main_entry_unix_ms - spawn_ms` (spawn wall-clock taken just before launching the process).

| Run | first_paint | interactive | pre_main | exec to first paint | exec to interactive |
|---|---|---|---|---|---|
| 1 | 270.8 | 311.6 | 15.8 | 286.6 | 327.4 |
| 2 | 232.4 | 288.6 | 29.8 | 262.2 | 318.4 |
| 3 | 214.5 | 268.6 | 27.6 | 242.1 | 296.2 |
| 4 | 214.3 | 261.2 | 28.8 | 243.1 | 290.0 |
| 5 | 226.8 | 276.8 | 28.3 | 255.1 | 305.1 |
| 6 | 246.3 | 292.5 | 25.4 | 271.7 | 317.9 |
| 7 | 248.8 | 304.6 | 30.2 | 279.0 | 334.8 |
| **median** | **232.4** | **288.6** | **28.3** | **262.2** | **317.9** |

Phase medians (ms since `main()`): settings_loaded 0.3, app_started 72.4, gpui_kit_init 73.6, theme_installed 74.6, fonts_loaded 76.9, theme_applied 77.0, motion_synced 77.1, window_opened 163.2, first_paint 232.4.

| Budget (R1) | main()-based | exec-based (honest cold start) | Verdict |
|---|---|---|---|
| Cold start to first paint <= 200 ms | 232.4 | 262.2 | **missed** |
| Cold start to interactive <= 400 ms | 288.6 | 317.9 | met |

### Startup findings (Task 14b)

- The installed-font enumeration probe was the largest avoidable cost; skipping it at startup cut first paint by about 95 ms (30 percent) under the same load. `fonts_loaded` is now about 0.3 ms after the theme.
- The menu bar is installed after first paint (between the `first_paint` and `interactive` callbacks). That is why interactive sits further from first paint than before; it is still well inside 400 ms.
- What remains is inside GPUI and macOS: app launch, NSWindow and Metal layer setup, and the first draw (`window_opened` to `first_paint` is about 70 ms, `app_started` about 72 ms). A bare GPUI window with no `gpui_kit::init` measured a first paint floor of 243.9 ms on this machine under load in the same loop, so our code is at that floor.
- The best single run during 14b was 201.9 ms. On a quiet machine the median may reach the budget; this run could not show that.

**First-paint budget: needs the author's sign-off to merge with this miss.** The budget is not retuned.

### Idle memory and CPU (30 s warm-up, then 5 samples 2 s apart, `ps -o rss=,%cpu=`)

| Run | RSS samples (KB) | %CPU samples |
|---|---|---|
| 1 | 108288, 111904, 112096, 112176, 112016 | 1.3, 6.8, 27.5, 11.8, 2.2 |
| 2 (with `top` sampling alongside) | 101776, 92880, 92576, 92880, 92992 | 0.0, 0.0, 0.0, 6.5, 7.6 |
| 3 (brief command only) | 105296 x5 | 0.0 x5 |

- RSS maximum 112176 KB against a 153600 KB budget: **met** (about 110 MB).
- CPU: run 3 was fully idle (0.0 on every sample), but runs 1 and 2 showed intermittent wake-ups well above 1.0. The machine was under heavy load with WindowServer busy, and run 2 was perturbed by `top`, so the cause is not established; I did not see it on the clean run. The brief's criterion is "every sample <= 1.0", so this is **missed in 2 of 3 runs and not explained**. The author should re-measure on a quiet machine, and if spikes reproduce, look for a timer or animation that keeps requesting frames while idle (caret blink, HUD, settings watcher).

### Frame times, tool switching, palette open

**Not measured.** Driving the window (resize for 10 s, keystrokes, reading the HUD) needs accessibility access; `osascript` reported "not allowed assistive access (-1719)" in this environment, and the Argent tools only target simulators and CDP apps. The existing automated budget test covers palette ranking only (1 test, passes). The author records these by hand; fill in the table.

| Check | Budget | Measured |
|---|---|---|
| Resize 10 s, HUD shown: Draw p99 | R1 draw <= 4 ms (UI-thread work), no frame > 16.7 ms | |
| Same: frame p99 (HUD "Frame (anim)" and "Dropped (anim)" rows fill only during animations, so use Draw here) | R1 <= 8.3 ms, R2 and R3 <= 16.7 ms | |
| Switch tools with cmd-1 to cmd-7 | each switch draws within 50 ms | |
| Palette open | next frame, no visible delay | |
| Palette frame cost: HUD shown, palette open, caret blinking, Draw p99 | record it | |

The palette re-measures all rows on each render. That is acceptable at about 26 commands in M1 and must be redesigned before M2 adds thousands of entries (see "Carry into M2").

## Author review checklist

Turn the HUD on with the menu item "Show Frame Times" or from the palette. Save screenshots next to this file as `m1-<os>-<theme>-<view>.png`.

- [ ] Visual review in light and dark, against design system sections 10.4 to 10.6: sidebar, each tool page, palette, cheat sheet, Settings, About, HUD. Check macOS, Windows and Linux.
- [ ] Frame times and tool switching (table above) on R1; frame <= 16.7 ms on R2 and R3.
- [ ] Palette opens on the next frame.
- [ ] Reduced motion: with the OS setting on, the palette and dialogs appear without animation.
- [ ] Native feel, macOS: traffic lights, menus, full screen, cmd shortcuts, Close Window (cmd-w).
- [ ] Native feel, Windows: caption buttons, Snap Layouts on hovering maximize, Ctrl shortcuts (including ctrl-w).
- [ ] Native feel, Linux: client-side frame and resize on GNOME and KDE, under both Wayland and X11.
- [ ] Screen readers (spec section 10.7): record what VoiceOver, Narrator and Orca announce for sidebar items, title bar buttons and palette rows. Gaps are a known limitation, not a blocker.

## Pending verification

- **First CI run on macOS, Windows and Linux.** The Windows and Linux Rust code has never been compiled or run: `fdt_ui::os` reduced-motion probe, Linux client-side decorations, the Windows title-bar hit-test with `.occlude()`, and the `ctrl-w` Close Window binding.
- **`jank_lab` on Flutter 3.41.9.** The lockfile was resolved on Dart 3.13 (transitive `web 1.1.2`); a plain `flutter pub get` should re-resolve it. The new Jank-screen dispose test only goes red on Flutter 3.41 to 3.44.
- **Measurements in this file** were taken on a loaded machine; re-measure startup and idle CPU on a quiet R1.

## Carry into M2

- A palette item model that carries parameters, plus a virtualised list (R12); the current palette re-measures every row per render.
- Key-context rules so shell shortcuts (`?`, cmd-[ and cmd-]) do not steal keys from text fields inside the shell: use `Shell && !Input`.
- Pages own their scrolling.
- Settings deserialisation that tolerates one bad field instead of rejecting the file.
- Run ids in `Location` and the time axis.
