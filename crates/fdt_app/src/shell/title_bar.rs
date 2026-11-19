//! The title bar: history buttons, the app label and the palette button.
//!
//! Controls are token-styled `div`s (gpui-component's `Button` takes no test id).
//! Each stops the press from reaching `TitleBar`'s drag handling, so a click on a
//! control never starts a window move.

use crate::actions::{ConnectToApp, GoBack, GoForward, OpenPalette};
use crate::keymap::SHELL_CONTEXT;
use crate::shell::MainWindow;
use fdt_capture::ConnStatus;
use fdt_ui::assets::icons;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, radius, row, space};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    Action, AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, div,
};
use std::time::Duration;

/// The text of the connection chip for `status`.
pub fn chip_label(status: &ConnStatus) -> String {
    match status {
        ConnStatus::Idle => "No app connected".to_string(),
        ConnStatus::Connecting { label } => format!("Connecting to {label}…"),
        ConnStatus::Connected { app, .. } => {
            let mode = match app.mode {
                fdt_store::run::Mode::Debug => Some("debug"),
                fdt_store::run::Mode::Profile => Some("profile"),
                fdt_store::run::Mode::Unknown => None,
            };
            let parts = [Some(app.name.as_str()), app.device.as_deref(), mode];
            parts.into_iter().flatten().collect::<Vec<_>>().join(" · ")
        }
        ConnStatus::Disconnected { app, .. } => {
            let name = app.as_ref().map_or("App", |a| a.name.as_str());
            format!("{name} · disconnected")
        }
        ConnStatus::Failed { .. } => "Couldn't connect".to_string(),
    }
}

/// `Rec 0:05`, minutes growing past 59 rather than rolling into hours.
pub fn rec_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("Rec {}:{:02}", secs / 60, secs % 60)
}

/// Keeps a press on a control from being read as the start of a window drag.
fn claim_press(window: &mut Window, cx: &mut App) {
    window.prevent_default();
    cx.stop_propagation();
}

fn nav_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    enabled: bool,
    action: impl Action + Clone,
    cx: &App,
) -> AnyElement {
    let t = cx.tokens();
    let tip = action.boxed_clone();
    let mut el = div()
        .id(id)
        .test_support()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .size(row::TABLE)
        .rounded(radius::CONTROL)
        .text_color(Hsla::from(if enabled {
            t.text_muted
        } else {
            t.text_faint
        }))
        .child(Icon::new(icon).size(layout::ICON_MD))
        .on_mouse_down(MouseButton::Left, |_, window, cx| claim_press(window, cx))
        .tooltip(move |window, cx| {
            Tooltip::new(label)
                .action(tip.as_ref(), Some(SHELL_CONTEXT))
                .build(window, cx)
        });
    if enabled {
        let border = t.border;
        el = el
            .cursor_pointer()
            .hover(move |s| s.bg(Hsla::from(border)))
            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx));
    }
    el.into_any_element()
}

