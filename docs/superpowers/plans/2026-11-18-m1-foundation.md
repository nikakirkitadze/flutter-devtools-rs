# M1 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a launchable `fdt` desktop shell for macOS, Windows and Linux. It has a themed window with per-OS chrome, a sidebar of tools, a command palette, back/forward navigation and a frame-time HUD, all built on a tested design system. Alongside it come CI on all three OSes and the `jank_lab` Flutter fixture app.

**Architecture:** A Cargo workspace with two crates:
- **`fdt_ui`** is the design system: tokens, fonts, theme, reduced motion, the empty state and the frame-time HUD.
- **`fdt_app`** is the GPUI application and builds the `fdt` binary: navigation model, actions, keymap, palette, settings, window shell and menus.

Pure logic lives in modules that use no GPUI and are tested with plain `#[test]`: tokens and contrast, frame statistics, navigation history, fuzzy ranking and settings serialization. Views are tested headlessly with `#[gpui_kit::test]`. The Flutter fixture app lives in `fixtures/jank_lab`.

**Tech stack:**
- Rust 1.100.0, edition 2024
- `gpui-kit` 0.10.0, which bundles `gpui-pre` 0.3.12, `gpui-component` 0.10.0 and Lucide icons
- serde and serde_json, `dirs` 6, `criterion` 0.8, `cargo-deny`
- Flutter 3.41–3.50 for `jank_lab`
- GitHub Actions

**Spec:** `docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md`. M1 is the first row of §13. This plan implements §8.2, §8.4, §10, §11 and §12.1 as they apply to M1.

## Scope

**M1 delivers** the M1 row of §13:
- the workspace
- the GPUI shell: window, sidebar and per-OS title bar
- design tokens and both themes
- the command palette
- navigation history plumbing
- the frame-time HUD
- CI on all three OSes
- `jank_lab`

**Deferred on purpose.** Each item below is reachable from a later milestone and not needed to prove M1:
- **`fdt bench-ui`, the stress datasets and the >10 % regression gate (§10.3).** There is nothing to replay until the M2 and M3 views exist. M1 enforces its budgets with a release-mode budget test (Task 7) and the measurements in Task 16.
- **Loading, error and disconnected states (§10.5).** Nothing in M1 loads or disconnects. The empty state component ships now; the others come in M2.
- **The data shortcuts ⌘F, ⌘⇧F, J/K, ←/→, Space, Enter, ⌘E, ⌘⇧S, ⌘R and ⌘N (§8.4).** They act on data that arrives later.
- **Every setting except Theme (§8.7).**
- **Row context menus (§10.6).**

## Global constraints

**Toolchain and dependencies**
- **Toolchain.** Rust `1.100.0`, pinned in `rust-toolchain.toml`. Edition 2024 everywhere. Every crate sets `license = "Apache-2.0"` and `publish = false`.
- **UI dependency.** UI crates depend on `gpui-kit = "=0.10.0"` only, never on `gpui` or `gpui-component` directly. Import GPUI items as `gpui_kit::…` and components as `gpui_kit::component::…`.
- **Never copy from Zed's GPL-3.0 crates** (`ui`, `picker`, `title_bar`, `workspace` and the others). `gpui` itself is Apache-2.0 and fine to use.

**The UI thread**
- No I/O, parsing or work proportional to session size runs on the UI thread. Writing files goes through `cx.background_spawn`.
- One exception: reading the settings file once before the event loop starts (Task 10).

**Design tokens**
- Views never hard-code colours, sizes, radii or durations. Everything comes from `fdt_ui::tokens`.
- The test `crates/fdt_ui/tests/token_usage.rs` enforces this.
- The only escape hatch is a same-line comment `// token-check: allow: <reason>`.
- **Unit tests go at the bottom of each file**, in `#[cfg(test)] mod tests`. The token check stops scanning a file at its `#[cfg(test)]` line.

**Exact values from the spec**
- **Typography:** Inter for UI text and JetBrains Mono for code, paths, logs and numbers. Sizes 11, 12, 13, 14, 16 and 20; weights 400, 500 and 600; line height 1.4 for UI text and 1.5 for logs; tabular figures in numbers.
- **Spacing and shape:** spacing 2, 4, 6, 8, 12, 16, 20, 24, 32. Radii 4 (chips), 6 (controls and rows), 8 (panels and popovers) and 12 (dialogs). Rows are 26 px for tables and trees, 22 px for logs.
- **Separation:** borders, not shadows. Only popovers and the palette get one soft shadow.
- **Motion:** 100 ms for hover and press, 160 ms for panels and selection, 200 ms for overlays, all ease-out. With reduced motion every duration is 0, and data redraws never animate.
- **Focus:** keyboard focus shows a 2 px accent ring; mouse focus shows none.

**Budgets that apply in M1 (§10.2)**

| Measure | Budget |
|---|---|
| Cold start to first window paint | ≤ 200 ms on R1 |
| Cold start to interactive | ≤ 400 ms on R1 |
| Command palette open | ≤ 1 frame |
| Palette results per keystroke | ≤ 8 ms over 5,000 items |
| Switch tools | ≤ 50 ms |
| Idle memory | ≤ 150 MB RSS |
| Idle CPU | ≤ 1 % |
| Resize | p99 frame time ≤ 8.3 ms on R1 |

**Shortcuts and tests**
- **Shortcuts use ⌘ on macOS and Ctrl elsewhere.** Write them as `keymap::platform("cmd-k", "ctrl-k")`. `KeyBinding::new` panics on `secondary-…`. Tests press keys through the same helper, so they pass on every CI OS.
- **Glob imports.** Never `use gpui_kit::*` in production modules, and never glob-import it into a test module. It exports GPUI's `test` macro, which shadows `#[test]` and fails with "recursion limit reached while expanding `#[test]`". Import names explicitly.
- **Elements that tests find or click** get `.id("…").test_support()` (`use gpui_kit::TestSupportExt as _;`). This compiles to nothing outside test builds.

**Privacy and copy**
- **No network access and no telemetry.** M1 makes no network calls at all.
- **User-facing copy** is sentence case, plain, and never mentions milestones. The product name is the placeholder `fdt`.

**Commits**
- Use conventional prefixes (`feat:`, `fix:`, `test:`, `build:`, `docs:`, `ci:`).
- End each message with the executing session's `Co-Authored-By` line.

## Review focus

These are the inputs and failure modes most likely to bite a real user that no feature test exercises by default. Each has a test in the task that owns the code.

1. **A corrupt, partly written or unwritable settings file.** The app starts with default settings, never crashes, and keeps running when a save fails. *(Task 10)*
2. **Typing a character that is also a shortcut (`?`) while the palette's text field has focus.** The character goes into the query and no command runs. *(Task 11)*
3. **Pressing ⌘K while the palette is open.** It closes the palette; a second palette never stacks on top. *(Task 11)*
4. **Pasting non-ASCII or very long text into the palette.** Nothing panics or stalls, and ranking returns quickly with no matches. *(Tasks 7 and 11)*
5. **The OS switching between light and dark.** The app follows only when Theme is "Match system"; a pinned Dark or Light stays put. *(Task 10)*

## Facts verified while planning (read before starting)

These come from compile-and-test spikes against `gpui-kit` 0.10.0 on this machine.

1. **App entry.** `gpui_kit::application().with_assets(assets).run(|cx: &mut App| { … })`.
   - Call `gpui_kit::init(cx)` before anything else.
   - `gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| View::new(window, cx)))` wraps the view in `gpui_kit::base::Root`.
   - `Root` draws dialogs, sheets and notifications as *sibling* layers of your view.
2. **Dialog actions never reach the shell.** An action dispatched while focus is inside a dialog does not reach the shell's `on_action` handlers. A palette or sheet that runs a command must do this:
   - close the dialog (`window.close_dialog(cx)`)
   - focus the shell's `FocusHandle`
   - then call `window.dispatch_action(action, cx)`
3. **Palette text field focus.** Focus it inside the `window.open_dialog(cx, |dialog, window, cx| …)` builder on the builder's first call only (use a `Cell<bool>` guard). The builder runs again on every redraw.
4. **The `Command` component** comes from `gpui_kit::component::command::{Command, CommandItem, CommandState}`.
   - `CommandState::new(window, cx)` lives in an entity. Focus it with `state.update(cx, |s, cx| s.focus(window, cx))`.
   - Build `Command::new(&state).filterable(false).items(items).on_query(|q: &str, window, cx| …).on_confirm(|ix: IndexPath, window, cx| …)`.
   - `ix.row` indexes the supplied `items`.
   - Give items `.label(..)` and `.child(..)` for custom rows. Do **not** give them `.action(..)`: Command would then dispatch the action from inside the dialog itself.
   - Show shortcut hints with `gpui_kit::component::kbd::Kbd::binding_for_action(action, None, window)`.
5. **Test harness.**
   - Use `#[gpui_kit::test] fn name(cx: &mut gpui_kit::TestAppContext)`.
   - For a window with dialogs: `cx.add_window_view(|window, cx| { let shell = cx.new(|cx| MainWindow::new(window, cx)); Root::new(shell, window, cx) })`. It returns `(Entity<Root>, &mut VisualTestContext)`.
   - The context offers `simulate_keystrokes("cmd-k")`, `simulate_input("text")`, `run_until_parked()`, `dispatch_action(A)` and `update(|window, cx| …)`, plus `read_with` on entities.
   - Take a `FocusHandle` out with `read_with` *before* calling `update`, to avoid borrow errors.
6. **Finding elements in tests.**
   - Bring `gpui_kit::test::TestWindowExt` into scope.
   - Inside `cx.update(|window, cx| …)`, call `window.render_frame(cx)`, then `window.try_find("id")`, `window.click("id", cx)` or `window.click_with_options("id", ClickOptions::new().with_button(b), cx)`.
   - `ClickOptions` lives in `gpui_kit::test`.
   - Only elements marked `.test_support()` are found.
7. **Frame profiler** (needs the `profiler` feature of `gpui-kit`).
   - Use `gpui_kit::profiler::{set_trace_enabled, FrameTimingCollector, FrameEvent, FrameTiming, PresentTiming, DrawPhases}`.
   - `FrameTimingCollector::new().collect_unseen()` returns `Vec<FrameEvent>` for all windows.
   - `FrameEvent::Draw(FrameTiming)` has `window_id`, `draw_start`, `draw_end` and `refresh_interval: Option<Duration>`.
   - `FrameEvent::Present(PresentTiming)` has `window_id` and `animation_interval: Option<Duration>`. That interval is `Some` only between frames of a continuous animation.
   - Build a `WindowId` in tests with `WindowId::from(1u64)`.
8. **Theme.** `gpui_kit::component::{Theme, ThemeMode, ThemeConfig, ActiveTheme}`.
   - `ThemeConfig` is `Deserialize` and accepts the JSON keys listed in Task 3.
   - Install a config with `Theme::global_mut(cx).dark_theme = Rc::new(config)` (or `light_theme`), then `Theme::change(ThemeMode::Dark, None, cx)`.
   - Motion tokens live in `Theme::global_mut(cx).motion`: `duration_fast`, `duration_normal`, `duration_slow`, `easing_enter`, `easing_exit` and `easing_move`. Build curves with `gpui_kit::base::Easing::cubic_bezier(..)`.
   - The colour fields we assert on: `background`, `foreground`, `border`, `muted_foreground`, `primary`, `ring`, `sidebar`, `popover`, `title_bar` and `danger`.
9. **Reduced motion.** `cx.set_reduce_motion(bool)` and `cx.reduce_motion()`. GPUI and gpui-component animations honour it, but nothing reads the OS setting for us.
10. **Icons.**
    - The full Lucide catalogue is `gpui_kit::assets::IconName`. A kebab-case file name becomes PascalCase (`chart-gantt.svg` → `ChartGantt`).
    - `gpui_kit::assets::icon_assets!(Name, [Gauge, …])` defines an asset source holding only the listed icons.
    - `gpui_kit::assets::Assets` is the default component icon bundle.
11. **Keys, fonts, menus and mouse buttons.**
    - `KeyBinding::new(keys, action, Some("Context"))`. Key strings `"?"`, `"cmd-["` and `"escape"` work.
    - Load fonts with `cx.text_system().add_fonts(Vec<Cow<'static, [u8]>>)`.
    - Menus: `cx.set_menus(Vec<Menu>)`, with `Menu { name, items, disabled }`, `MenuItem::action(name, action)`, `MenuItem::separator()` and `MenuItem::os_submenu("Services", SystemMenuType::Services)`.
    - Mouse back and forward buttons are `MouseButton::Navigate(NavigationDirection::Back | Forward)`.
12. **Licences.** The `deny.toml` in Task 1 passes `cargo deny check licenses` for this dependency tree on all four release targets.

## File structure

