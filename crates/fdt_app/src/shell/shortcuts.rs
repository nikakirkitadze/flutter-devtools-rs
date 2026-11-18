//! The keyboard shortcut cheat sheet, built from the registry and the live keymap.

use crate::keymap::shortcut_hint;
use crate::palette::registry::{CommandSpec, Group, commands};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, radius, space};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::{
    App, FocusHandle, Hsla, InteractiveElement as _, ParentElement as _, Styled as _,
    TestSupportExt as _, Window, div, relative,
};

const GROUPS: [Group; 4] = [Group::Navigation, Group::View, Group::Window, Group::App];

fn group_name(group: Group) -> &'static str {
    match group {
        Group::Navigation => "Navigation",
        Group::View => "View",
        Group::Window => "Window",
        Group::App => "App",
    }
}

fn hint(spec: &CommandSpec, shell_focus: &FocusHandle, window: &Window) -> Option<Kbd> {
    shortcut_hint((spec.action)().as_ref(), shell_focus, window)
}

/// Commands that have a binding, grouped in `Group` order; empty groups are dropped.
pub fn bound_commands(
    shell_focus: &FocusHandle,
    window: &Window,
) -> Vec<(Group, Vec<CommandSpec>)> {
    let all = commands();
    GROUPS
        .into_iter()
        .filter_map(|group| {
            let rows: Vec<CommandSpec> = all
                .iter()
                .filter(|s| s.group == group && hint(s, shell_focus, window).is_some())
                .cloned()
                .collect();
            (!rows.is_empty()).then_some((group, rows))
        })
        .collect()
}

/// Opens the cheat sheet dialog.
pub fn open_shortcuts(shell_focus: FocusHandle, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, window, cx| {
        let t = cx.tokens();
        let mut body = div()
            .id("shortcuts-dialog")
            .flex()
            .flex_col()
            .gap(space::S12);
        for (group, specs) in bound_commands(&shell_focus, window) {
            let mut section = div().flex().flex_col().gap(space::S4).child(
                div()
                    .text_size(font::LABEL)
                    .font_weight(font::MEDIUM)
                    .text_color(Hsla::from(t.text_muted))
                    .child(group_name(group)),
            );
            for spec in specs {
                section = section.child(
                    div()
                        .id(spec.id_for_row())
                        .test_support()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(font::BODY)
                        .text_color(Hsla::from(t.text))
                        .child(spec.label)
                        .children(hint(&spec, &shell_focus, window)),
                );
            }
            body = body.child(section);
        }
        dialog
            .rounded(radius::DIALOG)
            .line_height(relative(font::LINE_HEIGHT_UI))
            .title("Keyboard shortcuts")
            .child(body)
    });
}

trait RowId {
    fn id_for_row(&self) -> gpui_kit::SharedString;
}

impl RowId for CommandSpec {
    fn id_for_row(&self) -> gpui_kit::SharedString {
        format!("shortcut-row-{}", self.id).into()
    }
}

#[cfg(test)]
mod tests {
    use crate::actions::ShowShortcuts;
    use crate::test_support::{exists, open_shell, press};
    use gpui_kit::TestAppContext;
    use gpui_kit::component::WindowExt as _;

    fn dialog_open(cx: &mut gpui_kit::VisualTestContext) -> bool {
        cx.update(|window, cx| window.has_active_dialog(cx))
    }

    #[gpui_kit::test]
    fn question_mark_opens_the_cheat_sheet(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "?", "?");
        assert!(dialog_open(cx));
        assert!(exists(cx, "shortcut-row-tool.memory"));
        assert!(exists(cx, "shortcut-row-nav.back"));
    }

    #[gpui_kit::test]
    fn unbound_commands_are_not_listed(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        cx.dispatch_action(ShowShortcuts);
        cx.run_until_parked();
        assert!(exists(cx, "shortcut-row-tool.memory"));
        assert!(!exists(cx, "shortcut-row-theme.dark"));
    }

    #[gpui_kit::test]
    fn escape_closes_the_cheat_sheet(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "?", "?");
        assert!(dialog_open(cx));
        press(cx, "escape", "escape");
        assert!(!dialog_open(cx));
    }
}
