//! A centred icon, heading and body shown where a page has no data yet.

use crate::theme::TokensExt as _;
use crate::tokens::{font, layout, space};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::{
    App, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce,
    SharedString, Styled as _, TestSupportExt as _, Window, div,
};

#[derive(IntoElement)]
pub struct EmptyState {
    id: ElementId,
    icon: IconName,
    heading: SharedString,
    body: SharedString,
}

impl EmptyState {
    pub fn new(
        id: impl Into<ElementId>,
        icon: IconName,
        heading: impl Into<SharedString>,
        body: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon,
            heading: heading.into(),
            body: body.into(),
        }
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.tokens();
        div()
            .id(self.id)
            .test_support()
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .gap(space::S8)
            .max_w(layout::EMPTY_STATE_MAX_WIDTH)
            .child(
                Icon::new(self.icon)
                    .size(layout::ICON_MD)
                    .text_color(Hsla::from(t.text_faint)),
            )
            .child(
                div()
                    .text_size(font::SECTION)
                    .font_weight(font::MEDIUM)
                    .text_color(Hsla::from(t.text))
                    .child(self.heading),
            )
            .child(
                div()
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text_muted))
                    .child(self.body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Context, Render, TestAppContext};

    struct Host;

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            EmptyState::new("es", IconName::Gauge, "H", "B")
        }
    }

    #[gpui_kit::test]
    fn renders_heading_and_body_ids(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::install(cx);
        });
        let (_, cx) = cx.add_window_view(|_, _| Host);
        let found = cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find("es").is_some()
        });
        assert!(found);
    }
}
