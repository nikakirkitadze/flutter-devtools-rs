//! Helpers shared by the app's UI tests.

use crate::capture_host::CaptureControl;
use crate::keymap::platform;
use crate::session_model::SessionModel;
use crate::shell::MainWindow;
use fdt_capture::{ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::net::{BodyKind, BodyLimits};
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::{EntryId, RunId};
use fdt_vm::discovery::{AppKey, DiscoveredApp};
use fdt_vm::service::Mode as VmMode;
use gpui_kit::base::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::watch;

/// App-level setup shared with production, without fonts.
///
/// The OS reduced-motion probe still runs when a shell window activates
/// (`MainWindow::new` observes activation), so tests see the host's setting unless they set it.
pub fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        crate::app::init(cx);
        crate::settings::init_settings(cx, crate::settings::SettingsStore::in_memory());
    });
}

/// Opens the shell in a test window with focus on it. Its session is fixed: `Idle`, no capture.
///
/// Never installs a capture host, so no test touches the real DTD registry.
pub fn open_shell(cx: &mut TestAppContext) -> (Entity<MainWindow>, &mut VisualTestContext) {
    open_shell_with(cx, |_| {
        SessionModel::fixed(Arc::new(Snapshot::empty()), ConnStatus::Idle)
    })
}

/// Like [`open_shell`], with a session built by `session`.
pub fn open_shell_with(
    cx: &mut TestAppContext,
    session: impl FnOnce(&mut gpui_kit::Context<SessionModel>) -> SessionModel,
) -> (Entity<MainWindow>, &mut VisualTestContext) {
    init(cx);
    let slot: RefCell<Option<Entity<MainWindow>>> = RefCell::new(None);
    let (_, vcx) = cx.add_window_view(|window, cx| {
        let session = cx.new(session);
        let shell = cx.new(|cx| MainWindow::new(window, cx, session));
        *slot.borrow_mut() = Some(shell.clone());
        Root::new(shell, window, cx)
    });
    let shell = slot.into_inner().expect("shell was built");
    let focus = shell.read_with(vcx, |s, _| s.focus_handle());
    vcx.update(|window, cx| window.focus(&focus, cx));
    vcx.run_until_parked();
    (shell, vcx)
}

/// Opens the shell with a [`FakeCapture`] attached to its session.
pub fn open_shell_fake(
    cx: &mut TestAppContext,
) -> (Entity<MainWindow>, &mut VisualTestContext, FakeHandle) {
    let (fake, handle) = fake_capture();
    let (shell, vcx) = open_shell_with(cx, |cx| {
        let mut model = SessionModel::new(cx);
        model.attach(Box::new(fake), cx);
        model
    });
    (shell, vcx, handle)
}

/// Opens the shell with a fake capture and a start page fed by the returned apps channel.
///
/// With `initial` the list is published once before the page looks (a finished first scan);
/// with `None` nothing has been published yet.
pub fn open_start(
    cx: &mut TestAppContext,
    initial: Option<Vec<DiscoveredApp>>,
) -> (
    Entity<MainWindow>,
    &mut VisualTestContext,
    FakeHandle,
    watch::Sender<Arc<[DiscoveredApp]>>,
) {
    let (shell, vcx, handle) = open_shell_fake(cx);
    let (tx, rx) = watch::channel::<Arc<[DiscoveredApp]>>(Arc::from(Vec::new()));
    if let Some(apps) = initial {
        tx.send(Arc::from(apps)).unwrap();
    }
    shell.update_in(vcx, |s, window, cx| s.set_apps_source(rx, window, cx));
    vcx.run_until_parked();
    (shell, vcx, handle, tx)
}

pub fn discovered(name: &str, device: &str, mode: Option<VmMode>) -> DiscoveredApp {
    DiscoveredApp {
        key: AppKey {
            package: name.to_string(),
            device: device.to_string(),
            workspace: format!("/work/{name}"),
        },
        vm_uri: format!("ws://127.0.0.1:1/{name}/ws"),
        name: name.to_string(),
        device: device.to_string(),
        mode,
        flutter_version: Some("3.47.9".to_string()),
        workspace: Some(format!("/home/dev/projects/{name}")),
        pid: None,
    }
}

