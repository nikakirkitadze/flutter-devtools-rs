//! Timeline capture: the poller task, the assembler's output mapped onto session time, and the
//! session-side write.
//!
//! The task owns an [`Assembler`]. It polls `getVMTimeline` in windows that follow one another
//! (or takes events the router forwards from the `Timeline` stream), feeds the assembler and
//! sends the result to the session task as one [`TimelineBatch`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fdt_store::SessionWriter;
use fdt_store::clock::RunClock;
use fdt_store::cpu::{CpuSample, CpuStatus, FunctionId};
use fdt_store::frames::{RasterPhase, UiPhases};
use fdt_store::time::{RunId, SessionTime};
use fdt_store::timeline::{Slice, SliceCat, TrackId, TrackKind};
use fdt_vm::flutter::PROFILE_WIDGET_BUILDS;
use fdt_vm::perfetto::CpuProfile;
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;
use fdt_vm::timeline::{Category, TraceEvent};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};

use super::assemble::{AssembledBatch, Assembler, AssemblerStats, TrackKeyRaw};
use super::{Ingest, SharedSink, Sink};

#[derive(Debug)]
pub(crate) struct BatchSlice {
    pub(crate) track: TrackKeyRaw,
    pub(crate) depth: u16,
    pub(crate) start: SessionTime,
    pub(crate) dur_ns: u64,
    pub(crate) name: Box<str>,
    pub(crate) cat: SliceCat,
    pub(crate) widget_build: bool,
}

/// What one poll (or one block of stream events) produced, in session time.
#[derive(Debug)]
pub(crate) struct TimelineBatch {
    pub(crate) run: RunId,
    /// `(kind, key, name, order)`: tracks that are new or changed.
    pub(crate) tracks: Vec<(TrackKind, u64, Box<str>, u32)>,
    pub(crate) slices: Vec<BatchSlice>,
    pub(crate) ui: Vec<(u64, UiPhases)>,
    pub(crate) raster: Vec<(u64, RasterPhase)>,
}

impl TimelineBatch {
    fn is_empty(&self) -> bool {
        self.tracks.is_empty()
            && self.slices.is_empty()
            && self.ui.is_empty()
            && self.raster.is_empty()
    }
}

fn slice_cat(c: Category) -> SliceCat {
    match c {
        Category::Dart => SliceCat::Dart,
        Category::Embedder => SliceCat::Embedder,
        Category::Gc => SliceCat::Gc,
        Category::Other => SliceCat::Other,
    }
}

/// Session times of an interval in timeline µs. One that ended before the session began is
/// `None`; one that began before it starts at 0 (a new session starts in the middle of work).
fn span(clock: &RunClock, start_us: i64, dur_us: i64) -> Option<(SessionTime, u64)> {
    let s = clock.from_timeline_us(start_us);
    let e = clock.from_timeline_us(start_us.saturating_add(dur_us.max(0)));
    if e.clamped {
        return None;
    }
    Some((s.time, e.time.0.saturating_sub(s.time.0)))
}

fn to_batch(sink: &Sink, out: AssembledBatch) -> TimelineBatch {
    let clock = &sink.clock;
    TimelineBatch {
        run: sink.run,
        tracks: out
            .tracks
            .into_iter()
            .map(|((kind, key), name, order)| (kind, key, name, order))
            .collect(),
        slices: out
            .slices
            .into_iter()
            .filter_map(|s| {
                let (start, dur_ns) = span(clock, s.start_us, s.dur_us)?;
                Some(BatchSlice {
                    track: s.track,
                    depth: s.depth,
                    start,
                    dur_ns,
                    name: s.name,
                    cat: slice_cat(s.cat),
                    widget_build: s.widget_build,
                })
            })
            .collect(),
        ui: out
            .ui
            .into_iter()
            .map(|(n, p)| {
                (
                    n,
                    UiPhases {
                        begin: clock.from_timeline_us(p.begin_us).time,
                        ui_us: p.ui_us,
                        build_us: p.build_us,
                        layout_us: p.layout_us,
                        paint_us: p.paint_us,
                        composite_us: p.composite_us,
                        finalize_us: p.finalize_us,
                        widgets_built: p.widgets_built,
                    },
                )
            })
            .collect(),
        raster: out
            .raster
            .into_iter()
            .map(|(n, start, dur)| {
                (
                    n,
                    RasterPhase {
                        start: clock.from_timeline_us(start).time,
                        raster_us: dur.clamp(0, i64::from(u32::MAX)) as u32,
                    },
                )
            })
            .collect(),
    }
}

