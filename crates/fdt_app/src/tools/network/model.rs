//! The pure part of the Network table: how a request's fields read in a cell, where its
//! waterfall bar sits and what the row's menu copies. No GPUI.

use crate::nav::{SessionTime, TimeRange};
use crate::shell::strip::format_offset;
use fdt_store::net::{HttpRecord, SocketRecord, StatusClass};
use fdt_store::net_query::NetTab;
use serde::Serialize;

/// The smallest waterfall bar, as a fraction of the track (about one pixel).
pub const MIN_BAR: f32 = 0.002;

/// `0 B`, `999 B`, `1.0 KB`, `1.4 MB`, `2.0 GB` (base 1024, one decimal from KB).
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    // 1023.96 KB rounds to 1024.0: say 1.0 MB instead.
    if unit > 0 && unit < UNITS.len() - 1 && format!("{v:.1}") == "1024.0" {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// `850 µs`, `182 ms`, `1.4 s`, and `1:05` from a minute on.
pub fn format_duration_ns(ns: u64) -> String {
    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000_000_000;
    if ns < MS {
        format!("{} \u{b5}s", ns / 1_000)
    } else if ns < SEC {
        format!("{} ms", ns / MS)
    } else if ns < 59_950_000_000 {
        format!("{:.1} s", ns as f64 / SEC as f64)
    } else {
        let secs = (ns as f64 / SEC as f64).round() as u64;
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// `200`, `Failed`, `Pending` or `101`.
pub fn status_text(r: &HttpRecord) -> String {
    match r.status_class() {
        StatusClass::Failed => "Failed".to_string(),
        StatusClass::Pending => "Pending".to_string(),
        _ => r.status.map(|s| s.to_string()).unwrap_or_default(),
    }
}

/// How a status is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Muted,
    Warning,
    Danger,
}

pub fn status_tone(r: &HttpRecord) -> Tone {
    match r.status_class() {
        StatusClass::Success => Tone::Success,
        StatusClass::Informational | StatusClass::Redirect | StatusClass::Pending => Tone::Muted,
        StatusClass::ClientError => Tone::Warning,
        StatusClass::ServerError | StatusClass::Failed => Tone::Danger,
    }
}

/// The left edge and width of a request's waterfall bar, as fractions of `span`. A pending
/// request runs to `now`; every bar is at least [`MIN_BAR`] wide and stays inside the track.
pub fn waterfall(r: &HttpRecord, span: TimeRange, now: SessionTime) -> (f32, f32) {
    let total = span.duration_ns() as f64;
    if total <= 0.0 {
        return (0.0, MIN_BAR);
    }
    let frac = |t: SessionTime| (t.0 as f64 - span.start.0 as f64) / total;
    let start = frac(r.start).clamp(0.0, 1.0);
    let end = frac(r.end.unwrap_or(now).max(r.start)).clamp(start, 1.0);
    let min = f64::from(MIN_BAR);
    let width = (end - start).max(min);
    let x = start.min(1.0 - width);
    (x as f32, width as f32)
}

/// `m:ss.mmm` on the session axis.
pub fn format_time(t: SessionTime) -> String {
    format!("{}.{:03}", format_offset(t), (t.0 / 1_000_000) % 1_000)
}

/// "Open" or "Closed" from the linked socket, else "Unknown".
pub fn socket_state(socket: Option<&SocketRecord>) -> &'static str {
    match socket {
        Some(s) if s.end.is_none() => "Open",
        Some(_) => "Closed",
        None => "Unknown",
    }
}

#[derive(Serialize)]
struct JsonRequest<'a> {
    method: &'a str,
    url: &'a str,
    status: Option<u16>,
    duration_ms: Option<u64>,
    size: Option<u64>,
    start: String,
}

/// `{"method":"GET","url":"…","status":200,"duration_ms":182,"size":1024,"start":"0:01.250"}`
pub fn request_json(r: &HttpRecord) -> String {
    serde_json::to_string(&JsonRequest {
        method: &r.method,
        url: &r.url,
        status: r.status,
        duration_ms: r.duration_ns().map(|ns| ns / 1_000_000),
        size: r.size(),
        start: format_time(r.start),
    })
    .expect("a request of plain fields serializes")
}

/// The heading and body of a tab's empty state.
pub fn empty_copy(tab: NetTab) -> (&'static str, &'static str) {
    match tab {
        NetTab::Requests => (
            "No requests yet",
            "Requests appear as your app makes them. Requests sent before fdt connected aren't captured.",
        ),
        NetTab::WebSockets => (
            "No WebSockets yet",
            "fdt shows each connection's handshake and traffic volume; message contents aren't available to debugging tools.",
        ),
        NetTab::Errors => ("No failed requests", ""),
    }
}

