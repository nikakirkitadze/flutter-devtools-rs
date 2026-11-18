//! The frame-time HUD: live draw and frame-interval statistics from GPUI's profiler.

pub mod stats;

use crate::theme::TokensExt as _;
use crate::tokens::{font, layout, radius, space};
use gpui_kit::profiler::{FrameEvent, FrameTimingCollector, set_trace_enabled};
use gpui_kit::{
    App, Bounds, Context, Global, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Task, TestSupportExt as _, Window, WindowId, canvas, div, fill, point,
    size,
};
use std::time::Duration;

pub use stats::FrameStats;

/// Display period assumed until a draw reports the real one (60 Hz).
pub const DEFAULT_REFRESH: Duration = Duration::from_micros(16_667); // token-check: allow: display period, not motion

/// How often a visible HUD collects new frame events.
const POLL_INTERVAL: Duration = Duration::from_millis(250); // token-check: allow: poll interval, not motion

/// Most draws shown in the bar graph.
const GRAPH_SAMPLES: usize = 120;

/// Draws at or below this are drawn in the success colour.
const FAST_DRAW: Duration = Duration::from_millis(4); // token-check: allow: draw-time threshold, not motion

/// Number of visible HUDs across all windows; tracing stays on while it is above 0.
struct HudTrace(usize);

impl Global for HudTrace {}

/// Visible HUDs across all windows.
pub fn trace_users(cx: &App) -> usize {
    cx.try_global::<HudTrace>().map_or(0, |t| t.0)
}

fn add_trace_user(cx: &mut App) {
    let count = trace_users(cx) + 1;
    cx.set_global(HudTrace(count));
    set_trace_enabled(true);
}

fn remove_trace_user(cx: &mut App) {
    let count = trace_users(cx).saturating_sub(1);
    cx.set_global(HudTrace(count));
    if count == 0 {
        set_trace_enabled(false);
    }
}

/// Feeds the events for `window` into `stats`, tracking the display period in `refresh`.
pub fn ingest(
    stats: &mut FrameStats,
    events: &[FrameEvent],
    window: WindowId,
    refresh: &mut Duration,
) {
    for event in events {
        match event {
            FrameEvent::Draw(t) if t.window_id == window => {
                stats.record_draw(t.draw_end.saturating_duration_since(t.draw_start));
                if let Some(r) = t.refresh_interval {
                    *refresh = r;
                }
            }
            FrameEvent::Present(t) if t.window_id == window => {
                if let Some(i) = t.animation_interval {
                    stats.record_interval(i, *refresh);
                }
            }
            _ => {}
        }
    }
}

pub struct FrameHud {
    stats: FrameStats,
    refresh: Duration,
    visible: bool,
    collector: FrameTimingCollector,
    poll: Option<Task<()>>,
    window_id: WindowId,
}

impl FrameHud {
    /// Creates a hidden HUD for `window`.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.on_release(|hud, cx| {
            if hud.visible {
                hud.visible = false;
                remove_trace_user(cx);
            }
        })
        .detach();
        Self {
            stats: FrameStats::default(),
            refresh: DEFAULT_REFRESH,
            visible: false,
            collector: FrameTimingCollector::new(),
            poll: None,
            window_id: window.window_handle().window_id(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn stats(&self) -> &FrameStats {
        &self.stats
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_visible(!self.visible, window, cx);
    }

    pub fn set_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        if visible {
            add_trace_user(cx);
            self.collector = FrameTimingCollector::new();
            self.poll = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL_INTERVAL).await;
                    let alive = this
                        .update(cx, |hud, cx| {
                            let events = hud.collector.collect_unseen();
                            let (id, mut refresh) = (hud.window_id, hud.refresh);
                            ingest(&mut hud.stats, &events, id, &mut refresh);
                            hud.refresh = refresh;
                            cx.notify();
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            }));
        } else {
            self.poll = None;
            remove_trace_user(cx);
        }
        cx.notify();
    }
}

fn row(
    label: &'static str,
    p: Option<stats::Percentiles>,
    unit: &'static str,
    cx: &App,
) -> impl IntoElement {
    let t = cx.tokens();
    let value = |d: Option<std::time::Duration>| d.map_or("–".to_string(), stats::format_ms);
    let cell = |name: &'static str, d| {
        div()
            .flex()
            .gap(space::S4)
            .child(div().text_color(Hsla::from(t.text_muted)).child(name))
            .child(value(d))
    };
    div()
        .flex()
        .gap(space::S6)
        .child(div().text_color(Hsla::from(t.text_muted)).child(label))
        .child(cell("p50", p.map(|p| p.p50)))
        .child(cell("p95", p.map(|p| p.p95)))
        .child(cell("p99", p.map(|p| p.p99)))
        .child(div().text_color(Hsla::from(t.text_muted)).child(unit))
}

