//! Pieces shared by every tool page.

pub mod logs;
pub mod network;
pub mod performance;
pub mod timeline;

use crate::nav::Location;

/// Emitted by any tool view to open somewhere else (another tool, a record, a time range). The
/// shell navigates there and keeps a Back step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenLocation(pub Location);

/// Like [`OpenLocation`], but the shell rewrites the current history entry instead of adding one.
/// Keyboard movement through rows and frames sends this, so it adds no Back step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaceLocation(pub Location);

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
