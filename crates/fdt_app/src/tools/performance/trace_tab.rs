//! The Raw trace tab: a Perfetto-style view of the recorded VM timeline.
//!
//! One track per thread (and async row), one row per slice depth. A label column on the left,
//! a time ruler on top, and a canvas of slices: raw slices with names when zoomed in, level of
//! detail cells when a row is dense. The canvas is an entity of its own, drawn `.cached(..)` and
//! notified only when its data, viewport, scroll or selection change. Hover goes to the page as
//! an event, so the tooltip never busts the cache.

use crate::keymap::LANES_CONTEXT;
use crate::nav::TimeRange;
use crate::session_model::SessionModel;
use crate::shell::strip::MIN_DRAG_PX;
use crate::tools::network::model::format_duration_ns;
use fdt_store::Snapshot;
use fdt_store::time::SessionTime;
use fdt_store::timeline::{
    LANE_LOD_BASE_SHIFT, LANE_LOD_LEVELS, NameId, SliceCat, TrackId, TrackInfo,
};
use fdt_ui::canvas::{
    CANVAS_MAX_LABELS, CANVAS_MAX_QUADS, Label, Quad, ZoomPan, paint_labels, paint_quads,
    zoom_pan_listeners,
};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, space};
use fdt_ui::viewport::Viewport;
use gpui_kit::{
    Bounds, Context, CursorStyle, DispatchPhase, Entity, EventEmitter, FocusHandle, HitboxBehavior,
    Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window,
    canvas, point, px, size,
};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// What the tab says when nothing was recorded.
pub const EMPTY_TEXT: &str =
    "No timeline recorded yet. Timeline events appear while your app draws frames.";
/// How much one press of the zoom keys changes the scale.
const KEY_ZOOM: f32 = 1.5; // token-check: allow: zoom step, not a visual size
/// The canvas size before it has been drawn once.
const FIRST_WIDTH_PX: f32 = 900.0; // token-check: allow: stand-in until the first paint
const FIRST_HEIGHT_PX: f32 = 400.0; // token-check: allow: stand-in until the first paint
/// The spacing the ruler aims for between two ticks.
pub(crate) const TICK_TARGET_PX: f32 = 100.0; // token-check: allow: ruler density, not a visual size
/// Slices this wide or wider leave a one pixel gap to the next one.
const GAP_FROM_PX: f32 = 4.0; // token-check: allow: threshold, not a visual size
/// The faintest a level of detail cell is drawn.
const CELL_MIN_ALPHA: f32 = 0.2; // token-check: allow: opacity floor, not a visual size

/// One row of the track view: the lane at `depth` of `track`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRow {
    pub track: TrackId,
    pub depth: u16,
}

/// Quads kept back from the canvas cap for the overlays (selection outline, drag band, ruler
/// ticks, separators), which are added after the slices in the same layer. Reserving them up
/// front means the cap never cuts an overlay; a second layer would cost a second `paint_quads`
/// call for at most about 210 quads.
pub const OVERLAY_QUADS: usize = 256;
/// What the slices of one frame may use.
const SLICE_QUADS: usize = CANVAS_MAX_QUADS - OVERLAY_QUADS;

/// What an overlay quad is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    Outline,
    Drag,
    Tick,
    Border,
}

/// The overlay quads, in pixels from the canvas's top-left corner (`canvas` is its size): the
/// outline of the `selection` (plot coordinates), the ruler `drag` band, the ruler ticks and the
/// two separators. Never more than `OVERLAY_QUADS`.
pub fn overlay_rects(
    geo: &TraceGeometry,
    selection: Option<Rect>,
    drag: Option<(f32, f32)>,
    canvas: (f32, f32),
) -> Vec<(Rect, Overlay)> {
    let (cw, ch) = canvas;
    let label_w = f32::from(layout::LANE_LABEL_WIDTH);
    let ruler_h = f32::from(layout::RULER_HEIGHT);
    let e = f32::from(layout::BORDER);
    let mut out = Vec::new();
    if let Some(r) = selection {
        let (x, y, w, h) = (label_w + r.x, ruler_h + r.y, r.w, r.h);
        for r in [
            Rect { x, y, w, h: e },
            Rect {
                x,
                y: y + h - e,
                w,
                h: e,
            },
            Rect { x, y, w: e, h },
            Rect {
                x: x + w - e,
                y,
                w: e,
                h,
            },
        ] {
            out.push((r, Overlay::Outline));
        }
    }
    if let Some((a, z)) = drag {
        let (a, z) = (a.min(z), a.max(z));
        out.push((
            Rect {
                x: a,
                y: 0.0,
                w: z - a,
                h: ch,
            },
            Overlay::Drag,
        ));
    }
    let tick = 4.0; // token-check: allow: tick length
    for (x, _) in &geo.ruler {
        out.push((
            Rect {
                x: label_w + x,
                y: ruler_h - tick,
                w: e,
                h: tick,
            },
            Overlay::Tick,
        ));
    }
    out.push((
        Rect {
            x: 0.0,
            y: ruler_h - e,
            w: cw,
            h: e,
        },
        Overlay::Border,
    ));
    out.push((
        Rect {
            x: label_w - e,
            y: 0.0,
            w: e,
            h: ch,
        },
        Overlay::Border,
    ));
    // 4 outline + 1 drag + 2 borders + one per tick: the ticks are capped at 200 in
    // `ruler_ticks`, so this fits `OVERLAY_QUADS`. Raising that cap must raise the budget.
    debug_assert!(out.len() <= OVERLAY_QUADS, "{} overlay quads", out.len());
    out.truncate(OVERLAY_QUADS);
    out
}

/// The colour of a slice's name: `bg` or `text`, whichever reads better on the slice colour.
pub fn label_colour(t: &fdt_ui::tokens::Palette, cat: SliceCat) -> fdt_ui::tokens::Color {
    let fill = hue(t, cat);
    if t.bg.contrast(fill) >= t.text.contrast(fill) {
        t.bg
    } else {
        t.text
    }
}

/// A rectangle in pixels from the plot's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// How a quad is coloured: by the slice category, at an opacity (cells are scaled by coverage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorKey {
    pub cat: SliceCat,
    pub alpha: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceGeometry {
    pub quads: Vec<(Rect, ColorKey)>,
    pub labels: Vec<(Rect, NameId)>,
    /// The category of each label's slice, in the same order as `labels`.
    pub label_cats: Vec<SliceCat>,
    /// `(x, text)` for each ruler tick.
    pub ruler: Vec<(f32, String)>,
}

