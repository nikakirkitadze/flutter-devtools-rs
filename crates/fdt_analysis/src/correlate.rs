//! Which frames, requests and log lines happened together.

use fdt_store::Snapshot;
use fdt_store::frames::FrameRecord;
use fdt_store::log_query::{LogFilter, filter_logs};
use fdt_store::net_query::{NetFilter, filter_requests};
use fdt_store::time::{SessionTime, TimeRange};

pub const RELATED_LIMIT: usize = 20;

const AFTER_LOG_NS: u64 = 100_000_000;
const AFTER_RESPONSE_NS: u64 = 1_000_000_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Related {
    pub frames: Vec<usize>,
    pub requests: Vec<u32>,
    pub logs: Vec<u32>,
}

/// `[start, start + max(elapsed, budget)]`.
pub fn frame_window(frame: &FrameRecord, budget_us: u32) -> TimeRange {
    let us = u64::from(frame.elapsed_us.max(budget_us));
    TimeRange::new(
        frame.start,
        SessionTime(frame.start.0.saturating_add(us * 1_000)),
    )
}

fn net_in(snap: &Snapshot, range: TimeRange) -> Vec<u32> {
    filter_requests(
        &snap.net,
        &NetFilter {
            range: Some(range),
            ..NetFilter::default()
        },
    )
}

fn logs_in(snap: &Snapshot, range: TimeRange) -> Vec<u32> {
    filter_logs(
        &snap.logs,
        &LogFilter {
            range: Some(range),
            ..LogFilter::default()
        },
        0,
    )
}

/// At most `RELATED_LIMIT` items, those whose time is nearest `anchor`, in ascending order.
fn nearest(mut items: Vec<(u64, u32)>, anchor: SessionTime) -> Vec<u32> {
    if items.len() > RELATED_LIMIT {
        items.sort_by_key(|&(t, seq)| (t.abs_diff(anchor.0), seq));
        items.truncate(RELATED_LIMIT);
    }
    items.sort_by_key(|&(t, seq)| (t, seq));
    items.into_iter().map(|(_, seq)| seq).collect()
}

fn request_times(snap: &Snapshot, seqs: Vec<u32>) -> Vec<(u64, u32)> {
    seqs.into_iter()
        .filter_map(|s| Some((snap.net.get(s as usize)?.start.0, s)))
        .collect()
}

fn log_times(snap: &Snapshot, seqs: Vec<u32>) -> Vec<(u64, u32)> {
    seqs.into_iter()
        .filter_map(|s| Some((snap.logs.get_row(s as usize)?.time.0, s)))
        .collect()
}

/// The requests overlapping the frame's window and the log lines inside it.
pub fn related_to_frame(snap: &Snapshot, frame_seq: usize) -> Related {
    let Some(frame) = snap.frames.get(frame_seq) else {
        return Related::default();
    };
    let window = frame_window(frame, snap.frame_budget_us(frame.run));
    Related {
        frames: Vec::new(),
        requests: nearest(request_times(snap, net_in(snap, window)), frame.start),
        logs: nearest(log_times(snap, logs_in(snap, window)), frame.start),
    }
}

/// The frame at the line's time, else the first one starting within 100 ms after it, and the
/// requests in flight at that instant.
pub fn related_to_log(snap: &Snapshot, log_seq: usize) -> Related {
    let Some(row) = snap.logs.get_row(log_seq) else {
        return Related::default();
    };
    let t = row.time;
    let frame = snap.frames.at(t).or_else(|| {
        let i = snap.frames.first_after(t)?;
        let f = snap.frames.get(i)?;
        (f.start.0 - t.0 <= AFTER_LOG_NS).then_some(i)
    });
    Related {
        frames: frame.into_iter().collect(),
        requests: nearest(request_times(snap, net_in(snap, TimeRange::new(t, t))), t),
        logs: Vec::new(),
    }
}

