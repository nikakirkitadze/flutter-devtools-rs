//! The command palette: a dialog holding a non-filtering `Command` driven by our ranking.

use super::rank::rank;
use super::recent::record_recent;
use super::registry::{CommandSpec, haystack};
use crate::actions::OpenPalette;
use crate::settings::{settings, update_settings};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, space};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{IndexPath, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Entity, FocusHandle, Hsla, InteractiveElement as _, ParentElement as _,
    Styled as _, Window, div,
};
use std::cell::Cell;
use std::rc::Rc;

/// The commands, the current query and the ranked order of the commands.
pub struct PaletteModel {
    specs: Vec<CommandSpec>,
    haystacks: Vec<String>,
    query: String,
    ranked: Vec<usize>,
}

impl PaletteModel {
    pub fn new(specs: Vec<CommandSpec>, recent_ids: &[String]) -> Self {
        let haystacks = specs.iter().map(haystack).collect();
        let mut model = Self {
            specs,
            haystacks,
            query: String::new(),
            ranked: Vec::new(),
        };
        model.set_query("", recent_ids);
        model
    }

    pub fn set_query(&mut self, query: &str, recent_ids: &[String]) {
        let recent: Vec<usize> = recent_ids
            .iter()
            .filter_map(|id| self.specs.iter().position(|s| s.id == id))
            .collect();
        self.query = query.to_string();
        self.ranked = rank(query, &self.haystacks, &recent);
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn ranked(&self) -> &[usize] {
        &self.ranked
    }

    pub fn spec(&self, index: usize) -> &CommandSpec {
        &self.specs[index]
    }
}

/// Opens the palette dialog over the shell and returns its model.
pub fn open_palette(
    shell_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PaletteModel> {
    let recents = settings(cx).recent_commands.clone();
    let model = cx.new(|_| PaletteModel::new(super::registry::commands(), &recents));
    let state = cx.new(|cx| CommandState::new(window, cx));
    let focused = Rc::new(Cell::new(false));

    let builder_model = model.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if !focused.replace(true) {
            state.update(cx, |s, cx| s.focus(window, cx));
        }
        let t = cx.tokens();
        let focus_for_rows = shell_focus.clone();
        let items: Vec<CommandItem> = {
            let m = builder_model.read(cx);
            m.ranked()
                .iter()
                .map(|&i| {
                    let spec = m.spec(i);
                    let label = spec.label;
                    let make = spec.action;
                    let focus = focus_for_rows.clone();
                    CommandItem::new().label(label).child(move |window, _cx| {
                        let action = make();
                        let hint = Kbd::binding_for_action_in(action.as_ref(), &focus, window)
                            .or_else(|| Kbd::binding_for_action(action.as_ref(), None, window));
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .text_size(font::BODY)
                            .text_color(Hsla::from(t.text))
                            .child(label)
                            .children(hint)
                    })
                })
                .collect()
        };

        let model_q = builder_model.clone();
        let model_c = builder_model.clone();
        let focus_c = shell_focus.clone();
        let command = Command::new(&state)
            .filterable(false)
            .placeholder("Search commands…")
            .empty(move |_, _, _| {
                div()
                    .p(space::S16)
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text_muted))
                    .child("No matching commands")
            })
            .max_h(layout::PALETTE_MAX_HEIGHT)
            .items(items)
            .on_query(move |q: &str, _window, cx| {
                let recents = settings(cx).recent_commands.clone();
                model_q.update(cx, |m, cx| {
                    m.set_query(q, &recents);
                    cx.notify();
                });
            })
            .on_confirm(move |ix: IndexPath, window, cx| {
                let picked = {
                    let m = model_c.read(cx);
                    m.ranked().get(ix.row).map(|&i| {
                        let spec = m.spec(i);
                        (spec.id, spec.action)
                    })
                };
                let Some((id, make)) = picked else { return };
                update_settings(cx, |s| record_recent(&mut s.recent_commands, id));
                window.close_dialog(cx);
                focus_c.focus(window, cx);
                window.dispatch_action(make(), cx);
            });

        dialog
            .w(layout::PALETTE_WIDTH)
            .margin_top(layout::PALETTE_TOP_OFFSET)
            .close_button(false)
            .child(
                div()
                    .on_action(|_: &OpenPalette, window, cx| window.close_dialog(cx))
                    .child(command),
            )
    });
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Page, Tool};
    use crate::test_support::{open_shell, press};
    use gpui_kit::TestAppContext;

    fn open(cx: &mut gpui_kit::VisualTestContext) {
        press(cx, "cmd-k", "ctrl-k");
    }

    fn dialog_open(cx: &mut gpui_kit::VisualTestContext) -> bool {
        cx.update(|window, cx| window.has_active_dialog(cx))
    }

    fn model(
        shell: &Entity<crate::shell::MainWindow>,
        cx: &mut gpui_kit::VisualTestContext,
    ) -> Entity<PaletteModel> {
        shell.read_with(cx, |s, _| s.palette().cloned().expect("palette opened"))
    }

    #[gpui_kit::test]
    fn cmd_k_opens_the_palette(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(!dialog_open(cx));
        open(cx);
        assert!(dialog_open(cx));
    }

    #[gpui_kit::test]
    fn cmd_k_again_closes_it(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        let first = model(&shell, cx);
        open(cx);
        assert!(!dialog_open(cx));
        assert_eq!(model(&shell, cx), first, "no second palette was created");
    }

    #[gpui_kit::test]
    fn typing_and_enter_runs_the_top_command(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("memory");
        cx.run_until_parked();
        press(cx, "enter", "enter");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(Tool::Memory)
        );
        assert!(!dialog_open(cx));
        assert!(shell.read_with(cx, |s, _| s.history().can_go_back()));
    }

    #[gpui_kit::test]
    fn escape_closes_without_side_effects(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        press(cx, "escape", "escape");
        assert!(!dialog_open(cx));
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(Tool::Timeline)
        );
        assert!(!shell.read_with(cx, |s, _| s.history().can_go_back()));
    }

    #[gpui_kit::test]
    fn recent_command_is_first_on_next_open(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("go to logs");
        cx.run_until_parked();
        press(cx, "enter", "enter");
        open(cx);
        let m = model(&shell, cx);
        let first = m.read_with(cx, |m, _| m.spec(m.ranked()[0]).id);
        assert_eq!(first, "tool.logs");
    }

    #[gpui_kit::test]
    fn shortcut_characters_are_text_in_the_palette(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("?");
        cx.run_until_parked();
        let m = model(&shell, cx);
        assert_eq!(m.read_with(cx, |m, _| m.query().to_string()), "?");
        assert!(dialog_open(cx));
    }

    #[gpui_kit::test]
    fn ranking_drives_rows(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("go back");
        cx.run_until_parked();
        let m = model(&shell, cx);
        let first = m.read_with(cx, |m, _| m.spec(m.ranked()[0]).id);
        assert_eq!(first, "nav.back");
    }

    #[gpui_kit::test]
    fn long_non_ascii_query_is_safe(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input(&"ß".repeat(300));
        cx.run_until_parked();
        let m = model(&shell, cx);
        assert!(m.read_with(cx, |m, _| m.ranked().is_empty()));
    }
}
