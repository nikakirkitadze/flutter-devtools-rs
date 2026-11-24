//! Range-aware sidebar counts for Logs, Network, Performance and Memory, computed off the UI thread.

use crate::nav::{TimeRange, Tool};
use crate::shell::{MainWindow, ToolCount};
use fdt_store::Snapshot;
use fdt_store::frames::{FrameStatus, frame_status};
use fdt_store::heap::{HeapRecord, HeapStage};
use fdt_store::logs::LogLevel;
use fdt_store::net_query::{NetFilter, NetTab, count_requests};
use gpui_kit::{AppContext as _, Context};
use std::time::Duration;

/// The shortest time between the starts of two count jobs.
pub(super) const MIN_INTERVAL: Duration = Duration::from_millis(250); // token-check: allow: throttle, not a visual duration

/// What a count job was started for: the snapshot generation and the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CountsKey {
    generation: u64,
    range: Option<TimeRange>,
}

/// The Logs, Network and Performance counts of `snap` within `range`. O(records); not for the UI
/// thread.
///
/// Performance counts the slow and janky frames, and wants attention when any is janky.
/// With no range, a count of zero is left off; with a range, zero is shown.
pub fn range_counts(
    snap: &Snapshot,
    range: Option<TimeRange>,
) -> (Option<ToolCount>, Option<ToolCount>, Option<ToolCount>) {
    let shown = |value: u64, attention: bool| {
        (value > 0 || range.is_some()).then_some(ToolCount { value, attention })
    };
    let levels = snap.logs.level_counts(range);
    let logs = shown(levels.iter().sum(), levels[LogLevel::Error as usize] > 0);
    let all = NetFilter {
        range,
        ..NetFilter::default()
    };
    let errors = NetFilter {
        tab: NetTab::Errors,
        ..all.clone()
    };
    let net = shown(
        count_requests(&snap.net, &all) as u64,
        count_requests(&snap.net, &errors) > 0,
    );
    let (slow, janky) = slow_frames(snap, range);
    (logs, net, shown(slow, janky))
}

