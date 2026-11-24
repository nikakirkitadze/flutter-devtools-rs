//! The memory chart: heap used and capacity, external memory and the process's resident size over
//! time, with a tick for every garbage collection and a marker for every heap snapshot.
//!
//! [`chart_layout`] is a pure function from the store and a viewport to per-pixel columns, GC
//! ticks and markers; [`chart_quads`] turns that into rectangles. [`MemoryChart`] is a cached
//! entity that paints them in one layer and takes the input. Its tooltips are the parent page's:
//! the chart only says what is under the pointer ([`ChartTip`]).

use super::heap_number;
use crate::nav::{Location, Page, Selection, TimeRange, Tool};
use crate::session_model::SessionModel;
use crate::shell::strip::{MIN_DRAG_PX, format_offset, session_span};
use crate::tools::OpenLocation;
use crate::tools::network::model::{format_bytes, format_duration_ns};
use crate::tools::performance::trace_tab::Rect;
use fdt_store::heap::HeapId;
use fdt_store::memory::MemorySnapshot;
use fdt_store::time::SessionTime;
use fdt_ui::canvas::{
    CANVAS_MAX_LABELS, CANVAS_MAX_QUADS, Label, Quad, ZoomPan, paint_labels, paint_quads,
    zoom_pan_listeners,
};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{Palette, alpha, font, layout, row, space};
use fdt_ui::viewport::Viewport;
use gpui_kit::{
    Bounds, Context, CursorStyle, DispatchPhase, Entity, EventEmitter, FocusHandle, HitboxBehavior,
    Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, SharedString, Styled as _,
    Subscription, TestSupportExt as _, Window, canvas, div, point, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// Quads kept apart from the per-column ones: grid lines, snapshot markers and the drag band.
const RESERVED_QUADS: usize = 256;
/// A column is drawn with at most five quads (capacity, used, external, RSS and a GC tick).
const QUADS_PER_COLUMN: usize = 5;
/// The most columns a chart has; a wider chart draws wider columns.
pub const MAX_COLUMNS: usize = (CANVAS_MAX_QUADS - RESERVED_QUADS) / QUADS_PER_COLUMN;
/// The most snapshot markers drawn.
const MAX_MARKERS: usize = 100;
/// The height one row of the scale takes.
const ROW_PX: f32 = 32.0; // token-check: allow: scale density, not a visual size
/// How far from a marker or a tick, in pixels, a pointer still hits it.
const HIT_SLOP_PX: f32 = 3.0; // token-check: allow: pointer tolerance, not a visual size
/// The width of a GC tick or a snapshot marker.
const LINE_PX: f32 = 1.0; // token-check: allow: hairline, not a visual size
/// How much one press of the zoom keys changes the scale.
const KEY_ZOOM: f32 = 1.5; // token-check: allow: zoom step, not a visual size
/// The size before the first paint.
const FIRST_SIZE: (f32, f32) = (900.0, 160.0); // token-check: allow: stand-in until the first paint
/// The room a snapshot marker's label needs before the next marker.
const MARKER_LABEL_PX: f32 = 32.0; // token-check: allow: label room, not a visual size

/// What the chart says when there are no samples.
pub const EMPTY_TEXT: &str =
    "No memory samples yet. Memory is sampled twice a second while an app is connected.";

/// The top of the plot: below the GC ticks.
fn plot_top() -> f32 {
    f32::from(layout::GC_MAJOR_TICK_HEIGHT)
}

/// Round steps for a scale that tops out at `max`: the smallest of 1, 2 or 5 times a power of two
/// kilobytes (so also megabytes and gigabytes) that gives at most `rows` ticks, as `(bytes,
/// label)` pairs, from the lowest.
pub fn byte_ticks(max: u64, rows: usize) -> Vec<(u64, String)> {
    if max == 0 || rows == 0 {
        return Vec::new();
    }
    let mut steps: Vec<u64> = (0..53u32)
        .flat_map(|n| [1u64, 2, 5].map(|m| m.saturating_mul(1 << n).saturating_mul(1024)))
        .collect();
    steps.sort_unstable();
    let step = steps
        .into_iter()
        .find(|&s| max / s <= rows as u64)
        .unwrap_or(u64::MAX);
    (1..=max / step)
        .map(|i| i * step)
        .map(|v| (v, format_bytes(v)))
        .collect()
}

/// The largest values within one pixel column of the chart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    /// The column's index from the left edge, counted in columns.
    pub col: u32,
    pub used: u64,
    pub capacity: u64,
    pub external: u64,
    pub rss: Option<u64>,
}

impl Column {
    fn of(s: &fdt_store::memory::MemorySample) -> Self {
        Self {
            col: 0,
            used: s.heap_used,
            capacity: s.heap_capacity,
            external: s.external,
            rss: s.rss,
        }
    }

    fn merge(&mut self, s: &fdt_store::memory::MemorySample) {
        self.used = self.used.max(s.heap_used);
        self.capacity = self.capacity.max(s.heap_capacity);
        self.external = self.external.max(s.external);
        self.rss = match (self.rss, s.rss) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    fn peak(&self) -> u64 {
        self.used
            .max(self.capacity)
            .max(self.external)
            .max(self.rss.unwrap_or(0))
    }
}

/// Where everything of the chart is, in pixels from its left edge (y from its top).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartGeometry {
    /// The width of the chart.
    pub width: f32,
    pub height: f32,
    /// The width of one column: 1 px, or more when the chart is wider than [`MAX_COLUMNS`].
    pub pitch: f32,
    pub columns: Vec<Column>,
    /// `(x, major, index)`: a tick for the GCs of one column, major if any of them is, and the
    /// index of the first major one (else the first) in the store.
    pub gcs: Vec<(f32, bool, usize)>,
    /// A marker for each heap snapshot in view.
    pub heaps: Vec<(f32, HeapId)>,
    /// `(y, label)` of the scale.
    pub ticks: Vec<(f32, String)>,
    /// The value at the top of the scale.
    pub y_max: u64,
}

impl ChartGeometry {
    /// The y of `value` on the scale.
    pub fn y_of(&self, value: u64) -> f32 {
        let top = plot_top();
        let h = (self.height - top).max(1.0);
        let frac = value as f64 / self.y_max.max(1) as f64;
        (self.height - frac.min(1.0) as f32 * h).clamp(top, self.height)
    }

