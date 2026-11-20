//! The timeline slice store: interned names, tracks, per-depth lanes of slices and each lane's
//! level-of-detail cells.
//!
//! A lane is one track at one depth. Its slices arrive in start order and never overlap, so a
//! lane is a sorted array and every level-of-detail update touches the last cell or appends one.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::frames::{LodSeries, LodSnapshot};
use crate::time::{RunId, SessionTime, TimeRange};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Lanes deeper than this are dropped.
pub const MAX_DEPTH: usize = 256;
/// Level-0 cells are `1 << 22` ns (about 4.2 ms); each level doubles.
pub const LANE_LOD_BASE_SHIFT: u32 = 22;
pub const LANE_LOD_LEVELS: usize = 20;
/// The most widget-build slices a session keeps.
pub const RAW_BUILD_CAP: u64 = 8_000_000;

const SLICE_CHUNK: usize = 1024;
const CELL_CHUNK: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NameId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Thread,
    Async,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SliceCat {
    Dart,
    Embedder,
    Gc,
    Other,
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub id: TrackId,
    pub run: RunId,
    pub kind: TrackKind,
    pub name: Arc<str>,
    /// 0 for the UI thread, 1 for raster and 10 for the rest; tracks sort by `(run, order, name)`.
    pub order: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    pub start: SessionTime,
    pub dur_ns: u64,
    pub name: NameId,
    pub cat: SliceCat,
}

impl Slice {
    pub fn end(&self) -> SessionTime {
        SessionTime(self.start.0.saturating_add(self.dur_ns))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimelineDrops {
    pub depth: u64,
    pub overlap: u64,
    pub build_cap: u64,
}

/// One sparse cell of a lane level: `(index, slices touching it, nanoseconds covered)`.
#[derive(Clone, Copy, Debug)]
struct Cell {
    idx: u32,
    count: u32,
    covered_ns: u64,
}

/// A cell handed to the painter; `start`/`end` are clipped to the requested range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneCell {
    pub start: SessionTime,
    pub end: SessionTime,
    /// Nanoseconds of the whole cell that slices cover.
    pub covered_ns: u64,
    pub count: u32,
}

struct Lane {
    slices: Chunked<Slice>,
    levels: Vec<Chunked<Cell>>,
    end: u64,
    cached: Option<LaneSnapshot>,
}

impl Lane {
    fn new() -> Self {
        Self {
            slices: Chunked::with_chunk(SLICE_CHUNK),
            levels: (0..LANE_LOD_LEVELS)
                .map(|_| Chunked::with_chunk(CELL_CHUNK))
                .collect(),
            end: 0,
            cached: None,
        }
    }

    fn push(&mut self, s: Slice) {
        let end = s.end().0;
        for (k, level) in self.levels.iter_mut().enumerate() {
            let sh = LANE_LOD_BASE_SHIFT + k as u32;
            let lo = s.start.0 >> sh;
            let hi = if s.dur_ns == 0 { lo } else { (end - 1) >> sh };
            for idx in lo..=hi {
                let Ok(cell_idx) = u32::try_from(idx) else {
                    break;
                };
                let cover = end.min((idx + 1) << sh) - s.start.0.max(idx << sh).min(end);
                let last = level.len().checked_sub(1);
                match last.and_then(|i| level.get_mut(i)) {
                    Some(c) if c.idx == cell_idx => {
                        c.count += 1;
                        c.covered_ns += cover;
                    }
                    _ => {
                        level.push(Cell {
                            idx: cell_idx,
                            count: 1,
                            covered_ns: cover,
                        });
                    }
                }
            }
        }
        self.slices.push(s);
        self.end = self.end.max(end);
        self.cached = None;
    }

