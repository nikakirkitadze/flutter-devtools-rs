#!/usr/bin/env bash
# Records a redacted VM service transcript of fixtures/jank_lab.
#
#   scripts/record-jank-lab.sh <debug|profile>
#
# Runs `flutter run --machine` for the fixture, reads the VM service URL from the daemon's
# `app.debugPort` event, runs the recorder against it, then stops only the app it started
# (daemon `app.stop`, then that process group on timeout). Output goes to
# fixtures/recordings/jank_lab-<mode>.jsonl.
set -euo pipefail

mode="${1:-}"
case "$mode" in
  debug) mode_flags=() ;;
  profile) mode_flags=(--profile) ;;
  *) echo "usage: $0 <debug|profile>" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/fixtures/recordings/jank_lab-$mode.jsonl"
case "$(uname -s)" in
  Darwin) device=macos ;;
  Linux) device=linux ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

work="$(mktemp -d)"
log="$work/daemon.log"
fifo="$work/stdin"
mkfifo "$fifo"
: >"$log"
mkdir -p "$(dirname "$out")"

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

# Wait (up to 10 minutes for a cold build) for the VM service URL.
uri=""
for _ in $(seq 1 1200); do
  uri="$(grep -o '"wsUri":"[^"]*"' "$log" | head -1 | cut -d'"' -f4 || true)"
  [ -n "$uri" ] && break
  kill -0 "$child" 2>/dev/null || { echo "flutter run exited early" >&2; tail -5 "$log" >&2; exit 1; }
  sleep 0.5
done
[ -n "$uri" ] || { echo "timed out waiting for the VM service" >&2; exit 1; }
sleep 2 # let the first frame and main() settle

cargo run -q -p fdt_vm --example record_jank_lab -- --uri "$uri" --out "$out"
echo "wrote $out"