    fn column_at(&self, x: f32) -> Option<&Column> {
        let col = (x / self.pitch.max(f32::MIN_POSITIVE)).floor();
        if col < 0.0 {
            return None;
        }
        let col = col as u32;
        let i = self.columns.partition_point(|c| c.col < col);
        self.columns.get(i).filter(|c| c.col == col)
    }
}

/// Lays the chart out for `vp` in a chart of `size` (width, height). Work is proportional to the
/// width, the samples and GC records in view, and the snapshots; never to the whole session.
pub fn chart_layout(mem: &MemorySnapshot, vp: Viewport, size: (f32, f32)) -> ChartGeometry {
    let (width, height) = size;
    let ok = width.is_finite() && height.is_finite() && width >= 1.0 && height > plot_top();
    let samples = mem.samples();
    let Some(last) = samples.len().checked_sub(1).and_then(|i| samples.get(i)) else {
        return ChartGeometry::default();
    };
    if !ok {
        return ChartGeometry::default();
    }
    let n = (width.floor() as usize).clamp(1, MAX_COLUMNS);
    let pitch = width / n as f32;
    let (start, end) = vp.range();
    let col_of = |t: SessionTime| -> i64 { (vp.x_of(t.0, width) / pitch).floor() as i64 };
    let in_view = TimeRange::new(SessionTime(start), SessionTime(end));
    let mut geo = ChartGeometry {
        width,
        height,
        pitch,
        ..ChartGeometry::default()
    };

    // The last sample's value holds until the next one; past the last sample there is nothing.
    let range = mem.sample_range(in_view);
    let mut held = range
        .start
        .checked_sub(1)
        .and_then(|i| samples.get(i))
        .map(Column::of);
    let end_col = if last.time.0 < start {
        -1
    } else {
        col_of(last.time).min(n as i64 - 1)
    };
    let mut next = range.start;
    for c in 0..=end_col.max(-1) {
        let mut col = held;
        while next < range.end {
            let Some(s) = samples.get(next) else { break };
            if col_of(s.time).clamp(0, n as i64 - 1) > c {
                break;
            }
            match col.as_mut() {
                Some(col) => col.merge(s),
                None => col = Some(Column::of(s)),
            }
            held = Some(Column::of(s));
            next += 1;
        }
        if let Some(mut col) = col {
            col.col = c as u32;
            geo.columns.push(col);
        }
    }
    geo.y_max = geo
        .columns
        .iter()
        .map(Column::peak)
        .max()
        .unwrap_or(0)
        .max(1);

    let rows = ((height - plot_top()) / ROW_PX).floor().max(1.0) as usize;
    geo.ticks = byte_ticks(geo.y_max, rows)
        .into_iter()
        .map(|(v, label)| (geo.y_of(v), label))
        .collect();

    // One tick per column of GCs.
    let mut current: Option<(i64, bool, usize)> = None;
    let flush = |cur: Option<(i64, bool, usize)>, geo: &mut ChartGeometry| {
        if let Some((c, major, i)) = cur {
            geo.gcs.push((c as f32 * pitch, major, i));
        }
    };
    for i in mem.gc_range(in_view) {
        let Some(g) = mem.gcs().get(i) else { break };
        let c = col_of(g.time).clamp(0, n as i64 - 1);
        match current.as_mut() {
            Some((cc, major, first)) if *cc == c => {
                if g.kind.is_major() && !*major {
                    *major = true;
                    *first = i;
                }
            }
            _ => {
                flush(current.take(), &mut geo);
                current = Some((c, g.kind.is_major(), i));
            }
        }
    }
    flush(current, &mut geo);

    geo.heaps = mem
        .heaps()
        .iter()
        .filter_map(|h| {
            let t = h.time.unwrap_or(h.requested);
            in_view
                .contains(t)
                .then(|| (vp.x_of(t.0, width).clamp(0.0, width), h.id))
        })
        .take(MAX_MARKERS)
        .collect();
    geo
}

/// What a quad is; the page maps it to a colour token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartHue {
    Grid,
    Capacity,
    Used,
    External,
    Rss,
    GcMinor,
    GcMajor,
    Marker,
    Drag,
}

/// The rectangles of the chart, in paint order, and never more than `CANVAS_MAX_QUADS`. `drag` is
/// the `(x0, x1)` of a drag band in progress.
pub fn chart_quads(geo: &ChartGeometry, drag: Option<(f32, f32)>) -> Vec<(Rect, ChartHue)> {
    let mut out = Vec::with_capacity(geo.columns.len() * 4 + geo.gcs.len() + 32);
    for (y, _) in &geo.ticks {
        out.push((
            Rect {
                x: 0.0,
                y: *y,
                w: geo.width,
                h: LINE_PX,
            },
            ChartHue::Grid,
        ));
    }
    let area = |out: &mut Vec<(Rect, ChartHue)>, value: fn(&Column) -> u64, hue| {
        for c in &geo.columns {
            let y = geo.y_of(value(c));
            out.push((
                Rect {
                    x: c.col as f32 * geo.pitch,
                    y,
                    w: geo.pitch,
                    h: (geo.height - y).max(LINE_PX),
                },
                hue,
            ));
        }
    };
    area(&mut out, |c| c.capacity, ChartHue::Capacity);
    area(&mut out, |c| c.used, ChartHue::Used);
    // A line joins the previous column's value when the columns are neighbours.
    let line = |out: &mut Vec<(Rect, ChartHue)>, value: fn(&Column) -> Option<u64>, hue| {
        let mut prev: Option<(u32, f32)> = None;
        for c in &geo.columns {
            let Some(v) = value(c) else {
                prev = None;
                continue;
            };
            let y = geo.y_of(v);
            let from = prev.filter(|p| p.0 + 1 == c.col).map_or(y, |p| p.1);
            let (lo, hi) = (y.min(from), y.max(from));
            out.push((
                Rect {
                    x: c.col as f32 * geo.pitch,
                    y: lo,
                    w: geo.pitch,
                    h: (hi - lo).max(LINE_PX),
                },
                hue,
            ));
            prev = Some((c.col, y));
        }
    };
    line(&mut out, |c| Some(c.external), ChartHue::External);
    line(&mut out, |c| c.rss, ChartHue::Rss);
    for (x, major, _) in &geo.gcs {
        let (h, hue) = if *major {
            (layout::GC_MAJOR_TICK_HEIGHT, ChartHue::GcMajor)
        } else {
            (layout::GC_TICK_HEIGHT, ChartHue::GcMinor)
        };
        out.push((
            Rect {
                x: *x,
                y: 0.0,
                w: geo.pitch.max(LINE_PX),
                h: f32::from(h),
            },
            hue,
        ));
    }
    for (x, _) in &geo.heaps {
        out.push((
            Rect {
                x: *x,
                y: 0.0,
                w: LINE_PX,
                h: geo.height,
            },
            ChartHue::Marker,
        ));
    }
    if let Some((a, b)) = drag {
        let (x0, x1) = (a.min(b).max(0.0), a.max(b).min(geo.width));
        out.push((
            Rect {
                x: x0,
                y: 0.0,
                w: (x1 - x0).max(0.0),
                h: geo.height,
            },
            ChartHue::Drag,
        ));
    }
    out.truncate(CANVAS_MAX_QUADS);
    out
}

