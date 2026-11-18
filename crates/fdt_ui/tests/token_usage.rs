//! Fails when view code hard-codes visual values instead of using
//! `fdt_ui::tokens`. Escape hatch: a same-line `// token-check: allow: <reason>`.

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

const PATTERNS: &[&str] = &[
    r"\brgba?\(\s*0x",
    r"\bhsla\(",
    r##""#[0-9A-Fa-f]{6}([0-9A-Fa-f]{2})?""##,
    r"\bpx\(\s*[0-9]*\.?[0-9]*[1-9]",
    r"\.(p|px|py|pt|pr|pb|pl|m|mx|my|mt|mr|mb|ml|gap|gap_x|gap_y|w|h|size|min_w|min_h|max_w|max_h|top|right|bottom|left|inset)_\d",
    r"\.text_(xs|sm|base|lg|xl|2xl|3xl)\(",
    r"\.rounded_(xs|sm|md|lg|xl|2xl|3xl)\(",
    r"Duration::from_(millis|secs|secs_f32|secs_f64|micros|nanos)\(",
];

/// 1-based line numbers and text of lines that hard-code a visual value.
fn violations(source: &str) -> Vec<(usize, String)> {
    let regexes: Vec<Regex> = PATTERNS.iter().map(|p| Regex::new(p).unwrap()).collect();
    let mut found = Vec::new();
    for (i, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            break;
        }
        if line.contains("token-check: allow:") {
            continue;
        }
        if regexes.iter().any(|r| r.is_match(line)) {
            found.push((i + 1, line.trim().to_string()));
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn detector_flags_hard_coded_values() {
    for bad in [
        "div().bg(rgb(0x123456))",
        ".p_2()",
        ".text_sm()",
        ".w(px(12.))",
        "\"#0D0E11\"",
        "Duration::from_millis(250)",
    ] {
        assert_eq!(violations(bad).len(), 1, "should flag: {bad}");
    }
    for good in [
        ".w(px(0.))",
        ".w_full()",
        ".rounded_full()",
        ".p(space::S8)",
        "Duration::from_millis(250) // token-check: allow: poll interval",
    ] {
        assert!(violations(good).is_empty(), "should not flag: {good}");
    }
}

#[test]
fn views_use_tokens_not_literals() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tokens = workspace.join("crates/fdt_ui/src/tokens.rs");
    let mut files = Vec::new();
    let crates = fs::read_dir(workspace.join("crates")).expect("crates dir");
    for entry in crates.flatten() {
        rust_files(&entry.path().join("src"), &mut files);
    }
    let mut report = Vec::new();
    for file in files {
        if file.canonicalize().ok() == tokens.canonicalize().ok() {
            continue;
        }
        let source = fs::read_to_string(&file).expect("readable source");
        for (line, text) in violations(&source) {
            report.push(format!("{}:{line}: {text}", file.display()));
        }
    }
    assert!(
        report.is_empty(),
        "hard-coded visual values:\n{}",
        report.join("\n")
    );
}
