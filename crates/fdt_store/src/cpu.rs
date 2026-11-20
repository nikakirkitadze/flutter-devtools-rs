//! The CPU sample store: a function table, a call-stack trie and time-ordered samples.
//!
//! Every sample points at the trie node of its leaf frame; the stack is the path from that node
//! to the root, so samples with common callers share storage.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::time::{RunId, SessionTime, TimeRange};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Stacks deeper than this are cut at the root side, keeping the leaf end.
pub const MAX_STACK_DEPTH: usize = 256;
/// The sample period assumed for a run that never set one.
pub const DEFAULT_PERIOD_US: u32 = 1000;

const SAMPLE_CHUNK: usize = 4096;
const NODE_CHUNK: usize = 4096;
const FUNCTION_CHUNK: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FunctionId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    Dart,
    Native,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    pub name: Arc<str>,
    pub library: Option<Arc<str>>,
    pub kind: FunctionKind,
}

/// Node 0 is the trie root and has no function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuNode {
    pub parent: NodeId,
    pub function: FunctionId,
    pub depth: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuSample {
    pub time: SessionTime,
    pub leaf: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpuStatus {
    Unknown,
    On,
    Off(Arc<str>),
}

static UNKNOWN: CpuStatus = CpuStatus::Unknown;

const ROOT: CpuNode = CpuNode {
    parent: NodeId(0),
    function: FunctionId(u32::MAX),
    depth: 0,
};

pub(crate) struct CpuStore {
    functions: Chunked<FunctionInfo>,
    dart_ids: HashMap<Arc<str>, HashMap<Arc<str>, FunctionId>>,
    native_ids: HashMap<Arc<str>, FunctionId>,
    nodes: Chunked<CpuNode>,
    children: HashMap<(NodeId, FunctionId), NodeId>,
    samples: Chunked<CpuSample>,
    last: Option<SessionTime>,
    coverage: HashMap<RunId, SessionTime>,
    status: HashMap<RunId, CpuStatus>,
    period: HashMap<RunId, u32>,
}

impl CpuStore {
    pub(crate) fn new() -> Self {
        let mut nodes = Chunked::with_chunk(NODE_CHUNK);
        nodes.push(ROOT);
        Self {
            functions: Chunked::with_chunk(FUNCTION_CHUNK),
            dart_ids: HashMap::new(),
            native_ids: HashMap::new(),
            nodes,
            children: HashMap::new(),
            samples: Chunked::with_chunk(SAMPLE_CHUNK),
            last: None,
            coverage: HashMap::new(),
            status: HashMap::new(),
            period: HashMap::new(),
        }
    }

    /// Returns the id and whether the function is new.
    pub(crate) fn intern_function(
        &mut self,
        name: &str,
        library: Option<&str>,
    ) -> (FunctionId, bool) {
        let existing = match library {
            Some(l) => self.dart_ids.get(l).and_then(|m| m.get(name)),
            None => self.native_ids.get(name),
        };
        if let Some(&id) = existing {
            return (id, false);
        }
        let id = FunctionId(self.functions.len() as u32);
        let name: Arc<str> = Arc::from(name);
        let library: Option<Arc<str>> = library.map(Arc::from);
        match &library {
            Some(l) => {
                self.dart_ids
                    .entry(l.clone())
                    .or_default()
                    .insert(name.clone(), id);
            }
            None => {
                self.native_ids.insert(name.clone(), id);
            }
        }
        let kind = if library.is_some() {
            FunctionKind::Dart
        } else {
            FunctionKind::Native
        };
        self.functions.push(FunctionInfo {
            name,
            library,
            kind,
        });
        (id, true)
    }

    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn intern_stack(&mut self, root_first: &[FunctionId]) -> NodeId {
        let cut = root_first.len().saturating_sub(MAX_STACK_DEPTH);
        let mut at = NodeId(0);
        for (i, &f) in root_first[cut..].iter().enumerate() {
            at = match self.children.get(&(at, f)) {
                Some(&n) => n,
                None => {
                    let n = NodeId(self.nodes.len() as u32);
                    self.nodes.push(CpuNode {
                        parent: at,
                        function: f,
                        depth: i as u16 + 1,
                    });
                    self.children.insert((at, f), n);
                    n
                }
            };
        }
        at
    }

    /// Stores the samples after the last stored time; returns the newest stored time.
    pub(crate) fn push_samples(&mut self, mut batch: Vec<CpuSample>) -> Option<SessionTime> {
        batch.sort_by_key(|s| s.time);
        let mut newest = None;
        for s in batch {
            if self.last.is_some_and(|l| s.time <= l) {
                continue;
            }
            self.last = Some(s.time);
            self.samples.push(s);
            newest = Some(s.time);
        }
        newest
    }

    pub(crate) fn set_coverage(&mut self, run: RunId, end: SessionTime) {
        self.coverage.insert(run, end);
    }

    pub(crate) fn set_status(&mut self, run: RunId, status: CpuStatus) {
        self.status.insert(run, status);
    }

    pub(crate) fn set_period(&mut self, run: RunId, us: u32) {
        self.period.insert(run, us);
    }

    pub(crate) fn snapshot(&mut self) -> CpuSnapshot {
        CpuSnapshot {
            functions: self.functions.snapshot(),
            nodes: self.nodes.snapshot(),
            samples: self.samples.snapshot(),
            coverage: Arc::new(self.coverage.clone()),
            status: Arc::new(self.status.clone()),
            period: Arc::new(self.period.clone()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CpuSnapshot {
    functions: ChunkedSnapshot<FunctionInfo>,
    nodes: ChunkedSnapshot<CpuNode>,
    samples: ChunkedSnapshot<CpuSample>,
    coverage: Arc<HashMap<RunId, SessionTime>>,
    status: Arc<HashMap<RunId, CpuStatus>>,
    period: Arc<HashMap<RunId, u32>>,
}

impl Default for CpuSnapshot {
    fn default() -> Self {
        CpuStore::new().snapshot()
    }
}

impl CpuSnapshot {
    pub fn function(&self, id: FunctionId) -> &FunctionInfo {
        self.functions
            .get(id.0 as usize)
            .expect("function id from this session")
    }

    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    pub fn node(&self, id: NodeId) -> &CpuNode {
        self.nodes
            .get(id.0 as usize)
            .expect("node id from this session")
    }

    /// Trie nodes, the root included.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn samples(&self) -> &ChunkedSnapshot<CpuSample> {
        &self.samples
    }

    /// The samples whose time lies in `r` (inclusive at both ends).
    pub fn range(&self, r: TimeRange) -> Range<usize> {
        let part = |pred: &dyn Fn(&CpuSample) -> bool| {
            let (mut lo, mut hi) = (0, self.samples.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if pred(self.samples.get(mid).expect("within samples")) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        let lo = part(&|s| s.time < r.start);
        let hi = part(&|s| s.time <= r.end).max(lo);
        lo..hi
    }

    /// The functions of the stack ending at `leaf`, leaf first.
    pub fn stack(&self, leaf: NodeId) -> impl Iterator<Item = FunctionId> + '_ {
        let mut at = leaf;
        std::iter::from_fn(move || {
            if at.0 == 0 {
                return None;
            }
            let n = self.node(at);
            at = n.parent;
            Some(n.function)
        })
    }

    pub fn coverage_end(&self, run: RunId) -> Option<SessionTime> {
        self.coverage.get(&run).copied()
    }

    pub fn status(&self, run: RunId) -> &CpuStatus {
        self.status.get(&run).unwrap_or(&UNKNOWN)
    }

    /// The sample period of a run in microseconds; [`DEFAULT_PERIOD_US`] when unset.
    pub fn period_us(&self, run: RunId) -> u32 {
        self.period.get(&run).copied().unwrap_or(DEFAULT_PERIOD_US)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Calibration, RunClock};
    use crate::run::{AppInfo, Mode};
    use crate::session::SessionWriter;
    use crate::synth;

    fn begin(w: &mut SessionWriter) -> RunId {
        let app = AppInfo {
            name: "t".into(),
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
        w.start_run(app, clock, SessionTime(0))
    }

    fn smp(t: u64, leaf: NodeId) -> CpuSample {
        CpuSample {
            time: SessionTime(t),
            leaf,
        }
    }

    fn fns(w: &mut SessionWriter, n: usize) -> Vec<FunctionId> {
        (0..n)
            .map(|i| w.intern_function(&format!("f{i}"), Some("lib")))
            .collect()
    }

    #[test]
    fn functions_are_interned_by_name_and_library() {
        let mut w = SessionWriter::new(0);
        let a = w.intern_function("main", Some("file:///a.dart"));
        assert_eq!(a, w.intern_function("main", Some("file:///a.dart")));
        let b = w.intern_function("main", Some("file:///b.dart"));
        let n = w.intern_function("main", None);
        assert!(a != b && b != n && a != n);
        let s = w.publish();
        assert_eq!(s.cpu.function(a).kind, FunctionKind::Dart);
        assert_eq!(s.cpu.function(n).kind, FunctionKind::Native);
        assert_eq!(&*s.cpu.function(a).name, "main");
        assert_eq!(s.cpu.function(n).library, None);
        assert_eq!(s.cpu.function_count(), 3);
    }

    #[test]
    fn stacks_share_prefixes() {
        let mut w = SessionWriter::new(0);
        let f = fns(&mut w, 4);
        let a = w.intern_stack(&[f[0], f[1], f[2]]);
        let b = w.intern_stack(&[f[0], f[1], f[3]]);
        let a2 = w.intern_stack(&[f[0], f[1], f[2]]);
        assert_eq!(a, a2);
        assert_ne!(a, b);
        let s = w.publish();
        // Root, f0, f1, and the two distinct leaves.
        assert_eq!(s.cpu.node_count(), 5);
        assert_eq!(s.cpu.node(a).parent, s.cpu.node(b).parent);
        assert_eq!(w.intern_stack(&[]), NodeId(0));
    }

    #[test]
    fn stack_walks_leaf_to_root() {
        let mut w = SessionWriter::new(0);
        let f = fns(&mut w, 3);
        let leaf = w.intern_stack(&[f[0], f[1], f[2]]);
        let s = w.publish();
        assert_eq!(
            s.cpu.stack(leaf).collect::<Vec<_>>(),
            vec![f[2], f[1], f[0]]
        );
        assert_eq!(s.cpu.node(leaf).depth, 3);
        assert_eq!(s.cpu.stack(NodeId(0)).count(), 0);
    }

    #[test]
    fn batches_are_sorted_and_overlaps_dropped() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let f = fns(&mut w, 1);
        let n = w.intern_stack(&f);
        w.push_cpu_samples(run, vec![smp(300, n), smp(100, n), smp(200, n)]);
        // At or before the last stored time (300) is dropped; duplicates within a batch too.
        w.push_cpu_samples(
            run,
            vec![smp(250, n), smp(300, n), smp(400, n), smp(400, n)],
        );
        let s = w.publish();
        let times: Vec<u64> = (0..s.cpu.samples().len())
            .map(|i| s.cpu.samples().get(i).unwrap().time.0)
            .collect();
        assert_eq!(times, vec![100, 200, 300, 400]);
        assert_eq!(s.latest_time(), SessionTime(400));
    }

    #[test]
    fn deep_stacks_keep_the_leaf() {
        let mut w = SessionWriter::new(0);
        let f = fns(&mut w, 300);
        let leaf = w.intern_stack(&f);
        let s = w.publish();
        assert_eq!(s.cpu.node(leaf).depth as usize, MAX_STACK_DEPTH);
        assert_eq!(s.cpu.node(leaf).function, f[299]);
        let st: Vec<_> = s.cpu.stack(leaf).collect();
        assert_eq!(st.len(), MAX_STACK_DEPTH);
        assert_eq!(*st.last().unwrap(), f[44]);
    }

    #[test]
    fn range_by_time() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let f = fns(&mut w, 1);
        let n = w.intern_stack(&f);
        w.push_cpu_samples(run, (0..10).map(|i| smp(i * 100, n)).collect());
        let s = w.publish();
        let r = |a, b| TimeRange::new(SessionTime(a), SessionTime(b));
        assert_eq!(s.cpu.range(r(200, 500)), 2..6);
        assert_eq!(s.cpu.range(r(201, 499)), 3..5);
        assert_eq!(s.cpu.range(r(0, 10_000)), 0..10);
        assert_eq!(s.cpu.range(r(5_000, 6_000)), 10..10);
        assert_eq!(s.cpu.range(r(101, 199)), 2..2);
    }

    #[test]
    fn coverage_and_status_per_run() {
        let mut w = SessionWriter::new(0);
        let r1 = begin(&mut w);
        let r2 = begin(&mut w);
        let s = w.publish();
        assert_eq!(s.cpu.coverage_end(r1), None);
        assert_eq!(s.cpu.status(r1), &CpuStatus::Unknown);
        w.set_cpu_coverage(r1, SessionTime(5_000));
        w.set_cpu_status(r1, CpuStatus::On);
        w.set_cpu_status(r2, CpuStatus::Off("profile mode".into()));
        let s = w.publish();
        assert_eq!(s.cpu.coverage_end(r1), Some(SessionTime(5_000)));
        assert_eq!(s.cpu.coverage_end(r2), None);
        assert_eq!(s.cpu.status(r1), &CpuStatus::On);
        assert_eq!(s.cpu.status(r2), &CpuStatus::Off("profile mode".into()));
    }

    #[test]
    fn period_defaults_to_1000_and_is_per_run() {
        let mut w = SessionWriter::new(0);
        let r1 = begin(&mut w);
        let r2 = begin(&mut w);
        assert_eq!(w.publish().cpu.period_us(r1), 1000);
        w.set_cpu_period(r1, 250);
        let s = w.publish();
        assert_eq!((s.cpu.period_us(r1), s.cpu.period_us(r2)), (250, 1000));
    }

    #[test]
    fn synth_cpu_has_the_documented_shape() {
        let mut w = SessionWriter::new(0);
        let run = begin(&mut w);
        let span = TimeRange::new(SessionTime(0), SessionTime(10_000_000_000));
        synth::cpu(&mut w, run, 7_500, 5, span);
        let s = w.publish();
        assert_eq!(s.cpu.samples().len(), 7_500);
        assert!((250..=350).contains(&s.cpu.function_count()));
        assert!((2_500..4_000).contains(&s.cpu.node_count()));
        let (mut screen, mut fw, mut native) = (0, 0, 0);
        for i in 0..s.cpu.function_count() {
            let f = s.cpu.function(FunctionId(i as u32));
            match f.library.as_deref() {
                Some("file:///app/lib/screen.dart") => screen += 1,
                Some("file:///sdk/flutter/packages/flutter/lib/src/widgets/framework.dart") => {
                    fw += 1
                }
                None => native += 1,
                Some(other) => panic!("{other}"),
            }
        }
        let n = s.cpu.function_count() as f64;
        assert!((screen as f64 / n - 0.2).abs() < 0.05);
        assert!((fw as f64 / n - 0.5).abs() < 0.05);
        assert!((native as f64 / n - 0.3).abs() < 0.05);
        assert!((0..s.cpu.node_count()).all(|i| s.cpu.node(NodeId(i as u32)).depth <= 40));
    }

    #[test]
    fn stress_session_has_perf_data() {
        let s = synth::stress_session(42);
        assert_eq!(s.frames.len(), 72_000);
        assert!(s.timeline.slice_count() >= 4_500_000);
        assert_eq!(s.cpu.samples().len(), 450_000);
        assert_eq!(s.frame_budget_us(RunId(1)), 8_333);
        assert_eq!(s.logs.len(), 1_000_000);
        assert_eq!(s.net.len(), 100_000);
    }
}