/// The text of the chart: the scale and a `#N` for each snapshot marker with room before the next.
/// `(x, y, text, max width)`.
pub fn chart_labels(geo: &ChartGeometry) -> Vec<(f32, f32, String, f32, bool)> {
    let mut out = Vec::new();
    for (y, text) in &geo.ticks {
        out.push((f32::from(space::S4), *y, text.clone(), f32::MAX, false));
    }
    for (i, (x, id)) in geo.heaps.iter().enumerate() {
        let room = geo
            .heaps
            .get(i + 1)
            .map_or(geo.width, |next| next.0)
            .min(geo.width)
            - x;
        if room >= MARKER_LABEL_PX {
            out.push((
                *x + f32::from(space::S4),
                plot_top(),
                format!("#{}", heap_number(*id)),
                room - f32::from(space::S4),
                true,
            ));
        }
    }
    out.truncate(CANVAS_MAX_LABELS);
    out
}

/// What is under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartHit {
    /// A heap snapshot's marker.
    Heap(HeapId),
    /// A GC tick, by index into the store's GC records.
    Gc(usize),
    /// The sample in effect there, by index into the store's samples.
    Sample(usize),
}

/// The thing at `(x, y)`: a marker first, then a GC tick, then the sample in effect.
pub fn hit_at(
    geo: &ChartGeometry,
    mem: &MemorySnapshot,
    vp: Viewport,
    x: f32,
    y: f32,
) -> Option<ChartHit> {
    if !(x.is_finite() && y.is_finite()) || x < 0.0 || x > geo.width {
        return None;
    }
    if let Some((_, id)) = geo
        .heaps
        .iter()
        .find(|(mx, _)| (mx + LINE_PX / 2.0 - x).abs() <= HIT_SLOP_PX)
    {
        return Some(ChartHit::Heap(*id));
    }
    if let Some((_, _, i)) = geo.gcs.iter().find(|(gx, major, _)| {
        let h = if *major {
            layout::GC_MAJOR_TICK_HEIGHT
        } else {
            layout::GC_TICK_HEIGHT
        };
        y <= f32::from(h) && x >= gx - HIT_SLOP_PX && x <= gx + geo.pitch.max(LINE_PX) + HIT_SLOP_PX
    }) {
        return Some(ChartHit::Gc(*i));
    }
    geo.column_at(x)?;
    let t = SessionTime(vp.time_at(x, geo.width));
    let upto = mem.sample_range(TimeRange::new(SessionTime(0), t)).end;
    upto.checked_sub(1).map(ChartHit::Sample)
}

/// The tooltip of `hit`.
pub fn tooltip_text(mem: &MemorySnapshot, hit: ChartHit) -> Option<String> {
    match hit {
        ChartHit::Heap(id) => {
            mem.heap(id)?;
            Some(format!("Heap snapshot #{}", heap_number(id)))
        }
        ChartHit::Gc(i) => {
            let g = mem.gcs().get(i)?;
            let dur = g.dur_us.map_or_else(
                || "duration not known".to_string(),
                |us| format_duration_ns(u64::from(us) * 1_000),
            );
            Some(format!(
                "{} \u{b7} {} \u{b7} {} \u{b7} heap after {}",
                g.kind.label(),
                g.reason.label(),
                dur,
                format_bytes(g.heap_used)
            ))
        }
        ChartHit::Sample(i) => {
            let s = mem.samples().get(i)?;
            let mut text = format!(
                "{} \u{b7} heap {} of {} \u{b7} external {}",
                format_offset(s.time),
                format_bytes(s.heap_used),
                format_bytes(s.heap_capacity),
                format_bytes(s.external)
            );
            if let Some(rss) = s.rss {
                text.push_str(&format!(" \u{b7} RSS {}", format_bytes(rss)));
            }
            Some(text)
        }
    }
}

/// The legend: the latest values. `None` without samples.
pub fn legend_text(mem: &MemorySnapshot) -> Option<String> {
    let s = mem.samples().get(mem.samples().len().checked_sub(1)?)?;
    let mut text = format!(
        "Heap {} of {} \u{b7} External {}",
        format_bytes(s.heap_used),
        format_bytes(s.heap_capacity),
        format_bytes(s.external)
    );
    if let Some(rss) = s.rss {
        text.push_str(&format!(" \u{b7} RSS {}", format_bytes(rss)));
    }
    Some(text)
}

fn hue_colour(t: &Palette, hue: ChartHue) -> Hsla {
    match hue {
        ChartHue::Grid => Hsla::from(t.border),
        ChartHue::Capacity => Hsla::from(t.accent).opacity(alpha::AREA),
        ChartHue::Used => Hsla::from(t.accent),
        ChartHue::External => Hsla::from(t.lane_state),
        ChartHue::Rss => Hsla::from(t.lane_network),
        ChartHue::GcMinor => Hsla::from(t.text_muted),
        ChartHue::GcMajor => Hsla::from(t.warning),
        ChartHue::Marker => Hsla::from(t.text),
        ChartHue::Drag => t.accent.with_alpha(alpha::SELECTION),
    }
}

/// What the pointer is over, for the page's tooltip. `None` clears it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartTip(pub Option<(String, Point<Pixels>)>);

impl EventEmitter<ChartTip> for MemoryChart {}
impl EventEmitter<OpenLocation> for MemoryChart {}

#[derive(Clone, Copy, Debug)]
struct Drag {
    start_x: f32,
    x: f32,
}

