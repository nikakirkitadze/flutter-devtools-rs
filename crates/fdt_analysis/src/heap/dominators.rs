//! Heap reachability, dominators, retained sizes and the held index.
//!
//! Memory is the constraint at five million objects (about 80 B per object at the peak), so the
//! passes run in a fixed order and each frees what the next does not need:
//!
//! 1. parse, then breadth-first retainers (the shortest paths);
//! 2. (the scan hook: the forward references still exist here);
//! 3. depth-first numbering and predecessor lists, then the forward references are dropped;
//! 4. Lengauer-Tarjan, then retained sizes;
//! 5. renumbering into the held index.
//!
//! Nothing here recurses: the depth-first search, the path compression and the breadth-first
//! searches use explicit stacks and queues, so a million-object chain is no deeper than a list.

use std::cmp::Reverse;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fdt_store::heap::{HeapClass, HeapFile, HeapIndex, HeapIndexParts, HeapStage, HeapSummary};
use fdt_store::run::Mode;

use super::format::{ClassDef, Graph, HeapError, HeapHeader, Value, parse, read_object};
use super::{leaks, sdk};

/// How often the long loops look at the cancel flag.
const CANCEL_MASK: usize = 0xFFFF;

/// Which classes are weak, indexed by class id (`classes[0]` is the sentinel).
pub fn weak_classes(classes: &[ClassDef]) -> Vec<bool> {
    classes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            i != 0
                && sdk::WEAK
                    .iter()
                    .any(|(n, u)| c.name == *n && c.library_uri == *u)
        })
        .collect()
}

fn is_weak(weak: &[bool], class: u32) -> bool {
    weak.get(class as usize).copied().unwrap_or(false)
}

fn check(cancel: &AtomicBool) -> Result<(), HeapError> {
    if cancel.load(Ordering::Relaxed) {
        Err(HeapError::Cancelled)
    } else {
        Ok(())
    }
}

fn no_root() -> HeapError {
    HeapError::OutOfRange {
        what: "root",
        at: 0,
    }
}

/// A breadth-first search from object 1. `result[o]` is the object that first reached `o`;
/// 0 for the root and for unreachable objects. Weak classes' out-edges are not followed.
pub fn shortest_retainers(g: &Graph, weak: &[bool]) -> Vec<u32> {
    let n = g.object_count();
    let mut retainer = vec![0u32; n + 1];
    if n == 0 {
        return retainer;
    }
    let mut seen = vec![0u64; (n + 1).div_ceil(64)];
    let mut queue: Vec<u32> = Vec::with_capacity(n);
    seen[0] |= 1 << 1;
    queue.push(1);
    let mut head = 0;
    while head < queue.len() {
        let o = queue[head];
        head += 1;
        if is_weak(weak, g.class_of[o as usize]) {
            continue;
        }
        for &t in g.refs_of(o) {
            let (word, bit) = (t as usize / 64, 1u64 << (t % 64));
            if t == 0 || seen[word] & bit != 0 {
                continue;
            }
            seen[word] |= bit;
            retainer[t as usize] = o;
            queue.push(t);
        }
    }
    retainer
}

/// Dominators in depth-first numbering. Vertex 0 is a sentinel and vertex 1 is the root.
pub struct Dominated {
    /// DFS number to object.
    pub vertex: Vec<u32>,
    /// DFS number to the DFS number of the immediate dominator; the root's is 0.
    pub idom: Vec<u32>,
    pub retained: Vec<u64>,
    pub header: HeapHeader,
    pub classes: Vec<ClassDef>,
    pub class_of: Vec<u32>,
    pub shallow: Vec<u32>,
    pub identity_hash: Vec<u32>,
    pub offsets: Vec<u32>,
    pub mint_class: Option<u32>,
}

/// Path-compressing `eval` of Lengauer-Tarjan, with an explicit stack.
fn eval(
    v: u32,
    ancestor: &mut [u32],
    label: &mut [u32],
    semi: &[u32],
    stack: &mut Vec<u32>,
) -> u32 {
    if ancestor[v as usize] == 0 {
        return v;
    }
    stack.clear();
    let mut x = v;
    while ancestor[ancestor[x as usize] as usize] != 0 {
        stack.push(x);
        x = ancestor[x as usize];
    }
    while let Some(y) = stack.pop() {
        let a = ancestor[y as usize];
        if semi[label[a as usize] as usize] < semi[label[y as usize] as usize] {
            label[y as usize] = label[a as usize];
        }
        ancestor[y as usize] = ancestor[a as usize];
    }
    label[v as usize]
}

