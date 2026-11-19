//! Range-aware sidebar counts for Logs and Network, computed off the UI thread.

use crate::nav::{TimeRange, Tool};
use crate::shell::{MainWindow, ToolCount};
use fdt_store::Snapshot;
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

/// The Logs and Network counts of `snap` within `range`. O(records); not for the UI thread.
///
/// With no range, a count of zero is left off; with a range, zero is shown.
pub fn range_counts(
    snap: &Snapshot,
    range: Option<TimeRange>,
) -> (Option<ToolCount>, Option<ToolCount>) {
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
    (logs, net)
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
                    .background_spawn(async move { range_counts(&snap, key.range) })
                    .await;
                done = Some(key);
                let applied = this.update(cx, |this, cx| {
                    if this.counts_wanted == Some(key) {
                        let mut all = this.counts;
                        all[Tool::Logs.index()] = counts.0;
                        all[Tool::Network.index()] = counts.1;
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
        let (_, net) = range_counts(&snap, range);
        assert_eq!(
            net,
            Some(ToolCount {
                value: 1,
                attention: true
            })
        );
        let (_, net) = range_counts(&snap, None);
        assert_eq!(
            net,
            Some(ToolCount {
                value: 3,
                attention: true
            })
        );
        let ok_only = Some(TimeRange::new(SessionTime(SEC), SessionTime(3 * SEC)));
        let (_, net) = range_counts(&snap, ok_only);
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
        let (logs, net) = range_counts(&Snapshot::empty(), None);
        assert!(logs.is_none() && net.is_none());
    }
}
