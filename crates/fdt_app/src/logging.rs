//! A tiny stderr logger so `log::warn!` and `log::error!` are not silently dropped.

use log::{Level, LevelFilter, Log, Metadata, Record};

/// The environment variable that raises the log level (`info`, `debug` or `trace`).
pub const LEVEL_VAR: &str = "FDT_LOG";

struct StderrLogger {
    level: LevelFilter,
}

/// Parses an `FDT_LOG` value; anything unknown (or unset) means warnings and errors only.
pub fn parse_level(value: Option<&str>) -> LevelFilter {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("info") => LevelFilter::Info,
        Some("debug") => LevelFilter::Debug,
        Some("trace") => LevelFilter::Trace,
        _ => LevelFilter::Warn,
    }
}

/// Formats one record as `fdt <LEVEL> <target>: <message>`.
pub fn format_record(level: Level, target: &str, message: &std::fmt::Arguments<'_>) -> String {
    format!("fdt {level} {target}: {message}")
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "{}",
                format_record(record.level(), record.target(), record.args())
            );
        }
    }

    fn flush(&self) {}
}

/// Installs the stderr logger. Call once, first thing in `main`; later calls are ignored.
pub fn init() {
    let level = parse_level(std::env::var(LEVEL_VAR).ok().as_deref());
    if log::set_boxed_logger(Box::new(StderrLogger { level })).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_defaults_to_warn() {
        assert_eq!(parse_level(None), LevelFilter::Warn);
        assert_eq!(parse_level(Some("")), LevelFilter::Warn);
        assert_eq!(parse_level(Some("nonsense")), LevelFilter::Warn);
    }

    #[test]
    fn level_can_be_raised() {
        assert_eq!(parse_level(Some("info")), LevelFilter::Info);
        assert_eq!(parse_level(Some("DEBUG")), LevelFilter::Debug);
        assert_eq!(parse_level(Some(" trace ")), LevelFilter::Trace);
    }

    #[test]
    fn records_are_formatted_with_level_and_target() {
        let text = format_record(Level::Warn, "fdt_app::settings", &format_args!("bad {}", 1));
        assert_eq!(text, "fdt WARN fdt_app::settings: bad 1");
    }

    #[test]
    fn the_logger_filters_by_level() {
        let logger = StderrLogger {
            level: LevelFilter::Warn,
        };
        let meta = |l| Metadata::builder().level(l).build();
        assert!(logger.enabled(&meta(Level::Error)));
        assert!(logger.enabled(&meta(Level::Warn)));
        assert!(!logger.enabled(&meta(Level::Info)));
    }
}
