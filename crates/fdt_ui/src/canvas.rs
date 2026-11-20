//! Batched canvas painters and the zoom and pan input listeners shared by the frame chart, the
//! Timeline lanes, the Raw trace and the flame chart.
//!
//! A canvas paints all its quads and labels inside one `paint_layer` (bare `paint_quad` calls cost
//! more than ten times as much) and registers its input in the paint phase.

use crate::tokens::{font, layout};
use crate::viewport::{pinch_zoom_factor, scroll_zoom_factor};
use gpui_kit::{
    App, Bounds, ContentMask, DispatchPhase, Hitbox, Hsla, Modifiers, PinchEvent, Pixels, Point,
    ScrollEvent, SharedString, TextAlign, TextRun, TruncateFrom, Window, fill,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// The most quads one canvas paints per frame. The rest are ignored.
pub const CANVAS_MAX_QUADS: usize = 20_000;
/// The most labels one canvas paints per frame. The rest are ignored.
pub const CANVAS_MAX_LABELS: usize = 500;

#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub bounds: Bounds<Pixels>,
    pub color: Hsla,
}

#[derive(Clone, Debug)]
pub struct Label {
    pub text: SharedString,
    /// The top-left of the line box.
    pub origin: Point<Pixels>,
    /// The room the label has; it is elided to fit, or skipped when too narrow.
    pub max_width: Pixels,
    pub color: Hsla,
}

/// Logs the first time a canvas is over a cap, not every frame.
fn warn_once(flag: &AtomicBool, what: &str, cap: usize) {
    if !flag.swap(true, Ordering::Relaxed) {
        log::warn!("canvas over its cap of {cap} {what} per frame; the rest are not painted");
    }
}

/// Paints `quads` (at most `CANVAS_MAX_QUADS`) in one layer covering `layer` (it does not clip; mask the parent if needed).
pub fn paint_quads(window: &mut Window, layer: Bounds<Pixels>, quads: &[Quad]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if quads.len() > CANVAS_MAX_QUADS {
        warn_once(&WARNED, "quads", CANVAS_MAX_QUADS);
    }
    let quads = &quads[..quads.len().min(CANVAS_MAX_QUADS)];
    #[cfg(test)]
    tests::QUADS.with(|c| c.set(c.get() + quads.len()));
    window.paint_layer(layer, |window| {
        for q in quads {
            window.paint_quad(fill(q.bounds, q.color));
        }
    });
}

/// Paints `labels` (at most `CANVAS_MAX_LABELS`) in the mono label font, clipped to `clip`.
/// A label with less than `layout::LABEL_MIN_WIDTH` of room is skipped; one that does not fit is
/// cut with an ellipsis.
pub fn paint_labels(window: &mut Window, cx: &mut App, clip: Bounds<Pixels>, labels: &[Label]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if labels.len() > CANVAS_MAX_LABELS {
        warn_once(&WARNED, "labels", CANVAS_MAX_LABELS);
    }
    let size = font::LABEL;
    let line_height = size * font::LINE_HEIGHT_UI;
    let face = gpui_kit::font(font::MONO);
    let mut wrapper = window.text_system().line_wrapper(face.clone(), size);
    let mut prepared = Vec::new();
    for l in labels.iter().take(CANVAS_MAX_LABELS) {
        if l.max_width < layout::LABEL_MIN_WIDTH || l.text.is_empty() {
            continue;
        }
        let run = |len: usize| TextRun {
            len,
            font: face.clone(),
            color: l.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let text = if wrapper
            .should_truncate_line(&l.text, l.max_width, "…", TruncateFrom::End)
            .is_some()
        {
            #[cfg(test)]
            tests::ELIDED.with(|c| c.set(c.get() + 1));
            let runs = [run(l.text.len())];
            wrapper
                .truncate_line(l.text.clone(), l.max_width, "…", &runs, TruncateFrom::End)
                .0
        } else {
            l.text.clone()
        };
        prepared.push((text, l.origin, l.color));
    }
    drop(wrapper);
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        for (text, origin, color) in prepared {
            let runs = [TextRun {
                len: text.len(),
                font: face.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let line = window.text_system().shape_line(text, size, &runs, None);
            if line
                .paint(origin, line_height, TextAlign::Left, None, window, cx)
                .is_ok()
            {
                #[cfg(test)]
                tests::LABELS.with(|c| c.set(c.get() + 1));
            }
        }
    });
}

/// What the user asked of a chart with the wheel, trackpad or pinch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomPan {
    /// Zoom about `x`, measured from the left edge of the hitbox. A factor above 1 zooms in.
    Zoom { x: Pixels, factor: f32 },
    /// Pan horizontally by this many pixels. Positive shows later time, as `Viewport::pan_px`
    /// takes it. gpui's scroll delta is the motion of the content, so the listener negates it.
    PanX(Pixels),
    /// Pan vertically by this many pixels. Positive shows content further down (the same
    /// negation of gpui's delta).
    PanY(Pixels),
}

