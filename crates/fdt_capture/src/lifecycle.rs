//! The connection lifecycle: what happens when the app stops, reconnects, restarts, pauses
//! recording or starts a new session. Runs on the session task, as part of [`Session`].

use fdt_store::SessionWriter;
use fdt_store::clock::{RunClock, host_now_us};
use fdt_store::run::AppInfo;
use fdt_store::time::{RunId, SessionTime};
use fdt_vm::discovery::{AppKey, DiscoveredApp};
use fdt_vm::uri::VmServiceUri;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::capture::{Session, Target};
use crate::connect;
use crate::ingest::http::PollReply;
use crate::ingest::{Ingest, SharedSink, Sink};
use crate::status::ConnStatus;

/// Which VM a run belongs to, to tell a dropped connection from a restarted app.
#[derive(Clone)]
pub(crate) struct VmIdent {
    run: RunId,
    pid: Option<u32>,
    start_time: Option<i64>,
    clock: RunClock,
}

/// Retrying the same URL after the app stopped.
pub(crate) struct UriRetry {
    uri: VmServiceUri,
    /// When the next attempt is due; `None` while one is in flight.
    next: Option<Instant>,
    give_up: Instant,
}

/// Retrying a discovered app that is listed but would not connect.
pub(crate) struct DiscRetry {
    next: Option<Instant>,
    give_up: Instant,
}

/// Session state that exists only for the lifecycle rules.
pub(crate) struct Lifecycle {
    /// What the last connect command asked for.
    target: Option<Target>,
    waiting_for: Option<AppKey>,
    retry: Option<UriRetry>,
    disc_retry: Option<DiscRetry>,
    /// The last discovered list, to look for the app we wait for.
    pub(crate) apps: std::sync::Arc<[DiscoveredApp]>,
    /// The generation the live connection started with. Its own `Closed` and `Failed` carry
    /// that generation even after a new session bumped the current one.
    floor: Option<u64>,
    /// The connection in progress was started by us (a reconnect), not by the user.
    auto: bool,
    /// The status to go back to when an automatic attempt fails.
    restore: Option<ConnStatus>,
    pub(crate) recording: bool,
    last_vm: Option<VmIdent>,
    /// The app of the current or last run, kept for the status after it stops.
    pub(crate) app: Option<AppInfo>,
    /// The sink the connection uses; swapped when a new session starts on it.
    shared: Option<SharedSink>,
}

impl Lifecycle {
    pub(crate) fn new() -> Lifecycle {
        Lifecycle {
            target: None,
            waiting_for: None,
            retry: None,
            disc_retry: None,
            apps: std::sync::Arc::from(Vec::new()),
            floor: None,
            auto: false,
            restore: None,
            recording: true,
            last_vm: None,
            app: None,
            shared: None,
        }
    }

    /// When the session task should wake to retry the URL.
    pub(crate) fn retry_due(&self) -> Option<Instant> {
        let a = self.retry.as_ref().and_then(|r| r.next);
        let b = self.disc_retry.as_ref().and_then(|r| r.next);
        [a, b].into_iter().flatten().min()
    }

    /// Whether a `Closed`/`Failed` from generation `g` ends the live connection.
    pub(crate) fn ends_live_connection(&self, g: u64, current: u64) -> bool {
        self.floor.is_some_and(|f| g >= f && g <= current)
    }
}

pub(crate) fn session_time(w: &SessionWriter) -> SessionTime {
    let us = (host_now_us() - w.origin_host_us()).max(0) as u64;
    SessionTime(us.saturating_mul(1000))
}

