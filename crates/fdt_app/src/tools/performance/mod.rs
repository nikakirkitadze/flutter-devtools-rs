//! The Performance tool: frame timings, CPU profile, rebuilds and the raw trace.

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