```
Cargo.toml                       workspace manifest and shared dependency versions
rust-toolchain.toml              pins Rust 1.100.0
deny.toml                        licence allow-list
LICENSE, README.md
.github/workflows/ci.yml         Rust on 3 OSes, licences, jank_lab
crates/fdt_ui/
  assets/fonts/                  Inter (3 weights), JetBrains Mono (2 weights), OFL texts
  src/lib.rs
  src/tokens.rs                  colours, contrast, type, spacing, radii, rows, motion, layout
  src/fonts.rs                   bundled font bytes and loading
  src/assets.rs                  asset source: our icons plus the component defaults
  src/theme.rs                   ThemePreference, token-to-component theme mapping, install/apply
  src/os.rs                      the OS reduced-motion setting
  src/empty_state.rs             EmptyState component
  src/hud/stats.rs               FrameStats (pure)
  src/hud/mod.rs                 FrameHud view and profiler ingest
  tests/token_usage.rs           fails on hard-coded visual values
crates/fdt_app/
  src/main.rs                    CLI: --version, --startup-trace, or launch
  src/lib.rs
  src/app.rs                     init, window options, run
  src/actions.rs                 every action
  src/keymap.rs                  platform() and bindings()
  src/menus.rs                   macOS app menus
  src/nav.rs                     Tool, Page, Location, NavHistory (pure)
  src/settings.rs                Settings, SettingsStore, AppSettings global
  src/startup.rs                 StartupReport and the startup trace
  src/palette/{mod,rank,recent,registry,view}.rs
  src/shell/{mod,sidebar,tool_page,title_bar,settings_page,shortcuts,about}.rs
  src/test_support.rs            #[cfg(test)] helpers: open_shell, exists, press
  tests/cli.rs
  benches/palette_rank.rs
fixtures/jank_lab/               Flutter app with deliberate jank, leaks, failing requests, deep trees, logs
docs/milestones/m1-review.md     measured budgets and the author's review checklist (Task 16)
```

Task order is sequential. Task 15 (`jank_lab`) depends on nothing else and can run in parallel with any task after Task 1.

---

### Task 1: Workspace, licence files and CI

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `LICENSE`, `README.md`, `.github/workflows/ci.yml`
- Create: `crates/fdt_ui/Cargo.toml`, `crates/fdt_ui/src/lib.rs`
- Create: `crates/fdt_app/Cargo.toml`, `crates/fdt_app/src/lib.rs`, `crates/fdt_app/src/main.rs`, `crates/fdt_app/tests/cli.rs`
- Modify: `.gitignore` (append `/target`)

**Interfaces:**
- Produces: the binary `fdt` from package `fdt_app`. `fdt --version` prints `fdt <version>`. Unknown arguments print a line starting `usage: fdt` to stderr and exit with code 2.
- Produces: the workspace dependency keys `gpui_kit`, `anyhow`, `serde`, `serde_json`, `log`, `dirs`, `regex`, `tempfile` and `criterion`.

- [ ] **Step 1: Write the manifests**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/fdt_ui", "crates/fdt_app"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.100"
license = "Apache-2.0"
publish = false

[workspace.dependencies]
gpui_kit = { package = "gpui-kit", version = "=0.10.0", features = ["profiler"] }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
log = "0.4"
dirs = "6"
regex = "1"
tempfile = "3"
criterion = "0.8"

[profile.release]
debug = "line-tables-only"
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.100.0"
components = ["rustfmt", "clippy"]
```

`crates/fdt_ui/Cargo.toml`. Inherit `version`, `edition`, `rust-version`, `license` and `publish` from the workspace in both crates:
```toml
[package]
name = "fdt_ui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[dependencies]
gpui_kit.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
log.workspace = true

[dev-dependencies]
gpui_kit = { workspace = true, features = ["test-support"] }
regex.workspace = true
```

`crates/fdt_app/Cargo.toml` uses the same `[package]` block with `name = "fdt_app"`, plus:
```toml
[[bin]]
name = "fdt"
path = "src/main.rs"

[dependencies]
fdt_ui = { path = "../fdt_ui" }
gpui_kit.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
log.workspace = true
dirs.workspace = true

[dev-dependencies]
gpui_kit = { workspace = true, features = ["test-support"] }
tempfile.workspace = true
```

Give each `lib.rs` a one-line `//!` doc comment and nothing else.

- [ ] **Step 2: Write the failing CLI test** in `crates/fdt_app/tests/cli.rs`

```rust
use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_fdt")).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("fdt {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_argument_prints_usage_and_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_fdt")).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().starts_with("usage: fdt"));
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p fdt_app --test cli`

Expected: a compile failure, because `src/main.rs` does not exist yet. The first build compiles GPUI and takes a few minutes.

- [ ] **Step 4: Implement `fn main() -> std::process::ExitCode`** in `crates/fdt_app/src/main.rs`

`--version` prints `fdt {CARGO_PKG_VERSION}` and returns success. Anything else, including no arguments, prints `usage: fdt [--version]` to stderr and returns 2. Task 9 makes "no arguments" launch the app.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p fdt_app --test cli`

Expected: 2 passed.

- [ ] **Step 6: Add the licence, README and licence policy**

- `LICENSE`: `curl -sSLf https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE`.
- `README.md`: a short page with these headings:
  - "fdt": a fast, native Flutter DevTools for macOS, Windows and Linux. Status: early development.
  - "Build": `cargo run -p fdt_app`.
  - "Test": `cargo test --workspace`.
  - "Licence": Apache-2.0, with a note that bundled fonts are under the SIL Open Font License.
- `deny.toml`:

```toml
[graph]
targets = [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-pc-windows-msvc",
  "x86_64-unknown-linux-gnu",
]

[licenses]
allow = [
  "Apache-2.0",
  "Apache-2.0 WITH LLVM-exception",
  "MIT",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "ISC",
  "Zlib",
  "Unicode-3.0",
  "0BSD",
  "CC0-1.0",
]
confidence-threshold = 0.93
# MPL-2.0 only for these crates, used unmodified (spec §12.1). Adding one needs a decision, not an edit.
exceptions = [
  { allow = ["MPL-2.0"], crate = "dwrote" },
  { allow = ["MPL-2.0"], crate = "option-ext" },
  { allow = ["MPL-2.0"], crate = "cbindgen" },
]
```

- [ ] **Step 7: Write `.github/workflows/ci.yml`**

```yaml
name: CI
on:
  push:
    branches: [main, master]
  pull_request:
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
env:
  CARGO_TERM_COLOR: always
jobs:
  rust:
    name: Rust (${{ matrix.os }})
    strategy:
      fail-fast: false
      matrix:
        os: [macos-latest, windows-latest, ubuntu-24.04]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - name: Install Linux build dependencies
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
            libvulkan-dev libx11-xcb-dev libxcb1-dev libfontconfig1-dev libfreetype-dev
      - run: rustup show
      - uses: Swatinem/rust-cache@v2
      - name: Format
        if: runner.os == 'Linux'
        run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: cargo test --workspace --locked
      - name: Release-mode budget tests
        run: cargo test --workspace --release --locked -- budget
  licenses:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check licenses
```

- [ ] **Step 8: Verify everything locally**

Run each command:
- `cargo fmt --all --check`: no output.
- `cargo clippy --workspace --all-targets -- -D warnings`: no warnings.
- `cargo test --workspace`: all pass.
- `cargo deny check licenses` (if missing, first run `cargo install cargo-deny --locked`): `licenses ok`.
- `ruby -ryaml -e 'YAML.load_file(".github/workflows/ci.yml"); puts "ok"'`: `ok`.

If `cargo deny` rejects a crate, **stop and report it**. Do not edit the allow-list.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml deny.toml LICENSE README.md .gitignore .github crates
git commit -m "build: add Cargo workspace, licence policy and CI"
```

---

### Task 2: Design tokens and the token-usage check

**Files:**
- Create: `crates/fdt_ui/src/tokens.rs`, `crates/fdt_ui/tests/token_usage.rs`
- Modify: `crates/fdt_ui/src/lib.rs` (`pub mod tokens;`)

**Interfaces (produces, all in `fdt_ui::tokens`):**
```rust
pub enum Appearance { Dark, Light }                       // Clone, Copy, Debug, PartialEq, Eq
pub struct Color(pub u32);                                // 0xRRGGBB; Clone, Copy, Debug, PartialEq, Eq
impl Color {
    pub fn hsla(self) -> gpui_kit::Hsla;
    pub fn with_alpha(self, alpha: u8) -> gpui_kit::Hsla;
    pub fn hex(self) -> String;                           // "#0D0E11"
    pub fn hex_with_alpha(self, alpha: u8) -> String;     // "#0D0E1199"
    pub fn relative_luminance(self) -> f64;               // WCAG 2.x
    pub fn contrast(self, other: Color) -> f64;           // (L1 + 0.05) / (L2 + 0.05)
}
pub struct Palette { pub bg: Color, pub surface: Color, pub elevated: Color, pub border: Color,
    pub text: Color, pub text_muted: Color, pub text_faint: Color, pub accent: Color,
    pub success: Color, pub warning: Color, pub danger: Color,
    pub lane_network: Color, pub lane_state: Color, pub scrim: Color }
pub const DARK: Palette;  pub const LIGHT: Palette;
pub fn palette(appearance: Appearance) -> &'static Palette;
pub mod alpha { SELECTION = 0x55, ACTIVE_ROW = 0x33, HOVER_FILL = 0xE6, PRESS_FILL = 0xCC, SCRIM_DARK = 0x99, SCRIM_LIGHT = 0x33 }  // u8
pub mod font { UI = "Inter", MONO = "JetBrains Mono", LABEL = px(11.), DENSE = px(12.), BODY = px(13.),
    SECTION = px(14.), PAGE = px(16.), EMPTY_HEADING = px(20.), REGULAR/MEDIUM/SEMIBOLD: FontWeight (400/500/600),
    LINE_HEIGHT_UI = 1.4, LINE_HEIGHT_LOG = 1.5, fn tabular_figures() -> gpui_kit::FontFeatures }  // enables "tnum"
pub mod space { S2, S4, S6, S8, S12, S16, S20, S24, S32: Pixels }
pub mod radius { CHIP = px(4.), CONTROL = px(6.), PANEL = px(8.), DIALOG = px(12.) }
pub mod row { TABLE = px(26.), LOG = px(22.) }
pub mod motion { enum Motion { Hover, Panel, Overlay }; HOVER = 100 ms, PANEL = 160 ms, OVERLAY = 200 ms;
    EASE_OUT: [f32; 4] = [0.0, 0.0, 0.2, 1.0]; fn duration(m: Motion, reduce_motion: bool) -> Duration }
pub mod layout { SIDEBAR_WIDTH = px(208.), WINDOW_DEFAULT = size(px(1280.), px(800.)), WINDOW_MIN = size(px(960.), px(600.)),
    PALETTE_WIDTH = px(640.), PALETTE_TOP_OFFSET = px(96.), PALETTE_MAX_HEIGHT = px(420.), HUD_WIDTH = px(232.),
    HUD_GRAPH_HEIGHT = px(40.), FOCUS_RING = px(2.), BORDER = px(1.), ICON_SM = px(14.), ICON_MD = px(16.),
    EMPTY_STATE_MAX_WIDTH = px(360.), PALETTE_BUTTON_WIDTH = px(280.) }
```
`px` and `size` are `const fn` in GPUI, so these are `pub const`.

**Colour values.** These are the spec §10.4 values after contrast tuning.

| Field | DARK | LIGHT |
|---|---|---|
| bg | 0x0D0E11 | 0xFBFBFC |
| surface | 0x131519 | 0xF4F5F7 |
| elevated | 0x1A1C22 | 0xFFFFFF |
| border | 0x23262D | 0xE3E5E9 |
| text | 0xE7E8EB | 0x16181D |
| text_muted | 0x8A8F99 | 0x5C6370 |
| text_faint | 0x626772 | 0x888E9A |
| accent | 0x7B8CFF | 0x4E60EF |
| success | 0x3FB27F | 0x198051 |
| warning | 0xE2A336 | 0x99651A |
| danger | 0xE5534B | 0xD0362E |
| lane_network | 0x4EA1FF | 0x146CE2 |
| lane_state | 0xB07CFF | 0x804CF5 |
| scrim | 0x000000 | 0x16181D |

- [ ] **Step 1: Write the failing unit tests** at the bottom of `tokens.rs`

