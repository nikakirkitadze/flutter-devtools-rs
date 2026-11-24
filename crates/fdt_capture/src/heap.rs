//! Heap snapshots on the session side: the commands, the analysis jobs and what becomes of
//! their results. The transfer from the app is in `ingest::memory`.
//!
//! One analysis runs at a time, on a blocking thread, and lives across connections: its results
//! come back as [`HeapMsg`] tagged with the session they were started in, and a result from an
//! older session is dropped (which deletes its file when nothing else holds it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fdt_analysis::heap::{AnalyzeInput, HeapError, analyze, default_baseline, diff};
use fdt_store::heap::{HeapDiff, HeapFile, HeapId, HeapIndex, HeapStage, HeapSummary};
use fdt_store::run::Mode;
use tokio::sync::mpsc;

use crate::capture::{Session, SessionMsg};
use crate::config::CaptureConfig;
use crate::lifecycle::session_time;

/// Progress reaches the session at most this often, unless the stage changes.
const STAGE_EVERY: Duration = Duration::from_millis(100);
const DISCONNECTED: &str = "The app disconnected before the snapshot finished.";
const UNREADABLE: &str = "The snapshot couldn't be read.";

/// What an analysis job reports.
pub(crate) enum HeapMsg {
    Stage(HeapId, HeapStage),
    Analysed {
        id: HeapId,
        summary: Box<HeapSummary>,
        index: Box<HeapIndex>,
    },
    Cancelled(HeapId),
    Failed {
        id: HeapId,
        details: String,
    },
    /// `seq` tells the newest request from one that was overtaken.
    Diff {
        id: HeapId,
        seq: u64,
        diff: Option<Box<HeapDiff>>,
    },
}

struct Job {
    id: HeapId,
    cancel: Arc<AtomicBool>,
    /// Taken just now, not opened from the list: the app gets a profile for it.
    fresh: bool,
}

struct Queued {
    id: HeapId,
    file: Arc<HeapFile>,
    root_package: Option<String>,
    fresh: bool,
}

/// The analysis state of one session task.
pub(crate) struct HeapAnalyser {
    /// Bumped by every new session; names the files and tags the messages.
    pub(crate) session: u32,
    job: Option<Job>,
    queue: Vec<Queued>,
    diff_seq: u64,
}

impl HeapAnalyser {
    pub(crate) fn new() -> HeapAnalyser {
        HeapAnalyser {
            session: 0,
            job: None,
            queue: Vec::new(),
            diff_seq: 0,
        }
    }

    fn cancel_job(&self) {
        if let Some(j) = &self.job {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for HeapAnalyser {
    fn drop(&mut self) {
        self.cancel_job();
    }
}

/// Where snapshots are written: the configured folder, else the user's cache, else the temp
/// directory.
pub(crate) fn heap_dir(cfg: &CaptureConfig) -> PathBuf {
    cfg.heap_dir
        .clone()
        .or_else(|| dirs::cache_dir().map(|d| d.join("fdt").join("heap")))
        .unwrap_or_else(|| std::env::temp_dir().join("fdt-heap"))
}

/// Deletes snapshot files that a process which is no longer running left behind. Files are named
/// `<pid>-<session>-<id>.dartheap`; anything else in the folder is left alone.
pub(crate) fn remove_stale_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".dartheap")) else {
            continue;
        };
        let Some(pid) = stem.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if fdt_vm::discovery::pid_alive(pid) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            log::debug!("could not delete a stale heap file: {:?}", e.kind());
        }
    }
}

fn hub_owned(s: &HeapStage) -> bool {
    matches!(s, HeapStage::Requested | HeapStage::Receiving { .. })
}

fn analysing(s: &HeapStage) -> bool {
    s.is_busy() && !hub_owned(s)
}

impl Session {
    /// A snapshot is asked for. Ignored while one is going on, or with no app connected.
    pub(crate) fn take_heap(&mut self) {
        let (Some(m), Some(run), Some(w)) =
            (self.memory.clone(), self.current_run, self.writer.as_mut())
        else {
            return;
        };
        if w.busy_heap().is_some() {
            return;
        }
        let at = session_time(w);
        let id = w.heap_requested(run, &m.main_isolate(), at);
        log::debug!("heap snapshot {} requested", id.0);
        m.take(id, self.heap.session);
    }

