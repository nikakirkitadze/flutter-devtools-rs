//! The memory store: heap samples, GC records, allocation profiles and heap-snapshot records.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::frames::{LodSeries, LodSnapshot};
use crate::heap::{HeapDiff, HeapFile, HeapId, HeapIndex, HeapRecord, HeapStage, HeapSummary};
use crate::time::{RunId, SessionTime, TimeRange};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// RSS must move by this much for a sample with unchanged heap values to be stored.
pub const RSS_STEP: u64 = 256 * 1024;
/// A flat series still stores a sample this often.
pub const SAMPLE_MAX_GAP_NS: u64 = 5_000_000_000;
/// Allocation profiles kept; the oldest is dropped beyond it.
pub const MAX_PROFILES: usize = 1_000;

const SAMPLE_CHUNK: usize = 1024;
const GC_CHUNK: usize = 2048;
const CLASS_CHUNK: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySample {
    pub time: SessionTime,
    pub heap_used: u64,
    pub heap_capacity: u64,
    pub external: u64,
    pub rss: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GcKind {
    Scavenge,
    Evacuate,
    StartMark,
    MarkSweep,
    MarkCompact,
    Other,
}

impl GcKind {
    /// From the VM's `gcType`.
    pub fn from_vm(s: &str) -> Self {
        match s {
            "Scavenge" => Self::Scavenge,
            "Evacuate" => Self::Evacuate,
            "StartCMark" | "StartMark" | "StartConcurrentMark" => Self::StartMark,
            "MarkSweep" => Self::MarkSweep,
            "MarkCompact" => Self::MarkCompact,
            _ => Self::Other,
        }
    }

    pub fn is_major(self) -> bool {
        matches!(self, Self::MarkSweep | Self::MarkCompact)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Scavenge => "Scavenge",
            Self::Evacuate => "Evacuate",
            Self::StartMark => "Start mark",
            Self::MarkSweep => "Mark-sweep",
            Self::MarkCompact => "Mark-compact",
            Self::Other => "Other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GcReason {
    NewSpace,
    StoreBuffer,
    Promotion,
    OldSpace,
    Finalize,
    Full,
    External,
    Idle,
    Destroyed,
    Debugging,
    CatchUp,
    Other,
}

impl GcReason {
    /// From the VM's `reason`.
    pub fn from_vm(s: &str) -> Self {
        match s {
            "new space" => Self::NewSpace,
            "store buffer" => Self::StoreBuffer,
            "promotion" => Self::Promotion,
            "old space" => Self::OldSpace,
            "finalize" => Self::Finalize,
            "full" => Self::Full,
            "external" => Self::External,
            "idle" => Self::Idle,
            "destroyed" => Self::Destroyed,
            "debugging" => Self::Debugging,
            "catch-up" => Self::CatchUp,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NewSpace => "New space full",
            Self::StoreBuffer => "Store buffer full",
            Self::Promotion => "Promotion",
            Self::OldSpace => "Old space full",
            Self::Finalize => "Finalize",
            Self::Full => "Full collection",
            Self::External => "External memory",
            Self::Idle => "Idle",
            Self::Destroyed => "Isolate destroyed",
            Self::Debugging => "Requested by a tool",
            Self::CatchUp => "Catch-up",
            Self::Other => "Other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcRecord {
    /// When the GC ended.
    pub time: SessionTime,
    pub dur_us: Option<u32>,
    pub kind: GcKind,
    pub reason: GcReason,
    /// New plus old space after the GC.
    pub heap_used: u64,
    pub heap_capacity: u64,
    pub external: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AllocClassId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocClass {
    pub run: RunId,
    pub isolate: Arc<str>,
    pub vm_id: Arc<str>,
    pub name: Arc<str>,
    pub library: Option<Arc<str>>,
    /// `file.dart:39`.
    pub location: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocRow {
    pub class: AllocClassId,
    pub instances: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct AllocProfile {
    pub time: SessionTime,
    pub run: RunId,
    pub isolate: Arc<str>,
    /// Fetched with `gc: true`.
    pub gc: bool,
    pub heap_used: u64,
    pub rows: Arc<[AllocRow]>,
}

type ClassKeys = HashMap<RunId, HashMap<Arc<str>, HashMap<Arc<str>, AllocClassId>>>;

pub(crate) struct MemoryStore {
    samples: Chunked<MemorySample>,
    last_sample: HashMap<RunId, MemorySample>,
    gcs: Chunked<GcRecord>,
    gc_lod: LodSeries,
    classes: Chunked<Arc<AllocClass>>,
    class_ids: ClassKeys,
    profiles: Vec<Arc<AllocProfile>>,
    profiles_cache: Option<Arc<[Arc<AllocProfile>]>>,
    heaps: Vec<HeapRecord>,
    heaps_cache: Option<Arc<[HeapRecord]>>,
    next_heap: u32,
    held: Option<HeapId>,
    baseline: Option<HeapId>,
}

impl MemoryStore {
    pub(crate) fn new() -> Self {
        Self {
            samples: Chunked::with_chunk(SAMPLE_CHUNK),
            last_sample: HashMap::new(),
            gcs: Chunked::with_chunk(GC_CHUNK),
            gc_lod: LodSeries::new(),
            classes: Chunked::with_chunk(CLASS_CHUNK),
            class_ids: HashMap::new(),
            profiles: Vec::new(),
            profiles_cache: None,
            heaps: Vec::new(),
            heaps_cache: None,
            next_heap: 0,
            held: None,
            baseline: None,
        }
    }

    /// Whether the sample was stored. Samples older than the last stored one are dropped.
    pub(crate) fn push_sample(&mut self, run: RunId, s: MemorySample) -> bool {
        if !self.samples.is_empty()
            && self
                .samples
                .get(self.samples.len() - 1)
                .is_some_and(|l| s.time < l.time)
        {
            return false;
        }
        if let Some(l) = self.last_sample.get(&run) {
            let rss_same = match (l.rss, s.rss) {
                (Some(a), Some(b)) => a.abs_diff(b) < RSS_STEP,
                (None, None) => true,
                _ => false,
            };
            let fresh = s.time.0.saturating_sub(l.time.0) < SAMPLE_MAX_GAP_NS;
            if l.heap_used == s.heap_used
                && l.heap_capacity == s.heap_capacity
                && l.external == s.external
                && rss_same
                && fresh
            {
                return false;
            }
        }
        self.samples.push(s);
        self.last_sample.insert(run, s);
        true
    }

    /// Whether the record was stored; one earlier than the last stored record is dropped.
    pub(crate) fn push_gc(&mut self, g: GcRecord) -> bool {
        if self
            .gcs
            .len()
            .checked_sub(1)
            .and_then(|i| self.gcs.get(i))
            .is_some_and(|l| g.time < l.time)
        {
            return false;
        }
        self.gc_lod.insert(g.time, g.dur_us.unwrap_or(0));
        self.gcs.push(g);
        true
    }

    /// Returns the id and whether the class is new.
    pub(crate) fn intern_class(
        &mut self,
        run: RunId,
        isolate: &str,
        vm_id: &str,
        name: &str,
        library: Option<&str>,
        location: Option<&str>,
    ) -> (AllocClassId, bool) {
        if let Some(&id) = self
            .class_ids
            .get(&run)
            .and_then(|m| m.get(isolate))
            .and_then(|m| m.get(vm_id))
        {
            return (id, false);
        }
        let id = AllocClassId(self.classes.len() as u32);
        let (isolate, vm_id): (Arc<str>, Arc<str>) = (isolate.into(), vm_id.into());
        self.class_ids
            .entry(run)
            .or_default()
            .entry(isolate.clone())
            .or_default()
            .insert(vm_id.clone(), id);
        self.classes.push(Arc::new(AllocClass {
            run,
            isolate,
            vm_id,
            name: name.into(),
            library: library.map(Arc::from),
            location: location.map(Arc::from),
        }));
        (id, true)
    }

    pub(crate) fn push_profile(&mut self, p: AllocProfile) {
        let at = self.profiles.partition_point(|q| q.time <= p.time);
        self.profiles.insert(at, Arc::new(p));
        if self.profiles.len() > MAX_PROFILES {
            self.profiles.remove(0);
        }
        self.profiles_cache = None;
    }

    pub(crate) fn heap_requested(&mut self, run: RunId, isolate: &str, at: SessionTime) -> HeapId {
        let id = HeapId(self.next_heap);
        self.next_heap += 1;
        self.heaps.push(HeapRecord {
            id,
            run,
            isolate: isolate.into(),
            requested: at,
            time: None,
            bytes: 0,
            stage: HeapStage::Requested,
            file: None,
            summary: None,
            diff: None,
            index: None,
        });
        self.heaps_cache = None;
        id
    }

    /// Applies `f` to the record; false for an unknown id.
    fn heap_edit(&mut self, id: HeapId, f: impl FnOnce(&mut HeapRecord)) -> bool {
        match self.heaps.iter_mut().find(|h| h.id == id) {
            Some(h) => {
                f(h);
                self.heaps_cache = None;
                true
            }
            None => false,
        }
    }

    pub(crate) fn set_stage(&mut self, id: HeapId, stage: HeapStage) -> bool {
        self.heap_edit(id, |h| h.stage = stage)
    }

    pub(crate) fn set_received(
        &mut self,
        id: HeapId,
        time: SessionTime,
        bytes: u64,
        file: Arc<HeapFile>,
    ) -> bool {
        self.heap_edit(id, |h| {
            h.time = Some(time);
            h.bytes = bytes;
            h.file = Some(file);
        })
    }

    /// Makes `id` the held snapshot: every other record gives up its index.
    pub(crate) fn set_analysed(
        &mut self,
        id: HeapId,
        summary: Arc<HeapSummary>,
        index: Arc<HeapIndex>,
    ) -> bool {
        if !self.heaps.iter().any(|h| h.id == id) {
            return false;
        }
        for h in &mut self.heaps {
            if h.id == id {
                h.summary = Some(summary.clone());
                h.index = Some(index.clone());
            } else {
                h.index = None;
            }
        }
        self.held = Some(id);
        self.heaps_cache = None;
        true
    }

    pub(crate) fn set_diff(&mut self, id: HeapId, diff: Option<Arc<HeapDiff>>) -> bool {
        self.heap_edit(id, |h| h.diff = diff)
    }

    pub(crate) fn set_baseline(&mut self, id: Option<HeapId>) {
        self.baseline = id;
    }

    pub(crate) fn remove_heap(&mut self, id: HeapId) -> bool {
        let before = self.heaps.len();
        self.heaps.retain(|h| h.id != id);
        if self.heaps.len() == before {
            return false;
        }
        if self.held == Some(id) {
            self.held = None;
        }
        if self.baseline == Some(id) {
            self.baseline = None;
        }
        self.heaps_cache = None;
        true
    }

    pub(crate) fn snapshot(&mut self) -> MemorySnapshot {
        let profiles = self
            .profiles_cache
            .get_or_insert_with(|| self.profiles.clone().into())
            .clone();
        let heaps = self
            .heaps_cache
            .get_or_insert_with(|| self.heaps.clone().into())
            .clone();
        MemorySnapshot {
            samples: self.samples.snapshot(),
            gcs: self.gcs.snapshot(),
            gc_lod: self.gc_lod.snapshot(),
            classes: self.classes.snapshot(),
            profiles,
            heaps,
            held: self.held,
            baseline: self.baseline,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemorySnapshot {
    samples: ChunkedSnapshot<MemorySample>,
    gcs: ChunkedSnapshot<GcRecord>,
    gc_lod: LodSnapshot,
    classes: ChunkedSnapshot<Arc<AllocClass>>,
    profiles: Arc<[Arc<AllocProfile>]>,
    heaps: Arc<[HeapRecord]>,
    held: Option<HeapId>,
    baseline: Option<HeapId>,
}

/// The indices of the items whose time lies in `r` (inclusive), in a time-sorted series.
fn range_by_time<T: Clone>(
    items: &ChunkedSnapshot<T>,
    r: TimeRange,
    time: impl Fn(&T) -> SessionTime,
) -> Range<usize> {
    let part = |pred: &dyn Fn(SessionTime) -> bool| {
        let (mut lo, mut hi) = (0, items.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(time(items.get(mid).expect("within the series"))) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let lo = part(&|t| t < r.start);
    let hi = part(&|t| t <= r.end).max(lo);
    lo..hi
}

impl MemorySnapshot {
    pub fn samples(&self) -> &ChunkedSnapshot<MemorySample> {
        &self.samples
    }

    /// The samples whose time lies in `r`.
    pub fn sample_range(&self, r: TimeRange) -> Range<usize> {
        range_by_time(&self.samples, r, |s| s.time)
    }

    pub fn gcs(&self) -> &ChunkedSnapshot<GcRecord> {
        &self.gcs
    }

    /// The GC records whose end lies in `r`.
    pub fn gc_range(&self, r: TimeRange) -> Range<usize> {
        range_by_time(&self.gcs, r, |g| g.time)
    }

    /// `insert(end, dur_us)` per GC record.
    pub fn gc_lod(&self) -> &LodSnapshot {
        &self.gc_lod
    }

    pub fn alloc_class(&self, id: AllocClassId) -> &AllocClass {
        self.classes
            .get(id.0 as usize)
            .expect("class id from this session")
    }

    pub fn alloc_class_count(&self) -> usize {
        self.classes.len()
    }

    /// Oldest first.
    pub fn profiles(&self) -> &[Arc<AllocProfile>] {
        &self.profiles
    }

    /// The profile closest in time to `t`; the earlier one on a tie.
    pub fn nearest_profile(&self, t: SessionTime) -> Option<usize> {
        let after = self.profiles.partition_point(|p| p.time < t);
        let before = after.checked_sub(1);
        match (before, self.profiles.get(after)) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(_)) => Some(after),
            (Some(b), Some(a)) => {
                let db = t.0 - self.profiles[b].time.0;
                let da = a.time.0 - t.0;
                Some(if db <= da { b } else { after })
            }
        }
    }

    pub fn heaps(&self) -> &[HeapRecord] {
        &self.heaps
    }

    pub fn heap(&self, id: HeapId) -> Option<&HeapRecord> {
        self.heaps.iter().find(|h| h.id == id)
    }

    /// The snapshot whose index is held.
    pub fn held(&self) -> Option<HeapId> {
        self.held
    }

    pub fn baseline(&self) -> Option<HeapId> {
        self.baseline
    }

    /// The newest record that is still being taken or analysed.
    pub fn busy(&self) -> Option<HeapId> {
        self.heaps
            .iter()
            .rev()
            .find(|h| h.stage.is_busy())
            .map(|h| h.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::testkit::{temp_heap_file, tiny_index};
    use crate::heap::{Check, LeakChecks};
    use crate::run::Mode;
    use crate::session::SessionWriter;
    use crate::synth;

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000 * MS;

    fn writer() -> (SessionWriter, RunId) {
        synth::writer(1, 0, 0, 0)
    }

    fn sample(ms: u64, used: u64, rss: Option<u64>) -> MemorySample {
        MemorySample {
            time: SessionTime(ms * MS),
            heap_used: used,
            heap_capacity: 100,
            external: 5,
            rss,
        }
    }

    fn gc(ms: u64, dur: Option<u32>, kind: GcKind) -> GcRecord {
        GcRecord {
            time: SessionTime(ms * MS),
            dur_us: dur,
            kind,
            reason: GcReason::NewSpace,
            heap_used: 10,
            heap_capacity: 20,
            external: 1,
        }
    }

    fn summary() -> Arc<HeapSummary> {
        Arc::new(HeapSummary {
            objects: 6,
            reachable: 6,
            unreachable_bytes: 0,
            reachable_bytes: 100,
            header_shallow: 0,
            header_capacity: 0,
            header_external: 0,
            mode: Mode::Profile,
            root_package: None,
            classes: Arc::from(Vec::new()),
            findings: Arc::from(Vec::new()),
            findings_total: 0,
            checks: LeakChecks {
                states: Check::Done,
                elements: Check::Done,
                notifiers: Check::NotFound,
            },
            http: None,
            tracked: Arc::from(Vec::new()),
        })
    }

    #[test]
    fn unchanged_samples_are_not_stored() {
        let (mut w, run) = writer();
        assert!(w.push_memory_sample(run, sample(0, 50, Some(1_000_000))));
        w.publish();
        assert!(!w.push_memory_sample(run, sample(500, 50, Some(1_000_000))));
        assert!(!w.is_dirty(), "a dropped sample does not dirty the writer");
        assert!(w.push_memory_sample(run, sample(1_000, 51, Some(1_000_000))));
        assert!(w.is_dirty());
        assert_eq!(w.publish().memory.samples().len(), 2);
    }

    #[test]
    fn rss_jitter_below_the_step_is_not_stored() {
        let (mut w, run) = writer();
        let base = 100 * 1024 * 1024;
        assert!(w.push_memory_sample(run, sample(0, 50, Some(base))));
        assert!(!w.push_memory_sample(run, sample(500, 50, Some(base + RSS_STEP - 1))));
        assert!(!w.push_memory_sample(run, sample(1_000, 50, Some(base - RSS_STEP + 1))));
        assert!(w.push_memory_sample(run, sample(1_500, 50, Some(base + RSS_STEP))));
        // A sample that gains or loses its RSS reading is a change.
        assert!(w.push_memory_sample(run, sample(2_000, 50, None)));
        assert!(!w.push_memory_sample(run, sample(2_500, 50, None)));
        assert_eq!(w.publish().memory.samples().len(), 3);
    }

    #[test]
    fn a_flat_series_still_stores_every_5_s() {
        let (mut w, run) = writer();
        for i in 0..25 {
            w.push_memory_sample(run, sample(i * 500, 50, Some(1_000_000)));
        }
        let s = w.publish();
        let times: Vec<u64> = s
            .memory
            .samples()
            .chunks()
            .flatten()
            .map(|m| m.time.0 / S)
            .collect();
        assert_eq!(times, vec![0, 5, 10]);
    }

    #[test]
    fn samples_before_the_last_one_are_dropped_and_ranges_find_the_rest() {
        let (mut w, run) = writer();
        for i in 0..10u64 {
            assert!(w.push_memory_sample(run, sample(i * 500, 50 + i, None)));
        }
        assert!(!w.push_memory_sample(run, sample(100, 999, None)));
        let s = w.publish();
        let r = |a: u64, b: u64| TimeRange::new(SessionTime(a * MS), SessionTime(b * MS));
        assert_eq!(s.memory.sample_range(r(500, 1_500)), 1..4);
        assert_eq!(s.memory.sample_range(r(501, 999)), 2..2);
        assert_eq!(s.memory.sample_range(r(0, 100_000)), 0..10);
    }

    #[test]
    fn gc_records_stay_sorted_and_fill_the_lod() {
        let (mut w, run) = writer();
        w.push_gc(run, gc(100, Some(700), GcKind::Scavenge));
        w.push_gc(run, gc(300, None, GcKind::MarkSweep));
        w.push_gc(run, gc(200, Some(9), GcKind::Scavenge));
        w.push_gc(run, gc(300, Some(50), GcKind::Scavenge));
        let s = w.publish();
        let times: Vec<u64> = s
            .memory
            .gcs()
            .chunks()
            .flatten()
            .map(|g| g.time.0 / MS)
            .collect();
        assert_eq!(times, vec![100, 300, 300], "the earlier record is dropped");
        let lod = s.memory.gc_lod().level(0).unwrap();
        let (mut count, mut max) = (0, 0);
        for b in lod.chunks().flatten() {
            count += b.count;
            max = max.max(b.max_us);
        }
        assert_eq!((count, max), (3, 700), "a missing duration inserts 0");
        let r = TimeRange::new(SessionTime(150 * MS), SessionTime(300 * MS));
        assert_eq!(s.memory.gc_range(r), 1..3);
        assert!(GcKind::from_vm("MarkCompact").is_major());
        assert!(!GcKind::from_vm("Scavenge").is_major());
        assert_eq!(GcKind::from_vm("StartCMark"), GcKind::StartMark);
        assert_eq!(GcKind::from_vm("nope"), GcKind::Other);
        assert_eq!(GcReason::from_vm("catch-up"), GcReason::CatchUp);
        assert_eq!(
            GcReason::from_vm("store buffer").label(),
            "Store buffer full"
        );
    }

    #[test]
    fn alloc_classes_intern_per_run_and_isolate() {
        let (mut w, run) = writer();
        let run2 = RunId(run.0 + 1);
        let a = w.intern_alloc_class(
            run,
            "isolates/1",
            "classes/5",
            "Foo",
            Some("package:a/a.dart"),
            Some("a.dart:39"),
        );
        let again = w.intern_alloc_class(run, "isolates/1", "classes/5", "Foo", None, None);
        let other_id = w.intern_alloc_class(run, "isolates/1", "classes/6", "Bar", None, None);
        let other_isolate = w.intern_alloc_class(run, "isolates/2", "classes/5", "Foo", None, None);
        let other_run = w.intern_alloc_class(run2, "isolates/1", "classes/5", "Foo", None, None);
        assert_eq!(a, again);
        let ids = [a, other_id, other_isolate, other_run];
        for (i, x) in ids.iter().enumerate() {
            for y in &ids[i + 1..] {
                assert_ne!(x, y);
            }
        }
        let s = w.publish();
        let c = s.memory.alloc_class(a);
        assert_eq!(
            (&*c.name, c.run, c.library.as_deref(), c.location.as_deref()),
            ("Foo", run, Some("package:a/a.dart"), Some("a.dart:39"))
        );
        assert_eq!(s.memory.alloc_class(other_run).run, run2);
        w.publish();
        w.intern_alloc_class(run, "isolates/1", "classes/5", "Foo", None, None);
        assert!(!w.is_dirty(), "a known class changes nothing");
    }

    fn profile(run: RunId, ms: u64) -> AllocProfile {
        AllocProfile {
            time: SessionTime(ms * MS),
            run,
            isolate: "isolates/1".into(),
            gc: false,
            heap_used: ms,
            rows: Arc::from(Vec::new()),
        }
    }

    #[test]
    fn nearest_profile_cases() {
        let (mut w, run) = writer();
        assert_eq!(w.publish().memory.nearest_profile(SessionTime(5)), None);
        w.push_alloc_profile(profile(run, 100));
        let near = |w: &mut SessionWriter, ms: u64| {
            w.publish().memory.nearest_profile(SessionTime(ms * MS))
        };
        assert_eq!(near(&mut w, 0), Some(0));
        assert_eq!(near(&mut w, 100), Some(0));
        assert_eq!(near(&mut w, 9_999), Some(0));
        w.push_alloc_profile(profile(run, 300));
        w.push_alloc_profile(profile(run, 200));
        w.push_alloc_profile(profile(run, 1_000));
        // Stored by time whatever the push order.
        let times: Vec<u64> = w
            .publish()
            .memory
            .profiles()
            .iter()
            .map(|p| p.heap_used)
            .collect();
        assert_eq!(times, vec![100, 200, 300, 1_000]);
        assert_eq!(near(&mut w, 140), Some(0));
        assert_eq!(near(&mut w, 160), Some(1));
        assert_eq!(
            near(&mut w, 150),
            Some(0),
            "a tie takes the earlier profile"
        );
        assert_eq!(near(&mut w, 299), Some(2));
        assert_eq!(near(&mut w, 800), Some(3));
        assert_eq!(near(&mut w, 600), Some(2));
    }

    #[test]
    fn profiles_are_capped() {
        let (mut w, run) = writer();
        for i in 0..MAX_PROFILES as u64 + 5 {
            w.push_alloc_profile(profile(run, i));
        }
        let s = w.publish();
        assert_eq!(s.memory.profiles().len(), MAX_PROFILES);
        assert_eq!(
            s.memory.profiles()[0].heap_used,
            5,
            "the oldest are dropped"
        );
    }

    #[test]
    fn two_hundred_profiles_stay_bounded() {
        const CLASSES: usize = 6_666;
        const PROFILES: usize = 200;
        assert!(std::mem::size_of::<AllocRow>() <= 24);
        let (mut w, run) = writer();
        for p in 0..PROFILES as u64 {
            let rows: Vec<AllocRow> = (0..CLASSES)
                .map(|c| AllocRow {
                    class: w.intern_alloc_class(
                        run,
                        "isolates/1",
                        &format!("classes/{c}"),
                        &format!("Class{c}"),
                        None,
                        None,
                    ),
                    instances: p + c as u64,
                    bytes: 32 * (p + c as u64),
                })
                .collect();
            w.push_alloc_profile(AllocProfile {
                rows: rows.into(),
                ..profile(run, p * 1_000)
            });
        }
        let m = w.publish().memory.clone();
        assert_eq!(m.profiles().len(), PROFILES);
        let classes = m.alloc_class_count();
        assert_eq!(classes, CLASSES, "classes are interned once, not per row");
        let row_bytes: usize = m
            .profiles()
            .iter()
            .map(|p| p.rows.len() * std::mem::size_of::<AllocRow>())
            .sum();
        assert!(row_bytes <= 24 * CLASSES * PROFILES, "{row_bytes} bytes");
    }

    #[test]
    fn heap_record_lifecycle() {
        let (mut w, run) = writer();
        let id = w.heap_requested(run, "isolates/1", SessionTime(7));
        let s = w.publish();
        let rec = s.memory.heap(id).unwrap();
        assert_eq!(
            (rec.stage.clone(), rec.requested),
            (HeapStage::Requested, SessionTime(7))
        );
        assert_eq!(s.memory.busy(), Some(id));
        assert_eq!(s.memory.held(), None);

        w.set_heap_stage(
            id,
            HeapStage::Receiving {
                bytes: 10,
                chunks: 1,
            },
        );
        let file = temp_heap_file();
        let path = file.path().to_path_buf();
        w.set_heap_received(id, SessionTime(9), 1234, file);
        let s = w.publish();
        let rec = s.memory.heap(id).unwrap();
        assert_eq!((rec.time, rec.bytes), (Some(SessionTime(9)), 1234));
        assert!(rec.file.is_some() && rec.index.is_none());
        assert_eq!(s.memory.busy(), Some(id));

        let index = Arc::new(tiny_index(temp_heap_file()));
        w.set_heap_analysed(id, summary(), index);
        w.set_heap_stage(id, HeapStage::Ready);
        w.set_heap_baseline(Some(id));
        let s = w.publish();
        let rec = s.memory.heap(id).unwrap();
        assert!(rec.summary.is_some() && rec.index.is_some());
        assert_eq!(
            (s.memory.busy(), s.memory.held(), s.memory.baseline()),
            (None, Some(id), Some(id))
        );
        assert!(path.exists());

        w.remove_heap(id);
        let s = w.publish();
        assert!(s.memory.heaps().is_empty());
        assert_eq!((s.memory.held(), s.memory.baseline()), (None, None));
        assert!(s.memory.heap(id).is_none());
        let next = w.heap_requested(run, "isolates/1", SessionTime(8));
        assert_ne!(next, id, "ids are not reused");
    }

    #[test]
    fn only_one_index_is_held() {
        let (mut w, run) = writer();
        let first = w.heap_requested(run, "isolates/1", SessionTime(0));
        let a = Arc::new(tiny_index(temp_heap_file()));
        let weak = Arc::downgrade(&a);
        w.set_heap_analysed(first, summary(), a);
        w.publish();
        assert!(weak.upgrade().is_some());
        let second = w.heap_requested(run, "isolates/1", SessionTime(1));
        w.set_heap_analysed(second, summary(), Arc::new(tiny_index(temp_heap_file())));
        w.publish();
        assert!(weak.upgrade().is_none(), "the first index is freed");
        let s = w.publish();
        assert_eq!(s.memory.held(), Some(second));
        assert!(s.memory.heap(first).unwrap().index.is_none());
        assert!(
            s.memory.heap(first).unwrap().summary.is_some(),
            "its summary stays"
        );

        for i in 2..50 {
            let id = w.heap_requested(run, "isolates/1", SessionTime(i));
            w.set_heap_analysed(id, summary(), Arc::new(tiny_index(temp_heap_file())));
        }
        let s = w.publish();
        assert_eq!(s.memory.heaps().len(), 50);
        assert_eq!(
            s.memory
                .heaps()
                .iter()
                .filter(|h| h.index.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn removing_a_heap_deletes_its_file() {
        let (mut w, run) = writer();
        let id = w.heap_requested(run, "isolates/1", SessionTime(0));
        let file = temp_heap_file();
        let path = file.path().to_path_buf();
        w.set_heap_received(id, SessionTime(1), 1, file);
        let held_by_view = w.publish();
        w.remove_heap(id);
        w.publish();
        assert!(
            path.exists(),
            "a snapshot a view still holds keeps the file"
        );
        drop(held_by_view);
        assert!(!path.exists());
    }

    #[test]
    fn dropping_every_snapshot_deletes_files() {
        let (mut w, run) = writer();
        let mut paths = Vec::new();
        for i in 0..3 {
            let id = w.heap_requested(run, "isolates/1", SessionTime(i));
            let file = temp_heap_file();
            paths.push(file.path().to_path_buf());
            w.set_heap_received(id, SessionTime(i), 1, file);
        }
        let s = w.publish();
        assert!(paths.iter().all(|p| p.exists()));
        drop(w);
        assert!(
            paths.iter().all(|p| p.exists()),
            "a view's snapshot still holds them"
        );
        drop(s);
        assert!(paths.iter().all(|p| !p.exists()));
    }

    #[test]
    fn dropping_a_file_that_is_gone_is_quiet() {
        let f = temp_heap_file();
        std::fs::remove_file(f.path()).unwrap();
        drop(f);
    }

    /// Bytes the snapshot's memory series hold, from their chunk capacities.
    fn chunk_bytes<T: Clone>(c: &ChunkedSnapshot<T>) -> usize {
        c.len().div_ceil(c.chunk_size()) * c.chunk_size() * std::mem::size_of::<T>()
    }

    #[test]
    fn memory_store_stays_small() {
        let (mut w, run) = writer();
        // A GC every 10 ms for 30 minutes, and a sample every 250 ms.
        for i in 0..180_000u64 {
            w.push_gc(run, gc(i * 10, Some(500), GcKind::Scavenge));
        }
        for i in 0..7_200u64 {
            assert!(w.push_memory_sample(run, sample(i * 250, i, Some(i * 1_000_000))));
        }
        let m = w.publish().memory.clone();
        assert_eq!((m.gcs().len(), m.samples().len()), (180_000, 7_200));
        let lod: usize = (0..m.gc_lod().levels())
            .map(|k| chunk_bytes(m.gc_lod().level(k).unwrap()))
            .sum();
        let total = chunk_bytes(m.gcs()) + chunk_bytes(m.samples()) + lod;
        assert!(total < 16 * 1024 * 1024, "{total} bytes");
    }
}