    fn snapshot(&mut self) -> LaneSnapshot {
        if let Some(c) = &self.cached {
            return c.clone();
        }
        let snap = LaneSnapshot {
            slices: self.slices.snapshot(),
            levels: self.levels.iter_mut().map(|l| l.snapshot()).collect(),
        };
        self.cached = Some(snap.clone());
        snap
    }
}

#[derive(Default)]
struct TrackData {
    lanes: Vec<Lane>,
    cache: Option<Arc<[LaneSnapshot]>>,
}

pub(crate) struct TimelineStore {
    names: Vec<Arc<str>>,
    name_ids: HashMap<Arc<str>, NameId>,
    names_cache: Option<Arc<[Arc<str>]>>,
    tracks: Vec<TrackInfo>,
    track_ids: HashMap<(RunId, TrackKind, u64), TrackId>,
    sorted_cache: Option<Arc<[TrackInfo]>>,
    data: Vec<TrackData>,
    gc: LodSeries,
    drops: TimelineDrops,
    build_slices: u64,
    build_cap: u64,
    slices: u64,
}

impl TimelineStore {
    pub(crate) fn new() -> Self {
        Self {
            names: Vec::new(),
            name_ids: HashMap::new(),
            names_cache: None,
            tracks: Vec::new(),
            track_ids: HashMap::new(),
            sorted_cache: None,
            data: Vec::new(),
            gc: LodSeries::new(),
            drops: TimelineDrops::default(),
            build_slices: 0,
            build_cap: RAW_BUILD_CAP,
            slices: 0,
        }
    }

    pub(crate) fn name_count(&self) -> usize {
        self.names.len()
    }

    #[cfg(test)]
    pub(crate) fn set_build_cap(&mut self, cap: u64) {
        self.build_cap = cap;
    }

    pub(crate) fn intern(&mut self, s: &str) -> NameId {
        if let Some(&id) = self.name_ids.get(s) {
            return id;
        }
        let id = NameId(self.names.len() as u32);
        let a: Arc<str> = Arc::from(s);
        self.names.push(a.clone());
        self.name_ids.insert(a, id);
        self.names_cache = None;
        id
    }

    pub(crate) fn track(&mut self, run: RunId, kind: TrackKind, key: u64, name: &str) -> TrackId {
        if let Some(&id) = self.track_ids.get(&(run, kind, key)) {
            let t = &mut self.tracks[id.0 as usize];
            if t.name.is_empty() && !name.is_empty() {
                t.name = Arc::from(name);
                self.sorted_cache = None;
            }
            return id;
        }
        let id = TrackId(self.tracks.len() as u32);
        self.tracks.push(TrackInfo {
            id,
            run,
            kind,
            name: Arc::from(name),
            order: 10,
        });
        self.data.push(TrackData::default());
        self.track_ids.insert((run, kind, key), id);
        self.sorted_cache = None;
        id
    }

    pub(crate) fn set_order(&mut self, id: TrackId, order: u32) {
        if let Some(t) = self.tracks.get_mut(id.0 as usize)
            && t.order != order
        {
            t.order = order;
            self.sorted_cache = None;
        }
    }

    /// Returns false, storing nothing, when the slice is dropped (and counted).
    pub(crate) fn push(
        &mut self,
        track: TrackId,
        depth: u16,
        s: Slice,
        widget_build: bool,
    ) -> bool {
        let Some(data) = self.data.get_mut(track.0 as usize) else {
            return false;
        };
        let depth = depth as usize;
        if depth >= MAX_DEPTH {
            self.drops.depth += 1;
            return false;
        }
        if data.lanes.get(depth).is_some_and(|l| s.start.0 < l.end) {
            self.drops.overlap += 1;
            return false;
        }
        if widget_build && self.build_slices >= self.build_cap {
            self.drops.build_cap += 1;
            return false;
        }
        while data.lanes.len() <= depth {
            data.lanes.push(Lane::new());
        }
        data.lanes[depth].push(s);
        data.cache = None;
        if widget_build {
            self.build_slices += 1;
        }
        if s.cat == SliceCat::Gc && depth == 0 {
            self.gc
                .insert(s.start, (s.dur_ns / 1_000).min(u64::from(u32::MAX)) as u32);
        }
        self.slices += 1;
        true
    }

