//! The main window: sidebar, current tool page and overlays.

mod about;
mod banner;
mod counts;
mod settings_page;
mod shortcuts;
mod sidebar;
pub(crate) mod start_page;
mod strip;
mod title_bar;
mod tool_page;

use crate::actions::{
    About, ClearSelection, CloseWindow, ConnectToApp, Disconnect, GoBack, GoForward, Minimize,
    NewSession, OpenPalette, OpenSettings, ShowLogs, ShowMemory, ShowNetwork, ShowPerformance,
    ShowShortcuts, ShowState, ShowTimeline, ShowWidgets, StopWaiting, ToggleFrameHud,
    ToggleFullScreen, ToggleRecording, UseDarkTheme, UseLightTheme, UseSystemTheme, Zoom,
};
use crate::capture_host::{CaptureControl, CaptureHost};
use crate::keymap::SHELL_CONTEXT;
use crate::nav::{Location, NavHistory, Page, Tool};
use crate::palette::view::{PaletteModel, open_palette};
use crate::session_model::{SessionEvent, SessionModel};
use crate::settings::{settings, update_settings};
use fdt_capture::ConnStatus;
use fdt_ui::hud::FrameHud;
use fdt_ui::theme::{self, ThemePreference, TokensExt as _};
use fdt_ui::tokens::Appearance;
use fdt_vm::discovery::DiscoveredApp;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{
    Action, App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, NavigationDirection, ParentElement as _,
    Render, Styled as _, TestSupportExt as _, Window, div, relative,
};
use start_page::StartPage;
use std::sync::Arc;
use strip::StripView;
use tokio::sync::watch;

/// Makes the window actions work from inside a dialog.
///
/// A dialog is not part of the shell's element tree, so the shell never sees these actions while
/// one is open. Each one closes the dialog, focuses the shell, then runs again from there.
pub fn dialog_window_actions(el: Div, shell: FocusHandle) -> Div {
    fn forward<A: Action + Clone>(el: Div, shell: &FocusHandle, action: A) -> Div {
        let shell = shell.clone();
        el.on_action(move |_: &A, window, cx| {
            window.close_dialog(cx);
            shell.focus(window, cx);
            window.dispatch_action(action.boxed_clone(), cx);
        })
    }
    let el = forward(el, &shell, CloseWindow);
    let el = forward(el, &shell, Minimize);
    let el = forward(el, &shell, Zoom);
    forward(el, &shell, ToggleFullScreen)
}

/// A count shown beside a tool in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCount {
    pub value: u64,
    pub attention: bool,
}

