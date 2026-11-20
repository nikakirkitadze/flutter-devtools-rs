//! CPU aggregation over the sample store: top-down and bottom-up call trees, the busiest
//! functions and time-ordered flame-chart spans.

use fdt_store::Snapshot;
use fdt_store::cpu::{CpuSnapshot, DEFAULT_PERIOD_US, FunctionId, NodeId};
use fdt_store::time::{SessionTime, TimeRange};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// How many samples are visited between two looks at the cancel flag.
const CANCEL_STRIDE: usize = 4096;

/// One node of a [`CallTree`]. Depth 0 is a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallNode {
    pub function: FunctionId,
    pub self_count: u64,
    pub total_count: u64,
    pub depth: u16,
    /// Indices into [`CallTree::nodes`], sorted by `total_count` descending.
    pub children: Vec<u32>,
}

/// A call tree. In a top-down tree the children are callees and `self_count` is the samples
/// that ended in the node. In a bottom-up tree the roots are the functions with self samples and
/// the children are callers: `total_count` is the samples whose path (leaf to root) goes through
/// the node, `self_count` equals `total_count` at a root and is 0 below it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CallTree {
    pub nodes: Vec<CallNode>,
    /// Indices of the roots, sorted by `total_count` descending.
    pub roots: Vec<u32>,
    /// The samples counted.
    pub samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionCost {
    pub function: FunctionId,
    pub self_count: u64,
    /// Samples whose stack holds the function, each sample counted once.
    pub total_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlameSpan {
    /// 0 for the outermost frame.
    pub depth: u16,
    pub start: SessionTime,
    pub end: SessionTime,
    pub function: FunctionId,
}

/// Counts the samples of `range` per trie node. `None` when cancelled.
fn leaf_counts(
    cpu: &CpuSnapshot,
    range: Option<TimeRange>,
    cancel: &AtomicBool,
) -> Option<(Vec<u64>, u64)> {
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let samples = cpu.samples();
    let wanted = match range {
        Some(r) => cpu.range(r),
        None => 0..samples.len(),
    };
    let mut counts = vec![0u64; cpu.node_count()];
    let mut total = 0u64;
    let mut base = 0usize;
    let mut since_check = 0usize;
    for chunk in samples.chunks() {
        let (lo, hi) = (base, base + chunk.len());
        base = hi;
        if hi <= wanted.start {
            continue;
        }
        if lo >= wanted.end {
            break;
        }
        let from = wanted.start.max(lo) - lo;
        let to = wanted.end.min(hi) - lo;
        for s in &chunk[from..to] {
            counts[s.leaf.0 as usize] += 1;
            total += 1;
        }
        since_check += to - from;
        if since_check >= CANCEL_STRIDE {
            since_check = 0;
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
        }
    }
    // Samples with an empty stack carry no function.
    total -= counts[0];
    counts[0] = 0;
    Some((counts, total))
}

fn sort_children(nodes: &mut [CallNode]) {
    let totals: Vec<u64> = nodes.iter().map(|n| n.total_count).collect();
    for n in nodes.iter_mut() {
        n.children
            .sort_by(|&a, &b| totals[b as usize].cmp(&totals[a as usize]).then(a.cmp(&b)));
    }
}

/// The trie restricted to the samples of `range`: it is already the top-down tree. `None` when
/// `cancel` was set. O(samples + nodes).
pub fn top_down(
    cpu: &CpuSnapshot,
    range: Option<TimeRange>,
    cancel: &AtomicBool,
) -> Option<CallTree> {
    let (selfs, samples) = leaf_counts(cpu, range, cancel)?;
    let n = selfs.len();
    // A parent is always interned before its children, so ids grow away from the root.
    let mut totals = selfs.clone();
    for i in (1..n).rev() {
        let parent = cpu.node(NodeId(i as u32)).parent.0 as usize;
        totals[parent] += totals[i];
    }
    let mut out_of = vec![u32::MAX; n];
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut roots = Vec::new();
    for i in 1..n {
        if totals[i] == 0 {
            continue;
        }
        let node = cpu.node(NodeId(i as u32));
        let idx = nodes.len() as u32;
        out_of[i] = idx;
        nodes.push(CallNode {
            function: node.function,
            self_count: selfs[i],
            total_count: totals[i],
            depth: node.depth - 1,
            children: Vec::new(),
        });
        if node.parent.0 == 0 {
            roots.push(idx);
        } else {
            let p = out_of[node.parent.0 as usize] as usize;
            nodes[p].children.push(idx);
        }
    }
    sort_children(&mut nodes);
    roots.sort_by(|&a, &b| {
        nodes[b as usize]
            .total_count
            .cmp(&nodes[a as usize].total_count)
            .then(a.cmp(&b))
    });
    Some(CallTree {
        nodes,
        roots,
        samples,
    })
}

/// The inverted tree: each root is a function with self samples and its children are its callers.
/// Every leaf-to-root path is inserted with its sample count, so a recursive function appears
/// once per level. `None` when `cancel` was set.
pub fn bottom_up(
    cpu: &CpuSnapshot,
    range: Option<TimeRange>,
    cancel: &AtomicBool,
) -> Option<CallTree> {
    let (selfs, samples) = leaf_counts(cpu, range, cancel)?;
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut roots: Vec<u32> = Vec::new();
    let mut index: HashMap<(u32, FunctionId), u32> = HashMap::new();
    const TOP: u32 = u32::MAX;
    for (leaf, &count) in selfs.iter().enumerate().skip(1) {
        if count == 0 {
            continue;
        }
        if leaf % 256 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let mut parent = TOP;
        for (depth, f) in cpu.stack(NodeId(leaf as u32)).enumerate() {
            let at = *index.entry((parent, f)).or_insert_with(|| {
                let idx = nodes.len() as u32;
                nodes.push(CallNode {
                    function: f,
                    self_count: 0,
                    total_count: 0,
                    depth: depth as u16,
                    children: Vec::new(),
                });
                if parent == TOP {
                    roots.push(idx);
                } else {
                    nodes[parent as usize].children.push(idx);
                }
                idx
            });
            let node = &mut nodes[at as usize];
            node.total_count += count;
            if depth == 0 {
                node.self_count += count;
            }
            parent = at;
        }
    }
    sort_children(&mut nodes);
    roots.sort_by(|&a, &b| {
        nodes[b as usize]
            .total_count
            .cmp(&nodes[a as usize].total_count)
            .then(a.cmp(&b))
    });
    Some(CallTree {
        nodes,
        roots,
        samples,
    })
}

/// The `limit` functions with the most self samples (ties: more total, then lower id).
pub fn busiest(cpu: &CpuSnapshot, range: Option<TimeRange>, limit: usize) -> Vec<FunctionCost> {
    let never = AtomicBool::new(false);
    let Some((selfs, _)) = leaf_counts(cpu, range, &never) else {
        return Vec::new();
    };
    let mut cost: HashMap<FunctionId, (u64, u64)> = HashMap::new();
    // The leaf that last counted a function, so a recursive stack counts it once.
    let mut seen: HashMap<FunctionId, usize> = HashMap::new();
    for (leaf, &count) in selfs.iter().enumerate().skip(1) {
        if count == 0 {
            continue;
        }
        for (depth, f) in cpu.stack(NodeId(leaf as u32)).enumerate() {
            let e = cost.entry(f).or_default();
            if depth == 0 {
                e.0 += count;
            }
            if seen.insert(f, leaf) != Some(leaf) {
                e.1 += count;
            }
        }
    }
    let mut out: Vec<FunctionCost> = cost
        .into_iter()
        .map(|(function, (self_count, total_count))| FunctionCost {
            function,
            self_count,
            total_count,
        })
        .collect();
    out.sort_by(|a, b| {
        b.self_count
            .cmp(&a.self_count)
            .then(b.total_count.cmp(&a.total_count))
            .then(a.function.0.cmp(&b.function.0))
    });
    out.truncate(limit);
    out
}

/// The sample period, in microseconds, that applies to `range`: that of the one run whose span
/// covers it, else of the latest run, else the default.
pub fn period_for(snap: &Snapshot, range: TimeRange) -> u32 {
    let mut covering = snap
        .runs
        .iter()
        .filter(|r| r.started <= range.start && r.ended.is_none_or(|e| e >= range.end));
    let run = match (covering.next(), covering.next()) {
        (Some(r), None) => Some(r),
        _ => snap.runs.last(),
    };
    run.map_or(DEFAULT_PERIOD_US, |r| snap.cpu.period_us(r.id))
}

/// Flame-chart spans over `range`, using [`period_for`] for the sample period.
pub fn flame_spans(snap: &Snapshot, range: TimeRange, max_spans: usize) -> (Vec<FlameSpan>, bool) {
    flame_spans_with_period(&snap.cpu, range, period_for(snap, range), max_spans)
}

struct Open {
    node: NodeId,
    start: u64,
    last: u64,
}

/// Walks the samples in time order with one open span per depth. A sample extends the span at a
/// depth when it holds the same trie node (same function, same frames above) and follows within
/// 2 periods. A span ends at the next sample or at its last sample plus one period, whichever is
/// first. Output is sorted by start time, then depth, and stops at `max_spans`; the flag says
/// whether spans were left out.
pub fn flame_spans_with_period(
    cpu: &CpuSnapshot,
    range: TimeRange,
    period_us: u32,
    max_spans: usize,
) -> (Vec<FlameSpan>, bool) {
    let period = period_us as u64 * 1000;
    let mut out: Vec<FlameSpan> = Vec::new();
    let mut open: Vec<Option<Open>> = Vec::new();
    let mut path: Vec<NodeId> = Vec::new();
    let mut truncated = false;

    // Closes the span at `depth`; false once the cap is hit.
    let close = |o: Open,
                 depth: usize,
                 at: Option<u64>,
                 out: &mut Vec<FlameSpan>,
                 truncated: &mut bool|
     -> bool {
        if out.len() >= max_spans {
            *truncated = true;
            return false;
        }
        let natural = o.last + period;
        out.push(FlameSpan {
            depth: depth as u16,
            start: SessionTime(o.start),
            end: SessionTime(at.map_or(natural, |t| natural.min(t))),
            function: cpu.node(o.node).function,
        });
        true
    };

    'walk: for i in cpu.range(range) {
        let s = *cpu.samples().get(i).expect("within samples");
        let t = s.time.0;
        path.clear();
        let mut at = s.leaf;
        while at.0 != 0 {
            path.push(at);
            at = cpu.node(at).parent;
        }
        path.reverse();
        if open.len() < path.len() {
            open.resize_with(path.len(), || None);
        }
        let mut broken = false;
        for (d, slot) in open.iter_mut().enumerate() {
            let want = path.get(d).copied();
            let extend = !broken
                && matches!((&*slot, want), (Some(o), Some(n))
                    if o.node == n && t - o.last <= 2 * period);
            if extend {
                slot.as_mut().expect("checked").last = t;
                continue;
            }
            broken = true;
            if let Some(o) = slot.take()
                && !close(o, d, Some(t), &mut out, &mut truncated)
            {
                break 'walk;
            }
            *slot = want.map(|node| Open {
                node,
                start: t,
                last: t,
            });
        }
    }
    if !truncated {
        'flush: for (d, slot) in open.iter_mut().enumerate() {
            if let Some(o) = slot.take()
                && !close(o, d, None, &mut out, &mut truncated)
            {
                break 'flush;
            }
        }
    }
    out.sort_by_key(|s| (s.start, s.depth));
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{MS, writer};
    use fdt_store::cpu::CpuSample;
    use fdt_store::time::RunId;
    use std::sync::Arc;

    fn smp(t_ms: u64, leaf: NodeId) -> CpuSample {
        CpuSample {
            time: SessionTime(t_ms * MS),
            leaf,
        }
    }

    fn rng(a_ms: u64, b_ms: u64) -> TimeRange {
        TimeRange::new(SessionTime(a_ms * MS), SessionTime(b_ms * MS))
    }

    fn name(snap: &Snapshot, f: FunctionId) -> String {
        snap.cpu.function(f).name.to_string()
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// 3 roots (A, E, R) over two time ranges: 0..=9 ms is all A, 10..=16 ms is E and R.
    ///   A -> B x5, A -> B -> C x3, A -> D x2, E x4, R -> R x2, R -> R -> R x1.
    fn fixture() -> Arc<Snapshot> {
        let (mut w, run) = writer();
        let mut f = |n: &str| w.intern_function(n, Some("lib"));
        let (a, b, c, d, e, r) = (f("A"), f("B"), f("C"), f("D"), f("E"), f("R"));
        let ab = w.intern_stack(&[a, b]);
        let abc = w.intern_stack(&[a, b, c]);
        let ad = w.intern_stack(&[a, d]);
        let en = w.intern_stack(&[e]);
        let rr = w.intern_stack(&[r, r]);
        let rrr = w.intern_stack(&[r, r, r]);
        let mut t = 0;
        let mut batch = Vec::new();
        for (leaf, n) in [(ab, 5), (abc, 3), (ad, 2), (en, 4), (rr, 2), (rrr, 1)] {
            for _ in 0..n {
                batch.push(smp(t, leaf));
                t += 1;
            }
        }
        w.push_cpu_samples(run, batch);
        w.publish()
    }

    fn by_name<'a>(snap: &Snapshot, t: &'a CallTree, ids: &[u32], n: &str) -> &'a CallNode {
        let i = ids
            .iter()
            .find(|&&i| name(snap, t.nodes[i as usize].function) == n)
            .unwrap_or_else(|| panic!("no {n}"));
        &t.nodes[*i as usize]
    }

    #[test]
    fn top_down_counts_self_and_total() {
        let s = fixture();
        let t = top_down(&s.cpu, None, &never()).unwrap();
        assert_eq!(t.samples, 17);
        let names: Vec<_> = t
            .roots
            .iter()
            .map(|&i| name(&s, t.nodes[i as usize].function))
            .collect();
        assert_eq!(names, ["A", "E", "R"]);
        let a = by_name(&s, &t, &t.roots, "A");
        assert_eq!((a.self_count, a.total_count, a.depth), (0, 10, 0));
        let bn = by_name(&s, &t, &a.children, "B");
        assert_eq!((bn.self_count, bn.total_count, bn.depth), (5, 8, 1));
        let d = by_name(&s, &t, &a.children, "D");
        assert_eq!((d.self_count, d.total_count), (2, 2));
        // Children are sorted by total, descending.
        assert_eq!(t.nodes[a.children[0] as usize].total_count, 8);
        let c = by_name(&s, &t, &bn.children, "C");
        assert_eq!((c.self_count, c.total_count, c.depth), (3, 3, 2));
        let e = by_name(&s, &t, &t.roots, "E");
        assert_eq!((e.self_count, e.total_count), (4, 4));
        // The totals of the roots add up to the samples.
        let sum: u64 = t
            .roots
            .iter()
            .map(|&i| t.nodes[i as usize].total_count)
            .sum();
        assert_eq!(sum, 17);
    }

    #[test]
    fn bottom_up_lists_callers() {
        let s = fixture();
        let t = bottom_up(&s.cpu, None, &never()).unwrap();
        assert_eq!(t.samples, 17);
        let b = by_name(&s, &t, &t.roots, "B");
        assert_eq!((b.self_count, b.total_count), (5, 5));
        assert_eq!(b.children.len(), 1);
        let a = &t.nodes[b.children[0] as usize];
        assert_eq!(
            (name(&s, a.function), a.total_count, a.self_count, a.depth),
            ("A".into(), 5, 0, 1)
        );
        // C is called by B, which is called by A.
        let c = by_name(&s, &t, &t.roots, "C");
        assert_eq!(c.total_count, 3);
        let cb = &t.nodes[c.children[0] as usize];
        assert_eq!(name(&s, cb.function), "B");
        let cba = &t.nodes[cb.children[0] as usize];
        assert_eq!((name(&s, cba.function), cba.total_count), ("A".into(), 3));
        // Roots are the functions with self samples: B, C, D, E, R.
        assert_eq!(t.roots.len(), 5);
        assert!(
            t.roots.windows(2).all(|w| {
                t.nodes[w[0] as usize].total_count >= t.nodes[w[1] as usize].total_count
            })
        );
    }

    #[test]
    fn recursion_counts_once_per_sample() {
        let s = fixture();
        // R has 3 self samples; the [R,R] stacks hold it twice but count once per sample.
        let costs = busiest(&s.cpu, None, 10);
        let r = costs.iter().find(|c| name(&s, c.function) == "R").unwrap();
        assert_eq!((r.self_count, r.total_count), (3, 3));
        let a = costs.iter().find(|c| name(&s, c.function) == "A").unwrap();
        assert_eq!((a.self_count, a.total_count), (0, 10));
        let b = costs.iter().find(|c| name(&s, c.function) == "B").unwrap();
        assert_eq!((b.self_count, b.total_count), (5, 8));
        // Ordered by self count.
        let selfs: Vec<_> = costs.iter().map(|c| c.self_count).collect();
        assert!(selfs.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(name(&s, costs[0].function), "B");
        assert_eq!(busiest(&s.cpu, None, 2).len(), 2);
        // Bottom-up lists the recursion once per level, each level holding the sample count.
        let t = bottom_up(&s.cpu, None, &never()).unwrap();
        let root = by_name(&s, &t, &t.roots, "R");
        assert_eq!(root.total_count, 3);
        let l1 = &t.nodes[root.children[0] as usize];
        assert_eq!(l1.total_count, 3);
        let l2 = &t.nodes[l1.children[0] as usize];
        assert_eq!((l2.total_count, l2.depth, l2.children.len()), (1, 2, 0));
    }

    #[test]
    fn range_limits_samples() {
        let s = fixture();
        let t = top_down(&s.cpu, Some(rng(10, 16)), &never()).unwrap();
        assert_eq!(t.samples, 7);
        let names: Vec<_> = t
            .roots
            .iter()
            .map(|&i| name(&s, t.nodes[i as usize].function))
            .collect();
        assert_eq!(names, ["E", "R"]);
        let t = top_down(&s.cpu, Some(rng(0, 9)), &never()).unwrap();
        assert_eq!((t.samples, t.roots.len()), (10, 1));
        let t = bottom_up(&s.cpu, Some(rng(5, 7)), &never()).unwrap();
        assert_eq!(t.samples, 3);
        assert_eq!(t.roots.len(), 1);
        let empty = top_down(&s.cpu, Some(rng(100, 200)), &never()).unwrap();
        assert_eq!((empty.samples, empty.nodes.len()), (0, 0));
        assert!(busiest(&s.cpu, Some(rng(100, 200)), 5).is_empty());
        let late = busiest(&s.cpu, Some(rng(10, 16)), 5);
        assert!(late.iter().all(|c| name(&s, c.function) != "A"));
    }

    #[test]
    fn cancel_returns_none() {
        let s = fixture();
        let stop = AtomicBool::new(true);
        assert!(top_down(&s.cpu, None, &stop).is_none());
        assert!(bottom_up(&s.cpu, None, &stop).is_none());
    }

    /// A -> B and A -> C stacks at chosen times (ms).
    fn flame_fixture(times: &[(f64, bool)]) -> Arc<Snapshot> {
        let (mut w, run) = writer();
        let a = w.intern_function("A", Some("lib"));
        let b = w.intern_function("B", Some("lib"));
        let c = w.intern_function("C", Some("lib"));
        let ab = w.intern_stack(&[a, b]);
        let ac = w.intern_stack(&[a, c]);
        let batch = times
            .iter()
            .map(|&(ms, is_b)| CpuSample {
                time: SessionTime((ms * MS as f64) as u64),
                leaf: if is_b { ab } else { ac },
            })
            .collect();
        w.push_cpu_samples(run, batch);
        w.publish()
    }

    fn view(s: &Snapshot, spans: &[FlameSpan]) -> Vec<(u16, String, f64, f64)> {
        spans
            .iter()
            .map(|p| {
                (
                    p.depth,
                    name(s, p.function),
                    p.start.0 as f64 / MS as f64,
                    p.end.0 as f64 / MS as f64,
                )
            })
            .collect()
    }

    #[test]
    fn flame_spans_merge_and_split() {
        let s = flame_fixture(&[
            (0.0, true),
            (1.0, true),
            (2.0, false),
            (3.0, false),
            (10.0, false),
        ]);
        let (spans, cut) = flame_spans(&s, rng(0, 100), 100);
        assert!(!cut);
        assert_eq!(
            view(&s, &spans),
            vec![
                (0, "A".into(), 0.0, 4.0),
                (1, "B".into(), 0.0, 2.0),
                (1, "C".into(), 2.0, 4.0),
                (0, "A".into(), 10.0, 11.0),
                (1, "C".into(), 10.0, 11.0),
            ]
        );
        // A gap of exactly 2 periods merges; a bit more splits.
        let s = flame_fixture(&[(0.0, true), (2.0, true)]);
        let (spans, _) = flame_spans(&s, rng(0, 100), 100);
        assert_eq!(view(&s, &spans)[1], (1, "B".into(), 0.0, 3.0));
        let s = flame_fixture(&[(0.0, true), (2.5, true)]);
        let (spans, _) = flame_spans(&s, rng(0, 100), 100);
        assert_eq!(spans.len(), 4);
        // The range limits the walk.
        let s = flame_fixture(&[
            (0.0, true),
            (1.0, true),
            (2.0, false),
            (3.0, false),
            (10.0, false),
        ]);
        let (spans, _) = flame_spans(&s, rng(9, 100), 100);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn flame_period_comes_from_the_run() {
        let (mut w, run) = writer();
        let a = w.intern_function("A", Some("lib"));
        let n = w.intern_stack(&[a]);
        w.push_cpu_samples(run, vec![smp(0, n), smp(5, n)]);
        w.set_cpu_period(run, 4000);
        let s = w.publish();
        assert_eq!(period_for(&s, rng(0, 10)), 4000);
        // 5 ms apart is within 2 x 4 ms.
        let (spans, _) = flame_spans(&s, rng(0, 10), 10);
        assert_eq!((spans.len(), spans[0].end.0), (1, 9 * MS));
        assert_eq!(period_for(&Snapshot::empty(), rng(0, 1)), 1000);
        let _ = RunId(1);
    }

    #[test]
    fn flame_truncates_at_the_cap() {
        let s = flame_fixture(&[
            (0.0, true),
            (1.0, true),
            (2.0, false),
            (3.0, false),
            (10.0, false),
        ]);
        let (spans, cut) = flame_spans(&s, rng(0, 100), 2);
        assert!(cut);
        assert_eq!(spans.len(), 2);
        let (spans, cut) = flame_spans(&s, rng(0, 100), 5);
        assert!(!cut);
        assert_eq!(spans.len(), 5);
        let (spans, cut) = flame_spans(&s, rng(0, 100), 0);
        assert!(cut && spans.is_empty());
    }

    #[test]
    fn deep_stacks_and_long_names() {
        let (mut w, run) = writer();
        let ids: Vec<_> = (0..128)
            .map(|i| w.intern_function(&format!("{i:04}{}", "x".repeat(10 * 1024)), Some("lib")))
            .collect();
        let leaf = w.intern_stack(&ids);
        w.push_cpu_samples(run, vec![smp(0, leaf), smp(1, leaf)]);
        let s = w.publish();
        let t = top_down(&s.cpu, None, &never()).unwrap();
        assert_eq!((t.nodes.len(), t.samples), (128, 2));
        let deepest = t.nodes.last().unwrap();
        assert_eq!((deepest.depth, deepest.self_count), (127, 2));
        assert_eq!(s.cpu.function(deepest.function).name.len(), 10 * 1024 + 4);
        let t = bottom_up(&s.cpu, None, &never()).unwrap();
        assert_eq!((t.nodes.len(), t.roots.len()), (128, 1));
        assert_eq!(t.nodes.last().unwrap().depth, 127);
        let (spans, cut) = flame_spans(&s, rng(0, 10), 1000);
        assert!(!cut);
        assert_eq!(spans.len(), 128);
        let costs = busiest(&s.cpu, None, 200);
        assert_eq!(costs.len(), 128);
        assert!(costs.iter().all(|c| c.total_count == 2));
    }

    #[cfg(not(debug_assertions))]
    fn p50(mut f: impl FnMut()) -> std::time::Duration {
        let mut times: Vec<_> = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed()
            })
            .collect();
        times.sort();
        times[2]
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn call_tree_budget_450k() {
        #[cfg(not(debug_assertions))]
        {
            let s = fdt_store::synth::stress_session(42);
            let stop = never();
            let td = p50(|| {
                std::hint::black_box(top_down(&s.cpu, None, &stop).unwrap());
            });
            let bu = p50(|| {
                std::hint::black_box(bottom_up(&s.cpu, None, &stop).unwrap());
            });
            let tree = top_down(&s.cpu, None, &stop).unwrap();
            println!(
                "call_tree_budget: top_down p50 {td:?}, bottom_up p50 {bu:?}, nodes {}",
                tree.nodes.len()
            );
            assert_eq!(tree.samples, 450_000);
            assert!(td.as_millis() <= 100, "top_down {td:?}");
            assert!(bu.as_millis() <= 100, "bottom_up {bu:?}");
        }
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn flame_budget() {
        #[cfg(not(debug_assertions))]
        {
            let s = fdt_store::synth::stress_session(42);
            let r = TimeRange::new(SessionTime(100_000_000_000), SessionTime(110_000_000_000));
            let d = p50(|| {
                std::hint::black_box(flame_spans(&s, r, 1_000_000));
            });
            let (spans, cut) = flame_spans(&s, r, 1_000_000);
            println!(
                "flame_budget: 10 s range p50 {d:?}, {} spans, truncated {cut}",
                spans.len()
            );
            assert!(!spans.is_empty());
            assert!(d.as_millis() <= 50, "flame {d:?}");
        }
    }
}
