//! Frame records and a level-of-detail pyramid for the session strip.
//!
//! Level k summarises time in buckets 2^(24 + k) ns wide, so the strip can
//! draw one bar per pixel over any range by reading O(width) buckets.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::time::{EntryId, RunId, SessionTime, TimeRange};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
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
/// The budget when the refresh rate is unknown (60 Hz).
pub const DEFAULT_BUDGET_US: u32 = 16_667;
/// Phases for frames that have not arrived yet are parked up to this many; the oldest go first.
pub const MAX_PENDING_PHASES: usize = 4_096;

/// The frame budget for a refresh rate: `round(1e6 / hz)`, or the default when `hz` is
/// unknown, not finite or below 1.
pub fn budget_us(refresh_hz: Option<f32>) -> u32 {
    match refresh_hz {
        Some(hz) if hz.is_finite() && hz >= 1.0 => (1e6 / f64::from(hz)).round() as u32,
        _ => DEFAULT_BUDGET_US,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Ok,
    Slow,
    Janky,
}

/// `Ok` within the budget, `Slow` within twice it, `Janky` beyond.
pub fn frame_status(time_us: u32, budget_us: u32) -> FrameStatus {
    if time_us <= budget_us {
        FrameStatus::Ok
    } else if u64::from(time_us) <= 2 * u64::from(budget_us) {
        FrameStatus::Slow
    } else {
        FrameStatus::Janky
    }
}

/// The UI thread's part of a frame, from the timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiPhases {
    pub begin: SessionTime,
    pub ui_us: u32,
    pub build_us: u32,
    pub layout_us: u32,
    pub paint_us: u32,
    pub composite_us: u32,
    pub finalize_us: u32,
    pub widgets_built: Option<u32>,
}

/// The raster thread's part of a frame, from the timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterPhase {
    pub start: SessionTime,
    pub raster_us: u32,
}

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
    pub ui: Option<UiPhases>,
    pub raster: Option<RasterPhase>,
}

impl FrameRecord {
    /// What counts against the budget: the slower of the two threads.
    pub fn time_us(&self) -> u32 {
        self.build_us.max(self.raster_us)
    }

    pub fn end(&self) -> SessionTime {
        SessionTime(
            self.start
                .0
                .saturating_add(u64::from(self.elapsed_us) * 1_000),
        )
    }
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
    chunk: usize,
    frames: Chunked<FrameRecord>,
    lod: LodSeries,
    slow: Chunked<u32>,
    last_start: HashMap<RunId, SessionTime>,
    by_number: HashMap<(RunId, u64), usize>,
    budgets: HashMap<RunId, u32>,
    pending: HashMap<(RunId, u64), (Option<UiPhases>, Option<RasterPhase>)>,
    pending_order: VecDeque<(RunId, u64)>,
}

impl FrameStore {
    pub(crate) fn new(chunk: usize) -> Self {
        Self {
            chunk,
            frames: Chunked::with_chunk(chunk),
            lod: LodSeries::new(),
            slow: Chunked::with_chunk(chunk),
            last_start: HashMap::new(),
            by_number: HashMap::new(),
            budgets: HashMap::new(),
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
        }
    }

    fn budget(&self, run: RunId) -> u32 {
        self.budgets.get(&run).copied().unwrap_or(DEFAULT_BUDGET_US)
    }

    /// Stores a frame unless its start is not after the run's last stored frame.
    pub(crate) fn push(&mut self, f: FrameInput) -> Option<EntryId> {
        if self.last_start.get(&f.run).is_some_and(|&l| f.start <= l) {
            return None;
        }
        let seq = self.frames.len();
        let id = EntryId {
            run: f.run,
            seq: seq as u64,
        };
        let (ui, raster) = self.take_pending(f.run, f.number);
        let rec = FrameRecord {
            id,
            run: f.run,
            number: f.number,
            start: f.start,
            elapsed_us: f.elapsed_us,
            build_us: f.build_us,
            raster_us: f.raster_us,
            vsync_overhead_us: f.vsync_overhead_us,
            ui,
            raster,
        };
        self.lod.insert(f.start, rec.time_us());
        if frame_status(rec.time_us(), self.budget(f.run)) != FrameStatus::Ok {
            self.slow.push(seq as u32);
        }
        self.last_start.insert(f.run, f.start);
        self.by_number.insert((f.run, f.number), seq);
        self.frames.push(rec);
        Some(id)
    }

