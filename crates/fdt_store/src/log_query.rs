//! Filtering and searching log records, in parallel over chunks.
//! Search covers the full stored text of each record.

use crate::logs::{LogChunk, LogLevel, LogSource, LogsSnapshot};
use crate::time::TimeRange;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Phase one of a two-phase scan covers this many of the newest chunks.
pub const FIRST_PHASE_CHUNKS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelSet(u8);

impl LevelSet {
    pub const ALL: LevelSet = LevelSet(0b1111);

    pub fn contains(self, l: LogLevel) -> bool {
        self.0 & (1 << l.index()) != 0
    }

    pub fn toggle(&mut self, l: LogLevel) {
        self.0 ^= 1 << l.index();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSet(u8);

impl SourceSet {
    pub const ALL: SourceSet = SourceSet(0b11111);

    pub fn contains(self, s: LogSource) -> bool {
        self.0 & (1 << s.index()) != 0
    }

    pub fn toggle(&mut self, s: LogSource) {
        self.0 ^= 1 << s.index();
    }
}

#[derive(Clone, Debug)]
pub enum TextQuery {
    Plain {
        needle: String,
        case_sensitive: bool,
    },
    Regex(Regex),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct LogFilter {
    pub levels: LevelSet,
    pub sources: SourceSet,
    pub loggers: Option<Arc<HashSet<Arc<str>>>>,
    pub text: Option<TextQuery>,
    pub range: Option<TimeRange>,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            levels: LevelSet::ALL,
            sources: SourceSet::ALL,
            loggers: None,
            text: None,
            range: None,
        }
    }
}

/// The longest query accepted. Compilation runs on the UI thread on every keystroke.
pub const MAX_QUERY_CHARS: usize = 1_000;
/// Compiled-program and lazy-DFA size limits, so a hostile pattern fails fast.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Smart case: case-insensitive unless the input has an uppercase letter.
pub fn compile_query(input: &str, regex_mode: bool) -> Result<Option<TextQuery>, QueryError> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    if input.chars().nth(MAX_QUERY_CHARS).is_some() {
        return Err(QueryError {
            message: "Search is limited to 1,000 characters.".to_string(),
        });
    }
    let case_sensitive = if regex_mode {
        has_literal_uppercase(input)
    } else {
        input.chars().any(char::is_uppercase)
    };
    if regex_mode {
        RegexBuilder::new(input)
            .case_insensitive(!case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map(|r| Some(TextQuery::Regex(r)))
            .map_err(|e| QueryError {
                message: e.to_string(),
            })
    } else {
        Ok(Some(TextQuery::Plain {
            needle: input.to_string(),
            case_sensitive,
        }))
    }
}

/// Whether a regex pattern has an uppercase letter the user typed as a literal. Letters that are
/// part of an escape (`\S`, `\D`, `\p{Lu}`, `\x4F`), a group name or an inline flag do not count.
fn has_literal_uppercase(pattern: &str) -> bool {
    let mut it = pattern.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some('p' | 'P') => {
                    if it.peek() == Some(&'{') {
                        for c in it.by_ref() {
                            if c == '}' {
                                break;
                            }
                        }
                    } else {
                        it.next();
                    }
                }
                Some('x') => {
                    if it.peek() == Some(&'{') {
                        for c in it.by_ref() {
                            if c == '}' {
                                break;
                            }
                        }
                    } else {
                        it.next();
                        it.next();
                    }
                }
                Some('u') => {
                    for _ in 0..4 {
                        it.next();
                    }
                }
                Some('U') => {
                    for _ in 0..8 {
                        it.next();
                    }
                }
                _ => {}
            },
            '(' if it.peek() == Some(&'?') => {
                it.next();
                // `(?P<Name>`, `(?<Name>` or flags such as `(?Ui)`.
                if matches!(it.peek(), Some('P' | '<')) {
                    for c in it.by_ref() {
                        if c == '>' {
                            break;
                        }
                    }
                } else {
                    while let Some(c) = it.peek() {
                        if matches!(c, ')' | ':') {
                            break;
                        }
                        it.next();
                    }
                }
            }
            c if c.is_uppercase() => return true,
            _ => {}
        }
    }
    false
}