The test names and assertions:
- **`palettes_match_spec_values`**: table-driven over every row of the table above, for example `assert_eq!(DARK.text_faint, Color(0x626772))`.
- **`body_text_meets_4_5_on_every_surface`**: for both palettes, `text` and `text_muted` against `bg`, `surface` and `elevated` give `contrast >= 4.5`.
- **`accent_status_and_lanes_meet_4_5_on_every_surface`**: the same check for `accent`, `success`, `warning`, `danger`, `lane_network` and `lane_state`.
- **`text_faint_meets_3_on_every_surface`**.
- **`black_on_white_contrast_is_21`**: `(Color(0).contrast(Color(0xFFFFFF)) - 21.0).abs() < 1e-9`.
- **`hex_formatting`**: `Color(0x0D0E11).hex() == "#0D0E11"` and `Color(0x0D0E11).hex_with_alpha(0x99) == "#0D0E1199"`.
- **`scales_match_spec`**: font sizes `[11, 12, 13, 14, 16, 20]`; spacing `[2, 4, 6, 8, 12, 16, 20, 24, 32]`; radii `[4, 6, 8, 12]`; rows `26` and `22`.
- **`motion_durations_and_reduced_motion`**: Hover, Panel and Overlay give 100, 160 and 200 ms; every one gives `Duration::ZERO` when `reduce_motion` is true.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_ui --lib tokens`

Expected: a compile failure, because the items are not defined.

- [ ] **Step 3: Implement `tokens.rs`** with the values above

Use the WCAG relative luminance: channel `c/255`, then `c/12.92` if `c <= 0.04045`, else `((c + 0.055) / 1.055)^2.4`. Weight the channels 0.2126, 0.7152 and 0.0722.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fdt_ui --lib tokens`

Expected: all pass.

- [ ] **Step 5: Write the token-usage check** in `crates/fdt_ui/tests/token_usage.rs`

Write a helper, `fn violations(source: &str) -> Vec<(usize, String)>`. It returns 1-based line numbers and text. It stops at the first line whose trimmed text starts with `#[cfg(test)]`, and it skips lines containing `token-check: allow:`. It flags lines that match any of these regexes:
```
\brgba?\(\s*0x
\bhsla\(
"#[0-9A-Fa-f]{6}([0-9A-Fa-f]{2})?"
\bpx\(\s*[0-9]*\.?[0-9]*[1-9]
\.(p|px|py|pt|pr|pb|pl|m|mx|my|mt|mr|mb|ml|gap|gap_x|gap_y|w|h|size|min_w|min_h|max_w|max_h|top|right|bottom|left|inset)_\d
\.text_(xs|sm|base|lg|xl|2xl|3xl)\(
\.rounded_(xs|sm|md|lg|xl|2xl|3xl)\(
Duration::from_(millis|secs|secs_f32|secs_f64|micros|nanos)\(
```

Two tests use it:
- **`detector_flags_hard_coded_values`**: these lines are flagged: `div().bg(rgb(0x123456))`, `.p_2()`, `.text_sm()`, `.w(px(12.))`, `"#0D0E11"`, `Duration::from_millis(250)`. These are not: `.w(px(0.))`, `.w_full()`, `.rounded_full()`, `.p(space::S8)`, and `Duration::from_millis(250) // token-check: allow: poll interval`.
- **`views_use_tokens_not_literals`**: walks every `*.rs` file under `<workspace>/crates/*/src`, where the workspace is `CARGO_MANIFEST_DIR/../..`. It excludes `crates/fdt_ui/src/tokens.rs` and asserts that no file has violations. The failure message lists `path:line: text`.

- [ ] **Step 6: Run the check**

Run: `cargo test -p fdt_ui --test token_usage`

Expected: 2 passed.

- [ ] **Step 7: Commit**

```bash
git add crates/fdt_ui
git commit -m "feat(ui): add design tokens with contrast tests and a token-usage check"
```

---

### Task 3: Fonts, icons and theme installation

**Files:**
- Create: `crates/fdt_ui/assets/fonts/` with five `.ttf` files plus `Inter-OFL.txt` and `JetBrainsMono-OFL.txt`
- Create: `crates/fdt_ui/src/fonts.rs`, `crates/fdt_ui/src/assets.rs`, `crates/fdt_ui/src/theme.rs`
- Modify: `crates/fdt_ui/src/lib.rs`

**Interfaces:**
- Consumes: `fdt_ui::tokens::*` (Task 2).
- Produces:
```rust
// fdt_ui::fonts
pub struct FontFile { pub name: &'static str, pub bytes: &'static [u8] }
pub const FONT_FILES: [FontFile; 5];   // Inter-Regular, Inter-Medium, Inter-SemiBold, JetBrainsMono-Regular, JetBrainsMono-Medium (.ttf)
pub fn load(cx: &mut gpui_kit::App) -> anyhow::Result<()>;
// fdt_ui::assets
pub struct FdtAssets;                  // impl gpui_kit::AssetSource: our icons first, then gpui_kit::assets::Assets
pub mod icons {                        // gpui_kit::assets::IconName constants
    TIMELINE = ChartGantt, PERFORMANCE = Gauge, MEMORY = MemoryStick, NETWORK = ArrowDownUp, LOGS = ScrollText,
    WIDGETS = ListTree, STATE = Workflow, EXPORT = Upload, SETTINGS = Settings, BACK = ChevronLeft,
    FORWARD = ChevronRight, SEARCH = Search, KEYBOARD = Keyboard, INFO = Info }
// fdt_ui::theme
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference { #[default] System, Dark, Light }
pub fn resolve(pref: ThemePreference, os: Appearance) -> Appearance;
pub fn component_theme_json(appearance: Appearance) -> serde_json::Value;
pub fn install(cx: &mut App);                          // once, after gpui_kit::init
pub fn apply(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) -> Appearance;
pub fn os_appearance(window: Option<&Window>, cx: &App) -> Appearance;   // from window.appearance() / cx.window_appearance()
pub trait TokensExt { fn tokens(&self) -> &'static Palette; fn appearance(&self) -> Appearance; }
impl TokensExt for App                                  // reads Theme::global(cx).is_dark()
```

**What `component_theme_json(a)` returns** (with `p = palette(a)`):
- **Top-level keys:**
  - `"name"`: `"fdt Dark"` or `"fdt Light"`
  - `"mode"`: `"dark"` or `"light"`
  - `"is_default"`: false
  - `"font.family"`: `font::UI`; `"font.size"`: 13
  - `"mono_font.family"`: `font::MONO`; `"mono_font.size"`: 12
  - `"radius"`: 6; `"radius.lg"`: 8
  - `"shadow"`: false
- **`"colors"`**, every value made from `p` with `hex()` or `hex_with_alpha()`:

| Keys | Value |
|---|---|
| `background`, `list.background`, `list.even.background`, `table.background`, `table.even.background`, `tab.active.background` | `bg` |
| `muted.background`, `secondary.background`, `sidebar.background`, `title_bar.background`, `status_bar.background`, `tab_bar.background`, `tab.background`, `list.head.background`, `table.head.background`, `list.hover.background`, `table.hover.background`, `skeleton.background`, `group_box.background`, `accordion.background` | `surface` |
| `popover.background`, `accent.background`, `secondary.hover.background`, `sidebar.accent.background` | `elevated` |
| `border`, `input.border`, `sidebar.border`, `title_bar.border`, `status_bar.border`, `table.row.border`, `window.border`, `secondary.active.background`, `chart.grid` | `border` |
| `foreground`, `popover.foreground`, `accent.foreground`, `secondary.foreground`, `sidebar.foreground`, `sidebar.accent.foreground`, `tab.active.foreground`, `group_box.foreground` | `text` |
| `muted.foreground`, `tab.foreground`, `table.head.foreground`, `description_list_label.foreground` | `text_muted` |
| `primary.background`, `ring`, `caret`, `link.foreground`, `link.hover.foreground`, `link.active.foreground`, `list.active.border`, `table.active.border`, `drag_border`, `info.background`, `chart.1`, `sidebar.primary.background`, `progress_bar.background`, `slider.bar.background` | `accent` |
| `primary.hover.background` | `accent` with alpha `HOVER_FILL` |
| `primary.active.background` | `accent` with alpha `PRESS_FILL` |
| `primary.foreground`, `sidebar.primary.foreground`, `info.foreground`, `success.foreground`, `warning.foreground`, `danger.foreground` | `bg` in Dark, `elevated` in Light |
| `selection.background` | `accent` with alpha `SELECTION` |
| `list.active.background`, `table.active.background`, `drop_target.background` | `accent` with alpha `ACTIVE_ROW` |
| `success.background`, `chart.bullish` | `success` |
| `warning.background` | `warning` |
| `danger.background`, `chart.bearish` | `danger` |
| `chart.2` / `chart.3` / `chart.4` / `chart.5` | `lane_network` / `success` / `warning` / `lane_state` |
| `scrollbar.thumb.background` | `text_faint` |
| `overlay` | `scrim` with alpha `SCRIM_DARK` in Dark, `SCRIM_LIGHT` in Light |

Keys not listed keep gpui-component defaults.

**What `install` and `apply` do:**
- `install` deserializes both JSON values into `ThemeConfig`. It stores them as `Theme::global_mut(cx).dark_theme` and `light_theme`, then sets motion:
  - `duration_fast` = `HOVER`, `duration_normal` = `PANEL`, `duration_slow` = `OVERLAY`
  - all three easings = `Easing::cubic_bezier` of `EASE_OUT`
  - `Theme::global_mut(cx).focus_ring = true`
- `apply` resolves the preference against `os_appearance`, then calls `Theme::change(..)`. It re-applies the motion values, because `change` reloads the config. It returns the applied `Appearance`.

- [ ] **Step 1: Add the fonts**

Download into a fresh scratch directory, check, then copy:
```bash
D=$(mktemp -d) && cd "$D"
curl -sSLfO https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip
curl -sSLfO https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip
shasum -a 256 Inter-4.1.zip JetBrainsMono-2.304.zip
# expect 9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e  Inter-4.1.zip
# expect 6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf  JetBrainsMono-2.304.zip
unzip -q Inter-4.1.zip -d inter && unzip -q JetBrainsMono-2.304.zip -d jbm
```
Copy these into `crates/fdt_ui/assets/fonts/`:
- `inter/extras/ttf/Inter-{Regular,Medium,SemiBold}.ttf`
- `jbm/fonts/ttf/JetBrainsMono-{Regular,Medium}.ttf`
- `inter/LICENSE.txt` → `Inter-OFL.txt`
- `jbm/OFL.txt` → `JetBrainsMono-OFL.txt`

- [ ] **Step 2: Write the failing tests**

In `fonts.rs`:
- **`font_files_are_bundled_truetype`**: there are 5 names in the order above, and each `bytes` starts with `[0, 1, 0, 0]`.

In `assets.rs`:
- **`serves_our_icons_and_component_defaults`**:
  - `FdtAssets.load("icons/gauge.svg")` is `Ok(Some(_))`
  - `FdtAssets.load("icons/arrow-down.svg")` is `Ok(Some(_))` (a component default)
  - `FdtAssets.load("icons/nope.svg")` is `Ok(None)`

In `theme.rs`:
- **`resolve_follows_os_only_for_system`**: all 6 combinations.
- **`component_theme_maps_tokens`**:
  - `json["colors"]["background"] == "#0D0E11"`
  - `json["colors"]["muted.foreground"] == DARK.text_muted.hex()`
  - `json["colors"]["primary.background"] == DARK.accent.hex()`
  - `json["colors"]["sidebar.background"] == DARK.surface.hex()`
  - `json["colors"]["selection.background"] == DARK.accent.hex_with_alpha(alpha::SELECTION)`
  - `json["font.family"] == "Inter"`, `json["mono_font.family"] == "JetBrains Mono"`, `json["font.size"] == 13`, `json["radius"] == 6`, `json["radius.lg"] == 8`, `json["shadow"] == false`, `json["mode"] == "dark"`
  - for light, `json["colors"]["background"] == "#FBFBFC"`
- **`every_color_value_is_a_hex_token`**: every value under `"colors"`, in both themes, is a string of length 7 or 9 starting with `#`.

```rust
#[gpui_kit::test]
fn apply_switches_component_theme_and_motion(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        install(cx);
        assert_eq!(apply(ThemePreference::Dark, None, cx), Appearance::Dark);
        let t = Theme::global(cx);
        assert_eq!(t.colors.background, DARK.bg.hsla());
        assert_eq!(t.colors.foreground, DARK.text.hsla());
        assert_eq!(t.colors.border, DARK.border.hsla());
        assert_eq!(t.colors.danger, DARK.danger.hsla());
        assert_eq!(t.font_family.as_ref(), "Inter");
        assert_eq!(t.mono_font_family.as_ref(), "JetBrains Mono");
        assert_eq!(t.font_size, px(13.));
        assert_eq!(t.radius, radius::CONTROL);
        assert_eq!(t.radius_lg, radius::PANEL);
        assert_eq!(t.motion.duration_fast, motion::HOVER);
        assert_eq!(t.motion.duration_normal, motion::PANEL);
        assert_eq!(t.motion.duration_slow, motion::OVERLAY);
        assert_eq!(cx.tokens(), &DARK);
        apply(ThemePreference::Light, None, cx);
        assert_eq!(Theme::global(cx).colors.background, LIGHT.bg.hsla());
        assert_eq!(cx.appearance(), Appearance::Light);
    });
}
```
`Palette` derives `PartialEq` and `Debug` for `assert_eq!`.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p fdt_ui --lib`

Expected: a compile failure in `fonts`, `assets` and `theme`.

- [ ] **Step 4: Implement the three modules**

- `fonts::load` passes `FONT_FILES` bytes as `Cow::Borrowed` to `cx.text_system().add_fonts`.
- `FdtAssets` uses `gpui_kit::assets::icon_assets!(FdtIcons, [ChartGantt, Gauge, MemoryStick, ArrowDownUp, ScrollText, ListTree, Workflow, Upload, Settings, ChevronLeft, ChevronRight, Search, Keyboard, Info])`. It falls back to `gpui_kit::assets::Assets`, and `list` returns the union of both.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p fdt_ui`

