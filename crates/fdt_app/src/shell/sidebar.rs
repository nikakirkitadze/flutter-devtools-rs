//! The sidebar: tools with counts, then Export and Settings.
//!
//! Built from plain elements styled from tokens rather than the gpui-component
//! `Sidebar`, so every row carries a test id and a count suffix.

use crate::nav::{Page, Tool};
use crate::shell::{MainWindow, ToolCount, format_count};
use fdt_ui::assets::icons;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    ClickEvent, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div,
};

/// The sidebar icon for `tool`.
pub fn tool_icon(tool: Tool) -> IconName {
    match tool {
        Tool::Timeline => icons::TIMELINE,
        Tool::Performance => icons::PERFORMANCE,
        Tool::Memory => icons::MEMORY,
        Tool::Network => icons::NETWORK,
        Tool::Logs => icons::LOGS,
        Tool::Widgets => icons::WIDGETS,
        Tool::State => icons::STATE,
    }
}

type Click = Box<dyn Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static>;

struct Item {
    id: SharedString,
    icon: IconName,
    label: &'static str,
    active: bool,
    disabled: bool,
    tooltip: Option<&'static str>,
    count: Option<(SharedString, ToolCount)>,
    on_click: Option<Click>,
}

fn item(item: Item, cx: &gpui_kit::App) -> impl IntoElement {
    let t = cx.tokens();
    let mut el = div()
        .id(item.id)
        .test_support()
        .flex()
        .items_center()
        .gap(space::S8)
        .h(row::TABLE)
        .px(space::S8)
        .rounded(radius::CONTROL)
        .text_size(font::BODY)
        .text_color(Hsla::from(if item.disabled {
            t.text_faint
        } else {
            t.text
        }))
        .child(
            Icon::new(item.icon)
                .size(layout::ICON_MD)
                .text_color(Hsla::from(if item.active {
                    t.accent
                } else {
                    t.text_muted
                })),
        )
        .child(div().flex_1().child(item.label));
    if item.active {
        el = el
            .bg(t.accent.with_alpha(alpha::ACTIVE_ROW))
            .font_weight(font::MEDIUM);
    } else if !item.disabled {
        el = el.hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)));
    }
    if let Some((count_id, count)) = item.count {
        el = el.child(
            div()
                .id(count_id)
                .test_support()
                .text_size(font::LABEL)
                .font_family(fdt_ui::tokens::font::MONO)
                .text_color(Hsla::from(if count.attention {
                    t.danger
                } else {
                    t.text_muted
                }))
                .child(format_count(count.value)),
        );
    }
    if let Some(text) = item.tooltip {
        el = el.tooltip(move |window, cx| Tooltip::new(text).build(window, cx));
    }
    if let Some(on_click) = item.on_click
        && !item.disabled
    {
        el = el.cursor_pointer().on_click(on_click);
    }
    el
}

impl MainWindow {
    fn tool_item(&self, tool: Tool, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let active = self.current().page == Page::Tool(tool);
        let count = self.counts[tool.index()].map(|c| {
            (
                SharedString::from(format!("sidebar-count-{}", tool.slug())),
                c,
            )
        });
        item(
            Item {
                id: SharedString::from(format!("sidebar-{}", tool.slug())),
                icon: tool_icon(tool),
                label: tool.label(),
                active,
                disabled: false,
                tooltip: None,
                count,
                on_click: Some(Box::new(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        let to = this.current().with_page(Page::Tool(tool));
                        this.navigate(to, window, cx);
                    },
                ))),
            },
            cx,
        )
        .into_any_element()
    }

    fn connect_item(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        item(
            Item {
                id: "sidebar-connect".into(),
                icon: icons::CONNECT,
                label: "Connect",
                active: self.current().page == Page::Start,
                disabled: false,
                tooltip: None,
                count: None,
                on_click: Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                    let to = this.current().with_page(Page::Start);
                    this.navigate(to, window, cx);
                }))),
            },
            cx,
        )
        .into_any_element()
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        crate::perf::bump(crate::perf::Counter::Sidebar, 1);
        let t = cx.tokens();
        let items: Vec<_> = Tool::ALL[1..]
            .iter()
            .map(|tool| self.tool_item(*tool, cx))
            .collect();
        let tools = div().flex().flex_col().gap(space::S2).children(items);
        div()
            .id("sidebar")
            .test_support()
            .w(layout::SIDEBAR_WIDTH)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(space::S8)
            .p(space::S8)
            .bg(Hsla::from(t.surface))
            .border_r(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(self.connect_item(cx))
            .child(self.tool_item(Tool::Timeline, cx))
            .child(
                div()
                    .px(space::S8)
                    .pt(space::S8)
                    .text_size(font::LABEL)
                    .font_weight(font::MEDIUM)
                    .text_color(Hsla::from(t.text_faint))
                    .child("Tools"),
            )
            .child(tools)
            .child(div().flex_1())
            .child(item(
                Item {
                    id: "sidebar-export".into(),
                    icon: icons::EXPORT,
                    label: "Export…",
                    active: false,
                    disabled: true,
                    tooltip: Some("Connect an app to export a session"),
                    count: None,
                    on_click: None,
                },
                cx,
            ))
            .child(item(
                Item {
                    id: "sidebar-settings".into(),
                    icon: icons::SETTINGS,
                    label: "Settings",
                    active: self.current().page == Page::Settings,
                    disabled: false,
                    tooltip: None,
                    count: None,
                    on_click: Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        let to = this.current().with_page(Page::Settings);
                        this.navigate(to, window, cx);
                    }))),
                },
                cx,
            ))
            .into_any_element()
    }
}