pub struct MainWindow {
    history: NavHistory,
    counts: [Option<ToolCount>; 7],
    /// The newest counts input, the job computing it, and whether one is running.
    counts_wanted: Option<counts::CountsKey>,
    counts_running: bool,
    counts_task: Option<gpui_kit::Task<()>>,
    strip: StripView,
    hud: Entity<FrameHud>,
    focus: FocusHandle,
    palette: Option<Entity<PaletteModel>>,
    session: Entity<SessionModel>,
    /// The start page, created once the app list is available.
    start: Option<Entity<StartPage>>,
    _host_wait: Option<gpui_kit::Subscription>,
    /// Whether the last status seen was `Connected`, so only a new connection leaves Start.
    was_connected: bool,
    _appearance: gpui_kit::Subscription,
    _session_subs: [gpui_kit::Subscription; 2],
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, session: Entity<SessionModel>) -> Self {
        let hud = cx.new(|cx| FrameHud::new(window, cx));
        let focus = cx.focus_handle();
        cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                fdt_ui::os::sync_reduced_motion(cx);
            }
        })
        .detach();
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            let os = theme::os_appearance(Some(window), cx);
            this.appearance_changed(os, window, cx);
        });
        window.focus(&focus, cx);
        let was_connected = matches!(session.read(cx).status(), ConnStatus::Connected { .. });
        let observed = cx.observe(&session, |this, _, cx| {
            this.refresh_counts(cx);
            cx.notify();
        });
        let connected = cx.subscribe_in(&session, window, |this, _, event, window, cx| {
            if *event == SessionEvent::Status {
                this.status_changed(window, cx);
            }
        });
        let host_wait = (!cx.has_global::<CaptureHost>()).then(|| {
            cx.observe_global_in::<CaptureHost>(window, |this, window, cx| {
                this.apps_from_host(window, cx);
            })
        });
        let mut this = Self {
            history: NavHistory::new(Location::at(Page::Start)),
            counts: [None; 7],
            counts_wanted: None,
            counts_running: false,
            counts_task: None,
            strip: StripView::default(),
            hud,
            focus,
            palette: None,
            session,
            start: None,
            _host_wait: host_wait,
            was_connected,
            _appearance: appearance,
            _session_subs: [observed, connected],
        };
        this.apps_from_host(window, cx);
        this.refresh_counts(cx);
        this
    }

    /// Creates the start page from the host's discovery, once the host exists.
    fn apps_from_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.start.is_some() || !cx.has_global::<CaptureHost>() {
            return;
        }
        let apps = cx.global::<CaptureHost>().apps();
        self.set_apps_source(apps, window, cx);
    }

    /// Creates the start page over `apps` (a no-op if it exists).
    pub(crate) fn set_apps_source(
        &mut self,
        apps: watch::Receiver<Arc<[DiscoveredApp]>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.start.is_some() {
            return;
        }
        let session = self.session.clone();
        let page = cx.new(|cx| StartPage::new(apps, session, window, cx));
        self.start = Some(page);
        self.focus_start_if_current(window, cx);
        self._host_wait = None;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn start_page(&self) -> Option<&Entity<StartPage>> {
        self.start.as_ref()
    }

    pub fn session(&self) -> &Entity<SessionModel> {
        &self.session
    }

    /// Leaves the start page for Logs once an app is connected.
    fn status_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connected = matches!(self.session.read(cx).status(), ConnStatus::Connected { .. });
        let started = connected && !self.was_connected;
        self.was_connected = connected;
        if started && self.current().page == Page::Start {
            self.show(Tool::Logs, window, cx);
        }
    }

    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        if let (ConnStatus::Connected { recording, .. }, Some(control)) =
            (session.status(), session.control())
        {
            control.set_recording(!recording);
        }
    }

    fn with_control(&self, cx: &App, f: impl FnOnce(&dyn CaptureControl)) {
        if let Some(control) = self.session.read(cx).control() {
            f(control);
        }
    }

    pub fn current(&self) -> &Location {
        self.history.current()
    }

    pub fn history(&self) -> &NavHistory {
        &self.history
    }

    pub fn navigate(&mut self, to: Location, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.navigate(to) {
            self.focus_start_if_current(window, cx);
            self.refresh_counts(cx);
            cx.notify();
        }
    }

    /// Puts focus on the start page's app list whenever Start is the current page, so its keys
    /// work after the sidebar, "Connect to an app" or back/forward bring the user here.
    fn focus_start_if_current(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.current().page != Page::Start {
            return;
        }
        if let Some(page) = &self.start {
            let focus = page.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    pub fn set_counts(&mut self, counts: [Option<ToolCount>; 7], cx: &mut Context<Self>) {
        if self.counts != counts {
            self.counts = counts;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> [Option<ToolCount>; 7] {
        self.counts
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// The most recently opened palette's model.
    pub fn palette(&self) -> Option<&Entity<PaletteModel>> {
        self.palette.as_ref()
    }

    fn show_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        }
        shortcuts::open_shortcuts(self.focus_handle(), window, cx);
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        } else {
            self.palette = Some(open_palette(self.focus_handle(), window, cx));
        }
    }

    pub fn hud(&self) -> &Entity<FrameHud> {
        &self.hud
    }

    /// Re-applies the theme after the OS switched, only when the preference is "match system".
    pub fn appearance_changed(
        &mut self,
        os: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pref = settings(cx).theme;
        if pref == ThemePreference::System {
            theme::apply_with_os(pref, os, Some(window), cx);
            cx.notify();
        }
    }

    fn set_theme(&mut self, pref: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        update_settings(cx, |s| s.theme = pref);
        theme::apply(pref, Some(window), cx);
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let to = self.current().with_page(Page::Settings);
        self.navigate(to, window, cx);
    }

    fn show(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        let to = self.current().with_page(Page::Tool(tool));
        self.navigate(to, window, cx);
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.go_back().is_some() {
            self.focus_start_if_current(window, cx);
            self.refresh_counts(cx);
            cx.notify();
        }
    }

    fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.go_forward().is_some() {
            self.focus_start_if_current(window, cx);
            self.refresh_counts(cx);
            cx.notify();
        }
    }
}

