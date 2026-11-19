//! The Logs toolbar: level chips, source and logger menus, and the follow controls.

use super::view::{ALL_LEVELS, ALL_SOURCES, LogsView, level_color};
use crate::shell::format_count;
use fdt_store::logs::LogLevel;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, space};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, div,
};
use std::sync::Arc;

fn level_slug(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warning => "warning",
        LogLevel::Error => "error",
    }
}

impl LogsView {
    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let weak = cx.weak_entity();
        let chips = ALL_LEVELS.map(|level| {
            let count = self.counts[level as usize];
            let label = format!("{} {}", level.label(), format_count(count));
            let on = self.filter.levels.contains(level);
            let colour = level_color(level, &t);
            div()
                .id(SharedString::from(format!(
                    "logs-level-{}",
                    level_slug(level)
                )))
                .test_support()
                .aria_label(SharedString::from(label.clone()))
                .flex_none()
                .px(space::S8)
                .py(space::S2)
                .rounded(radius::CHIP)
                .text_size(font::LABEL)
                .cursor_pointer()
                .when_on(on, colour, Hsla::from(t.text_faint))
                .on_click(cx.listener(move |this, _, _, cx| this.level_clicked(level, cx)))
                .child(label)
        });

        let sources = {
            let view = weak.clone();
            Button::new("logs-source-button")
                .label("Source")
                .ghost()
                .compact()
                .dropdown_menu(move |mut menu, _window, cx| {
                    let current = view
                        .read_with(cx, |v, _| v.filter.sources)
                        .unwrap_or(fdt_store::log_query::SourceSet::ALL);
                    for source in ALL_SOURCES {
                        let view = view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(source.label())
                                .checked(current.contains(source))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |v, cx| v.toggle_source(source, cx)).ok();
                                }),
                        );
                    }
                    menu
                })
        };

        let loggers = {
            let view = weak.clone();
            Button::new("logs-logger-button")
                .label("Logger")
                .ghost()
                .compact()
                .dropdown_menu(move |menu, _window, cx| {
                    let (selected, names) = view
                        .read_with(cx, |v, _| {
                            (v.filter.loggers.clone(), v.snap.logs.loggers().to_vec())
                        })
                        .unwrap_or((None, Vec::new()));
                    let mut menu = menu.max_h(layout::PALETTE_MAX_HEIGHT).scrollable(true);
                    let all = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new("All loggers")
                            .checked(selected.is_none())
                            .on_click(move |_, _, cx| {
                                all.update(cx, |v, cx| v.all_loggers(cx)).ok();
                            }),
                    );
                    for name in names {
                        let on = selected.as_ref().is_none_or(|set| set.contains(&name));
                        let view = view.clone();
                        let label: SharedString = name.to_string().into();
                        let key: Arc<str> = name;
                        menu = menu.item(PopupMenuItem::new(label).checked(on).on_click(
                            move |_, _, cx| {
                                let key = key.clone();
                                view.update(cx, |v, cx| v.toggle_logger(key, cx)).ok();
                            },
                        ));
                    }
                    menu
                })
        };

        let follow = self.follow_state();
        let follow_toggle = div()
            .id("logs-follow")
            .test_support()
            .aria_label("Follow")
            .flex_none()
            .px(space::S8)
            .py(space::S2)
            .rounded(radius::CHIP)
            .text_size(font::LABEL)
            .cursor_pointer()
            .when_on(
                follow.following,
                Hsla::from(t.accent),
                Hsla::from(t.text_muted),
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_follow(cx)))
            .child("Follow");
        let unseen = (!follow.following && follow.unseen > 0).then(|| {
            let label = format!("{} new lines \u{2193}", follow.unseen);
            div()
                .id("logs-unseen")
                .test_support()
                .aria_label(SharedString::from(label.clone()))
                .flex_none()
                .px(space::S8)
                .py(space::S2)
                .rounded(radius::CHIP)
                .bg(Hsla::from(t.accent))
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.bg))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.follow_latest(cx)))
                .child(label)
        });

        let search = div()
            .id("logs-search")
            .test_support()
            .flex_none()
            .w(layout::SEARCH_WIDTH)
            .min_w(layout::SEARCH_MIN)
            .child(Input::new(&self.search).cleanable(true));
        let regex = div()
            .id("logs-regex")
            .test_support()
            .aria_label("Regular expression")
            .flex_none()
            .px(space::S6)
            .py(space::S2)
            .rounded(radius::CHIP)
            .font_family(font::MONO)
            .text_size(font::LABEL)
            .cursor_pointer()
            .when_on(self.regex, Hsla::from(t.accent), Hsla::from(t.text_muted))
            .tooltip(|window, cx| Tooltip::new("Regular expression").build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_regex(cx)))
            .child(".*");
        let status = if let Some(error) = self.search_error.clone() {
            Some(
                div()
                    .id("logs-search-error")
                    .test_support()
                    .aria_label(error.clone())
                    .flex_none()
                    .max_w(layout::SEARCH_WIDTH)
                    .truncate()
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.danger))
                    .child(error),
            )
        } else if self.searching() {
            Some(
                div()
                    .id("logs-searching")
                    .test_support()
                    .aria_label("Searching\u{2026}")
                    .flex_none()
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child("Searching\u{2026}"),
            )
        } else {
            self.match_count().map(|n| {
                let label = if n == 1 {
                    "1 match".to_string()
                } else {
                    format!("{} matches", format_count(n as u64))
                };
                div()
                    .id("logs-match-count")
                    .test_support()
                    .aria_label(SharedString::from(label.clone()))
                    .flex_none()
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(label)
            })
        };

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
            .children(chips)
            .child(div().id("logs-source").test_support().child(sources))
            .child(div().id("logs-logger").test_support().child(loggers))
            .child(search)
            .child(regex)
            .children(status)
            .child(div().flex_1())
            .children(unseen)
            .child(follow_toggle)
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
}

/// A chip that is tinted when on and plain when off.
trait ChipState: Sized {
    fn when_on(self, on: bool, colour: Hsla, off: Hsla) -> Self;
}

impl<E: gpui_kit::Styled> ChipState for E {
    fn when_on(self, on: bool, colour: Hsla, off: Hsla) -> Self {
        if on {
            self.bg(colour.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
                .text_color(colour)
        } else {
            self.text_color(off)
        }
    }
}