/// Numbers the objects reachable from object 1 through non-weak edges, then computes immediate
/// dominators and retained sizes. Consumes the graph and drops its forward references as soon
/// as the predecessor lists exist.
pub fn dominators(g: Graph, weak: &[bool], cancel: &AtomicBool) -> Result<Dominated, HeapError> {
    let n = g.object_count();
    if n == 0 {
        return Err(no_root());
    }
    check(cancel)?;
    let Graph {
        header,
        classes,
        class_of,
        shallow,
        kind,
        first_ref,
        refs,
        identity_hash,
        offsets,
        mint_class,
    } = g;
    drop(kind);

    // Depth-first preorder, numbered when a vertex is discovered.
    let mut num = vec![0u32; n + 1];
    // Sized for every object so neither grows by doubling; shrunk once the search is done.
    let mut vertex: Vec<u32> = Vec::with_capacity(n + 1);
    let mut parent: Vec<u32> = Vec::with_capacity(n + 1);
    vertex.extend([0, 1]);
    parent.extend([0, 0]);
    num[1] = 1;
    // Holds one frame (8 B) per vertex on the current path: at most `r` deep (a chain), and
    // freed before the predecessor lists are allocated, so it never sits at the pass-3 peak.
    let mut stack: Vec<(u32, u32)> = Vec::new();
    // A weak object starts with its cursor at the end: its out-edges are not followed.
    let start_of = |o: u32| {
        if is_weak(weak, class_of[o as usize]) {
            first_ref[o as usize + 1]
        } else {
            first_ref[o as usize]
        }
    };
    stack.push((1, start_of(1)));
    let mut steps = 0usize;
    while let Some(top) = stack.last_mut() {
        steps += 1;
        if steps & CANCEL_MASK == 0 {
            check(cancel)?;
        }
        let (v, cur) = *top;
        let o = vertex[v as usize];
        if cur < first_ref[o as usize + 1] {
            top.1 += 1;
            let t = refs[cur as usize];
            if t != 0 && num[t as usize] == 0 {
                let w = vertex.len() as u32;
                num[t as usize] = w;
                vertex.push(t);
                parent.push(v);
                stack.push((w, start_of(t)));
            }
        } else {
            stack.pop();
        }
    }
    drop(stack);
    vertex.shrink_to_fit();
    parent.shrink_to_fit();
    let r = vertex.len() - 1;

    // Predecessors as a CSR over DFS numbers. `first_pred[w]` ends as the start of `w`'s list;
    // `first_pred[r + 1]` is the total.
    let mut first_pred = vec![0u32; r + 2];
    let mut edges = 0u64;
    for (v, &o) in vertex.iter().enumerate().skip(1) {
        if is_weak(weak, class_of[o as usize]) {
            continue;
        }
        for &t in &refs[first_ref[o as usize] as usize..first_ref[o as usize + 1] as usize] {
            if t != 0 {
                first_pred[num[t as usize] as usize] += 1;
                edges += 1;
            }
        }
        if v & CANCEL_MASK == 0 {
            check(cancel)?;
        }
    }
    let total = u32::try_from(edges).map_err(|_| HeapError::Overflow("reference count"))?;
    for w in 1..=r {
        first_pred[w] += first_pred[w - 1];
    }
    first_pred[r + 1] = total;
    let mut preds = vec![0u32; total as usize];
    for (v, &o) in vertex.iter().enumerate().skip(1) {
        if is_weak(weak, class_of[o as usize]) {
            continue;
        }
        for &t in &refs[first_ref[o as usize] as usize..first_ref[o as usize + 1] as usize] {
            if t != 0 {
                let slot = &mut first_pred[num[t as usize] as usize];
                *slot -= 1;
                preds[*slot as usize] = v as u32;
            }
        }
    }
    // The forward references are not needed again.
    drop(num);
    drop(refs);
    drop(first_ref);
    check(cancel)?;

    // Lengauer-Tarjan (the "simple" variant: path compression, no balancing).
    let mut semi: Vec<u32> = (0..=r as u32).collect();
    let mut label = semi.clone();
    let mut ancestor = vec![0u32; r + 1];
    let mut idom = vec![0u32; r + 1];
    let mut head = vec![0u32; r + 1];
    let mut next = vec![0u32; r + 1];
    let mut path: Vec<u32> = Vec::new();
    for w in (2..=r).rev() {
        if w & CANCEL_MASK == 0 {
            check(cancel)?;
        }
        let (from, to) = (first_pred[w] as usize, first_pred[w + 1] as usize);
        for &v in &preds[from..to] {
            let u = eval(v, &mut ancestor, &mut label, &semi, &mut path);
            if semi[u as usize] < semi[w] {
                semi[w] = semi[u as usize];
            }
        }
        let s = semi[w] as usize;
        next[w] = head[s];
        head[s] = w as u32;
        let p = parent[w];
        ancestor[w] = p;
        let mut v = std::mem::take(&mut head[p as usize]);
        while v != 0 {
            let following = next[v as usize];
            let u = eval(v, &mut ancestor, &mut label, &semi, &mut path);
            idom[v as usize] = if semi[u as usize] < semi[v as usize] {
                u
            } else {
                p
            };
            v = following;
        }
    }
    for w in 2..=r {
        if idom[w] != semi[w] {
            idom[w] = idom[idom[w] as usize];
        }
    }
    idom[1] = 0;
    drop((
        preds, first_pred, semi, label, ancestor, head, next, path, parent,
    ));
    check(cancel)?;

    // Retained sizes: children have larger DFS numbers than their dominators.
    let mut retained = vec![0u64; r + 1];
    for v in 1..=r {
        retained[v] = u64::from(shallow[vertex[v] as usize]);
    }
    for w in (2..=r).rev() {
        let sum = retained[w];
        retained[idom[w] as usize] += sum;
    }

    Ok(Dominated {
        vertex,
        idom,
        retained,
        header,
        classes,
        class_of,
        shallow,
        identity_hash,
        offsets,
        mint_class,
    })
}

