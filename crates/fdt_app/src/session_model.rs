//! The per-window session: the latest snapshot and connection status of one capture.
//!
//! A model starts detached (status `Idle`, an empty snapshot, no control). It attaches a capture
//! as soon as the [`CaptureHost`] exists. Two tasks on GPUI's executor await the capture's watch
//! receivers; nothing here runs while the capture is quiet, apart from a 1 Hz clock that ticks
//! only while a recording is running.

use crate::capture_host::{CaptureControl, CaptureHost, config_from_settings};
use crate::settings::settings;
use fdt_capture::ConnStatus;
use fdt_store::Snapshot;
use fdt_store::time::RunId;
use gpui_kit::{App, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity};
use std::sync::Arc;
use std::time::Duration;

/// What changed on a [`SessionModel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Snapshot,
    Status,
}

/// How often the recording clock ticks.
const CLOCK_TICK: Duration = Duration::from_secs(1); // token-check: allow: 1 Hz recording clock

pub struct SessionModel {
    snapshot: Arc<Snapshot>,
    status: ConnStatus,
    control: Option<Box<dyn CaptureControl>>,
    elapsed: Duration,
    /// The run whose recording time `elapsed` counts.
    clock_run: Option<RunId>,
    clock: Option<Task<()>>,
    watchers: Vec<Task<()>>,
    /// Waits for the host to appear; dropped once a capture is attached.
    host_wait: Option<Subscription>,
}

impl EventEmitter<SessionEvent> for SessionModel {}

/// Every window's model that has a capture, so settings changes can reach all of them.
#[derive(Default)]
struct OpenSessions(Vec<WeakEntity<SessionModel>>);

impl Global for OpenSessions {}

/// Runs `f` on the control of every open window's capture.
pub fn for_each_control(cx: &mut App, f: impl Fn(&dyn CaptureControl)) {
    let sessions: Vec<Entity<SessionModel>> = {
        let open = cx.default_global::<OpenSessions>();
        open.0.retain(|w| w.upgrade().is_some());
        open.0.iter().filter_map(WeakEntity::upgrade).collect()
    };
    for session in sessions {
        if let Some(control) = session.read(cx).control() {
            f(control);
        }
    }
}

