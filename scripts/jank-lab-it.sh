#!/usr/bin/env bash
# Live integration test: runs the real capture against fixtures/jank_lab.
#
#   scripts/jank-lab-it.sh [debug|profile]
#
# Starts `flutter run --machine` for the fixture in its own process group, reads the VM service
# URL from the daemon's `app.debugPort` event, runs crates/fdt_capture/tests/jank_lab_live.rs,
# then stops only the app it started (daemon `app.stop`, then that process group on timeout).
# Exits with the test's status.
set -euo pipefail

mode="${1:-debug}"
case "$mode" in
  debug) mode_flags=() ;;
  profile) mode_flags=(--profile) ;;
  *) echo "usage: $0 [debug|profile]" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
# Keep the macOS window above others: a covered window draws no frames.
export JANK_LAB_FLOAT=1
case "$(uname -s)" in
  Darwin) device=macos ;;
  Linux) device=linux ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

(cd "$root/fixtures/jank_lab" && flutter pub get)

work="$(mktemp -d)"
log="$work/daemon.log"
fifo="$work/stdin"
mkfifo "$fifo"
: >"$log"

# Own process group for the flutter child, so a timeout kill touches only what we started.
set -m
(cd "$root/fixtures/jank_lab" && exec flutter run -d "$device" --machine ${mode_flags[@]+"${mode_flags[@]}"} <"$fifo" >"$log" 2>&1) &
child=$!
set +m
exec 3<>"$fifo"

stopped=0
cleanup() {
  if [ "$stopped" = 0 ]; then
    stopped=1
    app_id="$(grep -o '"appId":"[^"]*"' "$log" | head -1 | cut -d'"' -f4 || true)"
    if [ -n "${app_id:-}" ]; then
      printf '[{"method":"app.stop","id":99,"params":{"appId":"%s"}}]\n' "$app_id" >&3 2>/dev/null || true
      printf '[{"method":"daemon.shutdown","id":100}]\n' >&3 2>/dev/null || true
    fi
    for _ in $(seq 1 60); do
      kill -0 "$child" 2>/dev/null || break
      sleep 0.5
    done
    if kill -0 "$child" 2>/dev/null; then
      kill -TERM -- "-$child" 2>/dev/null || true
      sleep 2
      kill -KILL -- "-$child" 2>/dev/null || true
    fi
  fi
  exec 3>&- 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Wait for the VM service URL. This allows 10 minutes rather than 60 s because a cold CI build
# (first Linux desktop build) is slow; a warm local run takes seconds.
uri=""
for _ in $(seq 1 1200); do
  uri="$(grep -o '"wsUri":"[^"]*"' "$log" | head -1 | cut -d'"' -f4 || true)"
  [ -n "$uri" ] && break
  kill -0 "$child" 2>/dev/null || { echo "flutter run exited early" >&2; tail -5 "$log" >&2; exit 1; }
  sleep 0.5
done
[ -n "$uri" ] || { echo "timed out waiting for the VM service" >&2; exit 1; }
sleep 2 # let the first frame and main() settle

export FDT_JANK_LAB=1
export FDT_JANK_LAB_VM_URI="$uri"
cd "$root"
status=0
cargo test -p fdt_capture --test jank_lab_live -- --ignored --nocapture || status=$?
exit "$status"
