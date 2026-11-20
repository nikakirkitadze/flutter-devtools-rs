//! The visible time window of a chart, lane or track. Pure: plain `u64` nanoseconds and `f32`
//! pixels, no GPUI.

/// The narrowest window a viewport zooms into.
pub const MIN_SPAN_NS: u64 = 1_000;
/// How far past the data a viewport may zoom out, as a multiple of the data's span.
const MAX_SPAN_FACTOR: f64 = 1.1;
/// The share of the narrower of view and data that must stay in view.
const MIN_OVERLAP: f64 = 0.05;
/// Scroll pixels to the exponent of the zoom factor.
const SCROLL_ZOOM_RATE: f32 = 0.004;
const PINCH_MIN: f32 = 0.2;
const PINCH_MAX: f32 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub start_ns: u64,
    pub span_ns: u64,
}

impl Viewport {
    /// A viewport showing `[start_ns, end_ns]`, at least `MIN_SPAN_NS` wide.
    pub fn fit(start_ns: u64, end_ns: u64) -> Self {
        Self {
            start_ns,
            span_ns: end_ns.saturating_sub(start_ns).max(MIN_SPAN_NS),
        }
    }

    /// `(start, end)` in nanoseconds.
    pub fn range(&self) -> (u64, u64) {
        (self.start_ns, self.start_ns.saturating_add(self.span_ns))
    }

    /// The x of `t_ns` in a view `width` pixels wide. Left of the view it is negative.
    pub fn x_of(&self, t_ns: u64, width: f32) -> f32 {
        let dt = t_ns as f64 - self.start_ns as f64;
        (dt / self.span_ns.max(1) as f64 * f64::from(width)) as f32
    }

    /// The time at `x`, never before 0.
    pub fn time_at(&self, x: f32, width: f32) -> u64 {
        if (width.is_nan() || width <= 0.0) || !x.is_finite() {
            return self.start_ns;
        }
        let t = self.start_ns as f64 + f64::from(x) / f64::from(width) * self.span_ns as f64;
        t.max(0.0) as u64
    }

    /// Zooms about `x` so the time under it stays put. `factor > 1` zooms in. A factor that is not
    /// finite or not positive, or a width that is not positive, changes nothing.
    pub fn zoom_about(&mut self, x: f32, width: f32, factor: f32, bounds: (u64, u64)) {
        if !factor.is_finite()
            || factor <= 0.0
            || (width.is_nan() || width <= 0.0)
            || !x.is_finite()
        {
            return;
        }
        let frac = (f64::from(x) / f64::from(width)).clamp(0.0, 1.0);
        let anchor = self.start_ns as f64 + frac * self.span_ns as f64;
        let span = self.span_ns as f64 / f64::from(factor);
        let span = span.clamp(MIN_SPAN_NS as f64, max_span(bounds));
        self.span_ns = span.round() as u64;
        self.start_ns = clamp_start(anchor - frac * span, span, bounds);
    }

    /// Moves the view by `dx` pixels: positive shows later time. Keeps the clamps of `zoom_about`.
    pub fn pan_px(&mut self, dx: f32, width: f32, bounds: (u64, u64)) {
        if !dx.is_finite() || (width.is_nan() || width <= 0.0) {
            return;
        }
        let shift = f64::from(dx) / f64::from(width) * self.span_ns as f64;
        self.start_ns = clamp_start(self.start_ns as f64 + shift, self.span_ns as f64, bounds);
    }
}

fn bounds_span(bounds: (u64, u64)) -> f64 {
    bounds.1.saturating_sub(bounds.0) as f64
}

fn max_span(bounds: (u64, u64)) -> f64 {
    (bounds_span(bounds) * MAX_SPAN_FACTOR).max(MIN_SPAN_NS as f64)
}

/// `start` limited so the view `[start, start + span]` overlaps `bounds` by `MIN_OVERLAP` of the
/// narrower of the two. Never before 0.
fn clamp_start(start: f64, span: f64, bounds: (u64, u64)) -> u64 {
    let overlap = MIN_OVERLAP * span.min(bounds_span(bounds));
    let lo = (bounds.0 as f64 + overlap - span).max(0.0);
    let hi = (bounds.1 as f64 - overlap).max(lo);
    start.clamp(lo, hi).round() as u64
}

/// The zoom factor of a vertical scroll of `delta_y_px`. Positive (scrolling up) zooms in.
pub fn scroll_zoom_factor(delta_y_px: f32) -> f32 {
    (delta_y_px * SCROLL_ZOOM_RATE).exp()
}

