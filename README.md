<p align="center">
  <img src="assets/icon/linux/fdt-128.png" width="128" height="128" alt="fdt icon">
</p>

<h1 align="center">fdt</h1>

<p align="center">
  A fast, native Flutter DevTools for macOS, Windows and Linux, written in Rust.
</p>

<p align="center">
  <a href="https://github.com/nikakirkitadze/flutter-devtools-rs/actions/workflows/ci.yml"><img src="https://github.com/nikakirkitadze/flutter-devtools-rs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License: Apache-2.0"></a>
</p>

---

Flutter DevTools lags on big timelines, loses network logs, and leaves you to work out why a
frame janked. fdt is a native desktop app that connects to your running Flutter app and tries to
answer the questions directly: why did this frame jank, what is rebuilding too much, and what
happened around this failed request. Everything is tied together on one time axis, and it stays
smooth with a million log lines.

**Status: early development.** The Connect page, Logs, Network, Performance and Timeline tools
work today. Still to come: Memory (leaks and heap snapshots), the widget inspector, an optional
in-app probe package, saved sessions, and signed installers. Daily use happens on macOS; Windows
and Linux are built and tested in CI.

## What it does

- **Logs** and **Network**: search and filter a million log lines or a hundred thousand requests
  while the app keeps running, with details for every record and body.
- **Performance**: frames with their build, raster and GC phases and a one-sentence reason for each
  slow frame; the CPU profile (bottom-up, top-down, flame chart); widget rebuild counts (turn on
  "Trace widget builds" in profile mode); and the raw timeline trace.
- **Timeline**: one zoomable page with frames, requests, logs, route changes, GC and janky-frame
  findings on the same time axis. Click anything to jump to the same moment in the other tools.
  Drag across the page to set the time range; double-click its ruler to clear it. The UI and
  raster threads themselves are on the Performance tool's Raw trace tab.
- **Finds your apps**: apps started with `flutter run` or `flutter attach` show up on the Connect
  page by themselves. So do apps you open on a booted iOS simulator or an Android device, without
  `flutter run`. Apps on a physical iPhone appear only when started with `flutter run` or
  `flutter attach` for now.

## Install

There are no prebuilt downloads yet; build it from source.

You need the Rust toolchain from [rustup](https://rustup.rs). The exact Rust version is pinned in
`rust-toolchain.toml` and installs itself on the first build. On Linux, install the build
dependencies first (the same list CI uses):

```
sudo apt-get install -y pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libvulkan-dev libx11-xcb-dev libxcb1-dev libfontconfig1-dev libfreetype-dev
```

Then:

```
cargo run --release -p fdt_app
```

On a Mac, `scripts/bundle-macos.sh --install` builds `fdt.app` with its icon and puts it in
`/Applications`. Quit fdt before re-running it. The app is signed ad hoc, so it runs on the machine
that built it.

## Command line

- `fdt --version` prints the version.
- `fdt --connect <vm-service-url>` opens a window and connects it.
- `fdt --startup-trace` opens the window, opens the command palette, prints one JSON line and
  quits. Times are milliseconds measured from the start of `main()`:
  `{"main_entry_unix_ms":…,"first_paint_ms":…,"interactive_ms":…,"phases":{…}}`.
  `main_entry_unix_ms` is the wall-clock time at `main()` entry, so a script can add the time
  between spawning the process and `main()`. It exits 1 if the palette did not open.
- `fdt bench-ui <scenario>` replays a scripted interaction over a stress session and prints draw
  time, frame interval and latency as JSON (see `docs/milestones/m3-review.md`).
- `FDT_LOG=info|debug|trace` raises the log level (the default shows warnings and errors on stderr).

Settings has a switch for finding apps opened on simulators and Android devices.

## Test app

`fixtures/jank_lab` is a Flutter app with deliberate jank, leaks, failing requests, deep widget
trees and log bursts, for trying the tools against something real.

## Test

```
cargo test --workspace
```

### Release budgets

The performance budget tests (`-- budget`, release mode only) always run and print what they
measured. They fail the build only when `FDT_BUDGETS=strict` is set, which is how to run them on
the reference machine:

```
FDT_BUDGETS=strict cargo test --workspace --release -- budget
```

Hosted CI runners are slower and noisier than that machine, so CI runs them report-only.

### Live integration test

`crates/fdt_capture/tests/jank_lab_live.rs` runs the real capture against a running `jank_lab`
(discovery, logs, network, hot restart). It is ignored unless `FDT_JANK_LAB=1`. With Flutter
installed and a desktop device available:

```
scripts/jank-lab-it.sh debug     # or: profile
```

The script starts `flutter run --machine` for the fixture, waits for the VM service, runs the
test with `FDT_JANK_LAB_VM_URI` set, and stops only the app it started. It prints three
timings: `attach_ms` (`connect()` to the first `Connected` status), `attach_to_data_ms`
(`connect()` to the first live log record in a snapshot) and `first_log_ms` (the `emit_levels`
call to its record in a snapshot). Hot restart is required in debug and skipped in profile.
In profile mode it also checks that slow frames are blamed on the right code. Those checks depend
on the machine, so they fail the run locally but only report on CI (`FDT_BUDGETS` overrides this).
Discovery uses your real registry directory and picks the app named `jank_lab`; other Dart
tooling on the machine is ignored. In CI this is the `jank_lab_live` job (Linux, under
`xvfb-run`).

## How it is built

The design, the reasons behind it and the milestone plan are in
[`docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md`](docs/superpowers/specs/2026-11-18-flutter-devtools-rs-design.md).
Each milestone has its own plan in `docs/superpowers/plans/` and a review record in
`docs/milestones/`. The UI uses GPUI, the UI framework of the [Zed](https://github.com/zed-industries/zed)
editor, through `gpui-kit`.

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first,
and report security problems as described in [SECURITY.md](SECURITY.md). Everyone taking part
follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Licence

Apache-2.0, see [LICENSE](LICENSE). Bundled fonts (Inter, JetBrains Mono) are under the SIL Open
Font License. The icons come from Lucide, under the ISC licence.

fdt is an independent project. It is not affiliated with or endorsed by Google or the Flutter
team. Flutter and the related logo are trademarks of Google LLC.
