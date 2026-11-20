//! The Performance tool's page: a tab bar, the debug-mode banner and the tab's body.
//!
//! The page keeps the shared range and the selected frame the shell hands it, so each tab shows
//! the same slice of the session.

use super::frame_chart::{ChartEvent, FrameChart};
use super::frames_table::{FramesList, ListEvent};
use crate::nav::{EntryId, Location, Page, Selection, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use crate::shell::strip::format_offset;
use crate::tools::{OpenLocation, ReplaceLocation};
use fdt_store::run::Mode;
use fdt_ui::assets::icons;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, space};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Icon, Sizable as _, Size};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    TestSupportExt as _, Window, anchored, deferred, div, point,
};

/// What the debug banner says.
pub const DEBUG_BANNER: &str = "Debug mode timings aren't representative. Run the app with flutter run --profile for real numbers.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfTab {
    Frames,
    Cpu,
    Rebuilds,
    Trace,
}

impl PerfTab {
    pub const ALL: [PerfTab; 4] = [
        PerfTab::Frames,
        PerfTab::Cpu,
        PerfTab::Rebuilds,
        PerfTab::Trace,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PerfTab::Frames => "Frames",
            PerfTab::Cpu => "CPU",
            PerfTab::Rebuilds => "Rebuilds",
            PerfTab::Trace => "Raw trace",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn empty_body(self) -> &'static str {
        match self {
            PerfTab::Frames => "Frames appear as your app draws. Use the app to record some.",
            PerfTab::Cpu => "CPU samples appear here once your app has been running for a moment.",
            PerfTab::Rebuilds => "Widget rebuilds appear here when widget-build tracing is on.",
            PerfTab::Trace => "Timeline events appear here as your app records them.",
        }
    }
}

