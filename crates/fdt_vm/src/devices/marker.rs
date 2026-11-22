//! Pure parsing of the "Dart VM service is listening" log line from simulator and Android logs.

use std::sync::LazyLock;

use regex::Regex;

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"The Dart VM service is listening on (http://\S+)").expect("valid regex")
});

static ANDROID_PID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\w/\S+\s*\(\s*(\d+)\)").expect("valid regex"));

/// A VM service URL announced by a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub pid: u32,
    pub url: String,
}

fn marker_url(message: &str) -> Option<String> {
    let url = MARKER.captures(message)?.get(1)?.as_str();
    is_loopback_url(url).then(|| url.to_string())
}

/// Parses one line of `log stream --style ndjson`, taking the process id and the announced URL.
pub fn simulator_hit(ndjson_line: &str) -> Option<Hit> {
    let value: serde_json::Value = serde_json::from_str(ndjson_line).ok()?;
    let pid = u32::try_from(value.get("processID")?.as_u64()?).ok()?;
    let message = value.get("eventMessage")?.as_str()?;
    // The log tool's own invocation line quotes the marker without a URL, so it never matches.
    let url = marker_url(message)?;
    Some(Hit { pid, url })
}

/// Parses one `logcat -v brief` line such as
/// `I/flutter ( 9677): The Dart VM service is listening on http://127.0.0.1:40421/abc=/`.
pub fn android_hit(brief_line: &str) -> Option<Hit> {
    let pid = ANDROID_PID
        .captures(brief_line)?
        .get(1)?
        .as_str()
        .parse()
        .ok()?;
    let url = marker_url(brief_line)?;
    Some(Hit { pid, url })
}

/// Whether `url` points at this machine (`127.0.0.1` or `localhost`).
pub fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host == "127.0.0.1" || host == "localhost"
}

/// Cleans an adb `model:` value the way Flutter's `cleanAdbDeviceName` does, so names match the
/// labels DTD reports: `___` becomes `, `, `_` becomes a space, whitespace collapses.
pub fn clean_adb_device_name(model: &str) -> String {
    let spaced = model.replace("___", ", ").replace('_', " ");
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_reads_pid_and_url() {
        let line = r#"{"processID":62710,"processImagePath":"/…/Runner.app/Runner","eventMessage":"flutter: The Dart VM service is listening on http://127.0.0.1:61865/8JqAbc=/","timestamp":"2026-11-22 21:22:19.396005+0400"}"#;
        assert_eq!(
            simulator_hit(line),
            Some(Hit {
                pid: 62710,
                url: "http://127.0.0.1:61865/8JqAbc=/".into()
            })
        );
    }

    #[test]
    fn simulator_accepts_a_line_without_the_flutter_prefix() {
        let line = r#"{"processID":62710,"eventMessage":"The Dart VM service is listening on http://127.0.0.1:61865/8JqAbc=/"}"#;
        assert_eq!(
            simulator_hit(line),
            Some(Hit {
                pid: 62710,
                url: "http://127.0.0.1:61865/8JqAbc=/".into()
            })
        );
    }

    #[test]
    fn simulator_ignores_the_log_tools_own_invocation_line() {
        let line = r#"{"processID":18471,"processImagePath":"/…/usr/bin/log","eventMessage":"log run noninteractively, parent: 61916 (launchd_sim), args: 'log' 'stream' '--predicate' 'eventMessage CONTAINS \"Dart VM service is listening\"'"}"#;
        assert_eq!(simulator_hit(line), None);
    }

    #[test]
    fn simulator_ignores_non_json_and_other_messages() {
        assert_eq!(
            simulator_hit(r#"Filtering the log data using "eventMessage CONTAINS x""#),
            None
        );
        let line = r#"{"processID":1,"eventMessage":"flutter: hello"}"#;
        assert_eq!(simulator_hit(line), None);
    }

    #[test]
    fn android_reads_space_padded_pid() {
        let tail = "The Dart VM service is listening on http://127.0.0.1:40421/AbC-_=/";
        assert_eq!(
            android_hit(&format!("I/flutter ( 9677): {tail}")),
            Some(Hit {
                pid: 9677,
                url: "http://127.0.0.1:40421/AbC-_=/".into()
            })
        );
        assert_eq!(
            android_hit(&format!("I/flutter (12345): {tail}")).map(|h| h.pid),
            Some(12345)
        );
    }

    #[test]
    fn android_ignores_other_flutter_lines() {
        let line = "I/flutter ( 9739): [IMPORTANT:flutter/shell/platform/android/android_context_vk_impeller.cc(62)] Using the Impeller rendering backend (OpenGLES).";
        assert_eq!(android_hit(line), None);
    }

    #[test]
    fn only_loopback_urls_count() {
        assert!(is_loopback_url("http://127.0.0.1:1/a=/"));
        assert!(is_loopback_url("http://localhost:1/a=/"));
        assert!(!is_loopback_url("http://192.168.1.5:1/a=/"));
        let tail = "The Dart VM service is listening on http://192.168.1.5:1/a=/";
        assert_eq!(android_hit(&format!("I/flutter ( 1): {tail}")), None);
        let line = format!(r#"{{"processID":1,"eventMessage":"flutter: {tail}"}}"#);
        assert_eq!(simulator_hit(&line), None);
    }

    #[test]
    fn adb_names_are_cleaned_like_flutter() {
        assert_eq!(
            clean_adb_device_name("sdk_gphone64_arm64"),
            "sdk gphone64 arm64"
        );
        assert_eq!(clean_adb_device_name("Pixel___Tablet_x"), "Pixel, Tablet x");
        assert_eq!(clean_adb_device_name("  a__b "), "a b");
    }
}
