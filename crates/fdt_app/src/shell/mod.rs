//! The main window: sidebar, current tool page and overlays.

mod sidebar;
mod tool_page;

use crate::actions::{
    ClearSelection, GoBack, GoForward, ShowLogs, ShowMemory, ShowNetwork, ShowPerformance,
    ShowState, ShowTimeline, ShowWidgets, ToggleFrameHud,
};
use crate::keymap::SHELL_CONTEXT;
use crate::nav::{Location, NavHistory, Page, Tool};
use fdt_ui::hud::FrameHud;
use fdt_ui::theme::TokensExt as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, NavigationDirection, ParentElement as _, Render, Styled as _,
    TestSupportExt as _, Window, div,
};

/// A count shown beside a tool in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCount {
    pub value: u64,
    pub attention: bool,
}

pub struct MainWindow {
    history: NavHistory,
    counts: [Option<ToolCount>; 7],
    hud: Entity<FrameHud>,
    focus: FocusHandle,
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hud = cx.new(|cx| FrameHud::new(window, cx));
        let focus = cx.focus_handle();
        cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                fdt_ui::os::sync_reduced_motion(cx);
            }
        })
        .detach();
        window.focus(&focus, cx);
        Self {
            history: NavHistory::new(Location::at(Page::Tool(Tool::Timeline))),
            counts: [None; 7],
            hud,
            focus,
        }
    }

    pub fn current(&self) -> &Location {
        self.history.current()
    }

    pub fn history(&self) -> &NavHistory {
        &self.history
    }

    pub fn navigate(&mut self, to: Location, _window: &mut Window, cx: &mut Context<Self>) {
        if self.history.navigate(to) {
            cx.notify();
        }
    }

    pub fn set_counts(&mut self, counts: [Option<ToolCount>; 7], cx: &mut Context<Self>) {
        self.counts = counts;
        cx.notify();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn hud(&self) -> &Entity<FrameHud> {
        &self.hud
    }

    fn show(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        let to = self.current().with_page(Page::Tool(tool));
        self.navigate(to, window, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if self.history.go_back().is_some() {
            cx.notify();
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if self.history.go_forward().is_some() {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let page: gpui_kit::AnyElement = match self.current().page {
            Page::Tool(tool) => tool_page::render(tool, cx).into_any_element(),
            // Task 10 adds the settings page.
            Page::Settings => div().into_any_element(),
        };
        let nav = |dir| MouseButton::Navigate(dir);
        div()
            .id("shell")
            .test_support()
            .key_context(SHELL_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .font_family(fdt_ui::tokens::font::UI)
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
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &ClearSelection, w, cx| {
                if let Some(to) = this.current().cleared() {
                    this.navigate(to, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleFrameHud, w, cx| {
                this.hud.update(cx, |hud, cx| hud.toggle(w, cx));
            }))
            .on_mouse_down(
                nav(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.go_back(cx)),
            )
            .on_mouse_down(
                nav(NavigationDirection::Forward),
                cx.listener(|this, _, _, cx| this.go_forward(cx)),
            )
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
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ToggleFrameHud;
    use crate::nav::{Location, Page, SelectionKey, SessionTime, TimeRange, Tool};
    use crate::test_support::{exists, open_shell, press};
    use gpui_kit::test::{ClickOptions, TestWindowExt as _};
    use gpui_kit::{MouseButton, NavigationDirection, TestAppContext};

    fn range() -> TimeRange {
        TimeRange::new(SessionTime(10), SessionTime(20))
    }

    fn page(shell: &gpui_kit::Entity<MainWindow>, cx: &mut gpui_kit::VisualTestContext) -> Page {
        shell.read_with(cx, |s, _| s.current().page)
    }

    #[gpui_kit::test]
    fn starts_on_timeline_with_empty_history(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        assert!(exists(cx, "tool-page-timeline"));
        assert!(!shell.read_with(cx, |s, _| s.history().can_go_back()));
    }

    #[gpui_kit::test]
    fn number_shortcuts_switch_tools(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
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
        assert!(exists(cx, "tool-page-timeline"));
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
        assert!(exists(cx, "tool-page-timeline"));
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
    fn escape_clears_selection_then_range(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let mut loc = Location::at(Page::Tool(Tool::Timeline));
        loc.selection = Some(SelectionKey("a".into()));
        loc.range = Some(range());
        shell.update_in(cx, |s, window, cx| s.navigate(loc, window, cx));
        press(cx, "escape", "escape");
        let (sel, rng) = shell.read_with(cx, |s, _| {
            (s.current().selection.clone(), s.current().range)
        });
        assert!(sel.is_none() && rng.is_some());
        press(cx, "escape", "escape");
        let (sel, rng) = shell.read_with(cx, |s, _| {
            (s.current().selection.clone(), s.current().range)
        });
        assert!(sel.is_none() && rng.is_none());
        press(cx, "escape", "escape");
        // A third escape added no entry, so going back lands on the range-only location.
        press(cx, "cmd-[", "ctrl-[");
        assert!(shell.read_with(cx, |s, _| s.current().range.is_some()));
    }

    #[gpui_kit::test]
    fn range_survives_tool_switch(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let mut loc = Location::at(Page::Tool(Tool::Timeline));
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
}