/// The rows of every track, the deeper ones left out for the `collapsed` tracks. A track with no
/// slices has no row.
pub fn trace_rows(snap: &Snapshot, collapsed: &HashSet<TrackId>) -> Vec<TraceRow> {
    let mut rows = Vec::new();
    for t in snap.timeline.tracks() {
        let depths = snap.timeline.lanes(t.id).len();
        let shown = if collapsed.contains(&t.id) {
            depths.min(1)
        } else {
            depths
        };
        rows.extend((0..shown).map(|d| TraceRow {
            track: t.id,
            depth: d as u16,
        }));
    }
    rows
}

/// The time the recorded slices cover, in nanoseconds; `None` with no slices.
pub fn timeline_bounds(snap: &Snapshot) -> Option<(u64, u64)> {
    let mut out: Option<(u64, u64)> = None;
    for t in snap.timeline.tracks() {
        for lane in snap.timeline.lanes(t.id) {
            if lane.is_empty() {
                continue;
            }
            let first = lane.get(0).start.0;
            let last = lane.get(lane.len() - 1).end().0;
            out = Some(match out {
                Some((a, z)) => (a.min(first), z.max(last)),
                None => (first, last),
            });
        }
    }
    out
}

/// The nanoseconds one cell of the level `cells(.., budget)` reads spans.
fn cell_ns(range: TimeRange, budget: usize) -> u64 {
    let k = (0..LANE_LOD_LEVELS)
        .find(|&k| {
            let sh = LANE_LOD_BASE_SHIFT + k as u32;
            (range.end.0 >> sh) - (range.start.0 >> sh) < budget as u64
        })
        .unwrap_or(LANE_LOD_LEVELS - 1);
    1u64 << (LANE_LOD_BASE_SHIFT + k as u32)
}

/// Lays the visible rows out in a plot `size` pixels big: `rows` scrolled by `scroll_y` pixels.
///
/// Only the visible rows are drawn. A row with few enough slices in view draws them raw (with a
/// label where one fits, at most `CANVAS_MAX_LABELS` in all); a denser row draws level of detail
/// cells, whose opacity follows their coverage. The quads never exceed `CANVAS_MAX_QUADS`.
pub fn trace_layout(
    snap: &Snapshot,
    viewport: Viewport,
    rows: &[TraceRow],
    scroll_y: f32,
    size: (f32, f32),
) -> TraceGeometry {
    let (width, height) = size;
    let mut geo = TraceGeometry::default();
    if width <= 0.0 || height <= 0.0 || rows.is_empty() {
        return geo;
    }
    let row_h = f32::from(layout::TRACK_ROW);
    let gap = f32::from(layout::BORDER);
    let min_label = f32::from(layout::LABEL_MIN_WIDTH);
    let scroll_y = scroll_y.max(0.0);
    let first = ((scroll_y / row_h).floor() as usize).min(rows.len());
    let last = (((scroll_y + height) / row_h).ceil() as usize).min(rows.len());
    let visible = last.saturating_sub(first);
    let per_row = (SLICE_QUADS / visible.max(1)).max(1);
    let (a, z) = viewport.range();
    let range = TimeRange::new(SessionTime(a), SessionTime(z));
    for (i, row) in rows.iter().enumerate().take(last).skip(first) {
        let Some(lane) = snap.timeline.lanes(row.track).get(row.depth as usize) else {
            continue;
        };
        let top = i as f32 * row_h - scroll_y;
        // Clip the row to the plot vertically.
        let y0 = top.max(0.0);
        let y1 = (top + row_h - gap).min(height);
        if y1 <= y0 {
            continue;
        }
        let indices = lane.range_indices(range);
        if indices.len() <= per_row {
            for k in indices {
                let s = lane.get(k);
                let (x0, x1) = (
                    viewport.x_of(s.start.0, width),
                    viewport.x_of(s.end().0, width),
                );
                if x1 < 0.0 || x0 > width {
                    continue;
                }
                let x = x0.max(0.0);
                let mut w = (x1.min(width) - x).max(1.0);
                if w >= GAP_FROM_PX {
                    w -= gap;
                }
                let rect = Rect {
                    x,
                    y: y0,
                    w,
                    h: y1 - y0,
                };
                geo.quads.push((
                    rect,
                    ColorKey {
                        cat: s.cat,
                        alpha: u8::MAX,
                    },
                ));
                if w >= min_label && geo.labels.len() < CANVAS_MAX_LABELS {
                    geo.labels.push((rect, s.name));
                    geo.label_cats.push(s.cat);
                }
            }
        } else {
            let budget = per_row.min(width as usize).max(1);
            let nominal = cell_ns(range, budget) as f64;
            for c in lane.cells(range, budget) {
                let x0 = viewport.x_of(c.start.0, width).max(0.0);
                let x1 = viewport.x_of(c.end.0, width).min(width);
                if x1 < x0 {
                    continue;
                }
                let cover = (c.covered_ns as f64 / nominal).clamp(0.0, 1.0) as f32;
                let a = CELL_MIN_ALPHA + (1.0 - CELL_MIN_ALPHA) * cover;
                geo.quads.push((
                    Rect {
                        x: x0,
                        y: y0,
                        w: (x1 - x0).max(1.0),
                        h: y1 - y0,
                    },
                    ColorKey {
                        cat: SliceCat::Dart,
                        alpha: (a * 255.0).round() as u8,
                    },
                ));
            }
        }
    }
    geo.quads.truncate(SLICE_QUADS);
    geo.ruler = ruler_ticks(viewport, width);
    geo
}

/// A tick spacing of 1, 2 or 5 times a power of ten, near `raw_ns`.
fn nice_step(raw_ns: f64) -> u64 {
    let raw = raw_ns.max(1.0);
    let pow = 10f64.powf(raw.log10().floor());
    let m = raw / pow;
    let nice = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    ((nice * pow) as u64).max(1)
}

/// The ruler's ticks for `viewport` in `width` pixels, labelled in seconds from the session start.
pub(crate) fn ruler_ticks(viewport: Viewport, width: f32) -> Vec<(f32, String)> {
    let mut out = Vec::new();
    if width <= 0.0 {
        return out;
    }
    let step = nice_step(viewport.span_ns as f64 * f64::from(TICK_TARGET_PX) / f64::from(width));
    let (start, end) = viewport.range();
    let decimals = (-(step as f64 / 1e9).log10()).ceil().clamp(0.0, 9.0) as usize;
    let mut t = start.div_ceil(step) * step;
    while t <= end && out.len() < 200 {
        let x = viewport.x_of(t, width);
        out.push((x, format!("{:.decimals$} s", t as f64 / 1e9)));
        t += step;
    }
    out
}

/// Where a slice is, stable across snapshots: its lane and start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceKey {
    pub track: TrackId,
    pub depth: u16,
    pub start: u64,
}

