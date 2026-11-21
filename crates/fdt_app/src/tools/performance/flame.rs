//! The CPU flame chart: one row per stack depth, one box per call, time on the x axis.
//!
//! The spans come from `fdt_analysis::cpu_tree::flame_spans`, computed off the UI thread by the
//! CPU tab. The chart is an entity of its own, drawn `.cached(..)`, and notified only when its
//! data, viewport or selection change. Hover goes to the page as an event, so the tooltip never
//! busts the chart's cache. All boxes go to one layer through `paint_quads`.

use crate::keymap::LANES_CONTEXT;
use fdt_analysis::cpu_tree::FlameSpan;
use fdt_analysis::user_code::is_user_library;
use fdt_store::Snapshot;
use fdt_store::cpu::{FunctionId, FunctionKind};
use fdt_ui::canvas::{
    CANVAS_MAX_LABELS, CANVAS_MAX_QUADS, Label, Quad, ZoomPan, paint_labels, paint_quads,
    zoom_pan_listeners,
};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{layout, row, space};
use fdt_ui::viewport::Viewport;
use gpui_kit::{
    Bounds, Context, CursorStyle, DispatchPhase, EventEmitter, FocusHandle, HitboxBehavior, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Pixels, Point, Render, Styled as _, TestSupportExt as _, Window, canvas,
    point, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// The most spans one flame chart holds. More are left out, and the page says so.
pub const MAX_SPANS: usize = 100_000;
/// With no range set, the flame chart shows the session's last 10 seconds: the whole session
/// would be cut at `MAX_SPANS` and show only its start.
pub const RECENT_NS: u64 = 10_000_000_000;
/// What the chart says while it shows only the last [`RECENT_NS`].
pub const RECENT_NOTE: &str = "Showing the last 10 seconds. Select a range to see another part.";
/// How much one press of the zoom keys changes the scale.
const KEY_ZOOM: f32 = 1.5; // token-check: allow: zoom step, not a visual size
/// The chart's size before it has been drawn once.
const FIRST_WIDTH_PX: f32 = 800.0; // token-check: allow: stand-in until the first paint
const FIRST_HEIGHT_PX: f32 = 400.0; // token-check: allow: stand-in until the first paint
/// Boxes this wide or wider leave a one pixel gap to the next box.
const GAP_FROM_PX: f32 = 4.0; // token-check: allow: threshold, not a visual size

/// How a function is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    /// The app's own code.
    User,
    /// Dart code of the SDK, Flutter and packages.
    Dart,
    Native,
}

/// The fill of a box of hue `h`.
pub fn flame_fill(t: &fdt_ui::tokens::Palette, h: Hue) -> fdt_ui::tokens::Color {
    match h {
        Hue::User => t.accent,
        Hue::Dart => t.text_muted,
        Hue::Native => t.text_faint,
    }
}

/// The colour of a function's name on a box of hue `h`: `bg` or `text`, whichever reads better
/// on the fill (as in the Raw trace).
pub fn flame_label_colour(t: &fdt_ui::tokens::Palette, h: Hue) -> fdt_ui::tokens::Color {
    let fill = flame_fill(t, h);
    if t.bg.contrast(fill) >= t.text.contrast(fill) {
        t.bg
    } else {
        t.text
    }
}

/// The hue of function `id`.
pub fn hue_of(snap: &Snapshot, id: FunctionId) -> Hue {
    let info = snap.cpu.function(id);
    match info.kind {
        FunctionKind::Native => Hue::Native,
        FunctionKind::Dart if is_user_library(info.library.as_deref()) => Hue::User,
        FunctionKind::Dart => Hue::Dart,
    }
}