    /// Cancels the snapshot being received or analysed.
    pub(crate) fn cancel_heap(&mut self) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let Some(id) = w.busy_heap() else {
            return;
        };
        let Some(stage) = w.heap_record(id).map(|r| r.stage.clone()) else {
            return;
        };
        if hub_owned(&stage) {
            if let Some(m) = &self.memory {
                m.cancel(self.heap.session, id);
            }
            w.set_heap_stage(id, HeapStage::Cancelled);
        } else if self.heap.job.as_ref().is_some_and(|j| j.id == id) {
            // The job answers with `Cancelled` (or a result, which is dropped).
            self.heap.cancel_job();
        } else if let Some(at) = self.heap.queue.iter().position(|q| q.id == id) {
            let q = self.heap.queue.remove(at);
            end_analysis(w, id, q.fresh, HeapStage::Cancelled);
        }
    }

    /// Analyses a `Ready` snapshot again from its file; it becomes the held one.
    pub(crate) fn open_heap(&mut self, id: HeapId) {
        let Some(w) = self.writer.as_ref() else {
            return;
        };
        if w.busy_heap().is_some() {
            return;
        }
        let Some(rec) = w.heap_record(id) else {
            return;
        };
        if rec.stage != HeapStage::Ready || (w.held_heap() == Some(id) && rec.index.is_some()) {
            return;
        }
        let Some(file) = rec.file.clone() else {
            return;
        };
        let root = rec.summary.as_ref().and_then(|s| s.root_package.as_deref());
        let root = root.map(str::to_string);
        self.start_analysis(id, file, root, false);
    }

    /// Removes a snapshot, stopping whatever is being done to it.
    pub(crate) fn remove_heap(&mut self, id: HeapId) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let Some(stage) = w.heap_record(id).map(|r| r.stage.clone()) else {
            return;
        };
        if hub_owned(&stage)
            && let Some(m) = &self.memory
        {
            m.cancel(self.heap.session, id);
        }
        if self.heap.job.as_ref().is_some_and(|j| j.id == id) {
            self.heap.cancel_job();
        }
        self.heap.queue.retain(|q| q.id != id);
        w.remove_heap(id);
        // The held snapshot compared itself with the one that is gone.
        let held = w.held_heap();
        let compared = held
            .and_then(|h| w.heap_record(h))
            .is_some_and(|r| r.diff.as_ref().is_some_and(|d| d.baseline == id));
        if let (Some(h), true) = (held, compared) {
            self.start_diff(h);
        }
    }

    /// Stores the user's choice of baseline and compares the held snapshot with it.
    pub(crate) fn set_heap_baseline(&mut self, id: Option<HeapId>) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        w.set_heap_baseline(id);
        if let Some(h) = w.held_heap() {
            self.start_diff(h);
        }
    }

    /// A new session: whatever the old one was doing is stopped and its results are stale.
    pub(crate) fn new_heap_session(&mut self) {
        if let (Some(w), Some(m)) = (self.writer.as_ref(), &self.memory)
            && let Some(id) = w.busy_heap()
            && w.heap_record(id).is_some_and(|r| hub_owned(&r.stage))
        {
            m.cancel(self.heap.session, id);
        }
        self.heap.cancel_job();
        self.heap.job = None;
        self.heap.queue.clear();
        self.heap.diff_seq += 1;
        self.heap.session += 1;
    }

    /// The connection ended: a snapshot still arriving can't finish.
    pub(crate) fn fail_unfinished_heaps(&mut self) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let ids: Vec<HeapId> = w
            .heap_records()
            .iter()
            .filter(|r| hub_owned(&r.stage))
            .map(|r| r.id)
            .collect();
        for id in ids {
            w.set_heap_stage(
                id,
                HeapStage::Failed {
                    message: DISCONNECTED.into(),
                    details: "".into(),
                },
            );
        }
    }

    pub(crate) fn heap_stage(&mut self, session: u32, id: HeapId, stage: HeapStage) {
        if session != self.heap.session {
            return;
        }
        if let Some(w) = self.writer.as_mut()
            && w.heap_record(id).is_some_and(|r| hub_owned(&r.stage))
        {
            w.set_heap_stage(id, stage);
        }
    }

    pub(crate) fn heap_failed(&mut self, session: u32, id: HeapId, message: &str, details: &str) {
        if session != self.heap.session {
            return;
        }
        if let Some(w) = self.writer.as_mut()
            && w.heap_record(id).is_some_and(|r| hub_owned(&r.stage))
        {
            w.set_heap_stage(
                id,
                HeapStage::Failed {
                    message: message.into(),
                    details: details.into(),
                },
            );
        }
    }

    /// The file is whole. A snapshot of an older session, or of a record that was cancelled or
    /// removed meanwhile, drops it here, which deletes it.
    pub(crate) fn heap_received(
        &mut self,
        session: u32,
        id: HeapId,
        time: fdt_store::time::SessionTime,
        bytes: u64,
        file: Arc<HeapFile>,
        root_package: Option<String>,
    ) {
        // Ids restart in every session: a snapshot of an older one is not this one's, and its
        // file goes as `file` drops.
        if session != self.heap.session {
            return;
        }
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        if !w.heap_record(id).is_some_and(|r| hub_owned(&r.stage)) {
            return;
        }
        w.set_heap_received(id, time, bytes, file.clone());
        self.start_analysis(id, file, root_package, true);
    }

    fn start_analysis(
        &mut self,
        id: HeapId,
        file: Arc<HeapFile>,
        root_package: Option<String>,
        fresh: bool,
    ) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        w.set_heap_stage(id, HeapStage::Parsing { fraction: 0.0 });
        if self.heap.job.is_some() {
            self.heap.queue.push(Queued {
                id,
                file,
                root_package,
                fresh,
            });
            return;
        }
        self.spawn_analysis(Queued {
            id,
            file,
            root_package,
            fresh,
        });
    }

    fn spawn_analysis(&mut self, q: Queued) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let Some(rec) = w.heap_record(q.id) else {
            return self.next_analysis();
        };
        let (flutter_version, mode) = w.run_info(rec.run).map_or((None, Mode::Unknown), |r| {
            (r.app.flutter_version.clone(), r.app.mode)
        });
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.heap.job = Some(Job {
            id: q.id,
            cancel: cancel.clone(),
            fresh: q.fresh,
        });
        let input = AnalyzeInput {
            file: q.file,
            flutter_version,
            mode,
            root_package: q.root_package,
        };
        let (session, id) = (self.heap.session, q.id);
        tokio::task::spawn_blocking(move || run_job(&tx, session, id, &input, &cancel));
    }

    fn next_analysis(&mut self) {
        while !self.heap.queue.is_empty() && self.heap.job.is_none() {
            let q = self.heap.queue.remove(0);
            self.spawn_analysis(q);
        }
    }

    /// Compares the held snapshot `id` with its baseline, off the session task.
    fn start_diff(&mut self, id: HeapId) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        self.heap.diff_seq += 1;
        let seq = self.heap.diff_seq;
        let heaps = w.heap_records();
        let base = default_baseline(heaps, id, w.heap_baseline()).filter(|b| *b != id);
        let pair = heaps.iter().find(|r| r.id == id).and_then(|after| {
            let baseline = heaps.iter().find(|r| Some(r.id) == base)?;
            Some((after.clone(), after.index.clone()?, baseline.clone()))
        });
        let Some((after, index, baseline)) = pair else {
            w.set_heap_diff(id, None);
            return;
        };
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        let session = self.heap.session;
        tokio::task::spawn_blocking(move || {
            let result = match diff(&after, &index, &baseline) {
                Ok(d) => Some(Box::new(d)),
                Err(e) => {
                    // The kind only: the message can name the user's directories.
                    log::debug!("heap diff {} failed: {:?}", id.0, e.kind());
                    None
                }
            };
            let _ = tx.send(SessionMsg::Heap(
                session,
                HeapMsg::Diff {
                    id,
                    seq,
                    diff: result,
                },
            ));
        });
    }

    /// A job's message. Called only for the current session.
    pub(crate) fn on_heap(&mut self, msg: HeapMsg) {
        match msg {
            HeapMsg::Stage(id, stage) => {
                // `Ready` is the job's last word; the record becomes ready with its result.
                if let Some(w) = self.writer.as_mut()
                    && stage.is_busy()
                    && self.heap.job.as_ref().is_some_and(|j| j.id == id)
                    && w.heap_record(id).is_some_and(|r| analysing(&r.stage))
                {
                    w.set_heap_stage(id, stage);
                }
            }
            HeapMsg::Diff { id, seq, diff } => {
                if seq != self.heap.diff_seq {
                    return;
                }
                if let Some(w) = self.writer.as_mut()
                    && w.heap_record(id).is_some()
                {
                    w.set_heap_diff(id, diff.map(|d| Arc::new(*d)));
                }
            }
            HeapMsg::Analysed { id, summary, index } => {
                let Some(job) = self.finish_job(id) else {
                    return;
                };
                let Some(w) = self.writer.as_mut() else {
                    return;
                };
                if !w.heap_record(id).is_some_and(|r| analysing(&r.stage)) {
                    return self.next_analysis();
                }
                if job.cancel.load(Ordering::Relaxed) {
                    end_analysis(w, id, job.fresh, HeapStage::Cancelled);
                    return self.next_analysis();
                }
                w.set_heap_analysed(id, Arc::new(*summary), Arc::new(*index));
                w.set_heap_stage(id, HeapStage::Ready);
                log::debug!("heap snapshot {} analysed", id.0);
                self.start_diff(id);
                if job.fresh
                    && let Some(m) = &self.memory
                {
                    m.fetch_profile(false);
                }
                self.next_analysis();
            }
            HeapMsg::Cancelled(id) => {
                if let Some(job) = self.finish_job(id)
                    && let Some(w) = self.writer.as_mut()
                    && w.heap_record(id).is_some_and(|r| analysing(&r.stage))
                {
                    end_analysis(w, id, job.fresh, HeapStage::Cancelled);
                }
                self.next_analysis();
            }
            HeapMsg::Failed { id, details } => {
                if let Some(job) = self.finish_job(id)
                    && let Some(w) = self.writer.as_mut()
                    && w.heap_record(id).is_some_and(|r| analysing(&r.stage))
                {
                    log::debug!("heap snapshot {} could not be analysed", id.0);
                    let failed = HeapStage::Failed {
                        message: UNREADABLE.into(),
                        details: details.into(),
                    };
                    end_analysis(w, id, job.fresh, failed);
                }
                self.next_analysis();
            }
        }
    }

    /// Ends the running job if it is `id`'s.
    fn finish_job(&mut self, id: HeapId) -> Option<Job> {
        if self.heap.job.as_ref().is_some_and(|j| j.id == id) {
            self.heap.job.take()
        } else {
            None
        }
    }
}

