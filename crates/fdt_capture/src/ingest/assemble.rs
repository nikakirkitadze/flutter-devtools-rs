//! The timeline assembler: trace events in, nested slices per thread track, packed async rows and
//! per-frame phases out. Pure: no I/O, no clock, no store. Live polling (Task 9) feeds it.
//!
//! Events are stable-sorted by timestamp per feed (the VM does not emit them in order), then
//! matched: `B`/`E` on a stack per `(pid, tid)`, `b`/`e` on `(cat, name, id)`. Open slices and
//! frames in progress live across feeds, so a slice may begin in one poll and end in the next.
//! `M` (thread name) events carry no timestamp; they only name tracks and are never sorted into
//! the timeline.

use std::collections::HashMap;

use fdt_store::timeline::{MAX_DEPTH, TrackKind};
use fdt_vm::timeline::{Category, Phase, TraceEvent};

/// `(kind, id)`: a thread's tid, or a stable hash of `(name, row)` for an async row. Async keys
/// never equal thread keys because the kind differs, even when an async id equals a tid.
pub(crate) type TrackKeyRaw = (TrackKind, u64);

/// An `E` may close a slice at most this many levels below the top.
const MATCH_WINDOW: usize = 4;
/// Async rows per slice name.
const MAX_ASYNC_ROWS: usize = 16;
/// Unclosed async begins kept; beyond it the oldest half is dropped.
const ASYNC_OPEN_CAP: usize = 65_536;
const ORDER_UI: u32 = 0;
const ORDER_RASTER: u32 = 1;
const ORDER_THREAD: u32 = 10;
const ORDER_ASYNC: u32 = 11;

/// One frame's UI-thread phases. Times are timeline µs; durations µs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UiPhasesUs {
    pub(crate) begin_us: i64,
    pub(crate) ui_us: u32,
    pub(crate) build_us: u32,
    pub(crate) layout_us: u32,
    pub(crate) paint_us: u32,
    pub(crate) composite_us: u32,
    pub(crate) finalize_us: u32,
    pub(crate) widgets_built: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawSlice {
    pub(crate) track: TrackKeyRaw,
    pub(crate) depth: u16,
    pub(crate) start_us: i64,
    pub(crate) dur_us: i64,
    pub(crate) name: Box<str>,
    pub(crate) cat: Category,
    pub(crate) widget_build: bool,
}

