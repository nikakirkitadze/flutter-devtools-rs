//! The Performance tool: frame timings, CPU profile, rebuilds and the raw trace.

pub mod cpu_tab;
mod details;
pub mod flame;
pub mod frame_chart;
pub mod frames_table;
pub mod rebuilds_tab;
pub mod trace_tab;
pub mod view;

pub use view::{PerfTab, PerformanceView};

use fdt_store::Snapshot;
use std::ops::Range;

/// The seq of the slow or janky frame after (`forward`) or before `from`, or the first or last
/// one with no `from`. Only frames whose seq is in `within` count (all frames with `None`).
/// Binary searches the slow list, so it is cheap on the UI thread.
pub fn adjacent_slow(
    snap: &Snapshot,
    from: Option<usize>,
    within: Option<Range<usize>>,
    forward: bool,
) -> Option<usize> {
    let slow = snap.frames.slow();
    let window = within.unwrap_or(0..snap.frames.len());
    // The number of slow frames with a seq below `bound`.
    let below = |bound: usize| {
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
    if forward {
        let start = from.map_or(window.start, |f| (f + 1).max(window.start));
        let seq = *slow.get(below(start))? as usize;
        (seq < window.end).then_some(seq)
    } else {
        let end = from.map_or(window.end, |f| f.min(window.end));
        let seq = *slow.get(below(end).checked_sub(1)?)? as usize;
        (seq >= window.start).then_some(seq)
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::test_support::{frame_at, snapshot};
    use std::sync::Arc;

    /// `n` frames, 20 ms apart. Every 7th (from 3) is slow at 20 ms and every 25th (from 10) is
    /// janky at 40 ms; the rest take 8 ms. Frame numbers equal their seq.
    pub fn frames(n: usize) -> Arc<fdt_store::Snapshot> {
        snapshot(|w, run| push_frames(w, run, 0, n))
    }

    /// Frames `from..to` of the same pattern, into `w`.
    pub fn push_frames(
        w: &mut fdt_store::SessionWriter,
        run: fdt_store::time::RunId,
        from: usize,
        to: usize,
    ) {
        for i in from..to {
            let ms = if i % 25 == 10 {
                40
            } else if i % 7 == 3 {
                20
            } else {
                8
            };
            w.push_frame(frame_at(run, i as u64, 100 + i as u64 * 20, ms, 1));
        }
    }
}