    pub(crate) fn snapshot(&mut self) -> TimelineSnapshot {
        let names = self
            .names_cache
            .get_or_insert_with(|| self.names.clone().into())
            .clone();
        let tracks = self
            .sorted_cache
            .get_or_insert_with(|| {
                let mut t = self.tracks.clone();
                t.sort_by(|a, b| {
                    (a.run, a.order, &a.name, a.id).cmp(&(b.run, b.order, &b.name, b.id))
                });
                t.into()
            })
            .clone();
        let lanes = self
            .data
            .iter_mut()
            .map(|d| {
                d.cache
                    .get_or_insert_with(|| d.lanes.iter_mut().map(|l| l.snapshot()).collect())
                    .clone()
            })
            .collect();
        TimelineSnapshot {
            names,
            tracks,
            lanes,
            drops: self.drops,
            gc: self.gc.snapshot(),
            slices: self.slices,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TimelineSnapshot {
    names: Arc<[Arc<str>]>,
    tracks: Arc<[TrackInfo]>,
    lanes: Arc<[Arc<[LaneSnapshot]>]>,
    drops: TimelineDrops,
    gc: LodSnapshot,
    slices: u64,
}

impl Default for TimelineSnapshot {
    fn default() -> Self {
        TimelineStore::new().snapshot()
    }
}

impl TimelineSnapshot {
    pub fn name(&self, id: NameId) -> &str {
        self.names.get(id.0 as usize).map_or("", |n| n)
    }

    /// Tracks sorted by `(run, order, name)`.
    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    /// The lanes of a track, indexed by depth.
    pub fn lanes(&self, track: TrackId) -> &[LaneSnapshot] {
        self.lanes.get(track.0 as usize).map_or(&[], |l| l)
    }

    pub fn drops(&self) -> TimelineDrops {
        self.drops
    }

    /// Every `Gc` slice at depth 0, as `insert(start, dur_us)`.
    pub fn gc_lod(&self) -> &LodSnapshot {
        &self.gc
    }

    pub fn slice_count(&self) -> u64 {
        self.slices
    }
}

#[derive(Clone, Debug)]
pub struct LaneSnapshot {
    slices: ChunkedSnapshot<Slice>,
    levels: Arc<[ChunkedSnapshot<Cell>]>,
}

impl LaneSnapshot {
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    pub fn get(&self, i: usize) -> &Slice {
        self.slices.get(i).expect("slice index within the lane")
    }

    /// Cells stored across every level (for memory accounting).
    pub fn cell_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }

    fn partition_point(&self, pred: impl Fn(&Slice) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.get(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Every slice that overlaps `r` (inclusive at both ends).
    pub fn range_indices(&self, r: TimeRange) -> Range<usize> {
        let mut lo = self.partition_point(|s| s.start < r.start);
        let hi = self.partition_point(|s| s.start <= r.end).max(lo);
        if lo > 0 && self.get(lo - 1).end() >= r.start {
            lo -= 1;
        }
        lo..hi
    }

    /// Non-empty cells over `r` at the smallest level with at most `width_px` cells across it.
    pub fn cells(&self, r: TimeRange, width_px: usize) -> Vec<LaneCell> {
        if width_px == 0 || self.is_empty() {
            return Vec::new();
        }
        let span = |k: usize| {
            let sh = LANE_LOD_BASE_SHIFT + k as u32;
            (r.start.0 >> sh, r.end.0 >> sh)
        };
        let k = (0..LANE_LOD_LEVELS)
            .find(|&k| {
                let (lo, hi) = span(k);
                hi - lo < width_px as u64
            })
            .unwrap_or(LANE_LOD_LEVELS - 1);
        let level = &self.levels[k];
        let sh = LANE_LOD_BASE_SHIFT + k as u32;
        let (lo, hi) = span(k);
        // First cell with index >= lo.
        let (mut a, mut b) = (0, level.len());
        while a < b {
            let mid = a + (b - a) / 2;
            if u64::from(level.get(mid).expect("within level").idx) < lo {
                a = mid + 1;
            } else {
                b = mid;
            }
        }
        let mut out = Vec::new();
        for i in a..level.len() {
            let c = level.get(i).expect("within level");
            let idx = u64::from(c.idx);
            if idx > hi {
                break;
            }
            out.push(LaneCell {
                start: SessionTime((idx << sh).max(r.start.0)),
                end: SessionTime(((idx + 1) << sh).min(r.end.0)),
                covered_ns: c.covered_ns,
                count: c.count,
            });
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn slice_chunk(&self, c: usize) -> Option<&Arc<Vec<Slice>>> {
        self.slices.sealed_chunk(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Calibration, RunClock};
    use crate::logs::testkit::{p90_worst, serial};
    use crate::run::{AppInfo, Mode};
    use crate::session::SessionWriter;
    use crate::synth;

    fn begin(w: &mut SessionWriter) -> RunId {
        let app = AppInfo {
            name: "t".into(),
            device: None,
            mode: Mode::Debug,
            flutter_version: None,
            workspace: None,
            vm_uri: "ws://127.0.0.1:1/ws".into(),
            pid: None,
        };
        let clock = RunClock::new(
            0,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 0,
            },
        );
        w.start_run(app, clock, SessionTime(0))
    }

    fn sl(start: u64, dur: u64, name: NameId) -> Slice {
        Slice {
            start: SessionTime(start),
            dur_ns: dur,
            name,
            cat: SliceCat::Dart,
        }
    }

    fn range(a: u64, b: u64) -> TimeRange {
        TimeRange::new(SessionTime(a), SessionTime(b))
    }

    #[test]
    fn slice_is_24_bytes() {
        assert_eq!(std::mem::size_of::<Slice>(), 24);
    }

    #[test]
    fn tracks_are_idempotent_and_named_late() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let a = w.timeline_track(run, TrackKind::Thread, 7, "");
        let b = w.timeline_track(run, TrackKind::Thread, 7, "io.flutter.ui");
        let c = w.timeline_track(run, TrackKind::Thread, 7, "other");
        let d = w.timeline_track(run, TrackKind::Async, 7, "async");
        assert_eq!(a, b);
        assert_eq!(c, a);
        assert_ne!(d, a);
        w.set_track_order(a, 0);
        let s = w.publish();
        let t = s.timeline.tracks();
        assert_eq!(t.len(), 2);
        assert_eq!((&*t[0].name, t[0].order), ("io.flutter.ui", 0));
        assert_eq!((&*t[1].name, t[1].order), ("async", 10));
        assert_eq!(w.intern_name("x"), w.intern_name("x"));
    }

    #[test]
    fn range_indices_include_a_straddling_slice() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        for (s, d) in [(0, 100), (200, 300), (600, 50), (900, 10)] {
            assert!(w.push_slice(t, 0, sl(s, d, n), false));
        }
        let snap = w.publish();
        let lane = &snap.timeline.lanes(t)[0];
        assert_eq!(snap.timeline.name(n), "n");
        assert_eq!(lane.range_indices(range(250, 650)), 1..3);
        assert_eq!(lane.range_indices(range(100, 200)), 0..2);
        assert_eq!(lane.range_indices(range(101, 199)), 1..1);
        assert_eq!(lane.range_indices(range(0, 5_000)), 0..4);
        assert_eq!(lane.range_indices(range(2_000, 3_000)), 4..4);
    }

    #[test]
    fn overlapping_and_too_deep_slices_are_dropped_and_counted() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        assert!(w.push_slice(t, 0, sl(100, 100, n), false));
        assert!(!w.push_slice(t, 0, sl(150, 10, n), false));
        assert!(w.push_slice(t, 0, sl(200, 10, n), false));
        assert!(!w.push_slice(t, 0, sl(50, 10, n), false));
        assert!(!w.push_slice(t, MAX_DEPTH as u16, sl(0, 10, n), false));
        assert!(w.push_slice(t, MAX_DEPTH as u16 - 1, sl(0, 10, n), false));
        let s = w.publish();
        assert_eq!(
            s.timeline.drops(),
            TimelineDrops {
                depth: 1,
                overlap: 2,
                build_cap: 0
            }
        );
        assert_eq!(s.timeline.slice_count(), 3);
        assert_eq!(s.timeline.lanes(t).len(), MAX_DEPTH);
        assert_eq!(s.timeline.lanes(t)[0].len(), 2);
    }

    #[test]
    fn widget_build_cap_drops_only_build_slices() {
        let mut w = SessionWriter::new(0).with_build_cap(10);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        for i in 0..15u64 {
            assert_eq!(w.push_slice(t, 3, sl(i * 10, 5, n), true), i < 10);
        }
        for i in 0..5u64 {
            assert!(w.push_slice(t, 0, sl(i * 10, 5, n), false));
        }
        let s = w.publish();
        assert_eq!(s.timeline.drops().build_cap, 5);
        assert_eq!(s.timeline.slice_count(), 15);
    }

    #[test]
    fn cells_fit_the_width_and_conserve_coverage() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        let mut x = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut at, mut total) = (0u64, 0u64);
        for _ in 0..10_000 {
            at += next() % 3_000_000;
            let dur = next() % 20_000_000;
            assert!(w.push_slice(t, 0, sl(at, dur, n), false));
            total += dur;
            at += dur;
        }
        let snap = w.publish();
        let lane = &snap.timeline.lanes(t)[0];
        let whole = range(0, at);
        for width in [1usize, 10, 333, 2_000, 100_000] {
            let cells = lane.cells(whole, width);
            assert!(cells.len() <= width, "width {width}: {}", cells.len());
            assert_eq!(
                cells.iter().map(|c| c.covered_ns).sum::<u64>(),
                total,
                "width {width}"
            );
            assert!(cells.windows(2).all(|p| p[0].end <= p[1].start));
        }
        assert!(lane.cells(whole, 0).is_empty());
    }

    #[test]
    fn long_slices_spread_over_cells_and_levels() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        let cell = 1u64 << LANE_LOD_BASE_SHIFT;
        w.push_slice(t, 0, sl(cell / 2, 3 * cell, n), false);
        let snap = w.publish();
        let c = snap.timeline.lanes(t)[0].cells(range(0, 8 * cell), 16);
        let cov: Vec<u64> = c.iter().map(|c| c.covered_ns).collect();
        assert_eq!(cov, vec![cell / 2, cell, cell, cell / 2]);
        assert!(c.iter().all(|c| c.count == 1));
    }

