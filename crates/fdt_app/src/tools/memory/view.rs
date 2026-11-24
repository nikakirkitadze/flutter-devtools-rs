//! The Memory tool's page: the strip's chart slot, a tab bar with the snapshot button, and the
//! tab's body.
//!
//! The page keeps the shared range and the selection the shell hands it. The chart and the three
//! tabs are placeholders that show their empty states until they are built.

use super::{group_digits, heap_number};
use crate::actions::TakeHeapSnapshot;
use crate::focus::PendingFocus;
use crate::nav::{Selection, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use fdt_capture::ConnStatus;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, space};
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Disableable as _, Sizable as _, Size};
use gpui_kit::{
    Action as _, AnyElement, Context, Entity, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    TestSupportExt as _, Window, div,
};

/// The tooltip of the snapshot button while no app is connected.
pub const TAKE_SNAPSHOT_TIP: &str = "Connect to an app to take a heap snapshot.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryTab {
    Classes,
    Leaks,
    Snapshot,
}

impl MemoryTab {
    pub const ALL: [MemoryTab; 3] = [MemoryTab::Classes, MemoryTab::Leaks, MemoryTab::Snapshot];

    pub fn label(self) -> &'static str {
        match self {
            MemoryTab::Classes => "Classes",
            MemoryTab::Leaks => "Leaks",
            MemoryTab::Snapshot => "Snapshot",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            MemoryTab::Classes => "classes",
            MemoryTab::Leaks => "leaks",
            MemoryTab::Snapshot => "snapshot",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// The tab that shows `selection`, for a selection the Memory tool owns.
    fn showing(selection: Selection) -> Option<MemoryTab> {
        match selection {
            Selection::AllocClass(_) => Some(MemoryTab::Classes),
            Selection::Leak { .. } | Selection::Growth { .. } => Some(MemoryTab::Leaks),
            Selection::Heap(_) | Selection::HeapNode { .. } => Some(MemoryTab::Snapshot),
            Selection::Log(_)
            | Selection::Request(_)
            | Selection::Socket(_)
            | Selection::Frame(_) => None,
        }
    }
}

pub struct MemoryView {
    session: Entity<SessionModel>,
    /// Leaks is the default, so opening the page never pauses the app.
    tab: MemoryTab,
    range: Option<TimeRange>,
    selection: Option<Selection>,
    /// Where the keyboard focus goes when the page is shown.
    focus: FocusHandle,
    focus_pending: PendingFocus,
    /// Whether the Memory page is the current one.
    page_active: bool,
    _observe: Subscription,
}

impl MemoryView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&session, |_, _, cx| cx.notify());
        Self {
            session,
            tab: MemoryTab::Leaks,
            range: None,
            selection: None,
            focus: cx.focus_handle(),
            focus_pending: PendingFocus::default(),
            page_active: false,
            _observe: observe,
        }
    }

    pub fn tab(&self) -> MemoryTab {
        self.tab
    }

    pub fn range(&self) -> Option<TimeRange> {
        self.range
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Starts again for a new session: the default tab, with no range or selection.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.tab = MemoryTab::Leaks;
        self.range = None;
        self.selection = None;
        cx.notify();
    }

    pub fn set_tab(&mut self, tab: MemoryTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// Takes the range and selection of the current location. A new selection shows the tab it
    /// belongs to; one the Memory tool does not own is not kept.
    pub fn set_location(
        &mut self,
        range: Option<TimeRange>,
        selection: Option<Selection>,
        cx: &mut Context<Self>,
    ) {
        let selection = selection.filter(|s| s.is_memory());
        let selected = selection != self.selection;
        if range == self.range && !selected {
            return;
        }
        self.range = range;
        self.selection = selection;
        if selected && let Some(tab) = selection.and_then(MemoryTab::showing) {
            self.tab = tab;
        }
        cx.notify();
    }

    /// Tells the page whether it is the one on screen. Arriving asks for the focus.
    pub fn set_page_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if active == self.page_active {
            return;
        }
        self.page_active = active;
        if active {
            self.focus_pending.request();
            cx.notify();
        } else {
            self.focus_pending.cancel();
        }
    }

    fn connected(&self, cx: &gpui_kit::App) -> bool {
        matches!(self.session.read(cx).status(), ConnStatus::Connected { .. })
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let view = cx.weak_entity();
        let mut take = Button::new("memory-take-snapshot-button")
            .label("Take snapshot")
            .compact()
            .on_click(|_, window, cx| {
                window.dispatch_action(TakeHeapSnapshot.boxed_clone(), cx);
            });
        if !self.connected(cx) {
            take = take.disabled(true).tooltip(TAKE_SNAPSHOT_TIP);
        }
        div()
            .id("memory-tabs")
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .w_full()
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(
                div().flex_1().child(
                    TabBar::new("memory-tabs-bar")
                        .underline()
                        .with_size(Size::Small)
                        .selected_index(self.tab.index())
                        .on_click(move |ix: &usize, _, cx| {
                            let ix = *ix;
                            view.update(cx, |v, cx| {
                                if let Some(&tab) = MemoryTab::ALL.get(ix) {
                                    v.set_tab(tab, cx);
                                }
                            })
                            .ok();
                        })
                        .children(MemoryTab::ALL.map(|tab| Tab::new().label(tab.label()))),
                ),
            )
            .child(
                div()
                    .id("memory-take-snapshot")
                    .test_support()
                    .flex_none()
                    .child(take),
            )
            .into_any_element()
    }

    /// The chart's place above the tabs.
    fn render_chart_slot(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::perf::bump(crate::perf::Counter::MemoryChart, 1);
        let t = *cx.tokens();
        let samples = self.session.read(cx).snapshot().memory.samples().len();
        let (id, text) = if samples == 0 {
            (
                "memory-chart-empty",
                "No memory samples yet. They are recorded while an app is connected.".to_string(),
            )
        } else {
            (
                "memory-chart-placeholder",
                format!("{} memory samples recorded.", group_digits(samples as u64)),
            )
        };
        div()
            .id("memory-chart")
            .test_support()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w_full()
            .h(layout::FRAME_CHART_HEIGHT)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(
                div()
                    .id(id)
                    .test_support()
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text_muted))
                    .child(text),
            )
            .into_any_element()
    }

    fn empty(
        &self,
        id: &'static str,
        heading: &'static str,
        body: &'static str,
    ) -> gpui_kit::AnyElement {
        EmptyState::new(id, tool_icon(Tool::Memory), heading, body).into_any_element()
    }

    /// A line for a tab that has data its real body will show.
    fn note(&self, id: &'static str, text: String, cx: &Context<Self>) -> AnyElement {
        let t = cx.tokens();
        div()
            .id(id)
            .test_support()
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text_muted))
            .child(text)
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.session.read(cx).snapshot().clone();
        let memory = &snap.memory;
        let content = match self.tab {
            MemoryTab::Classes => {
                if memory.profiles().is_empty() {
                    self.empty(
                        "memory-classes-empty",
                        "No allocation profile yet",
                        "Refresh the allocation profile to see which classes use the most memory.",
                    )
                } else {
                    self.note(
                        "memory-classes-placeholder",
                        format!(
                            "{} classes in the allocation profile.",
                            group_digits(memory.alloc_class_count() as u64)
                        ),
                        cx,
                    )
                }
            }
            MemoryTab::Leaks => {
                if let Some(busy) = memory.busy() {
                    self.note(
                        "memory-leaks-busy",
                        format!("Taking heap snapshot #{}.", heap_number(busy)),
                        cx,
                    )
                } else if memory.heaps().is_empty() {
                    self.empty(
                        "memory-leaks-empty",
                        "No heap snapshot yet",
                        "Take a heap snapshot to check for leaked objects.",
                    )
                } else {
                    self.note(
                        "memory-leaks-placeholder",
                        format!(
                            "{} heap snapshots taken.",
                            group_digits(memory.heaps().len() as u64)
                        ),
                        cx,
                    )
                }
            }
            MemoryTab::Snapshot => {
                if memory.heaps().is_empty() {
                    self.empty(
                        "memory-snapshot-empty",
                        "No heap snapshot yet",
                        "Take a heap snapshot to browse the objects in memory.",
                    )
                } else {
                    self.note(
                        "memory-snapshot-placeholder",
                        format!(
                            "{} heap snapshots taken.",
                            group_digits(memory.heaps().len() as u64)
                        ),
                        cx,
                    )
                }
            }
        };
        div()
            .id(SharedString::from(format!(
                "memory-tab-{}",
                self.tab.slug()
            )))
            .test_support()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }
}

impl Render for MemoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::MemoryView, 1);
        if self.page_active {
            self.focus_pending.take(window, cx, &self.focus, true);
        }
        div()
            .id("memory-view")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_chart_slot(cx))
            .child(self.render_tabs(cx))
            .child(self.render_body(cx))
    }
}