/// What a slice is, for the tooltip and the footer.
#[derive(Clone, Debug, PartialEq)]
pub struct SliceInfo {
    pub name: String,
    pub dur_ns: u64,
    pub start_ns: u64,
    pub track: String,
    pub cat: SliceCat,
    pub depth: u16,
}

fn cat_name(cat: SliceCat) -> &'static str {
    match cat {
        SliceCat::Dart => "Dart",
        SliceCat::Embedder => "Embedder",
        SliceCat::Gc => "GC",
        SliceCat::Other => "Other",
    }
}

/// The slice `key` names, if the snapshot has it.
pub fn slice_info(snap: &Snapshot, key: SliceKey) -> Option<SliceInfo> {
    let lane = snap.timeline.lanes(key.track).get(key.depth as usize)?;
    let at = SessionTime(key.start);
    let idx = lane.range_indices(TimeRange::new(at, at));
    let s = idx.map(|i| lane.get(i)).find(|s| s.start.0 == key.start)?;
    let track: &TrackInfo = snap.timeline.tracks().iter().find(|t| t.id == key.track)?;
    Some(SliceInfo {
        name: snap.timeline.name(s.name).to_string(),
        dur_ns: s.dur_ns,
        start_ns: s.start.0,
        track: track.name.to_string(),
        cat: s.cat,
        depth: key.depth,
    })
}

impl SliceInfo {
    fn offset(&self) -> String {
        format!("+{}", format_duration_ns(self.start_ns))
    }

    /// The hover text: name, duration, start offset, track and category.
    pub fn tooltip_text(&self) -> String {
        format!(
            "{} \u{b7} {} \u{b7} {} \u{b7} {} \u{b7} {}",
            self.name,
            format_duration_ns(self.dur_ns),
            self.offset(),
            self.track,
            cat_name(self.cat)
        )
    }

    /// The footer of a selected slice: the tooltip's fields and the depth.
    pub fn footer_text(&self) -> String {
        format!("{} \u{b7} depth {}", self.tooltip_text(), self.depth)
    }
}

/// What the trace tells the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TraceEvent {
    /// The pointer is over this slice, at this window position; `None` once it left.
    Hover(Option<(SliceKey, Point<Pixels>)>),
    /// The ruler was dragged to this range, or double-clicked (`None`) to clear it.
    Range(Option<TimeRange>),
}

#[derive(Clone, Copy, Debug)]
struct RulerDrag {
    start_x: f32,
    x: f32,
}

pub struct TraceView {
    snap: Arc<Snapshot>,
    viewport: Viewport,
    /// The shared range the view fits, if any.
    range: Option<TimeRange>,
    /// Whether the view fits its range or the session, so new data refits it.
    fitted: bool,
    collapsed: HashSet<TrackId>,
    rows: Vec<TraceRow>,
    scroll_y: f32,
    selected: Option<SliceKey>,
    hover: Option<SliceKey>,
    drag: Option<RulerDrag>,
    /// The canvas size at its last paint, label column and ruler included.
    size: Rc<Cell<(f32, f32)>>,
    focus: FocusHandle,
    _observe: Option<Subscription>,
}

impl EventEmitter<TraceEvent> for TraceView {}

impl TraceView {
    pub fn new(session: Entity<SessionModel>, cx: &mut Context<Self>) -> Self {
        let snap = session.read(cx).snapshot().clone();
        let observe = cx.observe(&session, |this, session, cx| {
            let snap = session.read(cx).snapshot().clone();
            this.set_snapshot(snap, cx);
        });
        let mut v = Self::with_snapshot(snap, cx);
        v._observe = Some(observe);
        v
    }

    fn with_snapshot(snap: Arc<Snapshot>, cx: &mut Context<Self>) -> Self {
        let mut v = Self {
            snap,
            viewport: Viewport::fit(0, 0),
            range: None,
            fitted: true,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            scroll_y: 0.0,
            selected: None,
            hover: None,
            drag: None,
            size: Rc::new(Cell::new((FIRST_WIDTH_PX, FIRST_HEIGHT_PX))),
            focus: cx.focus_handle(),
            _observe: None,
        };
        v.rebuild();
        v.fit();
        v
    }

    #[cfg(test)]
    pub fn for_test(snap: Arc<Snapshot>, cx: &mut Context<Self>) -> Self {
        Self::with_snapshot(snap, cx)
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn selected(&self) -> Option<SliceKey> {
        self.selected
    }

    pub fn is_empty(&self) -> bool {
        self.snap.timeline.slice_count() == 0
    }

    fn rebuild(&mut self) {
        self.rows = trace_rows(&self.snap, &self.collapsed);
        self.scroll_y = self.scroll_y.min(self.max_scroll());
    }

    fn bounds(&self) -> (u64, u64) {
        timeline_bounds(&self.snap).unwrap_or((0, 0))
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

    pub fn set_snapshot(&mut self, snap: Arc<Snapshot>, cx: &mut Context<Self>) {
        let new_session = snap.session != self.snap.session;
        let same = !new_session
            && snap.timeline.slice_count() == self.snap.timeline.slice_count()
            && tracks_match(&snap, &self.snap);
        self.snap = snap;
        if same {
            // Nothing the canvas shows changed (a log-only snapshot): keep its cache.
            return;
        }
        if new_session {
            // Track ids restart in a new session, so every key of the old one is meaningless.
            self.selected = None;
            self.collapsed.clear();
            self.scroll_y = 0.0;
            self.fitted = true;
        }
        self.rebuild();
        if self.fitted {
            self.fit();
        }
        if self
            .selected
            .is_some_and(|k| slice_info(&self.snap, k).is_none())
        {
            self.selected = None;
        }
        if self.hover.take().is_some() {
            // The page keeps the tooltip until told; the slice may be gone or have moved.
            cx.emit(TraceEvent::Hover(None));
        }
        cx.notify();
    }

    /// Fits the shared range, or the whole session without one.
    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range == range {
            return;
        }
        self.range = range;
        self.fit();
        cx.notify();
    }

    pub fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
        self.scroll_y = 0.0;
        cx.notify();
    }

    fn plot_width(&self) -> f32 {
        (self.size.get().0 - f32::from(layout::LANE_LABEL_WIDTH)).max(1.0)
    }

    fn plot_height(&self) -> f32 {
        (self.size.get().1 - f32::from(layout::RULER_HEIGHT)).max(0.0)
    }

    /// Zooms about `x` pixels from the plot's left edge.
    pub fn zoom_about(&mut self, x: f32, factor: f32, cx: &mut Context<Self>) {
        let w = self.plot_width();
        let bounds = self.bounds();
        self.viewport.zoom_about(x, w, factor, bounds);
        self.fitted = false;
        cx.notify();
    }

