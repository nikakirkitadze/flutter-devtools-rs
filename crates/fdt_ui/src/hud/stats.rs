//! Frame statistics over bounded ring buffers. Pure: no GPUI.

use std::collections::VecDeque;
use std::time::Duration;

/// Nearest-rank percentiles of a sample set.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
}

/// Recent draw durations and frame intervals, plus a running dropped-frame total.
#[derive(Debug)]
pub struct FrameStats {
    capacity: usize,
    draws: VecDeque<Duration>,
    intervals: VecDeque<Duration>,
    dropped: u64,
}

/// Whole refreshes skipped by an interval: 0 when it is at most 1.5 refreshes,
/// otherwise `round(interval / refresh) - 1`.
pub fn missed_refreshes(interval: Duration, refresh: Duration) -> u64 {
    if refresh.is_zero() || interval.as_secs_f64() <= 1.5 * refresh.as_secs_f64() {
        return 0;
    }
    let ratio = (interval.as_secs_f64() / refresh.as_secs_f64()).round() as u64;
    ratio.saturating_sub(1)
}

/// Milliseconds with one decimal, e.g. `1.25 ms` becomes `"1.3"`.
pub fn format_ms(d: Duration) -> String {
    // Round half up on the microsecond count to avoid binary float artefacts.
    let tenths = (d.as_micros() + 50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
}

fn push_bounded(buf: &mut VecDeque<Duration>, capacity: usize, value: Duration) {
    if capacity == 0 {
        return;
    }
    while buf.len() >= capacity {
        buf.pop_front();
    }
    buf.push_back(value);
}

fn percentiles(buf: &VecDeque<Duration>) -> Option<Percentiles> {
    if buf.is_empty() {
        return None;
    }
    let mut sorted: Vec<Duration> = buf.iter().copied().collect();
    sorted.sort_unstable();
    let n = sorted.len();
    let at = |p: usize| sorted[(p * n).div_ceil(100).max(1) - 1];
    Some(Percentiles {
        p50: at(50),
        p95: at(95),
        p99: at(99),
    })
}

impl FrameStats {
    pub const DEFAULT_CAPACITY: usize = 600;

    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            draws: VecDeque::new(),
            intervals: VecDeque::new(),
            dropped: 0,
        }
    }

    pub fn record_draw(&mut self, draw: Duration) {
        push_bounded(&mut self.draws, self.capacity, draw);
    }

    /// Records a frame interval and adds its missed refreshes to the dropped total.
    pub fn record_interval(&mut self, interval: Duration, refresh: Duration) {
        push_bounded(&mut self.intervals, self.capacity, interval);
        self.dropped += missed_refreshes(interval, refresh);
    }

    pub fn draw_percentiles(&self) -> Option<Percentiles> {
        percentiles(&self.draws)
    }

    pub fn interval_percentiles(&self) -> Option<Percentiles> {
        percentiles(&self.intervals)
    }

    /// Dropped frames since the last reset; ring eviction does not reduce it.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Draw durations, oldest to newest.
    pub fn recent_draws(&self) -> impl Iterator<Item = Duration> + '_ {
        self.draws.iter().copied()
    }

    pub fn reset(&mut self) {
        self.draws.clear();
        self.intervals.clear();
        self.dropped = 0;
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: f64) -> Duration {
        Duration::from_secs_f64(v / 1000.0)
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut s = FrameStats::new(200);
        for i in 1..=100 {
            s.record_draw(Duration::from_millis(i));
        }
        let p = s.draw_percentiles().unwrap();
        assert_eq!(p.p50, Duration::from_millis(50));
        assert_eq!(p.p95, Duration::from_millis(95));
        assert_eq!(p.p99, Duration::from_millis(99));

        let mut one = FrameStats::new(10);
        one.record_draw(Duration::from_millis(7));
        let p = one.draw_percentiles().unwrap();
        assert_eq!((p.p50, p.p95, p.p99), (ms(7.0), ms(7.0), ms(7.0)));
    }

    #[test]
    fn empty_stats_have_no_percentiles() {
        let s = FrameStats::new(10);
        assert_eq!(s.draw_percentiles(), None);
        assert_eq!(s.interval_percentiles(), None);
    }

    #[test]
    fn missed_refreshes_rounds_to_whole_frames() {
        let r = ms(8.333);
        assert_eq!(missed_refreshes(ms(8.3), r), 0);
        assert_eq!(missed_refreshes(ms(12.0), r), 0);
        assert_eq!(missed_refreshes(ms(16.7), r), 1);
        assert_eq!(missed_refreshes(ms(25.0), r), 2);
        assert_eq!(missed_refreshes(ms(100.0), r), 11);
    }

    #[test]
    fn capacity_bounds_samples_but_not_dropped_total() {
        let mut s = FrameStats::new(600);
        for _ in 0..700 {
            s.record_draw(ms(1.0));
            s.record_interval(ms(25.0), ms(8.333));
        }
        assert_eq!(s.recent_draws().count(), 600);
        assert_eq!(s.intervals.len(), 600);
        assert_eq!(s.dropped_frames(), 1400);
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = FrameStats::new(10);
        s.record_draw(ms(1.0));
        s.record_interval(ms(25.0), ms(8.333));
        s.reset();
        assert_eq!(s.recent_draws().count(), 0);
        assert_eq!(s.draw_percentiles(), None);
        assert_eq!(s.interval_percentiles(), None);
        assert_eq!(s.dropped_frames(), 0);
    }

    #[test]
    fn format_ms_uses_one_decimal() {
        assert_eq!(format_ms(ms(1.25)), "1.3");
        assert_eq!(format_ms(ms(16.667)), "16.7");
    }
}
