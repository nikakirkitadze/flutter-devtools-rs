//! The Timeline tool's page: a ruler above shared time lanes for frames, requests, logs, route
//! changes, garbage collection and findings.
//!
//! Each lane is an entity of its own, drawn `.cached(..)` in one layer and notified only when
//! its geometry changes. The page owns what moves between them: the viewport, the ruler and its
//! drag band (an overlay canvas on top of the lanes, with a quad budget of its own), the input
//! and the tooltip, so hovering never repaints a lane.

use super::lanes::{
    LaneGeometry, LaneHit, LaneKind, Spans, cell_tooltip, fetch_spans, hit_at, hue_colour,
    label_colour, lane_geometry, location_for, name_colour, resolve_cell, span_fetch_range,
    tooltip_text, wants_spans, worst_frame,
};
use crate::actions::{ZoomIn, ZoomOut, ZoomToFit};
use crate::keymap::LANES_CONTEXT;
use crate::nav::{Location, Page, Selection, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::sidebar::tool_icon;
use crate::shell::strip::{MIN_DRAG_PX, session_span};
use crate::tools::OpenLocation;
use crate::tools::performance::trace_tab::{Rect, TICK_TARGET_PX, ruler_ticks};
use fdt_store::Snapshot;
use fdt_store::time::SessionTime;
use fdt_ui::canvas::{
    CANVAS_MAX_QUADS, Label, Quad, ZoomPan, paint_labels, paint_quads, zoom_pan_listeners,
};
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use fdt_ui::viewport::Viewport;
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, CursorStyle, DispatchPhase, Entity, EventEmitter,
    FocusHandle, HitboxBehavior, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription,
    Task, TestSupportExt as _, Window, anchored, canvas, deferred, div, point, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// What the page says when nothing was recorded.
pub const EMPTY_TEXT: &str = "Nothing recorded yet. Connect to an app to fill the timeline.";
/// The same while an app is connected.
pub const EMPTY_CONNECTED_TEXT: &str =
    "Nothing recorded yet. Timeline lanes fill in as your app runs.";
/// How much one press of the zoom keys changes the scale.
const KEY_ZOOM: f32 = 1.5; // token-check: allow: zoom step, not a visual size
/// The page size before it has been drawn once.
const FIRST_WIDTH_PX: f32 = 900.0; // token-check: allow: stand-in until the first paint
const FIRST_HEIGHT_PX: f32 = 400.0; // token-check: allow: stand-in until the first paint
/// The length of a ruler tick.
const TICK_PX: f32 = 4.0; // token-check: allow: tick length

/// Quads kept for the overlay canvas: the ruler ticks (at most 200), a border under the ruler
/// and one under each lane, the label column's border, the range band and the drag band.
pub const OVERLAY_QUADS: usize = 256;

/// What an overlay quad is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    Range,
    Drag,
    Tick,
    Border,
}

/// The overlay quads in pixels from the page's top-left corner: the shared `range` band and the
/// `drag` band (both `(x0, x1)`, from the left edge), the ruler `ticks` (x from the plot's left
/// edge), and the borders: under the ruler, under each of `lanes` lanes, and right of the label
/// column. `page` is the page's size. Never more than `OVERLAY_QUADS`.
pub fn overlay_rects(
    ticks: &[f32],
    range: Option<(f32, f32)>,
    drag: Option<(f32, f32)>,
    page: (f32, f32),
    lanes: usize,
) -> Vec<(Rect, Overlay)> {
    let (pw, ph) = page;
    let label_w = f32::from(layout::LANE_LABEL_WIDTH);
    let ruler_h = f32::from(layout::RULER_HEIGHT);
    let lane_h = f32::from(layout::LANE_HEIGHT);
    let e = f32::from(layout::BORDER);
    let bottom = (ruler_h + lanes as f32 * lane_h).min(ph);
    let mut out = Vec::new();
    for (band, kind) in [(range, Overlay::Range), (drag, Overlay::Drag)] {
        if let Some((a, z)) = band {
            let (a, z) = (a.min(z).max(label_w), a.max(z).min(pw));
            if z > a {
                out.push((
                    Rect {
                        x: a,
                        y: 0.0,
                        w: z - a,
                        h: bottom,
                    },
                    kind,
                ));
            }
        }
    }
    for x in ticks.iter().take(200) {
        out.push((
            Rect {
                x: label_w + x,
                y: ruler_h - TICK_PX,
                w: e,
                h: TICK_PX,
            },
            Overlay::Tick,
        ));
    }
    out.push((
        Rect {
            x: 0.0,
            y: ruler_h - e,
            w: pw,
            h: e,
        },
        Overlay::Border,
    ));
    for i in 0..lanes {
        out.push((
            Rect {
                x: 0.0,
                y: ruler_h + (i + 1) as f32 * lane_h - e,
                w: pw,
                h: e,
            },
            Overlay::Border,
        ));
    }
    out.push((
        Rect {
            x: label_w - e,
            y: 0.0,
            w: e,
            h: bottom,
        },
        Overlay::Border,
    ));
    debug_assert!(out.len() <= OVERLAY_QUADS, "{} overlay quads", out.len());
    out.truncate(OVERLAY_QUADS);
    out
}

/// One lane: its geometry, and a canvas that paints it.
pub struct LaneView {
    kind: LaneKind,
    geo: Arc<LaneGeometry>,
}

impl LaneView {
    fn new(kind: LaneKind) -> Self {
        Self {
            kind,
            geo: Arc::new(LaneGeometry::default()),
        }
    }

    /// Lays the lane out for the inputs; repaints only when the result differs.
    fn set_inputs(
        &mut self,
        snap: &Snapshot,
        vp: Viewport,
        width: f32,
        spans: Option<&Spans>,
        cx: &mut Context<Self>,
    ) {
        let geo = lane_geometry(self.kind, snap, vp, width, CANVAS_MAX_QUADS, spans);
        if *self.geo != geo {
            self.geo = Arc::new(geo);
            cx.notify();
        }
    }

    pub fn geometry(&self) -> &Arc<LaneGeometry> {
        &self.geo
    }
}

impl Render for LaneView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::Lanes, 1);
        let t = *cx.tokens();
        let geo = self.geo.clone();
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, (), window, cx| {
                let quads: Vec<Quad> = geo
                    .quads
                    .iter()
                    .map(|q| Quad {
                        bounds: Bounds::new(
                            point(
                                bounds.origin.x + px(q.rect.x),
                                bounds.origin.y + px(q.rect.y),
                            ),
                            size(px(q.rect.w), px(q.rect.h)),
                        ),
                        color: Hsla::from(hue_colour(&t, q.hue)),
                    })
                    .collect();
                paint_quads(window, bounds, &quads);
                let line = font::LABEL * font::LINE_HEIGHT_UI;
                let labels: Vec<Label> = geo
                    .labels
                    .iter()
                    .map(|l| Label {
                        text: SharedString::from(l.text.clone()),
                        origin: point(
                            bounds.origin.x + px(l.rect.x),
                            bounds.origin.y + px(l.rect.y) + (px(l.rect.h) - line) / 2.0,
                        ),
                        max_width: px(l.rect.w),
                        color: Hsla::from(label_colour(&t)),
                    })
                    .collect();
                paint_labels(window, cx, bounds, &labels);
            },
        )
        .size_full()
    }
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    start_x: f32,
    start_y: f32,
    x: f32,
}

/// What the pointer is over: the lane, where in it, and the window position for the tooltip.
#[derive(Clone, Copy, Debug)]
struct Hover {
    kind: LaneKind,
    x: f32,
    y: f32,
    hit: LaneHit,
    at: Point<Pixels>,
}

pub struct TimelineView {
    session: Entity<SessionModel>,
    snap: Arc<Snapshot>,
    /// The shared range the view fits, if any.
    range: Option<TimeRange>,
    viewport: Viewport,
    /// Whether the view fits its range or the session, so new data refits it.
    fitted: bool,
    /// Every lane, shown or not.
    lanes: Vec<(LaneKind, Entity<LaneView>)>,
    visible: Vec<LaneKind>,
    /// The lane width the lanes were last laid out for.
    lane_width: f32,
    /// The page's size at its last paint.
    size: Rc<Cell<(f32, f32)>>,
    drag: Option<Drag>,
    hover: Option<Hover>,
    tip: Option<String>,
    /// Which cell, in which snapshot, the pending (or last) background tooltip is for.
    tip_key: Option<(LaneHit, u64)>,
    tip_task: Option<Task<()>>,
    /// Requests fetched for the request lane's spans, and what they were fetched for.
    spans: Option<Spans>,
    /// The (session, request count) the spans were fetched for.
    /// What the spans were fetched from: see [`TimelineView::spans_key`].
    spans_for: Option<SpansKey>,
    spans_jobs: usize,
    spans_task: Option<Task<()>>,
    click_task: Option<Task<()>>,
    focus: FocusHandle,
    /// The page was just opened: the lanes take the focus when they are next drawn.
    focus_pending: bool,
    _observe: Subscription,
}