pub struct MemoryChart {
    session: Entity<SessionModel>,
    snap: Arc<fdt_store::Snapshot>,
    range: Option<TimeRange>,
    selection: Option<Selection>,
    viewport: Viewport,
    /// The view shows its range or the whole session: it has not been zoomed or panned.
    fitted: bool,
    /// The view's right edge follows the latest data.
    follow: bool,
    size: Rc<Cell<(f32, f32)>>,
    laid_for: (f32, f32),
    geo: Arc<ChartGeometry>,
    drag: Option<Drag>,
    hover: Option<(f32, f32, Point<Pixels>)>,
    hit: Option<ChartHit>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl MemoryChart {
    pub fn new(session: Entity<SessionModel>, focus: FocusHandle, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, _, cx| this.take_snapshot(cx));
        let snap = session.read(cx).snapshot().clone();
        let mut chart = Self {
            session,
            snap,
            range: None,
            selection: None,
            viewport: Viewport::fit(0, 0),
            fitted: true,
            follow: true,
            size: Rc::new(Cell::new(FIRST_SIZE)),
            laid_for: (0.0, 0.0),
            geo: Arc::new(ChartGeometry::default()),
            drag: None,
            hover: None,
            hit: None,
            focus,
            _observe: observe,
        };
        chart.fit();
        chart.sync(cx);
        chart
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn fitted(&self) -> bool {
        self.fitted
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    pub fn geometry(&self) -> &Arc<ChartGeometry> {
        &self.geo
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn hit(&self) -> Option<ChartHit> {
        self.hit
    }

    pub fn width(&self) -> f32 {
        self.size.get().0.max(1.0)
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
        self.follow = self.range.is_none();
    }

    /// The shared range and selection of the current location. The view fits the range, or the
    /// whole session without one.
    pub fn set_location(
        &mut self,
        range: Option<TimeRange>,
        selection: Option<Selection>,
        cx: &mut Context<Self>,
    ) {
        self.selection = selection;
        if self.range != range {
            self.range = range;
            self.fit();
            self.sync(cx);
        }
    }

    fn take_snapshot(&mut self, cx: &mut Context<Self>) {
        let snap = self.session.read(cx).snapshot().clone();
        if snap.session != self.snap.session {
            self.snap = snap;
            self.drag = None;
            self.clear_hover(cx);
            self.fit();
        } else {
            self.snap = snap;
            if self.follow && self.range.is_none() {
                let (_, latest) = self.bounds();
                if self.fitted {
                    self.fit();
                } else {
                    self.viewport.start_ns = latest.saturating_sub(self.viewport.span_ns);
                }
            }
        }
        self.sync(cx);
    }

    /// Lays the chart out again; repaints only when the result differs.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let size = self.size.get();
        self.laid_for = size;
        let geo = chart_layout(&self.snap.memory, self.viewport, size);
        if *self.geo != geo {
            self.geo = Arc::new(geo);
            self.resolve_hover(cx);
            cx.notify();
        }
    }

    // Zoom and pan.

    pub fn key_zoom(&mut self, z: crate::tools::KeyZoom, cx: &mut Context<Self>) {
        match z {
            crate::tools::KeyZoom::In => self.zoom_about(self.width() / 2.0, KEY_ZOOM, cx),
            crate::tools::KeyZoom::Out => self.zoom_about(self.width() / 2.0, 1.0 / KEY_ZOOM, cx),
            crate::tools::KeyZoom::Fit => self.zoom_to_fit(cx),
        }
    }

    pub fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
        self.sync(cx);
        cx.notify();
    }

    pub fn zoom_pan(&mut self, action: ZoomPan, cx: &mut Context<Self>) {
        match action {
            ZoomPan::Zoom { x, factor } => self.zoom_about(f32::from(x), factor, cx),
            // There is nothing to scroll vertically, so both axes pan in time.
            ZoomPan::PanX(d) | ZoomPan::PanY(d) => self.pan_px(f32::from(d), cx),
        }
    }

    pub fn zoom_about(&mut self, x: f32, factor: f32, cx: &mut Context<Self>) {
        let before = self.viewport;
        let (w, bounds) = (self.width(), self.bounds());
        self.viewport.zoom_about(x, w, factor, bounds);
        self.moved_view(before, cx);
    }

    /// Pans by `px` pixels; positive shows later time.
    pub fn pan_px(&mut self, px: f32, cx: &mut Context<Self>) {
        let before = self.viewport;
        let (w, bounds) = (self.width(), self.bounds());
        self.viewport.pan_px(px, w, bounds);
        self.moved_view(before, cx);
    }

    fn moved_view(&mut self, before: Viewport, cx: &mut Context<Self>) {
        if self.viewport == before {
            return;
        }
        self.fitted = false;
        // Panning back to the latest data follows it again.
        self.follow = self.range.is_none() && self.viewport.range().1 >= self.bounds().1;
        self.sync(cx);
        cx.notify();
    }

    // Hover and input, in pixels from the chart's top-left corner.

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        self.hover = None;
        if self.hit.take().is_some() {
            cx.emit(ChartTip(None));
        }
    }

    fn resolve_hover(&mut self, cx: &mut Context<Self>) {
        let Some((x, y, at)) = self.hover else { return };
        self.point_at(x, y, at, cx);
    }

    fn point_at(&mut self, x: f32, y: f32, at: Point<Pixels>, cx: &mut Context<Self>) {
        let mem = &self.snap.memory;
        let hit = hit_at(&self.geo, mem, self.viewport, x, y);
        let text = hit.and_then(|h| tooltip_text(mem, h));
        self.hit = hit.filter(|_| text.is_some());
        match text {
            Some(text) => cx.emit(ChartTip(Some((text, at)))),
            None => cx.emit(ChartTip(None)),
        }
    }

    pub fn press(&mut self, x: f32, y: f32, click_count: usize, cx: &mut Context<Self>) {
        // A double-click clears the range.
        if click_count >= 2 {
            self.drag = None;
            self.open(None, cx);
            return;
        }
        let _ = y;
        self.drag = Some(Drag { start_x: x, x });
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
        if inside {
            self.hover = Some((x, y, at));
            self.point_at(x, y, at, cx);
        } else if self.hover.is_some() || self.hit.is_some() {
            self.clear_hover(cx);
        }
    }