/// What one `feed` produced. The caller drains it; `feed` only appends.
#[derive(Default)]
pub(crate) struct AssembledBatch {
    /// Tracks that are new or whose name or order changed: `(key, name (maybe empty), order)`.
    pub(crate) tracks: Vec<(TrackKeyRaw, Box<str>, u32)>,
    pub(crate) slices: Vec<RawSlice>,
    /// Frame number and phases, when a `BeginFrame` carrying `frame_number` closed.
    pub(crate) ui: Vec<(u64, UiPhasesUs)>,
    /// Frame number, start and duration, when a `Rasterizer::DoDraw` carrying one closed.
    pub(crate) raster: Vec<(u64, i64, i64)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AssemblerStats {
    /// `E`/`e` events with no slice to close (also: a begin dropped by `reset`).
    pub(crate) unmatched_end: u64,
    /// Slices that were open when they were lost: skipped by a mismatched end, evicted from the
    /// async table or still open at `reset`.
    pub(crate) unclosed_dropped: u64,
    /// Async slices dropped because their name already had 16 busy rows.
    pub(crate) async_overflow: u64,
    /// Slices at depth 256 or deeper.
    pub(crate) too_deep: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Plain,
    BeginFrame,
    DoDraw,
}

struct Open {
    name: Box<str>,
    cat: Category,
    start: i64,
    frame: Option<u64>,
    role: Role,
    is_build: bool,
    /// Some `BUILD` is open below this slice.
    under_build: bool,
    /// The stack's widget count when this opened (for `BeginFrame`).
    widgets_at_open: u32,
    /// build, layout, paint, composite, finalize sums (for `BeginFrame`).
    phase: [u64; 5],
}

struct Stack {
    pid: i64,
    tid: i64,
    open: Vec<Open>,
    /// Begins dropped for depth whose ends are still to come.
    over: u32,
    widget_count: u32,
    order: u32,
    has_slice: bool,
    /// The name and order last handed out in `tracks`.
    emitted: Option<(Box<str>, u32)>,
}

struct AsyncOpen {
    name: Box<str>,
    start: i64,
}

pub(crate) struct Assembler {
    stacks: Vec<Stack>,
    last_stack: usize,
    thread_names: HashMap<i64, Box<str>>,
    async_open: HashMap<(u8, u64, u64), AsyncOpen>,
    /// Per slice name, the end of the last slice in each row.
    async_rows: HashMap<Box<str>, Vec<i64>>,
    stats: AssemblerStats,
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn cat_code(c: Category) -> u8 {
    match c {
        Category::Dart => 0,
        Category::Embedder => 1,
        Category::Gc => 2,
        Category::Other => 3,
    }
}

fn row_key(name: &str, row: usize) -> u64 {
    fnv(name.as_bytes()) ^ (row as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// `Some(i)` when `name` (case-insensitive, with a trailing ` (root)` ignored) is phase `i`:
/// 0 build, 1 layout, 2 paint, 3 composite, 4 finalize.
fn phase_of(name: &str) -> Option<usize> {
    let n = name.len();
    let n =
        if n > 7 && name.is_char_boundary(n - 7) && name[n - 7..].eq_ignore_ascii_case(" (root)") {
            &name[..n - 7]
        } else {
            name
        };
    const TABLE: [(&str, usize); 8] = [
        ("BUILD", 0),
        ("LAYOUT", 1),
        ("PAINT", 2),
        ("UPDATING COMPOSITING BITS", 3),
        ("COMPOSITING", 3),
        ("FINALIZE TREE", 4),
        ("POST_FRAME", 4),
        ("SEMANTICS", 4),
    ];
    TABLE
        .iter()
        .find(|(p, _)| n.eq_ignore_ascii_case(p))
        .map(|&(_, i)| i)
}

fn clamp_us(v: i64) -> u32 {
    v.clamp(0, i64::from(u32::MAX)) as u32
}

/// `"io.flutter.raster (45315)"` -> `"io.flutter.raster"`.
fn clean_thread_name(n: &str) -> &str {
    if let Some(i) = n.rfind(" (")
        && n.ends_with(')')
        && n[i + 2..n.len() - 1].bytes().all(|b| b.is_ascii_digit())
    {
        return &n[..i];
    }
    n
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Assembler {
            stacks: Vec::new(),
            last_stack: 0,
            thread_names: HashMap::new(),
            async_open: HashMap::new(),
            async_rows: HashMap::new(),
            stats: AssemblerStats::default(),
        }
    }

    /// Drops open slices, async rows and frames in progress. Thread names are kept: they are
    /// facts about the app, not about the timeline position. Statistics keep counting.
    pub(crate) fn reset(&mut self) {
        let open: usize = self.stacks.iter().map(|s| s.open.len()).sum();
        self.stats.unclosed_dropped += (open + self.async_open.len()) as u64;
        self.stacks.clear();
        self.last_stack = 0;
        self.async_open.clear();
        self.async_rows.clear();
    }

    pub(crate) fn stats(&self) -> AssemblerStats {
        self.stats
    }

    pub(crate) fn feed(&mut self, mut events: Vec<TraceEvent>, out: &mut AssembledBatch) {
        if events.iter().any(|e| e.ph == Phase::Metadata) {
            for e in &events {
                if e.ph == Phase::Metadata
                    && let Some(n) = &e.thread_name
                {
                    let n: Box<str> = clean_thread_name(n).into();
                    if self.thread_names.get(&e.tid) != Some(&n) {
                        self.thread_names.insert(e.tid, n);
                    }
                }
            }
            events.retain(|e| e.ph != Phase::Metadata);
        }
        if !events.is_sorted_by_key(|e| e.ts_us) {
            events.sort_by_key(|e| e.ts_us);
        }
        for ev in events {
            match ev.ph {
                Phase::Begin => self.begin(ev),
                Phase::End => self.end(ev, out),
                Phase::Complete => self.complete(ev, out),
                Phase::AsyncBegin => self.async_begin(ev),
                Phase::AsyncEnd => self.async_end(ev, out),
                _ => {}
            }
        }
        self.publish_tracks(out);
    }

    fn stack_index(&mut self, pid: i64, tid: i64) -> usize {
        if let Some(s) = self.stacks.get(self.last_stack)
            && s.pid == pid
            && s.tid == tid
        {
            return self.last_stack;
        }
        let i = match self
            .stacks
            .iter()
            .position(|s| s.pid == pid && s.tid == tid)
        {
            Some(i) => i,
            None => {
                self.stacks.push(Stack {
                    pid,
                    tid,
                    open: Vec::new(),
                    over: 0,
                    widget_count: 0,
                    order: ORDER_THREAD,
                    has_slice: false,
                    emitted: None,
                });
                self.stacks.len() - 1
            }
        };
        self.last_stack = i;
        i
    }

    fn begin(&mut self, ev: TraceEvent) {
        let i = self.stack_index(ev.pid, ev.tid);
        let stack = &mut self.stacks[i];
        if stack.open.len() + stack.over as usize >= MAX_DEPTH {
            stack.over += 1;
            self.stats.too_deep += 1;
            return;
        }
        let role = match (ev.frame_number, &*ev.name) {
            (Some(_), "Animator::BeginFrame") => {
                stack.order = ORDER_UI;
                Role::BeginFrame
            }
            (Some(_), "Rasterizer::DoDraw") => {
                stack.order = ORDER_RASTER;
                Role::DoDraw
            }
            _ => Role::Plain,
        };
        let under_build = stack
            .open
            .last()
            .is_some_and(|p| p.under_build || p.is_build);
        let is_build = &*ev.name == "BUILD";
        stack.open.push(Open {
            is_build,
            under_build,
            cat: ev.cat,
            start: ev.ts_us,
            frame: ev.frame_number,
            role,
            widgets_at_open: stack.widget_count,
            phase: [0; 5],
            name: ev.name,
        });
    }

    fn end(&mut self, ev: TraceEvent, out: &mut AssembledBatch) {
        let i = self.stack_index(ev.pid, ev.tid);
        let stack = &mut self.stacks[i];
        if stack.over > 0 {
            stack.over -= 1;
            return;
        }
        let len = stack.open.len();
        if len == 0 {
            self.stats.unmatched_end += 1;
            return;
        }
        // An end with no name (not seen in practice) closes the top.
        let target = if ev.name.is_empty() || *stack.open[len - 1].name == *ev.name {
            len - 1
        } else {
            let lowest = len.saturating_sub(MATCH_WINDOW + 1);
            match (lowest..len - 1)
                .rev()
                .find(|&k| *stack.open[k].name == *ev.name)
            {
                Some(k) => k,
                None => {
                    self.stats.unmatched_end += 1;
                    return;
                }
            }
        };
        // Slices above the match never saw their end.
        let skipped = len - 1 - target;
        if skipped > 0 {
            stack.open.truncate(target + 1);
            self.stats.unclosed_dropped += skipped as u64;
        }
        let open = stack.open.pop().expect("target is in range");
        let depth = stack.open.len();
        close(stack, open, depth, ev.ts_us, ev.tid, out);
    }

    fn complete(&mut self, ev: TraceEvent, out: &mut AssembledBatch) {
        let i = self.stack_index(ev.pid, ev.tid);
        let stack = &mut self.stacks[i];
        let depth = stack.open.len() + stack.over as usize;
        if depth >= MAX_DEPTH {
            self.stats.too_deep += 1;
            return;
        }
        let dur = ev.dur_us.unwrap_or(0).max(0);
        let under_build = stack
            .open
            .last()
            .is_some_and(|p| p.under_build || p.is_build);
        let open = Open {
            is_build: false,
            under_build,
            cat: ev.cat,
            start: ev.ts_us,
            frame: None,
            role: Role::Plain,
            widgets_at_open: 0,
            phase: [0; 5],
            name: ev.name,
        };
        close(
            stack,
            open,
            depth,
            ev.ts_us.saturating_add(dur),
            ev.tid,
            out,
        );
    }

    fn async_begin(&mut self, ev: TraceEvent) {
        let id = ev.id.unwrap_or(0);
        let key = (cat_code(ev.cat), id, fnv(ev.name.as_bytes()));
        let old = self.async_open.insert(
            key,
            AsyncOpen {
                start: ev.ts_us,
                name: ev.name,
            },
        );
        if old.is_some() {
            self.stats.unclosed_dropped += 1;
        }
        if self.async_open.len() > ASYNC_OPEN_CAP {
            let mut starts: Vec<i64> = self.async_open.values().map(|o| o.start).collect();
            let mid = starts.len() / 2;
            let cut = *starts.select_nth_unstable(mid).1;
            let before = self.async_open.len();
            self.async_open.retain(|_, o| o.start >= cut);
            self.stats.unclosed_dropped += (before - self.async_open.len()) as u64;
        }
    }

    fn async_end(&mut self, ev: TraceEvent, out: &mut AssembledBatch) {
        let key = (
            cat_code(ev.cat),
            ev.id.unwrap_or(0),
            fnv(ev.name.as_bytes()),
        );
        let Some(open) = self.async_open.remove(&key) else {
            self.stats.unmatched_end += 1;
            return;
        };
        let start = open.start;
        let end = ev.ts_us.max(start);
        let rows = self.async_rows.entry(open.name.clone()).or_default();
        let mut new_row = false;
        let row = match rows.iter().position(|&last| last <= start) {
            Some(r) => r,
            None if rows.len() < MAX_ASYNC_ROWS => {
                rows.push(i64::MIN);
                new_row = true;
                rows.len() - 1
            }
            None => {
                self.stats.async_overflow += 1;
                return;
            }
        };
        rows[row] = end;
        let track = (TrackKind::Async, row_key(&open.name, row));
        if new_row {
            out.tracks.push((track, open.name.clone(), ORDER_ASYNC));
        }
        out.slices.push(RawSlice {
            track,
            depth: 0,
            start_us: start,
            dur_us: end - start,
            name: open.name,
            cat: ev.cat,
            widget_build: false,
        });
    }

    /// Tracks of threads whose name or order was not handed out yet (and have a slice).
    fn publish_tracks(&mut self, out: &mut AssembledBatch) {
        for s in &mut self.stacks {
            if !s.has_slice {
                continue;
            }
            let name = self
                .thread_names
                .get(&s.tid)
                .map(|n| &**n)
                .unwrap_or_default();
            let same = s
                .emitted
                .as_ref()
                .is_some_and(|(n, o)| &**n == name && *o == s.order);
            if !same {
                s.emitted = Some((name.into(), s.order));
                out.tracks
                    .push(((TrackKind::Thread, s.tid as u64), name.into(), s.order));
            }
        }
    }
}

/// A slice has closed at `depth` with its end at `end`: emit it, count it for widget builds and
/// add it to its parent's frame phases.
fn close(
    stack: &mut Stack,
    open: Open,
    depth: usize,
    end: i64,
    tid: i64,
    out: &mut AssembledBatch,
) {
    let dur = end.saturating_sub(open.start).max(0);
    stack.has_slice = true;
    let widget = open.under_build;
    if widget {
        stack.widget_count += 1;
    }
    if let Some(parent) = stack.open.last_mut()
        && parent.role == Role::BeginFrame
        && let Some(p) = phase_of(&open.name)
    {
        parent.phase[p] = parent.phase[p].saturating_add(dur as u64);
    }
    match (open.role, open.frame) {
        (Role::BeginFrame, Some(n)) => {
            let built = stack.widget_count - open.widgets_at_open;
            let sat = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
            out.ui.push((
                n,
                UiPhasesUs {
                    begin_us: open.start,
                    ui_us: clamp_us(dur),
                    build_us: sat(open.phase[0]),
                    layout_us: sat(open.phase[1]),
                    paint_us: sat(open.phase[2]),
                    composite_us: sat(open.phase[3]),
                    finalize_us: sat(open.phase[4]),
                    widgets_built: (built > 0).then_some(built),
                },
            ));
        }
        (Role::DoDraw, Some(n)) => out.raster.push((n, open.start, dur)),
        _ => {}
    }
    out.slices.push(RawSlice {
        track: (TrackKind::Thread, tid as u64),
        depth: depth as u16,
        start_us: open.start,
        dur_us: dur,
        name: open.name,
        cat: open.cat,
        widget_build: widget,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_vm::events::{StreamEvent, decode_notification};
    use fdt_vm::rpc::Notification;
    use fdt_vm::timeline::{Phase, decode_trace_events};
    use serde_json::Value;

    fn ev(ph: Phase, name: &str, tid: i64, ts: i64) -> TraceEvent {
        TraceEvent {
            ph,
            name: name.into(),
            cat: Category::Dart,
            pid: 1,
            tid,
            ts_us: ts,
            dur_us: None,
            id: None,
            frame_number: None,
            thread_name: None,
        }
    }
    fn b(name: &str, tid: i64, ts: i64) -> TraceEvent {
        ev(Phase::Begin, name, tid, ts)
    }
    fn e(name: &str, tid: i64, ts: i64) -> TraceEvent {
        ev(Phase::End, name, tid, ts)
    }
    fn ab(name: &str, id: u64, ts: i64) -> TraceEvent {
        TraceEvent {
            id: Some(id),
            ..ev(Phase::AsyncBegin, name, 1, ts)
        }
    }
    fn ae(name: &str, id: u64, ts: i64) -> TraceEvent {
        TraceEvent {
            id: Some(id),
            ..ev(Phase::AsyncEnd, name, 1, ts)
        }
    }
    fn run(events: Vec<TraceEvent>) -> (AssembledBatch, Assembler) {
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        a.feed(events, &mut out);
        (out, a)
    }
    fn names(out: &AssembledBatch) -> Vec<(&str, u16, i64, i64)> {
        out.slices
            .iter()
            .map(|s| (&*s.name, s.depth, s.start_us, s.dur_us))
            .collect()
    }

    #[test]
    fn sync_slices_nest_by_depth() {
        let (out, a) = run(vec![
            b("a", 5, 10),
            b("b", 5, 12),
            e("b", 5, 15),
            e("a", 5, 20),
            ev(Phase::Complete, "x", 5, 30),
        ]);
        assert_eq!(
            names(&out),
            vec![("b", 1, 12, 3), ("a", 0, 10, 10), ("x", 0, 30, 0)]
        );
        assert_eq!(out.slices[0].track, (TrackKind::Thread, 5));
        assert_eq!(a.stats(), AssemblerStats::default());
    }

    /// Hostile timestamps: an `X` whose `ts + dur` overflows, and a span from `i64::MIN` to
    /// `i64::MAX`. Debug builds panicked; durations saturate instead.
    #[test]
    fn hostile_timestamps_do_not_overflow() {
        let mut x = ev(Phase::Complete, "x", 5, i64::MAX - 10);
        x.dur_us = Some(100);
        let (out, _) = run(vec![x]);
        assert_eq!(names(&out), vec![("x", 0, i64::MAX - 10, 10)]);
        let (out, _) = run(vec![b("a", 5, i64::MIN), e("a", 5, i64::MAX)]);
        assert_eq!(names(&out), vec![("a", 0, i64::MIN, i64::MAX)]);
    }

    #[test]
    fn complete_events_use_their_duration_at_the_current_depth() {
        let mut x = ev(Phase::Complete, "x", 5, 12);
        x.dur_us = Some(4);
        let (out, _) = run(vec![b("a", 5, 10), x, e("a", 5, 20)]);
        assert_eq!(names(&out), vec![("x", 1, 12, 4), ("a", 0, 10, 10)]);
    }

    #[test]
    fn end_without_begin_is_dropped() {
        let (out, a) = run(vec![e("a", 5, 10), b("b", 5, 11), e("b", 5, 12)]);
        assert_eq!(names(&out), vec![("b", 0, 11, 1)]);
        assert_eq!(a.stats().unmatched_end, 1);
    }

    #[test]
    fn mismatched_end_pops_to_match() {
        // "inner" never ends; the end of "outer" closes through it.
        let (out, a) = run(vec![
            b("outer", 5, 10),
            b("inner", 5, 11),
            e("outer", 5, 20),
            b("next", 5, 21),
            e("next", 5, 22),
        ]);
        assert_eq!(names(&out), vec![("outer", 0, 10, 10), ("next", 0, 21, 1)]);
        assert_eq!(a.stats().unclosed_dropped, 1);
        // No match within 4 levels: the end is dropped and the stack is untouched.
        let (out, a) = run(vec![b("p", 5, 1), e("zzz", 5, 2), e("p", 5, 3)]);
        assert_eq!(names(&out), vec![("p", 0, 1, 2)]);
        assert_eq!(a.stats().unmatched_end, 1);
    }

    #[test]
    fn unsorted_events_are_sorted_first() {
        let (out, _) = run(vec![
            e("a", 5, 20),
            b("b", 5, 12),
            e("b", 5, 15),
            b("a", 5, 10),
        ]);
        assert_eq!(names(&out), vec![("b", 1, 12, 3), ("a", 0, 10, 10)]);
    }

    #[test]
    fn metadata_events_only_name_threads() {
        let mut m = ev(Phase::Metadata, "thread_name", 5, 0);
        m.thread_name = Some("io.flutter.raster (5)".into());
        // The metadata event sorts first (ts 0) and must not disturb matching either way.
        let mut out = AssembledBatch::default();
        let mut a = Assembler::new();
        a.feed(vec![b("a", 5, 10), e("a", 5, 20), m], &mut out);
        assert_eq!(out.slices.len(), 1);
        let t = out.tracks.iter().find(|t| t.0 == (TrackKind::Thread, 5));
        assert_eq!(&*t.unwrap().1, "io.flutter.raster");
    }

    #[test]
    fn slices_straddle_feeds() {
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        a.feed(vec![b("a", 5, 10), b("c", 5, 11)], &mut out);
        assert!(out.slices.is_empty());
        a.feed(vec![e("c", 5, 12), e("a", 5, 30)], &mut out);
        assert_eq!(names(&out), vec![("c", 1, 11, 1), ("a", 0, 10, 20)]);
    }

    #[test]
    fn reset_drops_open_slices() {
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        a.feed(vec![b("a", 5, 10), ab("p", 3, 10)], &mut out);
        a.reset();
        a.feed(vec![e("a", 5, 30), ae("p", 3, 31)], &mut out);
        assert!(out.slices.is_empty());
        assert_eq!(a.stats().unmatched_end, 2);
    }

    #[test]
    fn async_id_equal_to_tid_stays_separate() {
        let (out, _) = run(vec![
            b("sync", 259, 10),
            ab("asy", 259, 11),
            ae("asy", 259, 15),
            e("sync", 259, 20),
        ]);
        assert_eq!(out.slices.len(), 2);
        let asy = out.slices.iter().find(|s| &*s.name == "asy").unwrap();
        let sy = out.slices.iter().find(|s| &*s.name == "sync").unwrap();
        assert_eq!(asy.track.0, TrackKind::Async);
        assert_eq!(sy.track, (TrackKind::Thread, 259));
        assert_eq!((asy.start_us, asy.dur_us, asy.depth), (11, 4, 0));
        assert_ne!(asy.track.1, 259);
    }

    #[test]
    fn async_rows_pack_and_cap() {
        // Two overlapping slices need two rows; a later one reuses row 0.
        let (out, a) = run(vec![
            ab("n", 1, 10),
            ab("n", 2, 11),
            ae("n", 1, 20),
            ae("n", 2, 21),
            ab("n", 3, 25),
            ae("n", 3, 26),
        ]);
        let t: Vec<_> = out.slices.iter().map(|s| s.track).collect();
        assert_ne!(t[0], t[1]);
        assert_eq!(t[2], t[0]);
        assert_eq!(out.tracks.iter().filter(|t| &*t.1 == "n").count(), 2);
        assert_eq!(a.stats().async_overflow, 0);
        // 20 simultaneous slices: 16 rows, 4 dropped.
        let mut evs = vec![];
        for i in 0..20 {
            evs.push(ab("m", 100 + i, 10 + i as i64));
        }
        for i in 0..20 {
            evs.push(ae("m", 100 + i, 100 + i as i64));
        }
        let (out, a) = run(evs);
        assert_eq!(out.slices.len(), 16);
        assert_eq!(a.stats().async_overflow, 4);
    }

    #[test]
    fn async_end_without_begin_is_counted() {
        let (out, a) = run(vec![ae("n", 9, 10)]);
        assert!(out.slices.is_empty());
        assert_eq!(a.stats().unmatched_end, 1);
    }

    fn frame_events(tid: i64, t0: i64, phases: &[(&str, i64)], n: Option<u64>) -> Vec<TraceEvent> {
        let mut v = vec![];
        let mut bf = b("Animator::BeginFrame", tid, t0);
        bf.frame_number = n;
        v.push(bf);
        let mut t = t0 + 1;
        for (name, d) in phases {
            v.push(b(name, tid, t));
            t += d;
            v.push(e(name, tid, t));
        }
        v.push(e("Animator::BeginFrame", tid, t + 1));
        v
    }

    #[test]
    fn frame_phases_sum_direct_children_only() {
        let mut evs = frame_events(
            7,
            100,
            &[
                ("BUILD", 40),
                ("LAYOUT (root)", 10),
                ("UPDATING COMPOSITING BITS (root)", 2),
                ("PAINT (root)", 6),
                ("COMPOSITING", 3),
                ("FINALIZE TREE", 1),
                ("POST_FRAME", 1),
            ],
            Some(9),
        );
        // Nested LAYOUT under LAYOUT (root) must not be counted twice.
        evs.push(b("LAYOUT", 7, 141));
        evs.push(e("LAYOUT", 7, 145));
        let (out, _) = run(evs);
        assert_eq!(out.ui.len(), 1);
        let (n, p) = out.ui[0];
        assert_eq!(n, 9);
        assert_eq!(p.begin_us, 100);
        assert_eq!(p.ui_us, 65);
        assert_eq!(
            (
                p.build_us,
                p.layout_us,
                p.paint_us,
                p.composite_us,
                p.finalize_us
            ),
            (40, 10, 6, 5, 2)
        );
        assert_eq!(p.widgets_built, None);
        let t = out.tracks.iter().find(|t| t.0 == (TrackKind::Thread, 7));
        assert_eq!(t.unwrap().2, 0);
    }

    #[test]
    fn lowercase_phase_names_still_count() {
        let (out, _) = run(frame_events(
            7,
            100,
            &[("build", 40), ("layout (root)", 10), ("Finalize Tree", 2)],
            Some(1),
        ));
        let p = out.ui[0].1;
        assert_eq!((p.build_us, p.layout_us, p.finalize_us), (40, 10, 2));
    }

    #[test]
    fn nested_phase_slices_are_not_phases_and_widgets_count() {
        let mut evs = vec![];
        let mut bf = b("Animator::BeginFrame", 7, 0);
        bf.frame_number = Some(4);
        evs.push(bf);
        evs.push(b("BUILD", 7, 1));
        for i in 0..3 {
            evs.push(b("W", 7, 2 + i * 4));
            evs.push(b("Inner", 7, 3 + i * 4));
            evs.push(e("Inner", 7, 4 + i * 4));
            evs.push(e("W", 7, 5 + i * 4));
        }
        evs.push(e("BUILD", 7, 20));
        evs.push(e("Animator::BeginFrame", 7, 21));
        let (out, _) = run(evs);
        assert_eq!(out.ui[0].1.widgets_built, Some(6));
        assert_eq!(out.slices.iter().filter(|s| s.widget_build).count(), 6);
        assert!(
            out.slices
                .iter()
                .all(|s| s.widget_build
                    == (&*s.name != "BUILD" && &*s.name != "Animator::BeginFrame"))
        );
    }

    #[test]
    fn raster_frames_come_from_do_draw() {
        let mut d = b("Rasterizer::DoDraw", 8, 50);
        d.frame_number = Some(3);
        let (out, _) = run(vec![d, e("Rasterizer::DoDraw", 8, 80)]);
        assert_eq!(out.raster, vec![(3, 50, 30)]);
        let t = out.tracks.iter().find(|t| t.0 == (TrackKind::Thread, 8));
        assert_eq!(t.unwrap().2, 1);
    }

    #[test]
    fn frame_in_progress_survives_feeds() {
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        let all = frame_events(7, 100, &[("BUILD", 40), ("PAINT (root)", 6)], Some(2));
        a.feed(all[..3].to_vec(), &mut out);
        a.feed(all[3..].to_vec(), &mut out);
        assert_eq!(out.ui.len(), 1);
        assert_eq!(out.ui[0].1.build_us, 40);
        assert_eq!(out.ui[0].1.paint_us, 6);
    }

    #[test]
    fn too_deep_is_dropped() {
        let mut evs = vec![];
        for i in 0..300 {
            evs.push(b("d", 5, i));
        }
        for i in 0..300 {
            evs.push(e("d", 5, 1000 + i));
        }
        let (out, a) = run(evs);
        assert_eq!(out.slices.len(), 256);
        assert_eq!(a.stats().too_deep, 44);
        assert_eq!(out.slices.iter().map(|s| s.depth).max(), Some(255));
        // The stack is usable afterwards.
        let mut out2 = AssembledBatch::default();
        let mut a = a;
        a.feed(vec![b("z", 5, 5000), e("z", 5, 5001)], &mut out2);
        assert_eq!(names(&out2), vec![("z", 0, 5000, 1)]);
    }

    // ---- the real recording ----

    struct Recorded {
        timelines: Vec<String>,
        pushed: Vec<Vec<TraceEvent>>,
        frames: Vec<(u64, i64, i64)>, // number, build, raster
    }

    fn recorded() -> Recorded {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/recordings/jank_lab-perf.jsonl");
        let text = std::fs::read_to_string(path).unwrap();
        let mut sent: std::collections::HashMap<String, Value> = Default::default();
        let mut r = Recorded {
            timelines: vec![],
            pushed: vec![],
            frames: vec![],
        };
        for line in text.lines() {
            let rec: Value = serde_json::from_str(line).unwrap();
            let msg = &rec["msg"];
            if rec["dir"] == "send" {
                sent.insert(msg["id"].as_str().unwrap().into(), msg.clone());
            } else if msg["method"] == "streamNotify" {
                let n = Notification {
                    method: "streamNotify".into(),
                    params: msg["params"].clone(),
                };
                match decode_notification(&n) {
                    Some(StreamEvent::TimelineEvents { events }) => r.pushed.push(events),
                    Some(StreamEvent::Extension { kind, data, .. }) if kind == "Flutter.Frame" => {
                        r.frames.push((
                            data["number"].as_u64().unwrap(),
                            data["build"].as_i64().unwrap(),
                            data["raster"].as_i64().unwrap(),
                        ));
                    }
                    _ => {}
                }
            } else if let (Some(id), Some(res)) = (msg["id"].as_str(), msg.get("result"))
                && sent.get(id).is_some_and(|q| q["method"] == "getVMTimeline")
            {
                r.timelines.push(res.to_string());
            }
        }
        r
    }

    #[test]
    fn recorded_frames_match_flutter_frame() {
        let rec = recorded();
        assert_eq!(rec.timelines.len(), 4);
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        let mut prev_end = None;
        let mut resets = 0;
        for t in &rec.timelines {
            let (events, skipped) = decode_trace_events(t);
            assert_eq!(skipped, 0);
            // The typed decoder agrees with the `Value` one on every real event.
            let v: Value = serde_json::from_str(t).unwrap();
            let slow = fdt_vm::timeline::decode_trace_values(v["traceEvents"].as_array().unwrap());
            assert_eq!(slow, (events.clone(), 0));
            let first = events
                .iter()
                .filter(|e| e.ph != Phase::Metadata)
                .map(|e| e.ts_us)
                .min();
            let last = events.iter().map(|e| e.ts_us).max();
            // The recorder cleared the timeline between some polls (a gap of about a second):
            // a slice open at the end of one window is not the one that ends in the next.
            if let (Some(p), Some(f)) = (prev_end, first)
                && f > p + 100_000
            {
                a.reset();
                resets += 1;
            }
            prev_end = last;
            a.feed(events, &mut out);
        }
        eprintln!("resets between non-contiguous windows: {resets}");
        let by_num: std::collections::HashMap<u64, (i64, i64)> =
            rec.frames.iter().map(|f| (f.0, (f.1, f.2))).collect();
        let raster: std::collections::HashMap<u64, i64> =
            out.raster.iter().map(|r| (r.0, r.2)).collect();
        let (mut matched, mut with_raster) = (0, 0);
        let (mut dmin, mut dmax, mut rmax) = (i64::MAX, i64::MIN, 0);
        for (n, p) in &out.ui {
            let Some(&(build, ras)) = by_num.get(n) else {
                continue;
            };
            matched += 1;
            let d = i64::from(p.ui_us) - build;
            dmin = dmin.min(d);
            dmax = dmax.max(d);
            assert!(
                (0..=300).contains(&d),
                "frame {n}: ui {} build {build}",
                p.ui_us
            );
            if let Some(&r) = raster.get(n) {
                with_raster += 1;
                rmax = rmax.max((r - ras).abs());
                assert!((r - ras).abs() <= 50, "frame {n}: raster {r} vs {ras}");
            }
        }
        eprintln!(
            "frames: ui={} raster={} matched={matched} with_raster={with_raster} \
             ui-build delta min={dmin} max={dmax}; max |raster delta|={rmax}; stats={:?}",
            out.ui.len(),
            out.raster.len(),
            a.stats()
        );
        assert!(matched >= 5, "matched {matched}");
        assert!(with_raster >= 5, "with_raster {with_raster}");
    }

    #[test]
    fn widget_builds_are_counted() {
        let rec = recorded();
        assert!(rec.pushed.len() > 100);
        let mut a = Assembler::new();
        let mut out = AssembledBatch::default();
        for block in rec.pushed {
            a.feed(block, &mut out);
        }
        let best = out.ui.iter().filter_map(|u| u.1.widgets_built).max();
        eprintln!(
            "pushed: ui frames={} max widgets_built={best:?} stats={:?}",
            out.ui.len(),
            a.stats()
        );
        assert!(best.is_some_and(|n| n >= 100), "{best:?}");
        assert!(out.slices.iter().any(|s| s.widget_build));
    }

    #[test]
    fn synthetic_frames_have_seventy_events_and_assemble() {
        use crate::bench_support::{EVENTS_PER_FRAME, synthetic_timeline_json};
        let (events, skipped) = decode_trace_events(&synthetic_timeline_json(3));
        assert_eq!((events.len(), skipped), (3 * EVENTS_PER_FRAME, 0));
        let (out, a) = run(events);
        assert_eq!(out.ui.len(), 3);
        assert_eq!(out.raster.len(), 3);
        assert_eq!(out.ui[0].1.widgets_built, Some(15));
        assert_eq!(a.stats(), AssemblerStats::default());
    }

    /// Release only: `cargo test -p fdt_capture --release -- budget --nocapture`.
    #[test]
    fn assemble_budget() {
        use crate::bench_support::{EVENTS_PER_FRAME, synthetic_timeline_json};
        let frames = 200_000 / EVENTS_PER_FRAME;
        let json = synthetic_timeline_json(frames);
        let total = frames * EVENTS_PER_FRAME;
        let mut runs = vec![];
        let mut dec = vec![];
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            // The decoder `get_vm_timeline` ships.
            let (events, _) = fdt_vm::timeline::decode_timeline_reply(&json).unwrap();
            let t1 = std::time::Instant::now();
            let mut a = Assembler::new();
            let mut out = AssembledBatch::default();
            a.feed(events, &mut out);
            runs.push(t0.elapsed());
            dec.push(t1 - t0);
            assert_eq!(out.ui.len(), frames);
        }
        runs.sort();
        dec.sort();
        let p50 = runs[2];
        eprintln!(
            "assemble_budget: {total} events, decode+feed p50 {p50:?} (decode alone p50 {:?}), runs {runs:?}, {:.2} M events/s",
            dec[2],
            total as f64 / p50.as_secs_f64() / 1e6
        );
        if !cfg!(debug_assertions) {
            assert!(p50 <= std::time::Duration::from_millis(200), "{p50:?}");
        }
    }
}
