//! How a request or response body is previewed: which kind it looks like, and the text lines
//! that show it. Pure functions; the view runs them on the background executor and caches the
//! result.

use super::model::format_bytes;
use gpui_kit::ImageFormat;

/// The most bytes of a body that are ever turned into lines. The store keeps up to 1 MiB of
/// each body, but a full reload can keep more, and the preview must not grow with it.
pub const RENDER_MAX: usize = 1 << 20;

/// The bytes a hex dump covers, and the sniff window for unlabelled bodies.
pub const HEX_BYTES: usize = 4096;

/// Preview lines are cut after this many characters, so one minified line cannot stall layout.
pub const LINE_MAX_CHARS: usize = 2_000;

const NOT_JSON: &str = "Not valid JSON. Shown as text.";
const NOT_UTF8: &str = "Contains bytes that aren't valid UTF-8.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKindGuess {
    Json,
    Form,
    Text,
    Image(ImageFormat),
    Binary,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedBody {
    pub lines: Vec<String>,
    pub note: Option<String>,
}

/// What `bytes` look like, going by the `Content-Type` header first and sniffing the first
/// 4 KiB when the header says nothing useful.
pub fn guess(content_type: Option<&str>, bytes: &[u8]) -> BodyKindGuess {
    let mime = content_type
        .map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    match mime.as_str() {
        "application/json" => return BodyKindGuess::Json,
        "application/x-www-form-urlencoded" => return BodyKindGuess::Form,
        "application/xml" | "application/javascript" => return BodyKindGuess::Text,
        "image/png" => return BodyKindGuess::Image(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => return BodyKindGuess::Image(ImageFormat::Jpeg),
        "image/gif" => return BodyKindGuess::Image(ImageFormat::Gif),
        "image/webp" => return BodyKindGuess::Image(ImageFormat::Webp),
        _ => {}
    }
    if mime.ends_with("+json") {
        return BodyKindGuess::Json;
    }
    if mime.starts_with("text/") {
        return BodyKindGuess::Text;
    }
    let head = &bytes[..bytes.len().min(HEX_BYTES)];
    let text = match std::str::from_utf8(head) {
        Ok(_) => true,
        // The window may end inside a multi-byte character.
        Err(e) => e.error_len().is_none() && bytes.len() > HEX_BYTES,
    };
    if text && !head.contains(&0) {
        BodyKindGuess::Text
    } else {
        BodyKindGuess::Binary
    }
}

/// The preview of `bytes` as `kind`. At most [`RENDER_MAX`] bytes are read.
pub fn render_lines(kind: BodyKindGuess, bytes: &[u8]) -> RenderedBody {
    // A body cut exactly at the cap is capped too: the caller may have truncated it.
    let capped = bytes.len() >= RENDER_MAX;
    let bytes = &bytes[..bytes.len().min(RENDER_MAX)];
    match kind {
        BodyKindGuess::Json => {
            // A cut-off document is never valid; its truncation notice says why.
            if capped {
                return text(bytes, None);
            }
            match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(value) => {
                    let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
                    RenderedBody {
                        lines: split_lines(&pretty),
                        note: None,
                    }
                }
                Err(_) => text(bytes, Some(NOT_JSON)),
            }
        }
        BodyKindGuess::Form => form(bytes),
        BodyKindGuess::Text => text(bytes, None),
        BodyKindGuess::Image(_) | BodyKindGuess::Binary => hex(bytes),
    }
}

fn text(bytes: &[u8], note: Option<&str>) -> RenderedBody {
    let (s, lossy) = match std::str::from_utf8(bytes) {
        Ok(s) => (std::borrow::Cow::Borrowed(s), false),
        Err(_) => (String::from_utf8_lossy(bytes), true),
    };
    let note = note
        .map(str::to_string)
        .or_else(|| lossy.then(|| NOT_UTF8.to_string()));
    RenderedBody {
        lines: split_lines(&s),
        note,
    }
}

fn split_lines(s: &str) -> Vec<String> {
    let s = s.strip_suffix('\n').unwrap_or(s);
    if s.is_empty() {
        return Vec::new();
    }
    s.split('\n')
        .map(|l| cut(l.strip_suffix('\r').unwrap_or(l)))
        .collect()
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(LINE_MAX_CHARS) {
        Some((at, _)) => format!("{}\u{2026}", &line[..at]),
        None => line.to_string(),
    }
}

fn form(bytes: &[u8]) -> RenderedBody {
    let lines = bytes
        .split(|&b| b == b'&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => (pair, &pair[pair.len()..]),
            };
            cut(&format!("{} = {}", decode(name), decode(value)))
        })
        .collect();
    RenderedBody { lines, note: None }
}