    pub fn released(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        cx.notify();
        let _ = y;
        if (x - d.start_x).abs() < MIN_DRAG_PX {
            // A click: on a snapshot's marker it opens the snapshot.
            let hit = hit_at(
                &self.geo,
                &self.snap.memory,
                self.viewport,
                d.start_x,
                plot_top(),
            );
            if let Some(ChartHit::Heap(id)) = hit {
                cx.emit(OpenLocation(Location {
                    page: Page::Tool(Tool::Memory),
                    selection: Some(Selection::Heap(id)),
                    range: self.range,
                }));
            }
            return;
        }
        let w = self.width();
        let time = |px: f32| self.viewport.time_at(px.clamp(0.0, w), w);
        let range = TimeRange::new(SessionTime(time(d.start_x)), SessionTime(time(x)));
        self.open(Some(range), cx);
    }

    /// Asks the shell to show the Memory tool over `range` (or the whole session).
    fn open(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        cx.emit(OpenLocation(Location {
            page: Page::Tool(Tool::Memory),
            selection: self.selection,
            range,
        }));
    }
}

impl Render for MemoryChart {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::MemoryChart, 1);
        // The chart was resized since the last layout.
        let dims = self.size.get();
        if (dims.0 - self.laid_for.0).abs() >= 1.0 || (dims.1 - self.laid_for.1).abs() >= 1.0 {
            self.laid_for = dims;
            self.geo = Arc::new(chart_layout(&self.snap.memory, self.viewport, dims));
        }
        let t = *cx.tokens();
        let geo = self.geo.clone();
        let drag = self.drag.map(|d| (d.start_x, d.x));
        let view = cx.entity();
        let size_cell = self.size.clone();
        let legend = legend_text(&self.snap.memory);
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
                let quads: Vec<Quad> = chart_quads(&geo, drag)
                    .into_iter()
                    .map(|(r, hue)| Quad {
                        bounds: Bounds::new(
                            point(ox + px(r.x), oy + px(r.y)),
                            size(px(r.w), px(r.h)),
                        ),
                        color: hue_colour(&t, hue),
                    })
                    .collect();
                paint_quads(window, bounds, &quads);
                let line = font::LABEL * font::LINE_HEIGHT_UI;
                let labels: Vec<Label> = chart_labels(&geo)
                    .into_iter()
                    .map(|(x, y, text, w, marker)| Label {
                        text: SharedString::from(text),
                        origin: point(
                            ox + px(x),
                            // The scale's labels sit on their line; a marker's below the ticks.
                            oy + px(y) - if marker { px(0.) } else { line },
                        ),
                        max_width: px(w).min(layout::LANE_LABEL_WIDTH),
                        color: if marker {
                            Hsla::from(t.text)
                        } else {
                            Hsla::from(t.text_faint)
                        },
                    })
                    .collect();
                paint_labels(window, cx, bounds, &labels);
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
                    window.capture_pointer(hit.id);
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
                    v.update(cx, |this, cx| {
                        this.released(f32::from(at.x), f32::from(at.y), cx)
                    });
                });
                let v = view.clone();
                zoom_pan_listeners(window, &hitbox, row::TABLE, move |action, _, cx| {
                    v.update(cx, |this, cx| this.zoom_pan(action, cx));
                });
            },
        )
        .size_full();
        let t = *cx.tokens();
        div()
            .id("memory-chart-canvas")
            .test_support()
            .relative()
            .size_full()
            .child(surface)
            .children(legend.map(|text| {
                div()
                    .id("memory-chart-legend")
                    .test_support()
                    .absolute()
                    .top(layout::GC_MAJOR_TICK_HEIGHT)
                    .right(space::S8)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(text)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{snapshot, writer};
    use fdt_store::memory::{GcKind, GcReason, GcRecord, MemorySample};

    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000 * MS;
    const MB: u64 = 1024 * 1024;

    fn sample(ms: u64, used: u64, cap: u64, ext: u64, rss: Option<u64>) -> MemorySample {
        MemorySample {
            time: SessionTime(ms * MS),
            heap_used: used,
            heap_capacity: cap,
            external: ext,
            rss,
        }
    }

    fn gc(ms: u64, kind: GcKind) -> GcRecord {
        GcRecord {
            time: SessionTime(ms * MS),
            dur_us: Some(1_200),
            kind,
            reason: GcReason::NewSpace,
            heap_used: 30 * MB,
            heap_capacity: 40 * MB,
            external: MB,
        }
    }

    #[test]
    fn byte_tick_cases() {
        assert!(byte_ticks(0, 4).is_empty());
        assert!(byte_ticks(10 * MB, 0).is_empty());
        let t = byte_ticks(100 * MB, 4);
        let labels: Vec<_> = t.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["32.0 MB", "64.0 MB", "96.0 MB"]);
        let t = byte_ticks(8 * MB, 4);
        let labels: Vec<_> = t.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["2.0 MB", "4.0 MB", "6.0 MB", "8.0 MB"]);
        // Small and huge maxima still give round steps within the row count.
        for max in [
            1u64,
            1_000,
            5_000,
            123_456,
            7 * MB + 1,
            3 * 1024 * MB,
            u64::MAX,
        ] {
            let t = byte_ticks(max, 5);
            assert!(t.len() <= 5, "{max}: {}", t.len());
            assert!(t.iter().all(|(v, _)| *v <= max));
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
            if let Some((step, _)) = t.first() {
                assert!(
                    [1u64, 2, 5].iter().any(|m| {
                        let k = step / 1024 / m;
                        step % (1024 * m) == 0 && k.is_power_of_two()
                    }),
                    "{step} is not 1, 2 or 5 x 2^n KB"
                );
            }
        }
    }

    /// A 30-minute session: a sample twice a second, a GC every 10 ms, a major one every 1,000.
    fn thirty_minutes() -> std::sync::Arc<fdt_store::Snapshot> {
        snapshot(|w, run| {
            for i in 0..3_600u64 {
                w.push_memory_sample(
                    run,
                    sample(
                        i * 500,
                        (20 + i % 40) * MB,
                        60 * MB,
                        (i % 7) * MB,
                        Some(200 * MB + i * 1000),
                    ),
                );
            }
            for i in 0..180_000u64 {
                let kind = if i % 1_000 == 999 {
                    GcKind::MarkSweep
                } else {
                    GcKind::Scavenge
                };
                w.push_gc(run, gc(i * 10, kind));
            }
        })
    }

    #[test]
    fn layout_fits_the_width_and_the_quad_cap() {
        let snap = thirty_minutes();
        let mem = &snap.memory;
        let vp = Viewport::fit(0, 1_800 * SEC);
        for width in [300.0, 2_000.0, 3_999.0, 5_000.0, 8_000.0] {
            let geo = chart_layout(mem, vp, (width, 160.0));
            let cols = (width as usize).min(MAX_COLUMNS);
            assert!(geo.columns.len() <= cols, "{width}: {}", geo.columns.len());
            assert!(geo.gcs.len() <= cols, "{width}: {}", geo.gcs.len());
            assert!(geo.columns.len() >= cols * 9 / 10, "the chart is filled");
            assert!(geo.gcs.len() >= cols * 9 / 10, "{width}: a tick per column");
            let quads = chart_quads(&geo, Some((10.0, 90.0)));
            assert!(
                quads.len() <= CANVAS_MAX_QUADS,
                "{width}: {} quads",
                quads.len()
            );
            assert!(chart_labels(&geo).len() <= CANVAS_MAX_LABELS);
            assert!(geo.columns.windows(2).all(|w| w[0].col < w[1].col));
            assert!(geo.gcs.windows(2).all(|w| w[0].0 < w[1].0));
        }
        // The scale tops out at the largest RSS in view.
        let geo = chart_layout(mem, vp, (2_000.0, 160.0));
        assert_eq!(geo.y_max, 200 * MB + 3_599 * 1000);
        // Zoomed in to a window with a few samples, columns hold their values across the gaps.
        let vp = Viewport::fit(100 * SEC, 102 * SEC);
        let geo = chart_layout(mem, vp, (1_000.0, 160.0));
        assert!(geo.columns.len() >= 990);
        assert!(geo.gcs.len() <= 201);
    }

    #[test]
    fn major_gcs_are_marked() {
        let snap = snapshot(|w, run| {
            w.push_memory_sample(run, sample(0, MB, 2 * MB, 0, None));
            w.push_memory_sample(run, sample(10_000, MB, 2 * MB, 0, None));
            // One column holds three GCs, one of them major; another holds a lone scavenge.
            w.push_gc(run, gc(1_000, GcKind::Scavenge));
            w.push_gc(run, gc(1_001, GcKind::MarkCompact));
            w.push_gc(run, gc(1_002, GcKind::Scavenge));
            w.push_gc(run, gc(5_000, GcKind::Scavenge));
        });
        let geo = chart_layout(&snap.memory, Viewport::fit(0, 10 * SEC), (100.0, 160.0));
        assert_eq!(geo.gcs.len(), 2);
        assert_eq!((geo.gcs[0].1, geo.gcs[0].2), (true, 1));
        assert!(!geo.gcs[1].1);
        let quads = chart_quads(&geo, None);
        let tick = |hue| {
            quads
                .iter()
                .find(|(_, h)| *h == hue)
                .map(|(r, _)| r.h)
                .expect("a tick")
        };
        assert_eq!(
            tick(ChartHue::GcMajor),
            f32::from(layout::GC_MAJOR_TICK_HEIGHT)
        );
        assert_eq!(tick(ChartHue::GcMinor), f32::from(layout::GC_TICK_HEIGHT));
        assert!(tick(ChartHue::GcMajor) > tick(ChartHue::GcMinor));
    }

    #[test]
    fn layout_is_finite_for_odd_inputs() {
        let snap = thirty_minutes();
        let vp = Viewport::fit(0, 1_800 * SEC);
        for size in [(0.0, 160.0), (f32::NAN, 160.0), (100.0, 0.0), (-5.0, -5.0)] {
            assert_eq!(
                chart_layout(&snap.memory, vp, size),
                ChartGeometry::default()
            );
        }
        let empty = snapshot(|_, _| {});
        let geo = chart_layout(&empty.memory, vp, (500.0, 160.0));
        assert!(geo.columns.is_empty() && geo.gcs.is_empty());
    }

    #[test]
    fn tooltip_text_cases() {
        let snap = snapshot(|w, run| {
            w.push_memory_sample(
                run,
                sample(65_000, 12 * MB, 24 * MB, 2 * MB, Some(300 * MB)),
            );
            w.push_memory_sample(run, sample(66_000, MB, 2 * MB, 0, None));
            w.push_gc(run, gc(1_000, GcKind::Scavenge));
            let mut unknown = gc(2_000, GcKind::MarkSweep);
            unknown.dur_us = None;
            unknown.reason = GcReason::OldSpace;
            w.push_gc(run, unknown);
        });
        let mem = &snap.memory;
        assert_eq!(
            tooltip_text(mem, ChartHit::Sample(0)).as_deref(),
            Some("1:05 \u{b7} heap 12.0 MB of 24.0 MB \u{b7} external 2.0 MB \u{b7} RSS 300.0 MB")
        );
        assert_eq!(
            tooltip_text(mem, ChartHit::Sample(1)).as_deref(),
            Some("1:06 \u{b7} heap 1.0 MB of 2.0 MB \u{b7} external 0 B")
        );
        assert_eq!(
            tooltip_text(mem, ChartHit::Gc(0)).as_deref(),
            Some("Scavenge \u{b7} New space full \u{b7} 1 ms \u{b7} heap after 30.0 MB")
        );
        assert_eq!(
            tooltip_text(mem, ChartHit::Gc(1)).as_deref(),
            Some(
                "Mark-sweep \u{b7} Old space full \u{b7} duration not known \u{b7} heap after 30.0 MB"
            )
        );
        assert_eq!(tooltip_text(mem, ChartHit::Sample(9)), None);
        assert_eq!(tooltip_text(mem, ChartHit::Heap(HeapId(0))), None);
    }

    #[test]
    fn legend_shows_latest_values() {
        let snap = snapshot(|w, run| {
            w.push_memory_sample(run, sample(0, MB, 2 * MB, 0, Some(100 * MB)));
            w.push_memory_sample(run, sample(500, 12 * MB, 24 * MB, 3 * MB, Some(300 * MB)));
        });
        assert_eq!(
            legend_text(&snap.memory).as_deref(),
            Some("Heap 12.0 MB of 24.0 MB \u{b7} External 3.0 MB \u{b7} RSS 300.0 MB")
        );
        let none = snapshot(|_, _| {});
        assert_eq!(legend_text(&none.memory), None);
    }

    #[test]
    fn the_hit_under_the_pointer() {
        let snap = snapshot(|w, run| {
            w.push_memory_sample(run, sample(0, MB, 2 * MB, 0, None));
            w.push_memory_sample(run, sample(10_000, 2 * MB, 4 * MB, 0, None));
            w.push_gc(run, gc(5_000, GcKind::Scavenge));
            w.heap_requested(run, "isolates/1", SessionTime(8_000 * MS));
        });
        let mem = &snap.memory;
        let vp = Viewport::fit(0, 10 * SEC);
        let geo = chart_layout(mem, vp, (100.0, 160.0));
        assert_eq!(geo.heaps.len(), 1);
        assert_eq!(
            hit_at(&geo, mem, vp, geo.heaps[0].0, 100.0),
            Some(ChartHit::Heap(HeapId(0)))
        );
        assert_eq!(hit_at(&geo, mem, vp, 50.5, 2.0), Some(ChartHit::Gc(0)));
        // Below a tick it is the sample in effect.
        assert_eq!(
            hit_at(&geo, mem, vp, 50.5, 100.0),
            Some(ChartHit::Sample(0))
        );
        assert_eq!(
            hit_at(&geo, mem, vp, 99.5, 100.0),
            Some(ChartHit::Sample(0))
        );
        assert_eq!(hit_at(&geo, mem, vp, -1.0, 100.0), None);
        assert_eq!(hit_at(&geo, mem, vp, f32::NAN, 100.0), None);
    }

    #[test]
    fn columns_hold_values_across_gaps() {
        let (mut w, run) = writer();
        w.push_memory_sample(run, sample(0, MB, 2 * MB, 0, None));
        w.push_memory_sample(run, sample(5_000, 3 * MB, 4 * MB, 0, None));
        let snap = w.publish();
        let geo = chart_layout(&snap.memory, Viewport::fit(0, 10 * SEC), (100.0, 160.0));
        // Samples at 0 s and 5 s: columns 0..=50 hold something, the rest is empty.
        assert_eq!(geo.columns.first().map(|c| c.col), Some(0));
        assert_eq!(geo.columns.last().map(|c| c.col), Some(50));
        assert_eq!(geo.columns.len(), 51);
        assert_eq!(geo.columns[10].used, MB);
        assert_eq!(geo.columns[50].used, 3 * MB);
        assert_eq!(geo.y_max, 4 * MB);
    }

    // The entity, in the shell.

    use crate::nav::Selection as Sel;
    use crate::shell::MainWindow;
    use crate::test_support::{
        FakeHandle, connected, exists, open_shell_fake, open_shell_snapshot,
    };
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext, px};

    fn tiny_heap(b: &mut fdt_analysis::heap::encode::SnapshotBuilder) {
        let class = b.class("Thing", "package:app/thing.dart", &[]);
        let o = b.object(class, 32, fdt_analysis::heap::format::Value::None, &[]);
        b.static_field("thing", o);
    }

    /// Ten seconds of samples, a GC and one heap snapshot (taken at 1 s).
    fn with_heap() -> std::sync::Arc<fdt_store::Snapshot> {
        snapshot(|w, run| {
            for i in 0..21u64 {
                w.push_memory_sample(
                    run,
                    sample(i * 500, (10 + i) * MB, 40 * MB, MB, Some(90 * MB)),
                );
            }
            w.push_gc(run, gc(3_000, GcKind::Scavenge));
            crate::test_support::heap_record(w, run, tiny_heap);
        })
    }

    fn settle(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        cx.run_until_parked();
    }

    fn show(shell: &Entity<MainWindow>, cx: &mut VisualTestContext) -> Entity<MemoryChart> {
        crate::test_support::press(cx, "cmd-3", "ctrl-3");
        settle(cx);
        shell.read_with(cx, |s, cx| s.memory_view().read(cx).chart().clone())
    }

    fn opened(cx: &mut VisualTestContext, shell: &Entity<MainWindow>) -> crate::nav::Location {
        shell.read_with(cx, |s, _| s.current().clone())
    }

    #[gpui_kit::test]
    fn snapshot_markers_open_the_snapshot(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, with_heap());
        let chart = show(&shell, cx);
        assert!(exists(cx, "memory-chart"));
        assert!(exists(cx, "memory-chart-legend"));
        let (x, hit) = chart.read_with(cx, |c, _| {
            let g = c.geometry();
            assert_eq!(g.heaps.len(), 1, "the marker is in view");
            (g.heaps[0].0, g.heaps[0].1)
        });
        assert_eq!(hit, HeapId(0));
        // Hovering says what it is; a click opens it in the Memory tool's Snapshot tab.
        let at = gpui_kit::point(px(30.), px(30.));
        chart.update(cx, |c, cx| c.moved(x, 50.0, at, false, true, cx));
        assert_eq!(
            chart.read_with(cx, |c, _| c.hit()),
            Some(ChartHit::Heap(HeapId(0)))
        );
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        assert_eq!(
            view.read_with(cx, |v, _| v.tooltip().map(str::to_string))
                .as_deref(),
            Some("Heap snapshot #1")
        );
        chart.update(cx, |c, cx| {
            c.press(x, 50.0, 1, cx);
            c.released(x, 50.0, cx);
        });
        cx.run_until_parked();
        let at = opened(cx, &shell);
        assert_eq!(at.page, Page::Tool(Tool::Memory));
        assert_eq!(at.selection, Some(Sel::Heap(HeapId(0))));
        assert_eq!(
            view.read_with(cx, |v, _| v.tab()),
            crate::tools::memory::MemoryTab::Snapshot
        );
        // A click on bare chart opens nothing.
        let before = shell.read_with(cx, |s, _| s.history().can_go_back());
        chart.update(cx, |c, cx| {
            c.press(300.0, 50.0, 1, cx);
            c.released(300.0, 50.0, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |s, _| s.history().can_go_back()),
            before
        );
    }

    #[gpui_kit::test]
    fn drag_sets_range(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, with_heap());
        let chart = show(&shell, cx);
        let w = chart.read_with(cx, |c, _| c.width());
        // A click that does not move sets nothing.
        chart.update(cx, |c, cx| {
            c.press(100.0, 50.0, 1, cx);
            c.released(101.0, 50.0, cx);
        });
        cx.run_until_parked();
        assert_eq!(opened(cx, &shell).range, None);
        // A drag across a quarter to a half of the chart does.
        let vp = chart.read_with(cx, |c, _| c.viewport());
        chart.update(cx, |c, cx| {
            c.press(w * 0.25, 50.0, 1, cx);
            assert!(c.dragging());
            c.moved(
                w * 0.5,
                50.0,
                gpui_kit::point(px(0.), px(0.)),
                true,
                true,
                cx,
            );
            c.released(w * 0.5, 50.0, cx);
        });
        cx.run_until_parked();
        let want = TimeRange::new(
            SessionTime(vp.time_at(w * 0.25, w)),
            SessionTime(vp.time_at(w * 0.5, w)),
        );
        let at = opened(cx, &shell);
        assert_eq!(at.range, Some(want));
        assert!(!chart.read_with(cx, |c, _| c.dragging()));
        // The chart now fits the range, and a double-click clears it.
        settle(cx);
        assert!(chart.read_with(cx, |c, _| c.fitted()));
        assert_eq!(
            chart.read_with(cx, |c, _| c.viewport().start_ns),
            want.start.0
        );
        chart.update(cx, |c, cx| c.press(100.0, 50.0, 2, cx));
        cx.run_until_parked();
        assert_eq!(opened(cx, &shell).range, None);
    }

    #[gpui_kit::test]
    fn zoom_is_clamped_and_finite(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, with_heap());
        let chart = show(&shell, cx);
        let fitted = chart.read_with(cx, |c, _| c.viewport());
        for factor in [1e30, 1e-30, f32::NAN, f32::INFINITY, 0.0, -2.0, 1e30, 1e30] {
            chart.update(cx, |c, cx| {
                c.zoom_pan(
                    ZoomPan::Zoom {
                        x: px(100.),
                        factor,
                    },
                    cx,
                );
            });
            let vp = chart.read_with(cx, |c, _| c.viewport());
            assert!(vp.span_ns >= fdt_ui::viewport::MIN_SPAN_NS, "{vp:?}");
            assert!(vp.span_ns <= fitted.span_ns * 12 / 10, "{vp:?}");
            let geo = chart.read_with(cx, |c, _| c.geometry().clone());
            assert!(geo.columns.len() <= MAX_COLUMNS);
            assert!(geo.y_max >= 1);
            assert!(chart_quads(&geo, None).iter().all(|(r, _)| r.x.is_finite()
                && r.y.is_finite()
                && r.w.is_finite()
                && r.h.is_finite()));
        }
        // The zoom keys and fit work, and fit returns to the whole session.
        crate::test_support::press(cx, "cmd-0", "ctrl-0");
        assert!(chart.read_with(cx, |c, _| c.fitted()));
        crate::test_support::press(cx, "cmd-=", "ctrl-=");
        assert!(!chart.read_with(cx, |c, _| c.fitted()));
        assert!(chart.read_with(cx, |c, _| c.viewport().span_ns) < fitted.span_ns);
        crate::test_support::press(cx, "cmd--", "ctrl--");
        crate::test_support::press(cx, "cmd-0", "ctrl-0");
        assert_eq!(chart.read_with(cx, |c, _| c.viewport()), fitted);
    }

    #[gpui_kit::test]
    fn follows_latest_until_panned(cx: &mut TestAppContext) {
        let (shell, cx, fake): (_, _, FakeHandle) = open_shell_fake(cx);
        fake.status.send(connected("demo", true)).unwrap();
        // One writer, so every snapshot belongs to the same session.
        let (mut w, run) = crate::test_support::writer();
        let mut pushed = 0u64;
        let mut grow = |n: u64| {
            for i in pushed..n {
                w.push_memory_sample(run, sample(i * 500, (10 + i) * MB, 80 * MB, 0, None));
            }
            pushed = n;
            w.publish()
        };
        fake.snaps.send(grow(40)).unwrap();
        let chart = show(&shell, cx);
        let end = |cx: &mut VisualTestContext| chart.read_with(cx, |c, _| c.viewport().range().1);
        assert!(chart.read_with(cx, |c, _| c.following() && c.fitted()));
        assert_eq!(end(cx), 19_500 * MS);
        fake.snaps.send(grow(60)).unwrap();
        settle(cx);
        assert_eq!(end(cx), 29_500 * MS, "a fitted chart grows with the data");
        // Zoomed in on the newest data it keeps the newest in view.
        chart.update(cx, |c, cx| c.zoom_about(c.width(), 4.0, cx));
        chart.update(cx, |c, cx| c.pan_px(1e9, cx));
        assert!(chart.read_with(cx, |c, _| c.following() && !c.fitted()));
        let span = chart.read_with(cx, |c, _| c.viewport().span_ns);
        fake.snaps.send(grow(80)).unwrap();
        settle(cx);
        assert_eq!(end(cx), 39_500 * MS);
        assert_eq!(chart.read_with(cx, |c, _| c.viewport().span_ns), span);
        // Panning back stops following: new data leaves the view where it is.
        chart.update(cx, |c, cx| c.pan_px(-200.0, cx));
        assert!(!chart.read_with(cx, |c, _| c.following()));
        let held = chart.read_with(cx, |c, _| c.viewport());
        fake.snaps.send(grow(100)).unwrap();
        settle(cx);
        assert_eq!(chart.read_with(cx, |c, _| c.viewport()), held);
        // Panning to the end follows again.
        chart.update(cx, |c, cx| c.pan_px(1e9, cx));
        assert!(chart.read_with(cx, |c, _| c.following()));
        fake.snaps.send(grow(120)).unwrap();
        settle(cx);
        assert_eq!(end(cx), 59_500 * MS);
    }

    #[gpui_kit::test]
    fn empty_state(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        crate::test_support::press(cx, "cmd-3", "ctrl-3");
        settle(cx);
        assert!(exists(cx, "memory-chart-empty"));
        assert!(!exists(cx, "memory-chart-legend"));
        assert_eq!(
            EMPTY_TEXT,
            "No memory samples yet. Memory is sampled twice a second while an app is connected."
        );
        // The first samples replace it.
        fake.snaps
            .send(snapshot(|w, run| {
                w.push_memory_sample(run, sample(0, MB, 2 * MB, 0, None));
            }))
            .unwrap();
        settle(cx);
        assert!(!exists(cx, "memory-chart-empty"));
        assert!(exists(cx, "memory-chart-legend"));
        let _ = shell;
    }
}