    /// Pans by `px` pixels; positive shows later time.
    pub fn pan_px(&mut self, px: f32, cx: &mut Context<Self>) {
        let w = self.plot_width();
        let bounds = self.bounds();
        self.viewport.pan_px(px, w, bounds);
        self.fitted = false;
        cx.notify();
    }

    fn max_scroll(&self) -> f32 {
        let content = self.rows.len() as f32 * f32::from(layout::TRACK_ROW);
        (content - self.plot_height()).max(0.0)
    }

    /// Scrolls the rows by `px` pixels; positive shows lower rows.
    pub fn scroll_y(&mut self, px: f32, cx: &mut Context<Self>) {
        let to = (self.scroll_y + px).clamp(0.0, self.max_scroll());
        if (to - self.scroll_y).abs() > f32::EPSILON {
            self.scroll_y = to;
            cx.notify();
        }
    }

    fn zoom_key(&mut self, factor: f32, cx: &mut Context<Self>) {
        let half = self.plot_width() / 2.0;
        self.zoom_about(half, factor, cx);
    }

    /// Collapses `track` to its first row, or expands it again.
    pub fn toggle_track(&mut self, track: TrackId, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&track) {
            self.collapsed.insert(track);
        }
        self.rebuild();
        cx.notify();
    }

    pub fn is_collapsed(&self, track: TrackId) -> bool {
        self.collapsed.contains(&track)
    }

    /// The slice at `(x, y)` pixels from the plot's top-left corner.
    fn slice_at(&self, x: f32, y: f32) -> Option<SliceKey> {
        let width = self.plot_width();
        if x < 0.0 || y < 0.0 || x > width {
            return None;
        }
        let row = ((y + self.scroll_y) / f32::from(layout::TRACK_ROW)).floor() as usize;
        let row = *self.rows.get(row)?;
        let lane = self
            .snap
            .timeline
            .lanes(row.track)
            .get(row.depth as usize)?;
        // Reach one pixel either side, so a sliver of a slice can be hit.
        let px_ns = (self.viewport.span_ns as f64 / f64::from(width)).ceil() as u64;
        let t = self.viewport.time_at(x, width);
        let r = TimeRange::new(
            SessionTime(t.saturating_sub(px_ns)),
            SessionTime(t.saturating_add(px_ns)),
        );
        let exact = SessionTime(t);
        let mut hit: Option<(u64, u64)> = None;
        for i in lane.range_indices(r) {
            let s = lane.get(i);
            let dist = if s.start <= exact && exact <= s.end() {
                0
            } else {
                s.start.0.abs_diff(t).min(s.end().0.abs_diff(t))
            };
            if hit.is_none_or(|(d, _)| dist < d) {
                hit = Some((dist, s.start.0));
            }
        }
        hit.map(|(_, start)| SliceKey {
            track: row.track,
            depth: row.depth,
            start,
        })
    }

    /// The track whose name is at `y` pixels below the ruler, if any.
    fn track_at(&self, y: f32) -> Option<TrackId> {
        let row = ((y + self.scroll_y) / f32::from(layout::TRACK_ROW)).floor();
        if row < 0.0 {
            return None;
        }
        self.rows.get(row as usize).map(|r| r.track)
    }

    fn selection_rect(&self) -> Option<Rect> {
        let key = self.selected?;
        let i = self
            .rows
            .iter()
            .position(|r| r.track == key.track && r.depth == key.depth)?;
        let info = slice_info(&self.snap, key)?;
        let width = self.plot_width();
        let row_h = f32::from(layout::TRACK_ROW);
        let x0 = self.viewport.x_of(info.start_ns, width);
        let x1 = self.viewport.x_of(info.start_ns + info.dur_ns, width);
        let top = i as f32 * row_h - self.scroll_y;
        if x1 < 0.0 || x0 > width || top + row_h < 0.0 || top > self.plot_height() {
            return None;
        }
        Some(Rect {
            x: x0.max(0.0),
            y: top,
            w: (x1.min(width) - x0.max(0.0)).max(1.0),
            h: row_h - f32::from(layout::BORDER),
        })
    }

    /// The tooltip text of the slice `key` names.
    pub fn tooltip(&self, key: SliceKey) -> Option<String> {
        slice_info(&self.snap, key).map(|i| i.tooltip_text())
    }

    /// The footer text of the selected slice.
    pub fn footer(&self) -> Option<String> {
        slice_info(&self.snap, self.selected?).map(|i| i.footer_text())
    }

    // Input, in pixels from the canvas's top-left corner.

    fn press(&mut self, x: f32, y: f32, click_count: usize, cx: &mut Context<Self>) {
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        let ruler_h = f32::from(layout::RULER_HEIGHT);
        if y < ruler_h {
            if x < label_w {
                return;
            }
            if click_count >= 2 {
                self.drag = None;
                cx.emit(TraceEvent::Range(None));
                return;
            }
            self.drag = Some(RulerDrag { start_x: x, x });
            cx.notify();
        } else if x < label_w {
            if let Some(track) = self.track_at(y - ruler_h) {
                self.toggle_track(track, cx);
            }
        } else {
            let key = self.slice_at(x - label_w, y - ruler_h);
            if key != self.selected {
                self.selected = key;
                cx.notify();
            }
        }
    }

    fn moved(
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
        let ruler_h = f32::from(layout::RULER_HEIGHT);
        let key = (inside && x >= label_w && y >= ruler_h)
            .then(|| self.slice_at(x - label_w, y - ruler_h))
            .flatten();
        if key != self.hover {
            self.hover = key;
            cx.emit(TraceEvent::Hover(key.map(|k| (k, at))));
        }
    }

    fn released(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        cx.notify();
        if (x - d.start_x).abs() < MIN_DRAG_PX {
            // A click on the ruler sets no range; it only drops the slice selection.
            if self.selected.take().is_some() {
                cx.notify();
            }
            return;
        }
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        let w = self.plot_width();
        let a = self.viewport.time_at(d.start_x - label_w, w);
        let z = self.viewport.time_at(x - label_w, w);
        cx.emit(TraceEvent::Range(Some(TimeRange::new(
            SessionTime(a),
            SessionTime(z),
        ))));
    }
}

/// Whether both snapshots list the same tracks: id, run, order and name. O(tracks).
fn tracks_match(a: &Snapshot, b: &Snapshot) -> bool {
    let (a, b) = (a.timeline.tracks(), b.timeline.tracks());
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.id == y.id && x.run == y.run && x.order == y.order && x.name == y.name)
}

fn hue(t: &fdt_ui::tokens::Palette, cat: SliceCat) -> fdt_ui::tokens::Color {
    match cat {
        SliceCat::Dart => t.accent,
        SliceCat::Embedder => t.lane_network,
        SliceCat::Gc => t.text_muted,
        SliceCat::Other => t.text_faint,
    }
}

