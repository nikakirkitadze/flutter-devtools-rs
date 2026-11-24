//! The single writer and the immutable snapshots views read.

use crate::clock::RunClock;
use crate::cpu::{CpuSample, CpuSnapshot, CpuStatus, CpuStore, FunctionId, NodeId};
use crate::frames::{
    DEFAULT_BUDGET_US, FrameInput, FrameStore, FramesSnapshot, RasterPhase, UiPhases, budget_us,
};
use crate::heap::{HeapDiff, HeapFile, HeapId, HeapIndex, HeapRecord, HeapStage, HeapSummary};
use crate::logs::{LOG_CHUNK, LogInput, LogStore, LogsSnapshot};
use crate::memory::{
    AllocClassId, AllocProfile, GcRecord, MemorySample, MemorySnapshot, MemoryStore,
};
use crate::net::{
    BodyKind, BodyLimits, BodyState, HttpUpdate, NetSnapshot, NetStore, RequestUpsert, SocketUpdate,
};
use crate::run::{AppInfo, RunInfo};
use crate::time::{EntryId, RunId, SessionTime};
use crate::timeline::{NameId, Slice, TimelineSnapshot, TimelineStore, TrackId, TrackKind};
use std::sync::Arc;

/// An immutable, cheaply clonable view of the session at one generation.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Names the session this snapshot belongs to: every writer gets a new one, so a change
    /// means the stored data was thrown away and replaced. 0 for the empty snapshot of no session.
    pub session: u64,
    pub generation: u64,
    pub origin_host_us: i64,
    pub runs: Arc<[RunInfo]>,
    pub logs: LogsSnapshot,
    pub net: NetSnapshot,
    pub frames: FramesSnapshot,
    pub timeline: TimelineSnapshot,
    pub cpu: CpuSnapshot,
    pub memory: MemorySnapshot,
    latest: SessionTime,
}

impl Snapshot {
    /// A valid generation-0 snapshot with no runs.
    pub fn empty() -> Self {
        Self {
            session: 0,
            generation: 0,
            origin_host_us: 0,
            runs: Arc::from(Vec::new()),
            logs: LogsSnapshot::default(),
            net: NetSnapshot::default(),
            frames: FramesSnapshot::default(),
            timeline: TimelineSnapshot::default(),
            cpu: CpuSnapshot::default(),
            memory: MemorySnapshot::default(),
            latest: SessionTime(0),
        }
    }

    pub fn run(&self, id: RunId) -> Option<&RunInfo> {
        self.runs.iter().find(|r| r.id == id)
    }

    /// The frame budget of a run; the default for an unknown run.
    pub fn frame_budget_us(&self, run: RunId) -> u32 {
        self.run(run)
            .map_or(DEFAULT_BUDGET_US, |r| budget_us(r.refresh_hz))
    }

    /// The last run's frame budget, or the default with no runs.
    pub fn latest_budget_us(&self) -> u32 {
        self.runs
            .last()
            .map_or(DEFAULT_BUDGET_US, |r| budget_us(r.refresh_hz))
    }

    /// The latest stored event time or run boundary.
    pub fn latest_time(&self) -> SessionTime {
        self.latest
    }
}

pub struct SessionWriter {
    session: u64,
    origin_host_us: i64,
    runs: Vec<RunInfo>,
    logs: LogStore,
    net: NetStore,
    frames: FrameStore,
    timeline: TimelineStore,
    cpu: CpuStore,
    memory: MemoryStore,
    latest: SessionTime,
    generation: u64,
    dirty: bool,
    last: Arc<Snapshot>,
}

impl SessionWriter {
    pub fn new(origin_host_us: i64) -> Self {
        Self::with_log_chunk(origin_host_us, LOG_CHUNK)
    }

    #[cfg(test)]
    pub(crate) fn new_with_log_chunk(origin_host_us: i64, chunk: usize) -> Self {
        Self::with_log_chunk(origin_host_us, chunk)
    }

    /// A writer whose stores seal chunks of `chunk` records, so tests can reach many chunks
    /// with few records.
    pub fn with_log_chunk(origin_host_us: i64, chunk: usize) -> Self {
        static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let session = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut empty = Snapshot::empty();
        empty.origin_host_us = origin_host_us;
        empty.session = session;
        Self {
            session,
            origin_host_us,
            runs: Vec::new(),
            logs: LogStore::new(chunk),
            net: NetStore::new(chunk),
            frames: FrameStore::new(chunk),
            timeline: TimelineStore::new(),
            cpu: CpuStore::new(),
            memory: MemoryStore::new(),
            latest: SessionTime(0),
            generation: 0,
            dirty: false,
            last: Arc::new(empty),
        }
    }

