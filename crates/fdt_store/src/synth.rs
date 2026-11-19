//! Deterministic synthetic datasets for tests and benches (no `rand`).

use crate::frames::FrameInput;
use crate::logs::{LogInput, LogLevel, LogSource};
use crate::net::HttpUpdate;
use crate::session::SessionWriter;
use crate::time::{RunId, SessionTime};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.next() >> 11) % n
    }
}

const WORDS: &[&str] = &[
    "request", "widget", "rebuild", "frame", "layout", "paint", "cache", "image", "decode",
    "socket", "stream", "future", "state", "provider", "route", "scroll", "gesture", "render",
    "isolate", "message", "update", "value", "key", "list", "item", "load", "save", "sync",
];

const LOGGERS: [&str; 24] = [
    "app",
    "net.http",
    "net.ws",
    "db",
    "db.sync",
    "ui.home",
    "ui.list",
    "ui.detail",
    "ui.nav",
    "auth",
    "auth.token",
    "cache",
    "image",
    "analytics",
    "push",
    "storage",
    "payments",
    "search",
    "feed",
    "profile",
    "settings",
    "router",
    "bloc",
    "platform",
];

/// Appends a line of `len` ASCII bytes made of words and occasional markers.
fn line(r: &mut Rng, len: usize, out: &mut String) {
    let start = out.len();
    while out.len() - start < len {
        match r.below(100) {
            0 => out.push_str("timeout "),
            1 if r.below(4) == 0 => out.push_str("Timeout "),
            2..=9 => {
                out.push_str("user_");
                out.push_str(&r.below(1000).to_string());
                out.push(' ');
            }
            10..=13 => {
                out.push_str("status=");
                out.push_str(&(200 + r.below(4) * 100 + r.below(20)).to_string());
                out.push(' ');
            }
            _ => {
                out.push_str(WORDS[r.below(WORDS.len() as u64) as usize]);
                out.push(' ');
            }
        }
    }
    out.truncate(start + len);
}

/// Appends `n` log records, about 1 ms apart, averaging roughly 150 bytes.
pub fn logs(w: &mut SessionWriter, run: RunId, n: usize, seed: u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for i in 0..n {
        let level = match r.below(100) {
            0..=29 => LogLevel::Debug,
            30..=79 => LogLevel::Info,
            80..=91 => LogLevel::Warning,
            _ => LogLevel::Error,
        };
        let mut text = String::new();
        if r.below(100) < 5 {
            // Multi-line: a message followed by stack-like lines.
            let lines = 2 + r.below(29) as usize;
            for k in 0..lines {
                if k > 0 {
                    text.push('\n');
                }
                let len = 40 + r.below(41) as usize;
                line(&mut r, len, &mut text);
            }
        } else {
            let len = 40 + r.below(135) as usize;
            line(&mut r, len, &mut text);
        }
        w.push_log(LogInput {
            run,
            time: SessionTime(i as u64 * 1_000_000 + r.below(900_000)),
            clamped: false,
            level,
            source: LogSource::Log,
            logger: LOGGERS[r.below(24) as usize].to_string(),
            text,
            error: None,
        });
    }
}

const HOSTS: [&str; 12] = [
    "api.example.com",
    "cdn.example.com",
    "auth.example.com",
    "127.0.0.1:53452",
    "img.example.net",
    "search.example.com",
    "pay.example.com",
    "push.example.org",
    "feed.example.com",
    "maps.example.net",
    "chat.example.io",
    "files.example.org",
];

const PATHS: [&str; 10] = [
    "/v2/products?page=3",
    "/v2/products/42",
    "/v2/orders",
    "/v2/users/me",
    "/login",
    "/static/app.js",
    "/images/hero.png",
    "/search?q=widget",
    "/feed",
    "/health",
];

const TYPES: [&str; 4] = [
    "application/json",
    "text/html; charset=utf-8",
    "image/png",
    "text/plain",
];