impl Render for TraceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.tokens();
        if self.is_empty() {
            return gpui_kit::div()
                .id("trace-empty")
                .test_support()
                .aria_label(EMPTY_TEXT)
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(fdt_ui::empty_state::EmptyState::new(
                    "trace-empty-state",
                    crate::shell::sidebar::tool_icon(crate::nav::Tool::Performance),
                    "No timeline recorded yet",
                    "Timeline events appear while your app draws frames.",
                ))
                .into_any_element();
        }
        let view = cx.entity();
        let snap = self.snap.clone();
        let (viewport, scroll_y) = (self.viewport, self.scroll_y);
        let rows = self.rows.clone();
        let selection = self.selection_rect();
        let drag = self.drag;
        let collapsed = self.collapsed.clone();
        let size_cell = self.size.clone();
        let surface = canvas(
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
                let label_w = layout::LANE_LABEL_WIDTH;
                let ruler_h = layout::RULER_HEIGHT;
                let (cw, ch) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let plot_w = (cw - f32::from(label_w)).max(1.0);
                let plot_h = (ch - f32::from(ruler_h)).max(0.0);
                let plot = Bounds::new(
                    point(ox + label_w, oy + ruler_h),
                    size(px(plot_w), px(plot_h)),
                );
                let geo = trace_layout(&snap, viewport, &rows, scroll_y, (plot_w, plot_h));
                let to_bounds = |r: &Rect| {
                    Bounds::new(
                        point(plot.origin.x + px(r.x), plot.origin.y + px(r.y)),
                        size(px(r.w), px(r.h)),
                    )
                };
                let mut quads: Vec<Quad> = geo
                    .quads
                    .iter()
                    .map(|(r, k)| Quad {
                        bounds: to_bounds(r),
                        color: Hsla::from(hue(&t, k.cat)).opacity(f32::from(k.alpha) / 255.0),
                    })
                    .collect();
                for (r, kind) in
                    overlay_rects(&geo, selection, drag.map(|d| (d.start_x, d.x)), (cw, ch))
                {
                    let color = match kind {
                        Overlay::Outline => Hsla::from(t.text),
                        Overlay::Drag => t.accent.with_alpha(alpha::SELECTION),
                        Overlay::Tick => Hsla::from(t.text_faint),
                        Overlay::Border => Hsla::from(t.border),
                    };
                    quads.push(Quad {
                        bounds: Bounds::new(
                            point(ox + px(r.x), oy + px(r.y)),
                            size(px(r.w), px(r.h)),
                        ),
                        color,
                    });
                }
                paint_quads(window, bounds, &quads);

                let line = font::LABEL * font::LINE_HEIGHT_UI;
                let centred = (layout::TRACK_ROW - line) / 2.0;
                let slice_labels: Vec<Label> = geo
                    .labels
                    .iter()
                    .zip(&geo.label_cats)
                    .map(|((r, name), cat)| Label {
                        text: snap.timeline.name(*name).to_string().into(),
                        origin: point(
                            plot.origin.x + px(r.x) + space::S4,
                            plot.origin.y + px(r.y) + centred,
                        ),
                        max_width: px(r.w) - space::S4,
                        color: Hsla::from(label_colour(&t, *cat)),
                    })
                    .collect();
                paint_labels(window, cx, plot, &slice_labels);

                let tick_labels: Vec<Label> = geo
                    .ruler
                    .iter()
                    .map(|(x, text)| Label {
                        text: SharedString::from(text.clone()),
                        origin: point(
                            plot.origin.x + px(*x) + space::S4,
                            oy + (ruler_h - line) / 2.0,
                        ),
                        max_width: px(TICK_TARGET_PX) - space::S8,
                        color: Hsla::from(t.text_muted),
                    })
                    .collect();
                paint_labels(
                    window,
                    cx,
                    Bounds::new(point(plot.origin.x, oy), size(px(plot_w), ruler_h)),
                    &tick_labels,
                );

                // Track names, at each track's first row.
                let row_h = f32::from(layout::TRACK_ROW);
                let first = (scroll_y / row_h).floor() as usize;
                let last = ((scroll_y + plot_h) / row_h).ceil() as usize;
                let names: Vec<Label> = rows
                    .iter()
                    .enumerate()
                    .take(last)
                    .skip(first)
                    .filter(|(_, r)| r.depth == 0)
                    .filter_map(|(i, r)| {
                        let info = snap.timeline.tracks().iter().find(|t| t.id == r.track)?;
                        let deeper = snap.timeline.lanes(r.track).len() > 1;
                        let mark = match (deeper, collapsed.contains(&r.track)) {
                            (false, _) => "  ",
                            (true, true) => "\u{25b8} ",
                            (true, false) => "\u{25be} ",
                        };
                        Some(Label {
                            text: format!("{mark}{}", info.name).into(),
                            origin: point(
                                ox + space::S4,
                                plot.origin.y + px(i as f32 * row_h - scroll_y) + centred,
                            ),
                            max_width: label_w - space::S8,
                            color: Hsla::from(t.text),
                        })
                    })
                    .collect();
                paint_labels(
                    window,
                    cx,
                    Bounds::new(
                        point(ox, plot.origin.y),
                        size(label_w - layout::BORDER, px(plot_h)),
                    ),
                    &names,
                );
                window.set_cursor_style(CursorStyle::Arrow, &hitbox);

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
                let label_w = f32::from(layout::LANE_LABEL_WIDTH);
                zoom_pan_listeners(
                    window,
                    &hitbox,
                    fdt_ui::tokens::row::TABLE,
                    move |action, _, cx| {
                        v.update(cx, |this, cx| match action {
                            ZoomPan::Zoom { x, factor } => {
                                this.zoom_about(f32::from(x) - label_w, factor, cx)
                            }
                            ZoomPan::PanX(d) => this.pan_px(f32::from(d), cx),
                            ZoomPan::PanY(d) => this.scroll_y(f32::from(d), cx),
                        });
                    },
                );
            },
        )
        .size_full();
        let footer = self.footer();
        gpui_kit::div()
            .id("trace-canvas")
            .test_support()
            .key_context(LANES_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .on_action(
                cx.listener(|this, _: &crate::actions::ZoomIn, _, cx| this.zoom_key(KEY_ZOOM, cx)),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ZoomOut, _, cx| {
                this.zoom_key(1.0 / KEY_ZOOM, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::actions::ZoomToFit, _, cx| this.zoom_to_fit(cx)),
            )
            .child(gpui_kit::div().flex_1().min_h_0().w_full().child(surface))
            .children(footer.map(|text| {
                gpui_kit::div()
                    .id("trace-footer")
                    .test_support()
                    .aria_label(SharedString::from(text.clone()))
                    .flex_none()
                    .h(fdt_ui::tokens::row::TABLE)
                    .flex()
                    .items_center()
                    .px(space::S16)
                    .border_t(layout::BORDER)
                    .border_color(Hsla::from(t.border))
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(text)
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::snapshot;
    use fdt_store::timeline::{Slice, TrackKind};
    use gpui_kit::AppContext as _;
    use std::cell::RefCell;

    fn ms(n: u64) -> SessionTime {
        SessionTime(n * 1_000_000)
    }

    fn slice(start_ms: u64, dur_ms: u64, name: NameId, cat: SliceCat) -> Slice {
        Slice {
            start: ms(start_ms),
            dur_ns: dur_ms * 1_000_000,
            name,
            cat,
        }
    }

    /// `tracks` tracks of `depths` lanes, each lane with `per_lane` 1 ms slices 2 ms apart.
    fn stress(tracks: u64, depths: u16, per_lane: u64) -> Arc<Snapshot> {
        snapshot(|w, run| {
            let name = w.intern_name("Build");
            for t in 0..tracks {
                let id = w.timeline_track(run, TrackKind::Thread, t, &format!("thread {t}"));
                for d in 0..depths {
                    for i in 0..per_lane {
                        w.push_slice(id, d, slice(i * 2, 1, name, SliceCat::Dart), false);
                    }
                }
            }
        })
    }

    fn rows_of(snap: &Snapshot) -> Vec<TraceRow> {
        trace_rows(snap, &HashSet::new())
    }

    #[test]
    fn layout_switches_to_cells_when_dense() {
        let snap = stress(20, 2, 5_000);
        let rows = rows_of(&snap);
        assert_eq!(rows.len(), 40);
        let (a, z) = timeline_bounds(&snap).unwrap();
        let vp = Viewport::fit(a, z);
        let geo = trace_layout(&snap, vp, &rows, 0.0, (1400.0, 40.0 * 18.0));
        assert!(!geo.quads.is_empty());
        assert!(geo.quads.len() <= CANVAS_MAX_QUADS, "{}", geo.quads.len());
        assert!(geo.labels.len() <= CANVAS_MAX_LABELS);
        // Dense rows draw cells, so nothing is labelled and the opacity follows coverage.
        assert!(geo.labels.is_empty());
        assert!(geo.quads.iter().all(|(_, k)| k.alpha < u8::MAX));
        assert!(geo.quads.iter().any(|(_, k)| k.alpha > 0x33));
        assert!(!geo.ruler.is_empty());
    }

    #[test]
    fn layout_draws_raw_slices_when_zoomed_in() {
        let snap = stress(2, 1, 5_000);
        let rows = rows_of(&snap);
        // 40 ms of a 10 s session: 20 slices a row, each 1 ms = 30 px wide.
        let vp = Viewport::fit(ms(1_000).0, ms(1_039).0);
        let geo = trace_layout(&snap, vp, &rows, 0.0, (1500.0, 100.0));
        assert_eq!(geo.quads.len(), 40);
        assert!(geo.quads.iter().all(|(_, k)| k.alpha == u8::MAX));
        assert_eq!(geo.labels.len(), 40);
        assert!(geo.labels.iter().all(|(r, _)| r.w >= 32.0));
        // Only rows in view are drawn: scrolled one row down, the first track is gone.
        let geo = trace_layout(&snap, vp, &rows, 18.0, (1500.0, 18.0));
        assert_eq!(geo.quads.len(), 20);
        assert!(
            geo.quads
                .iter()
                .all(|(r, _)| r.y >= 0.0 && r.y + r.h <= 18.0)
        );
    }

    #[test]
    fn colours_follow_the_category() {
        let snap = snapshot(|w, run| {
            let name = w.intern_name("x");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "t");
            for (i, cat) in [
                SliceCat::Dart,
                SliceCat::Embedder,
                SliceCat::Gc,
                SliceCat::Other,
            ]
            .into_iter()
            .enumerate()
            {
                w.push_slice(id, 0, slice(i as u64 * 10, 5, name, cat), false);
            }
        });
        let vp = Viewport::fit(0, ms(40).0);
        let geo = trace_layout(&snap, vp, &rows_of(&snap), 0.0, (400.0, 18.0));
        let cats: Vec<_> = geo.quads.iter().map(|(_, k)| k.cat).collect();
        assert_eq!(
            cats,
            [
                SliceCat::Dart,
                SliceCat::Embedder,
                SliceCat::Gc,
                SliceCat::Other
            ]
        );
    }

    #[test]
    fn deep_rows_stop_at_the_store_cap() {
        let snap = snapshot(|w, run| {
            let name = w.intern_name("deep");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "main");
            let mut kept = 0;
            for d in 0..300u16 {
                if w.push_slice(id, d, slice(0, 10, name, SliceCat::Dart), false) {
                    kept += 1;
                }
            }
            assert_eq!(kept, fdt_store::timeline::MAX_DEPTH);
        });
        assert_eq!(snap.timeline.drops().depth, 44);
        let rows = rows_of(&snap);
        assert_eq!(rows.len(), fdt_store::timeline::MAX_DEPTH);
        assert!(
            rows.iter()
                .all(|r| (r.depth as usize) < fdt_store::timeline::MAX_DEPTH)
        );
        // A tall plot draws every row, none past the cap, and nothing panics.
        let vp = Viewport::fit(0, ms(10).0);
        let geo = trace_layout(&snap, vp, &rows, 0.0, (400.0, 18.0 * 400.0));
        assert_eq!(geo.quads.len(), fdt_store::timeline::MAX_DEPTH);
        // Collapsing keeps only the first row.
        let main = snap.timeline.tracks()[0].id;
        let rows = trace_rows(&snap, &HashSet::from([main]));
        assert_eq!(rows.len(), 1);
    }

    fn view(cx: &mut gpui_kit::TestAppContext, snap: Arc<Snapshot>) -> Entity<TraceView> {
        cx.new(|cx| TraceView::for_test(snap, cx))
    }

    fn log_events(
        cx: &mut gpui_kit::TestAppContext,
        v: &Entity<TraceView>,
    ) -> (Rc<RefCell<Vec<TraceEvent>>>, Subscription) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let sub =
            cx.update(|cx| cx.subscribe(v, move |_, e: &TraceEvent, _| l.borrow_mut().push(*e)));
        (log, sub)
    }

    #[gpui_kit::test]
    fn ruler_drag_sets_range_and_zero_width_drag_selects(cx: &mut gpui_kit::TestAppContext) {
        let v = view(cx, stress(1, 1, 500));
        let (log, _sub) = log_events(cx, &v);
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        let ruler_y = 5.0;
        let at = point(px(0.), px(0.));
        let (w, vp) = v.read_with(cx, |v, _| (v.plot_width(), v.viewport()));
        v.update(cx, |v, cx| {
            v.press(label_w + 100.0, ruler_y, 1, cx);
            v.moved(label_w + 300.0, ruler_y, at, true, true, cx);
            v.released(label_w + 300.0, cx);
        });
        let want = TimeRange::new(
            SessionTime(vp.time_at(100.0, w)),
            SessionTime(vp.time_at(300.0, w)),
        );
        assert_eq!(*log.borrow(), vec![TraceEvent::Range(Some(want))]);
        log.borrow_mut().clear();
        // Select a slice, then click the ruler without moving: no range, the selection goes.
        v.update(cx, |v, cx| {
            v.press(label_w + 1.0, f32::from(layout::RULER_HEIGHT) + 1.0, 1, cx);
            assert!(v.selected().is_some());
            v.press(label_w + 50.0, ruler_y, 1, cx);
            v.released(label_w + 50.0, cx);
            assert!(v.selected().is_none());
        });
        assert!(log.borrow().is_empty());
        // A double-click clears the range.
        v.update(cx, |v, cx| v.press(label_w + 50.0, ruler_y, 2, cx));
        assert_eq!(*log.borrow(), vec![TraceEvent::Range(None)]);
    }

    #[gpui_kit::test]
    fn range_refits_and_zero_range_returns_to_the_session(cx: &mut gpui_kit::TestAppContext) {
        let v = view(cx, stress(1, 1, 500));
        v.update(cx, |v, cx| {
            let whole = v.viewport();
            let r = TimeRange::new(ms(100), ms(200));
            v.set_range(Some(r), cx);
            assert_eq!(v.viewport(), Viewport::fit(r.start.0, r.end.0));
            v.set_range(None, cx);
            assert_eq!(v.viewport(), whole);
        });
    }

    #[gpui_kit::test]
    fn zoom_keys_change_the_viewport(cx: &mut gpui_kit::TestAppContext) {
        let v = view(cx, stress(1, 1, 500));
        v.update(cx, |v, cx| {
            let fit = v.viewport();
            v.zoom_key(KEY_ZOOM, cx);
            let zoomed = v.viewport();
            assert!(zoomed.span_ns < fit.span_ns);
            v.pan_px(100.0, cx);
            assert!(v.viewport().start_ns > zoomed.start_ns);
            v.zoom_key(1.0 / KEY_ZOOM, cx);
            assert!(v.viewport().span_ns > zoomed.span_ns);
            v.zoom_to_fit(cx);
            assert_eq!(v.viewport(), fit);
        });
    }

    #[gpui_kit::test]
    fn vertical_scroll_moves_rows_and_clicks_collapse(cx: &mut gpui_kit::TestAppContext) {
        let v = view(cx, stress(40, 2, 10));
        v.update(cx, |v, cx| {
            let start = v.viewport();
            assert!(v.max_scroll() > 0.0);
            v.scroll_y(1e6, cx);
            assert_eq!(v.scroll_y, v.max_scroll());
            v.scroll_y(-1e6, cx);
            assert_eq!(v.scroll_y, 0.0);
            assert_eq!(v.viewport(), start, "time did not move");
            // Click the first track's name: its deeper row goes.
            let rows = v.rows.len();
            v.press(10.0, f32::from(layout::RULER_HEIGHT) + 5.0, 1, cx);
            assert_eq!(v.rows.len(), rows - 1);
            v.press(10.0, f32::from(layout::RULER_HEIGHT) + 5.0, 1, cx);
            assert_eq!(v.rows.len(), rows);
        });
    }

    #[gpui_kit::test]
    fn tooltip_text(cx: &mut gpui_kit::TestAppContext) {
        let snap = snapshot(|w, run| {
            let name = w.intern_name("Engine::BeginFrame");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "1.ui");
            w.push_slice(id, 0, slice(2_000, 5, name, SliceCat::Embedder), false);
            w.push_slice(id, 1, slice(2_001, 1, name, SliceCat::Dart), false);
        });
        let v = view(cx, snap.clone());
        let key = SliceKey {
            track: snap.timeline.tracks()[0].id,
            depth: 0,
            start: ms(2_000).0,
        };
        v.update(cx, |v, cx| {
            assert_eq!(
                v.tooltip(key).as_deref(),
                Some("Engine::BeginFrame \u{b7} 5 ms \u{b7} +2.0 s \u{b7} 1.ui \u{b7} Embedder")
            );
            // Hovering over the plot finds the same slice from a pixel position.
            let w = v.plot_width();
            let x = v.viewport().x_of(ms(2_002).0, w);
            assert_eq!(v.slice_at(x, 1.0), Some(key));
            // A click shows the footer with the depth.
            let label_w = f32::from(layout::LANE_LABEL_WIDTH);
            v.press(label_w + x, f32::from(layout::RULER_HEIGHT) + 1.0, 1, cx);
            assert_eq!(
                v.footer().as_deref(),
                Some(
                    "Engine::BeginFrame \u{b7} 5 ms \u{b7} +2.0 s \u{b7} 1.ui \u{b7} Embedder \u{b7} depth 0"
                )
            );
        });
    }

    #[gpui_kit::test]
    fn empty_state(cx: &mut gpui_kit::TestAppContext) {
        use crate::test_support::{exists, open_shell_snapshot};
        use gpui_kit::test::TestWindowExt as _;
        let snap = snapshot(|w, run| {
            w.push_frame(crate::test_support::frame_at(run, 0, 1, 8, 1));
        });
        let (_shell, cx) = open_shell_snapshot(cx, snap);
        cx.dispatch_action(crate::actions::ShowTraceTab);
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        assert!(exists(cx, "trace-empty"));
        let label = cx.update(|window, _| {
            window
                .try_find("trace-empty")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("No timeline recorded yet. Timeline events appear while your app draws frames.")
        );
    }

    #[gpui_kit::test]
    fn ruler_drag_reaches_the_shell_range(cx: &mut gpui_kit::TestAppContext) {
        use crate::test_support::open_shell_snapshot;
        let (shell, cx) = open_shell_snapshot(cx, stress(1, 1, 500));
        cx.dispatch_action(crate::actions::ShowTraceTab);
        cx.run_until_parked();
        let perf = shell.read_with(cx, |s, _| s.performance_view().clone());
        let trace = perf.read_with(cx, |p, _| p.trace().clone());
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        trace.update(cx, |t, cx| {
            t.press(label_w + 100.0, 5.0, 1, cx);
            t.released(label_w + 300.0, cx);
        });
        cx.run_until_parked();
        let range = perf.read_with(cx, |p, _| p.range());
        assert!(range.is_some_and(|r| r.duration_ns() > 0));
        // The trace now fits that range.
        let vp = trace.read_with(cx, |t, _| t.viewport());
        let r = range.unwrap();
        assert_eq!(vp, Viewport::fit(r.start.0, r.end.0));
        trace.update(cx, |t, cx| t.press(label_w + 10.0, 5.0, 2, cx));
        cx.run_until_parked();
        assert_eq!(perf.read_with(cx, |p, _| p.range()), None);
    }

    #[test]
    fn dense_trace_keeps_overlays() {
        let snap = stress(20, 2, 5_000);
        let rows = rows_of(&snap);
        let (a, z) = timeline_bounds(&snap).unwrap();
        let vp = Viewport::fit(a, z);
        let geo = trace_layout(&snap, vp, &rows, 0.0, (1400.0, 40.0 * 18.0));
        let sel = Rect {
            x: 10.0,
            y: 0.0,
            w: 50.0,
            h: 17.0,
        };
        let over = overlay_rects(&geo, Some(sel), Some((20.0, 90.0)), (1512.0, 740.0));
        assert!(geo.quads.len() + over.len() <= CANVAS_MAX_QUADS);
        assert_eq!(
            over.iter().filter(|(_, k)| *k == Overlay::Outline).count(),
            4
        );
        assert_eq!(
            over.iter().filter(|(_, k)| *k == Overlay::Tick).count(),
            geo.ruler.len()
        );
        assert!(!geo.ruler.is_empty());
        assert_eq!(
            over.iter().filter(|(_, k)| *k == Overlay::Border).count(),
            2
        );
        assert!(over.iter().any(|(_, k)| *k == Overlay::Drag));
    }

    #[test]
    fn trace_label_colours_meet_contrast() {
        for (name, p) in [
            ("dark", &fdt_ui::tokens::DARK),
            ("light", &fdt_ui::tokens::LIGHT),
        ] {
            for cat in [
                SliceCat::Dart,
                SliceCat::Embedder,
                SliceCat::Gc,
                SliceCat::Other,
            ] {
                let c = label_colour(p, cat).contrast(hue(p, cat));
                assert!(c >= 4.5, "{name} {cat:?}: {c:.2}");
            }
        }
    }

    #[gpui_kit::test]
    fn tooltip_clears_or_follows_after_new_data(cx: &mut gpui_kit::TestAppContext) {
        let v = view(cx, stress(1, 1, 50));
        let (log, _sub) = log_events(cx, &v);
        let label_w = f32::from(layout::LANE_LABEL_WIDTH);
        let ruler_h = f32::from(layout::RULER_HEIGHT);
        let at = point(px(0.), px(0.));
        // Hover over the first slice.
        v.update(cx, |v, cx| {
            v.moved(label_w + 1.0, ruler_h + 1.0, at, false, true, cx)
        });
        assert!(matches!(
            log.borrow().last(),
            Some(TraceEvent::Hover(Some(_)))
        ));
        // New data arrives: the page is told the tooltip is gone.
        v.update(cx, |v, cx| v.set_snapshot(stress(1, 1, 60), cx));
        assert_eq!(log.borrow().last(), Some(&TraceEvent::Hover(None)));
    }

    #[gpui_kit::test]
    fn snapshot_changes_that_keep_counts_still_refresh(cx: &mut gpui_kit::TestAppContext) {
        use crate::test_support::writer;
        let names = |v: &Entity<TraceView>, cx: &mut gpui_kit::TestAppContext| {
            v.read_with(cx, |v, _| {
                v.snap
                    .timeline
                    .tracks()
                    .iter()
                    .map(|t| t.name.to_string())
                    .collect::<Vec<_>>()
            })
        };
        // A renamed track and a re-ordered track, same counts, same session.
        let (mut w, run) = writer();
        let name = w.intern_name("x");
        let a = w.timeline_track(run, TrackKind::Thread, 1, "");
        let b = w.timeline_track(run, TrackKind::Thread, 2, "b");
        w.set_track_order(a, 10);
        w.set_track_order(b, 10);
        w.push_slice(a, 0, slice(0, 1, name, SliceCat::Dart), false);
        w.push_slice(b, 0, slice(0, 1, name, SliceCat::Dart), false);
        let first = w.publish();
        let v = view(cx, first);
        v.update(cx, |v, _| v.scroll_y = 0.0);
        let notified = Rc::new(Cell::new(0));
        let n = notified.clone();
        let _obs = cx.update(|cx| cx.observe(&v, move |_, _| n.set(n.get() + 1)));
        // An unchanged snapshot is not redrawn.
        let same = v.read_with(cx, |v, _| v.snap.clone());
        v.update(cx, |v, cx| v.set_snapshot(same, cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 0);
        w.timeline_track(run, TrackKind::Thread, 1, "late name");
        let renamed = w.publish();
        v.update(cx, |v, cx| v.set_snapshot(renamed, cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 1);
        assert!(names(&v, cx).contains(&"late name".to_string()));
        w.set_track_order(b, 1);
        let reordered = w.publish();
        v.update(cx, |v, cx| v.set_snapshot(reordered.clone(), cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 2);
        let first_row = v.read_with(cx, |v, _| v.rows[0].track);
        assert_eq!(first_row, reordered.timeline.tracks()[0].id);
        assert_eq!(first_row, b);
        // A different session with equal counts drops the selection, hover and collapse.
        let other = stress(2, 1, 1);
        assert_eq!(other.timeline.slice_count(), 2);
        assert_ne!(other.session, reordered.session);
        let (log, _sub) = log_events(cx, &v);
        v.update(cx, |v, cx| {
            let track = v.rows[0].track;
            v.collapsed.insert(track);
            v.selected = Some(SliceKey {
                track,
                depth: 0,
                start: 0,
            });
            v.hover = v.selected;
            v.set_snapshot(other, cx);
            assert!(v.selected.is_none() && v.hover.is_none() && v.collapsed.is_empty());
        });
        assert_eq!(log.borrow().last(), Some(&TraceEvent::Hover(None)));
        assert_eq!(notified.get(), 3);
    }

    #[test]
    fn ruler_ticks_are_round_and_labelled() {
        let vp = Viewport::fit(0, 1_000_000_000);
        let ticks = ruler_ticks(vp, 1000.0);
        assert_eq!(ticks[0].1, "0.0 s");
        assert_eq!(ticks[1].1, "0.1 s");
        assert!((ticks[1].0 - 100.0).abs() < 0.5);
        assert_eq!(nice_step(130.0), 200);
        assert_eq!(nice_step(4.0e6), 5_000_000);
    }
}