    pub fn origin_host_us(&self) -> i64 {
        self.origin_host_us
    }

    pub fn start_run(&mut self, app: AppInfo, clock: RunClock, at: SessionTime) -> RunId {
        let id = RunId(self.runs.len() as u32 + 1);
        self.runs.push(RunInfo {
            id,
            app,
            started: at,
            ended: None,
            clock,
            refresh_hz: None,
            tracing: false,
            tracing_periods: Vec::new(),
        });
        self.touch(at);
        id
    }

    pub fn push_log(&mut self, input: LogInput) -> EntryId {
        let at = input.time;
        let id = self.logs.push(input);
        self.touch(at);
        id
    }

    /// Stores a frame; its bucket in every level of detail is updated too. Returns `None`,
    /// storing nothing, when the frame does not start after the run's last stored frame.
    pub fn push_frame(&mut self, f: FrameInput) -> Option<EntryId> {
        let end = SessionTime(f.start.0.saturating_add(u64::from(f.elapsed_us) * 1_000));
        let start = f.start;
        let id = self.frames.push(f)?;
        self.touch(start);
        self.touch(end);
        Some(id)
    }

    /// Sets a frame's UI phases, or parks them until the frame arrives.
    pub fn set_frame_ui(&mut self, run: RunId, number: u64, ui: UiPhases) {
        self.frames.set_ui(run, number, ui);
        self.dirty = true;
    }

    /// Sets a frame's raster phase, or parks it until the frame arrives.
    pub fn set_frame_raster(&mut self, run: RunId, number: u64, raster: RasterPhase) {
        self.frames.set_raster(run, number, raster);
        self.dirty = true;
    }