Expected: all pass, including `token_usage`.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_ui
git commit -m "feat(ui): bundle Inter and JetBrains Mono, map tokens onto the component theme"
```

---

### Task 4: Follow the OS reduced-motion setting

**Files:**
- Create: `crates/fdt_ui/src/os.rs`
- Modify: `crates/fdt_ui/src/lib.rs`, `crates/fdt_ui/Cargo.toml`

**Interfaces (produces):**
```rust
pub fn prefers_reduced_motion() -> bool;                       // may block briefly on Linux; call off the UI thread
pub fn parse_gnome_enable_animations(stdout: &str) -> Option<bool>;
pub fn sync_reduced_motion(cx: &mut App);                      // = sync_reduced_motion_with(cx, prefers_reduced_motion)
pub fn sync_reduced_motion_with(cx: &mut App, read: fn() -> bool); // reads on the background executor, then cx.set_reduce_motion on the main thread
```

**How each OS answers:**
- **macOS:** `objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()`.
- **Windows:** `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, …)`. Reduced motion means animations are *off*.
- **Linux:** run `gsettings get org.gnome.desktop.interface enable-animations`. `false` means reduced. A missing command or unreadable output means `false`.

Add the platform dependencies, choosing versions already in `Cargo.lock` so nothing is duplicated:
- `[target.'cfg(target_os = "macos")'.dependencies] objc2-app-kit = { version = "0.3", default-features = false, features = ["std", "NSWorkspace"] }`
- `[target.'cfg(target_os = "windows")'.dependencies] windows = { version = "0.61", features = ["Win32_UI_WindowsAndMessaging"] }`

- [ ] **Step 1: Write the failing tests**
- **`parses_gsettings_output`**:
  - `"true\n"` → `Some(true)`
  - `"false"` → `Some(false)`
  - `"'yes'"` → `None`
  - `""` → `None`
- **`sync_applies_reader_result`**, a `#[gpui_kit::test]`: `cx.update(|cx| sync_reduced_motion_with(cx, || true)); cx.run_until_parked(); assert!(cx.update(|cx| cx.reduce_motion()));`. Then the same with `|| false`, asserting the flag is false.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_ui --lib os`

Expected: a compile failure.

- [ ] **Step 3: Implement `os.rs`** as specified.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fdt_ui --lib os && cargo clippy -p fdt_ui --all-targets -- -D warnings`

Expected: pass, with no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/fdt_ui Cargo.lock
git commit -m "feat(ui): follow the OS reduced-motion setting"
```

---

### Task 5: Frame statistics and the frame-time HUD

**Files:**
- Create: `crates/fdt_ui/src/hud/stats.rs`, `crates/fdt_ui/src/hud/mod.rs`
- Modify: `crates/fdt_ui/src/lib.rs`

**Interfaces:**
- Consumes: tokens and `TokensExt` (Tasks 2–3).
- Produces:
```rust
// fdt_ui::hud::stats (no GPUI)
pub struct Percentiles { pub p50: Duration, pub p95: Duration, pub p99: Duration }   // Debug, PartialEq
pub struct FrameStats { /* ring buffers */ }
impl FrameStats {
    pub const DEFAULT_CAPACITY: usize = 600;
    pub fn new(capacity: usize) -> Self;
    pub fn record_draw(&mut self, draw: Duration);
    pub fn record_interval(&mut self, interval: Duration, refresh: Duration); // also adds missed_refreshes to the dropped total
    pub fn draw_percentiles(&self) -> Option<Percentiles>;
    pub fn interval_percentiles(&self) -> Option<Percentiles>;
    pub fn dropped_frames(&self) -> u64;                  // since the last reset; not reduced by ring eviction
    pub fn recent_draws(&self) -> impl Iterator<Item = Duration> + '_;   // oldest → newest
    pub fn reset(&mut self);
}
pub fn missed_refreshes(interval: Duration, refresh: Duration) -> u64; // 0 if interval <= 1.5 × refresh, else round(interval / refresh) − 1
pub fn format_ms(d: Duration) -> String;                  // one decimal: 1.25 ms → "1.3"
// fdt_ui::hud
pub const DEFAULT_REFRESH: Duration;                      // 16.667 ms; used until a draw reports the display period
pub fn ingest(stats: &mut FrameStats, events: &[FrameEvent], window: WindowId, refresh: &mut Duration);
pub struct FrameHud { /* stats, visibility, collector, poll task, window id */ }
impl FrameHud {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self;  // hidden
    pub fn is_visible(&self) -> bool;
    pub fn set_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>);
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>);
    pub fn stats(&self) -> &FrameStats;
}
pub fn trace_users(cx: &App) -> usize;                    // visible HUDs across all windows
```

**How the pieces behave:**
- **Percentiles** use nearest rank: sort the samples, then take index `ceil(p/100 × n) − 1`.
- **`ingest`** handles only events whose `window_id` matches.
  - A `Draw` event records `draw_end − draw_start`. If it carries `refresh_interval: Some(r)`, set `*refresh = r`.
  - A `Present` event with `animation_interval: Some(i)` calls `record_interval(i, *refresh)`.
- **Tracing is shared by every window.** A global `HudTrace(usize)` counts visible HUDs. Showing a HUD increments it and calls `set_trace_enabled(true)`. Hiding it, or releasing the entity (register `cx.on_release` in `new`), decrements it. At 0, call `set_trace_enabled(false)`.
- **While visible**, a task polls `FrameTimingCollector::collect_unseen()` every 250 ms (`// token-check: allow: poll interval, not motion`), runs `ingest`, then `cx.notify()`. Hiding drops the task.

**What the HUD shows.** When hidden, `Render` draws an empty `div()`. When visible it draws a panel, `.id("frame-hud").test_support()`, positioned absolutely at the top right of its parent:
- **Panel:** `space::S12` from the top and right edges, width `layout::HUD_WIDTH`, background `elevated`, a 1 px `border`, radius `radius::PANEL`, padding `space::S12`, gap `space::S6`.
- **Text:** `font::MONO` at `font::LABEL` with `tabular_figures()`, colour `text`, labels in `text_muted`.
- **Three rows:**
  - `Draw   p50 1.2  p95 3.4  p99 5.1 ms`
  - `Frame  p50 8.3  p95 8.4  p99 16.6 ms` (shows `–` with no data)
  - `Dropped 3`
- **A bar graph** `layout::HUD_GRAPH_HEIGHT` tall, painted with `gpui_kit::canvas`:
  - It shows up to the last 120 draws.
  - The y-scale puts `refresh` at half height, and a 1 px line marks it.
  - Bars are `success` at ≤ 4 ms, `warning` up to `refresh`, and `danger` above.

- [ ] **Step 1: Write the failing stats tests** in `stats.rs`
- **`percentiles_use_nearest_rank`**: samples 1..=100 ms give p50 50, p95 95 and p99 99 ms. One sample of 7 ms gives 7 for all three.
- **`empty_stats_have_no_percentiles`**.
- **`missed_refreshes_rounds_to_whole_frames`** with a refresh of 8.333 ms:
  - an interval of 8.3 → 0
  - 12.0 → 0
  - 16.7 → 1
  - 25.0 → 2
  - 100.0 → 11
- **`capacity_bounds_samples_but_not_dropped_total`**: capacity 600. Recording 700 intervals of 25 ms at an 8.333 ms refresh leaves `recent_draws`/interval length 600 and `dropped_frames() == 1400`.
- **`reset_clears_everything`**.
- **`format_ms_uses_one_decimal`**: 1.25 ms → `"1.3"` and 16.667 ms → `"16.7"`.

- [ ] **Step 2: Write the failing ingest and view tests** in `hud/mod.rs`
- **`ingest_filters_by_window_and_tracks_refresh`**:
  - The events are a `Draw` for window 1 of 3 ms with refresh `Some(8.333 ms)`, a `Draw` for window 2, a `Present` for window 1 with `animation_interval: Some(25 ms)`, and a `Present` with `None`.
  - After ingest, `draw_percentiles().unwrap().p50 == 3 ms` (only one draw recorded) and `dropped_frames() == 2`.
- **`hud_is_hidden_by_default_and_toggles`**, a `#[gpui_kit::test]` that hosts a `FrameHud` with `cx.add_window_view`:
  - `exists("frame-hud") == false` and `trace_users == 0`
  - after `toggle`: `exists == true` and `trace_users == 1`
  - after a second `toggle`: hidden, and `trace_users == 0`
  - Define a local `exists` helper: `cx.update(|window, cx| { window.render_frame(cx); window.try_find(id).is_some() })`.
- **`releasing_a_visible_hud_stops_tracing`**: show the HUD inside a window, close the window (`cx.update(|window, _| window.remove_window())`), run until parked, then `trace_users == 0`.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p fdt_ui --lib hud`

Expected: a compile failure.

- [ ] **Step 4: Implement `stats.rs` and `hud/mod.rs`** as specified.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p fdt_ui`

Expected: all pass, including `token_usage`.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_ui
git commit -m "feat(ui): add frame statistics and the frame-time HUD"
```

---

### Task 6: Navigation model

**Files:**
- Create: `crates/fdt_app/src/nav.rs`
- Modify: `crates/fdt_app/src/lib.rs`

**Interfaces (produces, no GPUI):**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool { Timeline, Performance, Memory, Network, Logs, Widgets, State }
impl Tool {
    pub const ALL: [Tool; 7];                 // in the order above
    pub fn label(self) -> &'static str;       // "Timeline", "Performance", "Memory", "Network", "Logs", "Widgets", "State"
    pub fn slug(self) -> &'static str;        // "timeline", "performance", "memory", "network", "logs", "widgets", "state"
    pub fn index(self) -> usize;              // 0..=6
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page { Tool(Tool), Settings }
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionTime(pub u64);              // ns since session start; continuous across runs (spec §8.1)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeRange { pub start: SessionTime, pub end: SessionTime }
impl TimeRange { pub fn new(a: SessionTime, b: SessionTime) -> Self; }  // orders the ends
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectionKey(pub std::sync::Arc<str>);   // e.g. "frame:r1:4820"; meaning defined by later milestones
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location { pub page: Page, pub selection: Option<SelectionKey>, pub range: Option<TimeRange> }
impl Location {
    pub fn at(page: Page) -> Self;
    pub fn with_page(&self, page: Page) -> Self;    // keeps range, drops selection
    pub fn cleared(&self) -> Option<Self>;          // drops selection if any, else range if any, else None
}
pub struct NavHistory { /* back stack, current, forward stack, capacity */ }
impl NavHistory {
    pub const DEFAULT_CAPACITY: usize = 100;
    pub fn new(initial: Location) -> Self;
    pub fn with_capacity(initial: Location, capacity: usize) -> Self;
    pub fn current(&self) -> &Location;
    pub fn navigate(&mut self, to: Location) -> bool;   // false and no entry when to == current; clears forward
    pub fn go_back(&mut self) -> Option<&Location>;
    pub fn go_forward(&mut self) -> Option<&Location>;
    pub fn can_go_back(&self) -> bool;
    pub fn can_go_forward(&self) -> bool;
}
```

