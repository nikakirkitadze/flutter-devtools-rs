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
    RE.get_or_init(|| Regex::new(r"/[A-Za-z0-9_+\-]{7,}=+").unwrap())
}

fn unix_user_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"file:///Users/[^/\\"\s]+/"#).unwrap())
}

fn windows_user_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)([A-Z]:(?:\\\\|\\)Users(?:\\\\|\\))[^\\/"\s]+"#).unwrap())
}

/// Replaces `prefix` with `~` only where it ends a path component: at the end of the text or
/// before a separator, quote or whitespace. `/Users/jane` must not match inside `/Users/janet`.
fn replace_path_prefix(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let after = &rest[i + prefix.len()..];
        out.push_str(&rest[..i]);
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '/' | '\\' | '"' | '\'') || c.is_whitespace());
        out.push_str(if boundary { "~" } else { prefix });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn blob_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"("(?:samples|trace)"\s*:\s*")([A-Za-z0-9+/]{1024,}={0,2})(")"#).unwrap()
    })
}

/// Overwrites the user name in every `/Users/<name>/` and `/home/<name>/` of `bytes` with `x`.
/// The length does not change, so protobuf lengths in the blob stay valid.
fn mask_user_names(bytes: &mut [u8]) {
    for prefix in [&b"/Users/"[..], &b"/home/"[..]] {
        let mut i = 0;
        while let Some(at) = bytes[i..]
            .windows(prefix.len())
            .position(|w| w == prefix)
            .map(|p| p + i)
        {
            let start = at + prefix.len();
            let mut end = start;
            while end < bytes.len()
                && !matches!(bytes[end], b'/' | b'\\' | b'"')
                && bytes[end] >= 0x20
            {
                end += 1;
            }
            bytes[start..end].fill(b'x');
            i = end.max(start);
        }
    }
}

/// Redacts one base64 blob: decodes it, masks user names, re-encodes. A string that does not
/// decode is returned unchanged.
fn redact_blob(b64: &str) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    match STANDARD.decode(b64) {
        Ok(mut bytes) => {
            mask_user_names(&mut bytes);
            STANDARD.encode(bytes)
        }
        Err(_) => b64.to_string(),
    }
}

/// Removes secrets and personal paths from text before it is stored or shown:
/// auth tokens in URL paths, the home directory, and user names in `/Users/` paths. Base64 blobs
/// of 1 KB or more under `samples` and `trace` are decoded and their user names masked with `x`.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for c in blob_re().captures_iter(text) {
        let whole = c.get(0).unwrap();
        out.push_str(&redact_text(&text[last..whole.start()]));
        out.push_str(&c[1]);
        out.push_str(&redact_blob(&c[2]));
        out.push_str(&c[3]);
        last = whole.end();
    }
    out.push_str(&redact_text(&text[last..]));
    out
}

fn redact_text(text: &str) -> String {
    let mut out = text.to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().into_owned();
        if home.len() > 1 {
            out = replace_path_prefix(&out, &home);
            // JSON-escaped form of a Windows path.
            let escaped = home.replace('\\', "\\\\");
            if escaped != home {
                out = replace_path_prefix(&out, &escaped);
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
    fn redact_edge_cases() {
        assert_eq!(redact("ws://h:1/AbCdEf12345==/ws"), "ws://h:1/<token>/ws");
        assert_eq!(
            replace_path_prefix("/Users/janet/x", "/Users/jane"),
            "/Users/janet/x"
        );
        assert_eq!(
            replace_path_prefix("/Users/jane/x /Users/jane", "/Users/jane"),
            "~/x ~"
        );
        assert_eq!(
            replace_path_prefix("\"/Users/jane\"", "/Users/jane"),
            "\"~\""
        );
    }

    #[test]
    fn redacts_user_names_inside_base64_blobs() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let mut raw = vec![7u8; 600];
        raw.extend_from_slice(b"\x1a\x20file:///Users/jane.doe/dev/a.dart\x00/home/bob/x");
        raw.extend_from_slice(&[9u8; 600]);
        let b64 = STANDARD.encode(&raw);
        assert!(b64.len() >= 1024);
        let line = format!(r#"{{"samples":"{b64}","n":1}}"#);
        let out = redact(&line);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let back = STANDARD.decode(v["samples"].as_str().unwrap()).unwrap();
        assert_eq!(back.len(), raw.len());
        let text = String::from_utf8_lossy(&back);
        assert!(text.contains("/Users/xxxxxxxx/dev/a.dart"), "{text}");
        assert!(text.contains("/home/xxx/x"));
        assert!(!text.contains("jane"));
        // Other keys and short blobs are untouched.
        let other = format!(r#"{{"other":"{b64}"}}"#);
        assert_eq!(redact(&other), other);
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
