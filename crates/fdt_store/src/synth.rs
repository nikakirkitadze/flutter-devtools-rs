//! Deterministic synthetic datasets for tests and benches (no `rand`).

use crate::logs::{LogInput, LogLevel, LogSource};
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
