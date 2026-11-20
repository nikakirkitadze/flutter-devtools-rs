//! The Performance tool's page: a tab bar, the debug-mode banner and the tab's body.
//!
//! The page keeps the shared range and the selected frame the shell hands it, so each tab shows
//! the same slice of the session.

use crate::nav::{EntryId, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use crate::tools::OpenLocation;
use fdt_store::run::Mode;
use fdt_ui::assets::icons;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, space};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Icon, Sizable as _, Size};
use gpui_kit::{
    AnyElement, Context, Entity, EventEmitter, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, TestSupportExt as _, Window, div,
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
            PerfTab::Frames => "Frames appear here as your app draws them.",
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
    _observe: Subscription,
}

impl EventEmitter<OpenLocation> for PerformanceView {}

impl PerformanceView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&session, |_, _, cx| cx.notify());
        Self {
            session,
            tab: PerfTab::Frames,
            range: None,
            selection: None,
            _observe: observe,
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
            cx.notify();
        }
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

impl Render for PerformanceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::PerformanceView, 1);
        let body = EmptyState::new(
            "perf-empty",
            tool_icon(Tool::Performance),
            "Nothing to show yet",
            self.tab.empty_body(),
        );
        div()
            .id("performance-view")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(cx))
            .children(self.debug_run(cx).then(|| self.render_debug_banner(cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(body),
            )
    }
}