/// What the assembler has been fed, to tell whether the next window continues it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fed {
    Nothing,
    /// Polled windows up to here: the next one must start at `.0`.
    Polled(i64),
    /// Events from the stream, which carry no window.
    Streamed,
}

/// The assembler plus the rules for when it must forget what it holds.
struct Pipeline {
    asm: Assembler,
    conn: Option<u64>,
    fed: Fed,
    logged: AssemblerStats,
}

impl Pipeline {
    fn new() -> Pipeline {
        Pipeline {
            asm: Assembler::new(),
            conn: None,
            fed: Fed::Nothing,
            logged: AssemblerStats::default(),
        }
    }

    /// Feeds one window (`Some((origin, next))`: the events of `[origin, next)`) or a block of
    /// stream events (`None`). A new session or reconnect, or a window that does not follow the
    /// last one, would pair a begin with an end from far away: the assembler starts over.
    fn feed(
        &mut self,
        sink: &Sink,
        events: Vec<TraceEvent>,
        window: Option<(i64, i64)>,
    ) -> Option<TimelineBatch> {
        let gap = match (window, self.fed) {
            (Some((origin, _)), Fed::Polled(next)) => origin != next,
            (Some(_), Fed::Streamed) => true,
            _ => false,
        };
        if self.conn != Some(sink.conn) || gap {
            self.asm.reset();
        }
        self.conn = Some(sink.conn);
        self.fed = match window {
            Some((_, next)) => Fed::Polled(next),
            None => Fed::Streamed,
        };
        let mut out = AssembledBatch::default();
        self.asm.feed(events, &mut out);
        let stats = self.asm.stats();
        if stats != self.logged {
            log::debug!("timeline assembler: {stats:?}");
            self.logged = stats;
        }
        let batch = to_batch(sink, out);
        (!batch.is_empty()).then_some(batch)
    }
}

pub(crate) struct PerfConfig {
    pub(crate) poll: Duration,
    pub(crate) call_timeout: Duration,
    pub(crate) cpu_poll: Duration,
    pub(crate) cpu_call_timeout: Duration,
    /// The first window starts here (timeline µs).
    pub(crate) start_us: i64,
}

enum Msg {
    Events(Vec<TraceEvent>),
    /// Turn widget-build tracing on or off.
    SetTracing(bool),
    /// A hot restart: the new main isolate has tracing off again.
    Reapply(String),
    FetchCpuNow,
}

/// The most stream events fed to the assembler as one block, so one batch reaches the session
/// task for many 64-event blocks.
const MAX_COALESCED_EVENTS: usize = 16_384;

/// What the CPU shows when the profiler is off.
const CPU_OFF: &str = "CPU sampling is off in this app.";

/// One fetch of CPU samples, in session time. Functions and stacks index into this batch only;
/// the session interns them.
#[derive(Debug)]
pub(crate) struct CpuBatch {
    pub(crate) run: RunId,
    pub(crate) period_us: u32,
    /// `(name, library)`; no library is a native function.
    pub(crate) functions: Vec<(String, Option<String>)>,
    /// Root first; indices into `functions`.
    pub(crate) stacks: Vec<Vec<u32>>,
    /// Sorted by time; the index is into `stacks`.
    pub(crate) samples: Vec<(SessionTime, u32)>,
    /// The samples cover the run up to here.
    pub(crate) coverage_end: SessionTime,
}