impl EventEmitter<OpenLocation> for TimelineView {}

impl TimelineView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&session, |this, _, cx| this.take_snapshot(cx));
        let snap = session.read(cx).snapshot().clone();
        let lanes = LaneKind::ALL
            .iter()
            .map(|k| (*k, cx.new(|_| LaneView::new(*k))))
            .collect();
        let mut v = Self {
            session,
            snap,
            range: None,
            viewport: Viewport::fit(0, 0),
            fitted: true,
            lanes,
            visible: Vec::new(),
            lane_width: FIRST_WIDTH_PX - f32::from(layout::LANE_LABEL_WIDTH),
            size: Rc::new(Cell::new((FIRST_WIDTH_PX, FIRST_HEIGHT_PX))),
            drag: None,
            hover: None,
            tip: None,
            tip_key: None,
            tip_task: None,
            spans: None,
            spans_for: None,
            spans_jobs: 0,
            spans_task: None,
            click_task: None,
            focus: cx.focus_handle(),
            focus_pending: false,
            _observe: observe,
        };
        v.refresh_visible();
        v.fit();
        v.sync(cx);
        v
    }

    pub fn range(&self) -> Option<TimeRange> {
        self.range
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The lanes' focus.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The page was opened: the lanes take the focus when they are next drawn, so their keys
    /// work without a click. (With no lanes there is nothing to focus; focus on an element that
    /// is not drawn would leave the keys with nowhere to go.)
    pub fn focus_when_shown(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    /// A zoom command, from the lanes' own keys or handed over by the shell (the palette).
    pub fn key_zoom(&mut self, z: crate::tools::KeyZoom, cx: &mut Context<Self>) {
        match z {
            crate::tools::KeyZoom::In => self.zoom_key(KEY_ZOOM, cx),
            crate::tools::KeyZoom::Out => self.zoom_key(1.0 / KEY_ZOOM, cx),
            crate::tools::KeyZoom::Fit => self.zoom_to_fit(cx),
        }
    }

    /// Whether the view fits its range or the session (it has not been zoomed or panned).
    pub fn fitted(&self) -> bool {
        self.fitted
    }

    /// The lanes shown, top to bottom.
    pub fn visible_lanes(&self) -> &[LaneKind] {
        &self.visible
    }

    pub fn lane_geometry(&self, kind: LaneKind, cx: &gpui_kit::App) -> Option<Arc<LaneGeometry>> {
        let (_, lane) = self.lanes.iter().find(|(k, _)| *k == kind)?;
        Some(lane.read(cx).geometry().clone())
    }

    pub fn tooltip(&self) -> Option<&str> {
        self.tip.as_deref()
    }

    pub fn hover_hit(&self) -> Option<LaneHit> {
        self.hover.map(|h| h.hit)
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn plot_width(&self) -> f32 {
        (self.size.get().0 - f32::from(layout::LANE_LABEL_WIDTH)).max(1.0)
    }

    /// The shared time range, from the shell's location: the view fits it, or the whole session
    /// without one.
    pub fn set_location(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.fit();
            self.sync(cx);
            cx.notify();
        }
    }

    fn bounds(&self) -> (u64, u64) {
        let span = session_span(&self.snap, None);
        (span.start.0, span.end.0)
    }

    fn fit(&mut self) {
        self.viewport = match self.range {
            Some(r) => Viewport::fit(r.start.0, r.end.0),
            None => {
                let (a, z) = self.bounds();
                Viewport::fit(a, z)
            }
        };
        self.fitted = true;
    }

    fn refresh_visible(&mut self) {
        let snap = &self.snap;
        let nothing = snap.frames.is_empty()
            && snap.logs.is_empty()
            && snap.net.is_empty()
            && snap.timeline.slice_count() == 0;
        self.visible = if nothing {
            Vec::new()
        } else {
            LaneKind::ALL
                .into_iter()
                .filter(|k| k.has_data(snap))
                .collect()
        };
    }

    /// Takes the session's newest snapshot.
    fn take_snapshot(&mut self, cx: &mut Context<Self>) {
        let snap = self.session.read(cx).snapshot().clone();
        if Arc::ptr_eq(&snap, &self.snap) {
            // A status change: only the empty state's copy shows it.
            if self.visible.is_empty() {
                cx.notify();
            }
            return;
        }
        if snap.session != self.snap.session {
            // Ids and times of another session mean nothing here.
            self.fitted = true;
            self.drag = None;
            self.clear_hover();
            self.spans = None;
            self.spans_for = None;
            self.spans_task = None;
            self.click_task = None;
        }
        self.snap = snap;
        self.refresh_visible();
        if self.fitted {
            self.fit();
        }
        self.sync(cx);
        cx.notify();
    }

    /// Lays every shown lane out again, fetches what the request lane needs, and re-resolves the
    /// hover. A lane repaints only if its geometry came out different.
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.fetch_spans(cx);
        let (snap, vp, w) = (self.snap.clone(), self.viewport, self.lane_width);
        let spans = self.spans.clone();
        for (kind, lane) in &self.lanes {
            if self.visible.contains(kind) {
                lane.update(cx, |l, cx| l.set_inputs(&snap, vp, w, spans.as_ref(), cx));
            }
        }
        self.resolve_hover(cx);
    }

    /// Starts a fetch of the requests around the view when the request lane would draw spans
    /// and has none that cover it. At most one runs: a pan that moves on meanwhile is picked up
    /// when it returns (one trailing fetch for the latest view), and until then the last result
    /// is drawn, clipped.
    fn fetch_spans(&mut self, cx: &mut Context<Self>) {
        if !self.visible.contains(&LaneKind::Network)
            || !wants_spans(&self.snap, self.viewport, self.lane_width)
        {
            self.spans = None;
            self.spans_for = None;
            self.spans_task = None;
            return;
        }
        if self.spans_task.is_some() {
            return;
        }
        let (a, z) = self.viewport.range();
        let view = TimeRange::new(SessionTime(a), SessionTime(z));
        let covered = self
            .spans
            .as_ref()
            .is_some_and(|s| s.range.start <= view.start && s.range.end >= view.end);
        let pending = self.spans.as_ref().is_some_and(|s| s.pending);
        if covered && self.spans_for == Some(spans_key(&self.snap, pending)) {
            return;
        }
        let want = span_fetch_range(self.viewport);
        let ns_per_px = self.viewport.span_ns / (self.lane_width.max(1.0) as u64);
        self.spans_jobs += 1;
        let snap = self.snap.clone();
        self.spans_task = Some(cx.spawn(async move |this, cx| {
            let job = snap.clone();
            let spans = cx
                .background_spawn(async move { fetch_spans(&job, want, ns_per_px) })
                .await;
            this.update(cx, |this, cx| {
                this.spans_task = None;
                // Another session: the result means nothing here.
                if this.snap.session == snap.session {
                    this.spans_for = Some(spans_key(&snap, spans.pending));
                    this.spans = Some(spans);
                }
                // Trailing: `sync` fetches again if the view moved on meanwhile.
                this.sync(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The fetched spans, if any.
    pub fn spans(&self) -> Option<&Spans> {
        self.spans.as_ref()
    }

    /// How many span fetches have been started.
    pub fn spans_jobs(&self) -> usize {
        self.spans_jobs
    }

    /// Whether a span fetch is under way.
    pub fn spans_in_flight(&self) -> bool {
        self.spans_task.is_some()
    }

    /// Whether the lanes show what the data holds: no span fetch is pending.
    pub fn is_settled(&self) -> bool {
        !self.spans_in_flight()
    }

    // Zoom and pan.

    /// Applies what the wheel, trackpad or pinch asked. The lanes have no vertical content, so
    /// the vertical wheel pans time as well.
    pub fn zoom_pan(&mut self, action: ZoomPan, cx: &mut Context<Self>) {
        match action {
            ZoomPan::Zoom { x, factor } => self.zoom_about(
                f32::from(x) - f32::from(layout::LANE_LABEL_WIDTH),
                factor,
                cx,
            ),
            ZoomPan::PanX(d) | ZoomPan::PanY(d) => self.pan_px(f32::from(d), cx),
        }
    }

    /// Zooms about `x` pixels from the plot's left edge.
    pub fn zoom_about(&mut self, x: f32, factor: f32, cx: &mut Context<Self>) {
        let before = self.viewport;
        let (w, bounds) = (self.plot_width(), self.bounds());
        self.viewport.zoom_about(x, w, factor, bounds);
        self.moved_view(before, cx);
    }

    /// Pans by `px` pixels; positive shows later time.
    pub fn pan_px(&mut self, px: f32, cx: &mut Context<Self>) {
        let before = self.viewport;
        let (w, bounds) = (self.plot_width(), self.bounds());
        self.viewport.pan_px(px, w, bounds);
        self.moved_view(before, cx);
    }

    fn moved_view(&mut self, before: Viewport, cx: &mut Context<Self>) {
        if self.viewport == before {
            return;
        }
        self.fitted = false;
        self.sync(cx);
        cx.notify();
    }

    fn zoom_key(&mut self, factor: f32, cx: &mut Context<Self>) {
        let half = self.plot_width() / 2.0;
        self.zoom_about(half, factor, cx);
    }

    pub fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
        self.sync(cx);
        cx.notify();
    }

    // Hover and tooltip.

    fn clear_hover(&mut self) {
        self.hover = None;
        self.tip = None;
        self.tip_key = None;
        self.tip_task = None;
    }

    /// The lane under `y` pixels from the page's top (below the ruler), with the y in the lane.
    fn lane_at(&self, y: f32) -> Option<(LaneKind, f32)> {
        let below = y - f32::from(layout::RULER_HEIGHT);
        if below < 0.0 {
            return None;
        }
        let h = f32::from(layout::LANE_HEIGHT);
        let i = (below / h).floor() as usize;
        Some((*self.visible.get(i)?, below - i as f32 * h))
    }

    fn lane_hit(&self, kind: LaneKind, x: f32, y: f32, cx: &gpui_kit::App) -> Option<LaneHit> {
        match hit_at(&*self.lane_geometry(kind, cx)?, x, y)? {
            // Only the cell under the pointer is searched, for its worst frame.
            LaneHit::FrameCell(r) => worst_frame(&self.snap, r).map(LaneHit::Frame),
            LaneHit::FindingCell(r) => worst_frame(&self.snap, r).map(LaneHit::Finding),
            hit => Some(hit),
        }
    }

    /// Looks the hovered position up again, after the data or the view changed: the tooltip
    /// follows what is there now, or goes.
    fn resolve_hover(&mut self, cx: &mut Context<Self>) {
        let Some(h) = self.hover else { return };
        let hit = self
            .visible
            .contains(&h.kind)
            .then(|| self.lane_hit(h.kind, h.x, h.y, cx))
            .flatten();
        match hit {
            Some(hit) => {
                self.hover = Some(Hover { hit, ..h });
                self.refresh_tip(cx);
            }
            None => self.clear_hover(),
        }
        cx.notify();
    }

    /// Sets the tooltip for the hovered hit. A cell's text is counted in the background; the old
    /// text stays until the new one is there, unless it was for another hit.
    fn refresh_tip(&mut self, cx: &mut Context<Self>) {
        let Some(h) = self.hover else { return };
        if let Some(text) = tooltip_text(&self.snap, &h.hit) {
            self.tip = Some(text);
            self.tip_key = None;
            self.tip_task = None;
            return;
        }
        let key = (h.hit, self.snap.generation);
        if self.tip_key == Some(key) {
            return;
        }
        if self.tip_key.is_none_or(|k| k.0 != h.hit) {
            self.tip = None;
        }
        self.tip_key = Some(key);
        let (snap, hit) = (self.snap.clone(), h.hit);
        self.tip_task = Some(cx.spawn(async move |this, cx| {
            let job = snap.clone();
            let text = cx
                .background_spawn(async move { cell_tooltip(&job, &hit) })
                .await;
            this.update(cx, |this, cx| {
                if this.tip_key == Some((hit, snap.generation)) {
                    this.tip = text;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // Input, in pixels from the page's top-left corner.

    pub fn press(&mut self, x: f32, y: f32, click_count: usize, cx: &mut Context<Self>) {
        if x < f32::from(layout::LANE_LABEL_WIDTH) {
            return;
        }
        // A double-click on the ruler clears the range. On a lane it is two clicks: the first
        // has already opened what is under it.
        if click_count >= 2 && y < f32::from(layout::RULER_HEIGHT) {
            self.drag = None;
            self.open_timeline(None, cx);
            return;
        }
        self.drag = Some(Drag {
            start_x: x,
            start_y: y,
            x,
        });
        cx.notify();
    }

    pub fn moved(
        &mut self,
        x: f32,
        y: f32,
        at: Point<Pixels>,
        pressed: bool,
        inside: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(d) = self.drag.as_mut() {
            if pressed {
                d.x = x;
            } else {
                self.drag = None;
            }
            cx.notify();
            return;
        }
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        let target = (inside && x >= label_w)
            .then(|| self.lane_at(y))
            .flatten()
            .and_then(|(kind, ly)| {
                let lx = x - label_w;
                Some((kind, lx, ly, self.lane_hit(kind, lx, ly, cx)?))
            });
        match target {
            Some((kind, x, y, hit)) => {
                let changed = self.hover.is_none_or(|h| h.hit != hit || h.kind != kind);
                self.hover = Some(Hover {
                    kind,
                    x,
                    y,
                    hit,
                    at,
                });
                self.refresh_tip(cx);
                if changed || self.tip.is_some() {
                    cx.notify();
                }
            }
            None => {
                if self.hover.is_some() {
                    self.clear_hover();
                    cx.notify();
                }
            }
        }
    }

    pub fn released(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        cx.notify();
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        if (x - d.start_x).abs() < MIN_DRAG_PX {
            // A click: on a lane it opens what is under it at once; on the ruler it does nothing.
            if let Some((kind, ly)) = self.lane_at(d.start_y) {
                let lx = d.start_x - label_w;
                if let Some(hit) = self.lane_hit(kind, lx, ly, cx) {
                    self.activate(hit, cx);
                }
            }
            return;
        }
        let w = self.plot_width();
        let time = |px: f32| self.viewport.time_at((px - label_w).clamp(0.0, w), w);
        let range = TimeRange::new(SessionTime(time(d.start_x)), SessionTime(time(x)));
        self.open_timeline(Some(range), cx);
    }

    /// Asks the shell to show the Timeline over `range` (or the whole session).
    fn open_timeline(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        cx.emit(OpenLocation(Location {
            page: Page::Tool(Tool::Timeline),
            selection: None,
            range,
        }));
    }

    /// Opens what `hit` is in its tool, keeping the current range. A cell opens the first record
    /// at or after it, found in the background.
    pub(crate) fn activate(&mut self, hit: LaneHit, cx: &mut Context<Self>) {
        if let Some(loc) = location_for(&self.snap, &hit, self.range) {
            cx.emit(OpenLocation(loc));
            return;
        }
        let (snap, range) = (self.snap.clone(), self.range);
        self.click_task = Some(cx.spawn(async move |this, cx| {
            let job = snap.clone();
            let found = cx
                .background_spawn(async move { resolve_cell(&job, &hit) })
                .await;
            this.update(cx, |this, cx| {
                if this.snap.session != snap.session {
                    return;
                }
                let Some((tool, id)) = found else { return };
                let selection = id.map(|id| match tool {
                    Tool::Network => Selection::Request(id),
                    _ => Selection::Log(id),
                });
                cx.emit(OpenLocation(Location {
                    page: Page::Tool(tool),
                    selection,
                    range,
                }));
            })
            .ok();
        }));
    }

    fn render_tooltip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let (h, text) = (self.hover?, self.tip.clone()?);
        let offset = layout::TOOLTIP_OFFSET;
        Some(
            deferred(
                anchored()
                    .position(h.at + point(offset, offset))
                    .snap_to_window_with_margin(space::S8)
                    .child(
                        div()
                            .id("timeline-tooltip")
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

    /// The ruler and the drag and range bands over the lanes, and the input that goes with them.
    fn render_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.tokens();
        let view = cx.entity();
        let (viewport, drag, hovering) = (self.viewport, self.drag, self.hover.is_some());
        let lanes = self.visible.len();
        let range = self.range;
        let size_cell = self.size.clone();
        canvas(
            move |bounds: Bounds<Pixels>, window, cx| {
                let now = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let before = size_cell.replace(now);
                if (before.0 - now.0).abs() >= 1.0 || (before.1 - now.1).abs() >= 1.0 {
                    cx.notify(window.current_view());
                }
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds: Bounds<Pixels>, hitbox, window, cx| {
                let (ox, oy) = (bounds.origin.x, bounds.origin.y);
                let (pw, ph) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let label_w = f32::from(layout::LANE_LABEL_WIDTH);
                let plot_w = (pw - label_w).max(1.0);
                let ticks = ruler_ticks(viewport, plot_w);
                let band = range.map(|r| {
                    (
                        label_w + viewport.x_of(r.start.0, plot_w),
                        label_w + viewport.x_of(r.end.0, plot_w),
                    )
                });
                let quads: Vec<Quad> = overlay_rects(
                    &ticks.iter().map(|(x, _)| *x).collect::<Vec<_>>(),
                    band,
                    drag.map(|d| (d.start_x, d.x)),
                    (pw, ph),
                    lanes,
                )
                .into_iter()
                .map(|(r, kind)| Quad {
                    bounds: Bounds::new(point(ox + px(r.x), oy + px(r.y)), size(px(r.w), px(r.h))),
                    color: match kind {
                        Overlay::Range => t.accent.with_alpha(alpha::ACTIVE_ROW),
                        Overlay::Drag => t.accent.with_alpha(alpha::SELECTION),
                        Overlay::Tick => Hsla::from(t.text_faint),
                        Overlay::Border => Hsla::from(t.border),
                    },
                })
                .collect();
                paint_quads(window, bounds, &quads);
                let line = font::LABEL * font::LINE_HEIGHT_UI;
                let tick_labels: Vec<Label> = ticks
                    .iter()
                    .map(|(x, text)| Label {
                        text: SharedString::from(text.clone()),
                        origin: point(
                            ox + px(label_w + x) + space::S4,
                            oy + (layout::RULER_HEIGHT - line) / 2.0,
                        ),
                        max_width: px(TICK_TARGET_PX) - space::S8,
                        color: Hsla::from(t.text_muted),
                    })
                    .collect();
                paint_labels(
                    window,
                    cx,
                    Bounds::new(
                        point(ox + px(label_w), oy),
                        size(px(plot_w), layout::RULER_HEIGHT),
                    ),
                    &tick_labels,
                );
                window.set_cursor_style(
                    if hovering {
                        CursorStyle::PointingHand
                    } else {
                        CursorStyle::Arrow
                    },
                    &hitbox,
                );

                let (v, hit) = (view.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || e.button != MouseButton::Left
                        || !hit.is_hovered(window)
                    {
                        return;
                    }
                    let handle = v.read(cx).focus.clone();
                    window.focus(&handle, cx);
                    let at = e.position - hit.bounds.origin;
                    v.update(cx, |this, cx| {
                        this.press(f32::from(at.x), f32::from(at.y), e.click_count, cx)
                    });
                });
                let (v, hit) = (view.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let inside = hit.is_hovered(window);
                    let at = e.position - hit.bounds.origin;
                    let pressed = e.pressed_button == Some(MouseButton::Left);
                    v.update(cx, |this, cx| {
                        this.moved(
                            f32::from(at.x),
                            f32::from(at.y),
                            e.position,
                            pressed,
                            inside,
                            cx,
                        )
                    });
                });
                let (v, hit) = (view.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left {
                        return;
                    }
                    let at = e.position - hit.bounds.origin;
                    v.update(cx, |this, cx| this.released(f32::from(at.x), cx));
                });
                let v = view.clone();
                zoom_pan_listeners(window, &hitbox, row::TABLE, move |action, _, cx| {
                    v.update(cx, |this, cx| this.zoom_pan(action, cx));
                });
            },
        )
        .absolute()
        .size_full()
    }
}

/// What fetched spans depend on: the session, every change to a request (one that completes or
/// fails, with no request after it, still changes its span), and while one is pending the latest
/// time, which its span ends at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpansKey {
    session: u64,
    updates: u64,
    pending_until: Option<SessionTime>,
}

fn spans_key(snap: &Snapshot, pending: bool) -> SpansKey {
    SpansKey {
        session: snap.session,
        updates: snap.net.request_updates(),
        pending_until: pending.then(|| snap.net.latest()),
    }
}

impl Render for TimelineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::TimelineView, 1);
        let t = *cx.tokens();
        if self.visible.is_empty() {
            let connected = matches!(
                self.session.read(cx).status(),
                fdt_capture::ConnStatus::Connected { .. }
            );
            let (label, body) = if connected {
                (
                    EMPTY_CONNECTED_TEXT,
                    "Timeline lanes fill in as your app runs.",
                )
            } else {
                (EMPTY_TEXT, "Connect to an app to fill the timeline.")
            };
            return div()
                .id("timeline-view")
                .test_support()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("timeline-empty")
                        .test_support()
                        .aria_label(label)
                        .child(EmptyState::new(
                            "timeline-empty-state",
                            tool_icon(Tool::Timeline),
                            "Nothing recorded yet",
                            body,
                        )),
                )
                .into_any_element();
        }
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        // The page was resized: lay the lanes out for the new width.
        let width = self.plot_width();
        if (width - self.lane_width).abs() >= 1.0 {
            self.lane_width = width;
            self.sync(cx);
        }
        let rows: Vec<AnyElement> =
            self.visible
                .iter()
                .filter_map(|kind| {
                    let (_, lane) = self.lanes.iter().find(|(k, _)| k == kind)?;
                    Some(
                        div()
                            .flex()
                            .flex_none()
                            .w_full()
                            .h(layout::LANE_HEIGHT)
                            .child(
                                div()
                                    .id(SharedString::from(format!("lane-label-{}", kind.slug())))
                                    .test_support()
                                    .aria_label(kind.label())
                                    .flex_none()
                                    .w(layout::LANE_LABEL_WIDTH)
                                    .h_full()
                                    .px(space::S8)
                                    .flex()
                                    .items_center()
                                    .text_size(font::LABEL)
                                    .text_color(Hsla::from(name_colour(&t)))
                                    .child(kind.label()),
                            )
                            .child(div().flex_1().min_w_0().h_full().child(lane.clone().cached(
                                StyleRefinement::default().w_full().h(layout::LANE_HEIGHT),
                            )))
                            .into_any_element(),
                    )
                })
                .collect();
        div()
            .id("timeline-view")
            .test_support()
            .key_context(LANES_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_key(KEY_ZOOM, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_key(1.0 / KEY_ZOOM, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.zoom_to_fit(cx)))
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .size_full()
                            .child(div().flex_none().w_full().h(layout::RULER_HEIGHT))
                            .children(rows),
                    )
                    .child(self.render_overlay(cx))
                    .children(self.render_tooltip(cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::lanes::span_fetch_range;
    use super::*;
    use crate::nav::{Page, Selection, Tool};
    use crate::test_support::{
        FakeHandle, exists, fake_capture, frame_at, log_at, request, writer,
    };
    use fdt_store::logs::{LogLevel, LogSource};
    use fdt_store::timeline::{Slice, SliceCat, TrackKind};
    use fdt_ui::viewport::{MIN_SPAN_NS, pinch_zoom_factor, scroll_zoom_factor};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{ScrollDelta, VisualTestContext, point, px};
    use std::cell::RefCell;

    const MS: u64 = 1_000_000;

    fn label_w() -> f32 {
        f32::from(layout::LANE_LABEL_WIDTH)
    }

    fn ruler_h() -> f32 {
        f32::from(layout::RULER_HEIGHT)
    }

    fn lane_h() -> f32 {
        f32::from(layout::LANE_HEIGHT)
    }

    /// Fills `w` with a three second session: a frame every 100 ms (the sixth janky, the ninth
    /// slow), overlapping requests, a log line every 100 ms, two route changes and two pauses.
    fn fill(w: &mut fdt_store::SessionWriter, run: fdt_store::time::RunId) {
        for i in 0..30u64 {
            let (b, r) = match i {
                5 => (80, 6),
                8 => (25, 6),
                _ => (4, 4),
            };
            w.push_frame(frame_at(run, i + 1, i * 100, b, r));
            let level = if i % 10 == 0 {
                LogLevel::Error
            } else {
                LogLevel::Info
            };
            let mut l = log_at(run, i * 100 + 7, level);
            if i == 5 || i == 15 {
                l.source = LogSource::Navigation;
                l.text = format!("/route/{i}");
            }
            w.push_log(l);
            let status = if i % 7 == 0 { 500 } else { 200 };
            request(
                w,
                run,
                "GET",
                &format!("http://h/p{i}"),
                Some(status),
                i * 100 + 3,
                Some(i * 100 + 250),
            );
        }
        let name = w.intern_name("GC");
        let id = w.timeline_track(run, TrackKind::Thread, 1, "gc");
        for at in [1_000, 2_000] {
            let s = Slice {
                start: fdt_store::time::SessionTime(at * MS),
                dur_ns: 6 * MS,
                name,
                cat: SliceCat::Gc,
            };
            w.push_slice(id, 0, s, false);
        }
    }

    fn demo() -> Arc<Snapshot> {
        let (mut w, run) = writer();
        fill(&mut w, run);
        w.publish()
    }

    type Opened<'a> = (Entity<TimelineView>, &'a mut VisualTestContext, FakeHandle);

    /// A Timeline page over a session fed by a fake capture, showing `snap`.
    fn open(cx: &mut gpui_kit::TestAppContext, snap: Arc<Snapshot>) -> Opened<'_> {
        crate::test_support::init(cx);
        let (fake, handle) = fake_capture();
        let (view, vcx) = cx.add_window_view(|window, cx| {
            let session = cx.new(|cx| {
                let mut m = SessionModel::new(cx);
                m.attach(Box::new(fake), cx);
                m
            });
            TimelineView::new(session, window, cx)
        });
        handle.snaps.send(snap).unwrap();
        vcx.run_until_parked();
        vcx.update(|window, cx| window.render_frame(cx));
        vcx.run_until_parked();
        (view, vcx, handle)
    }

    fn log(
        cx: &mut VisualTestContext,
        v: &Entity<TimelineView>,
    ) -> (Rc<RefCell<Vec<Location>>>, Subscription) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let sub = cx.update(|_, cx| {
            cx.subscribe(v, move |_, e: &OpenLocation, _| {
                l.borrow_mut().push(e.0.clone())
            })
        });
        (log, sub)
    }

    /// The window position (from the page's top-left) of the first hit of `lane` that `want`
    /// accepts, at the middle of its rectangle.
    fn point_of(
        v: &Entity<TimelineView>,
        cx: &mut VisualTestContext,
        lane: LaneKind,
        want: impl Fn(&LaneHit) -> bool,
    ) -> (f32, f32, LaneHit) {
        v.read_with(cx, |v, cx| {
            let idx = v
                .visible_lanes()
                .iter()
                .position(|k| *k == lane)
                .expect("lane is shown");
            let geo = v.lane_geometry(lane, cx).expect("lane exists");
            let (r, hit) = geo
                .hits
                .iter()
                .find(|(_, h)| want(h))
                .unwrap_or_else(|| panic!("no hit in {lane:?}: {:?}", geo.hits));
            (
                label_w() + r.x + r.w / 2.0,
                ruler_h() + idx as f32 * lane_h() + r.y + r.h / 2.0,
                *hit,
            )
        })
    }

    fn range_0_to_4s() -> TimeRange {
        TimeRange::new(SessionTime(0), SessionTime(4_000 * MS))
    }

    fn click(v: &Entity<TimelineView>, cx: &mut VisualTestContext, x: f32, y: f32) {
        v.update(cx, |v, cx| {
            v.press(x, y, 1, cx);
            v.released(x, cx);
        });
        cx.run_until_parked();
    }

    /// A double-click on the ruler clears the range (ruling R22); its first click sets nothing.
    #[gpui_kit::test]
    fn double_click_on_the_ruler_clears_the_range(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        v.update(cx, |v, cx| v.set_location(Some(range_0_to_4s()), cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        let (events, _sub) = log(cx, &v);
        let (x, y) = (label_w() + 120.0, ruler_h() / 2.0);
        v.update(cx, |v, cx| {
            v.press(x, y, 1, cx);
            v.released(x, cx);
            v.press(x, y, 2, cx);
            v.released(x, cx);
        });
        assert_eq!(
            events.borrow().as_slice(),
            [Location {
                page: Page::Tool(Tool::Timeline),
                selection: None,
                range: None,
            }]
        );
    }

    /// A click on a lane opens what is under it at once, with no wait for a second click
    /// (ruling R22); a double-click on a lane is two clicks, and does not clear the range.
    #[gpui_kit::test]
    fn lane_click_opens_its_target_immediately(cx: &mut gpui_kit::TestAppContext) {
        let snap = demo();
        let (v, cx, _h) = open(cx, snap.clone());
        let range = range_0_to_4s();
        v.update(cx, |v, cx| v.set_location(Some(range), cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        let (events, _sub) = log(cx, &v);
        let (x, y, _) = point_of(&v, cx, LaneKind::Frames, |h| *h == LaneHit::Frame(5));
        let opened = Location {
            page: Page::Tool(Tool::Performance),
            selection: Some(Selection::Frame(snap.frames.get(5).unwrap().id)),
            range: Some(range),
        };
        // No clock advance and no parking: the location is emitted on mouse-up.
        v.update(cx, |v, cx| {
            v.press(x, y, 1, cx);
            v.released(x, cx);
        });
        assert_eq!(*events.borrow(), vec![opened.clone()]);
        // The second press of a double-click on a lane is another click.
        v.update(cx, |v, cx| {
            v.press(x, y, 2, cx);
            v.released(x, cx);
        });
        assert_eq!(*events.borrow(), vec![opened.clone(), opened]);
    }

    #[gpui_kit::test]
    fn empty_lanes_hide(cx: &mut gpui_kit::TestAppContext) {
        // Only frames: the Frames lane alone.
        let frames_only = crate::test_support::snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 10, 4, 4));
        });
        let (v, cx, _h) = open(cx, frames_only);
        assert_eq!(
            v.read_with(cx, |v, _| v.visible_lanes().to_vec()),
            vec![LaneKind::Frames]
        );
        assert!(exists(cx, "lane-label-frames"));
        assert!(!exists(cx, "lane-label-network"));
        assert!(!exists(cx, "timeline-empty"));
    }

    #[gpui_kit::test]
    fn all_lanes_show_with_data(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        assert_eq!(
            v.read_with(cx, |v, _| v.visible_lanes().to_vec()),
            LaneKind::ALL.to_vec()
        );
        for kind in LaneKind::ALL {
            assert!(exists(
                cx,
                match kind {
                    LaneKind::Frames => "lane-label-frames",
                    LaneKind::Network => "lane-label-network",
                    LaneKind::Logs => "lane-label-logs",
                    LaneKind::Navigation => "lane-label-navigation",
                    LaneKind::Gc => "lane-label-gc",
                    LaneKind::Findings => "lane-label-findings",
                }
            ));
        }
    }

    #[gpui_kit::test]
    fn nothing_recorded_shows_the_empty_state(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (v, cx, _h) = open(cx, Arc::new(Snapshot::empty()));
        assert!(v.read_with(cx, |v, _| v.visible_lanes().is_empty()));
        assert!(exists(cx, "timeline-view"));
        assert!(exists(cx, "timeline-empty"));
        let label = cx.update(|window, _| {
            window
                .try_find("timeline-empty")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("Nothing recorded yet. Connect to an app to fill the timeline.")
        );
    }

    /// Connected, with nothing recorded yet: the copy does not tell the user to connect.
    #[gpui_kit::test]
    fn nothing_recorded_while_connected_says_the_lanes_fill_in(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (_v, cx, h) = open(cx, Arc::new(Snapshot::empty()));
        h.status
            .send(crate::test_support::connected("demo", true))
            .unwrap();
        cx.run_until_parked();
        let label = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find("timeline-empty")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("Nothing recorded yet. Timeline lanes fill in as your app runs.")
        );
    }

    #[gpui_kit::test]
    fn clicks_map_to_locations_with_range_kept(cx: &mut gpui_kit::TestAppContext) {
        let snap = demo();
        let (v, cx, _h) = open(cx, snap.clone());
        let range = range_0_to_4s();
        v.update(cx, |v, cx| v.set_location(Some(range), cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        let (events, _sub) = log(cx, &v);
        let want = |page, selection| Location {
            page,
            selection,
            range: Some(range),
        };
        let last = |events: &Rc<RefCell<Vec<Location>>>| {
            events
                .borrow()
                .last()
                .cloned()
                .expect("a location was opened")
        };

        // A frame, and a finding: the Performance page with that frame selected.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Frames, |h| *h == LaneHit::Frame(5));
        assert_eq!(hit, LaneHit::Frame(5));
        click(&v, cx, x, y);
        let id = snap.frames.get(5).unwrap().id;
        assert_eq!(
            last(&events),
            want(Page::Tool(Tool::Performance), Some(Selection::Frame(id)))
        );
        let (x, y, hit) = point_of(&v, cx, LaneKind::Findings, |_| true);
        assert!(matches!(hit, LaneHit::FindingCell(_)), "{hit:?}");
        click(&v, cx, x, y);
        assert_eq!(
            last(&events),
            want(Page::Tool(Tool::Performance), Some(Selection::Frame(id)))
        );

        // A request span: the Network page with that request selected.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Network, |h| {
            matches!(h, LaneHit::Request(_))
        });
        let LaneHit::Request(seq) = hit else {
            unreachable!()
        };
        click(&v, cx, x, y);
        let id = snap.net.request(seq as usize).id;
        assert_eq!(
            last(&events),
            want(Page::Tool(Tool::Network), Some(Selection::Request(id)))
        );

        // A navigation marker: the Logs page at that record.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Navigation, |_| true);
        let LaneHit::Navigation(seq) = hit else {
            unreachable!()
        };
        click(&v, cx, x, y);
        let id = snap.logs.row(seq as usize).id;
        assert_eq!(
            last(&events),
            want(Page::Tool(Tool::Logs), Some(Selection::Log(id)))
        );

        // A log cell: the Logs page at the first record at or after the cell, found in the
        // background.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Logs, |_| true);
        let LaneHit::LogCell(cell) = hit else {
            unreachable!()
        };
        let before = events.borrow().len();
        click(&v, cx, x, y);
        assert_eq!(events.borrow().len(), before + 1);
        let loc = last(&events);
        let first = (0..snap.logs.len())
            .find(|&i| cell.contains(snap.logs.row(i).time))
            .unwrap();
        assert_eq!(
            loc,
            want(
                Page::Tool(Tool::Logs),
                Some(Selection::Log(snap.logs.row(first).id))
            )
        );

        // A cell of requests, which the lane shows when spans are not there.
        let cell = TimeRange::new(SessionTime(900 * MS), SessionTime(1_100 * MS));
        v.update(cx, |v, cx| v.activate(LaneHit::RequestCell(cell), cx));
        cx.run_until_parked();
        let loc = last(&events);
        assert_eq!(loc.page, Page::Tool(Tool::Network));
        assert_eq!(loc.range, Some(range));
        assert!(matches!(loc.selection, Some(Selection::Request(_))));

        // A pause: the Performance page, nothing selected.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Gc, |_| true);
        assert!(matches!(hit, LaneHit::Gc(_)));
        click(&v, cx, x, y);
        assert_eq!(last(&events), want(Page::Tool(Tool::Performance), None));
    }

    #[gpui_kit::test]
    fn drag_sets_range_and_double_click_clears(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        let (events, _sub) = log(cx, &v);
        let (w, vp) = v.read_with(cx, |v, _| (v.plot_width(), v.viewport()));
        let at = point(px(0.), px(0.));
        v.update(cx, |v, cx| {
            v.press(label_w() + 100.0, 5.0, 1, cx);
            v.moved(label_w() + 300.0, 5.0, at, true, true, cx);
            v.released(label_w() + 300.0, cx);
        });
        let want = TimeRange::new(
            SessionTime(vp.time_at(100.0, w)),
            SessionTime(vp.time_at(300.0, w)),
        );
        let timeline = |range| Location {
            page: Page::Tool(Tool::Timeline),
            selection: None,
            range,
        };
        assert_eq!(*events.borrow(), vec![timeline(Some(want))]);
        events.borrow_mut().clear();

        // A drag that starts on a lane and ends left of the plot clamps to the start.
        let y = ruler_h() + 3.0;
        v.update(cx, |v, cx| {
            v.press(label_w() + 200.0, y, 1, cx);
            v.released(-80.0, cx);
        });
        let r = events
            .borrow()
            .last()
            .and_then(|l| l.range)
            .expect("a range");
        assert_eq!(
            (r.start.0, r.end.0),
            (vp.time_at(0.0, w), vp.time_at(200.0, w))
        );
        events.borrow_mut().clear();

        // A click on the ruler without moving sets nothing.
        v.update(cx, |v, cx| {
            v.press(label_w() + 50.0, 5.0, 1, cx);
            v.released(label_w() + 51.0, cx);
        });
        assert!(events.borrow().is_empty());
        // The drag band shows while dragging, and goes after the release.
        v.update(cx, |v, cx| {
            v.press(label_w() + 50.0, 5.0, 1, cx);
            v.moved(label_w() + 90.0, 5.0, at, true, true, cx);
            assert!(v.dragging());
            v.moved(label_w() + 95.0, 5.0, at, false, true, cx);
            assert!(!v.dragging(), "a move with no button down ends the drag");
        });
        // A double-click clears the range.
        v.update(cx, |v, cx| v.press(label_w() + 50.0, 5.0, 2, cx));
        assert_eq!(*events.borrow(), vec![timeline(None)]);
        // The label column does not drag.
        events.borrow_mut().clear();
        v.update(cx, |v, cx| {
            v.press(10.0, 5.0, 1, cx);
            assert!(!v.dragging());
            v.press(10.0, 5.0, 2, cx);
        });
        assert!(events.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn the_shared_range_refits_the_view(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        let whole = v.read_with(cx, |v, _| v.viewport());
        let r = TimeRange::new(SessionTime(500 * MS), SessionTime(900 * MS));
        v.update(cx, |v, cx| v.set_location(Some(r), cx));
        assert_eq!(
            v.read_with(cx, |v, _| v.viewport()),
            Viewport::fit(r.start.0, r.end.0)
        );
        v.update(cx, |v, cx| v.set_location(None, cx));
        assert_eq!(v.read_with(cx, |v, _| v.viewport()), whole);
    }

    #[gpui_kit::test]
    fn extreme_zoom_is_clamped_and_finite(cx: &mut gpui_kit::TestAppContext) {
        let snap = demo();
        let (v, cx, _h) = open(cx, snap.clone());
        let bound = {
            let s = crate::shell::strip::session_span(&snap, None);
            s.end.0 - s.start.0
        };
        let w = v.read_with(cx, |v, _| v.plot_width());
        let zoom = |f: f32| ZoomPan::Zoom {
            x: px(label_w() + w / 2.0),
            factor: f,
        };
        let check = |v: &Entity<TimelineView>, cx: &mut VisualTestContext| {
            let vp = v.read_with(cx, |v, _| v.viewport());
            assert!(vp.span_ns >= MIN_SPAN_NS, "{vp:?}");
            assert!(vp.span_ns as f64 <= bound as f64 * 1.1 + 1.0, "{vp:?}");
            v.read_with(cx, |v, cx| {
                for kind in v.visible_lanes() {
                    let g = v.lane_geometry(*kind, cx).unwrap();
                    for q in &g.quads {
                        let r = q.rect;
                        assert!(
                            r.x.is_finite() && r.w.is_finite() && r.h.is_finite(),
                            "{kind:?} {r:?}"
                        );
                    }
                }
            });
        };
        // Pinches at the limits (+5 is +500 %, -5 is far below nothing).
        for _ in 0..40 {
            v.update(cx, |v, cx| v.zoom_pan(zoom(pinch_zoom_factor(5.0)), cx));
            check(&v, cx);
        }
        assert_eq!(
            v.read_with(cx, |v, _| v.viewport().span_ns),
            MIN_SPAN_NS,
            "past 1 us it stops"
        );
        for _ in 0..80 {
            v.update(cx, |v, cx| v.zoom_pan(zoom(pinch_zoom_factor(-5.0)), cx));
            check(&v, cx);
        }
        let span = v.read_with(cx, |v, _| v.viewport().span_ns);
        assert!(
            span as f64 >= bound as f64 * 1.05,
            "past the session it stops near {bound}: {span}"
        );
        // A wheel mouse reports lines; a huge one still lands in range.
        for lines in [3.0_f32, -3.0, 1.0e6, -1.0e6, f32::INFINITY, f32::NAN] {
            let dy = ScrollDelta::Lines(point(0.0, lines)).pixel_delta(fdt_ui::tokens::row::TABLE);
            v.update(cx, |v, cx| {
                v.zoom_pan(zoom(scroll_zoom_factor(f32::from(dy.y))), cx)
            });
            check(&v, cx);
        }
        // Panning far either way keeps part of the data in view.
        for d in [1.0e9_f32, -1.0e9, f32::NAN, f32::INFINITY] {
            v.update(cx, |v, cx| {
                v.zoom_pan(ZoomPan::PanX(px(d)), cx);
                v.zoom_pan(ZoomPan::PanY(px(d)), cx);
            });
            check(&v, cx);
        }
        v.update(cx, |v, cx| v.zoom_pan(zoom(f32::NAN), cx));
        check(&v, cx);
    }

    #[gpui_kit::test]
    fn pan_follows_the_scroll_direction(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        v.update(cx, |v, cx| {
            v.zoom_pan(
                ZoomPan::Zoom {
                    x: px(label_w() + 100.0),
                    factor: 8.0,
                },
                cx,
            );
        });
        let a = v.read_with(cx, |v, _| v.viewport().start_ns);
        v.update(cx, |v, cx| v.zoom_pan(ZoomPan::PanX(px(40.0)), cx));
        let b = v.read_with(cx, |v, _| v.viewport().start_ns);
        assert!(b > a, "positive pan shows later time");
        v.update(cx, |v, cx| v.zoom_pan(ZoomPan::PanY(px(-40.0)), cx));
        let c = v.read_with(cx, |v, _| v.viewport().start_ns);
        assert!(c < b, "the vertical wheel pans time too");
    }

    #[gpui_kit::test]
    fn growing_session_keeps_the_viewport_when_zoomed_in(cx: &mut gpui_kit::TestAppContext) {
        let (mut w, run) = writer();
        fill(&mut w, run);
        let (v, cx, h) = open(cx, w.publish());
        let fitted = v.read_with(cx, |v, _| v.viewport());
        assert!(v.read_with(cx, |v, _| v.fitted()));
        // Zoom in on the middle, then more data arrives: the view stays put.
        v.update(cx, |v, cx| {
            v.zoom_pan(
                ZoomPan::Zoom {
                    x: px(label_w() + 200.0),
                    factor: 4.0,
                },
                cx,
            );
        });
        let zoomed = v.read_with(cx, |v, _| v.viewport());
        assert_ne!(zoomed, fitted);
        assert!(!v.read_with(cx, |v, _| v.fitted()));
        for i in 0..50u64 {
            w.push_frame(frame_at(run, 100 + i, 20_000 + i * 100, 4, 4));
        }
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            v.read_with(cx, |v, _| v.viewport()),
            zoomed,
            "zoomed in: the view stays"
        );
        // Zoom to fit follows the session again, and it grows.
        v.update(cx, |v, cx| v.zoom_to_fit(cx));
        let wide = v.read_with(cx, |v, _| v.viewport());
        assert!(v.read_with(cx, |v, _| v.fitted()));
        for i in 0..200u64 {
            w.push_frame(frame_at(run, 200 + i, 25_000 + i * 100, 4, 4));
        }
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        let grown = v.read_with(cx, |v, _| v.viewport());
        assert!(grown.span_ns > wide.span_ns, "{grown:?} vs {wide:?}");
    }

    #[gpui_kit::test]
    fn a_new_session_resets_the_view(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, h) = open(cx, demo());
        let at = point(px(10.), px(10.));
        v.update(cx, |v, cx| {
            v.zoom_pan(
                ZoomPan::Zoom {
                    x: px(label_w() + 200.0),
                    factor: 4.0,
                },
                cx,
            );
        });
        let (x, y, _) = point_of(&v, cx, LaneKind::Frames, |_| true);
        v.update(cx, |v, cx| v.moved(x, y, at, false, true, cx));
        assert!(v.read_with(cx, |v, _| v.tooltip().is_some()));
        // Another session (another writer): the old view state means nothing.
        h.snaps.send(demo()).unwrap();
        cx.run_until_parked();
        v.read_with(cx, |v, _| {
            assert!(v.fitted());
            assert!(v.tooltip().is_none());
            assert!(!v.dragging());
        });
    }

    #[gpui_kit::test]
    fn tooltips_name_what_is_under_the_pointer(cx: &mut gpui_kit::TestAppContext) {
        let snap = demo();
        let (v, cx, _h) = open(cx, snap.clone());
        v.update(cx, |v, cx| v.set_location(Some(range_0_to_4s()), cx));
        cx.run_until_parked();
        let at = point(px(10.), px(10.));
        let hover = |v: &Entity<TimelineView>,
                     cx: &mut VisualTestContext,
                     kind,
                     want: &dyn Fn(&LaneHit) -> bool| {
            let (x, y, hit) = point_of(v, cx, kind, want);
            v.update(cx, |v, cx| v.moved(x, y, at, false, true, cx));
            cx.run_until_parked();
            (v.read_with(cx, |v, _| v.tooltip().map(str::to_string)), hit)
        };
        let (tip, _) = hover(&v, cx, LaneKind::Frames, &|h| *h == LaneHit::Frame(5));
        assert_eq!(tip.as_deref(), Some("Frame #6 \u{b7} 0:00 \u{b7} Janky"));
        let (tip, _) = hover(&v, cx, LaneKind::Findings, &|_| true);
        assert_eq!(tip.as_deref(), Some("Janky frame #6"));
        let (tip, hit) = hover(&v, cx, LaneKind::Network, &|h| {
            matches!(h, LaneHit::Request(_))
        });
        let LaneHit::Request(seq) = hit else {
            unreachable!()
        };
        let r = snap.net.request(seq as usize);
        assert_eq!(
            tip.unwrap(),
            format!(
                "GET {} \u{b7} {} \u{b7} {} ms",
                r.path,
                r.status.unwrap(),
                (r.duration_ns().unwrap() / MS)
            )
        );
        let (tip, _) = hover(&v, cx, LaneKind::Navigation, &|_| true);
        assert!(tip.unwrap().starts_with("/route/"));
        let (tip, _) = hover(&v, cx, LaneKind::Gc, &|_| true);
        assert_eq!(tip.as_deref(), Some("Garbage collection \u{b7} 6 ms"));
        // A cell of log lines is counted in the background.
        let (x, y, hit) = point_of(&v, cx, LaneKind::Logs, |_| true);
        let LaneHit::LogCell(cell) = hit else {
            unreachable!()
        };
        v.update(cx, |v, cx| v.moved(x, y, at, false, true, cx));
        cx.run_until_parked();
        let counts = snap.logs.level_counts(Some(cell));
        let n = counts.iter().sum::<u64>();
        let noun =
            |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        assert_eq!(
            v.read_with(cx, |v, _| v.tooltip().map(str::to_string)),
            Some(format!(
                "{}, {}",
                noun(n, "line", "lines"),
                noun(counts[3], "error", "errors")
            ))
        );
        // Leaving the lanes clears it.
        v.update(cx, |v, cx| v.moved(x, y, at, false, false, cx));
        assert!(v.read_with(cx, |v, _| v.tooltip().is_none()));
    }

    #[gpui_kit::test]
    fn tooltip_follows_new_data(cx: &mut gpui_kit::TestAppContext) {
        let (mut w, run) = writer();
        fill(&mut w, run);
        let (v, cx, h) = open(cx, w.publish());
        v.update(cx, |v, cx| v.set_location(Some(range_0_to_4s()), cx));
        cx.run_until_parked();
        let at = point(px(10.), px(10.));
        let (x, y, hit) = point_of(&v, cx, LaneKind::Logs, |_| true);
        let LaneHit::LogCell(cell) = hit else {
            unreachable!()
        };
        v.update(cx, |v, cx| v.moved(x, y, at, false, true, cx));
        cx.run_until_parked();
        let before = v
            .read_with(cx, |v, _| v.tooltip().map(str::to_string))
            .unwrap();
        // More lines land in the hovered cell: the text re-resolves, it is not left stale.
        for _ in 0..5 {
            w.push_log(log_at(run, cell.start.0 / MS + 8, LogLevel::Error));
        }
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        let after = v
            .read_with(cx, |v, _| v.tooltip().map(str::to_string))
            .unwrap();
        assert_ne!(before, after, "{before} / {after}");
        assert!(after.contains("lines"));
    }

    #[gpui_kit::test]
    fn hover_that_loses_its_target_clears(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        let at = point(px(10.), px(10.));
        let (x, y, _) = point_of(&v, cx, LaneKind::Frames, |_| true);
        v.update(cx, |v, cx| v.moved(x, y, at, false, true, cx));
        assert!(v.read_with(cx, |v, _| v.tooltip().is_some()));
        // Zooming moves everything: what is under the pointer is re-resolved, so a tooltip never
        // describes what is no longer there.
        v.update(cx, |v, cx| {
            v.zoom_pan(
                ZoomPan::Zoom {
                    x: px(x),
                    factor: 1000.0,
                },
                cx,
            )
        });
        let tip = v.read_with(cx, |v, _| v.tooltip().map(str::to_string));
        let hit = v.read_with(cx, |v, _| v.hover_hit());
        assert_eq!(tip.is_some(), hit.is_some());
        v.update(cx, |v, cx| v.zoom_pan(ZoomPan::PanX(px(1.0e6)), cx));
        let tip = v.read_with(cx, |v, _| v.tooltip().map(str::to_string));
        let hit = v.read_with(cx, |v, _| v.hover_hit());
        assert_eq!(tip.is_some(), hit.is_some());
    }

    #[gpui_kit::test]
    fn hovering_does_not_repaint_the_lanes(cx: &mut gpui_kit::TestAppContext) {
        use crate::perf::{Counter, count};
        let (v, cx, _h) = open(cx, demo());
        let at = |dx: f32| point(px(10. + dx), px(10.));
        let (x, y, _) = point_of(&v, cx, LaneKind::Frames, |_| true);
        for _ in 0..3 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        let before = count(Counter::Lanes);
        for i in 0..10 {
            v.update(cx, |v, cx| v.moved(x, y, at(i as f32), false, true, cx));
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert_eq!(
            count(Counter::Lanes),
            before,
            "the tooltip lives in the page"
        );
        v.update(cx, |v, cx| {
            v.zoom_pan(
                ZoomPan::Zoom {
                    x: px(x),
                    factor: 2.0,
                },
                cx,
            )
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(count(Counter::Lanes) > before, "zooming does");
    }

    #[test]
    fn overlays_stay_in_their_budget() {
        let vp = Viewport::fit(0, 50_000 * MS);
        let ticks: Vec<f32> = crate::tools::performance::trace_tab::ruler_ticks(vp, 20_000.0)
            .iter()
            .map(|(x, _)| *x)
            .collect();
        let over = overlay_rects(
            &ticks,
            Some((10.0, 5_000.0)),
            Some((300.0, 20.0)),
            (20_112.0, 400.0),
            6,
        );
        assert!(over.len() <= OVERLAY_QUADS, "{}", over.len());
        assert!(over.iter().any(|(_, k)| *k == Overlay::Drag));
        assert!(over.iter().any(|(_, k)| *k == Overlay::Range));
        // The two borders of the ruler and the label column, and one under each lane.
        assert_eq!(
            over.iter().filter(|(_, k)| *k == Overlay::Border).count(),
            2 + 6
        );
        // Even with every lane shown, nothing is cut: a lane layer can use its whole cap.
        const { assert!(OVERLAY_QUADS >= 200 + 6 + 2 + 2) };
    }

    /// A page fitted to 2.0-2.5 s (so the fetch range is 1.5-3.0 s), then fed `snap`, with its
    /// first span fetch done.
    fn open_zoomed(cx: &mut gpui_kit::TestAppContext, snap: Arc<Snapshot>) -> Opened<'_> {
        let (v, vcx, h) = open(cx, Arc::new(Snapshot::empty()));
        let r = TimeRange::new(SessionTime(2_000 * MS), SessionTime(2_500 * MS));
        v.update(vcx, |v, cx| v.set_location(Some(r), cx));
        h.snaps.send(snap).unwrap();
        vcx.run_until_parked();
        (v, vcx, h)
    }

    fn network_hits_are_spans(v: &Entity<TimelineView>, cx: &mut VisualTestContext) -> bool {
        v.read_with(cx, |v, cx| {
            let g = v.lane_geometry(LaneKind::Network, cx).unwrap();
            g.hits.iter().all(|(_, h)| matches!(h, LaneHit::Request(_)))
        })
    }

    #[gpui_kit::test]
    fn spans_fetch_in_background_and_stay_during_pan(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open_zoomed(cx, demo());
        let (jobs, spans) = v.read_with(cx, |v, _| (v.spans_jobs(), v.spans().cloned()));
        assert_eq!(jobs, 1);
        let spans = spans.expect("fetched in the background");
        assert_eq!(
            spans.range,
            span_fetch_range(v.read_with(cx, |v, _| v.viewport()))
        );
        assert!(
            spans
                .items
                .iter()
                .all(|s| (s.row as usize) < super::super::lanes::SPAN_ROWS)
        );
        assert!(network_hits_are_spans(&v, cx));
        // A pan out of the fetched range starts one fetch, and the last result is still drawn
        // (clipped) while it runs.
        let w = v.read_with(cx, |v, _| v.plot_width());
        v.update(cx, |v, cx| v.pan_px(3.0 * w, cx));
        assert!(v.read_with(cx, |v, _| v.spans_in_flight()));
        assert!(
            v.read_with(cx, |v, _| v.spans().is_some()),
            "kept while fetching"
        );
        assert!(
            network_hits_are_spans(&v, cx),
            "no flip to cells during the fetch"
        );
        cx.run_until_parked();
        v.read_with(cx, |v, _| {
            assert!(!v.spans_in_flight());
            assert_eq!(v.spans_jobs(), 2);
            let s = v.spans().unwrap();
            let (a, z) = v.viewport().range();
            assert!(
                s.range.start.0 <= a && s.range.end.0 >= z,
                "covers the new view"
            );
        });
    }

    #[gpui_kit::test]
    fn no_refetch_inside_margin(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open_zoomed(cx, demo());
        let w = v.read_with(cx, |v, _| v.plot_width());
        for dx in [0.2, 0.4, -0.3, 0.1, 0.5] {
            v.update(cx, |v, cx| v.pan_px(dx * w, cx));
            assert!(!v.read_with(cx, |v, _| v.spans_in_flight()));
        }
        // Zooming within the fetched range does not fetch either.
        v.update(cx, |v, cx| v.zoom_about(w / 2.0, 1.5, cx));
        cx.run_until_parked();
        assert_eq!(v.read_with(cx, |v, _| v.spans_jobs()), 1);
    }

    #[gpui_kit::test]
    fn span_jobs_coalesce_to_one_in_flight(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open_zoomed(cx, demo());
        let w = v.read_with(cx, |v, _| v.plot_width());
        // A continuous pan: many events before any fetch returns.
        for _ in 0..30 {
            v.update(cx, |v, cx| v.pan_px(0.5 * w, cx));
            assert!(
                v.read_with(cx, |v, _| v.spans_in_flight())
                    || v.read_with(cx, |v, _| v.spans_jobs()) == 1
            );
        }
        let started = v.read_with(cx, |v, _| v.spans_jobs());
        assert_eq!(
            started, 2,
            "one fetch for the first uncovered view, none per event"
        );
        cx.run_until_parked();
        v.read_with(cx, |v, _| {
            // One trailing fetch for where the pan ended, then it is covered and quiet.
            assert!(v.spans_jobs() <= 3, "{}", v.spans_jobs());
            assert!(!v.spans_in_flight());
            let s = v.spans().unwrap();
            let (a, z) = v.viewport().range();
            assert!(s.range.start.0 <= a && s.range.end.0 >= z);
        });
    }

    /// A request that completes or fails with no new request after it: the span is fetched
    /// again and drawn as it is now.
    #[gpui_kit::test]
    fn a_request_that_ends_changes_its_span(cx: &mut gpui_kit::TestAppContext) {
        let (mut w, run) = writer();
        w.push_frame(frame_at(run, 1, 10, 4, 4));
        request(&mut w, run, "GET", "http://h/slow", None, 100, None);
        request(&mut w, run, "GET", "http://h/other", None, 120, None);
        w.push_log(log_at(run, 1_000, LogLevel::Info));
        let (v, cx, h) = open(cx, w.publish());
        let span = |v: &Entity<TimelineView>, cx: &mut VisualTestContext, seq: u32| {
            v.read_with(cx, |v, _| {
                v.spans()
                    .and_then(|s| s.items.iter().find(|p| p.seq == seq).copied())
                    .map(|p| (p.end.0 / MS, p.error))
            })
        };
        assert_eq!(
            span(&v, cx, 0),
            Some((1_000, false)),
            "pending: to the latest time"
        );
        // The first fails at 400 ms and the second completes at 300 ms; more log lines arrive.
        request(
            &mut w,
            run,
            "GET",
            "http://h/slow",
            Some(500),
            100,
            Some(400),
        );
        request(
            &mut w,
            run,
            "GET",
            "http://h/other",
            Some(200),
            120,
            Some(300),
        );
        for i in 0..5 {
            w.push_log(log_at(run, 1_100 + i * 100, LogLevel::Info));
        }
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(span(&v, cx, 0), Some((400, true)));
        assert_eq!(span(&v, cx, 1), Some((300, false)));
        // Nothing about the requests changes: no new fetch.
        let jobs = v.read_with(cx, |v, _| v.spans_jobs());
        w.push_log(log_at(run, 1_700, LogLevel::Info));
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(v.read_with(cx, |v, _| v.spans_jobs()), jobs);
    }

    #[gpui_kit::test]
    fn stale_span_result_is_dropped_after_session_change(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, h) = open_zoomed(cx, demo());
        let w = v.read_with(cx, |v, _| v.plot_width());
        v.update(cx, |v, cx| v.pan_px(3.0 * w, cx));
        assert!(v.read_with(cx, |v, _| v.spans_in_flight()));
        // Another session, with 12 requests (6 of them in the fetch range), arrives before the fetch returns.
        let small = crate::test_support::snapshot(|w, run| {
            for i in 0..12u64 {
                request(
                    w,
                    run,
                    "GET",
                    "http://h/n",
                    Some(200),
                    i * 250,
                    Some(i * 250 + 50),
                );
            }
            w.push_frame(frame_at(run, 1, 10, 4, 4));
        });
        h.snaps.send(small).unwrap();
        cx.run_until_parked();
        v.read_with(cx, |v, _| {
            assert!(!v.spans_in_flight());
            let s = v.spans().expect("fetched for the new session");
            assert_eq!(s.items.len(), 6, "the new session's, not the old one's");
        });
    }

    #[gpui_kit::test]
    fn zoom_keys_change_the_viewport(cx: &mut gpui_kit::TestAppContext) {
        let (v, cx, _h) = open(cx, demo());
        let focus = v.read_with(cx, |v, _| v.focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        let fit = v.read_with(cx, |v, _| v.viewport());
        cx.dispatch_action(crate::actions::ZoomIn);
        let zoomed = v.read_with(cx, |v, _| v.viewport());
        assert!(zoomed.span_ns < fit.span_ns, "{zoomed:?} {fit:?}");
        cx.dispatch_action(crate::actions::ZoomOut);
        let out = v.read_with(cx, |v, _| v.viewport());
        assert!(out.span_ns > zoomed.span_ns);
        cx.dispatch_action(crate::actions::ZoomIn);
        cx.dispatch_action(crate::actions::ZoomToFit);
        assert_eq!(v.read_with(cx, |v, _| v.viewport()), fit);
    }
}