/// The empty state read as one sentence run, which is also its accessible label.
pub fn empty_label(tab: NetTab) -> String {
    let (heading, body) = empty_copy(tab);
    if body.is_empty() {
        format!("{heading}.")
    } else {
        format!("{heading}. {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{request, snapshot};
    use std::sync::Arc;

    const MS: u64 = 1_000_000;

    fn one(
        method: &str,
        status: Option<u16>,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> Arc<HttpRecord> {
        let snap =
            snapshot(|w, run| request(w, run, method, "http://a.com/x", status, start_ms, end_ms));
        snap.net.request(0).clone()
    }

    #[test]
    fn format_bytes_cases() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_468_006), "1.4 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(format_bytes(1_048_575), "1.0 MB");
        assert!(format_bytes(u64::MAX).ends_with("GB"));
    }

    #[test]
    fn format_duration_cases() {
        assert_eq!(format_duration_ns(0), "0 \u{b5}s");
        assert_eq!(format_duration_ns(850_000), "850 \u{b5}s");
        assert_eq!(format_duration_ns(182 * MS), "182 ms");
        assert_eq!(format_duration_ns(1_400 * MS), "1.4 s");
        assert_eq!(format_duration_ns(59_000 * MS), "59.0 s");
        assert_eq!(format_duration_ns(59_990 * MS), "1:00");
        assert_eq!(format_duration_ns(65_000 * MS), "1:05");
        assert_eq!(format_duration_ns(3_725_000 * MS), "62:05");
    }

    #[test]
    fn status_text_cases() {
        assert_eq!(status_text(&one("GET", Some(200), 0, Some(5))), "200");
        assert_eq!(status_text(&one("GET", Some(101), 0, Some(5))), "101");
        assert_eq!(status_text(&one("GET", None, 0, None)), "Pending");
        let snap = snapshot(|w, run| {
            request(w, run, "GET", "http://a.com/x", None, 0, Some(5));
        });
        // No status and no error is pending; a request with an error is failed.
        assert_eq!(status_text(snap.net.request(0)), "Pending");
        let failed = snapshot(|w, run| {
            w.upsert_request(fdt_store::net::HttpUpdate {
                run,
                isolate: "i".into(),
                vm_id: "1".into(),
                method: "GET".into(),
                url: "http://a.com/x".into(),
                start: SessionTime(0),
                start_clamped: false,
                request_sent: None,
                response_start: None,
                end: None,
                status: None,
                reason: None,
                request_headers: vec![],
                response_headers: vec![],
                request_content_length: None,
                response_content_length: None,
                events: vec![],
                error: Some("SocketException".into()),
                websocket: false,
                remote: None,
            });
        });
        let rec = failed.net.request(0);
        assert_eq!(status_text(rec), "Failed");
        assert_eq!(status_tone(rec), Tone::Danger);
        assert_eq!(
            status_tone(&one("GET", Some(204), 0, Some(5))),
            Tone::Success
        );
        assert_eq!(status_tone(&one("GET", Some(302), 0, Some(5))), Tone::Muted);
        assert_eq!(
            status_tone(&one("GET", Some(404), 0, Some(5))),
            Tone::Warning
        );
        assert_eq!(
            status_tone(&one("GET", Some(503), 0, Some(5))),
            Tone::Danger
        );
    }

    #[test]
    fn waterfall_positions_and_pending_extends_to_now() {
        let span = TimeRange::new(SessionTime(0), SessionTime(1_000 * MS));
        let done = one("GET", Some(200), 100, Some(300));
        let (x, w) = waterfall(&done, span, SessionTime(900 * MS));
        assert!((x - 0.1).abs() < 1e-6 && (w - 0.2).abs() < 1e-6, "{x} {w}");
        let pending = one("GET", None, 500, None);
        let (x, w) = waterfall(&pending, span, SessionTime(800 * MS));
        assert!((x - 0.5).abs() < 1e-6 && (w - 0.3).abs() < 1e-6, "{x} {w}");
        // Instant requests stay visible, even at the right edge.
        let instant = one("GET", Some(200), 1_000, Some(1_000));
        let (x, w) = waterfall(&instant, span, SessionTime(1_000 * MS));
        assert_eq!(w, MIN_BAR);
        assert!(x + w <= 1.0 + f32::EPSILON);
        let (_, w) = waterfall(&one("GET", Some(200), 10, Some(10)), span, SessionTime(0));
        assert_eq!(w, MIN_BAR);
        // A request before the span starts at its left edge.
        let early = TimeRange::new(SessionTime(200 * MS), SessionTime(1_200 * MS));
        let (x, _) = waterfall(&done, early, SessionTime(0));
        assert_eq!(x, 0.0);
    }

    #[test]
    fn request_json_has_the_documented_fields() {
        let rec = one("GET", Some(200), 1_250, Some(1_432));
        assert_eq!(
            request_json(&rec),
            r#"{"method":"GET","url":"http://a.com/x","status":200,"duration_ms":182,"size":null,"start":"0:01.250"}"#
        );
    }

    #[test]
    fn empty_copy_is_the_documented_text() {
        assert_eq!(
            empty_label(NetTab::Requests),
            "No requests yet. Requests appear as your app makes them. Requests sent before fdt connected aren't captured."
        );
        assert_eq!(empty_label(NetTab::Errors), "No failed requests.");
        assert!(empty_label(NetTab::WebSockets).starts_with("No WebSockets yet."));
    }
}