impl TextQuery {
    /// Builds the matcher once; use it for every row of a query.
    pub fn matcher(&self) -> Matcher {
        Matcher::new(self)
    }
}

/// A query prepared for scanning.
pub enum Matcher {
    /// Boxed because `memchr`'s finder is about 300 bytes on x86-64 (its SIMD searchers), which
    /// would make every `Matcher` that size. A matcher is built once per query, so the box is free.
    Finder(Box<memchr::memmem::Finder<'static>>),
    Re(Regex),
}

impl Matcher {
    fn new(q: &TextQuery) -> Matcher {
        match q {
            TextQuery::Plain {
                needle,
                case_sensitive: true,
            } => Matcher::Finder(Box::new(
                memchr::memmem::Finder::new(needle.as_bytes()).into_owned(),
            )),
            TextQuery::Plain {
                needle,
                case_sensitive: false,
            } => Matcher::Re(
                RegexBuilder::new(&regex::escape(needle))
                    .case_insensitive(true)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .dfa_size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .expect("an escaped literal is a valid regex"),
            ),
            TextQuery::Regex(r) => Matcher::Re(r.clone()),
        }
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Finder(f) => f.find(text.as_bytes()).is_some(),
            Matcher::Re(r) => r.is_match(text),
        }
    }

    /// Byte ranges of matches: sorted, non-overlapping, on char boundaries.
    pub fn ranges(&self, text: &str) -> Vec<Range<usize>> {
        match self {
            Matcher::Finder(f) => {
                let n = f.needle().len();
                f.find_iter(text.as_bytes()).map(|i| i..i + n).collect()
            }
            Matcher::Re(r) => r
                .find_iter(text)
                .map(|m| m.range())
                .filter(|r| !r.is_empty())
                .collect(),
        }
    }
}

/// Byte ranges of matches: sorted, non-overlapping, on char boundaries. Builds the matcher on
/// every call; to highlight many rows use [`TextQuery::matcher`].
pub fn match_ranges(text: &str, q: &TextQuery) -> Vec<Range<usize>> {
    q.matcher().ranges(text)
}

struct Prepared {
    allowed_loggers: Option<Vec<bool>>,
    matcher: Option<Matcher>,
}

impl Prepared {
    fn new(snap: &LogsSnapshot, f: &LogFilter) -> Self {
        Self {
            allowed_loggers: f.loggers.as_ref().map(|set| {
                snap.loggers_all()
                    .iter()
                    .map(|name| set.contains(name))
                    .collect()
            }),
            matcher: f.text.as_ref().map(Matcher::new),
        }
    }
}

fn scan_chunk(
    chunk: &LogChunk,
    base: usize,
    f: &LogFilter,
    p: &Prepared,
    from_seq: usize,
    cancel: Option<&AtomicBool>,
) -> Vec<u32> {
    let mut out = Vec::new();
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return out;
    }
    let skip = from_seq.saturating_sub(base);
    for (i, m) in chunk.metas.iter().enumerate().skip(skip) {
        if !f.levels.contains(m.level) || !f.sources.contains(m.source) {
            continue;
        }
        if let Some(a) = &p.allowed_loggers
            && !a[m.logger as usize]
        {
            continue;
        }
        if let Some(r) = f.range
            && !r.contains(m.time)
        {
            continue;
        }
        if let Some(mt) = &p.matcher
            && !mt.is_match(chunk.text_of(m))
        {
            continue;
        }
        out.push((base + i) as u32);
    }
    out
}

fn scan(
    chunks: &[(usize, &LogChunk)],
    f: &LogFilter,
    p: &Prepared,
    from_seq: usize,
    cancel: Option<&AtomicBool>,
) -> Vec<u32> {
    let parts: Vec<Vec<u32>> = chunks
        .par_iter()
        .map(|&(base, c)| {
            if base + c.metas.len() <= from_seq {
                return Vec::new();
            }
            scan_chunk(c, base, f, p, from_seq, cancel)
        })
        .collect();
    parts.concat()
}

