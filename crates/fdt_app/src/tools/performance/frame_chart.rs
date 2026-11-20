//! The frame chart: one bar per frame, coloured by status, with the budget line, the selected
//! frame, a range overlay and the input that moves them.
//!
//! The chart is an entity of its own, drawn `.cached(..)` by the Performance page and notified
//! only when its frames, viewport, selection or hover change. Its hover tooltip lives in the
//! page (the chart reports the hovered frame as an event), so hovering never busts the cache of
//! anything else. All bars go to one layer through `paint_quads`.

use crate::keymap::FRAME_CHART_CONTEXT;
use crate::nav::TimeRange;
use crate::shell::strip::MIN_DRAG_PX;
use fdt_store::Snapshot;
use fdt_store::frames::{FrameStatus, FramesSnapshot, frame_status};
use fdt_ui::canvas::{Label, Quad, ZoomPan, paint_labels, paint_quads, zoom_pan_listeners};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, row, space};
use gpui_kit::{
    Bounds, Context, CursorStyle, DispatchPhase, EventEmitter, FocusHandle, HitboxBehavior, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, Styled as _, TestSupportExt as _,
    Window, canvas, point, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// The widest a bar gets when zoomed in.
const MAX_BAR_PX: f32 = 24.0; // token-check: allow: zoom limit, not a visual size
/// The bar height scale tops out at this many budgets.
const Y_BUDGETS: f32 = 3.0;
/// Bars this wide or wider leave a one pixel gap.
const GAP_FROM_PX: f32 = 4.0; // token-check: allow: threshold, not a visual size
/// The chart's width before it has been drawn once.
const FIRST_WIDTH_PX: f32 = 800.0; // token-check: allow: stand-in until the first paint

/// One drawn bar. `x` and `w` are in pixels from the chart's left edge; `h_frac` is the share of
/// the chart's height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarGeom {
    pub x: f32,
    pub w: f32,
    pub h_frac: f32,
    pub status: FrameStatus,
    /// The index into the rows the chart shows (the worst frame of a pixel column when zoomed
    /// out).
    pub row: usize,
    /// The bar is taller than the scale and was cut off.
    pub clipped: bool,
}

/// The frames of `rows` (frame seqs) from index `first`, as bars in `width_px`. At `bar_px`
/// below one, each pixel column shows the worst of its frames, so there are at most `width_px`
/// bars however many frames there are.
pub fn chart_bars(
    frames: &FramesSnapshot,
    rows: &[u32],
    first: usize,
    bar_px: f32,
    width_px: f32,
    budget_us: u32,
) -> Vec<BarGeom> {
    if rows.is_empty() || first >= rows.len() || bar_px <= 0.0 || width_px <= 0.0 {
        return Vec::new();
    }
    let top = Y_BUDGETS * budget_us.max(1) as f32;
    let bar = |x: f32, w: f32, row: usize, time_us: u32| {
        let h = time_us as f32 / top;
        BarGeom {
            x,
            w,
            h_frac: h.min(1.0),
            status: frame_status(time_us, budget_us),
            row,
            clipped: h > 1.0,
        }
    };
    let time_of = |row: usize| frames.get(rows[row] as usize).map(|f| f.time_us());
    if bar_px >= 1.0 {
        let n = ((width_px / bar_px).ceil() as usize).min(rows.len() - first);
        let w = if bar_px >= GAP_FROM_PX {
            bar_px - 1.0
        } else {
            bar_px
        };
        return (0..n)
            .filter_map(|k| {
                let row = first + k;
                Some(bar(k as f32 * bar_px, w, row, time_of(row)?))
            })
            .collect();
    }
    let mut out = Vec::new();
    for c in 0..width_px.ceil() as usize {
        let range = column_rows(first, bar_px, c, rows.len());
        if range.is_empty() {
            break;
        }
        if let Some((row, t)) = worst(&time_of, range) {
            out.push(bar(c as f32, 1.0, row, t));
        }
    }
    out
}