/// Maps a decoded profile onto session time. Samples from before the session began are dropped,
/// and the rest are sorted, because the VM does not order them.
fn cpu_batch(sink: &Sink, profile: CpuProfile, period_us: i64, window_end_us: i64) -> CpuBatch {
    let mut samples: Vec<(SessionTime, u32)> = profile
        .samples
        .iter()
        .filter_map(|s| {
            let st = sink.clock.from_timeline_us((s.ts_ns / 1000) as i64);
            (!st.clamped).then_some((st.time, s.stack))
        })
        .collect();
    samples.sort_by_key(|s| s.0);
    CpuBatch {
        run: sink.run,
        period_us: period_us.clamp(1, i64::from(u32::MAX)) as u32,
        functions: profile
            .functions
            .into_iter()
            .map(|f| (f.name, f.library))
            .collect(),
        stacks: profile.stacks,
        samples,
        coverage_end: sink.clock.from_timeline_us(window_end_us).time,
    }
}

/// The session's handle for commands to the poller task.
pub(crate) struct PerfControl {
    tx: mpsc::UnboundedSender<Msg>,
    svc: VmService,
    shared: Arc<Cpu>,
}

impl PerfControl {
    /// Fetches CPU samples at once, whether or not frames were drawn.
    pub(crate) fn fetch_now(&self) {
        let _ = self.tx.send(Msg::FetchCpuNow);
    }

    /// Turns widget-build tracing on (and the timeline to the `Timeline` stream) or off (and
    /// back to polling). The run's actual state comes back as `Ingest::Tracing`.
    pub(crate) fn set_tracing(&self, on: bool) {
        if self.shared.wanted.swap(on, Ordering::Relaxed) != on {
            let _ = self.tx.send(Msg::SetTracing(on));
        }
    }

    /// Puts tracing back if fdt turned it on. Each call gives up after `timeout`.
    pub(crate) async fn restore(&self, timeout: Duration) {
        self.shared.wanted.store(false, Ordering::Relaxed);
        // Back to what the app had before fdt first changed it, if it differs now.
        let before = *self.shared.tracing_before.lock().unwrap();
        if before != Some(false) || !self.shared.tracing_now.swap(false, Ordering::Relaxed) {
            return;
        }
        let isolate = self.shared.main.lock().unwrap().clone();
        let off = self
            .svc
            .set_bool_extension(PROFILE_WIDGET_BUILDS, &isolate, false);
        match tokio::time::timeout(timeout, off).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::debug!("turning widget-build tracing off failed: {e}"),
            Err(_) => log::debug!("turning widget-build tracing off timed out"),
        }
    }
}

/// Owns the timeline task of one connection; aborts it when dropped.
pub(crate) struct PerfHub {
    tx: mpsc::UnboundedSender<Msg>,
    task: JoinHandle<()>,
    svc: VmService,
    /// What the router and the session share with the task.
    shared: Arc<Cpu>,
}

impl PerfHub {
    pub(crate) fn new(
        sink: SharedSink,
        svc: VmService,
        cfg: PerfConfig,
        main_isolate: String,
    ) -> PerfHub {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Cpu {
            main: Mutex::new(main_isolate),
            frames: AtomicU64::new(0),
            wanted: AtomicBool::new(false),
            tracing_before: Mutex::new(None),
            tracing_now: AtomicBool::new(false),
        });
        let task = tokio::spawn(run(sink, svc.clone(), cfg, rx, shared.clone()));
        PerfHub {
            tx,
            task,
            svc,
            shared,
        }
    }

    /// A hot restart: CPU samples come from the new isolate.
    pub(crate) fn set_main_isolate(&self, id: &str) {
        *self.shared.main.lock().unwrap() = id.to_string();
    }

    /// The extension that traces widget builds appeared in `isolate`: after a hot restart it is
    /// off again, so a wanted `true` is applied anew.
    pub(crate) fn reapply_tracing(&self, isolate: &str) {
        if self.shared.wanted.load(Ordering::Relaxed) {
            let _ = self.tx.send(Msg::Reapply(isolate.to_string()));
        }
    }

    /// A frame was drawn; the next CPU tick fetches.
    pub(crate) fn frame_drawn(&self) {
        self.shared.frames.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn control(&self) -> std::sync::Arc<PerfControl> {
        std::sync::Arc::new(PerfControl {
            tx: self.tx.clone(),
            svc: self.svc.clone(),
            shared: self.shared.clone(),
        })
    }

    /// Events from the `Timeline` stream, in the order they arrived.
    pub(crate) fn events(&self, events: Vec<TraceEvent>) {
        let _ = self.tx.send(Msg::Events(events));
    }
}

