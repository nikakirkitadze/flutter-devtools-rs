//! Memory polling, GC events and on-demand allocation profiles of one connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fdt_store::heap::{HeapFile, HeapId, HeapStage};
use fdt_store::memory::{GcKind, GcReason, GcRecord};
use fdt_store::time::RunId;
use fdt_vm::events::{GcEvent, HeapSpace};
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;

use super::{Ingest, SharedSink, Sink};

/// What the memory task needs, from `CaptureConfig`.
#[derive(Clone, Debug)]
pub(crate) struct MemoryConfig {
    pub(crate) poll: Duration,
    pub(crate) profile_timeout: Duration,
    /// A heap snapshot request gives up after this long.
    pub(crate) request_timeout: Duration,
    /// A heap snapshot gives up when no chunk arrives for this long.
    pub(crate) chunk_timeout: Duration,
    pub(crate) heap_dir: PathBuf,
}

enum Msg {
    FetchProfile(bool),
    Take(Take),
}

/// One snapshot to take.
struct Take {
    id: HeapId,
    session: u32,
    cancel: watch::Receiver<bool>,
}

/// Keyed by session too: ids restart in every session, and a transfer of the old one may still
/// be winding down.
type Cancels = Arc<Mutex<HashMap<(u32, HeapId), watch::Sender<bool>>>>;

/// The connection's memory commands.
pub(crate) struct MemoryControl {
    tx: mpsc::UnboundedSender<Msg>,
    target: Arc<Mutex<Target>>,
    cancels: Cancels,
}

impl MemoryControl {
    /// Fetches the allocation profile of the main isolate. `gc` forces a full GC first, which
    /// every client sees; the caller passes it only when the user switched it on.
    pub(crate) fn fetch_profile(&self, gc: bool) {
        let _ = self.tx.send(Msg::FetchProfile(gc));
    }

    /// The isolate a snapshot taken now would cover.
    pub(crate) fn main_isolate(&self) -> String {
        self.target.lock().unwrap().main.clone()
    }

    /// Takes the snapshot `id` of the main isolate. `session` goes into the file name, because
    /// ids restart in every session.
    pub(crate) fn take(&self, id: HeapId, session: u32) {
        let (tx, cancel) = watch::channel(false);
        self.cancels.lock().unwrap().insert((session, id), tx);
        let _ = self.tx.send(Msg::Take(Take {
            id,
            session,
            cancel,
        }));
    }

    /// Stops receiving snapshot `id`: the file is deleted, and chunks are ignored until the
    /// last one. The app's pause is not shortened.
    pub(crate) fn cancel(&self, session: u32, id: HeapId) {
        if let Some(tx) = self.cancels.lock().unwrap().get(&(session, id)) {
            tx.send_replace(true);
        }
    }
}

/// The isolate and group the task works on; the router updates them at a hot restart.
struct Target {
    main: String,
    group: String,
}

/// Owns the memory task of one connection; aborts it when dropped.
pub(crate) struct MemoryHub {
    tx: mpsc::UnboundedSender<Msg>,
    task: JoinHandle<()>,
    target: Arc<Mutex<Target>>,
    cancels: Cancels,
}

impl MemoryHub {
    /// `root_package` names the app's package, for classes of the app.
    pub(crate) fn new(
        sink: SharedSink,
        svc: VmService,
        cfg: MemoryConfig,
        main: String,
        group: String,
        root_package: Option<String>,
    ) -> MemoryHub {
        let (tx, rx) = mpsc::unbounded_channel();
        let target = Arc::new(Mutex::new(Target { main, group }));
        let cancels = Cancels::default();
        let task = tokio::spawn(run(
            sink,
            svc,
            cfg,
            rx,
            target.clone(),
            cancels.clone(),
            root_package,
        ));
        MemoryHub {
            tx,
            task,
            target,
            cancels,
        }
    }

