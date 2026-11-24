# Heap snapshot fixture

`jank_lab-profile-after-leaks.dartheap` is a real Dart heap snapshot (format `dartheap`, see
`crates/fdt_analysis/src/heap/format.rs`) of `fixtures/jank_lab`.

- **App state:** profile mode on macOS, taken after opening the `/leaks` screen and leaving it,
  so it holds the screen's unmounted `_LeakyScreenState`, its undisposed `TextEditingController`
  and the listener that `AppEvents` still references.
- **Flutter version:** 3.47.9 (Dart 3.13.7), macOS arm64.
- **How it was made:** `scripts/record-jank-lab.sh heap`. It runs the app, drives `goto_leaks`,
  `pump_frames`, `home` and `pump_frames` through `ext.jank_lab.run`, subscribes to the
  `HeapSnapshot` stream and calls `requestHeapSnapshot`.
- **Redacted:** the recorder runs `fdt_vm::tap::mask_snapshot_secrets` before writing. User
  names in `/Users/<name>/` and `/home/<name>/` become `x`, and the VM-service auth token
  becomes dots, in both the 1-byte and the UTF-16LE form. Every replacement keeps the length,
  so the file still parses, and the encoder still rebuilds it byte for byte.

Re-record only when the format or the fixture app changes, and check the result with
`cargo test -p fdt_analysis --test heap_fixture`, which also scans the file for secrets.