/// The first frame starting at or after the response end, within 1 s (the frame that processed
/// the response), and the log lines inside the request's span.
pub fn related_to_request(snap: &Snapshot, req_seq: usize) -> Related {
    let Some(req) = snap.net.get(req_seq) else {
        return Related::default();
    };
    let frames = req
        .end
        .and_then(|end| {
            let i = snap
                .frames
                .first_after(SessionTime(end.0.saturating_sub(1)))?;
            let f = snap.frames.get(i)?;
            (f.start.0 - end.0 <= AFTER_RESPONSE_NS).then_some(i)
        })
        .into_iter()
        .collect();
    let end = req.end.unwrap_or_else(|| snap.net.latest()).max(req.start);
    Related {
        frames,
        requests: Vec::new(),
        logs: nearest(
            log_times(snap, logs_in(snap, TimeRange::new(req.start, end))),
            end,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{MS, frame, http, writer};
    use fdt_store::logs::{LogInput, LogLevel, LogSource};
    use fdt_store::time::SessionTime;

    fn log(w: &mut fdt_store::SessionWriter, run: fdt_store::time::RunId, ms: u64) {
        w.push_log(LogInput {
            run,
            time: SessionTime(ms * MS),
            clamped: false,
            level: LogLevel::Info,
            source: LogSource::Log,
            logger: String::new(),
            text: format!("line at {ms}"),
            error: None,
        });
    }

    #[test]
    fn related_windows_and_limits() {
        let (mut w, run) = writer();
        // Frame 0: 100 ms .. 140 ms (elapsed 40 ms > the 16.667 ms budget).
        frame(&mut w, run, 1, 100, 40_000);
        frame(&mut w, run, 2, 500, 5_000);
        for ms in [90, 99] {
            log(&mut w, run, ms);
        }
        for ms in [100, 120, 140] {
            log(&mut w, run, ms);
        }
        log(&mut w, run, 141);
        w.upsert_request(http(run, "a", 50, Some(99)));
        w.upsert_request(http(run, "b", 60, Some(110)));
        w.upsert_request(http(run, "c", 130, Some(200)));
        w.upsert_request(http(run, "d", 141, Some(200)));
        let s = w.publish();
        let win = frame_window(s.frames.get(0).unwrap(), 16_667);
        assert_eq!(win.start, SessionTime(100 * MS));
        assert_eq!(win.end, SessionTime(140 * MS));
        // A fast frame is held to the budget.
        let win2 = frame_window(s.frames.get(1).unwrap(), 16_667);
        assert_eq!(win2.end, SessionTime(500 * MS + 16_667_000));
        let r = related_to_frame(&s, 0);
        assert_eq!(r.logs, vec![2, 3, 4]);
        assert_eq!(r.requests, vec![1, 2]);

        // Limits: many lines inside the window keep those nearest the frame start.
        let (mut w, run) = writer();
        frame(&mut w, run, 1, 1000, 100_000);
        for i in 0..60 {
            log(&mut w, run, 1000 + i);
        }
        let s = w.publish();
        let r = related_to_frame(&s, 0);
        assert_eq!(r.logs.len(), RELATED_LIMIT);
        assert_eq!(r.logs, (0..20).collect::<Vec<u32>>());
    }

    #[test]
    fn log_links_to_the_frame_at_its_time_or_just_after() {
        let (mut w, run) = writer();
        frame(&mut w, run, 1, 100, 20_000);
        frame(&mut w, run, 2, 300, 20_000);
        log(&mut w, run, 110); // 0: inside frame 0
        log(&mut w, run, 250); // 1: 50 ms before frame 1
        log(&mut w, run, 150); // 2: after frame 0 ended, far from frame 1
        w.upsert_request(http(run, "a", 100, Some(200)));
        w.upsert_request(http(run, "b", 120, Some(130)));
        let s = w.publish();
        let r = related_to_log(&s, 0);
        assert_eq!(r.frames, vec![0]);
        assert_eq!(r.requests, vec![0]);
        let r = related_to_log(&s, 1);
        assert_eq!(r.frames, vec![1]);
        assert!(r.requests.is_empty());
        let r = related_to_log(&s, 2);
        assert!(r.frames.is_empty(), "150 ms is 150 ms before frame 1");
        assert_eq!(r.requests, vec![0]);
    }

    #[test]
    fn request_links_to_the_frame_after_its_response() {
        let (mut w, run) = writer();
        frame(&mut w, run, 1, 100, 10_000);
        frame(&mut w, run, 2, 300, 10_000);
        frame(&mut w, run, 3, 2000, 10_000);
        w.upsert_request(http(run, "a", 50, Some(250))); // 0 -> frame 1 (300)
        w.upsert_request(http(run, "b", 50, Some(300))); // 1 -> frame 1 (at the end)
        w.upsert_request(http(run, "c", 500, Some(900))); // 2 -> nothing within 1 s
        w.upsert_request(http(run, "d", 1000, None)); // 3 -> unfinished
        log(&mut w, run, 60);
        log(&mut w, run, 240);
        let s = w.publish();
        let r = related_to_request(&s, 0);
        assert_eq!(r.frames, vec![1]);
        assert_eq!(r.logs, vec![0, 1]);
        assert_eq!(related_to_request(&s, 1).frames, vec![1]);
        assert!(related_to_request(&s, 2).frames.is_empty());
        assert!(related_to_request(&s, 3).frames.is_empty());
    }
}
