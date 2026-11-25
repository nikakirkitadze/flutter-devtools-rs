//! The Memory tool's page: the strip's chart slot, a tab bar with the snapshot button, and the
//! tab's body.
//!
//! The page keeps the shared range and the selection the shell hands it, and the chart's tooltip.
//! The three tabs are placeholders that show their empty states until they are built.

use super::chart::{ChartTip, EMPTY_TEXT, MemoryChart};
use super::classes_tab::{ClassesTab, ShowInSnapshot};
use super::group_digits;
use super::leaks_tab::{LeaksTab, PAUSE_TIP};
use crate::actions::TakeHeapSnapshot;
use crate::actions::{ZoomIn, ZoomOut, ZoomToFit};
use crate::focus::PendingFocus;
use crate::keymap::LANES_CONTEXT;
use crate::nav::{Location, Page, Selection, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use crate::tools::{KeyZoom, OpenLocation, ReplaceLocation};
use fdt_capture::ConnStatus;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, radius, space};
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Disableable as _, Sizable as _, Size};
use gpui_kit::{
    Action as _, AnyElement, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    TestSupportExt as _, Window, anchored, deferred, div, point,
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
            Selection::LeakSnapshot(_) | Selection::Leak { .. } | Selection::Growth { .. } => {
                Some(MemoryTab::Leaks)
            }
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
    chart: Entity<MemoryChart>,
    classes: Entity<ClassesTab>,
    leaks: Entity<LeaksTab>,
    /// The heap snapshots the store held when the page last looked, to tell which were removed.
    known_heaps: Vec<fdt_store::heap::HeapId>,
    /// A class name the Snapshot tab should search for, set by "Show in snapshot".
    pending_snapshot_search: Option<SharedString>,
    /// What the pointer is over on the chart, and where.
    tip: Option<(String, Point<Pixels>)>,
    _observe: Subscription,
    _chart: [Subscription; 2],
    _classes: [Subscription; 3],
    _leaks: [Subscription; 2],
}

impl EventEmitter<OpenLocation> for MemoryView {}
impl EventEmitter<ReplaceLocation> for MemoryView {}

