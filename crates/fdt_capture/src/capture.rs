//! The `Capture` handle and the session task that owns the store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fdt_store::net::{BodyKind, BodyLimits};
use fdt_store::time::{EntryId, RunId};
use fdt_store::{SessionWriter, Snapshot};
use fdt_vm::discovery::DiscoveredApp;
use fdt_vm::uri::VmServiceUri;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use crate::config::CaptureConfig;
use crate::ingest::Ingest;
use crate::ingest::http::{NetRestore, NetState};
use crate::ingest::perf::{self, PerfControl};
use crate::lifecycle::Lifecycle;
use crate::status::ConnStatus;

/// What to connect to.
#[derive(Clone)]
pub enum Target {
    Discovered(DiscoveredApp),
    Uri(VmServiceUri),
}

/// A command from the app. Handled in one place: [`Session::command`].
pub(crate) enum Command {
    Connect(Target),
    Disconnect,
    StopWaiting,
    SetRecording(bool),
    NewSession,
    LoadFullBody(EntryId, BodyKind),
    SetFreeAppHttpMemory(bool),
    SetBodyLimits(BodyLimits),
    FetchCpuNow,
    SetTraceWidgetBuilds(bool),
    /// The settings' default for widget-build tracing changed.
    SetTraceDefault(bool),
    /// Sent when the `Capture` is dropped.
    Shutdown,
}

pub(crate) enum SessionMsg {
    Command(Command),
    /// From the connection task of generation `.0`; stale generations are ignored.
    Ingest(u64, Ingest),
}

/// The app's handle on capture. Cheap to use from any thread; nothing here blocks.
pub struct Capture {
    tx: mpsc::UnboundedSender<SessionMsg>,
    status: watch::Receiver<ConnStatus>,
    snaps: watch::Receiver<Arc<Snapshot>>,
}

impl Capture {
    /// Starts the session task on `handle` with an empty session and status `Idle`.
    pub fn start(
        handle: Handle,
        config: CaptureConfig,
        discovery: Option<watch::Receiver<Arc<[DiscoveredApp]>>>,
    ) -> Capture {
        let (tx, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ConnStatus::Idle);
        let (snap_tx, snaps) = watch::channel(Arc::new(Snapshot::empty()));
        let free_app_memory = Arc::new(AtomicBool::new(config.free_app_http_memory));
        let session = Session::new(
            config,
            discovery,
            tx.downgrade(),
            status_tx,
            snap_tx,
            free_app_memory,
        );
        handle.spawn(session.run(rx));
        Capture { tx, status, snaps }
    }

    fn command(&self, c: Command) {
        let _ = self.tx.send(SessionMsg::Command(c));
    }

    pub fn connect(&self, target: Target) {
        self.command(Command::Connect(target));
    }

    pub fn disconnect(&self) {
        self.command(Command::Disconnect);
    }

    pub fn stop_waiting(&self) {
        self.command(Command::StopWaiting);
    }

    pub fn set_recording(&self, on: bool) {
        self.command(Command::SetRecording(on));
    }

    pub fn new_session(&self) {
        self.command(Command::NewSession);
    }

    pub fn load_full_body(&self, id: EntryId, kind: BodyKind) {
        self.command(Command::LoadFullBody(id, kind));
    }

    pub fn set_free_app_http_memory(&self, on: bool) {
        self.command(Command::SetFreeAppHttpMemory(on));
    }

    pub fn set_body_limits(&self, limits: BodyLimits) {
        self.command(Command::SetBodyLimits(limits));
    }

    /// Fetches CPU samples now, whether or not frames were drawn since the last fetch.
    pub fn fetch_cpu_samples(&self) {
        self.command(Command::FetchCpuNow);
    }

    /// Traces widget builds in the app (about +30 % build time while on) and takes the timeline
    /// from the `Timeline` stream. Kept across connects and hot restarts.
    pub fn set_trace_widget_builds(&self, on: bool) {
        self.command(Command::SetTraceWidgetBuilds(on));
    }

    /// The default for widget-build tracing changed (the "Trace widget builds by default"
    /// setting): the next connection starts with it. A live connection is left as it is.
    pub fn set_trace_default(&self, on: bool) {
        self.command(Command::SetTraceDefault(on));
    }

    pub fn status(&self) -> watch::Receiver<ConnStatus> {
        self.status.clone()
    }

    pub fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snaps.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Connection tasks hold senders too, so the channel would never close by itself.
        self.command(Command::Shutdown);
    }
}

