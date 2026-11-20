//! Log records: metadata in chunked vectors, text in one arena `String` per
//! chunk. Sealed chunks are shared by `Arc` between snapshots; a snapshot
//! copies only the tail chunk.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::frames::{LodSeries, LodSnapshot};
use crate::time::{EntryId, RunId, SessionTime, TimeRange};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Navigation seqs per chunk.
const NAV_CHUNK: usize = 1024;
/// Records per chunk.
pub const LOG_CHUNK: usize = 1024;
/// The store keeps at most this many bytes of a record's text.
pub const MAX_RECORD_BYTES: usize = 1 << 20;
/// The tail chunk is sealed once its text reaches this many bytes, so a snapshot never copies
/// much more than this, however large the records are.
pub const LOG_TAIL_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Debug => "Debug",
            LogLevel::Info => "Info",
            LogLevel::Warning => "Warning",
            LogLevel::Error => "Error",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warning => "WRN",
            LogLevel::Error => "ERR",
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// Maps a Dart `logging` level number to a level.
pub fn level_from_dart(n: i64) -> LogLevel {
    match n {
        0 => LogLevel::Info,
        ..=799 => LogLevel::Debug,
        800..=899 => LogLevel::Info,
        900..=999 => LogLevel::Warning,
        _ => LogLevel::Error,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogSource {
    Log,
    Stdout,
    Stderr,
    FlutterError,
    Navigation,
}

impl LogSource {
    pub fn label(self) -> &'static str {
        match self {
            LogSource::Log => "Log",
            LogSource::Stdout => "Stdout",
            LogSource::Stderr => "Stderr",
            LogSource::FlutterError => "Flutter error",
            LogSource::Navigation => "Navigation",
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ErrorDetail {
    pub description: String,
    pub summary: String,
    pub widget: Option<String>,
    pub location: Option<String>,
    pub stack: Vec<String>,
    pub rendered: String,
}

#[derive(Clone, Debug)]
pub struct LogInput {
    pub run: RunId,
    pub time: SessionTime,
    pub clamped: bool,
    pub level: LogLevel,
    pub source: LogSource,
    /// Empty means none.
    pub logger: String,
    pub text: String,
    pub error: Option<ErrorDetail>,
}

#[derive(Clone)]
pub(crate) struct LogMeta {
    pub(crate) run: RunId,
    pub(crate) time: SessionTime,
    pub(crate) clamped: bool,
    pub(crate) level: LogLevel,
    pub(crate) source: LogSource,
    /// Index into the interned logger list; 0 is "no logger".
    pub(crate) logger: u32,
    pub(crate) start: u32,
    pub(crate) len: u32,
    pub(crate) line_count: u32,
    pub(crate) error: Option<Arc<ErrorDetail>>,
}

#[derive(Clone, Default)]
pub(crate) struct LogChunk {
    pub(crate) metas: Vec<LogMeta>,
    pub(crate) text: String,
}

impl LogChunk {
    pub(crate) fn text_of(&self, m: &LogMeta) -> &str {
        &self.text[m.start as usize..(m.start + m.len) as usize]
    }
}

fn cut_text(text: String) -> String {
    if text.len() <= MAX_RECORD_BYTES {
        return text;
    }
    let mut end = MAX_RECORD_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = String::with_capacity(end + 32);
    cut.push_str(&text[..end]);
    cut.push_str(&format!("… (cut, {} bytes)", text.len()));
    cut
}

fn count_lines(text: &str) -> u32 {
    let n = text.bytes().filter(|&b| b == b'\n').count() + 1;
    let n = if text.ends_with('\n') { n - 1 } else { n };
    n.max(1) as u32
}

pub(crate) struct LogStore {
    chunk_size: usize,
    sealed: Vec<Arc<LogChunk>>,
    /// First seq of each sealed chunk (chunks seal by count or by bytes, so sizes vary).
    bases: Vec<usize>,
    tail: LogChunk,
    cache: Option<Arc<[Arc<LogChunk>]>>,
    bases_cache: Option<Arc<[usize]>>,
    loggers: Vec<Arc<str>>,
    logger_ix: HashMap<Arc<str>, u32>,
    loggers_cache: Option<Arc<[Arc<str>]>>,
    len: usize,
    lod: LodSeries,
    navigation: Chunked<u32>,
}

impl LogStore {
    pub(crate) fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size: chunk_size.max(1),
            sealed: Vec::new(),
            bases: Vec::new(),
            tail: LogChunk::default(),
            cache: None,
            bases_cache: None,
            loggers: vec![Arc::from("")],
            logger_ix: HashMap::new(),
            loggers_cache: None,
            len: 0,
            lod: LodSeries::new(),
            navigation: Chunked::with_chunk(NAV_CHUNK),
        }
    }

    fn intern(&mut self, name: &str) -> u32 {
        if name.is_empty() {
            return 0;
        }
        if let Some(&i) = self.logger_ix.get(name) {
            return i;
        }
        let i = self.loggers.len() as u32;
        let a: Arc<str> = Arc::from(name);
        self.loggers.push(a.clone());
        self.logger_ix.insert(a, i);
        self.loggers_cache = None;
        i
    }

    pub(crate) fn push(&mut self, input: LogInput) -> EntryId {
        let seq = self.len as u64;
        let text = cut_text(input.text);
        let logger = self.intern(&input.logger);
        let start = self.tail.text.len() as u32;
        self.tail.text.push_str(&text);
        self.tail.metas.push(LogMeta {
            run: input.run,
            time: input.time,
            clamped: input.clamped,
            level: input.level,
            source: input.source,
            logger,
            start,
            len: text.len() as u32,
            line_count: count_lines(&text),
            error: input.error.map(Arc::new),
        });
        self.lod.insert(input.time, input.level as u32);
        if input.source == LogSource::Navigation {
            self.navigation.push(seq as u32);
        }
        self.len += 1;
        if self.tail.metas.len() >= self.chunk_size || self.tail.text.len() >= LOG_TAIL_MAX_BYTES {
            let base = self.len - self.tail.metas.len();
            self.bases.push(base);
            self.sealed.push(Arc::new(std::mem::take(&mut self.tail)));
            self.cache = None;
            self.bases_cache = None;
        }
        EntryId {
            run: input.run,
            seq,
        }
    }

    pub(crate) fn snapshot(&mut self) -> LogsSnapshot {
        let sealed = self
            .cache
            .get_or_insert_with(|| self.sealed.clone().into())
            .clone();
        let bases = self
            .bases_cache
            .get_or_insert_with(|| self.bases.clone().into())
            .clone();
        let loggers = self
            .loggers_cache
            .get_or_insert_with(|| self.loggers.clone().into())
            .clone();
        LogsSnapshot {
            chunk_size: self.chunk_size,
            sealed,
            bases,
            tail: Arc::new(self.tail.clone()),
            loggers,
            len: self.len,
            lod: self.lod.snapshot(),
            navigation: self.navigation.snapshot(),
        }
    }
}

/// One log record as a view into the snapshot.
#[derive(Clone, Copy, Debug)]
pub struct LogRow<'a> {
    pub id: EntryId,
    pub time: SessionTime,
    pub clamped: bool,
    pub level: LogLevel,
    pub source: LogSource,
    pub logger: &'a str,
    pub text: &'a str,
    pub line_count: u32,
    pub error: Option<&'a ErrorDetail>,
}

#[derive(Clone)]
pub struct LogsSnapshot {
    chunk_size: usize,
    sealed: Arc<[Arc<LogChunk>]>,
    /// First seq of each sealed chunk.
    bases: Arc<[usize]>,
    tail: Arc<LogChunk>,
    /// Index 0 is the empty "no logger" name.
    loggers: Arc<[Arc<str>]>,
    len: usize,
    lod: LodSnapshot,
    navigation: ChunkedSnapshot<u32>,
}

impl Default for LogsSnapshot {
    fn default() -> Self {
        LogStore::new(LOG_CHUNK).snapshot()
    }
}

impl fmt::Debug for LogsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogsSnapshot")
            .field("len", &self.len)
            .field("chunk_size", &self.chunk_size)
            .finish()
    }
}

