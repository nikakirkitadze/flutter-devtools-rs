//! The title bar: history buttons, the app label and the palette button.
//!
//! Controls are token-styled `div`s (gpui-component's `Button` takes no test id).
//! Each stops the press from reaching `TitleBar`'s drag handling, so a click on a
//! control never starts a window move.

use crate::actions::{GoBack, GoForward, OpenPalette};
use crate::keymap::SHELL_CONTEXT;
use crate::shell::MainWindow;
use fdt_ui::assets::icons;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, radius, row, space};
use gpui_kit::assets::IconName;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, TitleBar};
use gpui_kit::{
    Action, AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window,
    div,
};

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
    pub(super) fn render_title_bar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        TitleBar::new()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(back)
                    .child(forward)
                    .child(
                        div()
                            .text_size(font::BODY)
                            .text_color(Hsla::from(t.text_muted))
                            .child("No app connected"),
                    ),
            )
            .child(div().flex_1())
            .child(div().pr(space::S12).child(palette))
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
    fn palette_button_opens_the_palette(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(exists(cx, "palette-button"));
        click(cx, "palette-button");
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