/// The rows pixel column `c` covers when each pixel is more than one frame.
fn column_rows(first: usize, bar_px: f32, c: usize, len: usize) -> std::ops::Range<usize> {
    let per = 1.0 / f64::from(bar_px);
    let lo = first + (c as f64 * per).floor() as usize;
    let hi = first + ((c + 1) as f64 * per).floor() as usize;
    lo.min(len)..hi.max(lo + 1).min(len)
}

/// The slowest row of `range` (the earliest among equals) and its time.
fn worst(
    time_of: &impl Fn(usize) -> Option<u32>,
    range: std::ops::Range<usize>,
) -> Option<(usize, u32)> {
    let mut best: Option<(usize, u32)> = None;
    for row in range {
        let t = time_of(row)?;
        if best.is_none_or(|(_, b)| t > b) {
            best = Some((row, t));
        }
    }
    best
}

/// What the chart tells the Performance page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChartEvent {
    /// A frame was picked, by a click or (`by_key`) an arrow key.
    Select { seq: usize, by_key: bool },
    /// A drag covered these frames, first to last.
    Range { first: usize, last: usize },
    /// The pointer is over a frame, at this window position; `None` when it left the chart.
    Hover(Option<(usize, Point<Pixels>)>),
    /// Following the newest frame started or stopped.
    Follow(bool),
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    start_x: f32,
    start_row: Option<usize>,
    row: Option<usize>,
    moved: bool,
}

pub struct FrameChart {
    snap: Arc<Snapshot>,
    /// The frame seqs shown, in order. Frames are only appended, so this only grows.
    rows: Arc<Vec<u32>>,
    first: usize,
    bar_px: f32,
    /// Panning that has not yet added up to a whole frame.
    pan_rem: f32,
    selected: Option<usize>,
    range: Option<TimeRange>,
    hover: Option<usize>,
    drag: Option<Drag>,
    follow: bool,
    /// The width of the canvas at its last paint.
    width: Rc<Cell<f32>>,
    focus: FocusHandle,
}

impl EventEmitter<ChartEvent> for FrameChart {}

impl FrameChart {
    pub fn new(snap: Arc<Snapshot>, cx: &mut Context<Self>) -> Self {
        let mut chart = Self {
            snap: Arc::new(Snapshot::empty()),
            rows: Arc::new(Vec::new()),
            first: 0,
            bar_px: f32::from(layout::FRAME_BAR_WIDTH),
            pan_rem: 0.0,
            selected: None,
            range: None,
            hover: None,
            drag: None,
            follow: true,
            width: Rc::new(Cell::new(FIRST_WIDTH_PX)),
            focus: cx.focus_handle(),
        };
        chart.take(snap);
        chart
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn first(&self) -> usize {
        self.first
    }

    pub fn bar_px(&self) -> f32 {
        self.bar_px
    }

    pub fn following(&self) -> bool {
        self.follow
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// How many frames fit across the chart.
    fn visible(&self) -> usize {
        ((self.width.get() / self.bar_px).floor() as usize).max(1)
    }

    fn max_first(&self) -> usize {
        self.rows.len().saturating_sub(self.visible())
    }

    /// Keeps `first` in range, and on the newest frames while following.
    fn settle(&mut self) {
        self.first = if self.follow {
            self.max_first()
        } else {
            self.first.min(self.max_first())
        };
    }

    /// Takes the frames of `snap`. Returns whether anything the chart draws changed.
    fn take(&mut self, snap: Arc<Snapshot>) -> bool {
        let len = snap.frames.len();
        let changed = snap.session != self.snap.session
            || len != self.rows.len()
            || snap.latest_budget_us() != self.snap.latest_budget_us();
        if snap.session != self.snap.session {
            self.rows = Arc::new(Vec::new());
            self.first = 0;
            self.follow = true;
            self.hover = None;
        }
        let rows = Arc::make_mut(&mut self.rows);
        rows.truncate(len);
        let have = rows.len() as u32;
        rows.extend(have..len as u32);
        self.snap = snap;
        self.settle();
        changed
    }

    /// Shows the frames of `snap`; the view stays on the newest while following.
    pub fn set_snapshot(&mut self, snap: Arc<Snapshot>, cx: &mut Context<Self>) {
        if self.take(snap) {
            cx.notify();
        }
    }

    /// Selects frame `seq` (from the location), scrolling it into view when it is off screen.
    pub fn set_selection(&mut self, seq: Option<usize>, cx: &mut Context<Self>) {
        if self.selected == seq {
            return;
        }
        self.selected = seq;
        if let Some(row) = seq {
            self.reveal(row, cx);
        }
        cx.notify();
    }

    /// The shared time range, drawn as a band over its frames.
    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            cx.notify();
        }
    }

    fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.follow != on {
            self.follow = on;
            cx.emit(ChartEvent::Follow(on));
        }
    }

    fn reveal(&mut self, row: usize, cx: &mut Context<Self>) {
        let before = self.first;
        if row < self.first {
            self.first = row;
        } else if row >= self.first + self.visible() {
            self.first = row + 1 - self.visible();
        }
        self.first = self.first.min(self.max_first());
        if self.first != before && self.first != self.max_first() {
            self.set_follow(false, cx);
        }
    }

    /// Pans by `px` pixels; positive shows later frames. Panning back stops following.
    pub fn pan_px(&mut self, px: f32, cx: &mut Context<Self>) {
        let frames = px / self.bar_px + self.pan_rem;
        let whole = frames.trunc();
        self.pan_rem = frames - whole;
        let target = (self.first as f64 + f64::from(whole)).max(0.0) as usize;
        let target = target.min(self.max_first());
        if target == self.first {
            return;
        }
        if target < self.first {
            self.set_follow(false, cx);
        }
        self.first = target;
        cx.notify();
    }

    /// Changes the bar width by `factor`, keeping the frame under `x` (pixels from the left
    /// edge) where it is.
    pub fn zoom_about(&mut self, x: f32, factor: f32, cx: &mut Context<Self>) {
        let min = f32::from(layout::FRAME_BAR_MIN_WIDTH);
        let new = (self.bar_px * factor).clamp(min, MAX_BAR_PX);
        if (new - self.bar_px).abs() < f32::EPSILON {
            return;
        }
        let under = self.first as f32 + x / self.bar_px;
        self.bar_px = new;
        self.pan_rem = 0.0;
        self.first = (under - x / new).max(0.0) as usize;
        self.first = self.first.min(self.max_first());
        cx.notify();
    }

    /// Jumps to the newest frame and follows it.
    pub fn latest(&mut self, cx: &mut Context<Self>) {
        self.set_follow(true, cx);
        self.settle();
        cx.notify();
    }

    /// Moves the selection one frame (the arrow keys).
    pub fn select_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let next = match self.selected {
            Some(s) if forward => (s + 1).min(len - 1),
            Some(s) => s.saturating_sub(1),
            None if forward => self.first,
            None => len - 1,
        };
        if self.selected == Some(next) {
            return;
        }
        self.selected = Some(next);
        self.reveal(next, cx);
        cx.emit(ChartEvent::Select {
            seq: self.rows[next] as usize,
            by_key: true,
        });
        cx.notify();
    }

    /// The row under `x` pixels from the left edge; the worst frame of the column when zoomed
    /// out.
    fn row_at(&self, x: f32) -> Option<usize> {
        if x < 0.0 {
            return None;
        }
        let len = self.rows.len();
        let frames = &self.snap.frames;
        if self.bar_px >= 1.0 {
            let row = self.first + (x / self.bar_px).floor() as usize;
            return (row < len).then_some(row);
        }
        let range = column_rows(self.first, self.bar_px, x.floor() as usize, len);
        let time_of = |row: usize| frames.get(self.rows[row] as usize).map(|f| f.time_us());
        worst(&time_of, range).map(|(row, _)| row)
    }

    /// Like `row_at`, but a position past the last frame (or left of the first) gives the nearest.
    fn row_clamped(&self, x: f32) -> Option<usize> {
        let w = self.width.get();
        self.row_at(x.clamp(0.0, (w - 1.0).max(0.0)))
            .or_else(|| (x >= 0.0).then(|| self.rows.len().checked_sub(1)).flatten())
    }

    fn press(&mut self, x: f32) {
        let row = self.row_at(x);
        self.drag = Some(Drag {
            start_x: x,
            start_row: row,
            row,
            moved: false,
        });
    }

    fn moved(
        &mut self,
        x: f32,
        at: Point<Pixels>,
        inside: bool,
        pressed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut d) = self.drag {
            if !pressed {
                self.drag = None;
                cx.notify();
                return;
            }
            d.moved |= (x - d.start_x).abs() >= MIN_DRAG_PX;
            d.row = self.row_clamped(x);
            self.drag = Some(d);
            cx.notify();
            return;
        }
        let row = if inside { self.row_at(x) } else { None };
        if row != self.hover {
            self.hover = row;
            cx.notify();
        }
        cx.emit(ChartEvent::Hover(row.map(|r| (r, at))));
    }

    fn released(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(d) = self.drag.take() else { return };
        cx.notify();
        if d.moved {
            let end = self.row_clamped(x).or(d.row);
            if let (Some(a), Some(b)) = (d.start_row, end) {
                cx.emit(ChartEvent::Range {
                    first: self.rows[a.min(b)] as usize,
                    last: self.rows[a.max(b)] as usize,
                });
            }
        } else if let Some(row) = d.start_row {
            self.selected = Some(row);
            cx.emit(ChartEvent::Select {
                seq: self.rows[row] as usize,
                by_key: false,
            });
        }
    }

    /// Rows the range band and the drag preview cover.
    fn band(&self) -> Option<(usize, usize)> {
        if let Some(d) = self.drag.filter(|d| d.moved) {
            let (a, b) = (d.start_row?, d.row?);
            return Some((a.min(b), a.max(b)));
        }
        let r = self.range?;
        let rows = self.snap.frames.range(r);
        (!rows.is_empty()).then(|| (rows.start, rows.end - 1))
    }
}