pub(crate) struct Session {
    pub(crate) cfg: CaptureConfig,
    discovery: Option<watch::Receiver<Arc<[DiscoveredApp]>>>,
    /// Weak: only connection tasks (and the `Capture`) keep the channel open.
    pub(crate) tx: mpsc::WeakUnboundedSender<SessionMsg>,
    /// Only the session task writes the status.
    pub(crate) status: watch::Sender<ConnStatus>,
    /// The current connection's generation; messages from older ones are stale.
    pub(crate) conn_gen: u64,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    /// Created at the first successful connect, because its origin is that moment.
    pub(crate) writer: Option<SessionWriter>,
    pub(crate) conn: Option<JoinHandle<()>>,
    pub(crate) current_run: Option<RunId>,
    last_publish: Option<Instant>,
    pub(crate) net: NetState,
    /// Read live by the pollers.
    pub(crate) free_app_memory: Arc<AtomicBool>,
    pub(crate) life: Lifecycle,
    /// The live connection's timeline task, for commands.
    pub(crate) perf: Option<Arc<PerfControl>>,
    /// What to put back in the live connection's app when the user disconnects.
    pub(crate) net_restore: Option<Arc<NetRestore>>,
    /// Whether widget builds should be traced, whatever the connection (from the settings, then
    /// the command).
    wanted_tracing: bool,
    /// The default changed while a connection was live: the next user connect starts from it.
    trace_default_pending: bool,
    /// Whether the app answered that tracing is on, for runs that start on this connection.
    pub(crate) tracing_actual: bool,
}