- [ ] **Step 1: Write the failing tests**
- **`tool_metadata`**: `ALL`, labels, slugs and indices match the lists above.
- **`navigate_pushes_and_clears_forward`**: Timeline → Memory → back gives Timeline. Navigating to Logs then makes `can_go_forward() == false`.
- **`navigating_to_the_current_location_is_ignored`**: returns false, and `can_go_back()` stays false.
- **`back_and_forward_round_trip`**.
- **`history_is_capped`**: capacity 100 and 150 navigations allow exactly 100 successful `go_back` calls.
- **`with_page_keeps_range_and_drops_selection`**.
- **`cleared_drops_selection_then_range`**: with both, `cleared()` keeps the range only; with the range only, it gives the bare location; with neither, it gives `None`.
- **`time_range_orders_its_ends`**.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib nav`

Expected: a compile failure.

- [ ] **Step 3: Implement `nav.rs`.** Use `VecDeque` for the back stack so dropping the oldest entry is O(1).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fdt_app --lib nav`

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add navigation model with capped back/forward history"
```

---

### Task 7: Fuzzy ranking and recent commands

**Files:**
- Create: `crates/fdt_app/src/palette/mod.rs`, `crates/fdt_app/src/palette/rank.rs`, `crates/fdt_app/src/palette/recent.rs`, `crates/fdt_app/benches/palette_rank.rs`
- Modify: `crates/fdt_app/src/lib.rs`, `crates/fdt_app/Cargo.toml` (`criterion.workspace = true` in `[dev-dependencies]`, plus `[[bench]] name = "palette_rank"` with `harness = false`)

**Interfaces (produces, no GPUI):**
```rust
// palette::rank
pub fn fuzzy_score(query: &str, haystack: &str) -> Option<i32>;
pub fn rank(query: &str, haystacks: &[String], recent: &[usize]) -> Vec<usize>;
// palette::recent
pub const RECENT_CAPACITY: usize = 20;
pub fn record_recent(recent: &mut Vec<String>, id: &str);   // move or insert at the front, keep at most 20
```

**Scoring algorithm.** The signature leaves this open, so implement exactly this:
- **Preparation.**
  - Split `query` on whitespace into terms. An empty query scores `Some(0)`.
  - Lower-case each `char` with `char::to_lowercase().next()`, keeping positions aligned.
  - A term longer than the haystack fails at once.
  - `boundary[j]` is true when `j == 0`, when `h[j-1]` is one of ` _-/.:`, or when the original `h[j-1]` is lowercase and `h[j]` uppercase.
- **The per-term recurrence.** Let `bonus(j) = 1 + 8·boundary[j]`.
  - `M[0][j] = bonus(j) − min(j, 5)` when `t[0] == h[j]`.
  - `M[i][j] = bonus(j) + max over k < j of (M[i−1][k] + (4 if k == j−1 else −min(j−k−1, 3)))` when `t[i] == h[j]`.
  - Both are −∞ when the characters differ.
  - The term's score is `max_j M[n−1][j]`.
  - Keep it O(n·m). The candidates are `k = j−1`, `j−2` and `j−3`, plus a running maximum of `M[i−1][k]` over `k ≤ j−4`.
- **The query's score** is the sum of its term scores, or `None` if any term fails.
- **`rank` ordering:**
  - An empty query lists `recent` first (most recent first, skipping duplicates and out-of-range indices), then the remaining indices in order.
  - Otherwise, keep only the matches and add a recency bonus of `20 − position` to items in `recent`. Sort by total descending, then haystack length ascending, then index ascending.

- [ ] **Step 1: Write the failing tests** in `rank.rs` and `recent.rs`
- **`matches_subsequence_case_insensitively`**: `fuzzy_score("MEM", "Go to Memory")` is `Some`, and `fuzzy_score("xyz", "Go to Memory")` is `None`.
- **`empty_query_scores_zero`**.
- **`word_start_beats_mid_word`**: `score("mem", "Go to Memory") > score("mem", "Remembered items")`.
- **`consecutive_beats_scattered`**: `score("net", "Go to Network") > score("net", "Nested items")`.
- **`every_term_must_match`**: `"net req"` matches `"Network: clear requests"`, but `"net xyz"` matches nothing.
- **`non_ascii_and_long_queries_do_not_panic`**:
  - `fuzzy_score("ß", "Straße")` is `Some`.
  - `fuzzy_score(&"ß".repeat(300), "Go to Memory")` is `None`.
  - `fuzzy_score("🙂", "Go to Memory")` is `None`.
- **`empty_query_lists_recents_first`**: `rank("", &["A","B","C"], &[2, 0, 2, 9]) == vec![2, 0, 1]`.
- **`recent_breaks_ties_for_matches`**: `rank("go", &["Go to Logs","Go to Memory"], &[1]) == vec![1, 0]`.
- **`record_recent_moves_to_front_and_caps`**: recording "a", "b", "a" gives `["a","b"]`. Recording 25 distinct ids keeps 20, newest first.

```rust
#[test]
#[cfg_attr(debug_assertions, ignore = "budget: run with --release")]
fn budget_rank_5000_items_per_keystroke() {
    const VERBS: [&str; 7] = ["Go to", "Show", "Open", "Copy", "Clear", "Export", "Toggle"];
    const NOUNS: [&str; 13] = ["Memory", "Network", "Logs", "Frame", "Request", "Widget", "State",
        "Timeline", "Heap snapshot", "Settings", "Range", "Selection", "Theme"];
    let items: Vec<String> = (0..5000).map(|i| format!("{} {} {}", VERBS[i % 7], NOUNS[i % 13], i)).collect();
    for q in ["s", "sh", "sho", "show", "show m", "show me", "net req", "frame 48", "zzz"] {
        std::hint::black_box(rank(q, &items, &[]));
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..20 {
            let t = std::time::Instant::now();
            std::hint::black_box(rank(q, &items, &[1, 2, 3]));
            worst = worst.max(t.elapsed());
        }
        assert!(worst <= std::time::Duration::from_millis(8), "query {q:?} took {worst:?}");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib palette`

Expected: a compile failure.

- [ ] **Step 3: Implement `rank.rs` and `recent.rs`** as specified.

- [ ] **Step 4: Run the tests to verify they pass, in debug and release**

Run: `cargo test -p fdt_app --lib palette && cargo test -p fdt_app --release --lib -- budget`

Expected: all pass. The release run includes `budget_rank_5000_items_per_keystroke`.

- [ ] **Step 5: Add the criterion bench**

Write `benches/palette_rank.rs`: `rank("show me", 5,000 items, &[1, 2, 3])`, with the item generator copied from the budget test. Run `cargo bench -p fdt_app --bench palette_rank -- --quick`. Expected: it reports a time well under 8 ms.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_app Cargo.lock
git commit -m "feat(app): add fuzzy palette ranking with recency and a 5,000-item budget test"
```

---

### Task 8: Actions, keymap and command registry

**Files:**
- Create: `crates/fdt_app/src/actions.rs`, `crates/fdt_app/src/keymap.rs`, `crates/fdt_app/src/palette/registry.rs`
- Modify: `crates/fdt_app/src/lib.rs`, `crates/fdt_app/src/palette/mod.rs`

**Interfaces:**
- Consumes: `nav::Tool` (Task 6).
- Produces:
```rust
// actions.rs: gpui_kit::actions!(fdt, [ ... ]) with exactly these names:
OpenPalette, ShowShortcuts, GoBack, GoForward, ClearSelection,
ShowTimeline, ShowPerformance, ShowMemory, ShowNetwork, ShowLogs, ShowWidgets, ShowState,
OpenSettings, ToggleFrameHud, UseSystemTheme, UseDarkTheme, UseLightTheme,
About, Quit, Minimize, Zoom, ToggleFullScreen, Hide, HideOthers, ShowAll
pub fn show_tool(tool: Tool) -> Box<dyn gpui_kit::Action>;
// keymap.rs
pub const SHELL_CONTEXT: &str = "Shell";
pub fn platform(mac: &'static str, other: &'static str) -> &'static str;   // cfg!(target_os = "macos")
pub fn bindings() -> Vec<gpui_kit::KeyBinding>;
// palette/registry.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Group { Navigation, View, Window, App }
#[derive(Clone)] pub struct CommandSpec { pub id: &'static str, pub label: &'static str,
    pub keywords: &'static [&'static str], pub group: Group, pub action: fn() -> Box<dyn gpui_kit::Action> }
pub fn commands() -> Vec<CommandSpec>;
pub fn haystack(spec: &CommandSpec) -> String;   // label, then keywords, space-separated
```

**Bindings.** Write keys as `platform(mac, other)`. "—" means the binding doesn't exist on that platform.

| Keys (macOS / others) | Action | Context |
|---|---|---|
| `cmd-k` / `ctrl-k` | OpenPalette | none |
| `cmd-1` … `cmd-7` / `ctrl-1` … `ctrl-7` | ShowTimeline … ShowState, in `Tool::ALL` order | Shell |
| `cmd-[` / `ctrl-[` | GoBack | Shell |
| `cmd-]` / `ctrl-]` | GoForward | Shell |
| `escape` | ClearSelection | Shell |
| `?` | ShowShortcuts | Shell |
| `cmd-,` / `ctrl-,` | OpenSettings | none |
| `cmd-q` / `ctrl-q` | Quit | none |
| `cmd-m` / — | Minimize | none |
| `ctrl-cmd-f` / `f11` | ToggleFullScreen | none |
| `cmd-h` / — | Hide | none |
| `alt-cmd-h` / — | HideOthers | none |

**Commands.** Each row is `id | label | keywords | group`.
- **Navigation group:**
  - `tool.timeline` | Go to Timeline | lanes, overview
  - `tool.performance` | Go to Performance | frames, jank, cpu
  - `tool.memory` | Go to Memory | heap, leaks, gc
  - `tool.network` | Go to Network | requests, http
  - `tool.logs` | Go to Logs | console, errors
  - `tool.widgets` | Go to Widget Tree | widgets, inspector, rebuilds
  - `tool.state` | Go to State | bloc, riverpod
  - `nav.back` | Go Back | history
  - `nav.forward` | Go Forward | history
  - `nav.clear` | Clear Selection | range, escape
- **App group:**
  - `app.settings` | Open Settings | preferences
  - `app.about` | About fdt | version, licence
  - `app.quit` | Quit fdt | exit
  - `app.hide`, `app.hide_others` and `app.show_all` (macOS only) | Hide fdt / Hide Others / Show All
- **View group:**
  - `view.frame_times` | Show Frame Times | hud, fps, performance
  - `view.shortcuts` | Show Keyboard Shortcuts | keys, help, cheat sheet
  - `theme.system` | Theme: Match System | appearance
  - `theme.dark` | Theme: Dark | appearance
  - `theme.light` | Theme: Light | appearance
- **Window group:**
  - `window.minimize` | Minimize Window | —
  - `window.zoom` | Zoom Window on macOS, Maximize Window elsewhere | —
  - `window.fullscreen` | Toggle Full Screen | —

`OpenPalette` is not in the registry.

- [ ] **Step 1: Write the failing tests**
- **`command_ids_and_labels_are_unique`**.
- **`every_bound_action_is_in_the_palette`**: for each binding in `bindings()` other than `OpenPalette`, some command's `(spec.action)().name() == binding.action().name()`.
- **`every_tool_has_a_command_and_a_shortcut`**: for each `Tool`, `show_tool(tool).name()` matches a command with id `"tool.<slug>"`.
- **`platform_picks_the_os_variant`**: on macOS `platform("a","b") == "a"`, elsewhere `"b"` (use `cfg!`).
- **`haystack_joins_label_and_keywords`**: for `tool.memory` it is `"Go to Memory heap leaks gc"`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib registry keymap`

Expected: a compile failure.

- [ ] **Step 3: Implement the three modules** as specified.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fdt_app --lib`

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add actions, cross-platform keymap and command registry"
```

---

### Task 9: Main window shell with sidebar and tool pages

**Files:**
- Create: `crates/fdt_ui/src/empty_state.rs`
- Create: `crates/fdt_app/src/shell/mod.rs`, `crates/fdt_app/src/shell/sidebar.rs`, `crates/fdt_app/src/shell/tool_page.rs`, `crates/fdt_app/src/app.rs`, `crates/fdt_app/src/test_support.rs`
- Modify: `crates/fdt_app/src/main.rs` (no arguments now runs the app), `crates/fdt_app/src/lib.rs`, `crates/fdt_ui/src/lib.rs`

**Interfaces:**
- Consumes: `FrameHud` (Task 5), `nav::*` (Task 6), `actions::*` and `keymap::*` (Task 8), `theme::{install, apply}` (Task 3), `os::sync_reduced_motion` (Task 4) and `FdtAssets`/`icons` (Task 3).
- Produces:
```rust
// fdt_ui::empty_state
#[derive(gpui_kit::IntoElement)] pub struct EmptyState { /* .. */ }
impl EmptyState {
    pub fn new(id: impl Into<gpui_kit::ElementId>, icon: gpui_kit::assets::IconName,
               heading: impl Into<gpui_kit::SharedString>, body: impl Into<gpui_kit::SharedString>) -> Self;
}
// fdt_app::shell
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct ToolCount { pub value: u64, pub attention: bool }
pub struct MainWindow { /* history, counts, hud, focus, palette (Task 11) */ }
impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self;   // starts at Page::Tool(Timeline), focuses itself
    pub fn current(&self) -> &Location;
    pub fn history(&self) -> &NavHistory;
    pub fn navigate(&mut self, to: Location, window: &mut Window, cx: &mut Context<Self>);
    pub fn set_counts(&mut self, counts: [Option<ToolCount>; 7], cx: &mut Context<Self>);
    pub fn focus_handle(&self) -> FocusHandle;
    pub fn hud(&self) -> &Entity<FrameHud>;
}
pub fn format_count(n: u64) -> String;
// fdt_app::app
pub fn init(cx: &mut App);                      // gpui_kit::init, fdt_ui::theme::install, cx.bind_keys(bindings()), global handlers
pub fn main_window_options(cx: &App) -> WindowOptions;
pub fn open_main_window(cx: &mut App) -> anyhow::Result<Entity<MainWindow>>;
pub fn run();                                   // application().with_assets(FdtAssets).run(..)
// fdt_app::test_support (#[cfg(test)])
pub fn init(cx: &mut TestAppContext);
pub fn open_shell(cx: &mut TestAppContext) -> (Entity<MainWindow>, &mut VisualTestContext);
pub fn exists(cx: &mut VisualTestContext, id: &str) -> bool;
pub fn press(cx: &mut VisualTestContext, mac: &'static str, other: &'static str);  // simulate_keystrokes(platform(..)) + run_until_parked
```

**`open_shell`** follows the verified pattern in Facts 5:
- Call `init(cx)`.
- `cx.add_window_view(|window, cx| { let shell = cx.new(|cx| MainWindow::new(window, cx)); *slot.borrow_mut() = Some(shell.clone()); Root::new(shell, window, cx) })`, with `slot` a `RefCell<Option<Entity<MainWindow>>>`.
- Focus the shell's handle, then `run_until_parked`.

**Shell behaviour:**
- The root element has `.id("shell").test_support()`, `.key_context(SHELL_CONTEXT)`, `.track_focus(..)` and a handler for each action below.
- **`Show<Tool>`** navigates to `current.with_page(Page::Tool(tool))`.
- **`GoBack` and `GoForward`** move through history. At either end they do nothing.
- **`ClearSelection`** navigates to `current.cleared()` if it is `Some`.
- **`ToggleFrameHud`** toggles the HUD.
- **Mouse back and forward buttons** anywhere in the shell (`on_mouse_down(MouseButton::Navigate(..))`) do the same as `GoBack` and `GoForward`.
- On window activation, call `fdt_ui::os::sync_reduced_motion(cx)`.

**Layout.**
- Left: the sidebar, width `layout::SIDEBAR_WIDTH`, background `surface`, with a 1 px `border` on its right edge.
- Right: the page area, background `bg`, containing the current page and the HUD overlay (`.relative()` parent).
- The title bar arrives in Task 12.

**The sidebar** uses `gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem}`. Element ids are `sidebar-<slug>`, `sidebar-export` and `sidebar-settings`.
- **Top:** "Timeline".
- **Group:** headed "Tools", listing Performance, Memory, Network, Logs, Widgets and State.
- **Footer:** "Export…", disabled in M1, with the tooltip "Connect an app to export a session"; then "Settings", which Task 10 wires up.
- **Each tool item** shows its `icons::*` icon and label. It is active when it is the current page.
- **Counts:** when a count is present, the item shows `format_count(value)` as a suffix, `.id("sidebar-count-<slug>").test_support()`. The colour is `danger` when `attention` is set and `text_muted` otherwise.

**`format_count`** behaves like this:
- below 1,000: the plain number
- below 10,000: one decimal, rounded down, plus `k` (`1234` → `1.2k`)
- below 1,000,000: whole thousands, rounded down, plus `k` (`12345` → `12k`)
- otherwise: one decimal, rounded down, plus `M`

**A tool page** (`.id("tool-page-<slug>").test_support()`) has two parts:
- A header row showing the tool's label at `font::PAGE`, weight `SEMIBOLD`, with padding `space::S16`/`space::S24`.
- A centred `EmptyState`. The `EmptyState` component shows its icon at `layout::ICON_MD` in `text_faint`, the heading at `font::SECTION`, weight `MEDIUM`, and the body in `text_muted` at `font::BODY`. Its maximum width is `layout::EMPTY_STATE_MAX_WIDTH` and its gap is `space::S8`.

**Empty-state copy** (heading, then body):

| Tool | Heading | Body |
|---|---|---|
| Timeline | Nothing recorded yet | Frames, requests, logs and GC events appear here on one time axis once an app is connected. |
| Performance | No frames yet | Frame timings appear here once an app is connected. Run it in profile mode for accurate timings. |
| Memory | No memory data yet | Heap usage and leak checks appear here once an app is connected. |
| Network | No requests yet | Requests appear here as your app makes them, once an app is connected. |
| Logs | No logs yet | Logs, print output and Flutter errors appear here once an app is connected. |
| Widgets | No widget tree yet | The widget tree appears here when an app is connected in debug mode. |
| State | No state events yet | BLoC and Riverpod events appear here when your app uses fdt_probe. |

**Startup (`app::run`):**
- `init`, then `fdt_ui::fonts::load(cx)`. On error, log a warning and continue with system fonts.
- `fdt_ui::theme::apply(ThemePreference::System, None, cx)`. Task 10 replaces this with the saved preference.
- `fdt_ui::os::sync_reduced_motion(cx)`, then `open_main_window(cx)`, then `cx.activate(true)`.
- Quit when the last window closes: `cx.on_window_closed`, checking `cx.windows().is_empty()`.
- `main_window_options` here:
  - `WindowOptions { window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, layout::WINDOW_DEFAULT, cx))), window_min_size: Some(layout::WINDOW_MIN), app_id: Some("dev.fdt.fdt".into()), ..TitleBar::window_options() }`.
  - Task 12 adds the per-OS parts.
- **Global handlers in `init`:**
  - `Quit` calls `cx.quit()`.
  - On macOS, `Hide`, `HideOthers` and `ShowAll` call `cx.hide()`, `cx.hide_other_apps()` and `cx.unhide_other_apps()`.

- [ ] **Step 1: Write the failing tests** in `shell/mod.rs`, using `test_support`
- **`starts_on_timeline_with_empty_history`**: `exists("tool-page-timeline")` and `!history().can_go_back()`.
- **`number_shortcuts_switch_tools`**: `press("cmd-2","ctrl-2")` gives `tool-page-performance` and no `tool-page-timeline`. `press("cmd-7","ctrl-7")` gives `tool-page-state`.
- **`back_and_forward_shortcuts`**: ⌘2, ⌘3, ⌘[ gives Performance. ⌘] gives Memory.
- **`back_at_start_is_a_no_op`**: pressing ⌘[ three times leaves the page on Timeline, with no panic.
- **`mouse_navigation_buttons`**: after ⌘3, `click_with_options("shell", ClickOptions::new().with_button(MouseButton::Navigate(NavigationDirection::Back)), cx)` gives Timeline. Forward gives Memory.
- **`clicking_a_sidebar_item_switches_tool`**: `window.click("sidebar-network", cx)` gives `tool-page-network`.
- **`escape_clears_selection_then_range`**:
  - First `navigate` to a location with a selection and a range.
  - The first `escape` leaves only the range; the second leaves neither.
  - A third `escape` adds no history entry.
- **`range_survives_tool_switch`**: navigate with a range, press ⌘4, and `current().range` is unchanged.
- **`toggle_frame_hud_shows_the_hud`**: `cx.dispatch_action(ToggleFrameHud)` gives `exists("frame-hud")`.
- **`sidebar_shows_counts`**: `set_counts` with Logs 214 and Performance 6 (attention) gives `exists("sidebar-count-logs")`, `exists("sidebar-count-performance")` and no `sidebar-count-memory`.
- **`format_count_cases`**:
  - 0 → `0`, 214 → `214`, 999 → `999`
  - 1000 → `1.0k`, 1234 → `1.2k`, 9999 → `9.9k`
  - 12345 → `12k`, 999999 → `999k`
  - 1234567 → `1.2M`

In `empty_state.rs`:
- **`renders_heading_and_body_ids`**: host an `EmptyState::new("es", IconName::Gauge, "H", "B")` in a test window, and `exists("es")`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib shell && cargo test -p fdt_ui --lib empty_state`