pub struct PerformanceView {
    session: Entity<SessionModel>,
    tab: PerfTab,
    range: Option<TimeRange>,
    selection: Option<EntryId>,
    chart: Entity<FrameChart>,
    list: Entity<FramesList>,
    /// The frame under the pointer and where, for the tooltip.
    hover: Option<(usize, Point<Pixels>)>,
    /// Whether the chart follows the newest frame; "Latest" shows while it does not.
    following: bool,
    /// Whether the Performance page is the current one.
    page_active: bool,
    _observe: Subscription,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenLocation> for PerformanceView {}
impl EventEmitter<ReplaceLocation> for PerformanceView {}

impl PerformanceView {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, session, cx| {
            let snap = session.read(cx).snapshot().clone();
            this.chart.update(cx, |c, cx| c.set_snapshot(snap, cx));
            cx.notify();
        });
        let snap = session.read(cx).snapshot().clone();
        let chart = cx.new(|cx| FrameChart::new(snap, cx));
        let list = cx.new(|cx| FramesList::new(session.clone(), window, cx));
        let subs = vec![
            cx.subscribe(&chart, |this, _, e: &ChartEvent, cx| {
                this.chart_event(*e, cx)
            }),
            cx.subscribe(&list, |this, _, e: &ListEvent, cx| this.list_event(*e, cx)),
        ];
        Self {
            session,
            tab: PerfTab::Frames,
            range: None,
            selection: None,
            chart,
            list,
            hover: None,
            following: true,
            page_active: false,
            _observe: observe,
            _subs: subs,
        }
    }

    #[cfg(test)]
    pub(crate) fn chart(&self) -> &Entity<FrameChart> {
        &self.chart
    }

    #[cfg(test)]
    pub(crate) fn list(&self) -> &Entity<FramesList> {
        &self.list
    }

    #[cfg(test)]
    pub(crate) fn snapshot_frame(
        &self,
        seq: usize,
        cx: &gpui_kit::App,
    ) -> Option<fdt_store::frames::FrameRecord> {
        self.session.read(cx).snapshot().frames.get(seq).cloned()
    }

    /// The location with frame `seq` selected, on this page and in the same range.
    fn frame_location(&self, seq: usize, cx: &gpui_kit::App) -> Option<Location> {
        let id = self.session.read(cx).snapshot().frames.get(seq)?.id;
        Some(Location {
            page: Page::Tool(Tool::Performance),
            selection: Some(Selection::Frame(id)),
            range: self.range,
        })
    }

    /// Asks the shell to select frame `seq`. A click adds a Back step; keys replace the step.
    fn pick(&mut self, seq: usize, by_key: bool, cx: &mut Context<Self>) {
        let Some(to) = self.frame_location(seq, cx) else {
            return;
        };
        if by_key {
            cx.emit(ReplaceLocation(to));
        } else {
            cx.emit(OpenLocation(to));
        }
    }

    fn chart_event(&mut self, event: ChartEvent, cx: &mut Context<Self>) {
        match event {
            ChartEvent::Select { seq, by_key } => self.pick(seq, by_key, cx),
            ChartEvent::Range { first, last } => {
                let snap = self.session.read(cx).snapshot().clone();
                let (Some(a), Some(z)) = (snap.frames.get(first), snap.frames.get(last)) else {
                    return;
                };
                cx.emit(OpenLocation(Location {
                    page: Page::Tool(Tool::Performance),
                    selection: self.selection.map(Selection::Frame),
                    range: Some(TimeRange::new(a.start, z.end())),
                }));
            }
            ChartEvent::Hover(hover) => {
                if self.hover != hover {
                    self.hover = hover;
                    cx.notify();
                }
            }
            ChartEvent::Follow(on) => {
                self.following = on;
                cx.notify();
            }
        }
    }

    fn list_event(&mut self, event: ListEvent, cx: &mut Context<Self>) {
        match event {
            ListEvent::Selected { seq, by_key } => self.pick(seq, by_key, cx),
            ListEvent::Open(seq) => self.pick(seq, false, cx),
        }
    }

    /// Enter: opens the selected frame, adding a Back step.
    pub fn open_details(&mut self, cx: &mut Context<Self>) {
        if self.tab == PerfTab::Frames {
            self.list.update(cx, |l, cx| l.open_selected(cx));
        }
    }

    pub fn tab(&self) -> PerfTab {
        self.tab
    }

    pub fn range(&self) -> Option<TimeRange> {
        self.range
    }

    /// The selected frame.
    pub fn selection(&self) -> Option<EntryId> {
        self.selection
    }

    pub fn set_tab(&mut self, tab: PerfTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.refresh_active(cx);
            cx.notify();
        }
    }

    /// Whether the Performance page is on screen. The frames table only works while its tab is.
    pub fn set_page_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.page_active != active {
            self.page_active = active;
            self.refresh_active(cx);
        }
    }

    fn refresh_active(&mut self, cx: &mut Context<Self>) {
        let on = self.page_active && self.tab == PerfTab::Frames;
        self.list.update(cx, |l, cx| l.set_active(on, cx));
    }

    /// The shared time range and the selected frame, from the shell's location.
    pub fn set_location(
        &mut self,
        range: Option<TimeRange>,
        selection: Option<EntryId>,
        cx: &mut Context<Self>,
    ) {
        if self.range != range || self.selection != selection {
            self.range = range;
            self.selection = selection;
            let seq = selection.map(|id| id.seq as usize);
            self.chart.update(cx, |c, cx| {
                c.set_range(range, cx);
                c.set_selection(seq, cx);
            });
            self.list.update(cx, |l, cx| {
                l.set_range(range, cx);
                l.set_selection(seq, cx);
            });
            cx.notify();
        }
    }

    /// Whether the latest run is a debug build, whose timings mean little.
    fn debug_run(&self, cx: &gpui_kit::App) -> bool {
        let session = self.session.read(cx);
        session
            .snapshot()
            .runs
            .last()
            .is_some_and(|r| r.app.mode == Mode::Debug)
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let view = cx.weak_entity();
        div()
            .id("perf-tabs")
            .test_support()
            .flex_none()
            .w_full()
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(
                TabBar::new("perf-tabs-bar")
                    .underline()
                    .with_size(Size::Small)
                    .selected_index(self.tab.index())
                    .on_click(move |ix: &usize, _, cx| {
                        let ix = *ix;
                        view.update(cx, |v, cx| {
                            if let Some(&tab) = PerfTab::ALL.get(ix) {
                                v.set_tab(tab, cx);
                            }
                        })
                        .ok();
                    })
                    .children(PerfTab::ALL.map(|tab| Tab::new().label(tab.label()))),
            )
            .into_any_element()
    }

    fn render_debug_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        div()
            .id("perf-debug-banner")
            .test_support()
            .aria_label(SharedString::from(DEBUG_BANNER))
            .flex()
            .items_center()
            .gap(space::S8)
            .h(layout::BANNER_HEIGHT)
            .flex_none()
            .px(space::S16)
            .bg(t.warning.with_alpha(alpha::ACTIVE_ROW))
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text))
            .child(
                Icon::new(icons::WARNING)
                    .size(layout::ICON_MD)
                    .text_color(Hsla::from(t.warning)),
            )
            .child(div().flex_1().child(DEBUG_BANNER))
            .into_any_element()
    }
}