/// Appends `n` HTTP requests, about 1 ms apart, without bodies.
pub fn requests(w: &mut SessionWriter, run: RunId, n: usize, seed: u64) {
    let mut r = Rng(seed.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1);
    for i in 0..n {
        let host = HOSTS[r.below(12) as usize];
        let path = PATHS[r.below(10) as usize];
        let method = match r.below(100) {
            0..=69 => "GET",
            70..=89 => "POST",
            90..=93 => "PUT",
            94..=96 => "PATCH",
            _ => "DELETE",
        };
        let start = i as u64 * 1_000_000 + r.below(900_000);
        // Skewed towards short requests: 5 ms to 3 s.
        let u = r.below(1000);
        let duration = (5 + u * u * 2995 / 1_000_000) * 1_000_000;
        let roll = r.below(100);
        let (status, error, pending) = match roll {
            0..=79 => (
                Some(200 + [0, 1, 4][r.below(3) as usize] as u16),
                None,
                false,
            ),
            80..=84 => (Some(301 + r.below(3) as u16 * 3), None, false),
            85..=92 => (
                Some([400, 401, 403, 404, 429][r.below(5) as usize]),
                None,
                false,
            ),
            93..=96 => (Some([500, 502, 503][r.below(3) as usize]), None, false),
            97..=98 => (
                None,
                Some("SocketException: Connection refused".to_string()),
                false,
            ),
            _ => (None, None, true),
        };
        let size = r.below(1000);
        let size = (size * size * 2_000_000 / 1_000_000) as i64;
        let ok = status.is_some();
        w.upsert_request(HttpUpdate {
            run,
            isolate: "isolates/1".into(),
            vm_id: i.to_string(),
            method: method.into(),
            url: format!("https://{host}{path}"),
            start: SessionTime(start),
            start_clamped: false,
            request_sent: (!pending).then_some(SessionTime(start + 100_000)),
            response_start: ok.then_some(SessionTime(start + duration / 2)),
            end: (!pending).then_some(SessionTime(start + duration)),
            status,
            reason: None,
            request_headers: Vec::new(),
            response_headers: if ok {
                vec![(
                    "content-type".into(),
                    vec![TYPES[r.below(4) as usize].into()],
                )]
            } else {
                Vec::new()
            },
            request_content_length: None,
            response_content_length: ok.then_some(size),
            events: Vec::new(),
            error,
            websocket: false,
            remote: Some(("127.0.0.1".into(), 53452)),
        });
    }
}

/// Appends `n` frames at `1/hz` spacing: 3 % slow (budget to 2x budget) and
/// 1 % janky (over 2x budget), where budget is `1/hz`.
pub fn frames(w: &mut SessionWriter, run: RunId, n: usize, seed: u64, hz: u32) {
    let mut r = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let hz = hz.max(1) as u64;
    let budget = (1_000_000 / hz).max(4);
    for i in 0..n as u64 {
        let elapsed = match r.below(100) {
            0 => 2 * budget + 1 + r.below(2 * budget),
            1..=3 => budget + r.below(budget),
            _ => budget / 4 + r.below(budget / 2),
        } as u32;
        let build = (elapsed as u64 * (30 + r.below(30)) / 100) as u32;
        w.push_frame(FrameInput {
            run,
            number: i,
            start: SessionTime(i * (1_000_000_000 / hz)),
            elapsed_us: elapsed,
            build_us: build,
            raster_us: elapsed - build,
            vsync_overhead_us: r.below(300) as u32,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::writer;
    use crate::net::StatusClass;

    #[test]
    fn synthetic_requests_follow_the_documented_mix() {
        let (mut w, run) = writer(1024);
        requests(&mut w, run, 20_000, 7);
        let s = w.publish();
        assert_eq!(s.net.len(), 20_000);
        assert_eq!(s.net.hosts().len(), 12);
        let n = |c: StatusClass| {
            (0..s.net.len())
                .filter(|&i| s.net.request(i).status_class() == c)
                .count() as f64
                / 200.0
        };
        assert!((75.0..85.0).contains(&n(StatusClass::Success)));
        assert!((5.0..11.0).contains(&n(StatusClass::ClientError)));
        assert!((0.3..2.0).contains(&n(StatusClass::Pending)));
        let posts = (0..s.net.len())
            .filter(|&i| s.net.request(i).method == "POST")
            .count();
        assert!((3_000..5_000).contains(&posts));
    }
}