Expected: a compile failure.

- [ ] **Step 3: Implement** `EmptyState`, the shell modules, `app.rs`, `test_support.rs`, and the no-argument launch in `main.rs`.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass, including `token_usage`, with no warnings.

- [ ] **Step 5: Look at it**

Run `cargo run -p fdt_app` and check each of these:
- The window opens in the OS theme with Inter text.
- The sidebar lists Timeline, the Tools group, and Export and Settings at the bottom.
- ⌘1 to ⌘7 switch pages.
- ⌘[ and ⌘] and the mouse side buttons move through history.
- Closing the window quits the app.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "feat(app): add main window shell with sidebar, tool pages and navigation"
```

---

### Task 10: Settings and theme preference

**Files:**
- Create: `crates/fdt_app/src/settings.rs`, `crates/fdt_app/src/shell/settings_page.rs`
- Modify: `crates/fdt_app/src/shell/mod.rs`, `crates/fdt_app/src/shell/sidebar.rs`, `crates/fdt_app/src/app.rs`, `crates/fdt_app/src/test_support.rs`, `crates/fdt_app/src/lib.rs`

**Interfaces:**
- Consumes: `ThemePreference`, `apply`, `resolve` and `os_appearance` (Task 3); `MainWindow` (Task 9).
- Produces:
```rust
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings { pub theme: ThemePreference, pub recent_commands: Vec<String> }
pub struct SettingsStore { /* path: Option<PathBuf>, current: Settings */ }
impl SettingsStore {
    pub fn default_path() -> Option<PathBuf>;            // dirs::config_dir()/fdt/settings.json
    pub fn load(path: PathBuf) -> Self;                  // missing, unreadable or invalid → defaults (log::warn for invalid)
    pub fn in_memory() -> Self;                          // no file; saves are no-ops
    pub fn get(&self) -> &Settings;
    pub fn write(path: &Path, settings: &Settings) -> std::io::Result<()>;  // create dirs, write settings.json.tmp, rename
}
pub fn init_settings(cx: &mut App, store: SettingsStore); // sets the AppSettings global
pub fn settings(cx: &App) -> &Settings;
pub fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)); // updates now, writes on cx.background_spawn; logs errors
impl MainWindow { pub fn appearance_changed(&mut self, os: Appearance, window: &mut Window, cx: &mut Context<Self>); }
```

**Behaviour:**
- **`OpenSettings`** navigates to `current.with_page(Page::Settings)`, and the sidebar's "Settings" item does the same. Back and forward include the Settings page.
- **The theme actions.** `UseSystemTheme`, `UseDarkTheme` and `UseLightTheme` each call `update_settings(|s| s.theme = …)` and then `theme::apply(pref, Some(window), cx)`.
- **Following the OS.** `MainWindow::new` subscribes with `cx.observe_window_appearance(window, …)`, which calls `appearance_changed(theme::os_appearance(Some(window), cx), …)`. `appearance_changed` re-applies the theme only when the saved preference is `System`.
- **Startup.** `app::run` does the following in order:
  - `SettingsStore::load(default_path())`, or `in_memory()` when there is no config directory, *before* `application().run`
  - `init_settings`
  - `apply(settings.theme, …)`
- **Tests.** `test_support::init` calls `init_settings(cx, SettingsStore::in_memory())`.

**The Settings page** has `.id("settings-page").test_support()` and a page title "Settings". It has two sections:
- **"Appearance":** a "Theme" row with a three-part segmented control (`gpui_kit::component::button::ButtonGroup`). The buttons are "Match system", "Dark" and "Light", with ids `theme-system`, `theme-dark` and `theme-light`. The selected one is shown as selected, and each dispatches its action.
- **"Keyboard":** a "Keyboard shortcuts" row with a button "Show" plus a `Kbd` for `?` (`settings-shortcuts`). It dispatches `ShowShortcuts`, which Task 13 handles.

- [ ] **Step 1: Write the failing tests**

Pure tests in `settings.rs` (use `tempfile::tempdir()`):
- **`missing_file_gives_defaults`**.
- **`write_then_load_round_trips`**, including `recent_commands`.
- **`corrupt_file_gives_defaults`**: `{not json` gives defaults.
- **`wrong_types_give_defaults`**: `{"theme": 5}` gives defaults.
- **`unknown_fields_are_ignored`**: `{"theme":"dark","future":1}` gives `ThemePreference::Dark`.
- **`write_creates_parent_directories`**.
- **`write_to_an_unwritable_path_errors`**: a path under a regular file (`<tmp>/file.txt/settings.json`) gives `Err`.

GPUI tests:
- **`failed_save_keeps_the_app_running`**:
  - `init_settings` with `SettingsStore::load(<tmp>/file.txt/settings.json)`.
  - `update_settings(|s| s.theme = Dark)`, then `run_until_parked()`.
  - `settings(cx).theme == Dark`, with no panic.
- **`theme_actions_apply_and_persist_in_memory`**: dispatch `UseDarkTheme` and check `Theme::global(cx).is_dark()` and `settings(cx).theme == Dark`. Dispatch `UseLightTheme` and check the theme is light.
- **`system_preference_follows_os_but_pinned_does_not`**:
  - With the theme `System`, `shell.update_in(cx, |s, window, cx| s.appearance_changed(Appearance::Light, window, cx))` makes the theme light.
  - `appearance_changed(Appearance::Dark)` then makes it dark.
  - With the theme `Light`, `appearance_changed(Dark)` leaves it light.
  - If `update_in` is unavailable, use `cx.update(|window, cx| shell.update(cx, |s, cx| s.appearance_changed(.., window, cx)))`.
- **`settings_shortcut_opens_settings_and_back_returns`**: `press("cmd-,","ctrl-,")` gives `exists("settings-page")`. ⌘[ gives `tool-page-timeline`.
- **`clicking_theme_buttons_applies_theme`**: `click("theme-light")` makes the theme light.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib settings shell`

Expected: a compile failure.

- [ ] **Step 3: Implement** the settings module and the Settings page, and wire up `app.rs`, the shell and the sidebar.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass, with no warnings.

- [ ] **Step 5: Look at it**

Run `cargo run -p fdt_app` and check:
- Choosing "Light" persists after relaunch.
- With "Match system", switching macOS appearance (System Settings → Appearance) changes the app live.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): persist settings and add the theme preference"
```

---

### Task 11: Command palette view

**Files:**
- Create: `crates/fdt_app/src/palette/view.rs`
- Modify: `crates/fdt_app/src/palette/mod.rs`, `crates/fdt_app/src/shell/mod.rs`

**Interfaces:**
- Consumes: `rank`, `record_recent` (Task 7); `commands`, `haystack`, `CommandSpec` (Task 8); `settings`, `update_settings` (Task 10); `MainWindow::focus_handle` (Task 9).
- Produces:
```rust
pub struct PaletteModel { /* specs, haystacks, query, ranked */ }
impl PaletteModel {
    pub fn new(specs: Vec<CommandSpec>, recent_ids: &[String]) -> Self;  // ranked for the empty query
    pub fn set_query(&mut self, query: &str, recent_ids: &[String]);
    pub fn query(&self) -> &str;
    pub fn ranked(&self) -> &[usize];
    pub fn spec(&self, index: usize) -> &CommandSpec;
}
pub fn open_palette(shell_focus: FocusHandle, window: &mut Window, cx: &mut App) -> Entity<PaletteModel>;
impl MainWindow { pub fn palette(&self) -> Option<&Entity<PaletteModel>>; }  // the most recently opened, for tests
```

**`open_palette`** works as follows. Recent ids map to registry indices by `id`.
- **Setup.** Create an `Entity<PaletteModel>` and an `Entity<CommandState>`, then call `window.open_dialog`.
- **The dialog builder:**
  - On its first call, it focuses the `CommandState` (Facts 3).
  - It reads the model, then builds one `CommandItem` per ranked spec with `.label(spec.label)` and a `.child(..)` row. The row shows the label at `font::BODY` in `text`, with the `Kbd` for `(spec.action)()` aligned right.
  - It returns `dialog.w(layout::PALETTE_WIDTH).margin_top(layout::PALETTE_TOP_OFFSET).close_button(false)` with a child: a wrapper `div` holding the `Command`.
  - The wrapper has `.on_action(|_: &OpenPalette, window, cx| window.close_dialog(cx))`, so ⌘K closes an open palette.
- **The `Command`:**
  - `.filterable(false)`
  - `.placeholder("Search commands…")`
  - `.empty(|..| "No matching commands")`
  - `.max_h(layout::PALETTE_MAX_HEIGHT)`
  - `.on_query`, which calls `model.update(cx, |m, cx| { m.set_query(q, recents); cx.notify() })`
  - `.on_confirm`, described next
- **On confirm**, with `spec = ranked[ix.row]`:
  - `update_settings(|s| record_recent(&mut s.recent_commands, spec.id))`
  - `window.close_dialog(cx)`, then `shell_focus.focus(window, cx)`
  - `window.dispatch_action((spec.action)(), cx)`
- **The `OpenPalette` handler** in `MainWindow`: if `window.has_active_dialog(cx)`, close it. Otherwise call `open_palette(self.focus_handle(), ..)` and store the returned model.

- [ ] **Step 1: Write the failing tests** in `palette/view.rs`, using `test_support::open_shell`
- **`cmd_k_opens_the_palette`**: `press("cmd-k","ctrl-k")` makes `window.has_active_dialog(cx)` true.
- **`cmd_k_again_closes_it`** (Review focus 3): a second press makes `has_active_dialog` false, and only one model was created.
- **`typing_and_enter_runs_the_top_command`**: `simulate_input("memory")` then `press("enter","enter")`. The page is `Tool(Memory)`, the dialog is closed, and `history().can_go_back()` is true.
- **`escape_closes_without_side_effects`**: open, then `escape`. The dialog is closed, the page is still Timeline, and `!can_go_back()`.
- **`recent_command_is_first_on_next_open`**: run "Go to Logs" through the palette. Reopen it, and `model.ranked()[0]`'s spec id is `"tool.logs"`.
- **`shortcut_characters_are_text_in_the_palette`** (Review focus 2): open, then `simulate_input("?")`. `model.query() == "?"` and the dialog is still open.
- **`ranking_drives_rows`**: `simulate_input("go back")` puts `nav.back` at `ranked()[0]`.
- **`long_non_ascii_query_is_safe`** (Review focus 4): `simulate_input(&"ß".repeat(300))` leaves `ranked()` empty, with no panic.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib palette::view`