    pub(crate) fn control(&self) -> Arc<MemoryControl> {
        Arc::new(MemoryControl {
            tx: self.tx.clone(),
            target: self.target.clone(),
            cancels: self.cancels.clone(),
        })
    }

    /// A hot restart: polling and profiles move to the new main isolate and its group.
    pub(crate) fn set_main(&self, isolate: &str, group: &str) {
        let mut t = self.target.lock().unwrap();
        t.main = isolate.to_string();
        t.group = group.to_string();
    }
}

impl Drop for MemoryHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    sink: SharedSink,
    svc: VmService,
    cfg: MemoryConfig,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    target: Arc<Mutex<Target>>,
    cancels: Cancels,
    root_package: Option<String>,
) {
    // Snapshots go one at a time: the chunk receiver is the client's only one. Dropped with
    // this task, which ends them.
    let mut transfers = JoinSet::new();
    let turn = Arc::new(tokio::sync::Mutex::new(()));
    let mut tick = tokio::time::interval(cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut rss_works = true;
    loop {
        tokio::select! {
            Some(_) = transfers.join_next(), if !transfers.is_empty() => {}
            _ = tick.tick() => {
                let group = target.lock().unwrap().group.clone();
                if group.is_empty() {
                    continue;
                }
                let (usage, rss) = tokio::join!(svc.get_isolate_group_memory_usage(&group), async {
                    if rss_works { Some(svc.get_process_rss().await) } else { None }
                });
                let rss = match rss {
                    Some(Ok(v)) => Some(v),
                    Some(Err(RpcError::Closed)) => return,
                    Some(Err(e)) => {
                        log::debug!("getProcessMemoryUsage failed; not asking again: {e}");
                        rss_works = false;
                        None
                    }
                    None => None,
                };
                match usage {
                    Ok(usage) => {
                        let s = sink.get();
                        s.send(Ingest::MemorySample { run: s.run, usage, rss });
                    }
                    Err(RpcError::Closed) => return,
                    Err(e) => log::debug!("getIsolateGroupMemoryUsage failed: {e}"),
                }
            }
            msg = rx.recv() => match msg {
                Some(Msg::FetchProfile(gc)) => {
                    let isolate = target.lock().unwrap().main.clone();
                    let svc = svc.clone();
                    let sink = sink.get();
                    let timeout = cfg.profile_timeout;
                    // Its own task: a slow profile must not hold up the polling.
                    tokio::spawn(async move {
                        match svc.get_allocation_profile(&isolate, gc, timeout).await {
                            Ok(profile) => sink.send(Ingest::AllocProfile {
                                run: sink.run,
                                isolate,
                                gc,
                                profile: Box::new(profile),
                            }),
                            Err(e) => log::warn!("getAllocationProfile failed: {e}"),
                        }
                    });
                }
                Some(Msg::Take(take)) => {
                    let isolate = target.lock().unwrap().main.clone();
                    transfers.spawn(transfer(
                        Job {
                            take,
                            isolate,
                            root_package: root_package.clone(),
                            turn: turn.clone(),
                            cancels: cancels.clone(),
                        },
                        sink.get(),
                        svc.clone(),
                        cfg.clone(),
                    ));
                }
                None => return,
            },
        }
    }
}

const STREAM: &str = "HeapSnapshot";
/// Progress reaches the session at most this often.
const STAGE_EVERY: Duration = Duration::from_millis(100);
/// Cancelling the stream must not hold up a result when the app has stopped answering.
const CANCEL_WAIT: Duration = Duration::from_secs(2);

const NO_SNAPSHOT: &str = "The app didn't send a snapshot.";
const STOPPED: &str = "The app stopped sending the snapshot.";
const DISCONNECTED: &str = "The app disconnected before the snapshot finished.";
const REFUSED: &str = "The app couldn't take a snapshot.";
const NOT_SAVED: &str = "The snapshot couldn't be saved.";

struct Job {
    take: Take,
    isolate: String,
    root_package: Option<String>,
    turn: Arc<tokio::sync::Mutex<()>>,
    cancels: Cancels,
}

/// The snapshot file while it is written. The handle is declared first so it closes before the
/// file is deleted (Windows cannot delete an open file).
struct Dest {
    file: tokio::fs::File,
    heap: Arc<HeapFile>,
}

enum Outcome {
    Done {
        heap: Arc<HeapFile>,
        bytes: u64,
        first_ms: i64,
    },
    Cancelled,
    Failed {
        message: &'static str,
        details: String,
    },
}

fn failed(message: &'static str, details: impl ToString) -> Outcome {
    Outcome::Failed {
        message,
        details: details.to_string(),
    }
}

fn rpc_failure(e: &RpcError) -> Outcome {
    match e {
        RpcError::Closed => failed(DISCONNECTED, e),
        RpcError::Timeout => failed(STOPPED, e),
        _ => failed(REFUSED, e),
    }
}

/// One snapshot, from the request to the file. Subscribes just before it and cancels after the
/// reply, so snapshots other tools ask for never reach fdt.
async fn transfer(job: Job, sink: Sink, svc: VmService, cfg: MemoryConfig) {
    let (id, session) = (job.take.id, job.take.session);
    let _turn = job.turn.lock().await;
    let mut cancel = job.take.cancel;
    if *cancel.borrow() {
        job.cancels.lock().unwrap().remove(&(session, id));
        return;
    }
    let outcome = receive(
        job.take.session,
        id,
        &job.isolate,
        &svc,
        &cfg,
        &sink,
        &mut cancel,
    )
    .await;
    match outcome {
        Outcome::Done {
            heap,
            bytes,
            first_ms,
        } => {
            cancel_stream(&svc).await;
            sink.send(Ingest::HeapReceived {
                session,
                id,
                time: sink.clock.from_epoch_ms(first_ms).time,
                bytes,
                file: heap,
                root_package: job.root_package,
            });
        }
        Outcome::Failed { message, details } => {
            log::debug!("heap snapshot {} failed: {message}", id.0);
            sink.send(Ingest::HeapFailed {
                session,
                id,
                message,
                details,
            });
            cancel_stream(&svc).await;
        }
        Outcome::Cancelled => cancel_stream(&svc).await,
    }
    job.cancels.lock().unwrap().remove(&(session, id));
}

async fn cancel_stream(svc: &VmService) {
    let _ = tokio::time::timeout(CANCEL_WAIT, svc.stream_cancel(STREAM)).await;
}

async fn create(dir: &Path, session: u32, id: HeapId) -> std::io::Result<Dest> {
    let mut dirs = tokio::fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    dirs.mode(0o700);
    dirs.create(dir).await?;
    let path = dir.join(format!(
        "{}-{session}-{}.dartheap",
        std::process::id(),
        id.0
    ));
    let mut open = tokio::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    open.mode(0o600);
    let file = open.open(&path).await?;
    Ok(Dest {
        file,
        heap: Arc::new(HeapFile::new(path)),
    })
}

#[allow(clippy::too_many_arguments)] // one transfer, each argument a different thing it needs
async fn receive(
    session: u32,
    id: HeapId,
    isolate: &str,
    svc: &VmService,
    cfg: &MemoryConfig,
    sink: &Sink,
    cancel: &mut watch::Receiver<bool>,
) -> Outcome {
    if let Err(e) = svc.stream_listen(STREAM).await {
        return rpc_failure(&e);
    }
    let mut dest = match create(&cfg.heap_dir, session, id).await {
        Ok(d) => Some(d),
        Err(e) => return failed(NOT_SAVED, e.kind()),
    };
    let mut rx = svc.client().heap_chunks();
    // Chunks of a snapshot another client asked for in the meantime are not ours.
    while rx.try_recv().is_ok() {}
    let req = svc.request_heap_snapshot(isolate, cfg.request_timeout);
    tokio::pin!(req);
    let idle = tokio::time::sleep(cfg.chunk_timeout);
    tokio::pin!(idle);
    let mut reply: Option<Result<(), RpcError>> = None;
    let mut cancelled = *cancel.borrow();
    let mut watching = !cancelled;
    if cancelled {
        dest = None;
    }
    let (mut bytes, mut chunks, mut last, mut first_ms) = (0u64, 0u32, false, 0i64);
    let mut staged: Option<Instant> = None;
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed(), if watching => {
                if changed.is_ok() && *cancel.borrow() {
                    cancelled = true;
                    watching = false;
                    // Deleted now; the rest of the snapshot is read and thrown away.
                    dest = None;
                } else if changed.is_err() {
                    watching = false;
                }
            }
            chunk = rx.recv() => {
                let Some(c) = chunk else {
                    return if cancelled { Outcome::Cancelled } else { failed(DISCONNECTED, "chunk channel closed") };
                };
                // One snapshot per request: another client's, for this isolate, may follow.
                if c.isolate_id != isolate || last {
                    continue;
                }
                idle.as_mut().reset(tokio::time::Instant::now() + cfg.chunk_timeout);
                last |= c.last;
                if let Some(d) = dest.as_mut() {
                    if chunks == 0 {
                        first_ms = c.timestamp_ms;
                    }
                    if let Err(e) = d.file.write_all(&c.payload).await {
                        return failed(NOT_SAVED, e.kind());
                    }
                    bytes += c.payload.len() as u64;
                    chunks += 1;
                    if staged.is_none_or(|t| t.elapsed() >= STAGE_EVERY) {
                        staged = Some(Instant::now());
                        sink.send(Ingest::HeapStage(session, id, HeapStage::Receiving { bytes, chunks }));
                    }
                } else {
                    chunks += 1;
                }
                if cancelled && last {
                    return Outcome::Cancelled;
                }
            }
            r = &mut req, if reply.is_none() => reply = Some(r),
            () = &mut idle => {
                if cancelled {
                    return Outcome::Cancelled;
                }
                if !last {
                    return failed(STOPPED, "no chunk arrived in time");
                }
                // The data is whole; only the reply is missing.
                reply = Some(Ok(()));
            }
        }
        if reply.is_some() {
            break;
        }
    }
    // The reply follows the last chunk on one connection, so nothing is left in the queue.
    if cancelled {
        return Outcome::Cancelled;
    }
    match reply {
        Some(Err(e)) => return rpc_failure(&e),
        Some(Ok(())) if chunks == 0 => return failed(NO_SNAPSHOT, "no chunks"),
        Some(Ok(())) if !last => return failed(STOPPED, "the last chunk never came"),
        _ => {}
    }
    let Some(Dest { mut file, heap }) = dest else {
        return Outcome::Cancelled;
    };
    if let Err(e) = file.flush().await {
        return failed(NOT_SAVED, e.kind());
    }
    drop(file);
    Outcome::Done {
        heap,
        bytes,
        first_ms,
    }
}