/// A capture stand-in: records every command and replays whatever the test sends.
pub struct FakeCapture {
    dropped: Rc<std::cell::Cell<bool>>,
    log: Rc<RefCell<Vec<String>>>,
    targets: Rc<RefCell<Vec<Target>>>,
    status: watch::Receiver<ConnStatus>,
    snaps: watch::Receiver<Arc<Snapshot>>,
}

/// The test's side of a [`FakeCapture`].
pub struct FakeHandle {
    /// Set when the `FakeCapture` is dropped.
    pub dropped: Rc<std::cell::Cell<bool>>,
    pub log: Rc<RefCell<Vec<String>>>,
    /// Every target passed to `connect`, in order.
    pub targets: Rc<RefCell<Vec<Target>>>,
    pub status: watch::Sender<ConnStatus>,
    pub snaps: watch::Sender<Arc<Snapshot>>,
}

impl FakeHandle {
    pub fn commands(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

pub fn fake_capture() -> (FakeCapture, FakeHandle) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let (status_tx, status) = watch::channel(ConnStatus::Idle);
    let (snaps_tx, snaps) = watch::channel(Arc::new(Snapshot::empty()));
    let dropped = Rc::new(std::cell::Cell::new(false));
    let targets = Rc::new(RefCell::new(Vec::new()));
    (
        FakeCapture {
            dropped: dropped.clone(),
            log: log.clone(),
            targets: targets.clone(),
            status,
            snaps,
        },
        FakeHandle {
            dropped,
            log,
            targets,
            status: status_tx,
            snaps: snaps_tx,
        },
    )
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl FakeCapture {
    fn record(&self, what: impl Into<String>) {
        self.log.borrow_mut().push(what.into());
    }
}

impl CaptureControl for FakeCapture {
    fn connect(&self, target: Target) {
        self.targets.borrow_mut().push(target);
        self.record("connect");
    }
    fn disconnect(&self) {
        self.record("disconnect");
    }
    fn stop_waiting(&self) {
        self.record("stop_waiting");
    }
    fn set_recording(&self, on: bool) {
        self.record(format!("set_recording({on})"));
    }
    fn new_session(&self) {
        self.record("new_session");
    }
    fn load_full_body(&self, id: EntryId, kind: BodyKind) {
        self.record(format!("load_full_body({id:?}, {kind:?})"));
    }
    fn set_free_app_http_memory(&self, on: bool) {
        self.record(format!("set_free_app_http_memory({on})"));
    }
    fn set_body_limits(&self, _limits: BodyLimits) {
        self.record("set_body_limits");
    }
    fn fetch_cpu_samples(&self) {
        self.record("fetch_cpu");
    }
    fn set_trace_widget_builds(&self, on: bool) {
        self.record(format!("trace_builds {on}"));
    }
    fn set_trace_default(&self, on: bool) {
        self.record(format!("trace_default {on}"));
    }
    fn status(&self) -> watch::Receiver<ConnStatus> {
        self.status.clone()
    }
    fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snaps.clone()
    }
}

pub fn app_info(name: &str) -> AppInfo {
    AppInfo {
        name: name.to_string(),
        device: Some("macOS".to_string()),
        mode: Mode::Debug,
        flutter_version: None,
        workspace: None,
        vm_uri: "ws://127.0.0.1:1/ws".to_string(),
        pid: None,
    }
}

pub fn connected(name: &str, recording: bool) -> ConnStatus {
    ConnStatus::Connected {
        run: RunId(1),
        app: app_info(name),
        recording,
    }
}

/// Whether a test-supported element with `id` is in the next rendered frame.
pub fn exists(cx: &mut VisualTestContext, id: &'static str) -> bool {
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
}

/// Presses `mac` on macOS and `other` elsewhere, then lets the app settle.
pub fn press(cx: &mut VisualTestContext, mac: &'static str, other: &'static str) {
    cx.simulate_keystrokes(platform(mac, other));
    cx.run_until_parked();
}

/// A snapshot built by `build` on a fresh writer with one run (id 1, started at 0).
pub fn snapshot(build: impl FnOnce(&mut fdt_store::SessionWriter, RunId)) -> Arc<Snapshot> {
    let (mut w, run) = writer();
    build(&mut w, run);
    w.publish()
}

/// A fresh writer with one run (id 1, started at 0), for tests that publish several snapshots.
pub fn writer() -> (fdt_store::SessionWriter, RunId) {
    writer_with_chunk(fdt_store::logs::LOG_CHUNK)
}

/// Like [`writer`], with stores that seal a chunk every `chunk` records.
pub fn writer_with_chunk(chunk: usize) -> (fdt_store::SessionWriter, RunId) {
    writer_in(chunk, Mode::Debug)
}

/// Like [`writer`], with the run launched in `mode`.
pub fn writer_in_mode(mode: Mode) -> (fdt_store::SessionWriter, RunId) {
    writer_in(fdt_store::logs::LOG_CHUNK, mode)
}

fn writer_in(chunk: usize, mode: Mode) -> (fdt_store::SessionWriter, RunId) {
    use fdt_store::clock::{Calibration, RunClock};
    use fdt_store::time::SessionTime;
    let mut w = fdt_store::SessionWriter::with_log_chunk(0, chunk);
    let clock = RunClock::new(
        0,
        Calibration {
            skew_us: 0,
            timeline_to_wall_us: 0,
        },
    );
    let mut app = app_info("demo");
    app.mode = mode;
    let run = w.start_run(app, clock, SessionTime(0));
    (w, run)
}

/// A frame of `run` that starts `start_ms` into the session and spends `build_ms` on the UI
/// thread and `raster_ms` on the raster thread. Its elapsed time is the slower of the two.
pub fn frame_at(
    run: RunId,
    number: u64,
    start_ms: u64,
    build_ms: u64,
    raster_ms: u64,
) -> fdt_store::frames::FrameInput {
    fdt_store::frames::FrameInput {
        run,
        number,
        start: fdt_store::time::SessionTime(start_ms * 1_000_000),
        elapsed_us: (build_ms.max(raster_ms) * 1_000) as u32,
        build_us: (build_ms * 1_000) as u32,
        raster_us: (raster_ms * 1_000) as u32,
        vsync_overhead_us: 0,
    }
}

/// A log record at `ms` milliseconds.
pub fn log_at(run: RunId, ms: u64, level: fdt_store::logs::LogLevel) -> fdt_store::logs::LogInput {
    fdt_store::logs::LogInput {
        run,
        time: fdt_store::time::SessionTime(ms * 1_000_000),
        clamped: false,
        level,
        source: fdt_store::logs::LogSource::Log,
        logger: String::new(),
        text: format!("line at {ms}"),
        error: None,
    }
}

/// Opens the shell over a fixed session showing `snapshot`.
pub fn open_shell_snapshot(
    cx: &mut TestAppContext,
    snapshot: Arc<Snapshot>,
) -> (Entity<MainWindow>, &mut VisualTestContext) {
    open_shell_with(cx, move |_| SessionModel::fixed(snapshot, ConnStatus::Idle))
}

/// Upserts one HTTP request, `start_ms` and `end_ms` in milliseconds on the session axis. The
/// request is keyed by its start time, so each call with a new start adds a request.
pub fn request(
    w: &mut fdt_store::SessionWriter,
    run: RunId,
    method: &str,
    url: &str,
    status: Option<u16>,
    start_ms: u64,
    end_ms: Option<u64>,
) {
    use fdt_store::time::SessionTime;
    const MS: u64 = 1_000_000;
    w.upsert_request(fdt_store::net::HttpUpdate {
        run,
        isolate: "i".into(),
        vm_id: start_ms.to_string(),
        method: method.into(),
        url: url.into(),
        start: SessionTime(start_ms * MS),
        start_clamped: false,
        request_sent: None,
        response_start: None,
        end: end_ms.map(|m| SessionTime(m * MS)),
        status,
        reason: None,
        request_headers: vec![],
        response_headers: vec![],
        request_content_length: None,
        response_content_length: None,
        events: vec![],
        error: None,
        websocket: status == Some(101),
        remote: None,
    });
}

/// A valid 1x1 PNG.
pub const PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
