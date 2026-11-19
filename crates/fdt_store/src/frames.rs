//! Frame records and a level-of-detail pyramid for the session strip.
//!
//! Level k summarises time in buckets 2^(24 + k) ns wide, so the strip can
//! draw one bar per pixel over any range by reading O(width) buckets.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::time::{EntryId, RunId, SessionTime, TimeRange};
use std::sync::Arc;

/// Level-0 buckets are 2^24 ns (about 16.8 ms) wide.
pub const BASE_SHIFT: u32 = 24;
/// Number of levels; level k buckets are 2^(24 + k) ns wide.
pub const LEVELS: usize = 20;
/// Buckets per chunk in every level.
pub const LOD_CHUNK: usize = 1024;
/// A level never grows past this many buckets (about 10 h at level 0), so a
/// wild timestamp cannot allocate without bound. Later frames are still
/// stored; they are only left out of the levels that cannot hold them.
pub const MAX_LEVEL_BUCKETS: usize = 1 << 21;
/// Frames per chunk.
pub const FRAME_CHUNK: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameInput {
    pub run: RunId,
    pub number: u64,
    pub start: SessionTime,
    pub elapsed_us: u32,
    pub build_us: u32,
    pub raster_us: u32,
    pub vsync_overhead_us: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameRecord {
    pub id: EntryId,
    pub run: RunId,
    pub number: u64,
    pub start: SessionTime,
    pub elapsed_us: u32,
    pub build_us: u32,
    pub raster_us: u32,
    pub vsync_overhead_us: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    pub max_us: u32,
    pub count: u32,
}

/// The writer side of the pyramid.
pub struct LodSeries {
    levels: Vec<Chunked<Bucket>>,
    cap: usize,
}

impl Default for LodSeries {
    fn default() -> Self {
        Self::new()
    }
}

impl LodSeries {
    pub fn new() -> Self {
        Self::with_cap(MAX_LEVEL_BUCKETS)
    }

    /// A series whose levels stop growing at `cap` buckets.
    pub(crate) fn with_cap(cap: usize) -> Self {
        Self {
            levels: (0..LEVELS)
                .map(|_| Chunked::with_chunk(LOD_CHUNK))
                .collect(),
            cap,
        }
    }

    /// Adds one value to every level. O(LEVELS); accepts any time order.
    pub fn insert(&mut self, t: SessionTime, value_us: u32) {
        for (k, level) in self.levels.iter_mut().enumerate() {
            let idx = (t.0 >> (BASE_SHIFT + k as u32)) as usize;
            if idx >= self.cap {
                continue;
            }
            while level.len() <= idx {
                level.push(Bucket::default());
            }
            let b = level.get_mut(idx).expect("bucket exists");
            b.max_us = b.max_us.max(value_us);
            b.count += 1;
        }
    }

    pub fn level(&self, k: usize) -> Option<&Chunked<Bucket>> {
        self.levels.get(k)
    }

    pub fn snapshot(&mut self) -> LodSnapshot {
        LodSnapshot {
            levels: self.levels.iter_mut().map(|l| l.snapshot()).collect(),
            cap: self.cap,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LodSnapshot {
    levels: Arc<[ChunkedSnapshot<Bucket>]>,
    cap: usize,
}

impl Default for LodSnapshot {
    fn default() -> Self {
        LodSeries::new().snapshot()
    }
}

impl LodSnapshot {
    pub fn level(&self, k: usize) -> Option<&ChunkedSnapshot<Bucket>> {
        self.levels.get(k)
    }

    pub fn levels(&self) -> usize {
        self.levels.len()
    }
}

pub(crate) struct FrameStore {
    frames: Chunked<FrameRecord>,
    lod: LodSeries,
}

impl FrameStore {
    pub(crate) fn new(chunk: usize) -> Self {
        Self {
            frames: Chunked::with_chunk(chunk),
            lod: LodSeries::new(),
        }
    }

    pub(crate) fn push(&mut self, f: FrameInput) -> EntryId {
        let id = EntryId {
            run: f.run,
            seq: self.frames.len() as u64,
        };
        self.lod.insert(f.start, f.elapsed_us);
        self.frames.push(FrameRecord {
            id,
            run: f.run,
            number: f.number,
            start: f.start,
            elapsed_us: f.elapsed_us,
            build_us: f.build_us,
            raster_us: f.raster_us,
            vsync_overhead_us: f.vsync_overhead_us,
        });
        id
    }

    pub(crate) fn snapshot(&mut self) -> FramesSnapshot {
        FramesSnapshot {
            frames: self.frames.snapshot(),
            lod: self.lod.snapshot(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FramesSnapshot {
    frames: ChunkedSnapshot<FrameRecord>,
    lod: LodSnapshot,
}

impl FramesSnapshot {
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The frame with this sequence number (its `EntryId.seq`).
    pub fn get(&self, i: usize) -> Option<&FrameRecord> {
        self.frames.get(i)
    }

    pub fn lod(&self) -> &LodSnapshot {
        &self.lod
    }
}

/// One bar of the strip: the worst frame in a bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripBar {
    /// The bucket's start, clipped to the requested range.
    pub start: SessionTime,
    /// The bucket's exclusive end, clipped to the requested range.
    pub end: SessionTime,
    pub max_elapsed_us: u32,
    pub frames: u32,
}

/// Bars for `range` at the smallest level whose bucket count over the range
/// is at most `width_px`. Non-empty buckets only, in time order; reads at most
/// `width_px` buckets (the bound is `2 * width_px + 2`).
///
/// Edge buckets may include frames just outside `range`, since a bucket is
/// counted whole. A level that stopped growing at the bucket cap cannot hold
/// the range's end, so the next coarser level that can is used instead.
pub fn strip_bars(frames: &FramesSnapshot, range: TimeRange, width_px: usize) -> Vec<StripBar> {
    if width_px == 0 {
        return Vec::new();
    }
    let span = |k: usize| {
        let sh = BASE_SHIFT + k as u32;
        (range.start.0 >> sh, range.end.0 >> sh)
    };
    let k = (0..LEVELS)
        .find(|&k| {
            let (lo, hi) = span(k);
            hi - lo < width_px as u64 && hi < frames.lod.cap as u64
        })
        .unwrap_or(LEVELS - 1);
    let Some(level) = frames.lod.level(k) else {
        return Vec::new();
    };
    let sh = BASE_SHIFT + k as u32;
    let (lo, hi) = span(k);
    let hi = hi.min((level.len() as u64).saturating_sub(1));
    let mut bars = Vec::new();
    if level.is_empty() || lo > hi {
        return bars;
    }
    let mut i = lo;
    while i <= hi {
        let b = level.get(i as usize).expect("within level length");
        if b.count > 0 {
            bars.push(StripBar {
                start: SessionTime((i << sh).max(range.start.0)),
                end: SessionTime(((i + 1) << sh).min(range.end.0)),
                max_elapsed_us: b.max_us,
                frames: b.count,
            });
        }
        i += 1;
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::*;
    use crate::synth;

    const SPAN: u64 = 1 << BASE_SHIFT;

    fn input(run: RunId, n: u64, start: u64, elapsed: u32) -> FrameInput {
        FrameInput {
            run,
            number: n,
            start: SessionTime(start),
            elapsed_us: elapsed,
            build_us: elapsed / 2,
            raster_us: elapsed / 2,
            vsync_overhead_us: 0,
        }
    }

    fn total(l: &ChunkedSnapshot<Bucket>) -> u32 {
        l.chunks().flatten().map(|b| b.count).sum()
    }

    #[test]
    fn a_frame_lands_in_one_bucket_per_level() {
        let mut s = LodSeries::new();
        let t = SessionTime(5 * SPAN + 123);
        s.insert(t, 9_000);
        let snap = s.snapshot();
        assert_eq!(snap.levels(), LEVELS);
        for k in 0..LEVELS {
            let l = snap.level(k).unwrap();
            let idx = (t.0 >> (BASE_SHIFT + k as u32)) as usize;
            assert_eq!(l.len(), idx + 1, "level {k} grows densely to the bucket");
            assert_eq!(total(l), 1, "level {k}");
            assert_eq!(
                l.get(idx),
                Some(&Bucket {
                    max_us: 9_000,
                    count: 1
                })
            );
        }
    }

    #[test]
    fn bucket_keeps_the_maximum() {
        let mut s = LodSeries::new();
        for v in [4_000, 20_000, 7_000] {
            s.insert(SessionTime(10), v);
        }
        let snap = s.snapshot();
        assert_eq!(
            snap.level(0).unwrap().get(0),
            Some(&Bucket {
                max_us: 20_000,
                count: 3
            })
        );
    }

    #[test]
    fn out_of_order_inserts_are_counted() {
        let mut s = LodSeries::new();
        for t in [9 * SPAN, 2 * SPAN, 9 * SPAN + 5, 0, 2 * SPAN + 1] {
            s.insert(SessionTime(t), 1);
        }
        let snap = s.snapshot();
        let l0 = snap.level(0).unwrap();
        assert_eq!(l0.len(), 10);
        assert_eq!(total(l0), 5);
        assert_eq!(l0.get(9).unwrap().count, 2);
        assert_eq!(l0.get(2).unwrap().count, 2);
        assert_eq!(l0.get(5).unwrap().count, 0);
        assert_eq!(total(snap.level(LEVELS - 1).unwrap()), 5);
    }

    #[test]
    fn frames_publish_in_snapshots_and_move_latest_time() {
        let (mut w, run) = writer(4);
        let a = w.push_frame(input(run, 1, 7_000_000, 5_000));
        let b = w.push_frame(input(run, 2, 9_000_000, 20_000));
        assert_eq!((a.seq, b.seq), (0, 1));
        let s = w.publish();
        assert_eq!(s.frames.len(), 2);
        assert_eq!(s.frames.get(1).unwrap().elapsed_us, 20_000);
        assert_eq!(s.frames.get(1).unwrap().id, b);
        assert!(s.frames.get(2).is_none());
        assert_eq!(s.latest_time(), SessionTime(9_000_000 + 20_000 * 1_000));
        let later = {
            w.push_frame(input(run, 3, 12_000_000, 1_000));
            w.publish()
        };
        assert_eq!(s.frames.len(), 2, "old snapshot unchanged");
        assert_eq!(later.frames.len(), 3);
    }

    fn dataset(n: usize) -> std::sync::Arc<crate::Snapshot> {
        let (mut w, run) = writer(crate::logs::LOG_CHUNK);
        synth::frames(&mut w, run, n, 7, 120);
        w.publish()
    }

    fn whole(s: &crate::Snapshot) -> TimeRange {
        TimeRange::new(SessionTime(0), s.latest_time())
    }

    #[test]
    fn strip_bars_fit_the_width() {
        let s = dataset(72_000);
        let bars = strip_bars(&s.frames, whole(&s), 1_000);
        assert!(!bars.is_empty() && bars.len() <= 1_000, "{}", bars.len());
        assert!(bars.windows(2).all(|w| w[0].end <= w[1].start));
        assert_eq!(bars.iter().map(|b| b.frames).sum::<u32>(), 72_000);
        assert!(bars.iter().all(|b| b.start <= b.end));
        let worst = (0..s.frames.len())
            .map(|i| s.frames.get(i).unwrap().elapsed_us)
            .max()
            .unwrap();
        assert_eq!(bars.iter().map(|b| b.max_elapsed_us).max(), Some(worst));
        assert!(strip_bars(&s.frames, whole(&s), 0).is_empty());
    }

    #[test]
    fn strip_bars_for_a_sub_range() {
        let s = dataset(72_000);
        let range = TimeRange::new(SessionTime(100_000_000_000), SessionTime(110_000_000_000));
        let bars = strip_bars(&s.frames, range, 500);
        assert!(!bars.is_empty() && bars.len() <= 500);
        assert!(
            bars.iter()
                .all(|b| b.start >= range.start && b.end <= range.end)
        );
        assert!(bars.windows(2).all(|w| w[0].end <= w[1].start));
        let inside: Vec<_> = (0..s.frames.len())
            .map(|i| s.frames.get(i).unwrap())
            .filter(|f| range.contains(f.start))
            .collect();
        let n: u32 = bars.iter().map(|b| b.frames).sum();
        // Level 1 buckets are 2 * SPAN wide, so edge buckets may add a few.
        assert!(n as usize >= inside.len());
        assert!((n as usize) <= inside.len() + 2 * 2 * 3, "{n}");
        let worst = inside.iter().map(|f| f.elapsed_us).max().unwrap();
        assert!(bars.iter().map(|b| b.max_elapsed_us).max().unwrap() >= worst);
        // A range with nothing in it, and one past the data.
        let past = TimeRange::new(
            SessionTime(10_000_000_000_000),
            SessionTime(10_001_000_000_000),
        );
        assert!(strip_bars(&s.frames, past, 100).is_empty());
        assert!(strip_bars(&crate::Snapshot::empty().frames, range, 100).is_empty());
    }

    #[test]
    fn zooming_past_the_level_cap_still_shows_frames() {
        let mut lod = LodSeries::with_cap(8);
        let base = 100 * SPAN;
        for i in 0..8 {
            lod.insert(SessionTime(base + i * SPAN / 4), 1_000 + i as u32);
        }
        let frames = FramesSnapshot {
            frames: ChunkedSnapshot::default(),
            lod: lod.snapshot(),
        };
        assert!(frames.lod.level(0).unwrap().len() <= 8, "level 0 is capped");
        let range = TimeRange::new(SessionTime(base), SessionTime(base + SPAN));
        let bars = strip_bars(&frames, range, 4);
        assert!(!bars.is_empty() && bars.len() <= 4);
        assert_eq!(bars.iter().map(|b| b.frames).sum::<u32>(), 8);
        assert_eq!(bars.iter().map(|b| b.max_elapsed_us).max(), Some(1_007));
    }

    #[test]
    fn synthetic_frames_follow_the_documented_mix() {
        let s = dataset(20_000);
        let budget = 1_000_000 / 120;
        let (mut slow, mut janky) = (0, 0);
        for i in 0..s.frames.len() {
            let e = s.frames.get(i).unwrap().elapsed_us;
            if e > 2 * budget {
                janky += 1;
            } else if e >= budget {
                slow += 1;
            }
        }
        assert!((400..800).contains(&slow), "{slow}");
        assert!((100..300).contains(&janky), "{janky}");
        let f1 = s.frames.get(1).unwrap();
        assert_eq!(f1.start, SessionTime(1_000_000_000 / 120));
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn strip_bars_budget_72k() {
        let _g = serial();
        let s = dataset(72_000);
        let range = whole(&s);
        let mut ms = Vec::new();
        for _ in 0..50 {
            let t = std::time::Instant::now();
            let bars = strip_bars(&s.frames, range, 1_000);
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(bars);
        }
        let (p90, worst) = p90_worst(ms);
        println!("strip_bars 72k frames, width 1000: p90 {p90:.4} ms, worst {worst:.4} ms");
        crate::budget::check(p90 <= 1.0, || format!("p90 {p90} ms"));
    }
}
