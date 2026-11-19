//! The command palette dialog: our own input and a virtualised list over [`PaletteModel`].
//!
//! Commands are listed at once; requests are collected on the background executor when the
//! palette opens and merged in, because scanning a long session must not block the UI thread.

use super::items::{
    ItemKind, PaletteItem, PaletteModel, REQUEST_ITEM_LIMIT, command_items, request_items,
};
use super::recent::record_recent;
use crate::actions::OpenPalette;
use crate::keymap::{PALETTE_CONTEXT, shortcut_hint};
use crate::nav::{Page, Tool};
use crate::settings::{settings, update_settings};
use crate::shell::{MainWindow, dialog_window_actions};
use fdt_store::Snapshot;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::base::actions::{SelectDown, SelectUp};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, ScrollStrategy, SharedString,
    Styled as _, Subscription, Task, TestSupportExt as _, UniformListScrollHandle, WeakEntity,
    Window, div, relative, uniform_list,
};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Rows built by the list's item closure, for the "only visible rows" test.
    static ROWS: Cell<usize> = const { Cell::new(0) };
}

/// The dialog's content.
struct PaletteView {
    model: Entity<PaletteModel>,
    input: Entity<InputState>,
    scroll: UniformListScrollHandle,
    selected: usize,
    /// Whether the user has moved the selection since the last query change.
    moved: bool,
    last_query: SharedString,
    shell: WeakEntity<MainWindow>,
    shell_focus: FocusHandle,
    _subs: Vec<Subscription>,
    _load: Task<()>,
}

/// The row to select after the background items merge in: the item the user had moved to, if it
/// is still listed; otherwise the top row. `moved_to` is `None` when the user had not moved.
fn selection_after_merge(moved_to: Option<&str>, model: &PaletteModel) -> usize {
    moved_to
        .and_then(|id| (0..model.row_count()).find(|&row| model.item(row).id == id))
        .unwrap_or(0)
}

impl PaletteView {
    fn new(
        model: Entity<PaletteModel>,
        shell: WeakEntity<MainWindow>,
        shell_focus: FocusHandle,
        snapshot: Arc<Snapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search commands, requests and logs\u{2026}")
        });
        let subs = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Change => {
                            let value = input.read(cx).value();
                            // Enter repeats a Change with the same text.
                            if value != this.last_query {
                                this.last_query = value.clone();
                                this.query_changed(&value, cx);
                            }
                        }
                        InputEvent::PressEnter { .. } => this.run(this.selected, window, cx),
                        _ => {}
                    }
                },
            ),
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];
        // R15: the request scan runs off the UI thread; the commands are usable meanwhile.
        let scan = cx.background_spawn(async move { request_items(&snapshot, REQUEST_ITEM_LIMIT) });
        let target = model.clone();
        let load = cx.spawn(async move |this, cx| {
            let items = scan.await;
            this.update(cx, |this, cx| {
                let recents = settings(cx).recent_commands.clone();
                let before = this.moved.then(|| {
                    let m = target.read(cx);
                    (this.selected < m.row_count()).then(|| m.item(this.selected).id.clone())
                });
                target.update(cx, |m, cx| {
                    m.extend(items, &recents);
                    cx.notify();
                });
                let row = selection_after_merge(before.flatten().as_deref(), target.read(cx));
                this.selected = row;
                this.scroll.scroll_to_item(
                    row,
                    if row == 0 {
                        ScrollStrategy::Top
                    } else {
                        ScrollStrategy::Nearest
                    },
                );
            })
            .ok();
        });
        Self {
            model,
            input,
            scroll: UniformListScrollHandle::new(),
            selected: 0,
            moved: false,
            last_query: SharedString::default(),
            shell,
            shell_focus,
            _subs: subs,
            _load: load,
        }
    }

    fn query_changed(&mut self, query: &str, cx: &mut Context<Self>) {
        let recents = settings(cx).recent_commands.clone();
        self.model.update(cx, |m, cx| {
            m.set_query(query, &recents);
            cx.notify();
        });
        self.selected = 0;
        self.moved = false;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn move_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        let count = self.model.read(cx).row_count();
        if count == 0 {
            return;
        }
        self.selected = if down {
            (self.selected + 1).min(count - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        self.moved = true;
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Closes the dialog, focuses the shell, then does what `row` stands for.
    fn run(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let item = {
            let m = self.model.read(cx);
            if row >= m.row_count() {
                return;
            }
            m.item(row).clone()
        };
        if item.is_command() {
            update_settings(cx, |s| record_recent(&mut s.recent_commands, &item.id));
        }
        window.close_dialog(cx);
        self.shell_focus.focus(window, cx);
        match item.kind {
            ItemKind::Action(make) => window.dispatch_action(make(), cx),
            ItemKind::Navigate(to) => {
                self.shell
                    .update(cx, |shell, cx| shell.navigate(to, window, cx))
                    .ok();
            }
            ItemKind::SearchLogs(query) => {
                self.shell
                    .update(cx, |shell, cx| {
                        let to = shell.current().with_page(Page::Tool(Tool::Logs));
                        shell.navigate(to, window, cx);
                        shell
                            .logs_view()
                            .update(cx, |logs, cx| logs.set_search(&query, false, window, cx));
                    })
                    .ok();
            }
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = cx.tokens();
        let model = self.model.read(cx);
        let end = range.end.min(model.row_count());
        let mut out = Vec::new();
        for ix in range.start..end {
            #[cfg(test)]
            ROWS.with(|c| c.set(c.get() + 1));
            let item: &PaletteItem = model.item(ix);
            let hint = match &item.kind {
                ItemKind::Action(make) => shortcut_hint(make().as_ref(), &self.shell_focus, window),
                _ => None,
            };
            let selected = ix == self.selected;
            let mut el = div()
                .id(SharedString::from(format!("palette-row-{ix}")))
                .test_support()
                .flex()
                .items_center()
                .gap(space::S8)
                .w_full()
                .h(row::TABLE)
                .px(space::S12)
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.run(ix, window, cx)),
                )
                .child(div().min_w_0().truncate().child(item.label.clone()));
            if selected {
                el = el.bg(t.accent.with_alpha(alpha::SELECTION));
            }
            if let Some(detail) = &item.detail {
                el = el.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(Hsla::from(t.text_muted))
                        .child(detail.clone()),
                );
            } else {
                el = el.child(div().flex_1());
            }
            out.push(el.children(hint).into_any_element());
        }
        out
    }
}