/// The zoom factor of a pinch of `delta` (0.1 is 10 % more).
pub fn pinch_zoom_factor(delta: f32) -> f32 {
    (1.0 + delta).clamp(PINCH_MIN, PINCH_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so the property tests need no crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }
        fn unit(&mut self) -> f32 {
            (self.next() % 10_000) as f32 / 10_000.0
        }
    }

    #[test]
    fn zoom_round_trips_and_keeps_the_time_under_x() {
        let mut r = Lcg(7);
        for _ in 0..1_000 {
            let b0 = r.range(0, 1_000_000_000);
            let bounds = (b0, b0 + r.range(100_000_000, 60_000_000_000));
            let bspan = bounds.1 - bounds.0;
            // Start inside the bounds with a span well inside the clamps.
            let span = r.range(bspan / 20, bspan / 2).max(MIN_SPAN_NS * 100);
            let start = r.range(bounds.0 + bspan / 4, bounds.0 + bspan / 2);
            let v = Viewport {
                start_ns: start,
                span_ns: span,
            };
            let width = 200.0 + r.unit() * 2_000.0;
            let x = r.unit() * width;
            let factor = 1.1 + r.unit() * 1.5;

            let mut z = v;
            z.zoom_about(x, width, factor, bounds);
            let tol = 2 + span / 1_000_000;
            let (a, b) = (v.time_at(x, width), z.time_at(x, width));
            assert!(a.abs_diff(b) <= tol, "anchor moved {a} -> {b}");
            assert!(z.span_ns >= MIN_SPAN_NS && z.span_ns as f64 <= max_span(bounds));

            z.zoom_about(x, width, 1.0 / factor, bounds);
            assert!(
                z.span_ns.abs_diff(v.span_ns) <= tol && z.start_ns.abs_diff(v.start_ns) <= tol,
                "{v:?} -> {z:?}"
            );
        }
    }

    #[test]
    fn span_stays_within_its_clamps() {
        let mut r = Lcg(11);
        for _ in 0..1_000 {
            let b0 = r.range(0, 1_000_000);
            let bounds = (b0, b0 + r.range(0, 10_000_000_000));
            let mut v = Viewport::fit(bounds.0, bounds.1);
            for _ in 0..8 {
                let f = 0.05 + r.unit() * 20.0;
                v.zoom_about(r.unit() * 500.0, 500.0, f, bounds);
                v.pan_px((r.unit() - 0.5) * 2_000.0, 500.0, bounds);
                assert!(v.span_ns >= MIN_SPAN_NS);
                assert!(v.span_ns as f64 <= max_span(bounds) + 1.0);
            }
        }
    }

    #[test]
    fn hostile_factors_never_break_the_viewport() {
        let bounds = (1_000, 5_000_000);
        for f in [0.0, -1.0, f32::INFINITY, f32::NAN, 1e9, 1e-9] {
            let mut v = Viewport::fit(bounds.0, bounds.1);
            v.zoom_about(100.0, 400.0, f, bounds);
            assert!(v.span_ns >= MIN_SPAN_NS, "factor {f}");
            assert!(v.span_ns as f64 <= max_span(bounds) + 1.0, "factor {f}");
            assert!(v.x_of(v.start_ns, 400.0).is_finite());
        }
        let before = Viewport::fit(0, 10_000);
        let mut v = before;
        v.zoom_about(1.0, 0.0, 2.0, (0, 10_000));
        v.zoom_about(f32::NAN, 100.0, 2.0, (0, 10_000));
        assert_eq!(v, before);
    }

    #[test]
    fn empty_bounds_keep_the_minimum_span() {
        let mut v = Viewport::fit(0, 0);
        assert_eq!(v.span_ns, MIN_SPAN_NS);
        for f in [0.5, 2.0, 1e9, 1e-9] {
            v.zoom_about(10.0, 100.0, f, (0, 0));
            assert_eq!(v.span_ns, MIN_SPAN_NS);
        }
        v.pan_px(1e9, 100.0, (0, 0));
        assert_eq!(v.span_ns, MIN_SPAN_NS);
    }

    #[test]
    fn pan_clamps_to_bounds() {
        let bounds = (1_000_000, 11_000_000);
        let mut v = Viewport::fit(bounds.0, bounds.0 + 1_000_000);
        v.pan_px(1e9, 100.0, bounds);
        let (s, e) = v.range();
        // At least 5 % of the view (50 us) is still on the data.
        assert!(s <= bounds.1 - 50_000 && e > bounds.1 - 100_000, "{s} {e}");
        v.pan_px(-1e9, 100.0, bounds);
        let (s, e) = v.range();
        assert!(e >= bounds.0 + 50_000 && s < bounds.0, "{s} {e}");
        // Near zero the start does not underflow.
        let mut z = Viewport::fit(0, 1_000_000);
        z.pan_px(-1e9, 100.0, (0, 10_000_000));
        assert_eq!(z.start_ns, 0);
        // A small pan moves by the right amount.
        let mut m = Viewport::fit(2_000_000, 3_000_000);
        m.pan_px(10.0, 100.0, bounds);
        assert_eq!(m.start_ns, 2_100_000);
    }

    #[test]
    fn x_and_time_are_inverse() {
        let v = Viewport::fit(1_000_000, 3_000_000);
        assert_eq!(v.x_of(2_000_000, 400.0), 200.0);
        assert_eq!(v.time_at(200.0, 400.0), 2_000_000);
        assert!(v.x_of(0, 400.0) < 0.0);
        assert_eq!(v.time_at(-1e12, 400.0), 0);
    }

    #[test]
    fn scroll_and_pinch_factors() {
        assert_eq!(scroll_zoom_factor(0.0), 1.0);
        assert!(scroll_zoom_factor(50.0) > 1.0);
        assert!(scroll_zoom_factor(-50.0) < 1.0);
        assert!((scroll_zoom_factor(100.0) - (0.4f32).exp()).abs() < 1e-6);
        assert!((scroll_zoom_factor(30.0) * scroll_zoom_factor(-30.0) - 1.0).abs() < 1e-6);
        assert_eq!(pinch_zoom_factor(0.1), 1.1);
        assert_eq!(pinch_zoom_factor(-5.0), 0.2);
        assert_eq!(pinch_zoom_factor(50.0), 5.0);
    }
}
