//! Reassembling `Stdout` and `Stderr` write events into lines.

use std::time::Duration;

use tokio::time::Instant;

const FLUTTER_PREFIX: &str = "flutter: ";

/// Joins write events into complete lines. A `print` arrives as `flutter: text` then `\n`.
#[derive(Default)]
pub(crate) struct LineAssembler {
    buf: Vec<u8>,
}

fn finish(mut bytes: &[u8]) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes = &bytes[..bytes.len() - 1];
    }
    let s = String::from_utf8_lossy(bytes);
    match s.strip_prefix(FLUTTER_PREFIX) {
        Some(rest) => rest.to_string(),
        None => s.into_owned(),
    }
}

impl LineAssembler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the lines completed by `bytes`. The rest stays buffered.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            self.buf.extend_from_slice(&rest[..i]);
            out.push(finish(&self.buf));
            self.buf.clear();
            rest = &rest[i + 1..];
        }
        self.buf.extend_from_slice(rest);
        out
    }

    /// The buffered partial line, if any.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = finish(&self.buf);
        self.buf.clear();
        Some(line)
    }

    pub(crate) fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// A [`LineAssembler`] that releases a partial line after `quiet` without writes.
pub(crate) struct QuietLines {
    asm: LineAssembler,
    quiet: Duration,
    deadline: Option<Instant>,
}

impl QuietLines {
    pub(crate) fn new(quiet: Duration) -> Self {
        QuietLines {
            asm: LineAssembler::new(),
            quiet,
            deadline: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], now: Instant) -> Vec<String> {
        let lines = self.asm.push(bytes);
        self.deadline = self.asm.has_partial().then(|| now + self.quiet);
        lines
    }

    /// When the partial line is due, if there is one.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn flush_due(&mut self, now: Instant) -> Option<String> {
        match self.deadline {
            Some(d) if now >= d => {
                self.deadline = None;
                self.asm.flush()
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_assembler_joins_split_writes_and_strips_prefix() {
        let mut a = LineAssembler::new();
        assert!(a.push(b"flutter: a").is_empty());
        assert_eq!(a.push(b"\n"), vec!["a"]);
        assert_eq!(a.push(b"flutter: b\nflutter: c"), vec!["b"]);
        assert_eq!(a.push(b"\n"), vec!["c"]);
        assert_eq!(a.flush(), None);
    }

    #[test]
    fn crlf_lines() {
        let mut a = LineAssembler::new();
        assert_eq!(a.push(b"flutter: x\r\nplain\r\n"), vec!["x", "plain"]);
        assert_eq!(a.push(b"\n"), vec![""]);
    }

    #[test]
    fn multibyte_split_across_writes() {
        let mut a = LineAssembler::new();
        let s = "é\n".as_bytes();
        assert!(a.push(&s[..1]).is_empty());
        assert_eq!(a.push(&s[1..]), vec!["é"]);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_line_flushes_after_quiet() {
        let mut q = QuietLines::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(q.push(b"flutter: half", t0).is_empty());
        assert_eq!(q.deadline(), Some(t0 + Duration::from_millis(100)));
        assert_eq!(q.flush_due(t0 + Duration::from_millis(50)), None);
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(q.flush_due(Instant::now()), Some("half".to_string()));
        assert_eq!(q.deadline(), None);
    }
}