impl Drop for PerfHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the router shares with the task.
struct Cpu {
    /// The main isolate, which the router updates at a hot restart.
    main: Mutex<String>,
    /// `Flutter.Frame` events seen since the task last looked.
    frames: AtomicU64,
    /// Widget-build tracing is wanted on.
    wanted: AtomicBool,
    /// fdt turned tracing on, so it turns it off when the user disconnects.
    /// What tracing was before fdt first changed it on this connection (`None`: not read).
    tracing_before: Mutex<Option<bool>>,
    /// What fdt last set it to.
    tracing_now: AtomicBool,
}

/// The CPU side of the task: where the next window starts and whether a fetch is owed.
struct CpuState {
    next: i64,
    /// Frames were drawn and no fetch has covered them yet (kept across a timeout).
    pending: bool,
    /// The profiler is off: no more fetches on this connection.
    off: bool,
    /// Consecutive server errors on `error_isolate`.
    errors: u8,
    error_isolate: String,
}

/// Server errors in a row, on one isolate, that mark the profiler off.
const OFF_AFTER_ERRORS: u8 = 3;

/// One CPU fetch. Returns false when the connection closed.
async fn fetch_cpu(
    shared: &SharedSink,
    svc: &VmService,
    cfg: &PerfConfig,
    cpu: &Cpu,
    st: &mut CpuState,
) -> bool {
    let now = match svc.get_vm_timeline_micros().await {
        Ok(n) => n,
        Err(RpcError::Closed) => return false,
        Err(e) => {
            log::debug!("getVMTimelineMicros failed: {e}");
            return true;
        }
    };
    if now < st.next {
        return true;
    }
    let isolate = cpu.main.lock().unwrap().clone();
    let result = svc
        .get_perfetto_cpu_samples(&isolate, st.next, now - st.next, cfg.cpu_call_timeout)
        .await;
    let sink = shared.get();
    match result {
        Ok((profile, window)) => {
            if profile.skipped_samples > 0 {
                log::debug!(
                    "skipped {} CPU samples with unknown ids",
                    profile.skipped_samples
                );
            }
            st.next = now + 1;
            st.pending = false;
            st.errors = 0;
            let batch = cpu_batch(&sink, profile, window.period_us, now);
            sink.send(Ingest::Cpu(batch));
        }
        Err(RpcError::Closed) => return false,
        // The window stays: the next tick asks for it again, and more.
        Err(RpcError::Timeout) => log::warn!("getPerfettoCpuSamples timed out; retrying"),
        // One error is not "off": the isolate may be gone in a hot restart. The window stays.
        Err(e @ RpcError::Server { .. }) => {
            log::debug!("getPerfettoCpuSamples failed on {isolate}: {e}");
            if st.error_isolate != isolate {
                st.error_isolate = isolate;
                st.errors = 0;
            }
            st.errors += 1;
            if st.errors >= OFF_AFTER_ERRORS {
                log::info!("CPU sampling is off: {e}");
                st.off = true;
                sink.send(Ingest::CpuStatus(sink.run, CpuStatus::Off(CPU_OFF.into())));
            }
        }
        // An answer that cannot be read would be unreadable again: move past it.
        Err(e) => {
            log::warn!("getPerfettoCpuSamples failed; skipping the window: {e}");
            st.next = now + 1;
            st.pending = false;
        }
    }
    true
}

