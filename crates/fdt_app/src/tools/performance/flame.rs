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
            bounds,
            period_us,
        }
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

/// Lays `spans` (sorted by start) out in `width` pixels for `viewport`, `rows` rows deep.
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
        let depth = s.depth as usize;
        if depth >= rows {
            continue;
        }
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
        self.selected = self.selected.filter(|&i| i < self.data.spans.len());
        self.hover = None;
        cx.notify();
    }

    fn fit(&mut self) {
        let (a, z) = self.data.bounds;
        self.viewport = Viewport::fit(a, z);
        self.fitted = true;
    }

    pub fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.fit();
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
        let depth = (y / f32::from(layout::FLAME_ROW)).floor() as usize;
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
        let (viewport, selected) = (self.viewport, self.selected);
        let size_cell = self.size.clone();
        let (accent, muted, faint, ink, border) =
            (t.accent, t.text_muted, t.text_faint, t.bg, t.text);
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
                let (boxes, labels) = flame_layout(&data.spans, &hue, viewport, width, rows);
                let colour = |h: Hue| match h {
                    Hue::User => accent,
                    Hue::Dart => muted,
                    Hue::Native => faint,
                };
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
                            color: Hsla::from(ink),
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
                        // There is nothing to scroll vertically, so both axes pan in time.
                        ZoomPan::PanX(d) | ZoomPan::PanY(d) => this.pan_px(f32::from(d), cx),
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
        let (quads, labels) = flame_layout(&spans, &hues, vp, 1400.0, 60);
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
        let (quads, labels) = flame_layout(&spans, &hues, vp, 1000.0, 5);
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
        let (quads, _) = flame_layout(&spans, &hues, vp, 1000.0, 5);
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].span, 2);
        assert!(quads[0].x >= 0.0 && quads[0].x + quads[0].w <= 1000.0);
        // Rows deeper than the canvas are not drawn.
        let (quads, _) = flame_layout(&spans, &hues, Viewport::fit(0, 1_000_000), 1000.0, 1);
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
}