Expected: a compile failure.

- [ ] **Step 3: Implement `view.rs`** and the shell handler.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass.

- [ ] **Step 5: Look at it**

Run `cargo run -p fdt_app` and check:
- ⌘K opens a centred palette in the next frame, with no visible delay.
- The arrow keys move the selection, and shortcut hints show on the right.
- "Show Frame Times" toggles the HUD.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add the command palette with fuzzy ranking and recents"
```

---

### Task 12: Title bar, per-OS window chrome and app menus

**Files:**
- Create: `crates/fdt_app/src/shell/title_bar.rs`, `crates/fdt_app/src/menus.rs`
- Modify: `crates/fdt_app/src/app.rs`, `crates/fdt_app/src/shell/mod.rs`, `crates/fdt_app/src/lib.rs`

**Interfaces:**
- Consumes: `commands` (Task 8), `MainWindow` (Task 9).
- Produces:
```rust
pub fn app_menus() -> Vec<gpui_kit::Menu>;
pub fn main_window_options(cx: &App) -> WindowOptions;   // extended with per-OS fields
```

**The title bar** is `gpui_kit::component::TitleBar` across the top of the shell, above the sidebar and page. From left to right:
- **Back and forward:** ghost icon buttons `nav-back` and `nav-forward` with `icons::BACK` and `icons::FORWARD`. They are disabled when history can't move. Their tooltips are "Back" and "Forward" with the shortcut.
- **The app label:** "No app connected" in `text_muted` at `font::BODY`.
- **A flexible spacer.**
- **The palette button** `palette-button`: width `layout::PALETTE_BUTTON_WIDTH`, background `bg`, a 1 px `border`, radius `radius::CONTROL`. It shows `icons::SEARCH`, "Search commands" in `text_muted` and the `Kbd` for `OpenPalette`. Clicking it dispatches `OpenPalette`.

All clickable parts use `.test_support()`.

**Per-OS window options:**
- **macOS:** `TitleBar::window_options()` defaults: a transparent title bar with the traffic lights.
- **Windows:** the same; `TitleBar` draws the caption buttons, and the native hit-test gives Snap Layouts.
- **Linux:** `window_decorations: Some(WindowDecorations::Client)`, with the shell's root wrapped in `gpui_kit::component::window_border()`, so resize edges and the frame are drawn on the client.

**Window actions** on the shell:
- `Minimize` calls `window.minimize_window()`.
- `Zoom` calls `window.zoom_window()`.
- `ToggleFullScreen` calls `window.toggle_fullscreen()`.

**App menus** (`cx.set_menus(app_menus())` in `run`, on every OS):
- **fdt:**
  - About fdt
  - —
  - Settings…
  - —
  - Services (`os_submenu`)
  - —
  - Hide fdt, Hide Others, Show All
  - —
  - Quit fdt
- **View:**
  - Command Palette… (`OpenPalette`)
  - Show Frame Times
  - —
  - Theme: Match System, Theme: Dark, Theme: Light
  - —
  - Toggle Full Screen
- **Go:**
  - Back, Forward
  - —
  - the seven Go to … commands
- **Window:** Minimize, Zoom
- **Help:** Keyboard Shortcuts

- [ ] **Step 1: Write the failing tests**
- **`back_button_is_inert_until_there_is_history`**:
  - `click("nav-back")` at the start leaves the page on Timeline.
  - After ⌘2, `click("nav-back")` returns to Timeline.
  - `click("nav-forward")` gives Performance.
- **`palette_button_opens_the_palette`**: `has_active_dialog` after `click("palette-button")`.
- **`menus_only_use_registered_commands`**: walk `app_menus()` recursively. Every `MenuItem::Action { action, .. }` other than `OpenPalette` has a registry command with the same `name()`. Top-level names are `["fdt", "View", "Go", "Window", "Help"]`.
- **`window_options_use_layout_tokens`**:
  - `main_window_options(cx).window_min_size == Some(layout::WINDOW_MIN)` and `app_id == Some("dev.fdt.fdt")`
  - on Linux, `window_decorations == Some(WindowDecorations::Client)` (`#[cfg(target_os = "linux")]`)

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib title_bar menus`

Expected: a compile failure.

- [ ] **Step 3: Implement** the title bar, the menus and the window options.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass.

- [ ] **Step 5: Look at it on macOS**

Run `cargo run -p fdt_app` and check:
- The traffic lights sit vertically centred against the back/forward buttons.
- You can drag the window by the title bar.
- Double-clicking the title bar zooms.
- The menu bar shows fdt, View, Go, Window and Help, and each item works.
- ⌃⌘F enters full screen.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add title bar, per-OS window chrome and app menus"
```

---

### Task 13: Shortcut cheat sheet and About dialog

**Files:**
- Create: `crates/fdt_app/src/shell/shortcuts.rs`, `crates/fdt_app/src/shell/about.rs`
- Modify: `crates/fdt_app/src/shell/mod.rs`

**Interfaces:**
- Consumes: `commands`, `Group` (Task 8).
- Produces:
```rust
pub fn bound_commands(window: &Window) -> Vec<(Group, Vec<CommandSpec>)>;  // commands with a binding, grouped in Group order
pub fn open_shortcuts(window: &mut Window, cx: &mut App);
pub fn open_about(window: &mut Window, cx: &mut App);
```

**Behaviour:**
- **`ShowShortcuts`** (the `?` key, the palette, or the Settings button) opens a dialog titled "Keyboard shortcuts".
  - Each group has a heading, its `Group` name in `text_muted` at `font::LABEL`, weight `MEDIUM`.
  - Each row shows the label and its `Kbd`, with id `shortcut-row-<command id>`.
  - Rows come from `bound_commands`, so they never drift from the keymap. A command has a binding when `Kbd::binding_for_action(..)` returns `Some`.
- **`About`** opens a dialog (`about-dialog`) containing:
  - "fdt" at `font::PAGE`
  - `Version {CARGO_PKG_VERSION}`
  - "A fast, native Flutter DevTools."
  - "Licensed under Apache-2.0."
- Both dialogs close with Esc.

- [ ] **Step 1: Write the failing tests**
- **`question_mark_opens_the_cheat_sheet`**: `press("?","?")` gives `has_active_dialog`, `exists("shortcut-row-tool.memory")` and `exists("shortcut-row-nav.back")`.
- **`unbound_commands_are_not_listed`**: no `shortcut-row-theme.dark`.
- **`escape_closes_the_cheat_sheet`**.
- **`about_shows_the_version`**: `dispatch_action(About)` gives `exists("about-dialog")`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app --lib shortcuts about`

Expected: a compile failure.

- [ ] **Step 3: Implement** both dialogs and their shell handlers.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add keyboard shortcut cheat sheet and About dialog"
```

---

### Task 14: Startup trace

**Files:**
- Create: `crates/fdt_app/src/startup.rs`
- Modify: `crates/fdt_app/src/main.rs`, `crates/fdt_app/src/app.rs`, `crates/fdt_app/tests/cli.rs`, `crates/fdt_app/src/lib.rs`

**Interfaces:**
- Produces:
```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartupReport { pub first_paint: Duration, pub interactive: Duration }
impl StartupReport { pub fn to_json(&self) -> String; }   // {"first_paint_ms":123.4,"interactive_ms":234.5}
pub fn run_with_startup_trace(t0: Instant);                // in app.rs: like run(), measures, prints, quits
```