    fn take_pending(&mut self, run: RunId, n: u64) -> (Option<UiPhases>, Option<RasterPhase>) {
        let Some(p) = self.pending.remove(&(run, n)) else {
            return (None, None);
        };
        self.pending_order.retain(|k| *k != (run, n));
        p
    }

    fn park(
        &mut self,
        run: RunId,
        n: u64,
        f: impl FnOnce(&mut (Option<UiPhases>, Option<RasterPhase>)),
    ) {
        let key = (run, n);
        if !self.pending.contains_key(&key) {
            if self.pending.len() >= MAX_PENDING_PHASES
                && let Some(old) = self.pending_order.pop_front()
            {
                self.pending.remove(&old);
            }
            self.pending_order.push_back(key);
        }
        f(self.pending.entry(key).or_default());
    }

    /// Returns whether a stored frame was updated.
    fn update(&mut self, run: RunId, n: u64, f: impl FnOnce(&mut FrameRecord)) -> bool {
        let Some(&i) = self.by_number.get(&(run, n)) else {
            return false;
        };
        if let Some(r) = self.frames.get_mut(i) {
            f(r);
            return true;
        }
        false
    }

    pub(crate) fn set_ui(&mut self, run: RunId, n: u64, ui: UiPhases) {
        if !self.update(run, n, |r| r.ui = Some(ui)) {
            self.park(run, n, |p| p.0 = Some(ui));
        }
    }

    pub(crate) fn set_raster(&mut self, run: RunId, n: u64, ra: RasterPhase) {
        if !self.update(run, n, |r| r.raster = Some(ra)) {
            self.park(run, n, |p| p.1 = Some(ra));
        }
    }

    /// Sets a run's budget and rebuilds the slow index for it.
    pub(crate) fn set_budget(&mut self, run: RunId, budget_us: u32) {
        self.budgets.insert(run, budget_us);
        let mut slow = Chunked::with_chunk(self.chunk);
        for i in 0..self.frames.len() {
            let r = self.frames.get(i).expect("within length");
            if frame_status(r.time_us(), self.budget(r.run)) != FrameStatus::Ok {
                slow.push(i as u32);
            }
        }
        self.slow = slow;
    }