impl Session {
    /// Starts connecting. `auto` marks a reconnect we start ourselves.
    pub(crate) fn begin_connect(&mut self, target: Target, auto: bool) {
        self.finish_run();
        if let Some(old) = self.conn.take() {
            old.abort();
        }
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        self.conn_gen += 1;
        self.net.set_fetcher(None);
        let label = connect::label_of(&target);
        let quiet = auto && matches!(target, Target::Uri(_));
        let previous = self.status.borrow().clone();
        if !auto {
            self.life.waiting_for = None;
            self.life.retry = None;
            self.life.disc_retry = None;
        }
        self.life.floor = Some(self.conn_gen);
        self.life.auto = auto;
        self.life.restore = auto.then_some(previous);
        self.life.target = Some(target.clone());
        if !quiet {
            self.status.send_replace(ConnStatus::Connecting { label });
        }
        self.conn = Some(tokio::spawn(connect::run(
            target,
            self.cfg.clone(),
            self.conn_gen,
            tx,
            self.free_app_memory.clone(),
        )));
    }

    /// Ends the open run, if any, and makes everything from the connection stale.
    fn finish_run(&mut self) {
        self.life.floor = None;
        let Some(run) = self.current_run.take() else {
            return;
        };
        self.conn_gen += 1;
        self.life.shared = None;
        self.perf = None;
        if let Some(w) = &mut self.writer {
            let at = session_time(w);
            w.end_run(run, at);
            self.net.end_run(w, at);
        }
    }

    fn set_status_if_changed(&self, new: ConnStatus) {
        self.status.send_if_modified(|s| {
            if *s == new {
                false
            } else {
                *s = new;
                true
            }
        });
    }

    fn disconnected(&self, reason: String) -> ConnStatus {
        ConnStatus::Disconnected {
            app: self.life.app.clone(),
            reason,
            waiting_for: self.life.waiting_for.clone(),
        }
    }

    /// The socket closed on its own: the app stopped (or the link dropped).
    pub(crate) fn on_closed(&mut self) {
        self.finish_run();
        if let Some(c) = self.conn.take() {
            c.abort();
        }
        let name = self
            .life
            .app
            .as_ref()
            .map_or("The app", |a| a.name.as_str());
        let reason = format!("{name} stopped.");
        match self.life.target.clone() {
            Some(Target::Discovered(a)) => {
                self.life.waiting_for = Some(a.key);
                // The app may still be listed (a dropped link): look again after a moment.
                let now = Instant::now();
                self.life.disc_retry = Some(DiscRetry {
                    next: Some(now + self.cfg.reconnect_uri_retry),
                    give_up: now + self.cfg.reconnect_uri_give_up,
                });
            }
            Some(Target::Uri(uri)) => {
                let now = Instant::now();
                self.life.retry = Some(UriRetry {
                    uri,
                    next: Some(now + self.cfg.reconnect_uri_retry),
                    give_up: now + self.cfg.reconnect_uri_give_up,
                });
            }
            None => {}
        }
        let st = self.disconnected(reason);
        self.status.send_replace(st);
    }

    /// The connect sequence failed.
    pub(crate) fn on_failed(&mut self, message: String, details: String) {
        self.finish_run();
        if let Some(c) = self.conn.take() {
            c.abort();
        }
        if !self.life.auto {
            self.status
                .send_replace(ConnStatus::Failed { message, details });
            return;
        }
        // A reconnect that failed: the app is simply still gone.
        log::debug!("reconnect failed: {message}");
        self.life.auto = false;
        if let Some(prev) = self.life.restore.take() {
            self.set_status_if_changed(prev);
        }
        let next = Instant::now() + self.cfg.reconnect_uri_retry;
        if let Some(r) = &mut self.life.retry {
            if next > r.give_up {
                self.life.retry = None;
            } else {
                r.next = Some(next);
            }
        }
        if let Some(r) = &mut self.life.disc_retry {
            if next > r.give_up {
                self.life.disc_retry = None;
            } else {
                r.next = Some(next);
            }
        }
    }

