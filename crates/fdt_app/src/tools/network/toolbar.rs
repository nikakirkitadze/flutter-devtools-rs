//! The Network tabs, toolbar (filter field, method and status chips, host menu) and empty
//! states.

use super::model::{empty_copy, empty_label};
use super::view::{METHOD_CHIPS, NetworkView, STATUS_CHIPS};
use crate::shell::sidebar::tool_icon;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, space};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::{
    AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _,
    TestSupportExt as _, Window, div,
};

pub(super) fn toolbar_build(
    view: &mut NetworkView,
    _: &mut Window,
    cx: &mut Context<NetworkView>,
) -> AnyElement {
    view.render_toolbar(cx)
}

/// Everything the toolbar reads from the table view, as text.
pub(super) fn toolbar_key(view: &NetworkView, cx: &App) -> String {
    format!(
        "{:?}",
        (
            cx.appearance(),
            view.filter.methods,
            view.filter.classes,
            &view.filter.host,
            view.show_skeleton,
        )
    )
}

pub(super) fn toolbar_style() -> StyleRefinement {
    StyleRefinement::default()
        .w_full()
        .h(layout::TOOLBAR_HEIGHT)
        .flex_none()
}

impl NetworkView {
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let view = cx.weak_entity();
        div()
            .id("net-tabs")
            .test_support()
            .flex_none()
            .w_full()
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(
                TabBar::new("net-tabs-bar")
                    .underline()
                    .with_size(Size::Small)
                    .selected_index(self.tab_index())
                    .on_click(move |ix: &usize, _, cx| {
                        let ix = *ix;
                        view.update(cx, |v, cx| v.tab_clicked(ix, cx)).ok();
                    })
                    .children(
                        self.tab_labels()
                            .into_iter()
                            .map(|label| Tab::new().label(label)),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::perf::bump(crate::perf::Counter::NetworkToolbar, 1);
        let t = *cx.tokens();
        let weak = cx.weak_entity();
        let methods = METHOD_CHIPS.map(|(slug, method)| {
            let on = self.filter.methods.contains(method);
            let label = if method == "OTHER" { "Other" } else { method };
            chip(
                format!("net-method-{slug}"),
                label,
                on,
                Hsla::from(t.accent),
                Hsla::from(t.text_faint),
                cx.listener(move |this, _, _, cx| this.method_clicked(method, cx)),
            )
        });
        let statuses = STATUS_CHIPS.map(|(slug, label, class)| {
            let on = self.filter.classes.contains(class);
            chip(
                format!("net-status-{slug}"),
                label,
                on,
                Hsla::from(t.accent),
                Hsla::from(t.text_faint),
                cx.listener(move |this, _, _, cx| this.status_clicked(class, cx)),
            )
        });

        let host = {
            let view = weak.clone();
            let label = self
                .filter
                .host
                .clone()
                .unwrap_or_else(|| "All hosts".to_string());
            Button::new("net-host-button")
                .label(label)
                .ghost()
                .compact()
                .dropdown_menu(move |menu, _window, cx| {
                    let (selected, hosts) = view
                        .read_with(cx, |v, _| (v.filter.host.clone(), v.snap.net.hosts()))
                        .unwrap_or((None, std::sync::Arc::from(Vec::new())));
                    let mut menu = menu.max_h(layout::PALETTE_MAX_HEIGHT).scrollable(true);
                    let all = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new("All hosts")
                            .checked(selected.is_none())
                            .on_click(move |_, _, cx| {
                                all.update(cx, |v, cx| v.set_host(None, cx)).ok();
                            }),
                    );
                    for name in hosts.iter() {
                        let view = view.clone();
                        let on = selected.as_deref() == Some(name.as_str());
                        let value = name.clone();
                        let shown: SharedString = if name.is_empty() {
                            "(no host)".into()
                        } else {
                            name.clone().into()
                        };
                        menu = menu.item(PopupMenuItem::new(shown).checked(on).on_click(
                            move |_, _, cx| {
                                let value = value.clone();
                                view.update(cx, |v, cx| v.set_host(Some(value), cx)).ok();
                            },
                        ));
                    }
                    menu
                })
        };

        let filter = div()
            .id("net-filter")
            .test_support()
            // Escape in the field hands focus to the table, keeping the text.
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                let handle = this.focus_handle(cx);
                window.focus(&handle, cx);
            }))
            .flex_none()
            .w(layout::SEARCH_WIDTH)
            .min_w(layout::SEARCH_MIN)
            .child(Input::new(&self.input).cleanable(true));

        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .w_full()
            .h(layout::TOOLBAR_HEIGHT)
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(filter)
            .children(methods)
            .child(
                div()
                    .w(layout::BORDER)
                    .h(space::S16)
                    .bg(Hsla::from(t.border)),
            )
            .children(statuses)
            .child(div().id("net-host").test_support().child(host))
            .children(self.show_skeleton.then(|| {
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .child(Skeleton::new().h(space::S2))
            }))
            .into_any_element()
    }

    /// The area where the table would be, when no request is shown.
    pub(super) fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        if self.filters_active() {
            let label = "No requests match your filters.";
            return div()
                .id("net-empty")
                .test_support()
                .aria_label(label)
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(space::S12)
                .child(
                    div()
                        .text_size(font::BODY)
                        .text_color(Hsla::from(t.text_muted))
                        .child(label),
                )
                .child(
                    div()
                        .id("net-clear-filters")
                        .test_support()
                        .aria_label("Clear filters")
                        .px(space::S12)
                        .py(space::S4)
                        .rounded(radius::CONTROL)
                        .bg(Hsla::from(t.elevated))
                        .text_size(font::BODY)
                        .text_color(Hsla::from(t.text))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx)))
                        .child("Clear filters"),
                )
                .into_any_element();
        }
        let tab = self.filter.tab;
        let (heading, body) = empty_copy(tab);
        div()
            .id("net-empty")
            .test_support()
            .aria_label(empty_label(tab))
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(EmptyState::new(
                "net-empty-state",
                tool_icon(crate::nav::Tool::Network),
                heading,
                body,
            ))
            .into_any_element()
    }
}

/// A chip that is tinted when on and plain when off.
fn chip(
    id: String,
    label: &'static str,
    on: bool,
    accent: Hsla,
    off: Hsla,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    let base = div()
        .id(SharedString::from(id))
        .test_support()
        .aria_label(label)
        .flex_none()
        .px(space::S8)
        .py(space::S2)
        .rounded(radius::CHIP)
        .text_size(font::LABEL)
        .cursor_pointer();
    let base = if on {
        base.bg(accent.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
            .text_color(accent)
    } else {
        base.text_color(off)
    };
    base.on_click(on_click).child(label)
}