    /// Records the display's refresh rate; the run's slow-frame index is rebuilt.
    pub fn set_refresh_rate(&mut self, run: RunId, hz: f32) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
            r.refresh_hz = Some(hz);
            self.frames.set_budget(run, budget_us(Some(hz)));
            self.dirty = true;
        }
    }

    /// Records whether widget-build tracing is on in the run's app, and that it turned on or off
    /// `at` (see [`RunInfo::tracing_at`]). Times never go back: a stamp before the last one is
    /// taken as the last one.
    pub fn set_tracing(&mut self, run: RunId, on: bool, at: SessionTime) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run)
            && r.tracing != on
        {
            r.tracing = on;
            let last = r.tracing_periods.last().map(|&(on, off)| off.unwrap_or(on));
            let at = last.map_or(at, |last| at.max(last));
            if on {
                r.tracing_periods.push((at, None));
            } else if let Some(p) = r.tracing_periods.last_mut() {
                p.1 = Some(at);
            }
            self.dirty = true;
        }
    }

    /// Interns a slice name.
    pub fn intern_name(&mut self, s: &str) -> NameId {
        let before = self.timeline_names();
        let id = self.timeline.intern(s);
        if self.timeline_names() != before {
            self.dirty = true;
        }
        id
    }

    fn timeline_names(&self) -> usize {
        self.timeline.name_count()
    }

    /// The track for `(run, kind, key)`, created on first use. A non-empty name replaces an
    /// empty one.
    pub fn timeline_track(&mut self, run: RunId, kind: TrackKind, key: u64, name: &str) -> TrackId {
        let id = self.timeline.track(run, kind, key, name);
        self.dirty |= self.timeline.tracks_changed();
        id
    }

    pub fn set_track_order(&mut self, track: TrackId, order: u32) {
        self.timeline.set_order(track, order);
        self.dirty = true;
    }

    /// Stores a slice in the lane of `track` at `depth`. Returns false, storing nothing and
    /// counting the drop, when the depth is out of range, the slice would overlap the lane's
    /// last slice, or the widget-build cap is reached.
    pub fn push_slice(&mut self, track: TrackId, depth: u16, s: Slice, widget_build: bool) -> bool {
        self.dirty = true;
        let stored = self.timeline.push(track, depth, s, widget_build);
        if stored {
            self.touch(s.end());
        }
        stored
    }

    /// Interns a function by `(name, library)`; no library makes it native.
    pub fn intern_function(&mut self, name: &str, library: Option<&str>) -> FunctionId {
        let (id, new) = self.cpu.intern_function(name, library);
        self.dirty |= new;
        id
    }

    /// The trie node of a stack given root first. Stacks deeper than
    /// [`crate::cpu::MAX_STACK_DEPTH`] lose their root end.
    pub fn intern_stack(&mut self, root_first: &[FunctionId]) -> NodeId {
        let before = self.cpu.node_count();
        let id = self.cpu.intern_stack(root_first);
        self.dirty |= self.cpu.node_count() != before;
        id
    }

    /// Stores a batch of samples sorted by time, dropping those at or before the last stored
    /// sample's time.
    pub fn push_cpu_samples(&mut self, _run: RunId, batch: Vec<CpuSample>) {
        if let Some(newest) = self.cpu.push_samples(batch) {
            self.touch(newest);
        }
    }

    /// The end of the time the run's CPU samples cover.
    pub fn set_cpu_coverage(&mut self, run: RunId, end: SessionTime) {
        self.cpu.set_coverage(run, end);
        self.dirty = true;
    }

    pub fn set_cpu_status(&mut self, run: RunId, status: CpuStatus) {
        self.cpu.set_status(run, status);
        self.dirty = true;
    }

    /// The run's sample period in microseconds, from the VM.
    pub fn set_cpu_period(&mut self, run: RunId, us: u32) {
        self.cpu.set_period(run, us);
        self.dirty = true;
    }

    /// Stores a memory sample unless it changes nothing: the heap values equal the last stored
    /// sample's, RSS moved by less than [`crate::memory::RSS_STEP`], and that sample is less than
    /// [`crate::memory::SAMPLE_MAX_GAP_NS`] old. A sample older than the last stored one is
    /// dropped too. Returns whether it was stored.
    pub fn push_memory_sample(&mut self, run: RunId, s: MemorySample) -> bool {
        let stored = self.memory.push_sample(run, s);
        if stored {
            self.touch(s.time);
        }
        stored
    }

    /// Stores a GC record, dropping one earlier than the last. Its duration (0 when unknown)
    /// goes into the GC level of detail.
    pub fn push_gc(&mut self, _run: RunId, g: GcRecord) {
        if self.memory.push_gc(g) {
            self.touch(g.time);
        }
    }

    /// Interns an allocation class by `(run, isolate, vm_id)`.
    pub fn intern_alloc_class(
        &mut self,
        run: RunId,
        isolate: &str,
        vm_id: &str,
        name: &str,
        library: Option<&str>,
        location: Option<&str>,
    ) -> AllocClassId {
        let (id, new) = self
            .memory
            .intern_class(run, isolate, vm_id, name, library, location);
        self.dirty |= new;
        id
    }

    /// Stores an allocation profile in time order; beyond [`crate::memory::MAX_PROFILES`] the
    /// oldest is dropped.
    pub fn push_alloc_profile(&mut self, p: AllocProfile) {
        let at = p.time;
        self.memory.push_profile(p);
        self.touch(at);
    }

    /// The heap-snapshot records, oldest first.
    pub fn heap_records(&self) -> &[HeapRecord] {
        self.memory.records()
    }

    pub fn heap_record(&self, id: HeapId) -> Option<&HeapRecord> {
        self.memory.records().iter().find(|h| h.id == id)
    }

    /// The snapshot whose index is held.
    pub fn held_heap(&self) -> Option<HeapId> {
        self.memory.held_id()
    }

    /// The baseline the user chose, if any.
    pub fn heap_baseline(&self) -> Option<HeapId> {
        self.memory.baseline_id()
    }

    /// The newest snapshot still being taken or analysed.
    pub fn busy_heap(&self) -> Option<HeapId> {
        self.memory
            .records()
            .iter()
            .rev()
            .find(|h| h.stage.is_busy())
            .map(|h| h.id)
    }

    pub fn run_info(&self, id: RunId) -> Option<&RunInfo> {
        self.runs.iter().find(|r| r.id == id)
    }

    /// Starts the record of a heap snapshot asked for at `at`.
    pub fn heap_requested(&mut self, run: RunId, isolate: &str, at: SessionTime) -> HeapId {
        let id = self.memory.heap_requested(run, isolate, at);
        self.touch(at);
        id
    }

    pub fn set_heap_stage(&mut self, id: HeapId, stage: HeapStage) {
        self.dirty |= self.memory.set_stage(id, stage);
    }

    /// The snapshot's first chunk arrived at `time`; `file` holds its `bytes`.
    pub fn set_heap_received(
        &mut self,
        id: HeapId,
        time: SessionTime,
        bytes: u64,
        file: Arc<HeapFile>,
    ) {
        if self.memory.set_received(id, time, bytes, file) {
            self.touch(time);
        }
    }

    /// Stores the analysis of `id` and makes it the held snapshot: every other record's index is
    /// dropped, so one index is held.
    pub fn set_heap_analysed(
        &mut self,
        id: HeapId,
        summary: Arc<HeapSummary>,
        index: Arc<HeapIndex>,
    ) {
        self.dirty |= self.memory.set_analysed(id, summary, index);
    }

    /// Drops the record's hold on its file, for a snapshot that will never be analysed.
    pub fn drop_heap_file(&mut self, id: HeapId) {
        self.dirty |= self.memory.drop_file(id);
    }

    pub fn set_heap_diff(&mut self, id: HeapId, diff: Option<Arc<HeapDiff>>) {
        self.dirty |= self.memory.set_diff(id, diff);
    }

    pub fn set_heap_baseline(&mut self, id: Option<HeapId>) {
        self.memory.set_baseline(id);
        self.dirty = true;
    }

    /// Removes the record; its file is deleted once no snapshot holds it.
    pub fn remove_heap(&mut self, id: HeapId) {
        self.dirty |= self.memory.remove_heap(id);
    }

    #[cfg(test)]
    pub(crate) fn with_build_cap(mut self, cap: u64) -> Self {
        self.timeline.set_build_cap(cap);
        self
    }

    /// Inserts or updates a request, keyed by `(run, isolate, vm_id)`.
    pub fn upsert_request(&mut self, u: HttpUpdate) -> RequestUpsert {
        for t in [Some(u.start), u.request_sent, u.response_start, u.end]
            .into_iter()
            .flatten()
            .chain(u.events.iter().map(|e| e.1))
        {
            self.touch(t);
        }
        self.net.upsert_request(u)
    }

    /// Closes a request that never finished (its app stopped): it ends at `at` with `error`.
    pub fn abort_request(&mut self, id: EntryId, at: SessionTime, error: &str) {
        self.net.abort_request(id, at, error);
        self.touch(at);
    }

    pub fn set_body_state(&mut self, id: EntryId, kind: BodyKind, state: BodyState) {
        self.net.set_body_state(id, kind, state);
        self.dirty = true;
    }

    /// Stores a body under the limits, evicting the oldest bodies first.
    pub fn store_body(&mut self, id: EntryId, kind: BodyKind, bytes: Vec<u8>) {
        self.net.store_body(id, kind, bytes);
        self.dirty = true;
    }

    /// Stores a whole body, ignoring `per_body`; it still counts toward `total`.
    pub fn store_full_body(&mut self, id: EntryId, kind: BodyKind, bytes: Vec<u8>) {
        self.net.store_full_body(id, kind, bytes);
        self.dirty = true;
    }

    pub fn set_body_limits(&mut self, limits: BodyLimits) {
        self.net.set_limits(limits);
        self.dirty = true;
    }

    /// Inserts or updates a socket, keyed by `(run, isolate, vm_id)`.
    pub fn upsert_socket(&mut self, u: SocketUpdate) -> EntryId {
        for t in [Some(u.start), u.end, u.last_read, u.last_write]
            .into_iter()
            .flatten()
        {
            self.touch(t);
        }
        self.net.upsert_socket(u)
    }

    pub fn update_run_app(&mut self, run: RunId, f: impl FnOnce(&mut AppInfo)) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
            f(&mut r.app);
            self.dirty = true;
        }
    }

    pub fn end_run(&mut self, run: RunId, at: SessionTime) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
            r.ended = Some(at);
            self.touch(at);
        }
    }

    /// Marks an ended run as running again, for a reconnect to the same VM.
    pub fn reopen_run(&mut self, run: RunId) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
            r.ended = None;
            self.dirty = true;
        }
    }

    /// Records that something was stored at `t`.
    pub(crate) fn touch(&mut self, t: SessionTime) {
        self.latest = self.latest.max(t);
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the current snapshot; a new generation only when changed.
    pub fn publish(&mut self) -> Arc<Snapshot> {
        if self.dirty {
            self.generation += 1;
            self.dirty = false;
            self.last = Arc::new(Snapshot {
                session: self.session,
                generation: self.generation,
                origin_host_us: self.origin_host_us,
                runs: self.runs.clone().into(),
                logs: self.logs.snapshot(),
                net: self.net.snapshot(self.latest),
                frames: self.frames.snapshot(),
                timeline: self.timeline.snapshot(),
                cpu: self.cpu.snapshot(),
                memory: self.memory.snapshot(),
                latest: self.latest,
            });
        }
        self.last.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Calibration, RunClock};
    use crate::run::Mode;

    fn app() -> AppInfo {
        AppInfo {
            name: "demo".into(),
            device: None,
            mode: Mode::Debug,
            flutter_version: None,
            workspace: None,
            vm_uri: "ws://127.0.0.1:1/ws".into(),
            pid: None,
        }
    }

    fn clock() -> RunClock {
        RunClock::new(
            0,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 0,
            },
        )
    }

    #[test]
    fn publish_bumps_generation_only_when_dirty() {
        let mut w = SessionWriter::new(42);
        assert_eq!(w.publish().generation, 0);
        assert!(!w.is_dirty());
        w.start_run(app(), clock(), SessionTime(5));
        assert!(w.is_dirty());
        let a = w.publish();
        assert_eq!(a.generation, 1);
        assert_eq!(a.origin_host_us, 42);
        let b = w.publish();
        assert!(Arc::ptr_eq(&a, &b));
        w.update_run_app(RunId(1), |a| a.pid = Some(7));
        assert_eq!(w.publish().generation, 2);
    }

    #[test]
    fn runs_are_numbered_from_one_and_record_end() {
        let mut w = SessionWriter::new(0);
        let r1 = w.start_run(app(), clock(), SessionTime(10));
        w.end_run(r1, SessionTime(50));
        let r2 = w.start_run(app(), clock(), SessionTime(60));
        assert_eq!((r1, r2), (RunId(1), RunId(2)));
        let s = w.publish();
        assert_eq!(s.run(r1).unwrap().ended, Some(SessionTime(50)));
        assert_eq!(s.run(r2).unwrap().ended, None);
        assert_eq!(s.latest_time(), SessionTime(60));
        assert!(s.run(RunId(9)).is_none());
    }

    #[test]
    fn tracing_is_per_run_and_dirties_only_on_change() {
        let mut w = SessionWriter::new(0);
        let r = w.start_run(app(), clock(), SessionTime(10));
        w.publish();
        assert!(!w.publish().run(r).unwrap().tracing);
        w.set_tracing(r, false, SessionTime(20));
        assert!(!w.is_dirty());
        w.set_tracing(r, true, SessionTime(30));
        assert!(w.publish().run(r).unwrap().tracing);
    }

    /// Tracing is judged at a time: on from when it turned on, off from when it turned off.
    #[test]
    fn tracing_at_follows_on_off_and_on_again() {
        let mut w = SessionWriter::new(0);
        let r = w.start_run(app(), clock(), SessionTime(10));
        w.set_tracing(r, true, SessionTime(100));
        w.set_tracing(r, true, SessionTime(150));
        w.set_tracing(r, false, SessionTime(200));
        w.set_tracing(r, false, SessionTime(250));
        w.set_tracing(r, true, SessionTime(300));
        let s = w.publish();
        let run = s.run(r).unwrap();
        assert!(run.tracing);
        let at = |t: u64| run.tracing_at(SessionTime(t));
        assert!(!at(10) && !at(99), "before it turned on");
        assert!(
            at(100) && at(150) && at(199),
            "on (a repeated on changes nothing)"
        );
        assert!(!at(200) && !at(250) && !at(299), "off");
        assert!(at(300) && at(u64::MAX), "on again, still on");
    }

    #[test]
    fn a_run_can_be_reopened() {
        let mut w = SessionWriter::new(0);
        let r = w.start_run(app(), clock(), SessionTime(10));
        w.end_run(r, SessionTime(50));
        w.publish();
        w.reopen_run(r);
        assert!(w.is_dirty());
        let s = w.publish();
        assert_eq!(s.run(r).unwrap().ended, None);
        assert_eq!(s.runs.len(), 1);
        w.reopen_run(RunId(9));
        assert!(!w.is_dirty());
    }

    #[test]
    fn every_writer_names_its_own_session() {
        let mut a = SessionWriter::new(0);
        let mut b = SessionWriter::new(0);
        assert_ne!(a.publish().session, 0);
        assert_ne!(a.publish().session, b.publish().session);
        assert_eq!(Snapshot::empty().session, 0);
    }
}
