//! The start page: connect to a running app from a live list, a pasted URL or a recent one.

use crate::actions::{AppListConnect, AppListNext, AppListPrev};
use crate::keymap::APP_LIST_CONTEXT;
use crate::session_model::SessionModel;
use crate::settings::{record_recent_url, settings, update_settings};
use fdt_capture::{ConnStatus, Target};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, motion, radius, row, space};
use fdt_vm::discovery::DiscoveredApp;
use fdt_vm::service::Mode;
use fdt_vm::uri::VmServiceUri;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How long the page says it is looking before it gives up waiting for the first scan.
const SCAN_WAIT: Duration = Duration::from_secs(2); // token-check: allow: first-scan grace period

const EMPTY_COPY: &str = "No running Flutter apps found. Start one with `flutter run`, or paste its VM service URL below.";

/// What the user asked to connect to, so the right row shows the spinner.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    App(String),
    Recent(String),
    Url,
}

pub struct StartPage {
    apps: Arc<[DiscoveredApp]>,
    /// Whether discovery has published at least once.
    scanned: bool,
    /// Whether the first-scan grace period is over.
    scan_waited: bool,
    selected: usize,
    list_focus: FocusHandle,
    url: Entity<InputState>,
    url_value: SharedString,
    url_error: Option<&'static str>,
    session: Entity<SessionModel>,
    pending: Option<Pending>,
    saw_connecting: bool,
    show_spinner: bool,
    spinner_task: Option<Task<()>>,
    details_open: bool,
    _tasks: [Task<()>; 2],
    _subs: [Subscription; 2],
}

impl StartPage {
    pub fn new(
        apps: watch::Receiver<Arc<[DiscoveredApp]>>,
        session: Entity<SessionModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut rx = apps;
        let scanned = rx.has_changed().unwrap_or(false);
        let list = rx.borrow_and_update().clone();
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("http://127.0.0.1:50300/abc=/"));
        let input_sub = cx.subscribe_in(
            &url,
            window,
            |this, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let value = state.read(cx).value();
                    if value != this.url_value {
                        this.url_value = value;
                        if this.url_error.take().is_some() {
                            cx.notify();
                        }
                    }
                }
                InputEvent::PressEnter { .. } => this.submit_url(window, cx),
                _ => {}
            },
        );
        let session_sub = cx.observe(&session, |this, _, cx| this.session_changed(cx));
        let watcher = cx.spawn(async move |this, cx| {
            while rx.changed().await.is_ok() {
                let list = rx.borrow_and_update().clone();
                if this.update(cx, |page, cx| page.set_apps(list, cx)).is_err() {
                    break;
                }
            }
        });
        let grace = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCAN_WAIT).await;
            this.update(cx, |page, cx| {
                page.scan_waited = true;
                cx.notify();
            })
            .ok();
        });
        let mut page = Self {
            apps: list,
            scanned,
            scan_waited: false,
            selected: 0,
            list_focus: cx.focus_handle(),
            url,
            url_value: SharedString::default(),
            url_error: None,
            session,
            pending: None,
            saw_connecting: false,
            show_spinner: false,
            spinner_task: None,
            details_open: false,
            _tasks: [watcher, grace],
            _subs: [input_sub, session_sub],
        };
        page.session_changed(cx);
        page
    }

    #[cfg(test)]
    pub fn focus_url(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.url.update(cx, |input, cx| input.focus(window, cx));
    }

    fn set_apps(&mut self, apps: Arc<[DiscoveredApp]>, cx: &mut Context<Self>) {
        self.apps = apps;
        self.scanned = true;
        self.selected = self.selected.min(self.apps.len().saturating_sub(1));
        cx.notify();
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        let status = self.session.read(cx).status().clone();
        let connecting = matches!(status, ConnStatus::Connecting { .. });
        if connecting {
            self.saw_connecting = true;
            if self.spinner_task.is_none() && !self.show_spinner {
                self.spinner_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(motion::LOADING_DELAY).await;
                    this.update(cx, |page, cx| {
                        page.show_spinner = true;
                        cx.notify();
                    })
                    .ok();
                }));
            }
        } else {
            self.spinner_task = None;
            self.show_spinner = false;
            let settled = matches!(
                status,
                ConnStatus::Connected { .. }
                    | ConnStatus::Failed { .. }
                    | ConnStatus::Disconnected { .. }
            );
            if self.saw_connecting || settled {
                self.pending = None;
                self.saw_connecting = false;
            }
        }
        if !matches!(status, ConnStatus::Failed { .. }) {
            self.details_open = false;
        }
        cx.notify();
    }

    /// Starts a connection; false when the window has no capture yet.
    fn connect(&mut self, target: Target, pending: Pending, cx: &mut Context<Self>) -> bool {
        let session = self.session.read(cx);
        let Some(control) = session.control() else {
            return false;
        };
        control.connect(target);
        self.pending = Some(pending);
        self.saw_connecting = false;
        cx.notify();
        true
    }

    fn connect_app(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        window.focus(&self.list_focus, cx);
        let Some(app) = self.apps.get(ix).cloned() else {
            return;
        };
        let pending = Pending::App(app.vm_uri.clone());
        self.connect(Target::Discovered(app), pending, cx);
    }

    fn submit_url(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let input = self.url.read(cx).value();
        match VmServiceUri::parse(&input) {
            Err(e) => {
                self.url_error = Some(e.message());
                cx.notify();
            }
            Ok(uri) => {
                self.url_error = None;
                let stored = uri.ws_url().to_string();
                if self.connect(Target::Uri(uri), Pending::Url, cx) {
                    remember(&stored, cx);
                } else {
                    self.url_error = Some("Not ready to connect yet. Try again in a moment.");
                }
                cx.notify();
            }
        }
    }

    fn connect_recent(&mut self, stored: String, cx: &mut Context<Self>) {
        let Ok(uri) = VmServiceUri::parse(&stored) else {
            return;
        };
        if self.connect(Target::Uri(uri), Pending::Recent(stored.clone()), cx) {
            remember(&stored, cx);
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.apps.is_empty() {
            return;
        }
        let last = self.apps.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        cx.notify();
    }
}

