//! The banner under the title bar that says the app went away and the data is kept.

use crate::actions::{ConnectToApp, StopWaiting};
use crate::nav::Page;
use crate::shell::MainWindow;
use fdt_capture::ConnStatus;
use fdt_ui::assets::icons;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::component::Icon;
use gpui_kit::{
    Action, AnyElement, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, div,
};

/// The banner's text and its button (id and label) for a disconnected status.
pub fn banner_copy(status: &ConnStatus) -> Option<(String, &'static str, &'static str)> {
    let ConnStatus::Disconnected {
        app, waiting_for, ..
    } = status
    else {
        return None;
    };
    let name = app.as_ref().map_or("The app", |a| a.name.as_str());
    Some(if waiting_for.is_some() {
        (
            format!("{name} stopped. Waiting for it to start again. Your recorded data is kept."),
            "banner-stop-waiting",
            "Stop waiting",
        )
    } else {
        (
            format!("Disconnected from {name}. Your recorded data is kept."),
            "banner-connect",
            "Connect to an app",
        )
    })
}

impl MainWindow {
    /// The disconnected banner, on tool pages only.
    pub(super) fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !matches!(self.current().page, Page::Tool(_)) {
            return None;
        }
        let (text, button_id, button_label) = banner_copy(self.session.read(cx).status())?;
        let t = cx.tokens();
        let action: Box<dyn Action> = if button_id == "banner-stop-waiting" {
            Box::new(StopWaiting)
        } else {
            Box::new(ConnectToApp)
        };
        let button = div()
            .id(button_id)
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
            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            .child(button_label);
        Some(
            div()
                .id("disconnected-banner")
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .flex()
                .items_center()
                .gap(space::S8)
                .h(layout::BANNER_HEIGHT)
                .flex_none()
                .px(space::S16)
                .bg(t.warning.with_alpha(alpha::ACTIVE_ROW))
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text))
                .child(
                    Icon::new(icons::WARNING)
                        .size(layout::ICON_MD)
                        .text_color(Hsla::from(t.warning)),
                )
                .child(div().flex_1().child(text))
                .child(button)
                .into_any_element(),
        )
    }
}