/// The budget as the chart labels it: `16.7 ms budget`.
pub fn budget_label(budget_us: u32) -> String {
    format!("{:.1} ms budget", budget_us as f64 / 1000.0)
}

impl Render for FrameChart {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::bump(crate::perf::Counter::FrameChart, 1);
        self.settle();
        let t = *cx.tokens();
        let view = cx.entity();
        let snap = self.snap.clone();
        let rows = self.rows.clone();
        let (first, bar_px) = (self.first, self.bar_px);
        let (selected, hover, band) = (self.selected, self.hover, self.band());
        let width_cell = self.width.clone();
        let budget = snap.latest_budget_us();
        let colors = (t.success, t.warning, t.danger);
        let (accent, text, faint) = (t.accent, t.text, t.text_faint);
        let prepaint_width = width_cell.clone();
        let surface = canvas(
            move |bounds: Bounds<Pixels>, window, cx| {
                let w = f32::from(bounds.size.width);
                if (prepaint_width.replace(w) - w).abs() >= 1.0 {
                    cx.notify(window.current_view());
                }
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds: Bounds<Pixels>, hitbox, window, cx| {
                let (ox, oy) = (bounds.origin.x, bounds.origin.y);
                let (w, h) = (bounds.size.width, bounds.size.height);
                let bars = chart_bars(&snap.frames, &rows, first, bar_px, f32::from(w), budget);
                let mut quads = Vec::with_capacity(bars.len() + 8);
                let column = |row: usize| {
                    let x = (row as f32 - first as f32) * bar_px;
                    (px(x), px(bar_px.max(1.0)))
                };
                if let Some((a, z)) = band {
                    let (x0, _) = column(a);
                    let (x1, w1) = column(z);
                    let (x0, x1) = (x0.max(Pixels::ZERO), (x1 + w1).min(w));
                    if x1 > x0 {
                        quads.push(Quad {
                            bounds: Bounds::new(point(ox + x0, oy), size(x1 - x0, h)),
                            color: accent.with_alpha(alpha::ACTIVE_ROW),
                        });
                    }
                }
                if let Some(row) = hover {
                    let (x, bw) = column(row);
                    quads.push(Quad {
                        bounds: Bounds::new(point(ox + x, oy), size(bw, h)),
                        color: text.with_alpha(alpha::ACTIVE_ROW),
                    });
                }
                for b in &bars {
                    let color = match b.status {
                        FrameStatus::Ok => colors.0,
                        FrameStatus::Slow => colors.1,
                        FrameStatus::Janky => colors.2,
                    };
                    let bh = h * b.h_frac;
                    quads.push(Quad {
                        bounds: Bounds::new(
                            point(ox + px(b.x), oy + h - bh),
                            size(px(b.w).max(layout::BORDER), bh),
                        ),
                        color: Hsla::from(color),
                    });
                }
                // The budget line, one third of the way up (the scale tops out at 3 budgets).
                let line_y = oy + h - h / Y_BUDGETS;
                quads.push(Quad {
                    bounds: Bounds::new(point(ox, line_y), size(w, layout::BORDER)),
                    color: Hsla::from(faint),
                });
                if let Some(row) = selected.filter(|&r| r >= first) {
                    let (x, bw) = column(row);
                    let b = layout::BORDER;
                    let rect = Bounds::new(point(ox + x, oy), size(bw.max(b * 2.0), h));
                    let c = Hsla::from(accent);
                    quads.push(Quad {
                        bounds: Bounds::new(rect.origin, size(rect.size.width, b)),
                        color: c,
                    });
                    quads.push(Quad {
                        bounds: Bounds::new(
                            point(rect.origin.x, rect.origin.y + h - b),
                            size(rect.size.width, b),
                        ),
                        color: c,
                    });
                    for edge in [rect.origin.x, rect.origin.x + rect.size.width - b] {
                        quads.push(Quad {
                            bounds: Bounds::new(point(edge, rect.origin.y), size(b, h)),
                            color: c,
                        });
                    }
                }
                paint_quads(window, bounds, &quads);
                let label_h = font::LABEL * font::LINE_HEIGHT_UI;
                paint_labels(
                    window,
                    cx,
                    bounds,
                    &[Label {
                        text: budget_label(budget).into(),
                        origin: point(ox + space::S8, line_y - label_h),
                        max_width: w - space::S16,
                        color: Hsla::from(faint),
                    }],
                );
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);

                let from_left = move |p: Pixels| f32::from(p - ox);
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
                    v.update(cx, |this, _| this.press(from_left(e.position.x)));
                });
                let (v, hit) = (view.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let inside = hit.is_hovered(window);
                    let pressed = e.pressed_button == Some(MouseButton::Left);
                    v.update(cx, |this, cx| {
                        this.moved(from_left(e.position.x), e.position, inside, pressed, cx)
                    });
                });
                let v = view.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left {
                        return;
                    }
                    v.update(cx, |this, cx| this.released(from_left(e.position.x), cx));
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
            .id("frame-chart")
            .test_support()
            .key_context(FRAME_CHART_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(
                cx.listener(|this, _: &crate::actions::PreviousFrame, _, cx| {
                    this.select_step(false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::NextFrame, _, cx| {
                    this.select_step(true, cx)
                }),
            )
            .child(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowPerformance;
    use crate::nav::{Page, Selection, Tool};
    use crate::perf::{Counter, count};
    use crate::test_support::{frame_at, open_shell_snapshot, snapshot};
    use crate::tools::performance::fixtures::{frames, push_frames};
    use fdt_store::frames::FrameStatus;
    use fdt_store::time::SessionTime;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px};

    const BUDGET: u32 = 16_667;

    fn open(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<FrameChart>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, frames(n));
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        frame(cx);
        let chart = shell.read_with(cx, |s, cx| s.performance_view().read(cx).chart().clone());
        (shell, chart, cx)
    }

    fn frame(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.render_frame(cx));
    }

    /// The window position of frame `i`'s bar at 6 px per bar and nothing scrolled.
    fn bar(cx: &mut VisualTestContext, i: usize) -> gpui_kit::Point<gpui_kit::Pixels> {
        let b = cx.update(|window, cx| {
            window.render_frame(cx);
            window.find("frame-chart").bounds()
        });
        point(
            b.origin.x + px(i as f32 * 6.0 + 3.0),
            b.origin.y + b.size.height / 2.0,
        )
    }

    fn click_bar(cx: &mut VisualTestContext, i: usize) {
        let p = bar(cx, i);
        cx.simulate_mouse_move(p, None, Modifiers::none());
        cx.simulate_click(p, Modifiers::none());
        cx.run_until_parked();
        frame(cx);
    }

    fn selected_seq(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
    ) -> Option<u64> {
        shell.read_with(cx, |s, _| match s.current().selection {
            Some(Selection::Frame(id)) => Some(id.seq),
            _ => None,
        })
    }

    #[test]
    fn chart_bars_cases() {
        let snap = frames(5);
        let rows: Vec<u32> = (0..5).collect();
        // One bar per frame at 6 px.
        let bars = chart_bars(&snap.frames, &rows, 0, 6.0, 100.0, BUDGET);
        assert_eq!(bars.len(), 5);
        for (i, b) in bars.iter().enumerate() {
            assert_eq!(b.row, i);
            assert_eq!(b.x, i as f32 * 6.0);
            assert!(b.w > 0.0 && b.w <= 6.0);
        }
        assert_eq!(bars[0].status, FrameStatus::Ok);
        assert_eq!(bars[3].status, FrameStatus::Slow);
        // A scrolled start and a narrow window only show what fits.
        let bars = chart_bars(&snap.frames, &rows, 2, 6.0, 13.0, BUDGET);
        assert_eq!(
            bars.iter().map(|b| b.row).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        // Empty input.
        assert!(chart_bars(&snap.frames, &[], 0, 6.0, 100.0, BUDGET).is_empty());
        assert!(chart_bars(&snap.frames, &rows, 9, 6.0, 100.0, BUDGET).is_empty());
    }

    #[test]
    fn chart_bars_take_the_pixel_maximum_when_zoomed_out() {
        let snap = frames(1_000);
        let rows: Vec<u32> = (0..1_000).collect();
        let bars = chart_bars(&snap.frames, &rows, 0, 0.1, 50.0, BUDGET);
        assert!(bars.len() <= 50, "{}", bars.len());
        assert!(!bars.is_empty());
        // Frame 10 is janky (40 ms) and shares a pixel column with nine ok frames.
        let column = bars
            .iter()
            .find(|b| b.row == 10)
            .expect("the worst frame is shown");
        assert_eq!(column.status, FrameStatus::Janky);
        assert_eq!(column.w, 1.0);
        for b in &bars {
            assert!(b.x >= 0.0 && b.x < 50.0);
        }
    }

    #[test]
    fn chart_bars_clip_at_three_budgets() {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 0, 8, 1));
            w.push_frame(frame_at(run, 1, 20, 100, 1));
        });
        let bars = chart_bars(&snap.frames, &[0, 1], 0, 6.0, 100.0, BUDGET);
        assert!(!bars[0].clipped);
        assert!((bars[0].h_frac - 8_000.0 / (3.0 * BUDGET as f32)).abs() < 1e-4);
        assert_eq!(bars[1].h_frac, 1.0);
        assert!(bars[1].clipped);
    }

    #[gpui_kit::test]
    fn click_selects_and_drag_sets_range(cx: &mut TestAppContext) {
        let (shell, _chart, cx) = open(cx, 30);
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        click_bar(cx, 5);
        assert_eq!(selected_seq(&shell, cx), Some(5));
        assert_eq!(shell.read_with(cx, |s, _| s.history().back_len()), back + 1);
        // A drag from frame 3 to frame 8.
        let (a, z) = (bar(cx, 3), bar(cx, 8));
        cx.simulate_mouse_move(a, None, Modifiers::none());
        cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            point((a.x + z.x) / 2.0, a.y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(z, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(z, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let range = shell
            .read_with(cx, |s, _| s.current().range)
            .expect("a range");
        assert_eq!(
            range.start,
            SessionTime(100 * 1_000_000 + 3 * 20 * 1_000_000)
        );
        let last_start = 100 * 1_000_000 + 8 * 20 * 1_000_000;
        assert!(range.end.0 >= last_start && range.end.0 < last_start + 20 * 1_000_000);
        assert_eq!(
            selected_seq(&shell, cx),
            Some(5),
            "a drag keeps the selection"
        );
        assert_eq!(shell.read_with(cx, |s, _| s.history().back_len()), back + 2);
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(Tool::Performance)
        );
    }

    #[gpui_kit::test]
    fn a_tiny_move_is_a_click_not_a_drag(cx: &mut TestAppContext) {
        let (shell, _chart, cx) = open(cx, 30);
        let a = bar(cx, 4);
        let near = point(a.x + px(1.0), a.y);
        cx.simulate_mouse_move(a, None, Modifiers::none());
        cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(near, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(near, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(selected_seq(&shell, cx), Some(4));
        assert!(shell.read_with(cx, |s, _| s.current().range).is_none());
    }

    #[gpui_kit::test]
    fn arrow_keys_move_between_frames(cx: &mut TestAppContext) {
        let (shell, _chart, cx) = open(cx, 30);
        click_bar(cx, 5);
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(selected_seq(&shell, cx), Some(6));
        cx.simulate_keystrokes("left left");
        cx.run_until_parked();
        assert_eq!(selected_seq(&shell, cx), Some(4));
        assert_eq!(
            shell.read_with(cx, |s, _| s.history().back_len()),
            back,
            "arrow keys add no Back step"
        );
        cx.simulate_keystrokes("left left left left left");
        cx.run_until_parked();
        assert_eq!(selected_seq(&shell, cx), Some(0), "no wrap");
    }

    #[gpui_kit::test]
    fn follow_latest_stops_after_panning_back(cx: &mut TestAppContext) {
        let (mut w, run) = crate::test_support::writer();
        push_frames(&mut w, run, 0, 400);
        let (_shell, cx) = open_shell_snapshot(cx, w.publish());
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        frame(cx);
        let chart = _shell.read_with(cx, |s, cx| s.performance_view().read(cx).chart().clone());
        let (first, following) = chart.read_with(cx, |c, _| (c.first(), c.following()));
        assert!(following);
        assert!(first > 0, "400 frames do not fit");
        // New frames arrive: the view moves with them.
        push_frames(&mut w, run, 400, 420);
        chart.update(cx, |c, cx| c.set_snapshot(w.publish(), cx));
        let moved = chart.read_with(cx, |c, _| c.first());
        assert_eq!(moved, first + 20);
        // Pan back: following stops and new frames leave the view where it is.
        chart.update(cx, |c, cx| c.pan_px(-120.0, cx));
        let (held, following) = chart.read_with(cx, |c, _| (c.first(), c.following()));
        assert!(!following);
        assert_eq!(held, moved - 20);
        push_frames(&mut w, run, 420, 440);
        chart.update(cx, |c, cx| c.set_snapshot(w.publish(), cx));
        assert_eq!(chart.read_with(cx, |c, _| c.first()), held);
        // "Latest" resumes.
        chart.update(cx, |c, cx| c.latest(cx));
        assert!(chart.read_with(cx, |c, _| c.following()));
        assert_eq!(chart.read_with(cx, |c, _| c.first()), moved + 20);
    }

    #[gpui_kit::test]
    fn zoom_changes_the_bar_width_within_its_limits(cx: &mut TestAppContext) {
        let (_shell, chart, cx) = open(cx, 100);
        chart.update(cx, |c, cx| c.zoom_about(100.0, 2.0, cx));
        assert_eq!(chart.read_with(cx, |c, _| c.bar_px()), 12.0);
        chart.update(cx, |c, cx| c.zoom_about(100.0, 100.0, cx));
        assert_eq!(chart.read_with(cx, |c, _| c.bar_px()), 24.0);
        chart.update(cx, |c, cx| c.zoom_about(100.0, 0.001, cx));
        assert_eq!(chart.read_with(cx, |c, _| c.bar_px()), 1.0);
    }

    #[gpui_kit::test]
    fn table_scroll_does_not_render_the_chart(cx: &mut TestAppContext) {
        let (shell, _chart, cx) = open(cx, 400);
        let draw = |cx: &mut VisualTestContext| cx.update(|w, cx| w.draw(cx).clear(cx));
        for _ in 0..3 {
            draw(cx);
        }
        let list = shell.read_with(cx, |s, cx| s.performance_view().read(cx).list().clone());
        let before = count(Counter::FrameChart);
        for row in [20, 60, 120] {
            list.update(cx, |l, cx| l.scroll_to_row(row, cx));
            draw(cx);
        }
        assert_eq!(count(Counter::FrameChart), before);
    }

    #[gpui_kit::test]
    fn hovering_a_bar_shows_the_tooltip_outside_the_chart(cx: &mut TestAppContext) {
        let (_shell, _chart, cx) = open(cx, 30);
        let p = bar(cx, 4);
        cx.simulate_mouse_move(p, None, Modifiers::none());
        cx.run_until_parked();
        frame(cx);
        let label = cx.update(|window, _| {
            window
                .try_find("frame-tooltip")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("Frame #4 \u{b7} 0:00 \u{b7} build 8.0 ms \u{b7} raster 1.0 ms")
        );
        let away = point(p.x, p.y - px(400.));
        cx.simulate_mouse_move(away, None, Modifiers::none());
        cx.run_until_parked();
        frame(cx);
        assert!(cx.update(|window, _| window.try_find("frame-tooltip").is_none()));
    }

    #[gpui_kit::test]
    fn the_latest_button_shows_after_panning_back(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, frames(400));
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        frame(cx);
        assert!(!crate::test_support::exists(cx, "frames-latest"));
        let chart = shell.read_with(cx, |s, cx| s.performance_view().read(cx).chart().clone());
        chart.update(cx, |c, cx| c.pan_px(-60.0, cx));
        cx.run_until_parked();
        frame(cx);
        assert!(crate::test_support::exists(cx, "frames-latest"));
        cx.update(|window, cx| window.click("frames-latest", cx));
        cx.run_until_parked();
        assert!(chart.read_with(cx, |c, _| c.following()));
        frame(cx);
        assert!(!crate::test_support::exists(cx, "frames-latest"));
    }

    #[gpui_kit::test]
    fn no_frames_shows_the_empty_state(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = crate::test_support::open_shell_fake(cx);
        fake.status
            .send(crate::test_support::connected("demo", true))
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        frame(cx);
        let label = cx.update(|window, _| {
            window
                .try_find("frames-none")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("No frames yet. Frames appear as your app draws. Use the app to record some.")
        );
        assert!(!crate::test_support::exists(cx, "frame-chart"));
    }
}
