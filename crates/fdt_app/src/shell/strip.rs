//! The session strip: a frame-time bar chart across the whole session, with run dividers.
//! Dragging on it selects the time range every tool filters by.

use crate::nav::{Location, TimeRange};
use crate::shell::MainWindow;
use fdt_store::Snapshot;
use fdt_store::frames::strip_bars;
use fdt_store::time::{RunId, SessionTime};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, space};
use gpui_kit::{
    AnyElement, Bounds, Context, DispatchPhase, Hitbox, HitboxBehavior, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, SharedString, Styled as _, TestSupportExt as _,
    Window, canvas, div, fill, point, px, relative, size,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

const SEC_NS: u64 = 1_000_000_000;
/// The shortest span the strip shows.
const MIN_SPAN_NS: u64 = 10 * SEC_NS;
/// A frame within this is on budget (60 Hz).
const BUDGET_US: u32 = 16_700;
/// A frame within this is slow; beyond it is janky. Also the full height of a bar.
const JANK_US: u32 = 33_400;
/// A press that moves less than this many pixels is a click, not a drag.
const MIN_DRAG_PX: f32 = 3.0; // token-check: allow: interaction threshold, not a visual size
/// The narrowest gap (in pixels) a "Run N" label needs.
const RUN_LABEL_MIN_PX: f32 = 56.0;

/// `m:ss` below one hour, `h:mm:ss` from one hour.
pub fn format_offset(t: SessionTime) -> String {
    let secs = t.0 / SEC_NS;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{}:{s:02}", secs / 60)
    }
}

/// `0:51\u{2013}1:07`, with an en dash.
pub fn format_range(r: TimeRange) -> String {
    format!("{}\u{2013}{}", format_offset(r.start), format_offset(r.end))
}

/// The time the strip covers: from 0 to the later of the latest stored time and `now`, and at
/// least ten seconds.
pub fn session_span(snap: &Snapshot, now: Option<SessionTime>) -> TimeRange {
    let end = snap
        .latest_time()
        .0
        .max(now.map_or(0, |n| n.0))
        .max(MIN_SPAN_NS);
    TimeRange::new(SessionTime(0), SessionTime(end))
}