impl Session {
    fn new(
        cfg: CaptureConfig,
        discovery: Option<watch::Receiver<Arc<[DiscoveredApp]>>>,
        tx: mpsc::WeakUnboundedSender<SessionMsg>,
        status: watch::Sender<ConnStatus>,
        snap_tx: watch::Sender<Arc<Snapshot>>,
        free_app_memory: Arc<AtomicBool>,
    ) -> Session {
        Session {
            wanted_tracing: cfg.trace_widget_builds,
            trace_default_pending: false,
            cfg,
            discovery,
            tx,
            status,
            conn_gen: 0,
            snap_tx,
            writer: None,
            conn: None,
            current_run: None,
            last_publish: None,
            net: NetState::default(),
            free_app_memory,
            life: Lifecycle::new(),
            perf: None,
            net_restore: None,
            tracing_actual: false,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<SessionMsg>) {
        let mut due: Option<Instant> = None;
        let mut discovery = self.discovery.take();
        if let Some(d) = &discovery {
            self.life.apps = d.borrow().clone();
        }
        loop {
            let retry_due = self.life.retry_due();
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(SessionMsg::Command(Command::Shutdown)) => {
                        self.restore_profiling().await;
                        break;
                    }
                    // A user connect leaves the current app (if live) like a disconnect does.
                    // Automatic reconnects and retries call `begin_connect` directly.
                    Some(SessionMsg::Command(Command::Connect(t))) => {
                        self.restore_profiling().await;
                        if std::mem::take(&mut self.trace_default_pending) {
                            self.wanted_tracing = self.cfg.trace_widget_builds;
                        }
                        self.begin_connect(t, false);
                    }
                    Some(SessionMsg::Command(Command::Disconnect)) => {
                        self.restore_profiling().await;
                        self.disconnect();
                    }
                    Some(SessionMsg::Command(c)) => self.command(c),
                    Some(SessionMsg::Ingest(g, i))
                        if g == self.conn_gen || self.ends_live_connection(g, &i) => self.ingest(i),
                    Some(SessionMsg::Ingest(g, i)) => {
                        log::debug!("ignoring stale connection {g}");
                        self.on_stale(i);
                    }
                    None => break,
                },
                _ = async {
                    match due {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
                _ = async {
                    match retry_due {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => self.retry_tick(),
                changed = async {
                    match &mut discovery {
                        Some(d) => d.changed().await.map(|()| d.borrow_and_update().clone()),
                        None => std::future::pending().await,
                    }
                } => match changed {
                    Ok(apps) => self.on_discovery(apps),
                    // The discovery side is gone; nothing more will arrive.
                    Err(_) => discovery = None,
                },
            }
            due = self.maybe_publish();
        }
        if let Some(c) = self.conn.take() {
            c.abort();
        }
    }

    /// Publishes if the store changed and the interval has passed. Otherwise returns when to
    /// try again; `None` when nothing is waiting.
    fn maybe_publish(&mut self) -> Option<Instant> {
        let writer = self.writer.as_mut()?;
        if !writer.is_dirty() {
            return None;
        }
        let now = Instant::now();
        let due = self
            .last_publish
            .map_or(now, |t| t + self.cfg.publish_interval);
        if now < due {
            return Some(due);
        }
        self.snap_tx.send_replace(writer.publish());
        self.last_publish = Some(now);
        None
    }

    fn ends_live_connection(&self, g: u64, i: &Ingest) -> bool {
        matches!(i, Ingest::Closed | Ingest::Failed { .. })
            && self.life.ends_live_connection(g, self.conn_gen)
    }

    /// Publishes now, whatever the interval.
    pub(crate) fn publish_now(&mut self) {
        if let Some(w) = &mut self.writer {
            self.snap_tx.send_replace(w.publish());
            self.last_publish = Some(Instant::now());
        }
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::Connect(_) => {} // handled by the run loop, which restores first
            Command::SetBodyLimits(l) => {
                self.cfg.body_limits = l;
                if let Some(w) = &mut self.writer {
                    w.set_body_limits(l);
                }
            }
            Command::Shutdown => {} // handled by the run loop
            Command::FetchCpuNow => {
                if let Some(p) = &self.perf {
                    p.fetch_now();
                }
            }
            Command::Disconnect => {} // handled by the run loop, which restores first
            Command::SetTraceWidgetBuilds(on) => {
                self.wanted_tracing = on;
                if let Some(p) = &self.perf {
                    p.set_tracing(on);
                }
            }
            Command::SetTraceDefault(on) => {
                self.cfg.trace_widget_builds = on;
                // No live connection: the next one starts with it. A live one keeps what it has
                // until the user connects again.
                if self.perf.is_none() {
                    self.wanted_tracing = on;
                } else {
                    self.trace_default_pending = true;
                }
            }
            Command::StopWaiting => self.stop_waiting(),
            Command::SetRecording(on) => self.set_recording(on),
            Command::NewSession => self.new_session(),
            Command::LoadFullBody(id, kind) => self.net.load_full(id, kind, self.current_run),
            Command::SetFreeAppHttpMemory(on) => {
                self.cfg.free_app_http_memory = on;
                self.free_app_memory.store(on, Ordering::Relaxed);
            }
        }
    }

    fn ingest(&mut self, i: Ingest) {
        match i {
            Ingest::StartRun {
                app,
                vm_start_time,
                calibration,
                refresh_hz,
                connect_timeline_us,
                reply,
            } => {
                return self.start_run(
                    *app,
                    vm_start_time,
                    calibration,
                    refresh_hz,
                    connect_timeline_us,
                    reply,
                );
            }
            Ingest::Failed { message, details } => return self.on_failed(message, details),
            Ingest::Closed => return self.on_closed(),
            _ => {}
        }
        let recording = self.life.recording;
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        match i {
            Ingest::StartRun { .. } | Ingest::Failed { .. } | Ingest::Closed => {}
            Ingest::Log(l) => {
                if recording {
                    w.push_log(l);
                }
            }
            Ingest::Frame(f) => {
                if recording {
                    w.push_frame(f);
                }
            }
            Ingest::NetReady(f) => self.net.set_fetcher(Some(f)),
            Ingest::Timeline(b) => perf::store(w, b, recording),
            Ingest::Cpu(b) => perf::store_cpu(w, b, recording),
            Ingest::CpuStatus(run, status) => w.set_cpu_status(run, status),
            Ingest::PerfReady(p) => {
                if self.wanted_tracing {
                    p.set_tracing(true);
                }
                self.perf = Some(p);
            }
            Ingest::NetRestoreReady(n) => self.net_restore = Some(n),
            Ingest::Tracing(run, on) => {
                self.tracing_actual = on;
                // Frames from before this moment were recorded with the old state.
                let at = crate::lifecycle::session_time(w);
                w.set_tracing(run, on, at);
            }
            Ingest::HttpPoll {
                isolate,
                updates,
                reply,
            } => {
                let r = self
                    .net
                    .poll(w, self.cfg.auto_fetch_limit, &isolate, updates, recording);
                let _ = reply.send(r);
            }
            Ingest::Body { id, result } => self.net.body(w, id, result),
            Ingest::Sockets(updates) => self.net.sockets(w, recording, updates),
            Ingest::IsolateExit { isolate } => {
                let at = crate::lifecycle::session_time(w);
                self.net.isolate_exit(w, &isolate, at);
            }
            Ingest::FlutterVersion { run, version } => {
                w.update_run_app(run, |a| a.flutter_version = Some(version.clone()));
                if let Some(a) = &mut self.life.app {
                    a.flutter_version = Some(version.clone());
                }
                // Keep the status in step with the stored app.
                self.status.send_if_modified(|st| match st {
                    ConnStatus::Connected { run: r, app, .. } if *r == run => {
                        app.flutter_version = Some(version);
                        true
                    }
                    _ => false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Session, mpsc::UnboundedSender<SessionMsg>) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (status, _) = watch::channel(ConnStatus::Idle);
        let (snap_tx, _) = watch::channel(Arc::new(Snapshot::empty()));
        let s = Session::new(
            CaptureConfig::default(),
            None,
            tx.downgrade(),
            status,
            snap_tx,
            Arc::new(AtomicBool::new(false)),
        );
        (s, tx)
    }

    #[test]
    fn stale_connection_cannot_overwrite_status() {
        let (mut s, _tx) = session();
        s.conn_gen = 2;
        s.status
            .send_replace(ConnStatus::Connecting { label: "b".into() });
        // The run loop drops messages whose generation is not current; mirror its check.
        let deliver = |s: &mut Session, g: u64| {
            if g == s.conn_gen {
                s.ingest(Ingest::Failed {
                    message: format!("from {g}"),
                    details: String::new(),
                });
            }
        };
        deliver(&mut s, 1);
        assert!(matches!(*s.status.borrow(), ConnStatus::Connecting { .. }));
        deliver(&mut s, 2);
        assert!(
            matches!(&*s.status.borrow(), ConnStatus::Failed { message, .. } if message == "from 2")
        );
    }
}
