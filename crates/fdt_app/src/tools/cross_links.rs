//! Tests for the links from Logs and Network details to frames.

use crate::actions::ShowLogs;
use crate::nav::{Location, Page, Selection, SessionTime, TimeRange, Tool};
use crate::shell::MainWindow;
use crate::test_support::{frame_at, log_at, open_shell_snapshot, request, snapshot};
use fdt_store::logs::LogLevel;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::sync::Arc;

const MS: u64 = 1_000_000;

/// Frames #0 at 100 ms (30 ms, Slow) and #1 at 500 ms (8 ms).
/// Logs: seq 0 at 110 ms (inside frame #0), seq 1 at 5 000 ms (no frame near).
/// Requests: seq 0 from 90 to 120 ms (in flight at log 0, answered before frame #1).
fn session() -> Arc<fdt_store::Snapshot> {
    snapshot(|w, run| {
        w.push_frame(frame_at(run, 0, 100, 30, 2));
        w.push_frame(frame_at(run, 1, 500, 8, 2));
        w.push_log(log_at(run, 110, LogLevel::Info));
        w.push_log(log_at(run, 5_000, LogLevel::Info));
        request(w, run, "GET", "http://a.com/x", Some(200), 90, Some(120));
    })
}

fn frame(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, cx| window.render_frame(cx));
    }
}

fn label(cx: &mut VisualTestContext, id: &str) -> Option<String> {
    frame(cx);
    let id = id.to_string();
    cx.update(|window, _| {
        window
            .try_find(id)
            .and_then(|e| e.label().map(str::to_string))
    })
}

fn present(cx: &mut VisualTestContext, id: &str) -> bool {
    frame(cx);
    let id = id.to_string();
    cx.update(|window, _| window.try_find(id).is_some())
}

fn click(cx: &mut VisualTestContext, id: &str) {
    frame(cx);
    let id = id.to_string();
    cx.update(|window, cx| window.click(id, cx));
    cx.run_until_parked();
    frame(cx);
}

fn go(shell: &Entity<MainWindow>, cx: &mut VisualTestContext, to: Location) {
    cx.update(|window, cx| shell.update(cx, |s, cx| s.navigate(to, window, cx)));
    cx.run_until_parked();
    frame(cx);
}

fn select_log(
    cx: &mut TestAppContext,
    snap: Arc<fdt_store::Snapshot>,
    seq: usize,
    range: Option<TimeRange>,
) -> (Entity<MainWindow>, &mut VisualTestContext) {
    let id = snap.logs.get_row(seq).unwrap().id;
    let (shell, cx) = open_shell_snapshot(cx, snap);
    cx.dispatch_action(ShowLogs);
    cx.run_until_parked();
    let to = Location {
        page: Page::Tool(Tool::Logs),
        selection: Some(Selection::Log(id)),
        range,
    };
    go(&shell, cx, to);
    let logs = shell.read_with(cx, |s, _| s.logs_view().clone());
    logs.update(cx, |v, cx| v.open_details(cx));
    // The first draw has the pane; the job has not run yet.
    frame(cx);
    (shell, cx)
}

fn select_request(
    cx: &mut TestAppContext,
    snap: Arc<fdt_store::Snapshot>,
    seq: usize,
    range: Option<TimeRange>,
) -> (Entity<MainWindow>, &mut VisualTestContext) {
    let id = snap.net.get(seq).unwrap().id;
    let (shell, cx) = open_shell_snapshot(cx, snap);
    let to = Location {
        page: Page::Tool(Tool::Network),
        selection: Some(Selection::Request(id)),
        range,
    };
    go(&shell, cx, to);
    let net = shell.read_with(cx, |s, _| s.network_view().clone());
    net.update(cx, |v, cx| v.open_details(cx));
    frame(cx);
    (shell, cx)
}

fn frame_location(snap: &fdt_store::Snapshot, seq: usize, range: Option<TimeRange>) -> Location {
    Location {
        page: Page::Tool(Tool::Performance),
        selection: Some(Selection::Frame(snap.frames.get(seq).unwrap().id)),
        range,
    }
}

#[gpui_kit::test]
fn log_details_link_to_the_frame(cx: &mut TestAppContext) {
    let snap = session();
    let (shell, cx) = select_log(cx, snap.clone(), 0, None);
    // The pane drew without waiting: the block fills in once the job has run.
    assert!(present(cx, "logs-details"));
    assert!(!present(cx, "logs-related-frame"));
    cx.run_until_parked();
    assert!(present(cx, "logs-related"));
    assert_eq!(
        label(cx, "logs-related-frame").as_deref(),
        Some("Frame #0 \u{b7} Slow")
    );
    assert_eq!(
        label(cx, "logs-related-request-0").as_deref(),
        Some("GET /x \u{b7} 200 \u{b7} 30 ms")
    );
    let back = shell.read_with(cx, |s, _| s.history().back_len());
    click(cx, "logs-related-frame");
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current(), &frame_location(&snap, 0, None));
        assert_eq!(s.history().back_len(), back + 1);
    });
}

