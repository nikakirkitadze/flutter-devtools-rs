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
    for want in ["jank_lab-debug.jsonl", "jank_lab-profile.jsonl"] {
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
