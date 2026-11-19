//! Range-aware sidebar counts for Logs and Network, computed off the UI thread.

use crate::nav::{TimeRange, Tool};
use crate::shell::{MainWindow, ToolCount};
use fdt_store::Snapshot;
use fdt_store::logs::LogLevel;
use fdt_store::net_query::{NetFilter, NetTab, filter_requests};
use gpui_kit::{AppContext as _, Context};

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
        filter_requests(&snap.net, &all).len() as u64,
        !filter_requests(&snap.net, &errors).is_empty(),
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
            loop {
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

    #[test]
    fn range_counts_without_a_range_hides_zero() {
        let (logs, net) = range_counts(&Snapshot::empty(), None);
        assert!(logs.is_none() && net.is_none());
    }
}