#[gpui_kit::test]
fn log_row_time_opens_the_frame(cx: &mut TestAppContext) {
    let snap = session();
    let (shell, cx) = open_shell_snapshot(cx, snap.clone());
    cx.dispatch_action(ShowLogs);
    cx.run_until_parked();
    frame(cx);
    // Row 1 has no frame near it: its time is plain text.
    assert!(present(cx, "logs-row-time-0"));
    assert!(!present(cx, "logs-row-time-1"));
    let back = shell.read_with(cx, |s, _| s.history().back_len());
    click(cx, "logs-row-time-0");
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current(), &frame_location(&snap, 0, None));
        assert_eq!(s.history().back_len(), back + 1);
    });
    // The link only opens the frame: it did not also select the row.
    let logs = shell.read_with(cx, |s, _| s.logs_view().clone());
    assert_eq!(logs.read_with(cx, |v, _| v.selected()), None);
}

#[gpui_kit::test]
fn request_details_link_to_the_frame_after_the_response(cx: &mut TestAppContext) {
    let snap = session();
    let (shell, cx) = select_request(cx, snap.clone(), 0, None);
    assert!(present(cx, "net-details"));
    assert!(present(cx, "net-section-related"));
    assert!(!present(cx, "net-related-frame"));
    cx.run_until_parked();
    assert_eq!(
        label(cx, "net-related-frame").as_deref(),
        Some("Frame that processed the response: #1")
    );
    assert_eq!(
        label(cx, "net-related-log-0").as_deref(),
        Some("0:00.110 \u{b7} Info \u{b7} line at 110")
    );
    assert_eq!(
        label(cx, "net-related-logs-heading").as_deref(),
        Some("Log lines during this request")
    );
    let back = shell.read_with(cx, |s, _| s.history().back_len());
    click(cx, "net-related-frame");
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current(), &frame_location(&snap, 1, None));
        assert_eq!(s.history().back_len(), back + 1);
    });
}

#[gpui_kit::test]
fn links_keep_the_range_and_add_a_back_step(cx: &mut TestAppContext) {
    let snap = session();
    let range = TimeRange::new(SessionTime(0), SessionTime(1_000 * MS));
    let (shell, cx) = select_log(cx, snap.clone(), 0, Some(range));
    cx.run_until_parked();
    let back = shell.read_with(cx, |s, _| s.history().back_len());
    click(cx, "logs-related-frame");
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current(), &frame_location(&snap, 0, Some(range)));
        assert_eq!(s.history().back_len(), back + 1);
    });
    // Back returns to the log, its range intact.
    cx.dispatch_action(crate::actions::GoBack);
    cx.run_until_parked();
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current().page, Page::Tool(Tool::Logs));
        assert_eq!(s.current().range, Some(range));
    });
}

#[gpui_kit::test]
fn request_links_keep_the_range_too(cx: &mut TestAppContext) {
    let snap = session();
    let range = TimeRange::new(SessionTime(0), SessionTime(1_000 * MS));
    let (shell, cx) = select_request(cx, snap.clone(), 0, Some(range));
    cx.run_until_parked();
    click(cx, "net-related-frame");
    shell.read_with(cx, |s, _| {
        assert_eq!(s.current(), &frame_location(&snap, 1, Some(range)));
    });
}

#[gpui_kit::test]
fn no_frame_copy(cx: &mut TestAppContext) {
    let snap = session();
    let (_shell, cx) = select_log(cx, snap.clone(), 1, None);
    cx.run_until_parked();
    assert_eq!(
        label(cx, "logs-related-empty").as_deref(),
        Some("No frame at this moment.")
    );
    assert!(!present(cx, "logs-related-frame"));
}

#[gpui_kit::test]
fn no_frame_copy_for_a_request(cx: &mut TestAppContext) {
    // A request still in flight has no response to a frame.
    let pending = snapshot(|w, run| {
        w.push_frame(frame_at(run, 0, 100, 5, 2));
        request(w, run, "GET", "http://a.com/p", None, 90, None);
    });
    let (_shell, cx) = select_request(cx, pending, 0, None);
    cx.run_until_parked();
    assert_eq!(
        label(cx, "net-related-empty").as_deref(),
        Some("No frame processed the response.")
    );
}