/// What the flame chart shows: the spans, sorted by start, and an index of them by row.
#[derive(Debug, Default)]
pub struct FlameData {
    pub spans: Vec<FlameSpan>,
    /// Span indices per depth, in start order. Spans of one depth do not overlap.
    pub by_depth: Vec<Vec<u32>>,
    /// The hue of every function, by function id.
    pub hues: Vec<Hue>,
    /// Spans were left out at `MAX_SPANS`.
    pub truncated: bool,
    /// No range was set and the session is longer than [`RECENT_NS`]: only its end is shown.
    pub recent: bool,
    /// The time the chart fits to, in nanoseconds.
    pub bounds: (u64, u64),
    pub period_us: u32,
}

impl FlameData {
    pub fn new(
        snap: &Snapshot,
        spans: Vec<FlameSpan>,
        truncated: bool,
        bounds: (u64, u64),
        period_us: u32,
    ) -> Self {
        let mut by_depth: Vec<Vec<u32>> = Vec::new();
        for (i, s) in spans.iter().enumerate() {
            let d = s.depth as usize;
            if by_depth.len() <= d {
                by_depth.resize_with(d + 1, Vec::new);
            }
            by_depth[d].push(i as u32);
        }
        let hues = (0..snap.cpu.function_count())
            .map(|i| hue_of(snap, FunctionId(i as u32)))
            .collect();
        Self {
            spans,
            by_depth,
            hues,
            truncated,
            recent: false,
            bounds,
            period_us,
        }
    }

    /// How many rows deep the deepest stack is.
    pub fn depth_count(&self) -> usize {
        self.by_depth.len()
    }

    /// The span with this depth, start and function, if the data has one. Spans of a depth are
    /// sorted by start, so this is a binary search.
    pub fn find(&self, depth: u16, start: u64, function: FunctionId) -> Option<usize> {
        let row = self.by_depth.get(depth as usize)?;
        let at = row.partition_point(|&i| self.spans[i as usize].start.0 < start);
        row[at..]
            .iter()
            .map(|&i| i as usize)
            .take_while(|&i| self.spans[i].start.0 == start)
            .find(|&i| self.spans[i].function == function)
    }

    /// The span at `depth` that holds time `t_ns`.
    pub fn span_at(&self, depth: usize, t_ns: u64) -> Option<usize> {
        let row = self.by_depth.get(depth)?;
        let after = row.partition_point(|&i| self.spans[i as usize].start.0 <= t_ns);
        let i = *row.get(after.checked_sub(1)?)? as usize;
        (self.spans[i].end.0 > t_ns).then_some(i)
    }
}

/// A box to draw, in pixels from the chart's top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlameQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub hue: Hue,
    /// The index into the spans.
    pub span: u32,
}

/// A label to draw: where it starts and how much room it has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlameLabel {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub span: u32,
}

