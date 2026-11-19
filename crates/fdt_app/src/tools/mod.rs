//! Pieces shared by every tool page.

pub mod logs;

use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, space};
use gpui_kit::{
    AnyElement, App, Hsla, IntoElement, ParentElement as _, RenderOnce, SharedString, Styled as _,
    Window, div,
};

/// The top of a tool page: its title, then the session strip.
#[derive(IntoElement)]
pub struct ToolHeader {
    title: SharedString,
    strip: AnyElement,
}

impl ToolHeader {
    pub fn new(title: impl Into<SharedString>, strip: impl IntoElement) -> Self {
        Self {
            title: title.into(),
            strip: strip.into_any_element(),
        }
    }
}

impl RenderOnce for ToolHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.tokens();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(space::S24)
                    .py(space::S16)
                    .text_size(font::PAGE)
                    .font_weight(font::SEMIBOLD)
                    .text_color(Hsla::from(t.text))
                    .child(self.title),
            )
            .child(self.strip)
    }
}