    #[test]
    fn sealed_chunks_are_shared_between_publishes() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("n");
        for i in 0..(SLICE_CHUNK as u64 + 10) {
            w.push_slice(t, 0, sl(i * 10, 5, n), false);
        }
        let a = w.publish();
        for i in 0..100u64 {
            w.push_slice(t, 0, sl(1_000_000 + i * 10, 5, n), false);
        }
        let b = w.publish();
        let (la, lb) = (&a.timeline.lanes(t)[0], &b.timeline.lanes(t)[0]);
        assert!(Arc::ptr_eq(
            la.slice_chunk(0).unwrap(),
            lb.slice_chunk(0).unwrap()
        ));
        assert_eq!((la.len(), lb.len()), (SLICE_CHUNK + 10, SLICE_CHUNK + 110));
        // An untouched track reuses its whole lane list.
        let t2 = w.timeline_track(run, TrackKind::Thread, 2, "u");
        w.push_slice(t2, 0, sl(0, 1, n), false);
        let c = w.publish();
        assert!(Arc::ptr_eq(
            &b.timeline.lanes[t.0 as usize],
            &c.timeline.lanes[t.0 as usize]
        ));
    }

    #[test]
    fn gc_lod_counts_gc_slices() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let t = w.timeline_track(run, TrackKind::Thread, 1, "t");
        let n = w.intern_name("gc");
        for i in 0..3u64 {
            let s = Slice {
                cat: SliceCat::Gc,
                ..sl(i * 1_000_000_000, 2_000_000, n)
            };
            w.push_slice(t, 0, s, false);
        }
        // Not depth 0, and not a Gc slice: neither counts.
        let deep = Slice {
            cat: SliceCat::Gc,
            ..sl(0, 5, n)
        };
        w.push_slice(t, 1, deep, false);
        w.push_slice(t, 0, sl(5_000_000_000, 5, n), false);
        let s = w.publish();
        let top = s
            .timeline
            .gc_lod()
            .level(crate::frames::LEVELS - 1)
            .unwrap();
        let (count, max) = top
            .chunks()
            .flatten()
            .fold((0, 0), |(c, m), b| (c + b.count, m.max(b.max_us)));
        assert_eq!((count, max), (3, 2_000));
    }

    #[test]
    fn synth_perf_makes_about_70_slices_per_frame() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        synth::perf(&mut w, run, 1_000, 3, 120);
        let s = w.publish();
        let n = s.timeline.slice_count();
        assert!((60_000..80_000).contains(&n), "{n} slices");
        assert_eq!(s.frames.len(), 1_000);
        assert!((0..s.frames.len()).all(|i| {
            let f = s.frames.get(i).unwrap();
            f.ui.is_some() && f.raster.is_some()
        }));
        let names: Vec<&str> = s.timeline.tracks().iter().map(|t| &*t.name).collect();
        assert_eq!(names[..2], ["io.flutter.ui", "io.flutter.raster"]);
        assert!(names.contains(&"DartWorker") && names.contains(&"PipelineItem"));
        let d = s.timeline.drops();
        assert_eq!((d.depth, d.overlap, d.build_cap), (0, 0, 0));
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn lane_query_budget_5m() {
        let _g = serial();
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        synth::perf(&mut w, run, 72_000, 7, 120);
        let s = w.publish();
        let whole = TimeRange::new(SessionTime(0), s.latest_time());
        let ui = s.timeline.tracks()[0].id;
        let lanes = s.timeline.lanes(ui);
        let (mut slices, mut cells) = (0u64, 0u64);
        for tr in s.timeline.tracks() {
            for l in s.timeline.lanes(tr.id) {
                slices += l.len() as u64;
                cells += l.cell_count() as u64;
            }
        }
        println!(
            "synth::perf(72_000): {slices} slices ({} MB) + {cells} cells ({} MB), drops {:?}",
            slices * 24 / 1_000_000,
            cells * 16 / 1_000_000,
            s.timeline.drops()
        );
        let mut ms = Vec::new();
        for _ in 0..50 {
            let t = std::time::Instant::now();
            for l in lanes {
                std::hint::black_box(l.range_indices(whole));
                std::hint::black_box(l.cells(whole, 2_000));
            }
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let (p90, worst) = p90_worst(ms);
        println!(
            "lane query over {} UI lanes: p90 {p90:.4} ms, worst {worst:.4} ms",
            lanes.len()
        );
        crate::budget::check(p90 <= 0.5, || format!("p90 {p90} ms"));
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn timeline_publish_budget() {
        let _g = serial();
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        synth::perf(&mut w, run, 2_000, 7, 120);
        w.publish();
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "");
        let n = w.intern_name("extra");
        let mut at = w.publish().latest_time().0 + 1_000;
        let mut ms = Vec::new();
        for _ in 0..50 {
            let t = std::time::Instant::now();
            for i in 0..10_000u64 {
                w.push_slice(ui, (i % 6) as u16 + 6, sl(at, 100, n), false);
                if i % 6 == 5 {
                    at += 1_000;
                }
            }
            std::hint::black_box(w.publish());
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let (p90, worst) = p90_worst(ms);
        println!("10,000 slices + publish: p90 {p90:.4} ms, worst {worst:.4} ms");
        crate::budget::check(p90 <= 2.0, || format!("p90 {p90} ms"));
    }
}