impl Focusable for MainWindow {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// `1234` as `1.2k`, `12345` as `12k`, `1234567` as `1.2M`; smaller numbers are plain.
pub fn format_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{}.{}k", n / 1_000, (n % 1_000) / 100),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000),
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let page: gpui_kit::AnyElement = match self.current().page {
            Page::Start => match &self.start {
                Some(page) => page.clone().into_any_element(),
                None => tool_page::render_start(cx).into_any_element(),
            },
            Page::Tool(tool) => {
                let strip = self.render_strip(window, cx);
                tool_page::render(tool, strip, cx).into_any_element()
            }
            Page::Settings => {
                let free = settings(cx).free_app_http_memory;
                settings_page::render(settings(cx).theme, free, cx.weak_entity(), cx)
                    .into_any_element()
            }
        };
        let nav = |dir| MouseButton::Navigate(dir);
        let root = div()
            .id("shell")
            .test_support()
            .key_context(SHELL_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .font_family(fdt_ui::tokens::font::UI)
            .line_height(relative(fdt_ui::tokens::font::LINE_HEIGHT_UI))
            .text_color(Hsla::from(t.text))
            .on_action(
                cx.listener(|this, _: &ShowTimeline, w, cx| this.show(Tool::Timeline, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowPerformance, w, cx| this.show(Tool::Performance, w, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowMemory, w, cx| this.show(Tool::Memory, w, cx)))
            .on_action(cx.listener(|this, _: &ShowNetwork, w, cx| this.show(Tool::Network, w, cx)))
            .on_action(cx.listener(|this, _: &ShowLogs, w, cx| this.show(Tool::Logs, w, cx)))
            .on_action(cx.listener(|this, _: &ShowWidgets, w, cx| this.show(Tool::Widgets, w, cx)))
            .on_action(cx.listener(|this, _: &ShowState, w, cx| this.show(Tool::State, w, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, w, cx| this.toggle_palette(w, cx)))
            .on_action(cx.listener(|this, _: &ShowShortcuts, w, cx| this.show_shortcuts(w, cx)))
            .on_action(cx.listener(|this, _: &About, w, cx| {
                if w.has_active_dialog(cx) {
                    w.close_dialog(cx);
                }
                about::open_about(this.focus_handle(), w, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, w, cx| this.open_settings(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleRecording, _, cx| this.toggle_recording(cx)))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| {
                this.with_control(cx, |c| c.new_session());
            }))
            .on_action(cx.listener(|this, _: &Disconnect, _, cx| {
                this.with_control(cx, |c| c.disconnect());
            }))
            .on_action(cx.listener(|this, _: &StopWaiting, _, cx| {
                this.with_control(cx, |c| c.stop_waiting());
            }))
            .on_action(cx.listener(|this, _: &ConnectToApp, w, cx| {
                let to = this.current().with_page(Page::Start);
                this.navigate(to, w, cx);
            }))
            .on_action(cx.listener(|this, _: &UseSystemTheme, w, cx| {
                this.set_theme(ThemePreference::System, w, cx)
            }))
            .on_action(cx.listener(|this, _: &UseDarkTheme, w, cx| {
                this.set_theme(ThemePreference::Dark, w, cx)
            }))
            .on_action(cx.listener(|this, _: &UseLightTheme, w, cx| {
                this.set_theme(ThemePreference::Light, w, cx)
            }))
            .on_action(cx.listener(|this, _: &GoBack, w, cx| this.go_back(w, cx)))
            .on_action(cx.listener(|this, _: &GoForward, w, cx| this.go_forward(w, cx)))
            .on_action(cx.listener(|this, _: &ClearSelection, w, cx| {
                if let Some(to) = this.current().cleared() {
                    this.navigate(to, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleFrameHud, w, cx| {
                this.hud.update(cx, |hud, cx| hud.toggle(w, cx));
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, w, _| w.remove_window()))
            .on_action(cx.listener(|_, _: &Minimize, w, _| w.minimize_window()))
            .on_action(cx.listener(|_, _: &Zoom, w, _| w.zoom_window()))
            .on_action(cx.listener(|_, _: &ToggleFullScreen, w, _| w.toggle_fullscreen()))
            .on_mouse_down(
                nav(NavigationDirection::Back),
                cx.listener(|this, _, w, cx| this.go_back(w, cx)),
            )
            .on_mouse_down(
                nav(NavigationDirection::Forward),
                cx.listener(|this, _, w, cx| this.go_forward(w, cx)),
            )
            .child(self.render_title_bar(window, cx))
            .children(self.render_banner(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .id("page-area")
                            .relative()
                            .flex_1()
                            .h_full()
                            .bg(Hsla::from(t.bg))
                            .child(page)
                            .child(self.hud.clone()),
                    ),
            );
        // Linux draws its own frame and resize edges.
        #[cfg(target_os = "linux")]
        let root = gpui_kit::component::window_border().child(root);
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ToggleFrameHud;
    use crate::nav::{Location, Page, Selection, SessionTime, TimeRange, Tool};
    use crate::test_support::{exists, open_shell, press};
    use fdt_store::time::{EntryId, RunId};
    use gpui_kit::test::{ClickOptions, TestWindowExt as _};
    use gpui_kit::{MouseButton, NavigationDirection, TestAppContext};

    fn range() -> TimeRange {
        TimeRange::new(SessionTime(10), SessionTime(20))
    }

    fn page(shell: &gpui_kit::Entity<MainWindow>, cx: &mut gpui_kit::VisualTestContext) -> Page {
        shell.read_with(cx, |s, _| s.current().page)
    }

    #[gpui_kit::test]
    fn close_window_removes_the_window(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
        press(cx, "cmd-w", "ctrl-w");
        let weak = shell.downgrade();
        drop(shell);
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "the window should be gone");
    }

    #[gpui_kit::test]
    fn shell_starts_on_the_start_page(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        assert!(exists(cx, "start-page"));
        assert!(!exists(cx, "tool-page-timeline"));
        assert!(!shell.read_with(cx, |s, _| s.history().can_go_back()));
    }

    #[gpui_kit::test]
    fn number_shortcuts_switch_tools(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "cmd-1", "ctrl-1");
        assert!(exists(cx, "tool-page-timeline"));
        press(cx, "cmd-2", "ctrl-2");
        assert!(exists(cx, "tool-page-performance"));
        assert!(!exists(cx, "tool-page-timeline"));
        press(cx, "cmd-7", "ctrl-7");
        assert!(exists(cx, "tool-page-state"));
    }

    #[gpui_kit::test]
    fn back_and_forward_shortcuts(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "cmd-2", "ctrl-2");
        press(cx, "cmd-3", "ctrl-3");
        press(cx, "cmd-[", "ctrl-[");
        assert!(exists(cx, "tool-page-performance"));
        press(cx, "cmd-]", "ctrl-]");
        assert!(exists(cx, "tool-page-memory"));
    }

    #[gpui_kit::test]
    fn back_at_start_is_a_no_op(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        for _ in 0..3 {
            press(cx, "cmd-[", "ctrl-[");
        }
        assert!(exists(cx, "start-page"));
    }

    #[gpui_kit::test]
    fn mouse_navigation_buttons(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "cmd-3", "ctrl-3");
        let back =
            ClickOptions::new().with_button(MouseButton::Navigate(NavigationDirection::Back));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click_with_options("shell", back, cx);
        });
        cx.run_until_parked();
        assert!(exists(cx, "start-page"));
        let fwd =
            ClickOptions::new().with_button(MouseButton::Navigate(NavigationDirection::Forward));
        cx.update(|window, cx| {
            window.click_with_options("shell", fwd, cx);
        });
        cx.run_until_parked();
        assert!(exists(cx, "tool-page-memory"));
    }

    #[gpui_kit::test]
    fn clicking_a_sidebar_item_switches_tool(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("sidebar-network", cx);
        });
        cx.run_until_parked();
        assert!(exists(cx, "tool-page-network"));
    }

    #[gpui_kit::test]
    fn shortcuts_still_work_after_clicking_a_sidebar_row(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("sidebar-memory", cx);
        });
        cx.run_until_parked();
        press(cx, "cmd-2", "ctrl-2");
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Performance));
    }

    #[gpui_kit::test]
    fn escape_clears_selection_then_range(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let mut loc = Location::at(Page::Start);
        loc.selection = Some(Selection::Log(EntryId {
            run: RunId(1),
            seq: 1,
        }));
        loc.range = Some(range());
        shell.update_in(cx, |s, window, cx| s.navigate(loc, window, cx));
        press(cx, "escape", "escape");
        let (sel, rng) = shell.read_with(cx, |s, _| (s.current().selection, s.current().range));
        assert!(sel.is_none() && rng.is_some());
        press(cx, "escape", "escape");
        let (sel, rng) = shell.read_with(cx, |s, _| (s.current().selection, s.current().range));
        assert!(sel.is_none() && rng.is_none());
        press(cx, "escape", "escape");
        // A third escape added no entry, so going back lands on the range-only location.
        press(cx, "cmd-[", "ctrl-[");
        assert!(shell.read_with(cx, |s, _| s.current().range.is_some()));
    }

    #[gpui_kit::test]
    fn range_survives_tool_switch(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let mut loc = Location::at(Page::Start);
        loc.range = Some(range());
        shell.update_in(cx, |s, window, cx| s.navigate(loc, window, cx));
        press(cx, "cmd-4", "ctrl-4");
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Network));
        assert_eq!(shell.read_with(cx, |s, _| s.current().range), Some(range()));
    }

    #[gpui_kit::test]
    fn toggle_frame_hud_shows_the_hud(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(!exists(cx, "frame-hud"));
        cx.dispatch_action(ToggleFrameHud);
        cx.run_until_parked();
        assert!(exists(cx, "frame-hud"));
    }

    #[gpui_kit::test]
    fn sidebar_shows_counts(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let mut counts = [None; 7];
        counts[Tool::Logs.index()] = Some(ToolCount {
            value: 214,
            attention: false,
        });
        counts[Tool::Performance.index()] = Some(ToolCount {
            value: 6,
            attention: true,
        });
        shell.update(cx, |s, cx| s.set_counts(counts, cx));
        cx.run_until_parked();
        assert!(exists(cx, "sidebar-count-logs"));
        assert!(exists(cx, "sidebar-count-performance"));
        assert!(!exists(cx, "sidebar-count-memory"));
    }

    #[test]
    fn format_count_cases() {
        for (n, want) in [
            (0, "0"),
            (214, "214"),
            (999, "999"),
            (1000, "1.0k"),
            (1234, "1.2k"),
            (9999, "9.9k"),
            (12345, "12k"),
            (999_999, "999k"),
            (1_234_567, "1.2M"),
        ] {
            assert_eq!(format_count(n), want, "n = {n}");
        }
    }

    /// A stand-in for a tool page with a text field: the shell's key context around an input.
    struct InputHost {
        focus: FocusHandle,
        input: Entity<gpui_kit::component::input::InputState>,
        shortcut_sheets: usize,
        backs: usize,
        escapes: usize,
    }

    impl Render for InputHost {
        fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            use gpui_kit::component::input::Input;
            div()
                .key_context(SHELL_CONTEXT)
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &ShowShortcuts, _, _| this.shortcut_sheets += 1))
                .on_action(cx.listener(|this, _: &GoBack, _, _| this.backs += 1))
                .on_action(cx.listener(|this, _: &ClearSelection, _, _| this.escapes += 1))
                .child(Input::new(&self.input))
        }
    }

    fn input_host(
        cx: &mut TestAppContext,
    ) -> (Entity<InputHost>, &mut gpui_kit::VisualTestContext) {
        use gpui_kit::component::input::InputState;
        crate::test_support::init(cx);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx));
            let focus = cx.focus_handle();
            InputHost {
                focus,
                input,
                shortcut_sheets: 0,
                backs: 0,
                escapes: 0,
            }
        });
        vcx.update(|window, cx| {
            let input = host.read(cx).input.clone();
            input.update(cx, |i, cx| i.focus(window, cx));
        });
        vcx.run_until_parked();
        (host, vcx)
    }

    #[gpui_kit::test]
    fn question_mark_types_into_a_focused_input(cx: &mut TestAppContext) {
        let (host, cx) = input_host(cx);
        cx.simulate_input("?");
        cx.run_until_parked();
        let (value, sheets) =
            host.read_with(cx, |h, cx| (h.input.read(cx).value(), h.shortcut_sheets));
        assert_eq!(value.as_ref(), "?");
        assert_eq!(sheets, 0, "the cheat sheet must not open");
    }

    #[gpui_kit::test]
    fn back_shortcut_does_not_fire_in_an_input(cx: &mut TestAppContext) {
        let (host, cx) = input_host(cx);
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(host.read_with(cx, |h, _| h.backs), 0);
    }

    #[gpui_kit::test]
    fn shell_shortcuts_still_fire_outside_an_input(cx: &mut TestAppContext) {
        let (host, cx) = input_host(cx);
        let focus = host.read_with(cx, |h, _| h.focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(host.read_with(cx, |h, _| h.backs), 1);
    }

    #[gpui_kit::test]
    fn settings_page_scrolls_in_a_short_window(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        cx.simulate_resize(fdt_ui::tokens::layout::WINDOW_MIN);
        press(cx, "cmd-,", "ctrl-,");
        assert!(exists(cx, "settings-page"));
        assert!(exists(cx, "settings-scroll"));
    }
}
