# jank_lab

A small Flutter app with deliberate problems. `fdt` connects to it and the tests run against it. Every scenario is reachable from the home list, or directly with `--dart-define=SCENARIO=<key>`.

Run any scenario in profile mode (jank numbers are only meaningful there):

```bash
flutter run -d macos --profile --dart-define=SCENARIO=<key>
```

| Key | What it provokes | Command |
|---|---|---|
| `jank` | A widget that busy-waits 24 ms in `build` on every animation frame, plus a "Raster heavy" switch with 40 blurred, translucent layers. | `flutter run --profile --dart-define=SCENARIO=jank` |
| `rebuilds` | A cart screen that calls `setState` every 16 ms, so all 64 product tiles rebuild each tick. | `flutter run --profile --dart-define=SCENARIO=rebuilds` |
| `leaks` | A screen that leaves a listener on a global notifier, never disposes a text controller, and keeps a disposed scroll controller in a static list. | `flutter run --profile --dart-define=SCENARIO=leaks` |
| `network` | A loopback server and buttons for a JSON GET, a failing POST (500), a slow image, a 256 KiB download, a burst of 100 requests and a WebSocket. | `flutter run --profile --dart-define=SCENARIO=network` |
| `deep-tree` | 200 nested `Padding` widgets around a `Wrap` of about 25,000 tiny leaves. | `flutter run --profile --dart-define=SCENARIO=deep-tree` |
| `logs` | Leveled `developer.log` records, 1,000 `print` lines, a 30-line record, a reported Flutter error and a widget that throws in `build`. | `flutter run --profile --dart-define=SCENARIO=logs` |

The network server binds to `127.0.0.1` only. The macOS entitlements allow it (`network.server` and `network.client`).

## Tests

```bash
flutter analyze
flutter test
```