fn zoom_modifier(m: &Modifiers) -> bool {
    m.platform || m.control
}

/// Registers the zoom and pan listeners for a canvas. Call it in the paint phase, with the hitbox
/// from prepaint. The listeners act only in the bubble phase and only while the hitbox is hovered,
/// so an event is handled once.
pub fn zoom_pan_listeners(
    window: &mut Window,
    hitbox: &Hitbox,
    line_height: Pixels,
    on: impl Fn(ZoomPan, &mut Window, &mut App) + 'static,
) {
    let on = std::rc::Rc::new(on);
    let hit = hitbox.clone();
    let scroll_on = on.clone();
    window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !hit.is_hovered(window) {
            return;
        }
        let d = e.delta.pixel_delta(line_height);
        let action = if zoom_modifier(&e.modifiers) {
            // A mouse wheel with a modifier often reports its motion on x.
            let dy = if d.y == Pixels::ZERO { d.x } else { d.y };
            ZoomPan::Zoom {
                x: e.position.x - hit.bounds.origin.x,
                factor: scroll_zoom_factor(f32::from(dy)),
            }
        } else if e.modifiers.shift {
            let dx = if d.x == Pixels::ZERO { d.y } else { d.x };
            ZoomPan::PanX(-dx)
        } else if d.x.abs() > d.y.abs() {
            ZoomPan::PanX(-d.x)
        } else {
            ZoomPan::PanY(-d.y)
        };
        scroll_on(action, window, cx);
        cx.stop_propagation();
    });
    let hit = hitbox.clone();
    window.on_mouse_event(move |e: &PinchEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !hit.is_hovered(window) {
            return;
        }
        on(
            ZoomPan::Zoom {
                x: e.position.x - hit.bounds.origin.x,
                factor: pinch_zoom_factor(e.delta),
            },
            window,
            cx,
        );
        cx.stop_propagation();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::point;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        Context, HitboxBehavior, IntoElement, Render, ScrollDelta, Styled as _, TestAppContext,
        TouchPhase, canvas, hsla, px, size,
    };
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    thread_local! {
        pub(super) static QUADS: Cell<usize> = const { Cell::new(0) };
        pub(super) static ELIDED: Cell<usize> = const { Cell::new(0) };
        pub(super) static LABELS: Cell<usize> = const { Cell::new(0) };
    }

    type Paint = Rc<dyn Fn(&mut Window, &mut App, Bounds<Pixels>, &Hitbox)>;

    struct Host(Paint);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let paint = self.0.clone();
            canvas(
                |b, window, _| window.insert_hitbox(b, HitboxBehavior::Normal),
                move |b, hit, window, cx| paint(window, cx, b, &hit),
            )
            .size_full()
        }
    }

    fn open(
        cx: &mut TestAppContext,
        paint: impl Fn(&mut Window, &mut App, Bounds<Pixels>, &Hitbox) + 'static,
    ) -> &mut gpui_kit::VisualTestContext {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::install(cx);
        });
        let paint: Paint = Rc::new(paint);
        let (_, cx) = cx.add_window_view(move |_, _| Host(paint));
        cx
    }

    fn draw(cx: &mut gpui_kit::VisualTestContext) {
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn reset() {
        QUADS.with(|c| c.set(0));
        LABELS.with(|c| c.set(0));
        ELIDED.with(|c| c.set(0));
    }

    #[gpui_kit::test]
    fn paint_quads_smoke(cx: &mut TestAppContext) {
        reset();
        let cx = open(cx, |window, _, b, _| {
            let q = Quad {
                bounds: Bounds::new(b.origin, size(px(3.), px(3.))),
                color: hsla(0.5, 0.5, 0.5, 1.0),
            };
            paint_quads(window, b, &vec![q; 10]);
            // Over the cap: the excess is ignored, with no panic.
            paint_quads(window, b, &vec![q; CANVAS_MAX_QUADS + 25]);
        });
        // The window already drew while opening; count one frame of its own.
        reset();
        draw(cx);
        assert_eq!(QUADS.with(Cell::get), 10 + CANVAS_MAX_QUADS);
    }

    #[gpui_kit::test]
    fn labels_elide_or_skip(cx: &mut TestAppContext) {
        reset();
        let cx = open(cx, |window, cx, b, _| {
            let mk = |text: &str, w: f32| Label {
                text: text.to_string().into(),
                origin: b.origin,
                max_width: px(w),
                color: hsla(0., 0., 1., 1.),
            };
            paint_labels(
                window,
                cx,
                b,
                &[
                    mk("fits", 200.),
                    mk("a very long label that cannot possibly fit", 60.),
                    mk("too narrow", 31.),
                    mk("", 100.),
                ],
            );
            let many = vec![mk("x", 100.); CANVAS_MAX_LABELS + 10];
            paint_labels(window, cx, b, &many);
        });
        reset();
        draw(cx);
        // Two of the four in the first call (the narrow and the empty one are skipped), then the
        // capped batch.
        assert_eq!(LABELS.with(Cell::get), 2 + CANVAS_MAX_LABELS);
        // Only the long one was cut.
        assert_eq!(ELIDED.with(Cell::get), 1);
    }

    #[gpui_kit::test]
    fn zoom_pan_listeners_bubble_only(cx: &mut TestAppContext) {
        let seen: Rc<RefCell<Vec<ZoomPan>>> = Rc::default();
        let sink = seen.clone();
        let cx = open(cx, move |window, _, _, hit| {
            let sink = sink.clone();
            zoom_pan_listeners(window, hit, px(20.), move |a, _, _| {
                sink.borrow_mut().push(a)
            });
        });
        draw(cx);
        let inside = point(px(50.), px(40.));
        cx.simulate_mouse_move(inside, None, Modifiers::none());
        let scroll = |dx: f32, dy: f32, m: Modifiers| ScrollEvent {
            position: inside,
            delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
            modifiers: m,
            touch_phase: TouchPhase::Moved,
            source: Default::default(),
        };

        // One dispatch runs both the capture and the bubble phase: the callback fires once.
        cx.simulate_event(scroll(0., 12., Modifiers::none()));
        assert_eq!(*seen.borrow(), vec![ZoomPan::PanY(px(-12.))]);

        cx.simulate_event(scroll(9., 1., Modifiers::none()));
        cx.simulate_event(scroll(0., 7., Modifiers::shift()));
        cx.simulate_event(scroll(0., 50., Modifiers::command()));
        cx.simulate_event(PinchEvent {
            position: inside,
            delta: 0.1,
            modifiers: Modifiers::none(),
            phase: TouchPhase::Moved,
        });
        let seen = seen.borrow();
        assert_eq!(seen.len(), 5, "{seen:?}");
        assert_eq!(seen[1], ZoomPan::PanX(px(-9.)));
        assert_eq!(seen[2], ZoomPan::PanX(px(-7.)));
        let ZoomPan::Zoom { x, factor } = seen[3] else {
            panic!("{:?}", seen[3])
        };
        assert!(factor > 1.0);
        assert_eq!(x, px(50.));
        assert!(matches!(seen[4], ZoomPan::Zoom { factor, .. } if (factor - 1.1).abs() < 1e-6));
    }

    #[gpui_kit::test]
    fn zoom_pan_listeners_ignore_events_off_the_hitbox(cx: &mut TestAppContext) {
        let seen: Rc<RefCell<Vec<ZoomPan>>> = Rc::default();
        let sink = seen.clone();
        let cx = open(cx, move |window, _, _, hit| {
            let sink = sink.clone();
            zoom_pan_listeners(window, hit, px(20.), move |a, _, _| {
                sink.borrow_mut().push(a)
            });
        });
        draw(cx);
        let outside = point(px(-50.), px(-40.));
        cx.simulate_mouse_move(outside, None, Modifiers::none());
        cx.simulate_event(ScrollEvent {
            position: outside,
            delta: ScrollDelta::Lines(point(0., 3.)),
            ..Default::default()
        });
        assert!(seen.borrow().is_empty());
    }

    #[gpui_kit::test]
    fn swipe_left_shows_later_time_and_scroll_down_shows_lower_content(cx: &mut TestAppContext) {
        use crate::viewport::Viewport;
        let bounds = (0, 100_000_000);
        let view = Rc::new(Cell::new(Viewport::fit(10_000_000, 20_000_000)));
        let down = Rc::new(Cell::new(0.0f32));
        let (v, d) = (view.clone(), down.clone());
        let cx = open(cx, move |window, _, _, hit| {
            let (v, d) = (v.clone(), d.clone());
            zoom_pan_listeners(window, hit, px(20.), move |a, _, _| match a {
                ZoomPan::PanX(dx) => {
                    let mut vp = v.get();
                    vp.pan_px(f32::from(dx), 500.0, bounds);
                    v.set(vp);
                }
                ZoomPan::PanY(dy) => d.set(d.get() + f32::from(dy)),
                ZoomPan::Zoom { .. } => {}
            });
        });
        draw(cx);
        let inside = point(px(50.), px(40.));
        cx.simulate_mouse_move(inside, None, Modifiers::none());
        let scroll = |dx: f32, dy: f32| ScrollEvent {
            position: inside,
            delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
            ..Default::default()
        };
        // A trackpad swipe left moves the content left: a negative x delta.
        cx.simulate_event(scroll(-40., 0.));
        assert!(view.get().start_ns > 10_000_000, "{:?}", view.get());
        // A swipe right goes back.
        cx.simulate_event(scroll(40., 0.));
        assert_eq!(view.get().start_ns, 10_000_000);
        // Content moving up (a negative y delta) shows content further down.
        cx.simulate_event(scroll(0., -30.));
        assert_eq!(down.get(), 30.0);
        // A wheel mouse reports lines; they go through the line height.
        cx.simulate_event(ScrollEvent {
            position: inside,
            delta: ScrollDelta::Lines(point(0., -2.)),
            ..Default::default()
        });
        assert_eq!(down.get(), 70.0);
    }
}
