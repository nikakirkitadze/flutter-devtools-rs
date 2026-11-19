# fdt

A fast, native Flutter DevTools for macOS, Windows and Linux.

Status: early development.

## Build

On Linux, install the build dependencies first (the same list CI uses):

```
sudo apt-get install -y pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libvulkan-dev libx11-xcb-dev libxcb1-dev libfontconfig1-dev libfreetype-dev
```

Then:

```
cargo run -p fdt_app
```

## Command line

- `fdt --version` prints the version.
- `fdt --startup-trace` opens the window, opens the command palette, prints one JSON line and
  quits. Times are milliseconds measured from the start of `main()`:
  `{"main_entry_unix_ms":…,"first_paint_ms":…,"interactive_ms":…,"phases":{…}}`.
  `main_entry_unix_ms` is the wall-clock time at `main()` entry, so a script can add the time
  between spawning the process and `main()`. It exits 1 if the palette did not open.
- `FDT_LOG=info|debug|trace` raises the log level (the default shows warnings and errors on stderr).

## Test app

`fixtures/jank_lab` is a Flutter app with deliberate jank, leaks, failing requests, deep widget
trees and log bursts, for trying the tools against something real.

## Test

```
cargo test --workspace
```

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
call to its record in a snapshot). Hot restart is required in debug and skipped in profile. Discovery uses your real registry directory and picks the app named
`jank_lab`; other Dart tooling on the machine is ignored. In CI this is the `jank_lab_live` job
(Linux, under `xvfb-run`).

## Licence

Apache-2.0. Bundled fonts (Inter, JetBrains Mono) are under the SIL Open Font License. The
icons come from Lucide, under the ISC licence.
