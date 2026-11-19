//! Transcript recording of a VM service session, with redaction.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use regex::Regex;

struct TapInner {
    start: Instant,
    out: Mutex<BufWriter<File>>,
}

/// Writes one JSON object per line: `{"t_us":…,"dir":"send"|"recv","msg":…}`.
/// Every frame is redacted before it is written. Cheap to clone.
#[derive(Clone)]
pub struct Tap {
    inner: Arc<TapInner>,
}

impl std::fmt::Debug for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tap")
    }
}

impl Tap {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Tap> {
        let file = File::create(path)?;
        Ok(Tap {
            inner: Arc::new(TapInner {
                start: Instant::now(),
                out: Mutex::new(BufWriter::new(file)),
            }),
        })
    }

    /// Records one frame. `dir` is "send" or "recv". Write errors are logged, never raised.
    pub fn record(&self, dir: &str, frame: &str) {
        let t_us = self.inner.start.elapsed().as_micros() as u64;
        let redacted = redact(frame);
        let msg: serde_json::Value = serde_json::from_str(&redacted)
            .unwrap_or_else(|_| serde_json::Value::String(redacted.clone()));
        let line = serde_json::json!({"t_us": t_us, "dir": dir, "msg": msg});
        let Ok(mut out) = self.inner.out.lock() else {
            return;
        };
        if let Err(e) = writeln!(out, "{line}").and_then(|_| out.flush()) {
            log::warn!("tap write failed: {e}");
        }
    }
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/[A-Za-z0-9_+\-]{7,}=").unwrap())
}

fn unix_user_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"file:///Users/[^/\\"\s]+/"#).unwrap())
}

fn windows_user_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)([A-Z]:(?:\\\\|\\)Users(?:\\\\|\\))[^\\/"\s]+"#).unwrap())
}

/// Removes secrets and personal paths from text before it is stored or shown:
/// auth tokens in URL paths, the home directory, and user names in `/Users/` paths.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().into_owned();
        if home.len() > 1 {
            out = out.replace(&home, "~");
            // JSON-escaped form of a Windows path.
            let escaped = home.replace('\\', "\\\\");
            if escaped != home {
                out = out.replace(&escaped, "~");
            }
        }
    }
    out = token_re().replace_all(&out, "/<token>").into_owned();
    out = unix_user_re()
        .replace_all(&out, "file:///Users/~/")
        .into_owned();
    out = windows_user_re().replace_all(&out, "${1}~").into_owned();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens() {
        let t = redact("ws://127.0.0.1:51570/AbCdEf12345=/ws and /abc=/x and /short=/");
        assert_eq!(
            t,
            "ws://127.0.0.1:51570/<token>/ws and /abc=/x and /short=/"
        );
    }

    #[test]
    fn redacts_home_dir_literally() {
        let Some(home) = dirs::home_dir() else { return };
        let home = home.to_string_lossy().into_owned();
        let out = redact(&format!("path {home}/proj/lib/main.dart"));
        assert_eq!(out, "path ~/proj/lib/main.dart");
        assert!(!redact(&format!("{{\"p\":\"{}\"}}", home.replace('\\', "\\\\"))).contains(&home));
    }

    #[test]
    fn redacts_user_forms() {
        assert_eq!(
            redact("file:///Users/jane/dev/a.dart"),
            "file:///Users/~/dev/a.dart"
        );
        assert_eq!(redact(r"C:\Users\jane\dev"), r"C:\Users\~\dev");
        assert_eq!(redact(r"C:\\Users\\jane\\dev"), r"C:\\Users\\~\\dev");
    }
}