    /// The URL retry timer fired.
    pub(crate) fn retry_tick(&mut self) {
        let now = Instant::now();
        if let Some(r) = &mut self.life.disc_retry
            && r.next.is_some_and(|n| n <= now)
        {
            r.next = None;
            if now >= r.give_up {
                self.life.disc_retry = None;
            } else {
                let apps = self.life.apps.clone();
                self.on_discovery_list(&apps);
            }
            return;
        }
        let Some(r) = &mut self.life.retry else {
            return;
        };
        if r.next.take().is_none() {
            return;
        }
        if now >= r.give_up {
            self.life.retry = None;
            return;
        }
        let uri = r.uri.clone();
        self.begin_connect(Target::Uri(uri), true);
    }

    /// The discovered apps changed: connect to the one we are waiting for.
    pub(crate) fn on_discovery(&mut self, apps: std::sync::Arc<[DiscoveredApp]>) {
        self.life.apps = apps.clone();
        self.on_discovery_list(&apps);
    }

    fn on_discovery_list(&mut self, apps: &[DiscoveredApp]) {
        let waiting = matches!(
            *self.status.borrow(),
            ConnStatus::Disconnected {
                waiting_for: Some(_),
                ..
            }
        );
        let Some(key) = self.life.waiting_for.clone().filter(|_| waiting) else {
            return;
        };
        if let Some(app) = apps.iter().find(|a| a.key == key) {
            self.begin_connect(Target::Discovered(app.clone()), true);
        }
    }

    pub(crate) fn disconnect(&mut self) {
        let had_connection = !matches!(
            *self.status.borrow(),
            ConnStatus::Idle | ConnStatus::Failed { .. }
        );
        self.finish_run();
        if let Some(c) = self.conn.take() {
            c.abort();
        }
        self.conn_gen += 1;
        self.life.waiting_for = None;
        self.life.retry = None;
        self.life.disc_retry = None;
        self.life.auto = false;
        self.life.restore = None;
        if had_connection {
            let st = self.disconnected("Disconnected.".to_string());
            self.status.send_replace(st);
        }
    }

    pub(crate) fn stop_waiting(&mut self) {
        self.life.waiting_for = None;
        self.life.retry = None;
        self.life.disc_retry = None;
        if self.life.auto {
            // A reconnect is in flight (quiet for a URL); call it off.
            if let Some(c) = self.conn.take() {
                c.abort();
            }
            self.conn_gen += 1;
            self.life.floor = None;
            self.life.auto = false;
            if let Some(prev) = self.life.restore.take() {
                self.set_status_if_changed(prev);
            }
        }
        self.status.send_if_modified(|s| match s {
            ConnStatus::Disconnected { waiting_for, .. } if waiting_for.is_some() => {
                *waiting_for = None;
                true
            }
            _ => false,
        });
    }

    pub(crate) fn set_recording(&mut self, on: bool) {
        self.life.recording = on;
        self.status.send_if_modified(|s| match s {
            ConnStatus::Connected { recording, .. } if *recording != on => {
                *recording = on;
                true
            }
            _ => false,
        });
    }

