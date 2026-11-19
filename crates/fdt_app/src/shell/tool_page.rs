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

/// Heading and body of a tool's empty state. `connected` is true while an app is connected: the
/// tools that are not built yet then say so instead of promising data.
pub fn empty_copy(tool: Tool, connected: bool) -> (String, String) {
    let (heading, body) = static_copy(tool, connected);
    (heading.to_string(), body)
}

fn static_copy(tool: Tool, connected: bool) -> (&'static str, String) {
    let not_built = matches!(
        tool,
        Tool::Timeline | Tool::Performance | Tool::Memory | Tool::Widgets | Tool::State
    );
    if connected && not_built {
        return (
            "Not available yet",
            format!(
                "{} isn't built yet. Logs and Network are recording this app.",
                tool.label()
            ),
        );
    }
    let (heading, body) = idle_copy(tool);
    (heading, body.to_string())
}

fn idle_copy(tool: Tool) -> (&'static str, &'static str) {
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

/// The start page before the capture host exists: just its heading.
pub fn render_start(cx: &App) -> impl IntoElement {
    div()
        .id("start-page")
        .test_support()
        .size_full()
        .flex()
        .justify_center()
        .py(space::S32)
        .child(crate::shell::start_page::heading(cx))
}

/// The page for `tool`: a header row and a centred empty state.
pub fn render(tool: Tool, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let (heading, body) = empty_copy(tool, false);
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

#[cfg(test)]
mod tests {
    use super::empty_copy;
    use crate::nav::Tool;

    #[test]
    fn not_built_tools_say_so_when_connected() {
        for tool in [
            Tool::Timeline,
            Tool::Performance,
            Tool::Memory,
            Tool::Widgets,
            Tool::State,
        ] {
            let (heading, body) = empty_copy(tool, true);
            assert_eq!(heading, "Not available yet");
            assert_eq!(
                body,
                format!(
                    "{} isn't built yet. Logs and Network are recording this app.",
                    tool.label()
                )
            );
        }
        assert_eq!(empty_copy(Tool::Logs, false).0, "No logs yet");
        assert_eq!(empty_copy(Tool::Logs, true).0, "No logs yet");
    }
}
