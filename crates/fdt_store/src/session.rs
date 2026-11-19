//! The single writer and the immutable snapshots views read.

use crate::clock::RunClock;
use crate::logs::{LOG_CHUNK, LogInput, LogStore, LogsSnapshot};
use crate::net::{
    BodyKind, BodyLimits, BodyState, HttpUpdate, NetSnapshot, NetStore, RequestUpsert, SocketUpdate,
};
use crate::run::{AppInfo, RunInfo};
use crate::time::{EntryId, RunId, SessionTime};
use std::sync::Arc;

/// An immutable, cheaply clonable view of the session at one generation.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub generation: u64,
    pub origin_host_us: i64,
    pub runs: Arc<[RunInfo]>,
    pub logs: LogsSnapshot,
    pub net: NetSnapshot,
    latest: SessionTime,
}

impl Snapshot {
    /// A valid generation-0 snapshot with no runs.
    pub fn empty() -> Self {
        Self {
            generation: 0,
            origin_host_us: 0,
            runs: Arc::from(Vec::new()),
            logs: LogsSnapshot::default(),
            net: NetSnapshot::default(),
            latest: SessionTime(0),
        }
    }

    pub fn run(&self, id: RunId) -> Option<&RunInfo> {
        self.runs.iter().find(|r| r.id == id)
    }

    /// The latest stored event time or run boundary.
    pub fn latest_time(&self) -> SessionTime {
        self.latest
    }
}

pub struct SessionWriter {
    origin_host_us: i64,
    runs: Vec<RunInfo>,
    logs: LogStore,
    net: NetStore,
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

    fn with_log_chunk(origin_host_us: i64, chunk: usize) -> Self {
        let mut empty = Snapshot::empty();
        empty.origin_host_us = origin_host_us;
        Self {
            origin_host_us,
            runs: Vec::new(),
            logs: LogStore::new(chunk),
            net: NetStore::new(chunk),
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

    pub fn set_body_state(&mut self, id: EntryId, kind: BodyKind, state: BodyState) {
        self.net.set_body_state(id, kind, state);
        self.dirty = true;
    }

    /// Stores a body under the limits, evicting the oldest bodies first.
    pub fn store_body(&mut self, id: EntryId, kind: BodyKind, bytes: Vec<u8>) {
        self.net.store_body(id, kind, bytes);
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
                generation: self.generation,
                origin_host_us: self.origin_host_us,
                runs: self.runs.clone().into(),
                logs: self.logs.snapshot(),
                net: self.net.snapshot(self.latest),
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
}