impl SessionModel {
    /// A detached model. It attaches a capture when the [`CaptureHost`] is (or becomes) available.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut model = Self::detached();
        if cx.has_global::<CaptureHost>() {
            model.attach_from_host(cx);
        } else {
            model.host_wait = Some(cx.observe_global::<CaptureHost>(|this, cx| {
                this.attach_from_host(cx);
            }));
        }
        model
    }

    /// A model that shows `snapshot` and `status` and never attaches (tests, benchmarks).
    pub fn fixed(snapshot: Arc<Snapshot>, status: ConnStatus) -> Self {
        Self {
            snapshot,
            status,
            ..Self::detached()
        }
    }

    fn detached() -> Self {
        Self {
            snapshot: Arc::new(Snapshot::empty()),
            status: ConnStatus::Idle,
            control: None,
            elapsed: Duration::ZERO,
            clock_run: None,
            clock: None,
            watchers: Vec::new(),
            host_wait: None,
        }
    }

    fn attach_from_host(&mut self, cx: &mut Context<Self>) {
        let config = config_from_settings(settings(cx));
        let Some(control) = cx
            .try_global::<CaptureHost>()
            .map(|host| host.new_capture(config))
        else {
            return;
        };
        self.attach(control, cx);
    }

    /// Starts following `control`, replacing any capture attached before.
    pub fn attach(&mut self, control: Box<dyn CaptureControl>, cx: &mut Context<Self>) {
        self.host_wait = None;
        let mut status_rx = control.status();
        let mut snap_rx = control.snapshots();
        self.control = Some(control);
        self.snapshot = snap_rx.borrow_and_update().clone();
        let status = status_rx.borrow_and_update().clone();
        self.apply_status(status, cx);
        self.watchers = vec![
            cx.spawn(async move |this, cx| {
                while status_rx.changed().await.is_ok() {
                    let status = status_rx.borrow_and_update().clone();
                    if this
                        .update(cx, |model, cx| model.apply_status(status, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }),
            cx.spawn(async move |this, cx| {
                while snap_rx.changed().await.is_ok() {
                    let snapshot = snap_rx.borrow_and_update().clone();
                    if this
                        .update(cx, |model, cx| {
                            model.snapshot = snapshot;
                            cx.notify();
                            cx.emit(SessionEvent::Snapshot);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
        ];
        let me = cx.weak_entity();
        cx.default_global::<OpenSessions>().0.push(me);
        cx.notify();
        cx.emit(SessionEvent::Snapshot);
    }

    fn apply_status(&mut self, status: ConnStatus, cx: &mut Context<Self>) {
        let recording = matches!(
            status,
            ConnStatus::Connected {
                recording: true,
                ..
            }
        );
        if let ConnStatus::Connected { run, .. } = &status
            && self.clock_run != Some(*run)
        {
            self.clock_run = Some(*run);
            self.elapsed = Duration::ZERO;
        }
        self.status = status;
        if recording {
            if self.clock.is_none() {
                self.clock = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(CLOCK_TICK).await;
                        let ticked = this.update(cx, |model, cx| {
                            model.elapsed += CLOCK_TICK;
                            cx.notify();
                        });
                        if ticked.is_err() {
                            break;
                        }
                    }
                }));
            }
        } else {
            self.clock = None;
        }
        cx.notify();
        cx.emit(SessionEvent::Status);
    }

    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    pub fn status(&self) -> &ConnStatus {
        &self.status
    }

    pub fn control(&self) -> Option<&dyn CaptureControl> {
        self.control.as_deref()
    }

    /// The time recorded in the current run, while a recording is running.
    pub fn recording_elapsed(&self) -> Option<Duration> {
        matches!(
            self.status,
            ConnStatus::Connected {
                recording: true,
                ..
            }
        )
        .then_some(self.elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{NewWindow, ToggleRecording};
    use crate::nav::{Page, Tool};
    use crate::palette::registry::commands;
    use crate::shell::MainWindow;
    use crate::test_support::{
        app_info, connected, exists, open_shell, open_shell_fake, open_shell_with, press,
    };
    use fdt_vm::discovery::AppKey;
    use gpui_kit::base::Root;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn label(cx: &mut VisualTestContext, id: &'static str) -> Option<String> {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find(id)
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn page(shell: &Entity<MainWindow>, cx: &mut VisualTestContext) -> Page {
        shell.read_with(cx, |s, _| s.current().page)
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    fn tick(cx: &mut VisualTestContext, secs: u64) {
        for _ in 0..secs {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
        }
    }

    fn disconnected(waiting: bool) -> ConnStatus {
        ConnStatus::Disconnected {
            app: Some(app_info("jank_lab")),
            reason: "closed".into(),
            waiting_for: waiting.then(|| AppKey {
                package: "jank_lab".into(),
                device: "macOS".into(),
                workspace: String::new(),
            }),
        }
    }

    #[gpui_kit::test]
    fn title_shows_each_status(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = open_shell_fake(cx);
        assert_eq!(label(cx, "connection-chip").unwrap(), "No app connected");
        let send = |s: ConnStatus, cx: &mut VisualTestContext| {
            fake.status.send(s).unwrap();
            cx.run_until_parked();
        };
        send(
            ConnStatus::Connecting {
                label: "jank_lab".into(),
            },
            cx,
        );
        assert_eq!(
            label(cx, "connection-chip").unwrap(),
            "Connecting to jank_lab…"
        );
        send(connected("jank_lab", true), cx);
        assert_eq!(
            label(cx, "connection-chip").unwrap(),
            "jank_lab · macOS · debug"
        );
        assert_eq!(label(cx, "rec-chip").unwrap(), "Rec 0:00");
        send(connected("jank_lab", false), cx);
        assert_eq!(label(cx, "rec-chip").unwrap(), "Paused");
        send(disconnected(false), cx);
        assert_eq!(
            label(cx, "connection-chip").unwrap(),
            "jank_lab · disconnected"
        );
        assert!(label(cx, "rec-chip").is_none());
        send(
            ConnStatus::Failed {
                message: "no".into(),
                details: String::new(),
            },
            cx,
        );
        assert_eq!(label(cx, "connection-chip").unwrap(), "Couldn't connect");
    }

    #[gpui_kit::test]
    fn recording_clock_ticks_once_per_second(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        assert_eq!(label(cx, "rec-chip").unwrap(), "Rec 0:00");
        tick(cx, 1);
        assert_eq!(label(cx, "rec-chip").unwrap(), "Rec 0:01");
        tick(cx, 64);
        assert_eq!(label(cx, "rec-chip").unwrap(), "Rec 1:05");
        // Paused: the clock stops and keeps its value for the run.
        fake.status.send(connected("jank_lab", false)).unwrap();
        cx.run_until_parked();
        tick(cx, 5);
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        assert_eq!(label(cx, "rec-chip").unwrap(), "Rec 1:05");
    }

    #[gpui_kit::test]
    fn cmd_r_toggles_recording(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = open_shell_fake(cx);
        press(cx, "cmd-r", "ctrl-r");
        assert!(fake.commands().is_empty(), "nothing to record yet");
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-r", "ctrl-r");
        assert_eq!(fake.commands(), ["set_recording(false)"]);
        fake.status.send(connected("jank_lab", false)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ToggleRecording);
        cx.run_until_parked();
        assert_eq!(
            fake.commands(),
            ["set_recording(false)", "set_recording(true)"]
        );
    }

    #[gpui_kit::test]
    fn cmd_n_starts_a_new_session(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = open_shell_fake(cx);
        press(cx, "cmd-n", "ctrl-n");
        assert_eq!(fake.commands(), ["new_session"]);
    }

    #[gpui_kit::test]
    fn connected_status_moves_start_page_to_logs(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        assert_eq!(page(&shell, cx), Page::Start);
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Logs));
        // A later status change does not pull the user off another page.
        press(cx, "cmd-4", "ctrl-4");
        fake.status.send(connected("jank_lab", false)).unwrap();
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Network));
    }

    #[gpui_kit::test]
    fn banner_offers_stop_waiting_and_sends_it(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        press(cx, "cmd-4", "ctrl-4");
        assert!(!exists(cx, "disconnected-banner"));
        fake.status.send(disconnected(true)).unwrap();
        cx.run_until_parked();
        assert_eq!(
            label(cx, "disconnected-banner").unwrap(),
            "jank_lab stopped. Waiting for it to start again. Your recorded data is kept."
        );
        click(cx, "banner-stop-waiting");
        assert_eq!(fake.commands(), ["stop_waiting"]);
        fake.status.send(disconnected(false)).unwrap();
        cx.run_until_parked();
        assert_eq!(
            label(cx, "disconnected-banner").unwrap(),
            "Disconnected from jank_lab. Your recorded data is kept."
        );
        assert!(!exists(cx, "banner-stop-waiting"));
        click(cx, "banner-connect");
        assert_eq!(page(&shell, cx), Page::Start);
        assert!(!exists(cx, "disconnected-banner"), "tool pages only");
    }

    #[gpui_kit::test]
    fn chip_opens_the_start_page(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Logs));
        click(cx, "connection-chip");
        assert_eq!(page(&shell, cx), Page::Start);
    }

    #[gpui_kit::test]
    fn new_window_opens_a_second_window_on_the_start_page(cx: &mut TestAppContext) {
        let (first, cx) = open_shell(cx);
        press(cx, "cmd-4", "ctrl-4");
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
        press(cx, "cmd-shift-n", "ctrl-shift-n");
        let windows = cx.update(|_, cx| cx.windows());
        assert_eq!(windows.len(), 2);
        let second = cx.update(|_, cx| {
            windows
                .iter()
                .filter_map(|w| w.downcast::<Root>())
                .filter_map(|root| {
                    root.read(cx)
                        .ok()?
                        .view()
                        .clone()
                        .downcast::<MainWindow>()
                        .ok()
                })
                .find(|shell| *shell != first)
                .expect("a second main window")
        });
        assert_eq!(second.read_with(cx, |s, _| s.current().page), Page::Start);
        assert_eq!(page(&first, cx), Page::Tool(Tool::Network));
        cx.dispatch_action(NewWindow);
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 3);
    }

    #[gpui_kit::test]
    fn idle_model_does_no_work(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        let session = shell.read_with(cx, |s, _| s.session().clone());
        let notifies = Rc::new(Cell::new(0usize));
        let seen = notifies.clone();
        let _a = cx.update(|_, cx| {
            let seen = seen.clone();
            cx.observe(&session, move |_, _| seen.set(seen.get() + 1))
        });
        let seen2 = notifies.clone();
        let _b = cx.update(|_, cx| cx.observe(&shell, move |_, _| seen2.set(seen2.get() + 1)));
        tick(cx, 5);
        assert_eq!(notifies.get(), 0, "an idle model must not re-render");
        // The clock runs only while recording.
        fake.status.send(connected("jank_lab", true)).unwrap();
        cx.run_until_parked();
        let before = notifies.get();
        tick(cx, 3);
        assert!(notifies.get() >= before + 3);
        fake.status.send(connected("jank_lab", false)).unwrap();
        cx.run_until_parked();
        let paused = notifies.get();
        tick(cx, 5);
        assert_eq!(notifies.get(), paused);
    }

    #[gpui_kit::test]
    fn free_http_memory_setting_reaches_the_capture(cx: &mut TestAppContext) {
        let (_a, cx, first) = open_shell_fake(cx);
        // A second window with its own capture.
        let (second_fake, second) = crate::test_support::fake_capture();
        let second_session = cx.update(|_, cx| {
            let session = cx.new(SessionModel::new);
            session.update(cx, |s, cx| s.attach(Box::new(second_fake), cx));
            session
        });
        press(cx, "cmd-,", "ctrl-,");
        assert!(exists(cx, "settings-page"));
        assert!(exists(cx, "settings-free-http-memory"));
        click(cx, "settings-free-http-memory");
        assert!(cx.update(|_, cx| crate::settings::settings(cx).free_app_http_memory));
        assert_eq!(first.commands(), ["set_free_app_http_memory(true)"]);
        assert_eq!(second.commands(), ["set_free_app_http_memory(true)"]);
        click(cx, "settings-free-http-memory");
        assert!(!cx.update(|_, cx| crate::settings::settings(cx).free_app_http_memory));
        assert_eq!(
            first.commands().last().unwrap(),
            "set_free_app_http_memory(false)"
        );
        drop(second_session);
    }

    #[gpui_kit::test]
    fn snapshots_reach_the_model(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        let session = shell.read_with(cx, |s, _| s.session().clone());
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        let _sub = cx.update(|_, cx| {
            cx.subscribe(&session, move |_, e: &SessionEvent, _| {
                log.borrow_mut().push(*e)
            })
        });
        fake.snaps
            .send(Arc::new({
                let mut s = Snapshot::empty();
                s.generation = 7;
                s
            }))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(session.read_with(cx, |s, _| s.snapshot().generation), 7);
        assert_eq!(*events.borrow(), [SessionEvent::Snapshot]);
    }

    #[gpui_kit::test]
    fn fixed_session_never_attaches(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_with(cx, |_| {
            SessionModel::fixed(Arc::new(Snapshot::empty()), connected("jank_lab", true))
        });
        let (has_control, elapsed) = shell.read_with(cx, |s, cx| {
            let m = s.session().read(cx);
            (m.control().is_some(), m.recording_elapsed())
        });
        assert!(!has_control);
        assert_eq!(elapsed, Some(Duration::ZERO));
    }

    #[test]
    fn registry_and_menus_list_the_new_commands() {
        use crate::keymap::bindings;
        use crate::menus::app_menus;
        use gpui_kit::MenuItem;
        let cmds = commands();
        let bound: Vec<&str> = bindings().iter().map(|b| b.action().name()).collect();
        let mut menu_actions = Vec::new();
        for m in app_menus() {
            for item in &m.items {
                if let MenuItem::Action { action, .. } = item {
                    menu_actions.push(action.name());
                }
            }
        }
        for (id, label, shortcut) in [
            ("view.recording", "Start or Stop Recording", true),
            ("session.new", "New Session", true),
            ("window.new", "New Window", true),
            ("connect.open", "Connect to an App…", false),
            ("connect.disconnect", "Disconnect", false),
            ("connect.stop_waiting", "Stop Waiting for the App", false),
        ] {
            let c = cmds
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id}"));
            assert_eq!(c.label, label);
            let name = (c.action)().name();
            assert_eq!(bound.contains(&name), shortcut, "{id}");
            assert!(menu_actions.contains(&name), "{id} is not in a menu");
        }
    }
}