/// Ascending seqs `>= from_seq` that pass the filter.
pub fn filter_logs(snap: &LogsSnapshot, f: &LogFilter, from_seq: usize) -> Vec<u32> {
    let chunks: Vec<(usize, &LogChunk)> = snap.chunks_with_base().collect();
    let p = Prepared::new(snap, f);
    scan(&chunks, f, &p, from_seq, None)
}

/// Scans the newest `FIRST_PHASE_CHUNKS` chunks and hands them to `on_first`,
/// then scans the rest. `None` when `cancel` was set (`on_first` may not run).
pub fn filter_logs_two_phase(
    snap: &LogsSnapshot,
    f: &LogFilter,
    cancel: &AtomicBool,
    on_first: impl FnOnce(Vec<u32>),
) -> Option<Vec<u32>> {
    let chunks: Vec<(usize, &LogChunk)> = snap.chunks_with_base().collect();
    let p = Prepared::new(snap, f);
    let split = chunks.len().saturating_sub(FIRST_PHASE_CHUNKS);
    let first = scan(&chunks[split..], f, &p, 0, Some(cancel));
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    on_first(first.clone());
    let mut all = scan(&chunks[..split], f, &p, 0, Some(cancel));
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    all.extend(first);
    Some(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::*;
    use crate::logs::{LogLevel, LogSource};
    use crate::time::SessionTime;
    use std::time::Instant;

    fn plain(s: &str) -> Option<TextQuery> {
        compile_query(s, false).unwrap()
    }

    fn snap_of(texts: &[&str]) -> std::sync::Arc<crate::Snapshot> {
        let (mut w, run) = writer(4);
        for (i, t) in texts.iter().enumerate() {
            w.push_log(input(run, i as u64, LogLevel::Info, "", t));
        }
        w.publish()
    }

    #[test]
    fn filter_by_level_source_logger_and_range() {
        let (mut w, run) = writer(2);
        let mut e = input(run, 3, LogLevel::Error, "db", "boom");
        e.source = LogSource::Stderr;
        w.push_log(input(run, 0, LogLevel::Debug, "net", "a"));
        w.push_log(input(run, 1, LogLevel::Info, "net", "b"));
        w.push_log(input(run, 2, LogLevel::Warning, "ui", "c"));
        w.push_log(e);
        w.push_log(input(run, 4, LogLevel::Info, "", "e"));
        let s = w.publish();
        let all = LogFilter::default();
        assert_eq!(filter_logs(&s.logs, &all, 0), vec![0, 1, 2, 3, 4]);
        assert_eq!(filter_logs(&s.logs, &all, 3), vec![3, 4]);

        let mut f = LogFilter::default();
        f.levels.toggle(LogLevel::Info);
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0, 2, 3]);

        let mut f = LogFilter::default();
        f.sources.toggle(LogSource::Log);
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![3]);

        let f = LogFilter {
            loggers: Some(std::sync::Arc::new(
                ["net".into()]
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>(),
            )),
            ..LogFilter::default()
        };
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0, 1]);

        let f = LogFilter {
            range: Some(TimeRange::new(
                SessionTime(1_000_000),
                SessionTime(3_000_000),
            )),
            ..LogFilter::default()
        };
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![1, 2, 3]);
    }

    #[test]
    fn level_and_source_sets_toggle() {
        let mut l = LevelSet::ALL;
        assert!(l.contains(LogLevel::Debug));
        l.toggle(LogLevel::Debug);
        assert!(!l.contains(LogLevel::Debug) && l.contains(LogLevel::Error));
        l.toggle(LogLevel::Debug);
        assert_eq!(l, LevelSet::ALL);
        assert!(SourceSet::ALL.contains(LogSource::Navigation));
    }

    #[test]
    fn smart_case_plain_search() {
        let s = snap_of(&["an Error here", "an error here", "fine"]);
        let mut f = LogFilter {
            text: plain("error"),
            ..LogFilter::default()
        };
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0, 1]);
        f.text = plain("Error");
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0]);
        assert!(plain("   ").is_none());
    }

    #[test]
    fn regex_search_and_invalid_regex_reports_message() {
        let s = snap_of(&["status=503 bad", "status=200 ok", "status=5xx"]);
        let f = LogFilter {
            text: compile_query(r"status=5\d\d", true).unwrap(),
            ..LogFilter::default()
        };
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0]);
        let err = compile_query("(", true).unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[test]
    fn pathological_regex_finishes_quickly() {
        let line = format!("{}b", "a".repeat(30));
        let texts: Vec<&str> = (0..1000).map(|_| line.as_str()).collect();
        let s = snap_of(&texts);
        let f = LogFilter {
            text: compile_query("(a+)+$", true).unwrap(),
            ..LogFilter::default()
        };
        let t = Instant::now();
        assert!(filter_logs(&s.logs, &f, 0).is_empty());
        assert!(t.elapsed().as_secs_f64() < 1.0);
    }

    #[test]
    fn search_finds_text_beyond_the_display_cap() {
        let text = format!("{}needle{}", "x".repeat(150_000), "y".repeat(50_000));
        let s = snap_of(&[&text, "other"]);
        let f = LogFilter {
            text: plain("needle"),
            ..LogFilter::default()
        };
        assert_eq!(filter_logs(&s.logs, &f, 0), vec![0]);
    }

    fn many(chunk: usize, n: usize) -> std::sync::Arc<crate::Snapshot> {
        let (mut w, run) = writer(chunk);
        for i in 0..n {
            w.push_log(input(run, i as u64, LogLevel::Info, "", "x match"));
        }
        w.publish()
    }

    #[test]
    fn two_phase_reports_newest_first_then_everything() {
        let s = many(4, 12_002); // 3,001 chunks, the last one partial
        let f = LogFilter {
            text: plain("match"),
            ..LogFilter::default()
        };
        let cancel = AtomicBool::new(false);
        let mut first = Vec::new();
        let all = filter_logs_two_phase(&s.logs, &f, &cancel, |v| first = v).unwrap();
        assert_eq!(first.first(), Some(&(2937 * 4)));
        assert_eq!(first.last(), Some(&12_001));
        assert!(first.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(all, (0..12_002).collect::<Vec<u32>>());
    }

    #[test]
    fn cancel_returns_none() {
        let s = many(4, 400);
        let cancel = AtomicBool::new(true);
        let mut called = false;
        let r = filter_logs_two_phase(&s.logs, &LogFilter::default(), &cancel, |_| called = true);
        assert!(r.is_none() && !called);
    }

    #[test]
    fn match_ranges_handle_case_and_multibyte() {
        let q = plain("ünï").unwrap();
        assert_eq!(match_ranges("Ünïcode", &q), vec![0..5]);
        let q = plain("aa").unwrap();
        assert_eq!(match_ranges("aaaaa", &q), vec![0..2, 2..4]);
        let q = compile_query(r"\d+", true).unwrap().unwrap();
        assert_eq!(match_ranges("a12b345", &q), vec![1..3, 4..7]);
        let q = plain("Zed").unwrap();
        assert!(match_ranges("zed", &q).is_empty());
    }

    #[test]
    fn hostile_queries_are_bounded() {
        let t = Instant::now();
        let r = compile_query(r"(\w{200}){50}", true);
        let compile = t.elapsed();
        crate::budget::check(compile < std::time::Duration::from_millis(500), || {
            format!("compiling a hostile query took {compile:?}")
        });
        assert!(r.is_err(), "the size limit rejects it");
        let long = "a".repeat(MAX_QUERY_CHARS + 1);
        for regex in [false, true] {
            let e = compile_query(&long, regex).unwrap_err();
            assert_eq!(e.message, "Search is limited to 1,000 characters.");
        }
        assert!(
            compile_query(&"a".repeat(MAX_QUERY_CHARS), false)
                .unwrap()
                .is_some()
        );
        // `(a+)+$` compiles and scans in linear time.
        let q = compile_query("(a+)+$", true).unwrap().unwrap();
        let text = format!("{}b", "a".repeat(100_000));
        let t = Instant::now();
        assert!(match_ranges(&text, &q).is_empty());
        let scan = t.elapsed();
        crate::budget::check(scan < std::time::Duration::from_secs(2), || {
            format!("scanning with a hostile query took {scan:?}")
        });
    }

    #[test]
    fn prepared_matcher_equals_match_ranges() {
        let texts = ["Ünïcode zed ZED", "aaaaa", "a12b345", "", "no hits"];
        let queries = [
            plain("ünï"),
            plain("aa"),
            plain("Zed"),
            plain("zed"),
            compile_query(r"\d+", true).unwrap(),
            compile_query("a*", true).unwrap(),
        ];
        for q in queries.iter().flatten() {
            let m = q.matcher();
            for t in texts {
                assert_eq!(m.ranges(t), match_ranges(t, q), "{q:?} on {t:?}");
            }
        }
    }

    #[test]
    fn regex_escapes_do_not_make_the_query_case_sensitive() {
        // `\S` matches "aX" as two characters whatever their case.
        let q = compile_query(r"\Sx", true).unwrap().unwrap();
        assert_eq!(match_ranges("aX", &q), vec![0..2]);
        let q = compile_query(r"\D\W?abc", true).unwrap().unwrap();
        assert_eq!(match_ranges("zABC", &q), vec![0..4]);
        let q = compile_query(r"\p{Lu}x", true).unwrap().unwrap();
        assert_eq!(match_ranges("AX", &q), vec![0..2]);
        let q = compile_query(r"(?P<Name>x)", true).unwrap().unwrap();
        assert_eq!(match_ranges("X", &q), vec![0..1]);
        // A literal uppercase letter still makes the search case-sensitive.
        let q = compile_query(r"\dFoo", true).unwrap().unwrap();
        assert!(match_ranges("1foo", &q).is_empty());
        assert_eq!(match_ranges("1Foo", &q), vec![0..4]);
    }

    // ----- budgets (release only) -----

    fn big() -> &'static std::sync::Arc<crate::Snapshot> {
        static BIG: std::sync::OnceLock<std::sync::Arc<crate::Snapshot>> =
            std::sync::OnceLock::new();
        BIG.get_or_init(|| {
            let (mut w, run) = writer(crate::logs::LOG_CHUNK);
            crate::synth::logs(&mut w, run, 1_000_000, 7);
            let s = w.publish();
            let bytes = s.logs.text_bytes();
            eprintln!("synthetic 1M logs: {} MB of text", bytes / 1_000_000);
            assert!((120_000_000..=180_000_000).contains(&bytes), "{bytes}");
            s
        })
    }

    fn budget_queries() -> Vec<(&'static str, Option<TextQuery>)> {
        vec![
            ("timeout", plain("timeout")),
            ("Timeout", plain("Timeout")),
            ("user_42", plain("user_42")),
            (
                "regex status=5\\d\\d",
                compile_query(r"status=5\d\d", true).unwrap(),
            ),
        ]
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn log_search_budget_full_scan_1m() {
        let _g = serial();
        let s = big();
        for (name, q) in budget_queries() {
            let f = LogFilter {
                text: q,
                ..LogFilter::default()
            };
            let times: Vec<f64> = (0..10)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(filter_logs(&s.logs, &f, 0));
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            let (p90, worst) = p90_worst(times);
            eprintln!("full scan {name}: p90 {p90:.1} ms, worst {worst:.1} ms");
            crate::budget::check(p90 <= 100.0, || format!("{name}: p90 {p90:.1} ms"));
        }
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn log_search_budget_first_results_1m() {
        let _g = serial();
        let s = big();
        for (name, q) in budget_queries() {
            let f = LogFilter {
                text: q,
                ..LogFilter::default()
            };
            let times: Vec<f64> = (0..10)
                .map(|_| {
                    let cancel = AtomicBool::new(false);
                    let t = Instant::now();
                    let mut ms = 0.0;
                    filter_logs_two_phase(&s.logs, &f, &cancel, |v| {
                        std::hint::black_box(&v);
                        ms = t.elapsed().as_secs_f64() * 1e3;
                        cancel.store(true, Ordering::Relaxed);
                    });
                    ms
                })
                .collect();
            let (p90, worst) = p90_worst(times);
            eprintln!("first results {name}: p90 {p90:.1} ms, worst {worst:.1} ms");
            crate::budget::check(p90 <= 16.0, || format!("{name}: p90 {p90:.1} ms"));
        }
    }
}
