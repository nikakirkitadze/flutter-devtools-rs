//! Timeline capture: the poller task, the assembler's output mapped onto session time, and the
//! session-side write.
//!
//! The task owns an [`Assembler`]. It polls `getVMTimeline` in windows that follow one another
//! (or takes events the router forwards from the `Timeline` stream), feeds the assembler and
//! sends the result to the session task as one [`TimelineBatch`].

use std::collections::HashMap;
use std::time::Duration;

use fdt_store::SessionWriter;
use fdt_store::clock::RunClock;
use fdt_store::frames::{RasterPhase, UiPhases};
use fdt_store::time::{RunId, SessionTime};
use fdt_store::timeline::{Slice, SliceCat, TrackId, TrackKind};
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;
use fdt_vm::timeline::{Category, TraceEvent};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

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
    /// The first window starts here (timeline µs).
    pub(crate) start_us: i64,
}

enum Msg {
    Events(Vec<TraceEvent>),
    StreamMode(bool),
}

/// The session's handle for commands to the poller task.
pub(crate) struct PerfControl {
    tx: mpsc::UnboundedSender<Msg>,
}

impl PerfControl {
    /// In stream mode the poll tick does nothing: events come from the `Timeline` stream.
    #[allow(dead_code)] // Used by the profiling commands that follow.
    pub(crate) fn set_stream_mode(&self, on: bool) {
        let _ = self.tx.send(Msg::StreamMode(on));
    }
}

/// Owns the timeline task of one connection; aborts it when dropped.
pub(crate) struct PerfHub {
    tx: mpsc::UnboundedSender<Msg>,
    task: JoinHandle<()>,
}

impl PerfHub {
    pub(crate) fn new(shared: SharedSink, svc: VmService, cfg: PerfConfig) -> PerfHub {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(shared, svc, cfg, rx));
        PerfHub { tx, task }
    }

    pub(crate) fn control(&self) -> std::sync::Arc<PerfControl> {
        std::sync::Arc::new(PerfControl {
            tx: self.tx.clone(),
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

async fn run(
    shared: SharedSink,
    svc: VmService,
    cfg: PerfConfig,
    mut rx: mpsc::UnboundedReceiver<Msg>,
) {
    let mut tick = interval(cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut pipe = Pipeline::new();
    let mut next = cfg.start_us;
    let mut stream_mode = false;
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
            msg = rx.recv() => match msg {
                None => return,
                Some(Msg::StreamMode(on)) => stream_mode = on,
                Some(Msg::Events(events)) => {
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
