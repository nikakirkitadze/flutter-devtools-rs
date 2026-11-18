//! A tool's page. Every tool shows its empty state until an app is connected.

use crate::nav::Tool;
use crate::shell::sidebar::tool_icon;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, space};
use gpui_kit::{
    App, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    TestSupportExt as _, div,
};

/// Heading and body of a tool's empty state.
pub fn empty_copy(tool: Tool) -> (&'static str, &'static str) {
    match tool {
        Tool::Timeline => (
            "Nothing recorded yet",
            "Frames, requests, logs and GC events appear here on one time axis once an app is connected.",
        ),
        Tool::Performance => (
            "No frames yet",
            "Frame timings appear here once an app is connected. Run it in profile mode for accurate timings.",
        ),
        Tool::Memory => (
            "No memory data yet",
            "Heap usage and leak checks appear here once an app is connected.",
        ),
        Tool::Network => (
            "No requests yet",
            "Requests appear here as your app makes them, once an app is connected.",
        ),
        Tool::Logs => (
            "No logs yet",
            "Logs, print output and Flutter errors appear here once an app is connected.",
        ),
        Tool::Widgets => (
            "No widget tree yet",
            "The widget tree appears here when an app is connected in debug mode.",
        ),
        Tool::State => (
            "No state events yet",
            "BLoC and Riverpod events appear here when your app uses fdt_probe.",
        ),
    }
}

/// The page for `tool`: a header row and a centred empty state.
pub fn render(tool: Tool, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (heading, body) = empty_copy(tool);
    div()
        .id(SharedString::from(format!("tool-page-{}", tool.slug())))
        .test_support()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .px(space::S24)
                .py(space::S16)
                .text_size(font::PAGE)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text))
                .child(tool.label()),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    SharedString::from(format!("empty-state-{}", tool.slug())),
                    tool_icon(tool),
                    heading,
                    body,
                )),
        )
}
