#![allow(dead_code)]
use std::sync::Arc;
use std::time::Duration;

use fdt_capture::{Capture, ConnStatus};
use fdt_store::Snapshot;
use tokio::time::{Instant, sleep, timeout};

/// Waits until the status is `Connected` and no new generation has appeared for `quiet`.
pub async fn settle(cap: &Capture, quiet: Duration, limit: Duration) -> Arc<Snapshot> {
    let mut status = cap.status();
    let mut snaps = cap.snapshots();
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        timeout(
            left,
            status.wait_for(|s| matches!(s, ConnStatus::Connected { .. })),
        )
        .await
        .unwrap_or_else(|_| panic!("not connected: {:?}", *cap.status().borrow()))
        .unwrap();
        let left = deadline.saturating_duration_since(Instant::now());
        if timeout(quiet.min(left), snaps.changed()).await.is_err()
            && matches!(*status.borrow(), ConnStatus::Connected { .. })
        {
            return snaps.borrow().clone();
        }
        assert!(Instant::now() < deadline, "snapshots never settled");
        sleep(Duration::from_millis(1)).await;
    }
}

/// Waits until `pred` holds for a published snapshot.
pub async fn wait_for(
    cap: &Capture,
    limit: Duration,
    what: &str,
    pred: impl Fn(&Snapshot) -> bool,
) -> Arc<Snapshot> {
    let mut snaps = cap.snapshots();
    let deadline = Instant::now() + limit;
    loop {
        let s = snaps.borrow_and_update().clone();
        if pred(&s) {
            return s;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        let _ = timeout(left, snaps.changed()).await;
    }
}

/// Polls `cond` every few milliseconds until it holds.
pub async fn eventually(limit: Duration, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(5)).await;
    }
}