/// How many slow and janky frames start in `range` (all of them with no range), and whether any
/// is janky. Reads only the slow frames, not every frame.
fn slow_frames(snap: &Snapshot, range: Option<TimeRange>) -> (u64, bool) {
    let frames = &snap.frames;
    let slow = frames.slow();
    let window = range.map_or(0..frames.len(), |r| frames.range(r));
    // The slow list is ascending, so the frames in `window` are one run of it.
    let first = |bound: usize| {
        let (mut lo, mut hi) = (0, slow.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if slow.get(mid).is_some_and(|&s| (s as usize) < bound) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let (lo, hi) = (first(window.start), first(window.end));
    let janky = (lo..hi).any(|i| {
        slow.get(i)
            .and_then(|&seq| frames.get(seq as usize))
            .is_some_and(|f| {
                frame_status(f.time_us(), snap.frame_budget_us(f.run)) == FrameStatus::Janky
            })
    });
    ((hi - lo) as u64, janky)
}

/// The Memory count: the leak suspects of the newest analysed heap snapshot taken in `range`, or
/// of the newest one overall when none was. Wants attention when there are any. With no range, a
/// count of zero is left off; with a range, zero is shown. Reads the records, not the heaps.
pub fn memory_count(snap: &Snapshot, range: Option<TimeRange>) -> Option<ToolCount> {
    let at = |h: &&HeapRecord| h.time.unwrap_or(h.requested);
    let mut ready = snap
        .memory
        .heaps()
        .iter()
        .filter(|h| h.stage == HeapStage::Ready);
    // The records are in the order they were taken.
    let newest = ready.clone().next_back();
    let record = range
        .and_then(|r| ready.rfind(|h| r.contains(at(h))))
        .or(newest)?;
    let value = u64::from(record.suspects());
    (value > 0 || range.is_some()).then_some(ToolCount {
        value,
        attention: value > 0,
    })
}

impl MainWindow {
    /// Recomputes the counts when the snapshot or range changed. Runs at most one job at a time;
    /// inputs that arrive meanwhile are picked up when it finishes.
    pub(super) fn refresh_counts(&mut self, cx: &mut Context<Self>) {
        let key = CountsKey {
            generation: self.session.read(cx).snapshot().generation,
            range: self.current().range,
        };
        if self.counts_wanted == Some(key) {
            return;
        }
        self.counts_wanted = Some(key);
        if self.counts_running {
            return;
        }
        self.counts_running = true;
        self.counts_task = Some(cx.spawn(async move |this, cx| {
            let mut done: Option<CountsKey> = None;
            let mut started: Option<std::time::Instant> = None;
            loop {
                // Wait out the interval first, so inputs that arrive meanwhile are coalesced.
                if let Some(at) = started {
                    let now = cx.background_executor().now();
                    let since = now.saturating_duration_since(at);
                    if since < MIN_INTERVAL {
                        cx.background_executor().timer(MIN_INTERVAL - since).await;
                    }
                }
                let next = this.update(cx, |this, cx| match this.counts_wanted {
                    Some(wanted) if Some(wanted) != done => {
                        Some((wanted, this.session.read(cx).snapshot().clone()))
                    }
                    _ => {
                        this.counts_running = false;
                        None
                    }
                });
                let Ok(Some((key, snap))) = next else { break };
                started = Some(cx.background_executor().now());
                let counts = cx
                    .background_spawn(async move {
                        let (logs, net, perf) = range_counts(&snap, key.range);
                        (logs, net, perf, memory_count(&snap, key.range))
                    })
                    .await;
                done = Some(key);
                let applied = this.update(cx, |this, cx| {
                    if this.counts_wanted == Some(key) {
                        let mut all = this.counts;
                        all[Tool::Logs.index()] = counts.0;
                        all[Tool::Network.index()] = counts.1;
                        all[Tool::Performance.index()] = counts.2;
                        all[Tool::Memory.index()] = counts.3;
                        this.set_counts(all, cx);
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Location, Page, SessionTime};
    use crate::test_support::{log_at, open_shell_snapshot, snapshot};

    const SEC: u64 = 1_000_000_000;

    #[gpui_kit::test]
    fn counts_follow_the_range(cx: &mut gpui_kit::TestAppContext) {
        let snap = snapshot(|w, run| {
            for ms in 0..10u64 {
                let level = if ms == 5 {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                };
                w.push_log(log_at(run, ms * 1_000, level));
            }
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.run_until_parked();
        let all = shell.read_with(cx, |s, _| s.counts()[Tool::Logs.index()]);
        assert_eq!(
            all,
            Some(ToolCount {
                value: 10,
                attention: true
            })
        );
        let mut loc = Location::at(Page::Tool(Tool::Logs));
        loc.range = Some(TimeRange::new(SessionTime(4 * SEC), SessionTime(6 * SEC)));
        shell.update_in(cx, |s, w, cx| s.navigate(loc.clone(), w, cx));
        cx.run_until_parked();
        // Throttled: the old counts stay until the interval passes.
        let early = shell.read_with(cx, |s, _| s.counts()[Tool::Logs.index()]);
        assert_eq!(early.map(|c| c.value), Some(10));
        cx.executor().advance_clock(MIN_INTERVAL);
        cx.run_until_parked();
        let logs = shell.read_with(cx, |s, _| s.counts()[Tool::Logs.index()]);
        assert_eq!(
            logs,
            Some(ToolCount {
                value: 3,
                attention: true
            })
        );
        // A range with no errors drops the attention.
        loc.range = Some(TimeRange::new(SessionTime(0), SessionTime(2 * SEC)));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(MIN_INTERVAL);
        cx.run_until_parked();
        let logs = shell.read_with(cx, |s, _| s.counts()[Tool::Logs.index()]);
        assert_eq!(
            logs,
            Some(ToolCount {
                value: 3,
                attention: false
            })
        );
        assert!(crate::test_support::exists(cx, "sidebar-count-logs"));
    }

    fn request(
        w: &mut fdt_store::SessionWriter,
        run: fdt_store::time::RunId,
        secs: u64,
        status: u16,
    ) {
        w.upsert_request(fdt_store::net::HttpUpdate {
            run,
            isolate: "i".into(),
            vm_id: secs.to_string(),
            method: "GET".into(),
            url: format!("https://example.com/{secs}"),
            start: SessionTime(secs * SEC),
            start_clamped: false,
            request_sent: None,
            response_start: None,
            end: Some(SessionTime(secs * SEC + 1_000_000)),
            status: Some(status),
            reason: None,
            request_headers: vec![],
            response_headers: vec![],
            request_content_length: None,
            response_content_length: None,
            events: vec![],
            error: None,
            websocket: false,
            remote: None,
        });
    }

    #[test]
    fn network_count_is_range_aware_with_attention() {
        let snap = snapshot(|w, run| {
            request(w, run, 2, 200);
            request(w, run, 5, 500);
            request(w, run, 9, 404);
        });
        let range = Some(TimeRange::new(SessionTime(4 * SEC), SessionTime(6 * SEC)));
        let (_, net, _) = range_counts(&snap, range);
        assert_eq!(
            net,
            Some(ToolCount {
                value: 1,
                attention: true
            })
        );
        let (_, net, _) = range_counts(&snap, None);
        assert_eq!(
            net,
            Some(ToolCount {
                value: 3,
                attention: true
            })
        );
        let ok_only = Some(TimeRange::new(SessionTime(SEC), SessionTime(3 * SEC)));
        let (_, net, _) = range_counts(&snap, ok_only);
        assert_eq!(
            net,
            Some(ToolCount {
                value: 1,
                attention: false
            })
        );
    }

    #[test]
    fn range_counts_without_a_range_hides_zero() {
        let (logs, net, perf) = range_counts(&Snapshot::empty(), None);
        assert!(logs.is_none() && net.is_none() && perf.is_none());
    }

    /// Frames at 60 Hz: 10 ms (ok), 20 ms (slow), 40 ms (janky), 25 ms (slow), one a second.
    fn frames_snapshot() -> std::sync::Arc<Snapshot> {
        snapshot(|w, run| {
            for (i, ms) in [10u64, 20, 40, 25].into_iter().enumerate() {
                let start = (i as u64 + 1) * 1_000;
                w.push_frame(crate::test_support::frame_at(run, i as u64, start, ms, 1));
            }
        })
    }

    #[test]
    fn performance_count_counts_slow_frames_in_range() {
        let snap = frames_snapshot();
        let (_, _, all) = range_counts(&snap, None);
        assert_eq!(
            all,
            Some(ToolCount {
                value: 3,
                attention: true
            })
        );
        // Only the 20 ms frame: slow, not janky.
        let slow_only = Some(TimeRange::new(
            SessionTime(1_500 * 1_000_000),
            SessionTime(2_500 * 1_000_000),
        ));
        let (_, _, one) = range_counts(&snap, slow_only);
        assert_eq!(
            one,
            Some(ToolCount {
                value: 1,
                attention: false
            })
        );
        // A range with only the on-time frame shows zero, with no attention.
        let ok_only = Some(TimeRange::new(
            SessionTime(500 * 1_000_000),
            SessionTime(1_500 * 1_000_000),
        ));
        let (_, _, none) = range_counts(&snap, ok_only);
        assert_eq!(
            none,
            Some(ToolCount {
                value: 0,
                attention: false
            })
        );
        // With no range, no slow frames means no count at all.
        let calm = snapshot(|w, run| {
            w.push_frame(crate::test_support::frame_at(run, 0, 1_000, 5, 5));
        });
        assert_eq!(range_counts(&calm, None).2, None);
    }

    #[gpui_kit::test]
    fn the_sidebar_shows_the_performance_count(cx: &mut gpui_kit::TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, frames_snapshot());
        cx.run_until_parked();
        let perf = shell.read_with(cx, |s, _| s.counts()[Tool::Performance.index()]);
        assert_eq!(perf.map(|c| c.value), Some(3));
        assert!(crate::test_support::exists(cx, "sidebar-count-performance"));
    }

    fn leaky(b: &mut fdt_analysis::heap::encode::SnapshotBuilder) {
        let class = b.class("LeakyState", "package:app/leaky.dart", &[]);
        let o = b.object(class, 32, fdt_analysis::heap::format::Value::None, &[]);
        b.static_field("leaky", o);
    }

    fn suspects(n: u32) -> impl FnOnce(&mut fdt_store::heap::HeapSummary) {
        move |s| s.findings_total = n
    }

    #[test]
    fn memory_count_uses_the_latest_snapshot_in_range() {
        use crate::test_support::heap_record_with;
        // Snapshots at 1 s, 2 s and 3 s with 4, 0 and 2 suspects.
        let snap = snapshot(|w, run| {
            for n in [4, 0, 2] {
                heap_record_with(w, run, leaky, suspects(n));
            }
        });
        let at = |from: u64, to: u64| {
            Some(TimeRange::new(
                SessionTime(from * SEC),
                SessionTime(to * SEC),
            ))
        };
        // No range: the latest overall.
        assert_eq!(
            memory_count(&snap, None),
            Some(ToolCount {
                value: 2,
                attention: true
            })
        );
        // The latest one inside the range, whichever is newest overall.
        assert_eq!(
            memory_count(&snap, at(0, 1)),
            Some(ToolCount {
                value: 4,
                attention: true
            })
        );
        // A range with several: the newest of them. Zero is shown with a range, without
        // attention.
        assert_eq!(
            memory_count(&snap, at(1, 2)),
            Some(ToolCount {
                value: 0,
                attention: false
            })
        );
        // No snapshot in the range: the latest overall.
        assert_eq!(memory_count(&snap, at(10, 20)).map(|c| c.value), Some(2));
        // Nothing to count: no snapshot, or none analysed yet.
        assert_eq!(memory_count(&Snapshot::empty(), None), None);
        let pending = snapshot(|w, run| {
            w.heap_requested(run, "isolates/1", SessionTime(SEC));
        });
        assert_eq!(memory_count(&pending, at(0, 5)), None);
        // Zero without a range is left off, like the other tools.
        let clean = snapshot(|w, run| {
            heap_record_with(w, run, leaky, suspects(0));
        });
        assert_eq!(memory_count(&clean, None), None);
    }

    #[gpui_kit::test]
    fn the_sidebar_shows_the_memory_count(cx: &mut gpui_kit::TestAppContext) {
        use crate::test_support::heap_record_with;
        let snap = snapshot(|w, run| {
            heap_record_with(w, run, leaky, suspects(3));
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.run_until_parked();
        let count = shell.read_with(cx, |s, _| s.counts()[Tool::Memory.index()]);
        assert_eq!(
            count,
            Some(ToolCount {
                value: 3,
                attention: true
            })
        );
        assert!(crate::test_support::exists(cx, "sidebar-count-memory"));
    }
}