**How it measures.** `main` captures `t0 = Instant::now()` as its first statement. `--startup-trace` calls `run_with_startup_trace(t0)`:
- **First paint.** Inside the `open_window` build closure, register `window.on_next_frame`. In that callback, register a second `on_next_frame`. The second callback runs after the first frame has been drawn and presented, so record `first_paint = t0.elapsed()` there.
- **Interactive.** Then dispatch `OpenPalette` to the focused shell. Use the same two-step `on_next_frame` wait to record `interactive = t0.elapsed()`.
- **Finish.** Print `to_json()` on stdout and call `cx.quit()`.
- Usage becomes `usage: fdt [--version] [--startup-trace]`.

- [ ] **Step 1: Write the failing tests**
- **`report_json_uses_one_decimal_milliseconds`**: 123.44 ms and 234.56 ms give `{"first_paint_ms":123.4,"interactive_ms":234.6}`.
- In `tests/cli.rs`, extend the usage test: stderr contains `--startup-trace`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p fdt_app startup cli`

Expected: failures.

- [ ] **Step 3: Implement** the report and the startup trace.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace`

Expected: all pass.

- [ ] **Step 5: Measure on this Mac**

Run: `cargo build --release -p fdt_app && for i in 1 2 3 4 5; do ./target/release/fdt --startup-trace; done`

Expected: five JSON lines. Note the medians; Task 16 checks them against the budgets.

- [ ] **Step 6: Commit**

```bash
git add crates/fdt_app
git commit -m "feat(app): add --startup-trace to measure first paint and interactive time"
```

---

### Task 15: The `jank_lab` fixture app

**Files:**
- Create: `fixtures/jank_lab/` (from `flutter create`, then replaced), including:
  - `lib/main.dart`
  - `lib/fixture_server.dart`
  - `lib/scenarios/{home,jank,rebuilds,leaks,network,deep_tree,logs}.dart`
  - `test/fixture_server_test.dart`, `test/scenarios_test.dart`
  - `README.md`
- Modify: `fixtures/jank_lab/macos/Runner/DebugProfile.entitlements` and `Release.entitlements`
- Modify: `.github/workflows/ci.yml` (add the `jank_lab` job)

**Interfaces (Dart, produces):**
```dart
class FixtureServer {
  static Future<FixtureServer> start({Duration imageDelay = const Duration(milliseconds: 1400)});
  Uri get baseUri;                       // http://127.0.0.1:<port>
  Future<void> close();
}
const Map<String, String> scenarioRoutes = {
  'jank': '/jank', 'rebuilds': '/rebuilds', 'leaks': '/leaks',
  'network': '/network', 'deep-tree': '/deep-tree', 'logs': '/logs',
};
class AppEvents extends ChangeNotifier { static final AppEvents instance; int get listenerCountForTesting; void ping(); }
class LeakJar { static final List<ScrollController> controllers; }
class HeavyBuildPanel extends StatefulWidget { const HeavyBuildPanel({super.key, this.buildCost = const Duration(milliseconds: 24)}); }
class ProductGrid extends StatelessWidget { static const tileCount = 64; }
class ProductTile extends StatelessWidget {}
class DeepTreeScreen extends StatelessWidget { const DeepTreeScreen({super.key, this.depth = 200, this.nodes = 50000}); }
```

**Scenarios.** Every one is reachable from the home list and by `--dart-define=SCENARIO=<key>`. `main` reads it with `String.fromEnvironment('SCENARIO')` and uses it as the initial route.
- **jank.**
  - `HeavyBuildPanel` is driven by a repeating `AnimationController`, so frames are continuous. Each build busy-waits `buildCost` with a `Stopwatch` loop, about 3× the 8.3 ms budget.
  - A switch turns on "Raster heavy": 40 overlapping tiles, each `Opacity(0.5)` over `BackdropFilter(ImageFilter.blur(sigmaX: 12, sigmaY: 12))`, animated.
- **rebuilds.** `CartScreen` calls `setState` on a `Timer.periodic(16 ms)` counter shown by `CartBadge`. Its `build` returns the badge *and* `ProductGrid` (64 `ProductTile`s, a few hundred widgets), so the whole grid rebuilds each tick.
- **leaks.**
  - `LeakyScreen.initState` adds a listener closure that captures `this` to `AppEvents.instance` and never removes it, so the state stays reachable after unmount.
  - It creates a `TextEditingController` that is never disposed.
  - It disposes a `ScrollController` in `dispose()` but also appends it to `LeakJar.controllers`, so it is disposed but still held.
- **network.** A `FixtureServer` starts on page load, bound to loopback. Buttons:
  - "Load products": `package:http` GET of `/v2/products?page=3`.
  - "Place order": a POST that returns 500.
  - "Load hero image": `Image.network` of the PNG.
  - "Download blob".
  - "Burst 100 requests".
  - "Open WebSocket": sends "ping <n>" every 2 s to `/ws` and shows the echoes.
- **deep-tree.**
  - A chain of `depth` nested `Padding(padding: EdgeInsets.zero)` widgets ends in a `Wrap` inside a `SingleChildScrollView`.
  - The `Wrap` holds `(nodes − depth) ~/ 2` leaves. Each leaf is `SizedBox(width: 2, height: 2, child: ColoredBox(...))`.
- **logs.** Buttons:
  - "Emit levels": `developer.log` at levels 500, 800, 900 and 1000, with names `jank_lab.net` and `jank_lab.ui`.
  - "Print burst": 1,000 `print` lines.
  - "Multi-line record": one 30-line `developer.log`.
  - "Report Flutter error": `FlutterError.reportError` with a summary and a widget-style description.
  - "Throw in build": shows a widget whose `build` throws.

**Server routes:**

| Request | Response |
|---|---|
| `GET /v2/products?page=N` | 200 `application/json`: `{"page":N,"items":[20 × {"id":int,"name":"Product <id>","price":double}]}` |
| `POST /v2/orders` | 500 `application/json`: `{"error":"payment_declined","message":"Card declined by issuer"}` |
| `GET /img/hero_banner.png` | after `imageDelay`, 200 `image/png`: a 512×256 gradient encoded with `package:image` |
| `GET /files/blob.bin` | 200 `application/octet-stream`: 262,144 bytes |
| `GET /ws` | WebSocket echo |
| anything else | 404 `{"error":"not_found"}` |

- [ ] **Step 1: Create the project**

```bash
cd fixtures && flutter create --org dev.fdt --project-name jank_lab --platforms=macos,windows,linux,ios,android --empty jank_lab && cd jank_lab && flutter pub add http image
```

Then set the version constraints in `pubspec.yaml`: `environment: sdk: ^3.11.0` and `flutter: ">=3.41.0"`. Flutter 3.41 is the oldest supported release (spec §5.3).

Add `com.apple.security.network.server` and `com.apple.security.network.client` (both `true`) to both macOS entitlements files. Without them, the macOS sandbox blocks the local server.

- [ ] **Step 2: Write the failing tests**

`test/fixture_server_test.dart` uses plain `test()`, with no widget binding, so real sockets work. Start the server with `imageDelay: Duration.zero` in `setUp` and close it in `tearDown`.
- **products**: 200, JSON `page == 3`, 20 items.
- **orders**: POST gives 500 and `error == "payment_declined"`.
- **hero image**: 200 and `content-type` `image/png`. The body starts with the PNG signature `[137, 80, 78, 71]`.
- **blob**: 262,144 bytes.
- **websocket**: `WebSocket.connect('ws://…/ws')` echoes `"ping 1"`.
- **unknown route**: 404.

`test/scenarios_test.dart` uses `testWidgets`:
- **home lists six scenarios**: one `ListTile` per `scenarioRoutes` key.
- **leaky screen keeps its listener after pop**: push `/leaks`, pop, and expect `AppEvents.instance.listenerCountForTesting` to have grown by 1 and `LeakJar.controllers.length` by 1.
- **product grid has 64 tiles**: `find.byType(ProductTile)` finds 64. Pump `CartScreen`, then dispose it before the test ends so the timer stops.
- **deep tree builds the requested shape**: `DeepTreeScreen(depth: 20, nodes: 1000)` gives `find.byType(ColoredBox)` 490.
- **heavy build panel builds**: `HeavyBuildPanel(buildCost: Duration(milliseconds: 1))` pumps without error.
- **logs screen has its buttons**: one test checking all five button labels.

- [ ] **Step 3: Run them to verify they fail**

Run: `cd fixtures/jank_lab && flutter test`

Expected: failures, because the files don't exist yet.

- [ ] **Step 4: Implement the app**

Write the app, the server and the scenarios. Each scenario file has a one-paragraph doc comment saying what it provokes and which fdt tool should show it.

- [ ] **Step 5: Run the checks**

Run: `cd fixtures/jank_lab && flutter analyze && flutter test`

Expected: `No issues found!` and all tests pass.

- [ ] **Step 6: Check one scenario by hand**

Run: `cd fixtures/jank_lab && flutter run -d macos --profile --dart-define=SCENARIO=jank`

Expected: the jank screen opens directly. Flutter's performance overlay, or DevTools, shows frames over budget.

- [ ] **Step 7: Write the README and add the CI job**

`README.md` lists each scenario, what it provokes, and its `flutter run` command.

Add this job to `ci.yml`:
```yaml
  jank_lab:
    name: jank_lab (Flutter ${{ matrix.flutter }})
    runs-on: ubuntu-24.04
    strategy:
      fail-fast: false
      matrix:
        flutter: ["3.41.9", "3.50.0"]
    defaults:
      run:
        working-directory: fixtures/jank_lab
    steps:
      - uses: actions/checkout@v4
      - uses: subosito/flutter-action@v2
        with:
          flutter-version: ${{ matrix.flutter }}
          channel: stable
      - run: flutter pub get
      - run: flutter analyze
      - run: flutter test
```

Run `ruby -ryaml -e 'YAML.load_file(".github/workflows/ci.yml"); puts "ok"'`. Expected: `ok`.

- [ ] **Step 8: Commit**

```bash
git add fixtures/jank_lab .github/workflows/ci.yml
git commit -m "feat(fixtures): add jank_lab Flutter app with jank, leak, network, deep-tree and log scenarios"
```

---

### Task 16: Milestone verification and review notes

**Files:**
- Create: `docs/milestones/m1-review.md`

- [ ] **Step 1: Run every automated check**

Run:
```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace \
  && cargo test --workspace --release -- budget && cargo deny check licenses \
  && (cd fixtures/jank_lab && flutter analyze && flutter test)
```

Expected: every step passes. Record the test counts.

- [ ] **Step 2: Measure startup on this machine (R1: Apple M3 Max)**

Run: `cargo build --release -p fdt_app && for i in 1 2 3 4 5 6 7; do ./target/release/fdt --startup-trace; done`

Expected: median `first_paint_ms` ≤ 200 and median `interactive_ms` ≤ 400. If either misses, **stop and report** the numbers. Do not tune budgets.

- [ ] **Step 3: Measure idle memory and CPU**

Run:
```bash
./target/release/fdt & PID=$!; sleep 30; for i in 1 2 3 4 5; do ps -o rss=,%cpu= -p $PID; sleep 2; done; kill $PID
```

Expected: every RSS sample is ≤ 153600 KB (150 MB) and every CPU sample is ≤ 1.0.

- [ ] **Step 4: Write `docs/milestones/m1-review.md`**

It holds the measured numbers from Steps 1–3, plus a checklist for the author's review on macOS, Windows and Linux:
- **Visual review in both themes**, against the design system (§10.4–10.6): sidebar, tool pages, palette, cheat sheet, Settings, About and the HUD. Save screenshots next to the file as `m1-<os>-<theme>-<view>.png`.
- **Frame times.**
  - With the HUD shown, resize the window continuously for 10 s. Record p99 frame and draw times: on R1, frame ≤ 8.3 ms and draw ≤ 4 ms; on R2 and R3, frame ≤ 16.7 ms.
  - Switch tools with ⌘1–⌘7. Every switch draws within 50 ms.
- **Palette** opens on the next frame, with no visible delay.
- **Reduced motion**: with the OS setting on, the palette and dialogs appear without animation.
- **Native feel.**
  - macOS: traffic lights, menus, full screen, ⌘ shortcuts.
  - Windows: caption buttons, Snap Layouts on hovering maximize, Ctrl shortcuts.
  - Linux: client-side frame and resize on GNOME and KDE, under both Wayland and X11.
- **Screen readers** (spec §10.7): record what VoiceOver, Narrator and Orca announce for the sidebar items, title bar buttons and palette rows. Gaps are a known limitation, not a blocker.

- [ ] **Step 5: Commit**

```bash
git add docs/milestones/m1-review.md
git commit -m "docs: add M1 verification results and review checklist"
```
