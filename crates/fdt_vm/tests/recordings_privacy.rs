//! Committed recordings must carry no secrets and no personal paths.

use std::fs;
use std::path::PathBuf;

use regex::Regex;

fn recordings() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recordings");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).expect("fixtures/recordings exists") {
        let path = entry.unwrap().path();
        if path.is_file() {
            let text = fs::read_to_string(&path).expect("recordings are UTF-8");
            out.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                text,
            ));
        }
    }
    out
}

#[test]
fn both_transcripts_are_present() {
    let names: Vec<String> = recordings().into_iter().map(|(n, _)| n).collect();
    for want in [
        "jank_lab-debug.jsonl",
        "jank_lab-profile.jsonl",
        "jank_lab-perf.jsonl",
    ] {
        assert!(names.iter().any(|n| n == want), "missing {want}");
    }
}

#[test]
fn recordings_contain_no_home_paths() {
    for (name, text) in recordings() {
        for needle in ["/Users/", "/home/", "C:\\\\Users", "C:\\Users"] {
            assert!(!text.contains(needle), "{name} contains {needle}");
        }
    }
}

#[test]
fn recordings_contain_no_auth_tokens() {
    let token = Regex::new(r"/[A-Za-z0-9_-]{8,}=").unwrap();
    for (name, text) in recordings() {
        if let Some(m) = token.find(&text) {
            panic!(
                "{name} contains a token-like path segment near byte {}",
                m.start()
            );
        }
    }
}

/// Every base64 string of 1 KB or more under `samples` or `trace`, decoded.
fn blobs(value: &serde_json::Value, out: &mut Vec<Vec<u8>>) {
    use base64::Engine;
    match value {
        serde_json::Value::Object(o) => {
            for (k, v) in o {
                if let (true, Some(s)) = (matches!(k.as_str(), "samples" | "trace"), v.as_str())
                    && s.len() >= 1024
                    && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s)
                {
                    out.push(bytes);
                }
                blobs(v, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|v| blobs(v, out)),
        _ => {}
    }
}

#[test]
fn base64_blobs_are_redacted() {
    let home = Regex::new(r"(?:/Users|/home)/([^/\\\x00-\x1f]+)/").unwrap();
    let mut checked = 0;
    for (name, text) in recordings() {
        if !name.ends_with(".jsonl") {
            continue;
        }
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let mut found = Vec::new();
            blobs(&v, &mut found);
            for bytes in found {
                checked += 1;
                let text = String::from_utf8_lossy(&bytes);
                for m in home.captures_iter(&text) {
                    assert!(
                        m[1].chars().all(|c| c == 'x'),
                        "{name} has a home directory inside a base64 blob: {}",
                        &m[0]
                    );
                }
            }
        }
    }
    assert!(checked > 0, "no base64 blobs were checked");
}