    /// The first message of a connection: opens (or reopens) its run.
    pub(crate) fn start_run(
        &mut self,
        app: AppInfo,
        vm_start_time: Option<i64>,
        calibration: fdt_store::clock::Calibration,
        refresh_hz: Option<f32>,
        connect_timeline_us: i64,
        reply: oneshot::Sender<SharedSink>,
    ) {
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        let limits = self.cfg.body_limits;
        let w = self.writer.get_or_insert_with(|| {
            let mut w = SessionWriter::new(host_now_us());
            w.set_body_limits(limits);
            w
        });
        let same = self.life.last_vm.clone().filter(|v| {
            v.pid.is_some()
                && v.start_time.is_some()
                && v.pid == app.pid
                && v.start_time == vm_start_time
        });
        let (id, clock, app) = match same {
            // The same VM again: the app never stopped, only the connection did.
            Some(v) => {
                let mut app = app;
                if app.flutter_version.is_none() {
                    app.flutter_version = self
                        .life
                        .app
                        .as_ref()
                        .and_then(|a| a.flutter_version.clone());
                }
                w.reopen_run(v.run);
                let stored = app.clone();
                w.update_run_app(v.run, |a| *a = stored);
                (v.run, v.clock, app)
            }
            None => {
                let clock = RunClock::new(w.origin_host_us(), calibration);
                let id = w.start_run(app.clone(), clock, session_time(w));
                (id, clock, app)
            }
        };
        if let Some(hz) = refresh_hz {
            w.set_refresh_rate(id, hz);
        }
        self.life.last_vm = Some(VmIdent {
            run: id,
            pid: app.pid,
            start_time: vm_start_time,
            clock,
        });
        self.life.waiting_for = None;
        self.life.retry = None;
        self.life.disc_retry = None;
        self.life.auto = false;
        self.life.restore = None;
        self.life.app = Some(app.clone());
        self.current_run = Some(id);
        self.net.new_run();
        let shared = SharedSink::new(Sink::new(id, clock, self.conn_gen, connect_timeline_us, tx));
        self.life.shared = Some(shared.clone());
        self.status.send_replace(ConnStatus::Connected {
            run: id,
            app,
            recording: self.life.recording,
        });
        let _ = reply.send(shared);
    }

    /// Throws the stored session away. If connected, run 1 of the new session starts at once on
    /// the same connection, with the same calibration.
    pub(crate) fn new_session(&mut self) {
        if self.writer.is_none() {
            return;
        }
        let mut w = SessionWriter::new(host_now_us());
        w.set_body_limits(self.cfg.body_limits);
        self.net.new_session();
        let old_vm = self.life.last_vm.take();
        if let (Some(_), Some(shared), Some(app), Some(tx)) = (
            self.current_run,
            self.life.shared.clone(),
            self.life.app.clone(),
            self.tx.upgrade(),
        ) {
            let clock = RunClock {
                origin_host_us: w.origin_host_us(),
                ..shared.get().clock
            };
            let id = w.start_run(app.clone(), clock, session_time(&w));
            // Everything the old session's work still sends is stale from here on.
            self.conn_gen += 1;
            shared.set(Sink::new(
                id,
                clock,
                self.conn_gen,
                shared.get().connect_timeline_us,
                tx,
            ));
            self.current_run = Some(id);
            self.life.last_vm = Some(VmIdent {
                run: id,
                pid: app.pid,
                start_time: old_vm.and_then(|v| v.start_time),
                clock,
            });
            self.status.send_replace(ConnStatus::Connected {
                run: id,
                app,
                recording: self.life.recording,
            });
        }
        self.writer = Some(w);
        self.publish_now();
    }

    /// An ingest message from a connection that is no longer current. A poller waiting for its
    /// answer must still get one, or it would stop.
    pub(crate) fn on_stale(&mut self, i: Ingest) {
        let i = match i {
            Ingest::HttpPoll { reply, .. } => {
                let _ = reply.send(PollReply {
                    fetch: Vec::new(),
                    incomplete: false,
                    has_websocket: false,
                });
                return;
            }
            i => i,
        };
        // Everything else from a connection that is no longer current is dropped. Listed so a
        // new variant makes this match fail to compile.
        match i {
            Ingest::Timeline(_) | Ingest::Cpu(_) | Ingest::CpuStatus(..) | Ingest::PerfReady(_) => {
            }
            Ingest::StartRun { .. }
            | Ingest::Failed { .. }
            | Ingest::Closed
            | Ingest::Log(_)
            | Ingest::Frame(_)
            | Ingest::FlutterVersion { .. }
            | Ingest::NetReady(_)
            | Ingest::HttpPoll { .. }
            | Ingest::Body { .. }
            | Ingest::Sockets(_)
            | Ingest::IsolateExit { .. } => {}
        }
    }
}