/// An analysis ended without a result. A snapshot just taken has nothing but its file, which
/// goes (the record stays, to say what happened). One that was opened again is still a good
/// record, so it goes back to `Ready`; only the held index stays as it was.
fn end_analysis(w: &mut fdt_store::SessionWriter, id: HeapId, fresh: bool, stage: HeapStage) {
    if fresh {
        w.drop_heap_file(id);
        w.set_heap_stage(id, stage);
    } else {
        w.set_heap_stage(id, HeapStage::Ready);
    }
}

fn run_job(
    tx: &mpsc::UnboundedSender<SessionMsg>,
    session: u32,
    id: HeapId,
    input: &AnalyzeInput,
    cancel: &AtomicBool,
) {
    let send = |m: HeapMsg| {
        let _ = tx.send(SessionMsg::Heap(session, m));
    };
    let mut last: Option<(std::mem::Discriminant<HeapStage>, Instant)> = None;
    let result = analyze(input, cancel, &mut |stage| {
        let kind = std::mem::discriminant(&stage);
        let due = last.is_none_or(|(k, at)| k != kind || at.elapsed() >= STAGE_EVERY);
        if due {
            last = Some((kind, Instant::now()));
            send(HeapMsg::Stage(id, stage));
        }
    });
    match result {
        Ok(a) => send(HeapMsg::Analysed {
            id,
            summary: Box::new(a.summary),
            index: Box::new(a.index),
        }),
        Err(HeapError::Cancelled) => send(HeapMsg::Cancelled(id)),
        Err(e) => send(HeapMsg::Failed {
            id,
            details: e.to_string(),
        }),
    }
}