/// Renumbers into the held index: node ids are the breadth-first order of the dominator tree,
/// whose children are sorted by retained size (descending, then DFS number), so every node's
/// children are contiguous. `retainers` is [`shortest_retainers`]'s result and `names` are the
/// name data of synthetic objects. Also returns the node of every object (`u32::MAX` if
/// unreachable).
pub fn build_index(
    d: Dominated,
    retainers: Vec<u32>,
    names: Vec<(u32, Arc<str>)>,
    file: Arc<HeapFile>,
) -> (HeapIndex, Vec<u32>) {
    let Dominated {
        vertex,
        idom,
        retained,
        class_of,
        shallow,
        identity_hash,
        offsets,
        mint_class,
        ..
    } = d;
    drop(identity_hash);
    let r = vertex.len() - 1;
    let mut obj_to_node = vec![u32::MAX; class_of.len()];
    if r == 0 {
        let parts = HeapIndexParts {
            orig: Vec::new(),
            class: Vec::new(),
            shallow: Vec::new(),
            retained: Vec::new(),
            first_child: vec![0],
            retainer: Vec::new(),
            offsets,
            mint_class,
            names: Vec::new(),
            file,
        };
        return (HeapIndex::from_parts(parts), obj_to_node);
    }

    // Dominator children in DFS space. `ends[p]` is where `p`'s list ends while counting, then
    // where it starts once filled; `ends[r + 1]` is the total.
    let mut ends = vec![0u32; r + 2];
    for w in 2..=r {
        ends[idom[w] as usize] += 1;
    }
    for p in 1..=r {
        ends[p] += ends[p - 1];
    }
    ends[r + 1] = (r - 1) as u32;
    let mut kids = vec![0u32; r - 1];
    for w in 2..=r {
        let slot = &mut ends[idom[w] as usize];
        *slot -= 1;
        kids[*slot as usize] = w as u32;
    }
    for p in 1..=r {
        kids[ends[p] as usize..ends[p + 1] as usize]
            .sort_unstable_by_key(|&c| (Reverse(retained[c as usize]), c));
    }

    // Breadth-first over the tree: a node's children are appended together.
    let mut order: Vec<u32> = Vec::with_capacity(r);
    let mut node_of = vec![0u32; r + 1];
    let mut first_child: Vec<u32> = Vec::with_capacity(r + 1);
    order.push(1);
    let mut i = 0;
    while i < order.len() {
        let v = order[i] as usize;
        first_child.push(order.len() as u32);
        for &c in &kids[ends[v] as usize..ends[v + 1] as usize] {
            node_of[c as usize] = order.len() as u32;
            order.push(c);
        }
        i += 1;
    }
    first_child.push(order.len() as u32);
    drop((kids, ends));

    let orig: Vec<u32> = order.iter().map(|&v| vertex[v as usize]).collect();
    for (node, &o) in orig.iter().enumerate() {
        obj_to_node[o as usize] = node as u32;
    }
    let class: Vec<u32> = orig.iter().map(|&o| class_of[o as usize]).collect();
    let shallow: Vec<u32> = orig.iter().map(|&o| shallow[o as usize]).collect();
    let retained: Vec<u64> = order.iter().map(|&v| retained[v as usize]).collect();
    drop((order, node_of, vertex));
    let retainer: Vec<u32> = orig
        .iter()
        .enumerate()
        .map(|(node, &o)| {
            if node == 0 {
                return 0;
            }
            let at = obj_to_node[retainers[o as usize] as usize];
            debug_assert_ne!(at, u32::MAX, "a reachable object's retainer is reachable");
            if at == u32::MAX { 0 } else { at }
        })
        .collect();
    let names = names
        .into_iter()
        .filter_map(|(o, name)| {
            let node = *obj_to_node.get(o as usize)?;
            (node != u32::MAX).then_some((node, name))
        })
        .collect();

    let parts = HeapIndexParts {
        orig,
        class,
        shallow,
        retained,
        first_child,
        retainer,
        offsets,
        mint_class,
        names,
        file,
    };
    (HeapIndex::from_parts(parts), obj_to_node)
}

pub struct AnalyzeInput {
    pub file: Arc<HeapFile>,
    pub flutter_version: Option<String>,
    pub mode: Mode,
    pub root_package: Option<String>,
}

pub struct Analysis {
    pub summary: HeapSummary,
    pub index: HeapIndex,
}

