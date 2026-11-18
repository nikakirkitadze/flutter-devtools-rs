//! The About dialog.

use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, radius, space};
use gpui_kit::component::WindowExt as _;
use gpui_kit::{
    App, Hsla, InteractiveElement as _, ParentElement as _, Styled as _, TestSupportExt as _,
    Window, div, relative,
};

/// Opens the About dialog.
pub fn open_about(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _window, cx| {
        let t = cx.tokens();
        dialog
            .rounded(radius::DIALOG)
            .line_height(relative(font::LINE_HEIGHT_UI))
            .child(
                div()
                    .id("about-dialog")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap(space::S8)
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text))
                    .child(div().text_size(font::PAGE).child("fdt"))
                    .child(
                        div()
                            .text_color(Hsla::from(t.text_muted))
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child("A fast, native Flutter DevTools.")
                    .child("Licensed under Apache-2.0."),
            )
    });
}

#[cfg(test)]
mod tests {
    use crate::actions::About;
    use crate::test_support::{exists, open_shell, press};
    use gpui_kit::TestAppContext;
    use gpui_kit::component::WindowExt as _;

    #[gpui_kit::test]
    fn about_shows_the_version(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        cx.dispatch_action(About);
        cx.run_until_parked();
        assert!(exists(cx, "about-dialog"));
        press(cx, "escape", "escape");
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