impl MainWindow {
    /// Connection status and, while connected, the recording state. Clicking it opens the
    /// start page.
    fn render_connection_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let session = self.session.read(cx);
        let status = session.status();
        let label = chip_label(status);
        let text_color = match status {
            ConnStatus::Disconnected { .. } => t.warning,
            ConnStatus::Idle | ConnStatus::Failed { .. } => t.text_muted,
            _ => t.text,
        };
        let mut chip = div()
            .id("connection-chip")
            .test_support()
            .aria_label(SharedString::from(label.clone()))
            .occlude()
            .flex()
            .items_center()
            .gap(space::S8)
            .h(row::TABLE)
            .px(space::S8)
            .rounded(radius::CONTROL)
            .text_size(font::BODY)
            .text_color(Hsla::from(text_color))
            .cursor_pointer()
            .hover(|s| s.bg(t.border.with_alpha(fdt_ui::tokens::alpha::ACTIVE_ROW)))
            .on_mouse_down(MouseButton::Left, |_, window, cx| claim_press(window, cx))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ConnectToApp), cx));
        if matches!(status, ConnStatus::Disconnected { .. }) {
            chip = chip.child(
                Icon::new(icons::WARNING)
                    .size(layout::ICON_MD)
                    .text_color(Hsla::from(t.warning)),
            );
        }
        chip = chip.child(label);
        if let ConnStatus::Connected { recording, .. } = status {
            let rec = if *recording {
                let text = rec_label(session.recording_elapsed().unwrap_or_default());
                div()
                    .id("rec-chip")
                    .test_support()
                    .aria_label(SharedString::from(text.clone()))
                    .flex()
                    .items_center()
                    .gap(space::S4)
                    .text_color(Hsla::from(t.text))
                    .child(
                        div()
                            .size(layout::STATUS_DOT)
                            .rounded_full()
                            .bg(Hsla::from(t.danger)),
                    )
                    .child(text)
            } else {
                div()
                    .id("rec-chip")
                    .test_support()
                    .aria_label("Paused")
                    .text_color(Hsla::from(t.text_muted))
                    .child("Paused")
            };
            chip = chip.child(rec);
        }
        chip.into_any_element()
    }

    /// The shared time range, with a button that clears it. Shown only while one is set.
    fn render_range_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let range = self.current().range?;
        let t = cx.tokens();
        let label = super::strip::format_range(range);
        let clear = div()
            .id("range-chip-clear")
            .test_support()
            .aria_label("Clear time range")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .size(layout::ICON_MD)
            .rounded(radius::CONTROL)
            .cursor_pointer()
            .hover(|s| s.bg(t.border.with_alpha(fdt_ui::tokens::alpha::ACTIVE_ROW)))
            .on_mouse_down(MouseButton::Left, |_, window, cx| claim_press(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.set_range(None, window, cx)))
            .child(Icon::new(icons::CLEAR).size(layout::ICON_SM));
        Some(
            div()
                .id("range-chip")
                .test_support()
                .aria_label(SharedString::from(label.clone()))
                .occlude()
                .flex()
                .items_center()
                .gap(space::S4)
                .h(row::TABLE)
                .pl(space::S8)
                .pr(space::S4)
                .rounded(radius::CONTROL)
                .bg(t.accent.with_alpha(fdt_ui::tokens::alpha::ACTIVE_ROW))
                .text_size(font::BODY)
                .font_family(font::MONO)
                .text_color(Hsla::from(t.text))
                .child(label)
                .child(clear)
                .into_any_element(),
        )
    }

    /// What the title bar holds: the history buttons and chips on the left and the palette button
    /// on the right. The bar's frame (window drag area, controls) is built by `TitleBar` itself.
    pub(super) fn render_title_content(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        crate::perf::bump(crate::perf::Counter::TitleBar, 1);
        let t = cx.tokens();
        let hint = Kbd::binding_for_action(&OpenPalette, Some(SHELL_CONTEXT), window);
        let back = nav_button(
            "nav-back",
            icons::BACK,
            "Back",
            self.history.can_go_back(),
            GoBack,
            cx,
        );
        let forward = nav_button(
            "nav-forward",
            icons::FORWARD,
            "Forward",
            self.history.can_go_forward(),
            GoForward,
            cx,
        );
        let palette = div()
            .id("palette-button")
            .test_support()
            .occlude()
            .flex()
            .items_center()
            .gap(space::S8)
            .w(layout::PALETTE_BUTTON_WIDTH)
            .h(row::TABLE)
            .px(space::S8)
            .bg(Hsla::from(t.bg))
            .border(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .rounded(radius::CONTROL)
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text_muted))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, window, cx| claim_press(window, cx))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx))
            .child(Icon::new(icons::SEARCH).size(layout::ICON_MD))
            .child(div().flex_1().child("Search commands"))
            .children(hint);
        div()
            .flex()
            .flex_1()
            .h_full()
            .items_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(back)
                    .child(forward)
                    .child(self.render_connection_chip(cx))
                    .children(self.render_range_chip(cx)),
            )
            .child(div().flex_1())
            .child(div().pr(space::S12).child(palette))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use crate::nav::{Page, Tool};
    use crate::shell::MainWindow;
    use crate::test_support::{exists, open_shell, press};
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    fn page(shell: &Entity<MainWindow>, cx: &mut VisualTestContext) -> Page {
        shell.read_with(cx, |s, _| s.current().page)
    }

    #[gpui_kit::test]
    fn back_button_is_inert_until_there_is_history(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        click(cx, "nav-back");
        assert_eq!(page(&shell, cx), Page::Start);
        press(cx, "cmd-2", "ctrl-2");
        click(cx, "nav-back");
        assert_eq!(page(&shell, cx), Page::Start);
        click(cx, "nav-forward");
        assert_eq!(page(&shell, cx), Page::Tool(Tool::Performance));
    }

    #[gpui_kit::test]
    fn range_chip_shows_and_clears(cx: &mut TestAppContext) {
        use crate::nav::{SessionTime, TimeRange};
        let (shell, cx) = open_shell(cx);
        assert!(!exists(cx, "range-chip"));
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.range = Some(TimeRange::new(
            SessionTime(51_000_000_000),
            SessionTime(67_000_000_000),
        ));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        assert!(exists(cx, "range-chip"));
        let label = cx.update(|window, _| window.find("range-chip").label().map(str::to_string));
        assert_eq!(label.as_deref(), Some("0:51\u{2013}1:07"));
        let before = shell.read_with(cx, |s, _| s.history().back_len());
        click(cx, "range-chip-clear");
        assert!(!exists(cx, "range-chip"));
        let (range, after) =
            shell.read_with(cx, |s, _| (s.current().range, s.history().back_len()));
        assert!(range.is_none());
        assert_eq!(after, before + 1);
    }

    #[gpui_kit::test]
    fn palette_button_opens_the_palette(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(exists(cx, "palette-button"));
        click(cx, "palette-button");
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