impl Focusable for StartPage {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.list_focus.clone()
    }
}

fn remember(stored: &str, cx: &mut Context<StartPage>) {
    update_settings(cx, |s| record_recent_url(&mut s.recent_urls, stored));
}

/// The last two components of a workspace path, as `parent/name`.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

fn mode_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Debug => "debug",
        Mode::Profile => "profile",
    }
}

fn row_label(app: &DiscoveredApp) -> String {
    let mut parts = vec![app.name.clone(), app.device.clone()];
    parts.extend(app.mode.map(|m| mode_text(m).to_string()));
    parts.extend(app.flutter_version.as_ref().map(|v| format!("Flutter {v}")));
    parts.extend(
        app.workspace
            .as_deref()
            .map(short_path)
            .filter(|p| !p.is_empty()),
    );
    parts.join(" · ")
}

impl StartPage {
    fn spinner(&self, cx: &gpui_kit::App) -> AnyElement {
        let t = cx.tokens();
        div()
            .id("start-spinner")
            .test_support()
            .flex_none()
            .size(layout::ICON_MD)
            .child(Spinner::new().color(Hsla::from(t.accent)))
            .into_any_element()
    }

    fn app_row(&self, ix: usize, app: &DiscoveredApp, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let selected = ix == self.selected;
        let spinning = self.show_spinner && self.pending == Some(Pending::App(app.vm_uri.clone()));
        let muted = Hsla::from(t.text_muted);
        let mut el = div()
            .id(SharedString::from(format!("start-app-{ix}")))
            .test_support()
            .aria_label(SharedString::from(row_label(app)))
            .flex()
            .items_center()
            .gap(space::S12)
            .h(row::TABLE)
            .px(space::S8)
            .rounded(radius::CONTROL)
            .cursor_pointer()
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text))
            .hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.connect_app(ix, window, cx);
            }))
            .child(div().font_weight(font::MEDIUM).child(app.name.clone()))
            .child(div().text_color(muted).child(app.device.clone()));
        if selected {
            el = el.bg(t.accent.with_alpha(alpha::ACTIVE_ROW));
        }
        if let Some(mode) = app.mode {
            el = el.child(
                div()
                    .px(space::S6)
                    .rounded(radius::CHIP)
                    .border(layout::BORDER)
                    .border_color(Hsla::from(t.border))
                    .text_size(font::LABEL)
                    .text_color(muted)
                    .child(mode_text(mode)),
            );
        }
        if let Some(v) = &app.flutter_version {
            el = el.child(
                div()
                    .text_size(font::DENSE)
                    .text_color(muted)
                    .child(format!("Flutter {v}")),
            );
        }
        el = el.child(div().flex_1());
        if let Some(path) = app.workspace.as_deref().map(short_path)
            && !path.is_empty()
        {
            el = el.child(
                div()
                    .text_size(font::DENSE)
                    .font_family(font::MONO)
                    .text_color(Hsla::from(t.text_faint))
                    .child(path),
            );
        }
        if spinning {
            el = el.child(self.spinner(cx));
        }
        el.into_any_element()
    }

    fn app_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let note = |id: &'static str, text: &'static str| {
            div()
                .id(id)
                .test_support()
                .aria_label(text)
                .px(space::S8)
                .py(space::S8)
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child(text)
                .into_any_element()
        };
        let body: Vec<AnyElement> = if !self.apps.is_empty() {
            self.apps
                .iter()
                .enumerate()
                .map(|(ix, app)| self.app_row(ix, app, cx))
                .collect()
        } else if self.scanned || self.scan_waited {
            vec![note("start-empty", EMPTY_COPY)]
        } else {
            vec![note("start-looking", "Looking for running apps…")]
        };
        div()
            .id("start-app-list")
            .test_support()
            .key_context(APP_LIST_CONTEXT)
            .track_focus(&self.list_focus)
            .on_action(cx.listener(|this, _: &AppListPrev, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &AppListNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &AppListConnect, window, cx| {
                let ix = this.selected;
                this.connect_app(ix, window, cx);
            }))
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(body)
            .into_any_element()
    }

    fn url_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let spinning = self.show_spinner && self.pending == Some(Pending::Url);
        let mut field = div()
            .flex()
            .items_center()
            .gap(space::S8)
            .child(div().flex_1().child(Input::new(&self.url)));
        if spinning {
            field = field.child(self.spinner(cx));
        }
        let mut col = div()
            .id("start-url")
            .test_support()
            .flex()
            .flex_col()
            .gap(space::S4)
            .child(field);
        if let Some(message) = self.url_error {
            col = col.child(
                div()
                    .id("start-url-error")
                    .test_support()
                    .aria_label(message)
                    .text_size(font::DENSE)
                    .text_color(Hsla::from(t.danger))
                    .child(message),
            );
        }
        col.into_any_element()
    }

    fn recent_urls(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let rows: Vec<AnyElement> = settings(cx)
            .recent_urls
            .iter()
            .filter_map(|stored| {
                let shown = VmServiceUri::parse(stored).ok()?.redacted();
                Some((stored.clone(), shown))
            })
            .enumerate()
            .map(|(ix, (stored, shown))| {
                let spinning =
                    self.show_spinner && self.pending == Some(Pending::Recent(stored.clone()));
                let mut el = div()
                    .id(SharedString::from(format!("start-recent-{ix}")))
                    .test_support()
                    .aria_label(SharedString::from(shown.clone()))
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .h(row::TABLE)
                    .px(space::S8)
                    .rounded(radius::CONTROL)
                    .cursor_pointer()
                    .text_size(font::DENSE)
                    .font_family(font::MONO)
                    .text_color(Hsla::from(t.text))
                    .hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.connect_recent(stored.clone(), cx);
                    }))
                    .child(div().flex_1().child(shown));
                if spinning {
                    el = el.child(self.spinner(cx));
                }
                el.into_any_element()
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .child(section_label("Recent URLs", cx))
                .children(rows)
                .into_any_element(),
        )
    }

    fn failure(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ConnStatus::Failed { message, details } = self.session.read(cx).status().clone() else {
            return None;
        };
        let t = cx.tokens();
        let button = |id: &'static str, text: &'static str| {
            div()
                .id(id)
                .test_support()
                .flex()
                .items_center()
                .h(row::TABLE)
                .px(space::S12)
                .rounded(radius::CONTROL)
                .border(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text))
                .cursor_pointer()
                .hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)))
                .child(text)
        };
        let open = self.details_open;
        let toggle = button(
            "start-error-details-toggle",
            if open { "Hide details" } else { "Show details" },
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.details_open = !this.details_open;
            cx.notify();
        }));
        let mut col = div()
            .id("start-error")
            .test_support()
            .aria_label(SharedString::from(message.clone()))
            .flex()
            .flex_col()
            .gap(space::S8)
            .p(space::S12)
            .rounded(radius::PANEL)
            .border(layout::BORDER)
            .border_color(t.danger)
            .child(
                div()
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.danger))
                    .child(message),
            )
            .child(div().flex().gap(space::S8).child(toggle));
        if open {
            let copied = details.clone();
            col =
                col.child(
                    div()
                        .id("start-error-details")
                        .test_support()
                        .aria_label(SharedString::from(details.clone()))
                        .text_size(font::DENSE)
                        .font_family(font::MONO)
                        .text_color(Hsla::from(t.text_muted))
                        .child(details),
                )
                .child(div().flex().child(
                    button("start-error-copy", "Copy").on_click(cx.listener(
                        move |_, _: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        },
                    )),
                ));
        }
        Some(col.into_any_element())
    }
}