/// Lays `spans` (sorted by start) out in `width` pixels for `viewport`: `rows` rows, from depth
/// `first_row`.
///
/// A span narrower than a pixel shares its pixel column with its neighbours at the same depth, so
/// there are at most `width` boxes a row, and never more than `CANVAS_MAX_QUADS` in all. Labels
/// go to boxes with room for one, at most `CANVAS_MAX_LABELS`.
pub fn flame_layout(
    spans: &[FlameSpan],
    hue_of: &dyn Fn(FunctionId) -> Hue,
    viewport: Viewport,
    width: f32,
    rows: usize,
    first_row: usize,
) -> (Vec<FlameQuad>, Vec<FlameLabel>) {
    let mut quads = Vec::new();
    let mut labels = Vec::new();
    if width <= 0.0 || rows == 0 {
        return (quads, labels);
    }
    let (_, end_ns) = viewport.range();
    let row_h = f32::from(layout::FLAME_ROW);
    let gap = f32::from(layout::BORDER);
    let min_label = f32::from(layout::LABEL_MIN_WIDTH);
    // The last pixel column drawn per depth, so sub-pixel spans do not pile up.
    let mut last_col: Vec<i64> = vec![i64::MIN; rows];
    for (i, s) in spans.iter().enumerate() {
        if s.start.0 > end_ns {
            break;
        }
        let Some(depth) = (s.depth as usize)
            .checked_sub(first_row)
            .filter(|&d| d < rows)
        else {
            continue;
        };
        let x0 = viewport.x_of(s.start.0, width);
        let x1 = viewport.x_of(s.end.0, width);
        if x1 < 0.0 || x0 > width {
            continue;
        }
        let (mut x, mut w) = (x0, x1 - x0);
        if w < 1.0 {
            let col = x0.floor() as i64;
            if last_col[depth] == col {
                continue;
            }
            last_col[depth] = col;
            w = 1.0;
        } else if w >= GAP_FROM_PX {
            w -= gap;
        }
        if x < 0.0 {
            w += x;
            x = 0.0;
        }
        w = w.min(width - x);
        if w <= 0.0 {
            continue;
        }
        let y = depth as f32 * row_h;
        if quads.len() < CANVAS_MAX_QUADS {
            quads.push(FlameQuad {
                x,
                y,
                w,
                h: row_h - gap,
                hue: hue_of(s.function),
                span: i as u32,
            });
        }
        if w >= min_label && labels.len() < CANVAS_MAX_LABELS {
            labels.push(FlameLabel {
                x: x + f32::from(space::S4),
                y,
                w: w - f32::from(space::S4),
                span: i as u32,
            });
        }
        if quads.len() >= CANVAS_MAX_QUADS && labels.len() >= CANVAS_MAX_LABELS {
            break;
        }
    }
    (quads, labels)
}

/// What the flame chart tells the CPU tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlameEvent {
    /// The pointer is over this span, at this window position; `None` once it left.
    Hover(Option<(usize, Point<Pixels>)>),
    /// A box was clicked; `None` for a click on nothing.
    Select(Option<usize>),
}

pub struct FlameChart {
    data: Arc<FlameData>,
    snap: Arc<Snapshot>,
    viewport: Viewport,
    /// Whether the view fits the data, so new data refits it.
    fitted: bool,
    selected: Option<usize>,
    /// What the selection is, so it can be found again in new data.
    selected_key: Option<(u16, u64, FunctionId)>,
    /// The first row shown, when the stacks are deeper than the canvas.
    row_offset: usize,
    /// Scrolling that has not yet added up to a whole row.
    row_rem: f32,
    hover: Option<usize>,
    /// The canvas size at its last paint.
    size: Rc<Cell<(f32, f32)>>,
    focus: FocusHandle,
}

impl EventEmitter<FlameEvent> for FlameChart {}

impl FlameChart {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            data: Arc::new(FlameData::default()),
            snap: Arc::new(Snapshot::empty()),
            viewport: Viewport::fit(0, 0),
            fitted: true,
            selected: None,
            selected_key: None,
            row_offset: 0,
            row_rem: 0.0,
            hover: None,
            size: Rc::new(Cell::new((FIRST_WIDTH_PX, FIRST_HEIGHT_PX))),
            focus: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Shows `data`. The view refits when it was fitted, or when `refit` asks (another range).
    pub fn set_data(
        &mut self,
        snap: Arc<Snapshot>,
        data: Arc<FlameData>,
        refit: bool,
        cx: &mut Context<Self>,
    ) {
        self.snap = snap;
        self.data = data;
        if refit || self.fitted {
            self.fit();
        }
        // The selection is a span of the old data: find the same call again, or drop it.
        let had = self.selected_key.is_some();
        self.selected = self
            .selected_key
            .and_then(|(d, start, f)| self.data.find(d, start, f));
        if self.selected.is_none() {
            self.selected_key = None;
        }
        self.row_offset = self.row_offset.min(self.max_row_offset());
        self.hover = None;
        if had {
            cx.emit(FlameEvent::Select(self.selected));
        }
        cx.notify();
    }

    fn fit(&mut self) {
        let (a, z) = self.data.bounds;
        self.viewport = Viewport::fit(a, z);
        self.fitted = true;
    }