/// Switches widget-build tracing and, with it, where the timeline comes from. Returns the state
/// the app is in afterwards (`Some(false)` when it could not be changed), or `None` when the
/// connection closed.
async fn apply_tracing(
    svc: &VmService,
    cfg: &PerfConfig,
    cpu: &Cpu,
    on: bool,
    stream_mode: &mut bool,
    next: &mut i64,
) -> Option<bool> {
    let isolate = cpu.main.lock().unwrap().clone();
    if on {
        // Tracing someone else already turned on is not ours to turn off: the state before
        // fdt first touched it is kept for the whole connection.
        let known = cpu.tracing_before.lock().unwrap().is_some();
        if !known {
            match tokio::time::timeout(
                cfg.call_timeout,
                svc.get_bool_extension(PROFILE_WIDGET_BUILDS, &isolate),
            )
            .await
            {
                Ok(Err(RpcError::Closed)) => return None,
                Ok(Ok(was)) => *cpu.tracing_before.lock().unwrap() = Some(was),
                _ => {}
            }
        }
        match svc
            .set_bool_extension(PROFILE_WIDGET_BUILDS, &isolate, true)
            .await
        {
            Ok(true) => cpu.tracing_now.store(true, Ordering::Relaxed),
            Ok(false) => return Some(false),
            Err(RpcError::Closed) => return None,
            Err(e) => {
                log::warn!("turning widget-build tracing on failed: {e}");
                return Some(false);
            }
        }
        if !*stream_mode {
            match svc.stream_listen("Timeline").await {
                Ok(()) => *stream_mode = true,
                Err(RpcError::Closed) => return None,
                // Tracing is on, and the timeline is still polled (it holds few frames).
                Err(e) => log::warn!("streamListen Timeline failed: {e}"),
            }
        }
        return Some(true);
    }
    let actual = match svc
        .set_bool_extension(PROFILE_WIDGET_BUILDS, &isolate, false)
        .await
    {
        Ok(on) => {
            cpu.tracing_now.store(on, Ordering::Relaxed);
            on
        }
        Err(RpcError::Closed) => return None,
        Err(e) => {
            log::warn!("turning widget-build tracing off failed: {e}");
            false
        }
    };
    if *stream_mode {
        match svc.stream_cancel("Timeline").await {
            Ok(()) => {}
            Err(RpcError::Closed) => return None,
            Err(e) => log::debug!("streamCancel Timeline failed: {e}"),
        }
        *stream_mode = false;
        // Polling continues from here: what the stream already delivered is not asked for again.
        match svc.get_vm_timeline_micros().await {
            Ok(now) => *next = now + 1,
            Err(RpcError::Closed) => return None,
            Err(e) => log::debug!("getVMTimelineMicros failed: {e}"),
        }
    }
    Some(actual)
}

