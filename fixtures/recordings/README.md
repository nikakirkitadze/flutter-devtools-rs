# Recorded VM service transcripts

Real traffic between a VM service client and the `fixtures/jank_lab` app, used by tests through
`fdt_vm::testing::ReplayServer`.

| File | Mode |
|---|---|
| `jank_lab-debug.jsonl` | `flutter run -d macos` (debug) |
| `jank_lab-profile.jsonl` | `flutter run -d macos --profile` |

Recorded with Flutter 3.47.9 and Dart 3.13.7 on macOS (arm64).

## Format

One JSON object per line: `{"t_us": <µs since start>, "dir": "send" | "recv", "msg": <JSON-RPC frame>}`.
`send` is a request from the recorder, `recv` is a response or a `streamNotify` notification.

## Redaction

Every frame is redacted by `fdt_vm::tap::redact` before it is written. Auth tokens in URL paths
become `<token>` and the home directory becomes `~`. `crates/fdt_vm/tests/recordings_privacy.rs`
checks that no file here contains a user home-directory path (macOS, Linux or Windows) or a token-like path segment.
Do not edit the files by hand; extend `redact` and re-record instead.

## Re-recording

```sh
scripts/record-jank-lab.sh debug
scripts/record-jank-lab.sh profile
```

The script starts `flutter run --machine` for the fixture, reads the VM service URL from the
daemon's `app.debugPort` event, runs `crates/fdt_vm/examples/record_jank_lab.rs` against it, and
stops only the app it started. The recorder drives the app through the `ext.jank_lab.run` service
extension (log levels, a 1,000-line print burst, a multi-line record, a reported error, a throwing
widget in debug, then HTTP requests, a refused connection, a slow request and a WebSocket).

Re-recording changes ids, timestamps, ports and the process id, so tests that use these files
must not depend on them.
