//! The Timeline tool's page: frames, requests, logs and GC events on one time axis.

use crate::nav::{TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use crate::tools::OpenLocation;
use fdt_ui::empty_state::EmptyState;
use gpui_kit::{
    Context, Entity, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, TestSupportExt as _, Window, div,
};

pub struct TimelineView {
    _session: Entity<SessionModel>,
    range: Option<TimeRange>,
    _observe: Subscription,
}

impl EventEmitter<OpenLocation> for TimelineView {}

impl TimelineView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&session, |_, _, cx| cx.notify());
        Self {
            _session: session,
            range: None,
            _observe: observe,
        }
    }

    pub fn range(&self) -> Option<TimeRange> {
        self.range
    }

    /// The shared time range, from the shell's location.
    pub fn set_location(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            cx.notify();
        }
    }
}

impl Render for TimelineView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::TimelineView, 1);
        div()
            .id("timeline-view")
            .test_support()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(EmptyState::new(
                "timeline-empty",
                tool_icon(Tool::Timeline),
                "Nothing to show yet",
                "Frames, requests and logs appear here as your app runs.",
            ))
    }
}
