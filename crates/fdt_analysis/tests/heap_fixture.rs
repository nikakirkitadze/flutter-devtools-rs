//! The real, redacted heap snapshot of `fixtures/jank_lab` (profile mode, after leaving `/leaks`).
//! The repository is public, so these tests also guard the fixture's privacy.

use std::sync::atomic::AtomicBool;

use fdt_analysis::heap::format::{FullSnapshot, decode_full, parse};
use fdt_analysis::heap::{Graph, encode_full};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/heap/jank_lab-profile-after-leaks.dartheap"
);

fn bytes() -> Vec<u8> {
    std::fs::read(FIXTURE).expect("read the fixture")
}

fn graph(b: &[u8]) -> Graph {
    let cancel = AtomicBool::new(false);
    parse(b, b.len() as u64, &cancel, &mut |_| {}).expect("parse")
}

/// Every string a snapshot holds, as text.
fn all_strings(s: &FullSnapshot) -> Vec<String> {
    let mut out = vec![s.header.name.clone()];
    for c in &s.classes {
        out.extend([
            c.name.clone(),
            c.library_name.clone(),
            c.library_uri.clone(),
            c.reserved.clone(),
        ]);
        for f in &c.fields {
            out.extend([f.name.clone(), f.reserved.clone()]);
        }
    }
    out.extend(s.objects.iter().filter_map(|o| o.value.text()));
    out.extend(s.externals.iter().map(|e| e.name.clone()));
    out
}

/// A `/Users/<name>/` or `/home/<name>/` whose name is not all `x`.
fn unmasked_user(text: &str) -> Option<String> {
    for prefix in ["/Users/", "/home/"] {
        for (at, _) in text.match_indices(prefix) {
            let name: String = text[at + prefix.len()..]
                .chars()
                .take_while(|c| !matches!(c, '/' | '\\' | '"') && !c.is_control())
                .collect();
            if name.chars().any(|c| c != 'x') {
                return Some(format!("{prefix}{name}"));
            }
        }
    }
    None
}

/// `/[A-Za-z0-9_-]{8,}=`: the shape of a VM-service auth token in a URI.
fn token_shape(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    for (i, _) in text.match_indices('/') {
        let run = b[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            .count();
        if run >= 8 && b.get(i + 1 + run) == Some(&b'=') {
            return Some(&text[i..i + run + 2]);
        }
    }
    None
}

/// Decodes `bytes` as UTF-16LE at both alignments, lossily.
fn utf16_views(bytes: &[u8]) -> [String; 2] {
    let view = |from: usize| {
        let units: Vec<u16> = bytes[from..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    };
    [view(0), view(1)]
}

#[test]
fn fixture_is_small() {
    let len = std::fs::metadata(FIXTURE).expect("fixture").len();
    assert!(len <= 12 * 1024 * 1024, "{len} bytes");
}

#[test]
fn fixture_has_no_secrets() {
    let raw = bytes();
    let snap = decode_full(&raw).expect("decode");
    for s in all_strings(&snap) {
        assert_eq!(unmasked_user(&s), None, "a user name in {s:?}");
        assert_eq!(token_shape(&s), None, "a token shape in {s:?}");
    }
    let text = String::from_utf8_lossy(&raw);
    assert_eq!(unmasked_user(&text), None);
    assert_eq!(token_shape(&text), None);
    for view in utf16_views(&raw) {
        assert_eq!(unmasked_user(&view), None);
        assert_eq!(token_shape(&view), None);
    }
}

#[test]
fn encoder_rebuilds_the_fixture_byte_for_byte() {
    let raw = bytes();
    let rebuilt = encode_full(&decode_full(&raw).expect("decode"));
    assert_eq!(rebuilt.len(), raw.len(), "length");
    let first = raw.iter().zip(&rebuilt).position(|(a, b)| a != b);
    assert_eq!(first, None, "first differing byte");
}

#[test]
fn fixture_layout_matches_h10() {
    let raw = bytes();
    let g = graph(&raw);
    let names: Vec<&str> = g.classes[1..=5].iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Root",
            "Read-Only Pages",
            "Root slice",
            "Isolate",
            "ObjectStore"
        ]
    );
    assert_eq!(g.class_of[1], 1, "object 1 is the Root");
    let root_refs = g.refs_of(1);
    assert_eq!(root_refs.len(), 15, "15 refs of Root");
    assert_eq!(root_refs[0], 2, "[0] is Read-Only Pages");
    let isolate_obj = *root_refs.last().unwrap();
    assert_eq!(
        g.class_of[isolate_obj as usize], 4,
        "the isolate object is last"
    );
    assert_eq!(g.isolate_object(), Some(isolate_obj));
    let instances = g.static_values("instance");
    assert!(
        !instances.is_empty(),
        "the Isolate class has a static `instance`"
    );
}

#[test]
fn fixture_holds_the_leaky_screen_state() {
    let raw = bytes();
    let snap = decode_full(&raw).expect("decode");
    let state = snap
        .classes
        .iter()
        .position(|c| c.name == "_LeakyScreenState")
        .expect("the leaky State class") as u32
        + 1;
    assert!(snap.objects.iter().any(|o| o.class == state));
}

#[test]
fn fixture_counts() {
    use fdt_analysis::heap::{AnalyzeInput, analyze};
    use fdt_store::heap::HeapFile;
    use fdt_store::run::Mode;
    use std::sync::Arc;

    // The analysis keeps the file until its handle drops, and dropping deletes it: use a copy.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("fixture.dartheap");
    std::fs::write(&path, bytes()).expect("copy the fixture");
    let input = AnalyzeInput {
        file: Arc::new(HeapFile::new(path)),
        flutter_version: None,
        mode: Mode::Profile,
        root_package: None,
    };
    let started = std::time::Instant::now();
    let a = analyze(&input, &AtomicBool::new(false), &mut |_| {}).expect("analyze");
    eprintln!("analyze: {:?}", started.elapsed());
    let s = &a.summary;
    assert_eq!(s.objects, 167_031);
    assert!(s.reachable < s.objects, "{} of {}", s.reachable, s.objects);
    assert_eq!(a.index.len() as u64, s.reachable);
    assert_eq!(a.index.retained(0), s.reachable_bytes);
    // The redacted fixture's header is not the sum of its objects, so compare with the objects.
    let all: u64 = decode_full(&bytes())
        .unwrap()
        .objects
        .iter()
        .map(|o| o.shallow)
        .sum();
    assert_eq!(s.reachable_bytes + s.unreachable_bytes, all);
    let counted: u64 = s.classes.iter().map(|c| c.count).sum();
    assert_eq!(counted, s.reachable);
}