impl MemoryView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&session, |this, _, cx| {
            this.heaps_changed(cx);
            cx.notify();
        });
        let focus = cx.focus_handle();
        let chart = cx.new(|cx| MemoryChart::new(session.clone(), focus.clone(), cx));
        let chart_subs = [
            cx.subscribe(&chart, |this, _, tip: &ChartTip, cx| {
                this.tip = tip.0.clone();
                cx.notify();
            }),
            cx.subscribe(&chart, |_, _, e: &OpenLocation, cx| cx.emit(e.clone())),
        ];
        let classes = cx.new(|cx| ClassesTab::new(session.clone(), _window, cx));
        let classes_subs = [
            cx.subscribe(&classes, |_, _, e: &OpenLocation, cx| cx.emit(e.clone())),
            cx.subscribe(&classes, |_, _, e: &ReplaceLocation, cx| cx.emit(e.clone())),
            cx.subscribe(&classes, |this, _, e: &ShowInSnapshot, cx| {
                this.pending_snapshot_search = Some(e.0.clone());
                this.set_tab(MemoryTab::Snapshot, cx);
            }),
        ];
        let leaks = cx.new(|cx| LeaksTab::new(session.clone(), _window, cx));
        let leaks_subs = [
            cx.subscribe(&leaks, |_, _, e: &OpenLocation, cx| cx.emit(e.clone())),
            cx.subscribe(&leaks, |_, _, e: &ReplaceLocation, cx| cx.emit(e.clone())),
        ];
        Self {
            session,
            tab: MemoryTab::Leaks,
            range: None,
            selection: None,
            focus,
            focus_pending: PendingFocus::default(),
            page_active: false,
            chart,
            classes,
            leaks,
            known_heaps: Vec::new(),
            pending_snapshot_search: None,
            tip: None,
            _observe: observe,
            _chart: chart_subs,
            _classes: classes_subs,
            _leaks: leaks_subs,
        }
    }

    pub fn tab(&self) -> MemoryTab {
        self.tab
    }

    pub fn range(&self) -> Option<TimeRange> {
        self.range
    }

    pub fn chart(&self) -> &Entity<MemoryChart> {
        &self.chart
    }

    /// The chart's tooltip, if the pointer is over something.
    pub fn tooltip(&self) -> Option<&str> {
        self.tip.as_ref().map(|(text, _)| text.as_str())
    }

    /// A zoom command from the keys or the palette, for the chart.
    pub fn key_zoom(&mut self, z: KeyZoom, cx: &mut Context<Self>) {
        self.chart.update(cx, |c, cx| c.key_zoom(z, cx));
    }

    /// The class a "Show in snapshot" asked the Snapshot tab to search for.
    pub fn pending_snapshot_search(&self) -> Option<&SharedString> {
        self.pending_snapshot_search.as_ref()
    }

    /// Takes the pending search, for the Snapshot tab to run.
    pub fn take_pending_snapshot_search(&mut self) -> Option<SharedString> {
        self.pending_snapshot_search.take()
    }

    /// Whether the Classes tab's "Live objects only" is on.
    pub fn live_only(&self, cx: &gpui_kit::App) -> bool {
        self.classes.read(cx).live_only()
    }

    /// Asks the app for an allocation profile, with a full GC only when "Live objects only" is
    /// on.
    pub fn refresh_profile(&mut self, cx: &mut Context<Self>) {
        self.classes.update(cx, |c, cx| c.refresh(cx));
    }

    pub fn classes(&self) -> &Entity<ClassesTab> {
        &self.classes
    }

    /// Up or Down through the Leaks list, when it is the tab on screen.
    pub fn step_leaks(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.page_active && self.tab == MemoryTab::Leaks {
            self.leaks.update(cx, |l, cx| l.step(down, cx));
        }
    }

    pub fn leaks(&self) -> &Entity<LeaksTab> {
        &self.leaks
    }

    /// Clears a selection that points at a heap snapshot that was removed.
    fn heaps_changed(&mut self, cx: &mut Context<Self>) {
        let now: Vec<_> = self
            .session
            .read(cx)
            .snapshot()
            .memory
            .heaps()
            .iter()
            .map(|h| h.id)
            .collect();
        let gone = self
            .selection
            .and_then(Selection::heap)
            .is_some_and(|h| self.known_heaps.contains(&h) && !now.contains(&h));
        self.known_heaps = now;
        if gone {
            self.selection = None;
            self.leaks.update(cx, |l, cx| l.set_selection(None, cx));
            cx.emit(ReplaceLocation(Location {
                page: Page::Tool(Tool::Memory),
                selection: None,
                range: self.range,
            }));
        }
    }

    fn sync_classes(&mut self, cx: &mut Context<Self>) {
        let on = self.page_active && self.tab == MemoryTab::Classes;
        self.classes.update(cx, |c, cx| c.set_active(on, cx));
        let on = self.page_active && self.tab == MemoryTab::Leaks;
        self.leaks.update(cx, |l, cx| l.set_active(on, cx));
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Starts again for a new session: the default tab, with no range or selection.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.tab = MemoryTab::Leaks;
        self.range = None;
        self.selection = None;
        self.tip = None;
        self.pending_snapshot_search = None;
        self.chart
            .update(cx, |c, cx| c.set_location(None, None, cx));
        self.classes.update(cx, |c, cx| c.reset(cx));
        self.leaks.update(cx, |l, cx| l.reset(cx));
        self.known_heaps.clear();
        self.sync_classes(cx);
        cx.notify();
    }

    pub fn set_tab(&mut self, tab: MemoryTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.sync_classes(cx);
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
        self.chart
            .update(cx, |c, cx| c.set_location(range, selection, cx));
        self.classes.update(cx, |c, cx| {
            c.set_range(range, cx);
            c.set_selection(selection, cx);
        });
        self.leaks.update(cx, |l, cx| {
            l.set_range(range, cx);
            l.set_selection(selection, cx);
        });
        if selected && let Some(tab) = selection.and_then(MemoryTab::showing) {
            self.tab = tab;
        }
        self.sync_classes(cx);
        cx.notify();
    }

    /// Tells the page whether it is the one on screen. Arriving asks for the focus.
    pub fn set_page_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if active == self.page_active {
            return;
        }
        self.page_active = active;
        self.chart.update(cx, |c, cx| c.set_active(active, cx));
        self.sync_classes(cx);
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
        take = take.tooltip(PAUSE_TIP);
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

    /// The chart above the tabs, or what it says while there are no samples.
    fn render_chart_slot(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let slot = div()
            .id("memory-chart")
            .test_support()
            .flex_none()
            .w_full()
            .h(layout::MEMORY_CHART_HEIGHT)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border));
        if self.session.read(cx).snapshot().memory.samples().is_empty() {
            return slot
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("memory-chart-empty")
                        .test_support()
                        .text_size(font::BODY)
                        .text_color(Hsla::from(t.text_muted))
                        .child(EMPTY_TEXT),
                )
                .into_any_element();
        }
        slot.child(
            self.chart
                .clone()
                .cached(StyleRefinement::default().size_full()),
        )
        .into_any_element()
    }

    fn render_tooltip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let (text, at) = self.tip.clone()?;
        let offset = layout::TOOLTIP_OFFSET;
        Some(
            deferred(
                anchored()
                    .position(at + point(offset, offset))
                    .snap_to_window_with_margin(space::S8)
                    .child(
                        div()
                            .id("memory-chart-tooltip")
                            .test_support()
                            .aria_label(SharedString::from(text.clone()))
                            .px(space::S8)
                            .py(space::S4)
                            .rounded(radius::CONTROL)
                            .bg(Hsla::from(t.elevated))
                            .border_1()
                            .border_color(Hsla::from(t.border))
                            .text_size(font::LABEL)
                            .text_color(Hsla::from(t.text))
                            .child(text),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
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
                return div()
                    .id("memory-tab-classes")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.classes.clone())
                    .into_any_element();
            }
            MemoryTab::Leaks => {
                return div()
                    .id("memory-tab-leaks")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.leaks.clone())
                    .into_any_element();
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
            .key_context(LANES_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.key_zoom(KeyZoom::In, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.key_zoom(KeyZoom::Out, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.key_zoom(KeyZoom::Fit, cx)))
            .child(self.render_chart_slot(cx))
            .child(self.render_tabs(cx))
            .child(self.render_body(cx))
            .children(self.render_tooltip(cx))
    }
}
