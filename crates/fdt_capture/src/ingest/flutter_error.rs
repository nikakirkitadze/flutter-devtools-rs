//! Decoding `Flutter.Error` extension events (debug mode only).

use fdt_store::clock::Stamp;
use fdt_store::logs::{ErrorDetail, LogInput, LogLevel, LogSource};
use fdt_store::time::RunId;
use serde_json::Value;

const WIDGET_BLOCK: &str = "The relevant error-causing widget was";

fn desc(v: &Value) -> Option<&str> {
    v.get("description").and_then(Value::as_str)
}

fn props(v: &Value) -> &[Value] {
    v.get("properties")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn kind(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or_default()
}

/// Reads the `extensionData` of a `Flutter.Error` event.
pub(crate) fn parse_flutter_error(data: &Value) -> ErrorDetail {
    let description = desc(data).unwrap_or_default().to_string();
    let summary = props(data)
        .iter()
        .find(|p| kind(p) == "ErrorSummary")
        .and_then(desc)
        .unwrap_or(&description)
        .to_string();

    let mut widget = None;
    let mut location = None;
    let block = props(data).iter().find(|p| {
        p.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.starts_with(WIDGET_BLOCK))
    });
    if let Some(first) = block
        .and_then(|b| b.get("children"))
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(desc)
    {
        let mut it = first.trim().splitn(2, char::is_whitespace);
        if let Some(name) = it.next().filter(|n| !n.is_empty()) {
            widget = Some(name.to_string());
            if let Some(rest) = it.next().map(str::trim).filter(|r| !r.is_empty()) {
                let prefix = format!("{name}:");
                location = Some(rest.strip_prefix(&prefix).unwrap_or(rest).to_string());
            }
        }
    }

    let stack = props(data)
        .iter()
        .find(|p| kind(p) == "DiagnosticsStackTrace")
        .map(|t| {
            props(t)
                .iter()
                .filter_map(desc)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let since_reload = data.get("errorsSinceReload").and_then(Value::as_i64);
    let full = data.get("renderedErrorText").and_then(Value::as_str);
    let rendered = match (since_reload.unwrap_or(0), full) {
        (0, Some(text)) => text.to_string(),
        _ => props(data)
            .iter()
            .filter_map(desc)
            .collect::<Vec<_>>()
            .join("\n"),
    };

    ErrorDetail {
        description,
        summary,
        widget,
        location,
        stack,
        rendered,
    }
}

/// The `Error` record for a `Flutter.Error` event.
pub(crate) fn record(run: RunId, at: Stamp, data: &Value) -> LogInput {
    let detail = parse_flutter_error(data);
    LogInput {
        run,
        time: at.time,
        clamped: at.clamped,
        level: LogLevel::Error,
        source: LogSource::FlutterError,
        logger: String::new(),
        text: format!("{}: {}", detail.description, detail.summary),
        error: Some(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_flutter_error_from_recording() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/recordings/jank_lab-debug.jsonl");
        let text = std::fs::read_to_string(path).unwrap();
        let mut errors = text.lines().filter_map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            let ev = v["msg"]["params"]["event"].clone();
            (ev["extensionKind"] == "Flutter.Error").then(|| ev["extensionData"].clone())
        });
        let first = parse_flutter_error(&errors.next().unwrap());
        assert_eq!(first.summary, "Bad state: Reported by jank_lab");
        let d = parse_flutter_error(&errors.next().unwrap());
        assert_eq!(d.summary, "Bad state: Deliberate failure in build");
        assert_eq!(d.description, "Exception caught by widgets library");
        assert_eq!(d.widget.as_deref(), Some("_Thrower"));
        let loc = d.location.unwrap();
        assert!(!loc.starts_with("_Thrower"), "{loc}");
        let re = regex::Regex::new(r"logs\.dart:\d+:\d+$").unwrap();
        assert!(re.is_match(&loc), "{loc}");
        assert!(!d.stack.is_empty());
        // errorsSinceReload is 1 here, so every property is joined.
        assert!(
            d.rendered
                .contains("Bad state: Deliberate failure in build")
        );
        assert!(
            d.rendered.contains("The relevant error-causing widget was")
                || d.rendered.contains("_Thrower")
        );
    }

    #[test]
    fn rendered_text_is_used_for_the_first_error_since_reload() {
        let d = parse_flutter_error(&json!({
            "description": "d", "errorsSinceReload": 0, "renderedErrorText": "FULL",
            "properties": [{"type": "ErrorSummary", "description": "s"}]
        }));
        assert_eq!(d.rendered, "FULL");
        assert_eq!(d.summary, "s");
        let d = parse_flutter_error(&json!({"description": "just this"}));
        assert_eq!(d.summary, "just this");
        assert_eq!(d.widget, None);
    }
}