#[gpui_kit::test]
fn a_frame_that_arrives_later_still_shows_up(cx: &mut TestAppContext) {
    let (shell, cx, fake) = crate::test_support::open_shell_fake(cx);
    let (mut w, run) = crate::test_support::writer();
    w.push_log(log_at(run, 110, LogLevel::Info));
    let first = w.publish();
    let id = first.logs.get_row(0).unwrap().id;
    fake.snaps.send(first).unwrap();
    fake.status
        .send(crate::test_support::connected("demo", true))
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(ShowLogs);
    cx.run_until_parked();
    let to = Location {
        page: Page::Tool(Tool::Logs),
        selection: Some(Selection::Log(id)),
        range: None,
    };
    go(&shell, cx, to);
    let logs = shell.read_with(cx, |s, _| s.logs_view().clone());
    logs.update(cx, |v, cx| v.open_details(cx));
    cx.run_until_parked();
    assert_eq!(
        label(cx, "logs-related-empty").as_deref(),
        Some("No frame at this moment.")
    );
    // The frame arrives after the line was selected; the next run is throttled, then runs.
    w.push_frame(frame_at(run, 0, 100, 15, 2));
    fake.snaps.send(w.publish()).unwrap();
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300)); // token-check: allow: test clock
    cx.run_until_parked();
    assert_eq!(
        label(cx, "logs-related-frame").as_deref(),
        Some("Frame #0 \u{b7} OK")
    );
    assert!(!present(cx, "logs-related-empty"));
}

#[gpui_kit::test]
fn related_result_for_a_previous_selection_is_dropped(cx: &mut TestAppContext) {
    let snap = session();
    let (shell, cx) = select_log(cx, snap.clone(), 0, None);
    // Log 0's job has not landed; log 1 is selected.
    let b = snap.logs.get_row(1).unwrap().id;
    let to = Location {
        page: Page::Tool(Tool::Logs),
        selection: Some(Selection::Log(b)),
        range: None,
    };
    go(&shell, cx, to);
    cx.run_until_parked();
    let logs = shell.read_with(cx, |s, _| s.logs_view().clone());
    assert_eq!(logs.read_with(cx, |v, _| v.related_runs()), 2);
    assert_eq!(
        label(cx, "logs-related-empty").as_deref(),
        Some("No frame at this moment.")
    );
    assert!(!present(cx, "logs-related-frame"));
    assert!(!present(cx, "logs-related-request-0"));
}

#[gpui_kit::test]
fn new_session_with_colliding_entry_id_does_not_show_old_related(cx: &mut TestAppContext) {
    let (shell, cx, fake) = crate::test_support::open_shell_fake(cx);
    let old = session();
    let id = old.logs.get_row(0).unwrap().id;
    let req = old.net.get(0).unwrap().id;
    fake.snaps.send(old.clone()).unwrap();
    fake.status
        .send(crate::test_support::connected("demo", true))
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(ShowLogs);
    cx.run_until_parked();
    let at = |page, selection| Location {
        page: Page::Tool(page),
        selection: Some(selection),
        range: None,
    };
    go(&shell, cx, at(Tool::Logs, Selection::Log(id)));
    let logs = shell.read_with(cx, |s, _| s.logs_view().clone());
    logs.update(cx, |v, cx| v.open_details(cx));
    cx.run_until_parked();
    assert!(present(cx, "logs-related-frame"));
    go(&shell, cx, at(Tool::Network, Selection::Request(req)));
    let net = shell.read_with(cx, |s, _| s.network_view().clone());
    net.update(cx, |v, cx| v.open_details(cx));
    cx.run_until_parked();
    assert!(present(cx, "net-related-frame"));
    // A new session whose record has the same id but no frames at all.
    let fresh = snapshot(|w, run| {
        w.push_log(log_at(run, 110, LogLevel::Info));
        request(w, run, "GET", "http://b.com/y", Some(200), 90, Some(120));
    });
    assert_eq!(fresh.logs.get_row(0).unwrap().id, id);
    assert_eq!(fresh.net.get(0).unwrap().id, req);
    fake.snaps.send(fresh).unwrap();
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300)); // token-check: allow: test clock
    cx.run_until_parked();
    // Select the colliding request again.
    go(&shell, cx, at(Tool::Network, Selection::Request(req)));
    net.update(cx, |v, cx| v.open_details(cx));
    cx.run_until_parked();
    assert!(!present(cx, "net-related-frame"));
    assert_eq!(
        label(cx, "net-related-empty").as_deref(),
        Some("No frame processed the response.")
    );
    go(&shell, cx, at(Tool::Logs, Selection::Log(id)));
    logs.update(cx, |v, cx| v.open_details(cx));
    cx.run_until_parked();
    assert!(!present(cx, "logs-related-frame"));
    assert!(
        label(cx, "logs-related-request-0").is_some_and(|t| t.contains("/y")),
        "only the new session's request shows"
    );
}