    /// A zoom command, from the chart's own keys or handed over by the shell (the palette).
    pub fn key_zoom(&mut self, z: crate::tools::KeyZoom, cx: &mut Context<Self>) {
        match z {
            crate::tools::KeyZoom::In => self.zoom_key(KEY_ZOOM, cx),
            crate::tools::KeyZoom::Out => self.zoom_key(1.0 / KEY_ZOOM, cx),
            crate::tools::KeyZoom::Fit => self.zoom_to_fit(cx),
        }
    }

    pub fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
        self.row_offset = 0;
        self.row_rem = 0.0;
        cx.notify();
    }

    pub fn zoom_about(&mut self, x: f32, factor: f32, cx: &mut Context<Self>) {
        let width = self.size.get().0;
        self.viewport.zoom_about(x, width, factor, self.data.bounds);
        self.fitted = false;
        cx.notify();
    }

    /// Pans by `px` pixels; positive shows later time.
    pub fn pan_px(&mut self, px: f32, cx: &mut Context<Self>) {
        let width = self.size.get().0;
        self.viewport.pan_px(px, width, self.data.bounds);
        self.fitted = false;
        cx.notify();
    }

    /// How many rows fit in the canvas.
    fn visible_rows(&self) -> usize {
        (self.size.get().1 / f32::from(layout::FLAME_ROW)).floor() as usize
    }

    fn max_row_offset(&self) -> usize {
        self.data
            .depth_count()
            .saturating_sub(self.visible_rows().max(1))
    }

    /// A vertical scroll of `px` pixels: moves the rows when the stacks are deeper than the
    /// canvas (positive shows deeper rows), else pans time.
    pub fn scroll_y(&mut self, px: f32, cx: &mut Context<Self>) {
        let max = self.max_row_offset();
        if max == 0 {
            self.pan_px(px, cx);
            return;
        }
        let rows = px / f32::from(layout::FLAME_ROW) + self.row_rem;
        let whole = rows.trunc();
        self.row_rem = rows - whole;
        let target = (self.row_offset as f32 + whole).clamp(0.0, max as f32) as usize;
        if target != self.row_offset {
            self.row_offset = target;
            cx.notify();
        }
    }

    fn zoom_key(&mut self, factor: f32, cx: &mut Context<Self>) {
        let half = self.size.get().0 / 2.0;
        self.zoom_about(half, factor, cx);
    }

    /// The span under `(x, y)` pixels from the top-left corner.
    fn span_at(&self, x: f32, y: f32) -> Option<usize> {
        let (width, _) = self.size.get();
        if x < 0.0 || y < 0.0 || x > width {
            return None;
        }
        let depth = (y / f32::from(layout::FLAME_ROW)).floor() as usize + self.row_offset;
        self.data.span_at(depth, self.viewport.time_at(x, width))
    }

    fn moved(&mut self, x: f32, y: f32, at: Point<Pixels>, inside: bool, cx: &mut Context<Self>) {
        let span = if inside { self.span_at(x, y) } else { None };
        if span != self.hover {
            self.hover = span;
            cx.emit(FlameEvent::Hover(span.map(|s| (s, at))));
        }
    }

    fn clicked(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let span = self.span_at(x, y);
        if span != self.selected {
            self.selected = span;
            self.selected_key = span.map(|i| {
                let s = &self.data.spans[i];
                (s.depth, s.start.0, s.function)
            });
            cx.notify();
        }
        cx.emit(FlameEvent::Select(span));
    }
}

