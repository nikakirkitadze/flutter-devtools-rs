//! Cold-start measurement report for `fdt --startup-trace`.

use std::time::Duration;

/// Time from `main()` entry to first paint and to an interactive (palette drawn) window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartupReport {
    pub first_paint: Duration,
    pub interactive: Duration,
}

impl StartupReport {
    /// One JSON line, milliseconds with one decimal: `{"first_paint_ms":123.4,"interactive_ms":234.6}`.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"first_paint_ms\":{:.1},\"interactive_ms\":{:.1}}}",
            self.first_paint.as_secs_f64() * 1000.0,
            self.interactive.as_secs_f64() * 1000.0
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
        };
        assert_eq!(
            report.to_json(),
            r#"{"first_paint_ms":123.4,"interactive_ms":234.6}"#
        );
    }
}