impl Render for FrameHud {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }
        let t = cx.tokens();
        let refresh = self.refresh;
        let samples: Vec<Duration> = {
            let all: Vec<Duration> = self.stats.recent_draws().collect();
            all[all.len().saturating_sub(GRAPH_SAMPLES)..].to_vec()
        };
        let (success, warning, danger, line) = (
            Hsla::from(t.success),
            Hsla::from(t.warning),
            Hsla::from(t.danger),
            Hsla::from(t.text_faint),
        );

        let graph = canvas(
            |_, _, _| (),
            move |bounds: Bounds<gpui_kit::Pixels>, _, window, _| {
                let h = bounds.size.height;
                let w = bounds.size.width;
                // `refresh` sits at half height.
                let scale = |d: Duration| {
                    let frac = (d.as_secs_f32() / (2.0 * refresh.as_secs_f32())).clamp(0.0, 1.0);
                    h * frac
                };
                let bar_w = w / GRAPH_SAMPLES as f32;
                for (i, d) in samples.iter().enumerate() {
                    let bar_h = scale(*d).max(layout::BORDER);
                    let color = if *d <= FAST_DRAW {
                        success
                    } else if *d <= refresh {
                        warning
                    } else {
                        danger
                    };
                    let x = bounds.origin.x + bar_w * i as f32;
                    let y = bounds.origin.y + h - bar_h;
                    window.paint_quad(fill(Bounds::new(point(x, y), size(bar_w, bar_h)), color));
                }
                let y = bounds.origin.y + h / 2.0;
                window.paint_quad(fill(
                    Bounds::new(point(bounds.origin.x, y), size(w, layout::BORDER)),
                    line,
                ));
            },
        )
        .w_full()
        .h(layout::HUD_GRAPH_HEIGHT);

        div()
            .id("frame-hud")
            .test_support()
            .absolute()
            .top(space::S12)
            .right(space::S12)
            .w(layout::HUD_WIDTH)
            .bg(Hsla::from(t.elevated))
            .border(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .rounded(radius::PANEL)
            .p(space::S12)
            .flex()
            .flex_col()
            .gap(space::S6)
            .font_family(font::MONO)
            .text_size(font::LABEL)
            .font_features(font::tabular_figures())
            .text_color(Hsla::from(t.text))
            .child(row("Draw  ", self.stats.draw_percentiles(), "ms", cx))
            .child(row("Frame ", self.stats.interval_percentiles(), "ms", cx))
            .child(
                div()
                    .flex()
                    .gap(space::S6)
                    .child(div().text_color(Hsla::from(t.text_muted)).child("Dropped"))
                    .child(self.stats.dropped_frames().to_string()),
            )
            .child(graph)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use gpui_kit::WindowId;
    use gpui_kit::profiler::{DrawPhases, FrameEvent, FrameTiming, PresentTiming};
    use gpui_kit::test::TestWindowExt as _;
    use std::time::{Duration, Instant};

    fn draw(win: u64, dur: Duration, refresh: Option<Duration>) -> FrameEvent {
        let start = Instant::now();
        FrameEvent::Draw(FrameTiming {
            window_id: WindowId::from(win),
            dirty_at: None,
            invalidations: 1,
            draw_start: start,
            draw_end: start + dur,
            refresh_interval: refresh,
            signal_at: None,
            phases: DrawPhases::default(),
        })
    }

    fn present(win: u64, interval: Option<Duration>) -> FrameEvent {
        let now = Instant::now();
        FrameEvent::Present(PresentTiming {
            window_id: WindowId::from(win),
            present_start: now,
            present_end: now,
            animation_interval: interval,
        })
    }

    #[test]
    fn ingest_filters_by_window_and_tracks_refresh() {
        let mut stats = FrameStats::new(100);
        let mut refresh = DEFAULT_REFRESH;
        let events = [
            draw(
                1,
                Duration::from_millis(3),
                Some(Duration::from_secs_f64(0.008333)),
            ),
            draw(2, Duration::from_millis(9), None),
            present(1, Some(Duration::from_millis(25))),
            present(1, None),
        ];
        ingest(&mut stats, &events, WindowId::from(1u64), &mut refresh);
        assert_eq!(
            stats.draw_percentiles().unwrap().p50,
            Duration::from_millis(3)
        );
        assert_eq!(stats.recent_draws().count(), 1);
        assert_eq!(stats.dropped_frames(), 2);
        assert_eq!(refresh, Duration::from_secs_f64(0.008333));
    }

    fn exists(cx: &mut gpui_kit::VisualTestContext, id: &'static str) -> bool {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
    }

    #[gpui_kit::test]
    fn hud_is_hidden_by_default_and_toggles(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (hud, cx) = cx.add_window_view(FrameHud::new);
        assert!(!exists(cx, "frame-hud"));
        assert_eq!(cx.update(|_, cx| trace_users(cx)), 0);

        cx.update(|window, cx| hud.update(cx, |h, cx| h.toggle(window, cx)));
        assert!(exists(cx, "frame-hud"));
        assert_eq!(cx.update(|_, cx| trace_users(cx)), 1);

        cx.update(|window, cx| hud.update(cx, |h, cx| h.toggle(window, cx)));
        assert!(!exists(cx, "frame-hud"));
        assert_eq!(cx.update(|_, cx| trace_users(cx)), 0);
    }

    #[gpui_kit::test]
    fn releasing_a_visible_hud_stops_tracing(app: &mut TestAppContext) {
        app.update(gpui_kit::init);
        let (hud, cx) = app.add_window_view(FrameHud::new);
        cx.update(|window, cx| hud.update(cx, |h, cx| h.set_visible(true, window, cx)));
        assert_eq!(cx.update(|_, cx| trace_users(cx)), 1);

        cx.update(|window, _| window.remove_window());
        drop(hud);
        app.run_until_parked();
        app.update(|_| {});
        assert_eq!(app.read(trace_users), 0);
    }
}