impl PerformanceView {
    /// The Frames tab: the chart over the table, or the empty state.
    fn render_frames(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let snap = self.session.read(cx).snapshot().clone();
        if snap.frames.is_empty() {
            let label =
                "No frames yet. Frames appear as your app draws. Use the app to record some.";
            return div()
                .id("frames-none")
                .test_support()
                .aria_label(label)
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "perf-empty",
                    tool_icon(Tool::Performance),
                    "No frames yet",
                    "Frames appear as your app draws. Use the app to record some.",
                ))
                .into_any_element();
        }
        let latest = (!self.following).then(|| {
            div()
                .id("frames-latest")
                .test_support()
                .aria_label("Latest")
                .absolute()
                .top(space::S8)
                .right(space::S16)
                .px(space::S8)
                .py(space::S2)
                .rounded(radius::CHIP)
                .bg(Hsla::from(t.elevated))
                .border_1()
                .border_color(Hsla::from(t.border))
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chart.update(cx, |c, cx| c.latest(cx));
                }))
                .child("Latest")
        });
        let tooltip = self.hover.and_then(|(seq, at)| {
            let f = snap.frames.get(seq)?;
            let ms = |us: u32| format!("{:.1} ms", f64::from(us) / 1000.0);
            let text = format!(
                "Frame #{} \u{b7} {} \u{b7} build {} \u{b7} raster {}",
                f.number,
                format_offset(f.start),
                ms(f.build_us),
                ms(f.raster_us)
            );
            let offset = layout::TOOLTIP_OFFSET;
            Some(
                deferred(
                    anchored()
                        .position(at + point(offset, offset))
                        .snap_to_window_with_margin(space::S8)
                        .child(
                            div()
                                .id("frame-tooltip")
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
                .with_priority(1),
            )
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w_full()
                    .h(layout::FRAME_CHART_HEIGHT)
                    .border_b(layout::BORDER)
                    .border_color(Hsla::from(t.border))
                    .child(
                        self.chart.clone().cached(
                            StyleRefinement::default()
                                .w_full()
                                .h(layout::FRAME_CHART_HEIGHT),
                        ),
                    )
                    .children(latest),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    self.list
                        .clone()
                        .cached(StyleRefinement::default().size_full()),
                ),
            )
            .children(tooltip)
            .into_any_element()
    }
}

impl Render for PerformanceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::PerformanceView, 1);
        let body = if self.tab == PerfTab::Frames {
            self.render_frames(cx)
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "perf-empty",
                    tool_icon(Tool::Performance),
                    "Nothing to show yet",
                    self.tab.empty_body(),
                ))
                .into_any_element()
        };
        div()
            .id("performance-view")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(cx))
            .children(self.debug_run(cx).then(|| self.render_debug_banner(cx)))
            .child(body)
    }
}