/// `basename:line` for a class's location.
pub(crate) fn location_text(uri: &str, line: Option<u32>) -> String {
    let base = Path::new(uri)
        .file_name()
        .and_then(|n| n.to_str())
        .or_else(|| uri.rsplit('/').next())
        .unwrap_or(uri);
    match line {
        Some(l) => format!("{base}:{l}"),
        None => base.to_string(),
    }
}

/// Turns GC events into records: one per GC (the VM sends one event per isolate), with the
/// duration from the growth of the cumulative `time`.
#[derive(Default)]
pub(crate) struct GcTracker {
    last: HashMap<String, (HeapSpace, HeapSpace)>,
}

impl GcTracker {
    pub(crate) fn record(&mut self, sink: &super::Sink, ev: &GcEvent) -> Option<(RunId, GcRecord)> {
        let prev = self
            .last
            .insert(ev.isolate_group.clone(), (ev.new_space, ev.old_space));
        let dur_us = match prev {
            Some((pn, po)) => {
                if pn.collections == ev.new_space.collections
                    && po.collections == ev.old_space.collections
                {
                    return None;
                }
                let grew = |a: &HeapSpace, b: &HeapSpace| {
                    (b.collections > a.collections).then(|| (b.time_s - a.time_s).max(0.0))
                };
                let secs = grew(&pn, &ev.new_space).unwrap_or(0.0)
                    + grew(&po, &ev.old_space).unwrap_or(0.0);
                Some((secs * 1e6).round().min(f64::from(u32::MAX)) as u32)
            }
            None => None,
        };
        Some((
            sink.run,
            GcRecord {
                time: sink.clock.from_epoch_ms(ev.timestamp_ms).time,
                dur_us,
                kind: GcKind::from_vm(&ev.gc_type),
                reason: ev
                    .reason
                    .as_deref()
                    .map_or(GcReason::Other, GcReason::from_vm),
                heap_used: ev.new_space.used + ev.old_space.used,
                heap_capacity: ev.new_space.capacity + ev.old_space.capacity,
                external: ev.new_space.external + ev.old_space.external,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::clock::RunClock;

    fn sink() -> super::super::Sink {
        let (tx, _rx) = mpsc::unbounded_channel();
        let clock = RunClock {
            origin_host_us: 0,
            skew_us: 0,
            timeline_to_wall_us: 0,
        };
        super::super::Sink::new(RunId(1), clock, 1, 0, tx)
    }

    fn space(collections: u64, time_s: f64) -> HeapSpace {
        HeapSpace {
            used: 10,
            capacity: 20,
            external: 1,
            collections,
            time_s,
        }
    }

    fn ev(group: &str, new: HeapSpace, old: HeapSpace) -> GcEvent {
        GcEvent {
            isolate_group: group.into(),
            gc_type: "Scavenge".into(),
            reason: None,
            timestamp_ms: 5,
            new_space: new,
            old_space: old,
        }
    }

    #[test]
    fn duration_sums_the_spaces_that_grew() {
        let (mut t, s) = (GcTracker::default(), sink());
        let first = ev("g", space(1, 0.001), space(1, 0.002));
        assert_eq!(t.record(&s, &first).unwrap().1.dur_us, None);
        // Both spaces collected: 0.0004 + 0.0003 s.
        let both = ev("g", space(2, 0.0014), space(2, 0.0023));
        assert_eq!(t.record(&s, &both).unwrap().1.dur_us, Some(700));
    }

    #[test]
    fn duration_of_an_old_space_only_gc_ignores_new_space_time() {
        let (mut t, s) = (GcTracker::default(), sink());
        t.record(&s, &ev("g", space(1, 0.001), space(1, 0.002)));
        // New space's time moved but its count did not: only old space grew.
        let old_only = ev("g", space(1, 0.0015), space(2, 0.0027));
        assert_eq!(t.record(&s, &old_only).unwrap().1.dur_us, Some(700));
    }

    #[test]
    fn a_new_group_starts_without_a_duration() {
        let (mut t, s) = (GcTracker::default(), sink());
        t.record(&s, &ev("g1", space(5, 0.5), space(5, 0.5)));
        t.record(&s, &ev("g1", space(6, 0.6), space(5, 0.5)));
        // After a hot restart the new group's counters start over.
        let first = t.record(&s, &ev("g2", space(1, 0.0006), space(1, 0.0)));
        assert_eq!(first.unwrap().1.dur_us, None);
        // The same GC seen again is dropped.
        assert!(
            t.record(&s, &ev("g2", space(1, 0.0006), space(1, 0.0)))
                .is_none()
        );
    }
}