/// Percent-decodes `raw`; `+` is a space.
fn decode(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < raw.len()
                && hex_val(raw[i + 1]).is_some()
                && hex_val(raw[i + 2]).is_some() =>
            {
                out.push(hex_val(raw[i + 1]).unwrap() << 4 | hex_val(raw[i + 2]).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// `00000000  89 50 4e 47 …  .PNG…`: 16 bytes a line over the first [`HEX_BYTES`].
fn hex(bytes: &[u8]) -> RenderedBody {
    let shown = &bytes[..bytes.len().min(HEX_BYTES)];
    let lines = shown
        .chunks(16)
        .enumerate()
        .map(|(n, chunk)| {
            let cells: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {:<47}  {ascii}", n * 16, cells.join(" "))
        })
        .collect();
    let size = format_bytes(bytes.len() as u64);
    let note = if bytes.len() > HEX_BYTES {
        format!("Binary data, {size}. Showing the first 4 KB.")
    } else {
        format!("Binary data, {size}.")
    };
    RenderedBody {
        lines,
        note: Some(note),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::PNG;

    #[test]
    fn guess_cases() {
        use BodyKindGuess::*;
        let j = br#"{"a":1}"#;
        assert_eq!(guess(Some("application/json"), j), Json);
        assert_eq!(guess(Some("application/json; charset=utf-8"), j), Json);
        assert_eq!(guess(Some("Application/Problem+JSON"), j), Json);
        assert_eq!(
            guess(Some("application/x-www-form-urlencoded"), b"a=1"),
            Form
        );
        assert_eq!(guess(Some("text/html"), b"<p>"), Text);
        assert_eq!(guess(Some("application/xml"), b"<a/>"), Text);
        assert_eq!(guess(Some("application/javascript"), b"1"), Text);
        assert_eq!(guess(Some("image/png"), &PNG), Image(ImageFormat::Png));
        assert_eq!(guess(Some("image/jpeg"), &PNG), Image(ImageFormat::Jpeg));
        assert_eq!(guess(Some("image/gif"), &PNG), Image(ImageFormat::Gif));
        assert_eq!(guess(Some("image/webp"), &PNG), Image(ImageFormat::Webp));
        // JSON served as text/plain is text; the label wins over the content.
        assert_eq!(guess(Some("text/plain"), j), Text);
        // Unlabelled or unknown: sniffed.
        assert_eq!(guess(None, b"hello"), Text);
        assert_eq!(guess(Some("application/octet-stream"), b"hello"), Text);
        assert_eq!(
            guess(None, &PNG),
            Binary,
            "a PNG without a content type is binary"
        );
        assert_eq!(guess(None, b"a\0b"), Binary);
        assert_eq!(guess(None, &[0xff, 0xfe, 0x00]), Binary);
        assert_eq!(guess(None, b""), Text);
        // A multi-byte character cut by the 4 KiB sniff window is still text.
        let mut cut = vec![b'a'; HEX_BYTES - 1];
        cut.extend("é".as_bytes());
        assert_eq!(guess(None, &cut), Text);
    }

    #[test]
    fn json_is_pretty_printed() {
        let r = render_lines(BodyKindGuess::Json, br#"{"b":[1,2],"a":{"x":null}}"#);
        assert_eq!(r.note, None);
        assert_eq!(
            r.lines,
            [
                "{",
                "  \"b\": [",
                "    1,",
                "    2",
                "  ],",
                "  \"a\": {",
                "    \"x\": null",
                "  }",
                "}"
            ],
            "keys keep the order they came in"
        );
    }

    #[test]
    fn invalid_json_falls_back_with_a_note() {
        let r = render_lines(BodyKindGuess::Json, b"{\"a\": oops}\nsecond");
        assert_eq!(r.note.as_deref(), Some("Not valid JSON. Shown as text."));
        assert_eq!(r.lines, ["{\"a\": oops}", "second"]);
    }

    #[test]
    fn form_is_decoded() {
        let r = render_lines(
            BodyKindGuess::Form,
            b"name=Ada+Lovelace&q=a%26b%3Dc&empty=&flag&caf%C3%A9=%E2%9C%93",
        );
        assert_eq!(r.note, None);
        assert_eq!(
            r.lines,
            [
                "name = Ada Lovelace",
                "q = a&b=c",
                "empty = ",
                "flag = ",
                "caf\u{e9} = \u{2713}"
            ]
        );
    }

    #[test]
    fn invalid_utf8_is_lossy_with_a_note() {
        let r = render_lines(BodyKindGuess::Text, b"ok \xff\xfe end\r\nnext");
        assert_eq!(
            r.note.as_deref(),
            Some("Contains bytes that aren't valid UTF-8.")
        );
        assert_eq!(r.lines, ["ok \u{fffd}\u{fffd} end", "next"]);
        let plain = render_lines(BodyKindGuess::Text, b"a\nb\n");
        assert_eq!(plain.note, None);
        assert_eq!(plain.lines, ["a", "b"]);
    }

    #[test]
    fn binary_hex_dump_format() {
        let r = render_lines(
            BodyKindGuess::Binary,
            &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
        );
        assert_eq!(
            r.lines[0],
            "00000000  89 50 4e 47 0d 0a 1a 0a                          .PNG...."
        );
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.note.as_deref(), Some("Binary data, 8 B."));
        let exact = render_lines(BodyKindGuess::Binary, &[7u8; HEX_BYTES]);
        assert_eq!(exact.note.as_deref(), Some("Binary data, 4.0 KB."));
        assert_eq!(exact.lines.len(), HEX_BYTES / 16);
        let long: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let r = render_lines(BodyKindGuess::Binary, &long);
        assert_eq!(r.lines.len(), HEX_BYTES / 16, "only the first 4 KiB");
        assert!(r.lines[1].starts_with("00000010  10 11 12"));
        assert!(r.lines[255].starts_with("00000ff0  f0 f1"));
        assert_eq!(
            r.note.as_deref(),
            Some("Binary data, 9.8 KB. Showing the first 4 KB.")
        );
    }

    #[test]
    fn an_image_that_cannot_be_shown_falls_back_to_hex() {
        let r = render_lines(BodyKindGuess::Image(ImageFormat::Png), &PNG);
        assert_eq!(r.lines.len(), 5);
        assert!(r.lines[0].starts_with("00000000  89 50 4e 47"));
    }

    #[test]
    fn a_json_body_cut_exactly_at_the_cap_counts_as_capped() {
        let mut body = br#"{"a":""#.to_vec();
        body.resize(RENDER_MAX, b'x');
        let r = render_lines(BodyKindGuess::Json, &body);
        assert_eq!(r.note, None, "shown as text, without the invalid-JSON note");
        assert_eq!(r.lines.len(), 1);
        assert!(r.lines[0].starts_with("{\"a\":"));
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(LINE_MAX_CHARS * 3);
        let r = render_lines(BodyKindGuess::Text, long.as_bytes());
        assert_eq!(r.lines[0].chars().count(), LINE_MAX_CHARS + 1);
        assert!(r.lines[0].ends_with('\u{2026}'));
    }

    /// Release-only checks of the limits in the spec's review focus 2.
    mod budget {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        #[cfg_attr(debug_assertions, ignore)]
        fn a_50_mb_json_body_renders_in_bounded_time() {
            // What a 50 MB response looks like once stored: the first MiB, with the true length.
            let mut full = Vec::with_capacity(50 << 20);
            full.push(b'[');
            let mut i = 0u64;
            while full.len() < 50 << 20 {
                full.extend(format!(r#"{{"id":{i},"name":"item {i}","tags":["a","b"]}},"#).bytes());
                i += 1;
            }
            let stored = &full[..1 << 20];
            assert_eq!(stored.len(), RENDER_MAX);
            let start = Instant::now();
            let kind = guess(Some("application/json"), stored);
            assert_eq!(kind, BodyKindGuess::Json);
            // A truncated body is not valid JSON; the view renders it as text.
            let text = render_lines(BodyKindGuess::Text, stored);
            // A body that was stored whole and larger than the render limit is capped too.
            let capped = render_lines(BodyKindGuess::Json, &full[..4 << 20]);
            let elapsed = start.elapsed();
            let rendered: usize = text.lines.iter().map(|l| l.len()).sum();
            assert!(
                rendered <= RENDER_MAX + text.lines.len() * 4,
                "{rendered} bytes shown"
            );
            let capped_bytes: usize = capped.lines.iter().map(|l| l.len()).sum();
            assert!(capped_bytes <= RENDER_MAX + capped.lines.len() * 4);
            assert!(
                capped.note.is_none(),
                "a capped JSON body is text without the invalid note"
            );
            println!("render of the 50 MB body's stored MiB: {elapsed:?}");
            fdt_store::budget::check(elapsed < Duration::from_millis(500), || {
                format!("{elapsed:?}")
            });
            // A well-formed 1 MiB JSON document pretty-prints within the same bound.
            let mut doc = String::from("[");
            while doc.len() < (1 << 20) - 64 {
                doc.push_str(r#"{"id":1,"name":"item","tags":["a","b"]},"#);
            }
            doc.push_str("0]");
            let start = Instant::now();
            let pretty = render_lines(BodyKindGuess::Json, doc.as_bytes());
            let elapsed = start.elapsed();
            println!("pretty-print of 1 MiB JSON: {elapsed:?}");
            assert!(pretty.lines.len() > 1000 && pretty.note.is_none());
            fdt_store::budget::check(elapsed < Duration::from_millis(500), || {
                format!("{elapsed:?}")
            });
        }
    }
}