fn section_label(text: &'static str, cx: &gpui_kit::App) -> impl IntoElement {
    div()
        .px(space::S8)
        .pt(space::S8)
        .text_size(font::LABEL)
        .font_weight(font::MEDIUM)
        .text_color(Hsla::from(cx.tokens().text_faint))
        .child(text)
}

/// The page heading, shared with the placeholder shown before the capture host exists.
pub fn heading(cx: &gpui_kit::App) -> impl IntoElement {
    div()
        .text_size(font::PAGE)
        .font_weight(font::SEMIBOLD)
        .text_color(Hsla::from(cx.tokens().text))
        .child("Connect to a running app")
}

impl Render for StartPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.app_list(cx);
        let failure = self.failure(cx);
        let url = self.url_field(cx);
        let recent = self.recent_urls(cx);
        div()
            .id("start-page")
            .test_support()
            .size_full()
            .flex()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .w(layout::START_WIDTH)
                    .max_w_full()
                    .flex()
                    .flex_col()
                    .gap(space::S16)
                    .px(space::S16)
                    .py(space::S32)
                    .child(heading(cx))
                    .children(failure)
                    .child(list)
                    .child(section_label("Or paste a VM service URL", cx))
                    .child(url)
                    .children(recent),
            )
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::settings;
    use crate::test_support::{discovered, exists, open_start};
    use fdt_capture::{ConnStatus, Target};
    use fdt_vm::service::Mode as VmMode;
    use fdt_vm::uri::UriError;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use std::time::Duration;

    fn label(cx: &mut VisualTestContext, id: &'static str) -> Option<String> {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find(id)
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    fn two_apps() -> Vec<fdt_vm::discovery::DiscoveredApp> {
        vec![
            discovered("jank_lab", "macOS", Some(VmMode::Debug)),
            discovered("shop", "iPhone 15", Some(VmMode::Profile)),
        ]
    }

    fn type_url(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
        text: &str,
    ) {
        shell.update_in(cx, |s, window, cx| {
            s.start_page()
                .unwrap()
                .update(cx, |p, cx| p.focus_url(window, cx));
        });
        cx.simulate_input(text);
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn lists_discovered_apps(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, _tx) = open_start(cx, Some(two_apps()));
        assert_eq!(
            label(cx, "start-app-0").unwrap(),
            "jank_lab · macOS · debug · Flutter 3.47.9 · projects/jank_lab"
        );
        assert_eq!(
            label(cx, "start-app-1").unwrap(),
            "shop · iPhone 15 · profile · Flutter 3.47.9 · projects/shop"
        );
        assert!(!exists(cx, "start-app-2"));
        assert!(!exists(cx, "start-empty"));
    }

    #[gpui_kit::test]
    fn arrow_down_and_enter_connects_the_second_app(cx: &mut TestAppContext) {
        let (_shell, cx, fake, _tx) = open_start(cx, Some(two_apps()));
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let targets = fake.targets.borrow();
        assert_eq!(targets.len(), 1);
        match &targets[0] {
            Target::Discovered(app) => assert_eq!(app.name, "shop"),
            _ => panic!("expected a discovered app"),
        }
    }

    #[gpui_kit::test]
    fn j_and_k_move_and_a_click_connects(cx: &mut TestAppContext) {
        let (_shell, cx, fake, _tx) = open_start(cx, Some(two_apps()));
        cx.simulate_keystrokes("j k j");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(fake.targets.borrow().len(), 1);
        click(cx, "start-app-0");
        let targets = fake.targets.borrow();
        assert!(matches!(&targets[1], Target::Discovered(a) if a.name == "jank_lab"));
    }

    #[gpui_kit::test]
    fn invalid_url_shows_message(cx: &mut TestAppContext) {
        let (shell, cx, fake, _tx) = open_start(cx, Some(two_apps()));
        type_url(&shell, cx, "nonsense");
        assert_eq!(
            label(cx, "start-url-error").unwrap(),
            UriError::NotVmService.message()
        );
        assert!(fake.targets.borrow().is_empty());
        assert!(cx.update(|_, cx| settings(cx).recent_urls.is_empty()));
    }

    #[gpui_kit::test]
    fn valid_url_connects_and_is_remembered(cx: &mut TestAppContext) {
        let (shell, cx, fake, _tx) = open_start(cx, Some(Vec::new()));
        type_url(&shell, cx, " http://127.0.0.1:50300/abc=/ ");
        assert!(!exists(cx, "start-url-error"));
        assert!(
            matches!(&fake.targets.borrow()[0], Target::Uri(u) if u.host_port() == "127.0.0.1:50300")
        );
        assert_eq!(
            cx.update(|_, cx| settings(cx).recent_urls.clone()),
            ["ws://127.0.0.1:50300/abc=/ws"]
        );
        assert_eq!(
            label(cx, "start-recent-0").unwrap(),
            "ws://127.0.0.1:50300/…/ws"
        );
        click(cx, "start-recent-0");
        assert_eq!(fake.targets.borrow().len(), 2);
    }

    #[gpui_kit::test]
    fn spinner_appears_only_after_the_delay(cx: &mut TestAppContext) {
        let (_shell, cx, fake, _tx) = open_start(cx, Some(two_apps()));
        click(cx, "start-app-1");
        fake.status
            .send(ConnStatus::Connecting {
                label: "shop".into(),
            })
            .unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(!exists(cx, "start-spinner"));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(exists(cx, "start-spinner"));
        fake.status.send(ConnStatus::Idle).unwrap();
        cx.run_until_parked();
        assert!(!exists(cx, "start-spinner"));
    }

    #[gpui_kit::test]
    fn failure_shows_message_and_copyable_details(cx: &mut TestAppContext) {
        let (_shell, cx, fake, _tx) = open_start(cx, Some(two_apps()));
        fake.status
            .send(ConnStatus::Failed {
                message: "Couldn't reach the app.".into(),
                details: "connection refused (os error 61)".into(),
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(label(cx, "start-error").unwrap(), "Couldn't reach the app.");
        assert!(!exists(cx, "start-error-details"));
        click(cx, "start-error-details-toggle");
        assert_eq!(
            label(cx, "start-error-details").unwrap(),
            "connection refused (os error 61)"
        );
        click(cx, "start-error-copy");
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()));
        assert_eq!(copied.as_deref(), Some("connection refused (os error 61)"));
    }

    #[gpui_kit::test]
    fn empty_state_copy(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, _tx) = open_start(cx, Some(Vec::new()));
        assert_eq!(
            label(cx, "start-empty").unwrap(),
            "No running Flutter apps found. Start one with `flutter run`, or paste its VM service URL below."
        );
        assert!(!exists(cx, "start-looking"));
    }

    #[gpui_kit::test]
    fn list_updates_live(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, tx) = open_start(cx, Some(Vec::new()));
        assert!(!exists(cx, "start-app-0"));
        tx.send(std::sync::Arc::from(two_apps())).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "start-app-1"));
        assert!(!exists(cx, "start-empty"));
    }

    #[gpui_kit::test]
    fn looking_for_apps_until_the_first_scan(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, tx) = open_start(cx, None);
        assert_eq!(
            label(cx, "start-looking").unwrap(),
            "Looking for running apps…"
        );
        assert!(!exists(cx, "start-empty"));
        cx.executor().advance_clock(Duration::from_millis(2100));
        cx.run_until_parked();
        assert!(exists(cx, "start-empty"));
        assert!(!exists(cx, "start-looking"));
        tx.send(std::sync::Arc::from(two_apps())).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "start-app-0"));
    }

    #[gpui_kit::test]
    fn first_scan_publishing_empty_shows_the_empty_state_at_once(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, tx) = open_start(cx, None);
        tx.send(std::sync::Arc::from(Vec::new())).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "start-empty"));
    }

    #[gpui_kit::test]
    fn list_keys_do_not_fire_while_typing_in_the_url_field(cx: &mut TestAppContext) {
        let (shell, cx, _fake, _tx) = open_start(cx, Some(two_apps()));
        shell.update_in(cx, |s, window, cx| {
            s.start_page()
                .unwrap()
                .update(cx, |p, cx| p.focus_url(window, cx));
        });
        cx.simulate_input("jjk");
        cx.run_until_parked();
        let value = shell.read_with(cx, |s, cx| {
            s.start_page().unwrap().read(cx).url.read(cx).value()
        });
        assert_eq!(value.as_ref(), "jjk");
    }

    #[gpui_kit::test]
    fn the_sidebar_connect_item_opens_the_start_page(cx: &mut TestAppContext) {
        let (_shell, cx, _fake, _tx) = open_start(cx, Some(Vec::new()));
        click(cx, "sidebar-logs");
        assert!(!exists(cx, "start-page"));
        click(cx, "sidebar-connect");
        assert!(exists(cx, "start-page"));
    }
}