impl Render for PaletteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let count = self.model.read(cx).row_count();
        let body = if count == 0 {
            div()
                .p(space::S16)
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child("No matching results")
                .into_any_element()
        } else {
            // An explicit height keeps the number of built rows independent of the window.
            let height = (row::TABLE * count as f32).min(layout::PALETTE_MAX_HEIGHT);
            uniform_list(
                "palette-list",
                count,
                cx.processor(|this, range: Range<usize>, window, cx| {
                    this.render_rows(range, window, cx)
                }),
            )
            .track_scroll(&self.scroll)
            .h(height)
            .into_any_element()
        };
        div()
            .id("palette")
            .key_context(PALETTE_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.move_selection(true, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.move_selection(false, cx)))
            .on_action(|_: &Escape, window, cx| window.close_dialog(cx))
            .on_action(|_: &OpenPalette, window, cx| window.close_dialog(cx))
            .child(
                div()
                    .id("palette-input")
                    .test_support()
                    .p(space::S8)
                    .child(Input::new(&self.input)),
            )
            .child(body)
    }
}

/// Opens the palette dialog over the shell and returns its model.
pub fn open_palette(
    shell: WeakEntity<MainWindow>,
    shell_focus: FocusHandle,
    snapshot: Arc<Snapshot>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PaletteModel> {
    let recents = settings(cx).recent_commands.clone();
    let model = cx.new(|_| PaletteModel::new(command_items(), &recents));
    let view = cx.new(|cx| {
        PaletteView::new(
            model.clone(),
            shell,
            shell_focus.clone(),
            snapshot,
            window,
            cx,
        )
    });
    let focused = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, window, cx| {
        if !focused.replace(true) {
            let input = view.read(cx).input.clone();
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        dialog
            .rounded(radius::DIALOG)
            .line_height(relative(font::LINE_HEIGHT_UI))
            .w(layout::PALETTE_WIDTH)
            .margin_top(layout::PALETTE_TOP_OFFSET)
            .close_button(false)
            .child(dialog_window_actions(
                div().child(view.clone()),
                shell_focus.clone(),
            ))
    });
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Page, Selection, Tool};
    use crate::palette::items::{ItemKind, PaletteItem};
    use crate::palette::registry::Group;
    use crate::test_support::{
        exists, log_at, open_shell, open_shell_snapshot, press, request, snapshot,
    };
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt as _;

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

    fn first_id(m: &Entity<PaletteModel>, cx: &mut gpui_kit::VisualTestContext) -> String {
        m.read_with(cx, |m, _| m.item(0).id.clone())
    }

    #[test]
    fn merging_keeps_the_item_the_user_moved_to() {
        let mut model = PaletteModel::new(command_items(), &[]);
        let moved_id = model.item(3).id.clone();
        let extra = |id: &str| PaletteItem {
            id: id.to_string(),
            label: id.to_string(),
            detail: None,
            keywords: Vec::new(),
            group: model.item(0).group,
            kind: ItemKind::SearchLogs(String::new()),
        };
        let more = vec![extra("req:a"), extra("req:b")];
        model.extend(more, &[]);
        // The user had moved: the same item stays selected, wherever it ranks now.
        let row = selection_after_merge(Some(&moved_id), &model);
        assert_eq!(model.item(row).id, moved_id);
        // The user had not moved: the top row.
        assert_eq!(selection_after_merge(None, &model), 0);
        // The item vanished: the top row.
        assert_eq!(selection_after_merge(Some("gone"), &model), 0);
    }

    #[gpui_kit::test]
    fn cmd_k_opens_the_palette(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(!dialog_open(cx));
        open(cx);
        assert!(dialog_open(cx));
    }

    #[gpui_kit::test]
    fn cmd_k_closes(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        let first = model(&shell, cx);
        open(cx);
        assert!(!dialog_open(cx));
        assert_eq!(model(&shell, cx), first, "no second palette was created");
    }

    #[gpui_kit::test]
    fn cmd_shift_f_opens_the_palette(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        assert!(!dialog_open(cx));
        press(cx, "cmd-shift-f", "ctrl-shift-f");
        assert!(dialog_open(cx));
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
    fn arrows_move_the_selection_and_enter_runs_it(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("go to");
        cx.run_until_parked();
        let second = model(&shell, cx).read_with(cx, |m, _| m.item(1).id.clone());
        press(cx, "down", "down");
        press(cx, "enter", "enter");
        assert!(!dialog_open(cx));
        let slug = second
            .strip_prefix("tool.")
            .expect("a tool command is second");
        let tool = Tool::ALL.into_iter().find(|t| t.slug() == slug).unwrap();
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(tool)
        );
    }

    #[gpui_kit::test]
    fn escape_closes_without_side_effects(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        press(cx, "escape", "escape");
        assert!(!dialog_open(cx));
        assert_eq!(shell.read_with(cx, |s, _| s.current().page), Page::Start);
        assert!(!shell.read_with(cx, |s, _| s.history().can_go_back()));
    }

    #[gpui_kit::test]
    fn recent_commands_rank_first(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("go to logs");
        cx.run_until_parked();
        press(cx, "enter", "enter");
        open(cx);
        assert_eq!(first_id(&model(&shell, cx), cx), "tool.logs");
    }

    #[gpui_kit::test]
    fn question_mark_types_into_the_palette(cx: &mut TestAppContext) {
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
        assert_eq!(first_id(&model(&shell, cx), cx), "nav.back");
        assert!(exists(cx, "palette-row-0"));
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

    #[gpui_kit::test]
    fn close_window_works_while_the_palette_is_open(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        open(cx);
        assert!(dialog_open(cx));
        press(cx, "cmd-w", "ctrl-w");
        cx.run_until_parked();
        assert!(cx.windows().is_empty());
    }

    #[gpui_kit::test]
    fn only_visible_rows_render_at_5000_items(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        open(cx);
        cx.run_until_parked();
        let many: Vec<PaletteItem> = (0..5000)
            .map(|i| PaletteItem {
                id: format!("fake.{i}"),
                label: format!("GET /fake/{i}"),
                detail: Some("host \u{b7} 200".into()),
                keywords: Vec::new(),
                group: Group::Data,
                kind: ItemKind::SearchLogs(String::new()),
            })
            .collect();
        model(&shell, cx).update(cx, |m, cx| {
            m.extend(many, &[]);
            cx.notify();
        });
        cx.run_until_parked();
        ROWS.with(|c| c.set(0));
        cx.update(|window, cx| window.render_frame(cx));
        let built = ROWS.with(|c| c.get());
        assert!(built > 0, "rows were built");
        assert!(built <= 30, "built {built} rows for 5000 items");
    }

    fn net_shell(
        cx: &mut TestAppContext,
    ) -> (
        Entity<crate::shell::MainWindow>,
        &mut gpui_kit::VisualTestContext,
    ) {
        let snap = snapshot(|w, run| {
            request(
                w,
                run,
                "GET",
                "https://api.test/v2/products?page=3",
                Some(200),
                1,
                Some(2),
            );
            request(
                w,
                run,
                "POST",
                "https://api.test/v2/cart",
                Some(201),
                3,
                Some(4),
            );
        });
        open_shell_snapshot(cx, snap)
    }

    #[gpui_kit::test]
    fn typing_a_path_finds_the_request_and_enter_navigates_to_it(cx: &mut TestAppContext) {
        let (shell, cx) = net_shell(cx);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("products");
        cx.run_until_parked();
        let m = model(&shell, cx);
        assert_eq!(
            m.read_with(cx, |m, _| m.item(0).label.clone()),
            "GET /v2/products?page=3"
        );
        press(cx, "enter", "enter");
        let loc = shell.read_with(cx, |s, _| s.current().clone());
        assert_eq!(loc.page, Page::Tool(Tool::Network));
        assert!(matches!(loc.selection, Some(Selection::Request(_))));
        assert!(!dialog_open(cx));
    }

    #[gpui_kit::test]
    fn search_logs_item_appears_last_and_runs_a_log_search(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_log(log_at(run, 1, fdt_store::logs::LogLevel::Info));
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        open(cx);
        cx.run_until_parked();
        cx.simulate_input("zzqq");
        cx.run_until_parked();
        let m = model(&shell, cx);
        let (count, last) = m.read_with(cx, |m, _| {
            (m.row_count(), m.item(m.row_count() - 1).label.clone())
        });
        assert_eq!(count, 1);
        assert_eq!(last, "Search logs for \u{201c}zzqq\u{201d}");
        press(cx, "enter", "enter");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(Tool::Logs)
        );
        let q = shell.read_with(cx, |s, cx| {
            s.logs_view().read(cx).search_query().to_string()
        });
        assert_eq!(q, "zzqq");
    }
}