/// The time at horizontal fraction `x_frac` (clamped to 0..=1) of `span`.
pub fn x_to_time(x_frac: f32, span: TimeRange) -> SessionTime {
    let f = f64::from(x_frac.clamp(0.0, 1.0));
    SessionTime(span.start.0 + (span.duration_ns() as f64 * f).round() as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Ok,
    Slow,
    Janky,
}

/// Bars and dividers in pixels for a strip `width_px` wide.
#[derive(Clone, Debug, PartialEq)]
pub struct StripLayout {
    pub width_px: usize,
    /// `(x, width, height fraction, kind)`.
    pub bars: Vec<(f32, f32, f32, BarKind)>,
    /// `(x, run)` for each run that starts after the span's start.
    pub dividers: Vec<(f32, RunId)>,
}

/// Lays the strip out. Visits at most about `2 * width_px` buckets, never every frame.
pub fn strip_layout(snap: &Snapshot, span: TimeRange, width_px: usize) -> StripLayout {
    let dur = span.duration_ns().max(1) as f64;
    let w = width_px as f64;
    let x_of = |t: SessionTime| (t.0.saturating_sub(span.start.0) as f64 / dur * w) as f32;
    let bars = strip_bars(&snap.frames, span, width_px)
        .into_iter()
        .map(|b| {
            let x = x_of(b.start);
            let width = (x_of(b.end) - x).max(1.0);
            let kind = if b.max_elapsed_us <= BUDGET_US {
                BarKind::Ok
            } else if b.max_elapsed_us <= JANK_US {
                BarKind::Slow
            } else {
                BarKind::Janky
            };
            let h = (b.max_elapsed_us as f32 / JANK_US as f32).min(1.0);
            (x, width, h, kind)
        })
        .collect();
    let dividers = snap
        .runs
        .iter()
        .filter(|r| r.started > span.start && r.started <= span.end)
        .map(|r| (x_of(r.started), r.id))
        .collect();
    StripLayout {
        width_px,
        bars,
        dividers,
    }
}

#[derive(Clone, Copy)]
struct Drag {
    start_x: Pixels,
    x: Pixels,
}

/// What the strip's paint and mouse handlers share with the window across frames.
#[derive(Default)]
struct StripState {
    bounds: Bounds<Pixels>,
    drag: Option<Drag>,
}

struct StripCache {
    generation: u64,
    span: TimeRange,
    layout: Arc<StripLayout>,
}

/// Per-window strip state: drag and bounds, and the last layout.
#[derive(Default)]
pub struct StripView {
    state: Rc<RefCell<StripState>>,
    cache: Option<StripCache>,
}

impl StripView {
    fn layout(&mut self, snap: &Snapshot, span: TimeRange, width_px: usize) -> Arc<StripLayout> {
        if let Some(c) = &self.cache
            && c.generation == snap.generation
            && c.span == span
            && c.layout.width_px == width_px
        {
            return c.layout.clone();
        }
        let layout = Arc::new(strip_layout(snap, span, width_px));
        self.cache = Some(StripCache {
            generation: snap.generation,
            span,
            layout: layout.clone(),
        });
        layout
    }
}

fn frac_of(x: Pixels, bounds: Bounds<Pixels>) -> f32 {
    let w = f32::from(bounds.size.width);
    if w <= 0.0 {
        return 0.0;
    }
    (f32::from(x - bounds.origin.x) / w).clamp(0.0, 1.0)
}

impl MainWindow {
    /// Where the strip was last laid out; zero-sized before its first frame.
    pub(crate) fn strip_bounds(&self) -> Bounds<Pixels> {
        self.strip.state.borrow().bounds
    }

    /// Moves to the current location with `range`.
    pub(super) fn set_range(
        &mut self,
        range: Option<TimeRange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let to = Location {
            range,
            ..self.current().clone()
        };
        self.navigate(to, window, cx);
    }

    /// The strip for a tool page's header.
    pub(super) fn render_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let session = self.session.read(cx);
        let snap = session.snapshot().clone();
        let now = session.recording_elapsed().and_then(|e| {
            let run = snap.runs.last()?;
            Some(SessionTime(run.started.0 + e.as_nanos() as u64))
        });
        let span = session_span(&snap, now);
        let known = f32::from(self.strip.state.borrow().bounds.size.width);
        let width = if known >= 1.0 {
            known
        } else {
            f32::from(window.viewport_size().width)
        };
        let layout = self.strip.layout(&snap, span, width.round() as usize);
        let range = self.current().range;
        let state = self.strip.state.clone();
        let shell = cx.weak_entity();

        let (bar_ok, bar_slow, bar_jank) = (t.success, t.warning, t.danger);
        let (line, divider, overlay) = (
            t.border,
            t.text_faint,
            t.accent.with_alpha(alpha::SELECTION),
        );
        let paint_layout = layout.clone();
        let paint_state = state.clone();
        let surface = canvas(
            {
                let state = state.clone();
                move |bounds, window, _| {
                    state.borrow_mut().bounds = bounds;
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                }
            },
            move |bounds: Bounds<Pixels>, hitbox: Hitbox, window, _cx| {
                let st = paint_state.clone();
                let l = &paint_layout;
                let scale = f32::from(bounds.size.width) / l.width_px.max(1) as f32;
                let (ox, oy) = (bounds.origin.x, bounds.origin.y);
                let height = bounds.size.height;
                for &(x, w, h, kind) in &l.bars {
                    let color: Hsla = match kind {
                        BarKind::Ok => bar_ok,
                        BarKind::Slow => bar_slow,
                        BarKind::Janky => bar_jank,
                    }
                    .into();
                    let bh = height * h;
                    let rect = Bounds::new(
                        point(ox + px(x * scale), oy + height - bh),
                        size(px(w * scale).max(layout::BORDER), bh),
                    );
                    window.paint_quad(fill(rect, color));
                }
                // The budget line sits at half height.
                window.paint_quad(fill(
                    Bounds::new(
                        point(ox, oy + height / 2.0),
                        size(bounds.size.width, layout::BORDER),
                    ),
                    Hsla::from(line),
                ));
                for &(x, _) in &l.dividers {
                    window.paint_quad(fill(
                        Bounds::new(point(ox + px(x * scale), oy), size(layout::BORDER, height)),
                        Hsla::from(divider),
                    ));
                }
                let shown = match st.borrow().drag {
                    Some(d) => {
                        let (a, b) = (frac_of(d.start_x, bounds), frac_of(d.x, bounds));
                        Some((a.min(b), a.max(b)))
                    }
                    None => range.map(|r| {
                        let dur = span.duration_ns().max(1) as f64;
                        let f = |t: SessionTime| {
                            (t.0.saturating_sub(span.start.0) as f64 / dur).clamp(0.0, 1.0) as f32
                        };
                        (f(r.start), f(r.end))
                    }),
                };
                if let Some((a, b)) = shown {
                    let rect = Bounds::new(
                        point(ox + bounds.size.width * a, oy),
                        size(bounds.size.width * (b - a), height),
                    );
                    window.paint_quad(fill(rect, overlay));
                }

                let state = st.clone();
                let target = shell.clone();
                let hit = hitbox.clone();
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || e.button != MouseButton::Left
                        || !hit.is_hovered(window)
                    {
                        return;
                    }
                    if e.click_count >= 2 {
                        state.borrow_mut().drag = None;
                        target
                            .update(cx, |this, cx| this.set_range(None, window, cx))
                            .ok();
                        return;
                    }
                    state.borrow_mut().drag = Some(Drag {
                        start_x: e.position.x,
                        x: e.position.x,
                    });
                    window.refresh();
                });
                let state = st.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, _, window, _| {
                    let mut s = state.borrow_mut();
                    let Some(d) = s.drag.as_mut() else { return };
                    if e.pressed_button == Some(MouseButton::Left) {
                        d.x = e.position.x;
                    } else {
                        s.drag = None;
                    }
                    window.refresh();
                });
                let state = st;
                let target = shell;
                window.on_mouse_event(move |e: &MouseUpEvent, _, window, cx| {
                    let Some(d) = state.borrow_mut().drag.take() else {
                        return;
                    };
                    window.refresh();
                    if f32::from((e.position.x - d.start_x).abs()) < MIN_DRAG_PX {
                        return;
                    }
                    let b = state.borrow().bounds;
                    let (a, z) = (
                        x_to_time(frac_of(d.start_x, b), span),
                        x_to_time(frac_of(e.position.x, b), span),
                    );
                    target
                        .update(cx, |this, cx| {
                            this.set_range(Some(TimeRange::new(a, z)), window, cx)
                        })
                        .ok();
                });
            },
        )
        .size_full();

        let mut strip = div()
            .id("session-strip")
            .test_support()
            .relative()
            .overflow_hidden()
            .w_full()
            .h(layout::STRIP_HEIGHT)
            .child(surface);
        // "Run N" labels, where there is room before the next divider.
        let w_px = width.max(1.0);
        let xs: Vec<f32> = layout
            .dividers
            .iter()
            .map(|d| d.0 * w_px / layout.width_px.max(1) as f32)
            .collect();
        for (i, &(_, run)) in layout.dividers.iter().enumerate() {
            let next = xs.get(i + 1).copied().unwrap_or(w_px);
            if next - xs[i] < RUN_LABEL_MIN_PX {
                continue;
            }
            strip = strip.child(
                div()
                    .absolute()
                    .top_0()
                    .left(relative(xs[i] / w_px))
                    .pl(space::S4)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_faint))
                    .child(SharedString::from(format!("Run {}", run.0))),
            );
        }
        strip.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Page, Selection, Tool};
    use crate::test_support::{exists, log_at, open_shell_snapshot, press, snapshot};
    use fdt_store::frames::FrameInput;
    use fdt_store::logs::LogLevel;
    use fdt_store::time::{EntryId, RunId};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, TestAppContext,
        VisualTestContext, point, px,
    };

    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000_000_000;

    fn frame(run: RunId, n: u64, start_ms: u64, elapsed_us: u32) -> FrameInput {
        FrameInput {
            run,
            number: n,
            start: SessionTime(start_ms * MS),
            elapsed_us,
            build_us: elapsed_us / 2,
            raster_us: elapsed_us / 2,
            vsync_overhead_us: 0,
        }
    }

    /// Two runs; a 40 ms frame in the second.
    fn two_runs() -> Arc<Snapshot> {
        snapshot(|w, run| {
            w.push_frame(frame(run, 0, 1_000, 8_000));
            w.end_run(run, SessionTime(5 * SEC));
            let clock = fdt_store::clock::RunClock::new(
                0,
                fdt_store::clock::Calibration {
                    skew_us: 0,
                    timeline_to_wall_us: 0,
                },
            );
            let second = w.start_run(
                crate::test_support::app_info("demo"),
                clock,
                SessionTime(10 * SEC),
            );
            w.push_frame(frame(second, 1, 12_000, 40_000));
            w.push_frame(frame(second, 2, 13_000, 20_000));
        })
    }

    #[test]
    fn format_offset_cases() {
        assert_eq!(format_offset(SessionTime(0)), "0:00");
        assert_eq!(format_offset(SessionTime(65 * SEC)), "1:05");
        assert_eq!(format_offset(SessionTime(3_725 * SEC)), "1:02:05");
    }

    #[test]
    fn format_range_uses_an_en_dash() {
        let r = TimeRange::new(SessionTime(51 * SEC), SessionTime(67 * SEC));
        assert_eq!(format_range(r), "0:51\u{2013}1:07");
    }

    #[test]
    fn span_has_a_minimum_and_follows_now() {
        let empty = Snapshot::empty();
        let s = session_span(&empty, None);
        assert_eq!((s.start, s.end), (SessionTime(0), SessionTime(10 * SEC)));
        let snap = two_runs();
        assert_eq!(session_span(&snap, None).end, snap.latest_time());
        let s = session_span(&snap, Some(SessionTime(60 * SEC)));
        assert_eq!(s.end, SessionTime(60 * SEC));
        let s = session_span(&empty, Some(SessionTime(3 * SEC)));
        assert_eq!(s.end, SessionTime(10 * SEC));
    }

    #[test]
    fn strip_layout_classifies_bars_and_places_dividers() {
        let snap = two_runs();
        let span = session_span(&snap, None);
        let layout = strip_layout(&snap, span, 400);
        let kinds: Vec<BarKind> = layout.bars.iter().map(|b| b.3).collect();
        assert!(kinds.contains(&BarKind::Ok));
        let janky: Vec<_> = layout
            .bars
            .iter()
            .filter(|b| b.3 == BarKind::Janky)
            .collect();
        assert_eq!(janky.len(), 1);
        assert_eq!(janky[0].2, 1.0);
        // The 8 ms frame is an Ok bar at under half height.
        let ok = layout.bars.iter().find(|b| b.3 == BarKind::Ok).unwrap();
        assert!(ok.2 > 0.0 && ok.2 < 0.5);
        // The second run starts 10 s into a 13.x s span.
        assert_eq!(layout.dividers.len(), 1);
        assert_eq!(layout.dividers[0].1, RunId(2));
        let want = 400.0 * 10.0 / (span.duration_ns() as f32 / SEC as f32);
        assert!((layout.dividers[0].0 - want).abs() < 0.5);
    }

    #[test]
    fn bar_kind_boundaries() {
        let snap = snapshot(|w, run| {
            for (i, us) in [16_700u32, 16_701, 33_400, 33_401].into_iter().enumerate() {
                w.push_frame(frame(run, i as u64, 1_000 + i as u64 * 2_000, us));
            }
        });
        let span = session_span(&snap, None);
        let layout = strip_layout(&snap, span, 400);
        let kinds: Vec<BarKind> = layout.bars.iter().map(|b| b.3).collect();
        assert_eq!(
            kinds,
            [BarKind::Ok, BarKind::Slow, BarKind::Slow, BarKind::Janky]
        );
        assert_eq!(layout.bars[2].2, 1.0);
    }

    #[test]
    fn x_to_time_maps_edges() {
        let span = TimeRange::new(SessionTime(0), SessionTime(10 * SEC));
        assert_eq!(x_to_time(0.0, span), SessionTime(0));
        assert_eq!(x_to_time(1.0, span), SessionTime(10 * SEC));
        assert_eq!(x_to_time(0.5, span), SessionTime(5 * SEC));
        assert_eq!(x_to_time(-1.0, span), SessionTime(0));
        assert_eq!(x_to_time(2.0, span), SessionTime(10 * SEC));
    }

    /// 20 s of logs and frames, so the span is 20 s.
    fn twenty_seconds() -> Arc<Snapshot> {
        snapshot(|w, run| {
            for i in 0..20 {
                w.push_frame(frame(run, i, i * 1_000, 10_000));
            }
            w.push_log(log_at(run, 20_000, LogLevel::Info));
        })
    }

    fn strip_bounds(cx: &mut VisualTestContext) -> gpui_kit::Bounds<Pixels> {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.find("session-strip").bounds()
        })
    }

    fn goto_tool(shell: &gpui_kit::Entity<MainWindow>, cx: &mut VisualTestContext) {
        press(cx, "cmd-5", "ctrl-5");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().page),
            Page::Tool(Tool::Logs)
        );
    }

    fn drag(cx: &mut VisualTestContext, from: f32, to: f32) {
        let b = strip_bounds(cx);
        let y = b.origin.y + b.size.height / 2.0;
        let at = |x: f32| point(b.origin.x + b.size.width * x, y);
        cx.simulate_mouse_move(at(from), None, Modifiers::none());
        cx.simulate_mouse_down(at(from), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at((from + to) / 2.0), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(to), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(to), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn dragging_the_strip_sets_one_history_entry(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        goto_tool(&shell, cx);
        assert!(exists(cx, "session-strip"));
        let before = shell.read_with(cx, |s, _| s.history().back_len());
        drag(cx, 0.25, 0.75);
        let (range, after) =
            shell.read_with(cx, |s, _| (s.current().range, s.history().back_len()));
        let r = range.expect("a range was set");
        let span_ns = 20 * SEC as i64;
        let tol = span_ns / 100;
        assert!((r.start.0 as i64 - span_ns / 4).abs() <= tol, "{r:?}");
        assert!((r.end.0 as i64 - span_ns * 3 / 4).abs() <= tol, "{r:?}");
        assert_eq!(after, before + 1);
    }

    #[gpui_kit::test]
    fn a_click_without_movement_does_nothing(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        goto_tool(&shell, cx);
        let b = strip_bounds(cx);
        let p = point(b.origin.x + px(40.), b.origin.y + px(8.));
        cx.simulate_mouse_move(p, None, Modifiers::none());
        cx.simulate_click(p, Modifiers::none());
        assert!(shell.read_with(cx, |s, _| s.current().range.is_none()));
    }

    #[gpui_kit::test]
    fn drag_past_the_edge_clamps(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        goto_tool(&shell, cx);
        let b = strip_bounds(cx);
        let y = b.origin.y + b.size.height / 2.0;
        let start = point(b.origin.x + b.size.width * 0.5, y);
        let outside = point(b.origin.x + b.size.width + px(300.), y + px(200.));
        cx.simulate_mouse_move(start, None, Modifiers::none());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(outside, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let r = shell.read_with(cx, |s, _| s.current().range).unwrap();
        assert_eq!(r.end, SessionTime(20 * SEC));
        assert!(r.start.0 > 9 * SEC && r.start.0 < 11 * SEC);
    }

    #[gpui_kit::test]
    fn double_click_clears_the_range(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        goto_tool(&shell, cx);
        drag(cx, 0.25, 0.75);
        assert!(shell.read_with(cx, |s, _| s.current().range.is_some()));
        let b = strip_bounds(cx);
        let p = point(b.origin.x + b.size.width * 0.5, b.origin.y + px(8.));
        cx.simulate_mouse_move(p, None, Modifiers::none());
        cx.simulate_event(MouseDownEvent {
            position: p,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position: p,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
        });
        assert!(shell.read_with(cx, |s, _| s.current().range.is_none()));
    }

    #[gpui_kit::test]
    fn escape_inside_the_strip_clears_selection_then_range(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        goto_tool(&shell, cx);
        drag(cx, 0.25, 0.75);
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.selection = Some(Selection::Log(EntryId {
            run: RunId(1),
            seq: 0,
        }));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        press(cx, "escape", "escape");
        let (sel, rng) = shell.read_with(cx, |s, _| (s.current().selection, s.current().range));
        assert!(sel.is_none() && rng.is_some());
        press(cx, "escape", "escape");
        assert!(shell.read_with(cx, |s, _| s.current().range.is_none()));
    }

    #[gpui_kit::test]
    fn the_strip_only_shows_on_tool_pages(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell_snapshot(cx, twenty_seconds());
        assert!(!exists(cx, "session-strip"));
        press(cx, "cmd-,", "ctrl-,");
        assert!(!exists(cx, "session-strip"));
        press(cx, "cmd-3", "ctrl-3");
        assert!(exists(cx, "session-strip"));
    }
}