async fn run(
    shared: SharedSink,
    svc: VmService,
    cfg: PerfConfig,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    cpu: Arc<Cpu>,
) {
    let mut tick = interval(cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut cpu_tick = interval_at(Instant::now() + cfg.cpu_poll, cfg.cpu_poll);
    cpu_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut cs = CpuState {
        next: cfg.start_us,
        pending: false,
        off: false,
        errors: 0,
        error_isolate: String::new(),
    };
    let mut pipe = Pipeline::new();
    let mut next = cfg.start_us;
    let mut stream_mode = false;
    let mut held: Option<Msg> = None;
    loop {
        tokio::select! {
            _ = tick.tick(), if !stream_mode => {
                let now = match svc.get_vm_timeline_micros().await {
                    Ok(n) => n,
                    Err(RpcError::Closed) => return,
                    Err(e) => {
                        log::debug!("getVMTimelineMicros failed: {e}");
                        continue;
                    }
                };
                if now < next {
                    continue;
                }
                let chunk = match svc.get_vm_timeline(next, now - next, cfg.call_timeout).await {
                    Ok(c) => c,
                    Err(RpcError::Closed) => return,
                    // The window stays: the next tick asks for it again, and more.
                    Err(e) => {
                        log::warn!("getVMTimeline failed; retrying the same window: {e}");
                        continue;
                    }
                };
                if chunk.skipped > 0 {
                    log::debug!("skipped {} unreadable timeline events", chunk.skipped);
                }
                let sink = shared.get();
                let window = (next, now + 1);
                next = now + 1;
                if let Some(b) = pipe.feed(&sink, chunk.events, Some(window)) {
                    sink.send(Ingest::Timeline(b));
                }
            }
            _ = cpu_tick.tick(), if !cs.off => {
                if cpu.frames.swap(0, Ordering::Relaxed) > 0 {
                    cs.pending = true;
                }
                if cs.pending && !fetch_cpu(&shared, &svc, &cfg, &cpu, &mut cs).await {
                    return;
                }
            }
            msg = async {
                match held.take() {
                    Some(m) => Some(m),
                    None => rx.recv().await,
                }
            } => match msg {
                None => return,
                Some(Msg::SetTracing(on)) => {
                    let Some(actual) = apply_tracing(&svc, &cfg, &cpu, on, &mut stream_mode, &mut next).await else {
                        return;
                    };
                    shared.get().send(Ingest::Tracing(shared.get().run, actual));
                }
                Some(Msg::Reapply(isolate)) => {
                    // A restart of something other than the main isolate changes nothing.
                    if cpu.wanted.load(Ordering::Relaxed)
                        && *cpu.main.lock().unwrap() == isolate
                    {
                        let Some(actual) = apply_tracing(&svc, &cfg, &cpu, true, &mut stream_mode, &mut next).await else {
                            return;
                        };
                        shared.get().send(Ingest::Tracing(shared.get().run, actual));
                    }
                }
                Some(Msg::FetchCpuNow) => {
                    cpu.frames.store(0, Ordering::Relaxed);
                    if !cs.off && !fetch_cpu(&shared, &svc, &cfg, &cpu, &mut cs).await {
                        return;
                    }
                }
                Some(Msg::Events(mut events)) => {
                    // Under tracing the stream delivers hundreds of thousands of events a
                    // second in blocks of 64: take what is already waiting as one block.
                    while events.len() < MAX_COALESCED_EVENTS {
                        match rx.try_recv() {
                            Ok(Msg::Events(more)) => events.extend(more),
                            Ok(other) => {
                                held = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    let sink = shared.get();
                    if let Some(b) = pipe.feed(&sink, events, None) {
                        sink.send(Ingest::Timeline(b));
                    }
                }
            },
        }
    }
}

/// Writes a batch into the store. Tracks are always kept (a track is only announced once, and
/// the names stay right when recording resumes); slices and phases only while recording.
pub(crate) fn store(w: &mut SessionWriter, batch: TimelineBatch, recording: bool) {
    let run = batch.run;
    let mut ids: HashMap<TrackKeyRaw, TrackId> = HashMap::new();
    for (kind, key, name, order) in &batch.tracks {
        let id = w.timeline_track(run, *kind, *key, name);
        w.set_track_order(id, *order);
        ids.insert((*kind, *key), id);
    }
    if !recording {
        return;
    }
    for s in batch.slices {
        let id = *ids
            .entry(s.track)
            .or_insert_with(|| w.timeline_track(run, s.track.0, s.track.1, ""));
        let name = w.intern_name(&s.name);
        w.push_slice(
            id,
            s.depth,
            Slice {
                start: s.start,
                dur_ns: s.dur_ns,
                name,
                cat: s.cat,
            },
            s.widget_build,
        );
    }
    for (n, ui) in batch.ui {
        w.set_frame_ui(run, n, ui);
    }
    for (n, r) in batch.raster {
        w.set_frame_raster(run, n, r);
    }
}

/// Writes a fetch into the store: interns its functions and stacks, then the samples (only while
/// recording), the coverage, the period and `On`.
pub(crate) fn store_cpu(w: &mut SessionWriter, batch: CpuBatch, recording: bool) {
    let run = batch.run;
    w.set_cpu_period(run, batch.period_us);
    w.set_cpu_status(run, CpuStatus::On);
    if !recording {
        return;
    }
    let functions: Vec<FunctionId> = batch
        .functions
        .iter()
        .map(|(name, lib)| w.intern_function(name, lib.as_deref()))
        .collect();
    let leaves: Vec<_> = batch
        .stacks
        .iter()
        .map(|stack| {
            let ids: Vec<FunctionId> = stack.iter().map(|&i| functions[i as usize]).collect();
            w.intern_stack(&ids)
        })
        .collect();
    let samples = batch
        .samples
        .into_iter()
        .map(|(time, stack)| CpuSample {
            time,
            leaf: leaves[stack as usize],
        })
        .collect();
    w.push_cpu_samples(run, samples);
    w.set_cpu_coverage(run, batch.coverage_end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::clock::Calibration;
    use fdt_vm::timeline::Phase;

    fn sink(conn: u64) -> Sink {
        let (tx, _rx) = mpsc::unbounded_channel();
        let clock = RunClock::new(
            1_000_000,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 1_000_000,
            },
        );
        Sink::new(RunId(1), clock, conn, 0, tx)
    }

    fn ev(ph: Phase, name: &str, ts: i64) -> TraceEvent {
        TraceEvent {
            ph,
            name: name.into(),
            cat: Category::Dart,
            pid: 1,
            tid: 5,
            ts_us: ts,
            dur_us: None,
            id: None,
            frame_number: None,
            thread_name: None,
        }
    }

    #[test]
    fn a_window_that_does_not_follow_the_last_starts_over() {
        let s = sink(1);
        let mut p = Pipeline::new();
        assert!(
            p.feed(&s, vec![ev(Phase::Begin, "a", 10)], Some((0, 100)))
                .is_none()
        );
        // Contiguous: the end closes the begin from the earlier window.
        let b = p
            .feed(&s, vec![ev(Phase::End, "a", 150)], Some((100, 200)))
            .unwrap();
        assert_eq!(b.slices.len(), 1);
        // A gap (origin 500, not 200): the begin is forgotten, so the end matches nothing.
        p.feed(&s, vec![ev(Phase::Begin, "b", 210)], Some((200, 300)));
        let b = p.feed(&s, vec![ev(Phase::End, "b", 600)], Some((500, 700)));
        assert!(b.is_none());
        assert_eq!(p.asm.stats().unmatched_end, 1);
    }

    #[test]
    fn polling_after_stream_events_and_a_new_connection_start_over() {
        let s = sink(1);
        let mut p = Pipeline::new();
        p.feed(&s, vec![ev(Phase::Begin, "a", 10)], None);
        let b = p.feed(&s, vec![ev(Phase::End, "a", 20)], Some((0, 100)));
        assert!(b.is_none(), "stream to poll is a gap");
        p.feed(&s, vec![ev(Phase::Begin, "c", 110)], Some((100, 200)));
        let b = p.feed(&sink(2), vec![ev(Phase::End, "c", 120)], Some((200, 300)));
        assert!(b.is_none(), "a new connection forgets open slices");
    }

    #[test]
    fn slices_from_before_the_session_are_trimmed_or_dropped() {
        let s = sink(1);
        let clock = &s.clock;
        // timeline 0 is session 0 here (timeline_to_wall == origin).
        assert_eq!(span(clock, 10, 5), Some((SessionTime(10_000), 5_000)));
        assert_eq!(span(clock, -10, 5), None);
        assert_eq!(span(clock, -3, 10), Some((SessionTime(0), 7_000)));
    }
}
