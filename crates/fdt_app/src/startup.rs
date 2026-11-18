//! Cold-start measurement report for `fdt --startup-trace`.

use std::io::Write;
use std::time::{Duration, Instant};

/// Time from `main()` entry to first paint and to an interactive (palette drawn) window.
#[derive(Clone, Debug, PartialEq)]
pub struct StartupReport {
    pub first_paint: Duration,
    pub interactive: Duration,
    /// Elapsed-from-`t0` timestamp at the end of each startup step, in order.
    pub phases: Vec<(&'static str, Duration)>,
}

/// How long the trace may run before it gives up.
pub const WATCHDOG: Duration = Duration::from_secs(10); // token-check: allow: watchdog timeout

/// Writes the report line to `out` and returns whether the trace succeeded.
///
/// The report is always printed. If the palette did not open, a one-line note goes to `err` and
/// the trace counts as failed, because "interactive" would not mean what it says.
pub fn emit(
    report: &StartupReport,
    palette_open: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> bool {
    let _ = writeln!(out, "{}", report.to_json());
    if !palette_open {
        let _ = writeln!(err, "startup-trace: palette did not open");
    }
    palette_open
}

/// Collects named elapsed-from-`t0` timestamps while the trace variant starts up.
#[derive(Debug)]
pub struct StartupPhases {
    t0: Instant,
    marks: Vec<(&'static str, Duration)>,
}

impl StartupPhases {
    pub fn new(t0: Instant) -> Self {
        Self {
            t0,
            marks: Vec::new(),
        }
    }

    /// Records that `name` just completed.
    pub fn mark(&mut self, name: &'static str) {
        self.marks.push((name, self.t0.elapsed()));
    }

    pub fn into_marks(self) -> Vec<(&'static str, Duration)> {
        self.marks
    }
}

/// Records a phase when a recorder is present; free otherwise.
pub fn mark(phases: &mut Option<&mut StartupPhases>, name: &'static str) {
    if let Some(p) = phases {
        p.mark(name);
    }
}

impl StartupReport {
    /// One JSON line, milliseconds with one decimal:
    /// `{"first_paint_ms":123.4,"interactive_ms":234.6,"phases":{"a":1.0}}`.
    pub fn to_json(&self) -> String {
        let phases: Vec<String> = self
            .phases
            .iter()
            .map(|(name, d)| format!("\"{name}\":{:.1}", d.as_secs_f64() * 1000.0))
            .collect();
        format!(
            "{{\"first_paint_ms\":{:.1},\"interactive_ms\":{:.1},\"phases\":{{{}}}}}",
            self.first_paint.as_secs_f64() * 1000.0,
            self.interactive.as_secs_f64() * 1000.0,
            phases.join(",")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_json_uses_one_decimal_milliseconds() {
        let report = StartupReport {
            first_paint: Duration::from_micros(123_440),
            interactive: Duration::from_micros(234_560),
            phases: Vec::new(),
        };
        assert_eq!(
            report.to_json(),
            r#"{"first_paint_ms":123.4,"interactive_ms":234.6,"phases":{}}"#
        );
    }

    #[test]
    fn report_json_lists_phases_in_order() {
        let report = StartupReport {
            first_paint: Duration::from_millis(10),
            interactive: Duration::from_millis(20),
            phases: vec![
                ("settings_loaded", Duration::from_micros(1_250)),
                ("app_started", Duration::from_micros(5_000)),
            ],
        };
        assert_eq!(
            report.to_json(),
            r#"{"first_paint_ms":10.0,"interactive_ms":20.0,"phases":{"settings_loaded":1.2,"app_started":5.0}}"#
        );
    }

    #[test]
    fn emit_prints_json_and_succeeds_when_the_palette_is_open() {
        let report = StartupReport {
            first_paint: Duration::from_millis(1),
            interactive: Duration::from_millis(2),
            phases: Vec::new(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(emit(&report, true, &mut out, &mut err));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\n", report.to_json())
        );
        assert!(err.is_empty());
    }

    #[test]
    fn emit_still_prints_json_but_fails_when_the_palette_did_not_open() {
        let report = StartupReport {
            first_paint: Duration::from_millis(1),
            interactive: Duration::from_millis(2),
            phases: Vec::new(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(!emit(&report, false, &mut out, &mut err));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\n", report.to_json())
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "startup-trace: palette did not open\n"
        );
    }

    #[test]
    fn marks_are_recorded_in_order_and_skipped_without_recorder() {
        let mut phases = StartupPhases::new(Instant::now());
        mark(&mut Some(&mut phases), "a");
        mark(&mut Some(&mut phases), "b");
        mark(&mut None, "ignored");
        let names: Vec<_> = phases.into_marks().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["a", "b"]);
    }
}
