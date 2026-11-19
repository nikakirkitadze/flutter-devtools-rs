//! The settings page: theme preference and a pointer to the shortcut list.

use crate::actions::{ShowShortcuts, UseDarkTheme, UseLightTheme, UseSystemTheme};
use fdt_ui::theme::{ThemePreference, TokensExt as _};
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::{
    Action, AnyElement, App, Hsla, InteractiveElement as _, IntoElement, Keystroke,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, div,
};

fn dispatcher(
    action: impl Action + Clone,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) {
    move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
}

fn control_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    action: impl Action + Clone,
    cx: &App,
) -> AnyElement {
    let t = cx.tokens();
    let el = div()
        .id(id)
        .test_support()
        .px(space::S12)
        .h(row::TABLE)
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(font::BODY)
        .font_weight(if selected {
            font::MEDIUM
        } else {
            font::REGULAR
        })
        .text_color(Hsla::from(if selected { t.accent } else { t.text }))
        .hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)))
        .on_click(dispatcher(action))
        .child(label);
    if selected {
        el.bg(t.accent.with_alpha(alpha::ACTIVE_ROW))
            .into_any_element()
    } else {
        el.into_any_element()
    }
}

fn segment_row(buttons: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .rounded(radius::CONTROL)
        .border(layout::BORDER)
        .border_color(Hsla::from(t.border))
        .overflow_hidden()
        .children(buttons)
}

fn row(label: &'static str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .py(space::S8)
        .child(
            div()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text))
                .child(label),
        )
        .child(control)
}

fn section(title: &'static str, rows: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .pb(space::S16)
        .child(
            div()
                .text_size(font::SECTION)
                .font_weight(font::MEDIUM)
                .text_color(Hsla::from(t.text))
                .pb(space::S4)
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .child(title),
        )
        .children(rows)
}

pub fn render(pref: ThemePreference, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    let theme = segment_row(
        vec![
            control_button(
                "theme-system",
                "Match system",
                pref == ThemePreference::System,
                UseSystemTheme,
                cx,
            ),
            control_button(
                "theme-dark",
                "Dark",
                pref == ThemePreference::Dark,
                UseDarkTheme,
                cx,
            ),
            control_button(
                "theme-light",
                "Light",
                pref == ThemePreference::Light,
                UseLightTheme,
                cx,
            ),
        ],
        cx,
    );
    let shortcuts = div()
        .flex()
        .items_center()
        .gap(space::S8)
        .child(segment_row(
            vec![control_button(
                "settings-shortcuts",
                "Show",
                false,
                ShowShortcuts,
                cx,
            )],
            cx,
        ))
        .child(match Keystroke::parse("?") {
            Ok(key) => Kbd::new(key).into_any_element(),
            Err(_) => div().into_any_element(),
        });
    div()
        .id("settings-page")
        .test_support()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px(space::S24)
                .py(space::S16)
                .text_size(font::PAGE)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text))
                .child(SharedString::from("Settings")),
        )
        .child(
            div()
                .id("settings-scroll")
                .test_support()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(space::S24)
                .flex()
                .flex_col()
                .max_w(layout::EMPTY_STATE_MAX_WIDTH)
                .child(section(
                    "Appearance",
                    vec![row("Theme", theme, cx).into_any_element()],
                    cx,
                ))
                .child(section(
                    "Keyboard",
                    vec![row("Keyboard shortcuts", shortcuts, cx).into_any_element()],
                    cx,
                )),
        )
}
