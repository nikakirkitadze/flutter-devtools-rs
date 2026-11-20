//! Level-of-detail summaries for the Timeline page's log and request lanes.
//!
//! The log lane's series holds `insert(time, level_index)` per record, with
//! `Debug=0 .. Error=3`, so a cell's `max_us` is the worst level in it. The request lane's
//! series holds `insert(start, 0)` per request and `raise(start, 1)` for a failed one, so a
//! cell with `max_us == 1` contains an error.

pub use crate::frames::lod_cells;
use crate::frames::{LodSnapshot, StripBar};
use crate::time::TimeRange;

/// Cells for a log or request lane over `range`: at most about `2 * width_px + 2`, however
/// many records there are. See [`lod_cells`].
pub fn lane_cells(lod: &LodSnapshot, range: TimeRange, width_px: usize) -> Vec<StripBar> {
    lod_cells(lod, range, width_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::{input, writer};
    use crate::logs::{LogLevel, LogSource};
    use crate::net::HttpUpdate;
    use crate::time::SessionTime;

    const MS: u64 = 1_000_000;

    fn all() -> TimeRange {
        TimeRange::new(SessionTime(0), SessionTime(10_000 * MS))
    }

    #[test]
    fn log_lod_keeps_the_worst_level() {
        let (mut w, run) = writer(4);
        for (ms, lv) in [
            (1, LogLevel::Info),
            (2, LogLevel::Error),
            (3, LogLevel::Debug),
            (9_000, LogLevel::Warning),
        ] {
            w.push_log(input(run, ms, lv, "", "x"));
        }
        let s = w.publish();
        let cells = lane_cells(s.logs.lod(), all(), 2);
        assert_eq!(cells.iter().map(|c| c.count).sum::<u32>(), 4);
        assert_eq!(cells.first().unwrap().max_us, 3);
        assert_eq!(cells.last().unwrap().max_us, 2);
    }

    #[test]
    fn navigation_index_lists_route_changes() {
        let (mut w, run) = writer(2);
        for (i, src) in [
            LogSource::Log,
            LogSource::Navigation,
            LogSource::Log,
            LogSource::Navigation,
            LogSource::Navigation,
        ]
        .into_iter()
        .enumerate()
        {
            let mut l = input(run, i as u64, LogLevel::Info, "", "x");
            l.source = src;
            w.push_log(l);
        }
        let s = w.publish();
        let nav = s.logs.navigation();
        let seqs: Vec<u32> = (0..nav.len()).map(|i| *nav.get(i).unwrap()).collect();
        assert_eq!(seqs, vec![1, 3, 4]);
    }

    fn http(run: crate::time::RunId, id: &str, start_ms: u64) -> HttpUpdate {
        HttpUpdate {
            run,
            isolate: "isolates/1".into(),
            vm_id: id.into(),
            method: "GET".into(),
            url: "http://h/x".into(),
            start: SessionTime(start_ms * MS),
            start_clamped: false,
            request_sent: None,
            response_start: None,
            end: None,
            status: None,
            reason: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            request_content_length: None,
            response_content_length: None,
            events: Vec::new(),
            error: None,
            websocket: false,
            remote: None,
        }
    }

    #[test]
    fn request_lod_marks_errors_without_double_counting() {
        let (mut w, run) = writer(4);
        for _ in 0..3 {
            w.upsert_request(http(run, "1", 10));
        }
        let mut u = http(run, "1", 10);
        u.status = Some(500);
        u.end = Some(SessionTime(20 * MS));
        w.upsert_request(u);
        w.upsert_request(http(run, "2", 9_000));
        let s = w.publish();
        let cells = lane_cells(s.net.lod(), all(), 2);
        assert_eq!(cells.iter().map(|c| c.count).sum::<u32>(), 2);
        assert_eq!((cells[0].count, cells[0].max_us), (1, 1));
        assert_eq!((cells[1].count, cells[1].max_us), (1, 0));
    }

    #[test]
    fn aborted_request_marks_an_error() {
        let (mut w, run) = writer(4);
        let id = w.upsert_request(http(run, "1", 10)).id;
        w.abort_request(id, SessionTime(30 * MS), "gone");
        let s = w.publish();
        let cells = lane_cells(s.net.lod(), all(), 2);
        assert_eq!((cells[0].count, cells[0].max_us), (1, 1));
    }

    #[test]
    fn lane_cells_fit_the_width() {
        let s = crate::synth::stress_session(7);
        let end = s.latest_time().max(SessionTime(1_000 * MS));
        let range = TimeRange::new(SessionTime(0), end);
        for width in [1, 100, 1_000, 2_000] {
            for lod in [s.logs.lod(), s.net.lod()] {
                let cells = lane_cells(lod, range, width);
                assert!(!cells.is_empty());
                assert!(
                    cells.len() <= 2 * width + 2,
                    "{} cells at {width}",
                    cells.len()
                );
            }
        }
        let n: u32 = lane_cells(s.logs.lod(), range, 2_000)
            .iter()
            .map(|c| c.count)
            .sum();
        assert_eq!(n as usize, s.logs.len());
        let n: u32 = lane_cells(s.net.lod(), range, 2_000)
            .iter()
            .map(|c| c.count)
            .sum();
        assert_eq!(n as usize, s.net.len());
    }
}
