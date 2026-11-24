//! Heap-snapshot records: one per snapshot taken, with its file, summary, diff and (for the one
//! held snapshot) its index. Types only; the analysis in `fdt_analysis` fills them in.

use crate::run::Mode;
use crate::time::{RunId, SessionTime};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Session-unique, never reused. Stored records of different sessions share ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HeapId(pub u32);

#[derive(Clone, Debug, PartialEq)]
pub enum HeapStage {
    Requested,
    Receiving {
        bytes: u64,
        chunks: u32,
    },
    Parsing {
        fraction: f32,
    },
    Reachability,
    Leaks,
    Dominators,
    Ready,
    Cancelled,
    Failed {
        message: Arc<str>,
        details: Arc<str>,
    },
}

impl HeapStage {
    /// Whether work on the snapshot is still going on.
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Ready | Self::Cancelled | Self::Failed { .. })
    }
}

/// A snapshot file in the heap directory. Dropping the last handle deletes the file. Creating it
/// (with mode 0600 on Unix) is the capture's job.
#[derive(Debug)]
pub struct HeapFile {
    path: PathBuf,
}

impl HeapFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for HeapFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // The kind only: the path names the user's directories.
            Err(e) => log::debug!("could not delete a heap snapshot file: {:?}", e.kind()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HeapRecord {
    pub id: HeapId,
    pub run: RunId,
    pub isolate: Arc<str>,
    pub requested: SessionTime,
    /// The first chunk's timestamp.
    pub time: Option<SessionTime>,
    pub bytes: u64,
    pub stage: HeapStage,
    pub file: Option<Arc<HeapFile>>,
    pub summary: Option<Arc<HeapSummary>>,
    pub diff: Option<Arc<HeapDiff>>,
    pub index: Option<Arc<HeapIndex>>,
}

impl HeapRecord {
    /// The leak findings plus the classes the diff shows as grown.
    pub fn suspects(&self) -> u32 {
        let findings = self.summary.as_ref().map_or(0, |s| s.findings_total);
        let grown = self
            .diff
            .as_ref()
            .map_or(0, |d| d.user.len() + d.lifecycle.len());
        findings.saturating_add(u32::try_from(grown).unwrap_or(u32::MAX))
    }
}

#[derive(Clone, Debug)]
pub struct HeapSummary {
    pub objects: u64,
    pub reachable: u64,
    pub unreachable_bytes: u64,
    pub reachable_bytes: u64,
    pub header_shallow: u64,
    pub header_capacity: u64,
    pub header_external: u64,
    pub mode: Mode,
    pub root_package: Option<Arc<str>>,
    /// Indexed by snapshot class id.
    pub classes: Arc<[HeapClass]>,
    pub findings: Arc<[LeakFinding]>,
    pub findings_total: u32,
    pub checks: LeakChecks,
    pub http: Option<HttpHeld>,
    pub tracked: Arc<[Tracked]>,
}

/// The counts cover reachable objects only.
#[derive(Clone, Debug)]
pub struct HeapClass {
    pub name: Arc<str>,
    pub library: Arc<str>,
    pub user: bool,
    pub lifecycle: Option<LifecycleKind>,
    pub fields: Arc<[(u32, Arc<str>)]>,
    pub count: u64,
    pub shallow: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifecycleKind {
    State,
    Element,
    Notifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    Done,
    NotFound,
    Undecidable(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakChecks {
    pub states: Check,
    pub elements: Check,
    pub notifiers: Check,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LeakKind {
    UnmountedState,
    DefunctElement,
    DisposedNotifier,
}

#[derive(Clone, Debug)]
pub struct LeakFinding {
    pub kind: LeakKind,
    pub node: u32,
    pub class: u32,
    pub retained: u64,
    pub path: Arc<[PathHop]>,
    pub culprit: Option<u16>,
    /// `(class, retained)` of what the object keeps alive.
    pub keeps: Arc<[(u32, u64)]>,
}

/// `edge` is how this hop reaches the next one; the last hop has [`Edge::None`].
#[derive(Clone, Debug)]
pub struct PathHop {
    pub node: u32,
    pub class: u32,
    pub retained: u64,
    pub edge: Edge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    None,
    Field(Arc<str>),
    Static(Arc<str>),
    Element(u32),
    ClosureContext,
    Captured(u32),
    RootSlice(Arc<str>),
    Internal(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpHeld {
    pub bytes: u64,
    pub requests: u64,
}

/// The identity hashes of a user or lifecycle class's objects, for matching across snapshots.
#[derive(Clone, Debug)]
pub struct Tracked {
    pub class: u32,
    /// Sorted.
    pub hashes: Arc<[u32]>,
    /// Parallel to `hashes`.
    pub nodes: Arc<[u32]>,
}

#[derive(Clone, Debug)]
pub struct HeapDiff {
    pub baseline: HeapId,
    pub comparable: Result<(), Arc<str>>,
    pub user: Arc<[Growth]>,
    pub lifecycle: Arc<[Growth]>,
}

/// `(node, retained, path)`.
pub type GrowthSample = (u32, u64, Arc<[PathHop]>);

#[derive(Clone, Debug)]
pub struct Growth {
    pub class: u32,
    pub before: u64,
    pub after: u64,
    pub new_objects: u64,
    pub new_bytes: u64,
    /// `(node, retained, path)` of a few new objects.
    pub samples: Arc<[GrowthSample]>,
}

/// Longest path [`HeapIndex::path`] returns.
pub const MAX_PATH_HOPS: usize = 200;

/// The held snapshot's graph over nodes `0..n`; node 0 is the root. The children of a node are
/// a contiguous range of node ids, sorted by retained size, descending.
pub struct HeapIndex {
    orig: Vec<u32>,
    class: Vec<u32>,
    shallow: Vec<u32>,
    retained: Vec<u64>,
    first_child: Vec<u32>,
    retainer: Vec<u32>,
    offsets: Vec<u32>,
    mint_class: Option<u32>,
    names: Vec<(u32, Arc<str>)>,
    file: Arc<HeapFile>,
}

/// The fields of a [`HeapIndex`], for `fdt_analysis` to fill in.
pub struct HeapIndexParts {
    pub orig: Vec<u32>,
    pub class: Vec<u32>,
    pub shallow: Vec<u32>,
    pub retained: Vec<u64>,
    /// `n + 1` entries: node `i`'s children are `first_child[i]..first_child[i + 1]`.
    pub first_child: Vec<u32>,
    pub retainer: Vec<u32>,
    pub offsets: Vec<u32>,
    pub mint_class: Option<u32>,
    /// The name data of synthetic nodes, by node id.
    pub names: Vec<(u32, Arc<str>)>,
    pub file: Arc<HeapFile>,
}

impl std::fmt::Debug for HeapIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeapIndex")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl HeapIndex {
    /// # Panics
    /// When the per-node vectors differ in length, or `first_child` is not `n + 1` long.
    pub fn from_parts(p: HeapIndexParts) -> Self {
        let n = p.orig.len();
        assert!(
            p.class.len() == n
                && p.shallow.len() == n
                && p.retained.len() == n
                && p.retainer.len() == n
                && p.first_child.len() == n + 1,
            "heap index parts must have one entry per node (and n + 1 child starts)"
        );
        debug_assert!(p.first_child.windows(2).all(|w| w[0] <= w[1]));
        let mut names = p.names;
        if !names.is_sorted_by_key(|n| n.0) {
            names.sort_by_key(|n| n.0);
        }
        Self {
            orig: p.orig,
            class: p.class,
            shallow: p.shallow,
            retained: p.retained,
            first_child: p.first_child,
            retainer: p.retainer,
            offsets: p.offsets,
            mint_class: p.mint_class,
            names,
            file: p.file,
        }
    }

    pub fn len(&self) -> usize {
        self.orig.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orig.is_empty()
    }

    pub fn class_of(&self, node: u32) -> u32 {
        self.class[node as usize]
    }

    pub fn shallow(&self, node: u32) -> u32 {
        self.shallow[node as usize]
    }

    pub fn retained(&self, node: u32) -> u64 {
        self.retained[node as usize]
    }

    /// The immediate dominator of `node`: the node whose children range holds it (the ranges
    /// are assigned in parent order, so `first_child` is nondecreasing). The root, which has
    /// none, gives 0. O(log n).
    pub fn idom(&self, node: u32) -> u32 {
        if node == 0 {
            return 0;
        }
        (self.first_child.partition_point(|&f| f <= node) - 1) as u32
    }

    pub fn retainer(&self, node: u32) -> u32 {
        self.retainer[node as usize]
    }

    /// The snapshot's own object index of `node`.
    pub fn orig(&self, node: u32) -> u32 {
        self.orig[node as usize]
    }

    /// The node ids of `node`'s dominator-tree children; empty for an unknown node.
    pub fn children(&self, node: u32) -> Range<u32> {
        match (
            self.first_child.get(node as usize),
            self.first_child.get(node as usize + 1),
        ) {
            (Some(&a), Some(&b)) => a..b,
            _ => 0..0,
        }
    }

    /// The nodes from the root to `node` along `retainer`. Capped at [`MAX_PATH_HOPS`]: a longer
    /// path keeps the hops nearest `node`, so it starts below the root.
    pub fn path(&self, node: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = node;
        while (at as usize) < self.len() && out.len() < MAX_PATH_HOPS {
            out.push(at);
            if at == 0 {
                break;
            }
            at = self.retainer[at as usize];
        }
        out.reverse();
        out
    }

    pub fn synthetic_name(&self, node: u32) -> Option<&str> {
        let i = self.names.binary_search_by_key(&node, |n| n.0).ok()?;
        Some(&self.names[i].1)
    }

    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    pub fn mint_class(&self) -> Option<u32> {
        self.mint_class
    }

    pub fn file(&self) -> &Arc<HeapFile> {
        &self.file
    }

    /// What the index holds, summing capacities.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        self.orig.capacity() * size_of::<u32>()
            + self.class.capacity() * size_of::<u32>()
            + self.shallow.capacity() * size_of::<u32>()
            + self.retained.capacity() * size_of::<u64>()
            + self.first_child.capacity() * size_of::<u32>()
            + self.retainer.capacity() * size_of::<u32>()
            + self.offsets.capacity() * size_of::<u32>()
            + self.names.capacity() * size_of::<(u32, Arc<str>)>()
            + self.names.iter().map(|n| n.1.len()).sum::<usize>()
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// A file in the temp directory, which exists until its `HeapFile` drops.
    pub fn temp_heap_file() -> Arc<HeapFile> {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fdt-store-test-{}-{n}.dartheap",
            std::process::id()
        ));
        std::fs::write(&path, b"x").unwrap();
        Arc::new(HeapFile::new(path))
    }

    /// A tree: 0 has children 1..4, 1 has 4..6, the rest are leaves.
    pub fn tiny_index(file: Arc<HeapFile>) -> HeapIndex {
        HeapIndex::from_parts(HeapIndexParts {
            orig: (0..6).collect(),
            class: vec![0, 1, 1, 1, 2, 2],
            shallow: vec![0, 10, 10, 10, 5, 5],
            retained: vec![100, 40, 30, 20, 8, 5],
            first_child: vec![1, 4, 6, 6, 6, 6, 6],
            retainer: vec![0, 0, 0, 0, 1, 1],
            offsets: Vec::new(),
            mint_class: None,
            names: vec![(3, "root slice".into())],
            file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn children_ranges_are_contiguous() {
        let ix = tiny_index(temp_heap_file());
        assert_eq!(ix.children(0), 1..4);
        assert_eq!(ix.children(1), 4..6);
        for leaf in 2..6 {
            assert!(ix.children(leaf).is_empty());
        }
        assert_eq!(ix.children(99), 0..0);
        // Every non-root node is the child of exactly the node whose range holds it.
        for node in 1..ix.len() as u32 {
            let parents: Vec<u32> = (0..ix.len() as u32)
                .filter(|&p| ix.children(p).contains(&node))
                .collect();
            assert_eq!(parents, vec![ix.idom(node)], "node {node}");
        }
        // Siblings are sorted by retained size, descending.
        let r: Vec<u64> = ix.children(0).map(|c| ix.retained(c)).collect();
        assert!(r.is_sorted_by(|a, b| a >= b));
    }

    #[test]
    fn path_runs_from_the_root_and_is_capped() {
        let ix = tiny_index(temp_heap_file());
        assert_eq!(ix.path(5), vec![0, 1, 5]);
        assert_eq!(ix.path(0), vec![0]);
        assert!(ix.path(77).is_empty());
        assert_eq!(ix.synthetic_name(3), Some("root slice"));
        assert_eq!(ix.synthetic_name(2), None);
        assert!(ix.heap_bytes() >= 6 * 4 * 6);

        // A chain of 1,000 nodes: the path keeps the 200 hops nearest the node.
        let n = 1_000u32;
        let long = HeapIndex::from_parts(HeapIndexParts {
            orig: (0..n).collect(),
            class: vec![0; n as usize],
            shallow: vec![0; n as usize],
            retained: vec![0; n as usize],
            first_child: (0..=n).map(|i| (i + 1).min(n)).collect(),
            retainer: (0..n).map(|i| i.saturating_sub(1)).collect(),
            offsets: Vec::new(),
            mint_class: None,
            names: Vec::new(),
            file: temp_heap_file(),
        });
        let p = long.path(n - 1);
        assert_eq!(p.len(), MAX_PATH_HOPS);
        assert_eq!(*p.last().unwrap(), n - 1);
    }

    #[test]
    #[should_panic(expected = "one entry per node")]
    fn from_parts_checks_lengths() {
        HeapIndex::from_parts(HeapIndexParts {
            orig: vec![0, 1],
            class: vec![0],
            shallow: vec![0, 0],
            retained: vec![0, 0],
            first_child: vec![0, 0, 0],
            retainer: vec![0, 0],
            offsets: Vec::new(),
            mint_class: None,
            names: Vec::new(),
            file: temp_heap_file(),
        });
    }

    #[test]
    fn suspects_add_findings_and_grown_classes() {
        let mut rec = HeapRecord {
            id: HeapId(0),
            run: RunId(1),
            isolate: "isolates/1".into(),
            requested: SessionTime(0),
            time: None,
            bytes: 0,
            stage: HeapStage::Requested,
            file: None,
            summary: None,
            diff: None,
            index: None,
        };
        assert_eq!(rec.suspects(), 0);
        let growth = || Growth {
            class: 1,
            before: 1,
            after: 3,
            new_objects: 2,
            new_bytes: 20,
            samples: Arc::from(Vec::new()),
        };
        rec.diff = Some(Arc::new(HeapDiff {
            baseline: HeapId(0),
            comparable: Ok(()),
            user: Arc::from(vec![growth(), growth()]),
            lifecycle: Arc::from(vec![growth()]),
        }));
        assert_eq!(rec.suspects(), 3);
    }
}