fn io_error(e: std::io::Error) -> HeapError {
    // The kind only: the message can name the user's directories.
    HeapError::Io(e.kind().to_string())
}

/// Parses the snapshot file, finds what is reachable and dominated, and builds the held index
/// and the summary. Cancelling returns `Err(Cancelled)` and leaves the file to its owner.
pub fn analyze(
    input: &AnalyzeInput,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(HeapStage),
) -> Result<Analysis, HeapError> {
    progress(HeapStage::Parsing { fraction: 0.0 });
    check(cancel)?;
    let file = File::open(input.file.path()).map_err(io_error)?;
    let len = file.metadata().map_err(io_error)?.len();
    let g = parse(file, len, cancel, &mut |at| {
        let fraction = if len == 0 {
            0.0
        } else {
            (at as f64 / len as f64).min(1.0) as f32
        };
        progress(HeapStage::Parsing { fraction });
    })?;
    check(cancel)?;
    if g.object_count() == 0 {
        return Err(no_root());
    }

    progress(HeapStage::Reachability);
    let weak = weak_classes(&g.classes);
    let retainers = shortest_retainers(&g, &weak);
    check(cancel)?;

    // The scan hook: the forward references still exist, the predecessor lists do not yet.
    let rows = sdk::rows_for(input.flutter_version.as_deref());
    let scan_file = File::open(input.file.path()).map_err(io_error)?;
    let scan = leaks::scan(
        &g,
        &retainers,
        &scan_file,
        &rows,
        input.root_package.as_deref(),
        cancel,
    )?;
    drop(scan_file);

    progress(HeapStage::Dominators);
    let objects = g.object_count() as u64;
    let total_bytes: u64 = g.shallow[1..].iter().map(|&s| u64::from(s)).sum();
    let d = dominators(g, &weak, cancel)?;
    let reachable = (d.vertex.len() - 1) as u64;
    let reachable_bytes = d.retained[1];

    let checks = scan.checks.clone();
    let mut counts = vec![(0u64, 0u64); d.classes.len()];
    for &o in &d.vertex[1..] {
        let c = &mut counts[d.class_of[o as usize] as usize];
        c.0 += 1;
        c.1 += u64::from(d.shallow[o as usize]);
    }
    let classes: Vec<HeapClass> = d
        .classes
        .iter()
        .zip(&counts)
        .enumerate()
        .map(|(i, (c, &(count, shallow)))| HeapClass {
            name: c.name.as_str().into(),
            library: c.library_uri.as_str().into(),
            user: scan.user.get(i).copied().unwrap_or(false),
            lifecycle: scan.lifecycle.get(i).copied().flatten(),
            fields: c
                .fields
                .iter()
                .map(|f| (f.index, Arc::<str>::from(f.name.as_str())))
                .collect(),
            count,
            shallow,
        })
        .collect();

    // The synthetic classes (1-5) name their objects in the data; a handful are reachable.
    let mut names = Vec::new();
    let f = File::open(input.file.path()).map_err(io_error)?;
    for &o in &d.vertex[1..] {
        if (1..=5).contains(&d.class_of[o as usize]) {
            let rec = read_object(&f, &d.offsets, d.mint_class, o).map_err(io_error)?;
            if let Value::Name(name) = rec.value {
                names.push((o, Arc::<str>::from(name.as_str())));
            }
        }
    }

    check(cancel)?;
    let header = (
        d.header.shallow_size,
        d.header.capacity,
        d.header.external_size,
    );
    let (index, node_of) = build_index(d, retainers, names, input.file.clone());
    let mut summary = HeapSummary {
        objects,
        reachable,
        unreachable_bytes: total_bytes.saturating_sub(reachable_bytes),
        reachable_bytes,
        header_shallow: header.0,
        header_capacity: header.1,
        header_external: header.2,
        mode: input.mode,
        root_package: input.root_package.as_deref().map(Arc::from),
        classes: classes.into(),
        findings: Arc::from([]),
        findings_total: 0,
        checks,
        http: None,
        tracked: Arc::from([]),
    };
    let found = leaks::finalize(scan, &index, &node_of, &summary)?;
    drop(node_of);
    summary.findings = found.findings.into();
    summary.findings_total = found.total;
    summary.http = found.http;
    summary.tracked = found.tracked.into();
    progress(HeapStage::Ready);
    Ok(Analysis { summary, index })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::encode::{SnapshotBuilder, encode_full};
    use crate::heap::format::{FullSnapshot, parse};
    use std::collections::HashMap;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/heap/jank_lab-profile-after-leaks.dartheap"
    );

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn graph_of(snap: &FullSnapshot) -> Graph {
        let bytes = encode_full(snap);
        parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .expect("parse")
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// The snapshot as a file that lives as long as the returned directory.
    fn heap_file(bytes: &[u8]) -> (tempfile::TempDir, Arc<HeapFile>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.dartheap");
        std::fs::write(&path, bytes).unwrap();
        (dir, Arc::new(HeapFile::new(path)))
    }

    fn input(file: &Arc<HeapFile>) -> AnalyzeInput {
        AnalyzeInput {
            file: file.clone(),
            flutter_version: None,
            mode: Mode::Profile,
            root_package: None,
        }
    }

    fn run(snap: &FullSnapshot) -> (tempfile::TempDir, Analysis) {
        let (dir, file) = heap_file(&encode_full(snap));
        let a = analyze(&input(&file), &no_cancel(), &mut |_| {}).expect("analyze");
        (dir, a)
    }

    /// Object to immediate-dominator object, from `dominators`.
    fn idoms(d: &Dominated) -> HashMap<u32, u32> {
        (2..d.vertex.len())
            .map(|v| (d.vertex[v], d.vertex[d.idom[v] as usize]))
            .collect()
    }

    // ---- the oracle: Cooper-Harvey-Kennedy over explicit adjacency ----

    struct Oracle {
        /// By object; 0 for the root and for unreachable objects.
        idom: Vec<u32>,
        /// Reachable objects, reverse postorder.
        rpo: Vec<u32>,
    }

    fn adjacency(g: &Graph) -> Vec<Vec<u32>> {
        (0..=g.object_count() as u32)
            .map(|o| g.refs_of(o).iter().copied().filter(|&t| t != 0).collect())
            .collect()
    }

    fn oracle(adj: &[Vec<u32>], class_of: &[u32], weak: &[bool]) -> Oracle {
        let n = adj.len();
        let edges = |o: u32| -> &[u32] {
            if is_weak(weak, class_of[o as usize]) {
                &[]
            } else {
                &adj[o as usize]
            }
        };
        // Postorder by an explicit stack.
        let mut post = Vec::new();
        let mut seen = vec![false; n];
        let mut stack = vec![(1u32, 0usize)];
        seen[1] = true;
        while let Some(&mut (o, ref mut i)) = stack.last_mut() {
            if let Some(&t) = edges(o).get(*i) {
                *i += 1;
                if !seen[t as usize] {
                    seen[t as usize] = true;
                    stack.push((t, 0));
                }
            } else {
                post.push(o);
                stack.pop();
            }
        }
        let mut po = vec![usize::MAX; n];
        for (i, &o) in post.iter().enumerate() {
            po[o as usize] = i;
        }
        let rpo: Vec<u32> = post.iter().rev().copied().collect();
        let mut preds = vec![Vec::new(); n];
        for &o in &post {
            for &t in edges(o) {
                preds[t as usize].push(o);
            }
        }
        let mut idom = vec![u32::MAX; n];
        idom[1] = 1;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new = u32::MAX;
                for &p in &preds[b as usize] {
                    if idom[p as usize] == u32::MAX {
                        continue;
                    }
                    new = if new == u32::MAX {
                        p
                    } else {
                        let (mut x, mut y) = (p, new);
                        while x != y {
                            while po[x as usize] < po[y as usize] {
                                x = idom[x as usize];
                            }
                            while po[y as usize] < po[x as usize] {
                                y = idom[y as usize];
                            }
                        }
                        x
                    };
                }
                if idom[b as usize] != new {
                    idom[b as usize] = new;
                    changed = true;
                }
            }
        }
        idom[1] = 0;
        for (o, i) in idom.iter_mut().enumerate() {
            if !seen[o] {
                *i = 0;
            }
        }
        Oracle { idom, rpo }
    }

    /// Compares `dominators` with the oracle on the same graph: the reachable set, every
    /// immediate dominator and every retained size.
    fn assert_matches_oracle(g: Graph, what: &str) {
        let adj = adjacency(&g);
        let weak = weak_classes(&g.classes);
        let class_of = g.class_of.clone();
        let shallow = g.shallow.clone();
        let want = oracle(&adj, &class_of, &weak);
        let d = dominators(g, &weak, &no_cancel()).expect("dominators");
        assert_eq!(
            d.vertex.len() - 1,
            want.rpo.len(),
            "{what}: reachable count"
        );
        let got = idoms(&d);
        for &o in want.rpo.iter().skip(1) {
            assert_eq!(
                got.get(&o),
                Some(&want.idom[o as usize]),
                "{what}: idom of {o}"
            );
        }
        let mut ret = vec![0u64; adj.len()];
        for &o in &want.rpo {
            ret[o as usize] = u64::from(shallow[o as usize]);
        }
        for &o in want.rpo.iter().skip(1).rev() {
            let up = ret[o as usize];
            ret[want.idom[o as usize] as usize] += up;
        }
        for v in 1..d.vertex.len() {
            assert_eq!(
                d.retained[v], ret[d.vertex[v] as usize],
                "{what}: retained of {}",
                d.vertex[v]
            );
        }
    }

    /// A random snapshot: `1..=max` plain or weak objects with random references (zero
    /// included) from every object, the slices and the read-only object.
    fn random_snapshot(seed: u64, max: u64) -> FullSnapshot {
        let mut r = Rng(seed);
        let mut b = SnapshotBuilder::new();
        let plain = b.class("Node", "package:app/a.dart", &["a", "b"]);
        let weak = b.class("_WeakProperty", "dart:core", &["key", "value"]);
        let count = 1 + r.below(max);
        let ids: Vec<u32> = (0..count)
            .map(|_| {
                let class = if r.below(8) == 0 { weak } else { plain };
                let shallow = r.below(100);
                b.object(class, shallow, Value::None, &[])
            })
            .collect();
        let top = 5 + count;
        let refs = |r: &mut Rng| -> Vec<u32> {
            (0..r.below(5)).map(|_| r.below(top + 1) as u32).collect()
        };
        for &o in &ids {
            let v = refs(&mut r);
            b.set_refs(o, &v);
        }
        for s in [3, 4] {
            let v = refs(&mut r);
            b.set_refs(s, &v);
        }
        for t in refs(&mut r) {
            if t != 0 {
                b.read_only(t);
            }
        }
        b.finish()
    }

    #[test]
    fn dominator_cases() {
        let mut b = SnapshotBuilder::new();
        let node = b.class("Node", "package:app/a.dart", &["a", "b"]);
        let weak = b.class("_WeakProperty", "dart:core", &["key", "value"]);
        let mk = |b: &mut SnapshotBuilder| b.object(node, 10, Value::None, &[]);
        // Objects 1-5 are the fixed layout; slices are 3 and 4.
        let (diamond, ba, bb, bd) = (mk(&mut b), mk(&mut b), mk(&mut b), mk(&mut b));
        b.set_refs(diamond, &[ba, bb]);
        b.set_refs(ba, &[bd]);
        b.set_refs(bb, &[bd]);
        let (c1, c2, c3) = (mk(&mut b), mk(&mut b), mk(&mut b));
        b.set_refs(c1, &[c2]);
        b.set_refs(c2, &[c3]);
        let (y1, y2) = (mk(&mut b), mk(&mut b));
        b.set_refs(y1, &[y2]);
        b.set_refs(y2, &[y1]);
        let (z, zs) = (mk(&mut b), mk(&mut b));
        b.set_refs(z, &[z, zs]);
        let (shared, s_only) = (mk(&mut b), mk(&mut b));
        b.set_refs(s_only, &[shared]);
        let (u1, u2) = (mk(&mut b), mk(&mut b));
        b.set_refs(u1, &[u2]);
        b.set_refs(u2, &[u1]);
        let (w, behind, other) = (b.object(weak, 7, Value::None, &[]), mk(&mut b), mk(&mut b));
        b.set_refs(w, &[behind, other]);
        b.set_refs(3, &[diamond, c1, y1, z, shared, w, other]);
        b.set_refs(4, &[s_only, shared]);
        let snap = b.finish();
        let g = graph_of(&snap);
        let weak = weak_classes(&g.classes);
        let d = dominators(g, &weak, &no_cancel()).unwrap();
        let got = idoms(&d);
        let reach: Vec<u32> = d.vertex[1..].to_vec();
        for (o, i) in [
            (diamond, 3),
            (ba, diamond),
            (bb, diamond),
            (bd, diamond),
            (c1, 3),
            (c2, c1),
            (c3, c2),
            (y1, 3),
            (y2, y1),
            (z, 3),
            (zs, z),
            (shared, 1),
            (s_only, 4),
            (w, 3),
            (other, 3),
            (3, 1),
            (4, 1),
        ] {
            assert_eq!(got.get(&o), Some(&i), "idom of {o}");
        }
        for gone in [u1, u2, behind] {
            assert!(!reach.contains(&gone), "{gone} is unreachable");
        }
        // The diamond keeps its two arms and the join: 4 * 10; the weak property keeps itself.
        let v_of = |o: u32| d.vertex.iter().position(|&x| x == o).unwrap();
        assert_eq!(d.retained[v_of(diamond)], 40);
        assert_eq!(d.retained[v_of(w)], 7);
        assert_eq!(d.retained[v_of(z)], 20);
    }

    #[test]
    fn zero_objects_has_no_root() {
        let snap = FullSnapshot {
            header: HeapHeader::default(),
            classes: SnapshotBuilder::new().finish().classes,
            objects: Vec::new(),
            externals: Vec::new(),
            hashes: None,
        };
        let g = graph_of(&snap);
        assert!(shortest_retainers(&g, &[]).iter().all(|&r| r == 0));
        let want = Err::<(), _>(HeapError::OutOfRange {
            what: "root",
            at: 0,
        });
        assert_eq!(dominators(g, &[], &no_cancel()).map(|_| ()), want);
        let (_dir, file) = heap_file(&encode_full(&snap));
        let got = analyze(&input(&file), &no_cancel(), &mut |_| {});
        assert!(matches!(
            got,
            Err(HeapError::OutOfRange { what: "root", .. })
        ));
    }

    #[test]
    fn lengauer_tarjan_matches_an_iterative_oracle() {
        for seed in 0..1000 {
            let snap = random_snapshot(seed, 200);
            assert_matches_oracle(graph_of(&snap), &format!("seed {seed}"));
        }
    }

    #[test]
    fn lengauer_tarjan_matches_the_oracle_on_the_fixture() {
        let bytes = std::fs::read(FIXTURE).unwrap();
        let g = parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &no_cancel(),
            &mut |_| {},
        )
        .unwrap();
        assert_matches_oracle(g, "fixture");
    }

    #[test]
    fn root_retains_every_reachable_byte() {
        for seed in 0..50 {
            let snap = random_snapshot(seed, 100);
            let g = graph_of(&snap);
            let weak = weak_classes(&g.classes);
            let retainers = shortest_retainers(&g, &weak);
            let sum: u64 = (1..=g.object_count())
                .filter(|&o| o == 1 || retainers[o] != 0)
                .map(|o| u64::from(g.shallow[o]))
                .sum();
            let d = dominators(g, &weak, &no_cancel()).unwrap();
            assert_eq!(d.retained[1], sum, "seed {seed}");
        }
    }

    /// `HeapIndex::idom` is derived from the children ranges; check it against the oracle.
    fn assert_idom_matches_oracle(snap: &FullSnapshot, what: &str) {
        let g = graph_of(snap);
        let weak = weak_classes(&g.classes);
        let want = oracle(&adjacency(&g), &g.class_of, &weak);
        let (_dir, a) = run(snap);
        let ix = &a.index;
        assert_eq!(ix.len(), want.rpo.len(), "{what}: reachable count");
        assert_eq!(ix.idom(0), 0, "{what}: the root has no dominator");
        let node_of: HashMap<u32, u32> = (0..ix.len() as u32).map(|n| (ix.orig(n), n)).collect();
        for node in 1..ix.len() as u32 {
            let o = ix.orig(node);
            assert_eq!(
                ix.orig(ix.idom(node)),
                want.idom[o as usize],
                "{what}: idom of object {o}"
            );
            assert_eq!(node_of[&o], node);
        }
    }

    #[test]
    fn derived_idom_matches_the_oracle() {
        for seed in 0..40 {
            assert_idom_matches_oracle(&random_snapshot(seed, 200), &format!("seed {seed}"));
        }
        let raw = std::fs::read(FIXTURE).expect("fixture");
        let snap = crate::heap::format::decode_full(&raw).expect("decode");
        assert_idom_matches_oracle(&snap, "fixture");
    }

    #[test]
    fn children_are_contiguous_and_sorted() {
        for seed in 0..30 {
            let snap = random_snapshot(seed, 150);
            let (_dir, a) = run(&snap);
            let ix = &a.index;
            let n = ix.len() as u32;
            let mut next = 1;
            for node in 0..n {
                let kids = ix.children(node);
                assert_eq!(
                    kids.start, next,
                    "seed {seed}: node {node} starts where the last ended"
                );
                next = kids.end;
                let mut last = u64::MAX;
                for c in kids {
                    assert_eq!(ix.idom(c), node);
                    assert!(ix.retained(c) <= last, "sorted descending");
                    last = ix.retained(c);
                }
            }
            assert_eq!(next, n, "every non-root node is somebody's child");
            assert_eq!(ix.retained(0), a.summary.reachable_bytes);
        }
    }

    #[test]
    fn analysis_is_deterministic() {
        let bytes = std::fs::read(FIXTURE).unwrap();
        let run = || {
            let (dir, file) = heap_file(&bytes);
            let a = analyze(&input(&file), &no_cancel(), &mut |_| {}).unwrap();
            (dir, a)
        };
        let (_d1, a) = run();
        let (_d2, b) = run();
        assert_eq!(a.index.len(), b.index.len());
        for node in 0..a.index.len() as u32 {
            let same = a.index.orig(node) == b.index.orig(node)
                && a.index.retained(node) == b.index.retained(node)
                && a.index.idom(node) == b.index.idom(node)
                && a.index.retainer(node) == b.index.retainer(node)
                && a.index.children(node) == b.index.children(node);
            assert!(same, "node {node}");
        }
    }

    #[test]
    fn retainers_are_shortest() {
        for seed in 0..200 {
            let snap = random_snapshot(seed, 100);
            let g = graph_of(&snap);
            let weak = weak_classes(&g.classes);
            let adj = adjacency(&g);
            let got = shortest_retainers(&g, &weak);
            // Reference distances, by a plain queue.
            let mut dist = vec![u32::MAX; adj.len()];
            dist[1] = 0;
            let mut q = std::collections::VecDeque::from([1u32]);
            while let Some(o) = q.pop_front() {
                if is_weak(&weak, g.class_of[o as usize]) {
                    continue;
                }
                for &t in &adj[o as usize] {
                    if dist[t as usize] == u32::MAX {
                        dist[t as usize] = dist[o as usize] + 1;
                        q.push_back(t);
                    }
                }
            }
            assert_eq!(got[0], 0);
            assert_eq!(got[1], 0);
            for o in 2..adj.len() {
                if dist[o] == u32::MAX {
                    assert_eq!(got[o], 0, "seed {seed}: unreachable {o}");
                } else {
                    let r = got[o] as usize;
                    assert_eq!(dist[r] + 1, dist[o], "seed {seed}: {o} via {r}");
                    assert!(adj[r].contains(&(o as u32)));
                    assert!(!is_weak(&weak, g.class_of[r]));
                }
            }
        }
    }

    #[test]
    fn deep_chain_and_wide_list() {
        const N: u32 = 1_000_000;
        // A linked list a million long.
        let mut b = SnapshotBuilder::new();
        let node = b.class("Node", "package:app/a.dart", &["next"]);
        let first = b.object(node, 8, Value::None, &[]);
        let mut prev = first;
        for _ in 1..N {
            let o = b.object(node, 8, Value::None, &[]);
            b.set_refs(prev, &[o]);
            prev = o;
        }
        b.set_refs(3, &[first]);
        let (_dir, a) = run(&b.finish());
        let ix = &a.index;
        assert_eq!(ix.len() as u32, 5 + N);
        let last = ix.len() as u32 - 1;
        assert_eq!(ix.retained(last), 8);
        assert_eq!(ix.idom(last), last - 1);
        assert_eq!(ix.retained(ix.idom(ix.idom(last))), 24);
        assert_eq!(ix.path(last).len(), fdt_store::heap::MAX_PATH_HOPS);
        drop(a);

        // One list of a million elements.
        let mut b = SnapshotBuilder::new();
        let leaf = b.class("Leaf", "package:app/a.dart", &[]);
        let list = b.class("_List", "dart:core", &[]);
        let elems: Vec<u32> = (0..N)
            .map(|_| b.object(leaf, 4, Value::None, &[]))
            .collect();
        let l = b.object(list, 16, Value::Length(u64::from(N)), &elems);
        b.set_refs(3, &[l]);
        let (_dir, a) = run(&b.finish());
        let ix = &a.index;
        let ln = (0..ix.len() as u32).find(|&n| ix.orig(n) == l).unwrap();
        assert_eq!(ix.children(ln).len() as u32, N);
        assert_eq!(ix.retained(ln), 16 + 4 * u64::from(N));
    }

    #[test]
    fn unreachable_objects_are_counted_not_indexed() {
        let mut b = SnapshotBuilder::new();
        let node = b.class("Node", "package:app/a.dart", &["a"]);
        let live = b.object(node, 10, Value::None, &[]);
        let (d1, d2) = (
            b.object(node, 30, Value::None, &[]),
            b.object(node, 5, Value::None, &[]),
        );
        b.set_refs(d1, &[d2]);
        b.set_refs(3, &[live]);
        let snap = b.finish();
        let (_dir, a) = run(&snap);
        assert_eq!(a.summary.objects, 8);
        assert_eq!(a.summary.reachable, 6);
        assert_eq!(a.index.len(), 6);
        assert_eq!(a.summary.unreachable_bytes, 35);
        assert_eq!(a.summary.reachable_bytes, 10);
        let c = &a.summary.classes[node as usize];
        assert_eq!((&*c.name, c.count, c.shallow), ("Node", 1, 10));
        assert_eq!(a.summary.classes[0].count, 0);
        assert_eq!(a.summary.classes.len(), snap.classes.len() + 1);
    }

    #[test]
    fn synthetic_nodes_are_named() {
        let (_dir, a) = run(&SnapshotBuilder::new().finish());
        let named: Vec<&str> = (0..a.index.len() as u32)
            .filter_map(|n| a.index.synthetic_name(n))
            .collect();
        assert_eq!(named, ["class table", "api state"]);
    }

    #[test]
    fn stages_in_order_and_cancel_at_each() {
        let snap = random_snapshot(7, 50);
        let (_dir, file) = heap_file(&encode_full(&snap));
        let mut seen = Vec::new();
        analyze(&input(&file), &no_cancel(), &mut |s| seen.push(s)).unwrap();
        let tag = |s: &HeapStage| match s {
            HeapStage::Parsing { .. } => 0,
            HeapStage::Reachability => 1,
            HeapStage::Dominators => 2,
            HeapStage::Ready => 3,
            _ => 9,
        };
        let tags: Vec<i32> = seen.iter().map(tag).collect();
        assert!(tags.is_sorted(), "{seen:?}");
        for want in 0..4 {
            assert!(tags.contains(&want), "stage {want} in {seen:?}");
        }
        assert_eq!(seen.last(), Some(&HeapStage::Ready));

        for stop in 0..3 {
            let cancel = AtomicBool::new(false);
            let got = analyze(&input(&file), &cancel, &mut |s| {
                if tag(&s) == stop {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
            assert!(matches!(got, Err(HeapError::Cancelled)), "stop {stop}");
            assert!(file.path().exists(), "cancelling keeps the file");
        }
    }
}