    pub(crate) fn snapshot(&mut self) -> FramesSnapshot {
        FramesSnapshot {
            frames: self.frames.snapshot(),
            lod: self.lod.snapshot(),
            slow: self.slow.snapshot(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FramesSnapshot {
    frames: ChunkedSnapshot<FrameRecord>,
    lod: LodSnapshot,
    slow: ChunkedSnapshot<u32>,
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

    /// The first index in `0..len` for which `pred` is false; `pred` must be true then false.
    fn partition_point(&self, pred: impl Fn(&FrameRecord) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.frames.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.frames.get(mid).expect("within length")) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Frames whose start is in `r`, inclusive at both ends.
    pub fn range(&self, r: TimeRange) -> Range<usize> {
        let lo = self.partition_point(|f| f.start < r.start);
        let hi = self.partition_point(|f| f.start <= r.end);
        lo..hi.max(lo)
    }

    /// The frame whose `[start, end]` contains `t`; the latest-starting one when several do.
    pub fn at(&self, t: SessionTime) -> Option<usize> {
        let i = self.partition_point(|f| f.start <= t).checked_sub(1)?;
        (self.frames.get(i)?.end() >= t).then_some(i)
    }

    /// The first frame that starts after `t`.
    pub fn first_after(&self, t: SessionTime) -> Option<usize> {
        let i = self.partition_point(|f| f.start <= t);
        (i < self.frames.len()).then_some(i)
    }

    /// The frame with this number in this run. Numbers rise with start within a run, and
    /// runs are stored in order, so this is a binary search.
    pub fn by_number(&self, run: RunId, number: u64) -> Option<usize> {
        let i = self.partition_point(|f| (f.run, f.number) < (run, number));
        let f = self.frames.get(i)?;
        (f.run == run && f.number == number).then_some(i)
    }

    /// The seqs of slow and janky frames, judged with each run's budget, ascending.
    pub fn slow(&self) -> &ChunkedSnapshot<u32> {
        &self.slow
    }
}

/// One bar of the strip: the worst frame in a bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripBar {
    /// The bucket's start, clipped to the requested range.
    pub start: SessionTime,
    /// The bucket's exclusive end, clipped to the requested range.
    pub end: SessionTime,
    pub max_us: u32,
    pub count: u32,
}

/// Bars for `range` at the smallest level whose bucket count over the range
/// is at most `width_px`. Non-empty buckets only, in time order; reads at most
/// `width_px` buckets (the bound is `2 * width_px + 2`).
///
/// Edge buckets may include frames just outside `range`, since a bucket is
/// counted whole. A level that stopped growing at the bucket cap cannot hold
/// the range's end, so the next coarser level that can is used instead.
pub fn strip_bars(frames: &FramesSnapshot, range: TimeRange, width_px: usize) -> Vec<StripBar> {
    lod_cells(&frames.lod, range, width_px)
}

/// [`strip_bars`] over any level-of-detail series.
pub fn lod_cells(lod: &LodSnapshot, range: TimeRange, width_px: usize) -> Vec<StripBar> {
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
            hi - lo < width_px as u64 && hi < lod.cap as u64
        })
        .unwrap_or(LEVELS - 1);
    let Some(level) = lod.level(k) else {
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
                max_us: b.max_us,
                count: b.count,
            });
        }
        i += 1;
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Snapshot;
    use crate::clock::{Calibration, RunClock};
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
        let a = w.push_frame(input(run, 1, 7_000_000, 5_000)).unwrap();
        let b = w.push_frame(input(run, 2, 9_000_000, 20_000)).unwrap();
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
        assert_eq!(bars.iter().map(|b| b.count).sum::<u32>(), 72_000);
        assert!(bars.iter().all(|b| b.start <= b.end));
        let worst = (0..s.frames.len())
            .map(|i| s.frames.get(i).unwrap().time_us())
            .max()
            .unwrap();
        assert_eq!(bars.iter().map(|b| b.max_us).max(), Some(worst));
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
        let n: u32 = bars.iter().map(|b| b.count).sum();
        // Level 1 buckets are 2 * SPAN wide, so edge buckets may add a few.
        assert!(n as usize >= inside.len());
        assert!((n as usize) <= inside.len() + 2 * 2 * 3, "{n}");
        let worst = inside.iter().map(|f| f.time_us()).max().unwrap();
        assert!(bars.iter().map(|b| b.max_us).max().unwrap() >= worst);
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
            slow: ChunkedSnapshot::default(),
        };
        assert!(
            frames.lod().level(0).unwrap().len() <= 8,
            "level 0 is capped"
        );
        let range = TimeRange::new(SessionTime(base), SessionTime(base + SPAN));
        let bars = strip_bars(&frames, range, 4);
        assert!(!bars.is_empty() && bars.len() <= 4);
        assert_eq!(bars.iter().map(|b| b.count).sum::<u32>(), 8);
        assert_eq!(bars.iter().map(|b| b.max_us).max(), Some(1_007));
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

    fn rec(run: RunId, n: u64, start: u64, build: u32, raster: u32, elapsed: u32) -> FrameInput {
        FrameInput {
            run,
            number: n,
            start: SessionTime(start),
            elapsed_us: elapsed,
            build_us: build,
            raster_us: raster,
            vsync_overhead_us: 0,
        }
    }

    fn ui(begin: u64) -> UiPhases {
        UiPhases {
            begin: SessionTime(begin),
            ui_us: 10,
            build_us: 6,
            layout_us: 1,
            paint_us: 1,
            composite_us: 1,
            finalize_us: 1,
            widgets_built: Some(3),
        }
    }

    #[test]
    fn budget_from_refresh_rate() {
        assert_eq!(budget_us(Some(120.0)), 8_333);
        assert_eq!(budget_us(Some(59.94)), 16_683);
        assert_eq!(budget_us(Some(60.0)), 16_667);
        assert_eq!(budget_us(Some(0.0)), 16_667);
        assert_eq!(budget_us(Some(f32::NAN)), 16_667);
        assert_eq!(budget_us(Some(f32::INFINITY)), 16_667);
        assert_eq!(budget_us(None), DEFAULT_BUDGET_US);
    }

    #[test]
    fn status_thresholds() {
        let s = |t| frame_status(t, 8_333);
        assert_eq!(s(8_333), FrameStatus::Ok);
        assert_eq!(s(8_334), FrameStatus::Slow);
        assert_eq!(s(16_666), FrameStatus::Slow);
        assert_eq!(s(16_667), FrameStatus::Janky);
    }

    #[test]
    fn status_uses_build_or_raster_not_elapsed() {
        let (mut w, run) = writer(4);
        w.push_frame(rec(run, 1, 1_000, 5_000, 6_000, 20_000));
        let s = w.publish();
        let f = s.frames.get(0).unwrap();
        assert_eq!(f.time_us(), 6_000);
        assert_eq!(f.end(), SessionTime(1_000 + 20_000_000));
        assert_eq!(frame_status(f.time_us(), 8_333), FrameStatus::Ok);
        assert!(s.frames.slow().is_empty());
    }

    #[test]
    fn phases_set_before_and_after_the_frame_arrive() {
        let (mut w, run) = writer(4);
        w.push_frame(rec(run, 1, 1_000, 1, 1, 2));
        let before = w.publish();
        w.set_frame_ui(run, 1, ui(5));
        w.set_frame_raster(
            run,
            1,
            RasterPhase {
                start: SessionTime(7),
                raster_us: 4,
            },
        );
        // Parked first, then drained by the push.
        w.set_frame_ui(run, 2, ui(9));
        w.set_frame_raster(
            run,
            2,
            RasterPhase {
                start: SessionTime(11),
                raster_us: 5,
            },
        );
        w.push_frame(rec(run, 2, 2_000, 1, 1, 2));
        let after = w.publish();
        assert_eq!(
            before.frames.get(0).unwrap().ui,
            None,
            "old snapshot unchanged"
        );
        let f1 = after.frames.get(0).unwrap();
        assert_eq!(f1.ui, Some(ui(5)));
        assert_eq!(f1.raster.unwrap().raster_us, 4);
        let f2 = after.frames.get(1).unwrap();
        assert_eq!(f2.ui, Some(ui(9)));
        assert_eq!(f2.raster.unwrap().start, SessionTime(11));
    }

    #[test]
    fn pending_phases_are_bounded() {
        let (mut w, run) = writer(4);
        for n in 0..5_000u64 {
            w.set_frame_ui(run, n, ui(n));
        }
        for (i, n) in [0u64, 903, 904, 4_999].into_iter().enumerate() {
            w.push_frame(rec(run, n, 1_000 + i as u64, 1, 1, 2));
        }
        let s = w.publish();
        let has = |i: usize| s.frames.get(i).unwrap().ui.is_some();
        assert!(!has(0), "oldest dropped");
        assert!(!has(1), "903 dropped");
        assert!(has(2), "904 kept");
        assert!(has(3), "4999 kept");
    }

    #[test]
    fn replayed_and_duplicate_frames_are_dropped() {
        let (mut w, run) = writer(4);
        assert!(w.push_frame(rec(run, 5, 10_000, 1, 1, 2)).is_some());
        assert_eq!(w.push_frame(rec(run, 5, 10_000, 1, 1, 2)), None);
        assert_eq!(w.push_frame(rec(run, 4, 9_000, 1, 1, 2)), None);
        assert!(w.push_frame(rec(run, 6, 10_001, 1, 1, 2)).is_some());
        let clock = RunClock::new(
            0,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 0,
            },
        );
        let run2 = w.start_run(
            crate::run::AppInfo {
                name: "b".into(),
                device: None,
                mode: crate::run::Mode::Debug,
                flutter_version: None,
                workspace: None,
                vm_uri: String::new(),
                pid: None,
            },
            clock,
            SessionTime(20_000),
        );
        let id = w.push_frame(rec(run2, 1, 20_001, 1, 1, 2));
        assert_eq!(id.map(|i| i.seq), Some(2));
        assert_eq!(w.publish().frames.len(), 3);
    }

    #[test]
    fn range_at_first_after_and_by_number() {
        let (mut w, run) = writer(64);
        synth::frames(&mut w, run, 10_000, 3, 120);
        let s = w.publish();
        let f = &s.frames;
        let step = 1_000_000_000 / 120;
        let r = TimeRange::new(SessionTime(10 * step), SessionTime(20 * step));
        assert_eq!(f.range(r), 10..21, "inclusive at both ends");
        let r = TimeRange::new(SessionTime(10 * step + 1), SessionTime(20 * step - 1));
        assert_eq!(f.range(r), 11..20);
        let none = TimeRange::new(SessionTime(u64::MAX - 1), SessionTime(u64::MAX));
        assert!(f.range(none).is_empty());
        let rec7 = f.get(7).unwrap();
        assert_eq!(f.at(rec7.start), Some(7));
        assert_eq!(f.at(SessionTime(rec7.start.0 + 1)), Some(7));
        assert_eq!(f.at(rec7.end()), Some(7));
        assert_eq!(f.first_after(rec7.start), Some(8));
        assert_eq!(f.first_after(SessionTime(0)), Some(1));
        assert_eq!(f.first_after(f.get(9_999).unwrap().start), None);
        assert_eq!(f.by_number(run, 1234), Some(1234));
        assert_eq!(f.by_number(run, 10_000), None);
        assert_eq!(f.by_number(RunId(9), 1), None);
        // A frame ends before the gap to the next one starts: nothing is there.
        let gap = f.get(3).unwrap();
        if gap.end() < f.get(4).unwrap().start {
            assert_eq!(f.at(SessionTime(gap.end().0 + 1)), None);
        }
        let ui = f.get(5).unwrap().ui.unwrap();
        let ra = f.get(5).unwrap().raster.unwrap();
        let fr = f.get(5).unwrap();
        assert_eq!(ui.begin, fr.start);
        assert_eq!(ui.ui_us, fr.build_us);
        assert_eq!(
            ui.build_us + ui.layout_us + ui.paint_us + ui.composite_us + ui.finalize_us,
            fr.build_us
        );
        assert_eq!(ra.start.0, fr.start.0 + fr.build_us as u64 * 1_000);
        assert_eq!(ra.raster_us, fr.raster_us);
        assert_eq!(ui.widgets_built, None);
    }

    #[test]
    fn slow_index_follows_the_refresh_rate() {
        let (mut w, run) = writer(8);
        synth::frames(&mut w, run, 5_000, 11, 120);
        let at_60 = w.publish();
        let n60 = at_60.frames.slow().len();
        let expect = |s: &crate::Snapshot| {
            (0..s.frames.len())
                .filter(|&i| s.frames.get(i).unwrap().time_us() > s.frame_budget_us(run))
                .count()
        };
        assert_eq!(n60, expect(&at_60));
        w.set_refresh_rate(run, 120.0);
        let at_120 = w.publish();
        assert_eq!(at_120.frame_budget_us(run), 8_333);
        assert_eq!(at_120.latest_budget_us(), 8_333);
        assert!(
            at_120.frames.slow().len() > n60,
            "{} {}",
            at_120.frames.slow().len(),
            n60
        );
        assert_eq!(at_120.frames.slow().len(), expect(&at_120));
        assert_eq!(at_60.frames.slow().len(), n60, "old snapshot unchanged");
        assert_eq!(Snapshot::empty().latest_budget_us(), DEFAULT_BUDGET_US);
        // Seqs are ascending frame indexes.
        let seqs: Vec<u32> = at_120.frames.slow().chunks().flatten().copied().collect();
        assert!(seqs.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn lod_keeps_max_of_build_and_raster() {
        let (mut w, run) = writer(4);
        w.push_frame(rec(run, 1, 10, 3_000, 9_000, 50_000));
        w.push_frame(rec(run, 2, 20, 7_000, 2_000, 50_000));
        let s = w.publish();
        assert_eq!(
            s.frames.lod().level(0).unwrap().get(0),
            Some(&Bucket {
                max_us: 9_000,
                count: 2
            })
        );
        let range = TimeRange::new(SessionTime(0), SessionTime(SPAN));
        let bars = lod_cells(s.frames.lod(), range, 10);
        assert_eq!(bars.len(), 1);
        assert_eq!((bars[0].max_us, bars[0].count), (9_000, 2));
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
