//! The `Capture` handle and the session task that owns the store.

use std::sync::Arc;

use fdt_store::clock::host_now_us;
use fdt_store::net::{BodyKind, BodyLimits};
use fdt_store::time::{EntryId, RunId, SessionTime};
use fdt_store::{SessionWriter, Snapshot};
use fdt_vm::discovery::DiscoveredApp;
use fdt_vm::uri::VmServiceUri;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use crate::config::CaptureConfig;
use crate::connect;
use crate::ingest::Ingest;
use crate::status::ConnStatus;

/// What to connect to.
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
}

pub(crate) enum SessionMsg {
    Command(Command),
    Ingest(Ingest),
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
        let session = Session {
            cfg: config,
            _discovery: discovery,
            tx: tx.clone(),
            status: Arc::new(status_tx),
            snap_tx,
            writer: None,
            conn: None,
            current_run: None,
            last_publish: None,
        };
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

    pub fn status(&self) -> watch::Receiver<ConnStatus> {
        self.status.clone()
    }

    pub fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snaps.clone()
    }
}

struct Session {
    cfg: CaptureConfig,
    _discovery: Option<watch::Receiver<Arc<[DiscoveredApp]>>>,
    tx: mpsc::UnboundedSender<SessionMsg>,
    status: Arc<watch::Sender<ConnStatus>>,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    /// Created at the first connect, because its origin is that moment.
    writer: Option<SessionWriter>,
    conn: Option<JoinHandle<()>>,
    current_run: Option<RunId>,
    last_publish: Option<Instant>,
}

impl Session {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<SessionMsg>) {
        let mut due: Option<Instant> = None;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(SessionMsg::Command(c)) => self.command(c),
                    Some(SessionMsg::Ingest(i)) => self.ingest(i),
                    None => break,
                },
                _ = async {
                    match due {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
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

    fn command(&mut self, c: Command) {
        match c {
            Command::Connect(target) => self.connect(target),
            Command::SetBodyLimits(l) => {
                self.cfg.body_limits = l;
                if let Some(w) = &mut self.writer {
                    w.set_body_limits(l);
                }
            }
            // Filled in by later tasks; accepted and ignored for now.
            Command::Disconnect => log::debug!("command: disconnect"),
            Command::StopWaiting => log::debug!("command: stop_waiting"),
            Command::SetRecording(on) => log::debug!("command: set_recording({on})"),
            Command::NewSession => log::debug!("command: new_session"),
            Command::LoadFullBody(id, kind) => {
                log::debug!("command: load_full_body({id:?}, {kind:?})")
            }
            Command::SetFreeAppHttpMemory(on) => {
                self.cfg.free_app_http_memory = on;
                log::debug!("command: set_free_app_http_memory({on})")
            }
        }
    }

    fn connect(&mut self, target: Target) {
        if let Some(old) = self.conn.take() {
            old.abort();
        }
        let limits = self.cfg.body_limits;
        let writer = self.writer.get_or_insert_with(|| {
            let mut w = SessionWriter::new(host_now_us());
            w.set_body_limits(limits);
            w
        });
        let origin = writer.origin_host_us();
        self.status.send_replace(ConnStatus::Connecting {
            label: connect::label_of(&target),
        });
        self.conn = Some(tokio::spawn(connect::run(
            target,
            self.cfg.clone(),
            origin,
            self.tx.clone(),
            self.status.clone(),
        )));
    }

    fn ingest(&mut self, i: Ingest) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        match i {
            Ingest::StartRun { app, clock, reply } => {
                let us = (host_now_us() - clock.origin_host_us).max(0) as u64;
                let id = w.start_run(*app, clock, SessionTime(us.saturating_mul(1000)));
                self.current_run = Some(id);
                let _ = reply.send(id);
            }
            Ingest::Log(l) => {
                w.push_log(l);
            }
            Ingest::Frame(f) => {
                w.push_frame(f);
            }
            Ingest::FlutterVersion { run, version } => {
                w.update_run_app(run, |a| a.flutter_version = Some(version.clone()));
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