impl Render for FlameChart {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.tokens();
        let view = cx.entity();
        let (data, snap) = (self.data.clone(), self.snap.clone());
        let (viewport, selected, first_row) = (self.viewport, self.selected, self.row_offset);
        let size_cell = self.size.clone();
        let border = t.text;
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
                let width = f32::from(bounds.size.width);
                let rows =
                    (f32::from(bounds.size.height) / f32::from(layout::FLAME_ROW)).floor() as usize;
                let hue =
                    |f: FunctionId| data.hues.get(f.0 as usize).copied().unwrap_or(Hue::Native);
                let (boxes, labels) =
                    flame_layout(&data.spans, &hue, viewport, width, rows, first_row);
                let colour = |h: Hue| flame_fill(&t, h);
                let mut quads: Vec<Quad> = boxes
                    .iter()
                    .map(|b| Quad {
                        bounds: Bounds::new(
                            point(ox + px(b.x), oy + px(b.y)),
                            size(px(b.w), px(b.h)),
                        ),
                        color: Hsla::from(colour(b.hue)),
                    })
                    .collect();
                if let Some(b) = selected.and_then(|s| boxes.iter().find(|b| b.span as usize == s))
                {
                    let e = layout::BORDER;
                    let c = Hsla::from(border);
                    let (x, y, w, h) = (ox + px(b.x), oy + px(b.y), px(b.w), px(b.h));
                    for r in [
                        Bounds::new(point(x, y), size(w, e)),
                        Bounds::new(point(x, y + h - e), size(w, e)),
                        Bounds::new(point(x, y), size(e, h)),
                        Bounds::new(point(x + w - e, y), size(e, h)),
                    ] {
                        quads.push(Quad {
                            bounds: r,
                            color: c,
                        });
                    }
                }
                paint_quads(window, bounds, &quads);
                let line = fdt_ui::tokens::font::LABEL * fdt_ui::tokens::font::LINE_HEIGHT_UI;
                let drawn: Vec<Label> = labels
                    .iter()
                    .filter_map(|l| {
                        let s = data.spans.get(l.span as usize)?;
                        let name = snap.cpu.function(s.function).name.to_string();
                        Some(Label {
                            text: name.into(),
                            origin: point(
                                ox + px(l.x),
                                oy + px(l.y) + (layout::FLAME_ROW - line) / 2.0,
                            ),
                            max_width: px(l.w),
                            color: Hsla::from(flame_label_colour(&t, hue(s.function))),
                        })
                    })
                    .collect();
                paint_labels(window, cx, bounds, &drawn);
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
                        this.clicked(f32::from(at.x), f32::from(at.y), cx)
                    });
                });
                let (v, hit) = (view.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let inside = hit.is_hovered(window);
                    let at = e.position - hit.bounds.origin;
                    v.update(cx, |this, cx| {
                        this.moved(f32::from(at.x), f32::from(at.y), e.position, inside, cx)
                    });
                });
                let v = view.clone();
                zoom_pan_listeners(window, &hitbox, row::TABLE, move |action, _, cx| {
                    v.update(cx, |this, cx| match action {
                        ZoomPan::Zoom { x, factor } => this.zoom_about(f32::from(x), factor, cx),
                        ZoomPan::PanX(d) => this.pan_px(f32::from(d), cx),
                        // Deep stacks scroll their rows; shallow ones have nothing to scroll.
                        ZoomPan::PanY(d) => this.scroll_y(f32::from(d), cx),
                    });
                });
            },
        )
        .size_full();
        gpui_kit::div()
            .id("cpu-flame")
            .test_support()
            .key_context(LANES_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(
                cx.listener(|this, _: &crate::actions::ZoomIn, _, cx| this.zoom_key(KEY_ZOOM, cx)),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ZoomOut, _, cx| {
                this.zoom_key(1.0 / KEY_ZOOM, cx)
            }))
            .on_action(
                cx.listener(|this, _: &crate::actions::ZoomToFit, _, cx| this.zoom_to_fit(cx)),
            )
            .child(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::time::SessionTime;

    fn span(depth: u16, start_us: u64, end_us: u64, f: u32) -> FlameSpan {
        FlameSpan {
            depth,
            start: SessionTime(start_us * 1_000),
            end: SessionTime(end_us * 1_000),
            function: FunctionId(f),
        }
    }

    fn hues(_: FunctionId) -> Hue {
        Hue::Dart
    }

    #[test]
    fn flame_paints_at_most_the_quad_cap() {
        // 120k one-microsecond spans over 60 depths, 1 ms apart per depth step: far more
        // pixel columns than the cap allows at 1400 px wide and 60 rows.
        let mut spans = Vec::new();
        for i in 0..120_000u64 {
            spans.push(span((i % 60) as u16, i * 10, i * 10 + 5, 0));
        }
        let end = 120_000 * 10;
        let vp = Viewport::fit(0, end * 1_000);
        let (quads, labels) = flame_layout(&spans, &hues, vp, 1400.0, 60, 0);
        assert!(quads.len() <= CANVAS_MAX_QUADS, "{} quads", quads.len());
        assert!(!quads.is_empty());
        assert!(labels.len() <= CANVAS_MAX_LABELS);
        // Sub-pixel spans share a column per depth.
        assert!(quads.len() <= 1400 * 60);
    }

    #[test]
    fn layout_labels_wide_boxes_and_clips_to_the_view() {
        let spans = vec![
            span(0, 0, 500, 1),
            span(1, 100, 110, 2),
            span(0, 500, 1_000, 3),
        ];
        let vp = Viewport::fit(0, 1_000_000);
        let (quads, labels) = flame_layout(&spans, &hues, vp, 1000.0, 5, 0);
        assert_eq!(quads.len(), 3);
        // Only the 500 px boxes have room for a label.
        assert_eq!(
            labels.iter().map(|l| l.span).collect::<Vec<_>>(),
            vec![0, 2]
        );
        // A viewport on the right half clips the first box away and the second to the edge.
        let vp = Viewport {
            start_ns: 750_000,
            span_ns: 500_000,
        };
        let (quads, _) = flame_layout(&spans, &hues, vp, 1000.0, 5, 0);
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].span, 2);
        assert!(quads[0].x >= 0.0 && quads[0].x + quads[0].w <= 1000.0);
        // Rows deeper than the canvas are not drawn.
        let (quads, _) = flame_layout(&spans, &hues, Viewport::fit(0, 1_000_000), 1000.0, 1, 0);
        assert!(quads.iter().all(|q| q.y < f32::from(layout::FLAME_ROW)));
    }

    #[test]
    fn span_at_finds_the_box_under_a_time() {
        let spans = vec![span(0, 0, 100, 1), span(1, 10, 20, 2), span(1, 30, 40, 3)];
        let (snap, _) = (Arc::new(Snapshot::empty()), ());
        let mut data = FlameData::new(&snap, spans, false, (0, 100_000), 1000);
        data.hues = vec![Hue::Dart; 4];
        assert_eq!(data.span_at(0, 50_000), Some(0));
        assert_eq!(data.span_at(1, 15_000), Some(1));
        assert_eq!(data.span_at(1, 25_000), None);
        assert_eq!(data.span_at(1, 35_000), Some(2));
        assert_eq!(data.span_at(2, 15_000), None);
    }

    fn data_of(spans: Vec<FlameSpan>) -> Arc<FlameData> {
        let snap = Snapshot::empty();
        Arc::new(FlameData::new(&snap, spans, false, (0, 100_000_000), 1000))
    }

    fn chart(cx: &mut gpui_kit::TestAppContext) -> gpui_kit::Entity<FlameChart> {
        use gpui_kit::AppContext as _;
        cx.new(FlameChart::new)
    }

    fn ms_span(depth: u16, start_ms: u64, end_ms: u64, f: u32) -> FlameSpan {
        FlameSpan {
            depth,
            start: SessionTime(start_ms * 1_000_000),
            end: SessionTime(end_ms * 1_000_000),
            function: FunctionId(f),
        }
    }

    #[gpui_kit::test]
    fn selection_survives_refresh_or_clears(cx: &mut gpui_kit::TestAppContext) {
        let chart = chart(cx);
        let snap = Arc::new(Snapshot::empty());
        let first = data_of(vec![ms_span(0, 0, 100, 0), ms_span(1, 10, 50, 1)]);
        chart.update(cx, |c, cx| {
            c.set_data(snap.clone(), first, true, cx);
            // Depth 1, 30 ms in (the chart is 800 px for 100 ms).
            c.clicked(240.0, f32::from(layout::FLAME_ROW) * 1.5, cx);
            assert_eq!(c.selected(), Some(1));
        });
        // New data puts the same call at another index: the selection follows it.
        let moved = data_of(vec![
            ms_span(0, 0, 100, 0),
            ms_span(1, 5, 8, 9),
            ms_span(1, 10, 55, 1),
        ]);
        chart.update(cx, |c, cx| {
            c.set_data(snap.clone(), moved, false, cx);
            assert_eq!(c.selected(), Some(2));
        });
        // New data without that call clears it.
        let gone = data_of(vec![ms_span(0, 0, 100, 0), ms_span(1, 12, 20, 1)]);
        chart.update(cx, |c, cx| {
            c.set_data(snap, gone, false, cx);
            assert_eq!(c.selected(), None);
        });
    }

    #[gpui_kit::test]
    fn deep_stack_scrolls_vertically(cx: &mut gpui_kit::TestAppContext) {
        let chart = chart(cx);
        let spans: Vec<FlameSpan> = (0..120).map(|d| ms_span(d, 0, 100, d as u32)).collect();
        let data = data_of(spans);
        chart.update(cx, |c, cx| {
            c.set_data(Arc::new(Snapshot::empty()), data.clone(), true, cx);
            let visible = c.visible_rows();
            assert!(visible < 120);
            let (quads, _) = flame_layout(&data.spans, &hues, c.viewport(), 800.0, visible, 0);
            assert!(quads.iter().all(|q| q.span < visible as u32));
            let start = c.viewport();
            c.scroll_y(f32::from(layout::FLAME_ROW) * 500.0, cx);
            // Clamped to the last full page; time did not move.
            assert_eq!(c.row_offset, 120 - visible);
            assert_eq!(c.viewport(), start);
            let (quads, _) = flame_layout(
                &data.spans,
                &hues,
                c.viewport(),
                800.0,
                visible,
                c.row_offset,
            );
            assert!(quads.iter().any(|q| q.span == 119), "the leaf row is drawn");
            assert!(quads.iter().all(|q| q.span as usize >= c.row_offset));
            // A click on the bottom row picks the leaf.
            let y = (visible as f32 - 0.5) * f32::from(layout::FLAME_ROW);
            c.clicked(400.0, y, cx);
            assert_eq!(c.selected(), Some(119));
            c.scroll_y(-f32::from(layout::FLAME_ROW) * 500.0, cx);
            assert_eq!(c.row_offset, 0);
        });
    }

    #[gpui_kit::test]
    fn shallow_stack_pans_time_on_vertical_scroll(cx: &mut gpui_kit::TestAppContext) {
        let chart = chart(cx);
        let data = data_of(vec![ms_span(0, 0, 100, 0), ms_span(1, 10, 50, 1)]);
        chart.update(cx, |c, cx| {
            c.set_data(Arc::new(Snapshot::empty()), data, true, cx);
            c.zoom_about(0.0, 4.0, cx);
            let before = c.viewport();
            c.scroll_y(100.0, cx);
            assert_eq!(c.row_offset, 0);
            assert!(c.viewport().start_ns > before.start_ns, "later time shows");
        });
    }
    #[test]
    fn flame_label_colours_meet_contrast() {
        for (name, p) in [
            ("dark", &fdt_ui::tokens::DARK),
            ("light", &fdt_ui::tokens::LIGHT),
        ] {
            for h in [Hue::User, Hue::Dart, Hue::Native] {
                let c = flame_label_colour(p, h).contrast(flame_fill(p, h));
                assert!(c >= 4.5, "{name} {h:?}: {c:.2}");
            }
        }
    }
}
