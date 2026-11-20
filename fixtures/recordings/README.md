# Recorded VM service transcripts

Real traffic between a VM service client and the `fixtures/jank_lab` app, used by tests through
`fdt_vm::testing::ReplayServer`.

| File | Mode |
|---|---|
| `jank_lab-debug.jsonl` | `flutter run -d macos` (debug) |
| `jank_lab-profile.jsonl` | `flutter run -d macos --profile` |
| `jank_lab-perf.jsonl` | `flutter run -d macos --profile`, driven by `record_jank_lab --mode perf` |

Recorded with Flutter 3.47.9 and Dart 3.13.7 on macOS (arm64).

## Format

One JSON object per line: `{"t_us": <µs since start>, "dir": "send" | "recv", "msg": <JSON-RPC frame>}`.
`send` is a request from the recorder, `recv` is a response or a `streamNotify` notification.

## Redaction

Every frame is redacted by `fdt_vm::tap::redact` before it is written. Auth tokens in URL paths
become `<token>` and the home directory becomes `~`. Base64 strings of 1 KB or more under the
keys `samples` and `trace` (Perfetto CPU samples) are decoded, every user directory under the macOS and Linux home roots (for example `file:` URLs of the Dart SDK packages)
in them gets its user name replaced by the same number of `x`, and they are re-encoded.
The length is unchanged, so the protobuf lengths stay valid. `crates/fdt_vm/tests/recordings_privacy.rs`
checks that no file here contains a user home-directory path (macOS, Linux or Windows) or a token-like path segment, and decodes every such base64 blob to check it too.
Do not edit the files by hand; extend `redact` and re-record instead.

## Re-recording

```sh
scripts/record-jank-lab.sh debug
scripts/record-jank-lab.sh profile
scripts/record-jank-lab.sh perf
```

The script starts `flutter run --machine` for the fixture, reads the VM service URL from the
daemon's `app.debugPort` event, runs `crates/fdt_vm/examples/record_jank_lab.rs` against it, and
stops only the app it started. The recorder drives the app through the `ext.jank_lab.run` service
extension (log levels, a 1,000-line print burst, a multi-line record, a reported error, a throwing
widget in debug, then HTTP requests, a refused connection, a slow request and a WebSocket).

Re-recording changes ids, timestamps, ports and the process id, so tests that use these files
must not depend on them.

### The perf recording

`perf` runs a profile build with `JANK_LAB_FLOAT=1` (the script exports it), so the macOS window
floats above others: a covered window draws no frames. The session is: `getVMTimelineFlags`, the
display refresh rate, `Extension` and `Isolate` streams; `goto_jank`, two `getVMTimeline` polls of
about 500 ms and `getPerfettoCpuSamples` for that window; `raster_on`, two polls, `raster_off`;
`home`, `goto_rebuilds`, `profileWidgetBuilds` on, a 120 ms `Timeline` stream window, then
`streamCancel` and `profileWidgetBuilds` off. The stream window is short because with widget
builds on the app pushes about 400 k events a second (about 12 KB per block of 64). The recorder
is built in release mode: in a debug build redaction cannot keep up with that rate.

`ReplayServer` answers `getVMTimeline` and `getPerfettoCpuSamples` with an empty timeline or an
empty sample set once the recorded answers run out; it never repeats the last one.