impl LogsSnapshot {
    /// Level-of-detail summary of the records by time; a bucket's `max_us` is its worst level
    /// (`Debug=0, Info=1, Warning=2, Error=3`).
    pub fn lod(&self) -> &LodSnapshot {
        &self.lod
    }

    /// The seqs of `LogSource::Navigation` records, ascending.
    pub fn navigation(&self) -> &ChunkedSnapshot<u32> {
        &self.navigation
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Interned logger names in first-seen order (no "no logger" entry).
    pub fn loggers(&self) -> &[Arc<str>] {
        &self.loggers[1..]
    }

    /// Total bytes of stored text.
    pub fn text_bytes(&self) -> usize {
        self.chunks().map(|c| c.text.len()).sum()
    }

    /// The record at `seq`, or `None` when out of range.
    pub fn get_row(&self, seq: usize) -> Option<LogRow<'_>> {
        (seq < self.len).then(|| self.row(seq))
    }

    /// The record at `seq`. Panics when out of range.
    pub fn row(&self, seq: usize) -> LogRow<'_> {
        assert!(seq < self.len, "log row {seq} out of range ({})", self.len);
        let tail_base = self.len - self.tail.metas.len();
        let (chunk, base) = if seq >= tail_base {
            (&*self.tail, tail_base)
        } else {
            let c = self.bases.partition_point(|&b| b <= seq) - 1;
            (&*self.sealed[c], self.bases[c])
        };
        let m = &chunk.metas[seq - base];
        LogRow {
            id: EntryId {
                run: m.run,
                seq: seq as u64,
            },
            time: m.time,
            clamped: m.clamped,
            level: m.level,
            source: m.source,
            logger: &self.loggers[m.logger as usize],
            text: chunk.text_of(m),
            line_count: m.line_count,
            error: m.error.as_deref(),
        }
    }

    /// O(n); call off the UI thread.
    pub fn level_counts(&self, range: Option<TimeRange>) -> [u64; 4] {
        self.level_counts_from(0, range)
    }

    /// Counts only the records at or after `from_seq`.
    pub fn level_counts_from(&self, from_seq: usize, range: Option<TimeRange>) -> [u64; 4] {
        let mut counts = [0u64; 4];
        for (base, chunk) in self.chunks_with_base() {
            if base + chunk.metas.len() <= from_seq {
                continue;
            }
            for m in &chunk.metas[from_seq.saturating_sub(base)..] {
                if range.is_none_or(|r| r.contains(m.time)) {
                    counts[m.level.index()] += 1;
                }
            }
        }
        counts
    }

    /// Text bytes in the tail chunk (the part a snapshot copies).
    pub fn tail_text_bytes(&self) -> usize {
        self.tail.text.len()
    }

    /// Non-empty chunks in order, each with the seq of its first record.
    pub(crate) fn chunks_with_base(&self) -> impl Iterator<Item = (usize, &LogChunk)> {
        let tail_base = self.len - self.tail.metas.len();
        self.bases
            .iter()
            .copied()
            .zip(self.sealed.iter().map(|c| &**c))
            .chain((!self.tail.metas.is_empty()).then_some((tail_base, &*self.tail)))
    }

    pub(crate) fn chunks(&self) -> impl Iterator<Item = &LogChunk> {
        self.chunks_with_base().map(|(_, c)| c)
    }

    pub(crate) fn loggers_all(&self) -> &[Arc<str>] {
        &self.loggers
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::clock::{Calibration, RunClock};
    use crate::run::{AppInfo, Mode};
    use crate::session::SessionWriter;
    use crate::time::{RunId, SessionTime};

    pub fn writer(chunk: usize) -> (SessionWriter, RunId) {
        let mut w = SessionWriter::new_with_log_chunk(0, chunk);
        let app = AppInfo {
            name: "demo".into(),
            device: None,
            mode: Mode::Debug,
            flutter_version: None,
            workspace: None,
            vm_uri: "ws://127.0.0.1:1/ws".into(),
            pid: None,
        };
        let clock = RunClock::new(
            0,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 0,
            },
        );
        let run = w.start_run(app, clock, SessionTime(0));
        (w, run)
    }

    /// Budget tests run one at a time so they do not steal each other's cores.
    pub fn serial() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The p90 and the worst of a set of millisecond timings.
    pub fn p90_worst(mut v: Vec<f64>) -> (f64, f64) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[(v.len() * 9).div_ceil(10) - 1], *v.last().unwrap())
    }

    pub fn input(run: RunId, ms: u64, level: LogLevel, logger: &str, text: &str) -> LogInput {
        LogInput {
            run,
            time: SessionTime(ms * 1_000_000),
            clamped: false,
            level,
            source: LogSource::Log,
            logger: logger.into(),
            text: text.into(),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn dart_levels_map_at_the_boundaries() {
        use LogLevel::*;
        for (n, want) in [
            (0, Info),
            (1, Debug),
            (799, Debug),
            (800, Info),
            (899, Info),
            (900, Warning),
            (999, Warning),
            (1000, Error),
            (1200, Error),
        ] {
            assert_eq!(level_from_dart(n), want, "{n}");
        }
        assert_eq!(Warning.short(), "WRN");
        assert_eq!(LogSource::FlutterError.label(), "Flutter error");
    }

    #[test]
    fn multi_line_text_counts_lines_and_keeps_all_text() {
        let (mut w, run) = writer(4);
        w.push_log(input(run, 1, LogLevel::Info, "", "a\nb\nc\n"));
        w.push_log(input(run, 2, LogLevel::Info, "", "single"));
        let s = w.publish();
        assert_eq!(s.logs.row(0).line_count, 3);
        assert_eq!(s.logs.row(0).text, "a\nb\nc\n");
        assert_eq!(s.logs.row(1).line_count, 1);
    }

    #[test]
    fn loggers_are_interned() {
        let (mut w, run) = writer(2);
        for l in ["net", "ui", "net", "", "ui", "net"] {
            w.push_log(input(run, 1, LogLevel::Info, l, "x"));
        }
        let s = w.publish();
        assert_eq!(s.logs.loggers().len(), 2);
        assert_eq!(s.logs.row(0).logger, "net");
        assert_eq!(s.logs.row(3).logger, "");
        assert_eq!(s.logs.len(), 6);
        assert_eq!(s.logs.row(5).id.seq, 5);
        assert_eq!(s.logs.row(5).id.run, run);
    }

    #[test]
    fn oversized_records_are_cut_with_a_note() {
        let (mut w, run) = writer(4);
        let text = "é".repeat(MAX_RECORD_BYTES); // 2 MiB, multibyte
        w.push_log(input(run, 1, LogLevel::Info, "", &text));
        let s = w.publish();
        let got = s.logs.row(0).text;
        assert!(got.len() <= MAX_RECORD_BYTES + 64);
        assert!(got.ends_with(&format!("… (cut, {} bytes)", text.len())));
    }

    #[test]
    fn latest_time_includes_log_records() {
        let (mut w, run) = writer(4);
        w.push_log(input(run, 500, LogLevel::Info, "", "x"));
        assert_eq!(w.publish().latest_time(), SessionTime(500_000_000));
    }

    #[test]
    fn level_counts_respect_the_range() {
        let (mut w, run) = writer(2);
        for (ms, l) in [
            (1, LogLevel::Debug),
            (2, LogLevel::Info),
            (3, LogLevel::Info),
            (4, LogLevel::Error),
        ] {
            w.push_log(input(run, ms, l, "", "x"));
        }
        let s = w.publish();
        assert_eq!(s.logs.level_counts(None), [1, 2, 0, 1]);
        let r = TimeRange::new(SessionTime(2_000_000), SessionTime(3_000_000));
        assert_eq!(s.logs.level_counts(Some(r)), [0, 2, 0, 0]);
    }

    #[test]
    fn a_stream_of_large_records_seals_by_bytes() {
        let (mut w, run) = writer(LOG_CHUNK);
        let text = "x".repeat(64 * 1024);
        for i in 0..100 {
            w.push_log(input(run, i, LogLevel::Info, "", &text));
            let s = w.publish();
            assert!(s.logs.tail_text_bytes() <= LOG_TAIL_MAX_BYTES + 64 * 1024);
        }
        let s = w.publish();
        assert_eq!(s.logs.len(), 100);
        for i in [0, 3, 4, 5, 50, 99] {
            assert_eq!(s.logs.row(i).id.seq, i as u64);
            assert_eq!(s.logs.row(i).time, SessionTime(i as u64 * 1_000_000));
        }
    }

    #[test]
    fn level_counts_from_matches_a_full_count() {
        let (mut w, run) = writer(LOG_CHUNK);
        crate::synth::logs(&mut w, run, 10_000, 3);
        let s = w.publish();
        let ranges = [
            None,
            Some(TimeRange::new(SessionTime(0), SessionTime(3_000_000_000))),
            Some(TimeRange::new(
                SessionTime(2_000_000_000),
                SessionTime(9_000_000_000),
            )),
        ];
        for r in ranges {
            let full = s.logs.level_counts(r);
            for k in [0usize, 1, 1023, 1024, 7_777] {
                let from = s.logs.level_counts_from(k, r);
                let mut head = [0u64; 4];
                for i in 0..k {
                    let row = s.logs.row(i);
                    if r.is_none_or(|r| r.contains(row.time)) {
                        head[row.level as usize] += 1;
                    }
                }
                let sum: Vec<u64> = (0..4).map(|i| from[i] + head[i]).collect();
                assert_eq!(sum, full.to_vec(), "k={k} r={r:?}");
            }
        }
    }
}
