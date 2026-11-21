//! Rebuild attribution from widget-build timeline slices.

use crate::framework_widgets::is_framework_widget;
use crate::user_code::is_user_library;
use fdt_store::Snapshot;
use fdt_store::cpu::FunctionId;
use fdt_store::time::{RunId, SessionTime, TimeRange};
use fdt_store::timeline::{LaneSnapshot, NameId, SliceCat};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WidgetCount {
    pub name: Arc<str>,
    pub builds: u32,
    pub frames: u32,
    pub max_in_frame: u32,
    pub self_ns: u64,
    pub total_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRebuild {
    pub name: Arc<str>,
    pub widgets: u32,
    pub total_ns: u64,
    pub largest_child: Option<(Arc<str>, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameRebuilds {
    pub total: u32,
    pub roots: Vec<RootRebuild>,
    pub widgets: Vec<WidgetCount>,
}

const BUILD: &str = "BUILD";

struct Node {
    name: NameId,
    start: u64,
    end: u64,
    parent: Option<usize>,
    subtree: u32,
    child_ns: u64,
}

/// The UI-thread lanes of the run of `frame`.
fn ui_lanes(snap: &Snapshot, run: RunId) -> &[LaneSnapshot] {
    snap.timeline
        .tracks()
        .iter()
        .find(|t| t.run == run && t.order == 0)
        .map_or(&[], |t| snap.timeline.lanes(t.id))
}

pub fn frame_rebuilds(snap: &Snapshot, frame_seq: usize) -> Option<FrameRebuilds> {
    frame_rebuilds_with(snap, frame_seq, &user_classes(snap))
}

/// Appends the widget slices below the BUILD at `build_depth` spanning `span`: the slices of each
/// lane further down that sit inside BUILD and inside a slice one lane up. The BUILD's direct
/// children are roots. Children come after their parents.
fn walk_build(lanes: &[LaneSnapshot], build_depth: usize, span: TimeRange, nodes: &mut Vec<Node>) {
    let mut prev: Vec<usize> = Vec::new();
    for lane in lanes.iter().skip(build_depth + 1) {
        let mut cur = Vec::new();
        let mut p = 0;
        for i in lane.range_indices(span) {
            let s = lane.get(i);
            // Strict at the end: a slice that starts where BUILD ends (the next phase) is not in it.
            if s.start < span.start || s.start >= span.end || s.end() > span.end {
                continue;
            }
            // Widget builds are Dart slices; a GC pause inside BUILD is not a rebuild.
            if s.cat != SliceCat::Dart {
                continue;
            }
            let parent = if prev.is_empty() {
                None
            } else {
                // The enclosing slice one lane up; both lists are sorted by start.
                while p < prev.len() && nodes[prev[p]].end < s.end().0 {
                    p += 1;
                }
                let found = prev.get(p).copied().filter(|&n| {
                    nodes[n].start <= s.start.0
                        && s.start.0 < nodes[n].end
                        && nodes[n].end >= s.end().0
                });
                if found.is_none() {
                    continue;
                }
                found
            };
            cur.push(nodes.len());
            nodes.push(Node {
                name: s.name,
                start: s.start.0,
                end: s.end().0,
                parent,
                subtree: 1,
                child_ns: 0,
            });
        }
        if cur.is_empty() {
            break;
        }
        prev = cur;
    }
}

/// [`frame_rebuilds`] with the CPU user-class set from [`user_classes`], for callers that look at
/// many frames of one snapshot.
pub fn frame_rebuilds_with(
    snap: &Snapshot,
    frame_seq: usize,
    user: &HashSet<String>,
) -> Option<FrameRebuilds> {
    let frame = snap.frames.get(frame_seq)?;
    let ui = frame.ui?;
    let lanes = ui_lanes(snap, frame.run);
    let window = TimeRange::new(
        ui.begin,
        SessionTime(ui.begin.0 + u64::from(ui.ui_us) * 1_000),
    );
    // Every BUILD in the window that is not inside another one, at whatever depth it sits: the
    // frame's own BUILD, and those that layout runs for lazily built children (slivers,
    // `LayoutBuilder`). The assembler's `widgets_built` counts the widgets under all of them.
    let mut builds: Vec<(usize, TimeRange)> = Vec::new();
    // Names are interned: once one BUILD is seen, the rest compare by id.
    let mut build_id: Option<NameId> = None;
    let mut is_build = |id: NameId| match build_id {
        Some(b) => id == b,
        None => {
            let yes = snap.timeline.name(id) == BUILD;
            if yes {
                build_id = Some(id);
            }
            yes
        }
    };
    for (d, lane) in lanes.iter().enumerate() {
        for i in lane.range_indices(window) {
            let s = lane.get(i);
            if s.start < window.start || s.end() > window.end || !is_build(s.name) {
                continue;
            }
            // Shallower BUILDs were found first; outermost ones never overlap each other.
            let inside = builds
                .iter()
                .any(|(_, b)| b.start <= s.start && s.end() <= b.end);
            if !inside {
                builds.push((d, TimeRange::new(s.start, s.end())));
            }
        }
    }
    let mut nodes: Vec<Node> = Vec::new();
    for (build_depth, span) in builds {
        walk_build(lanes, build_depth, span, &mut nodes);
    }
    if nodes.is_empty() {
        return None;
    }
    // Children come after their parents, so one reverse pass folds subtrees upward.
    for i in (0..nodes.len()).rev() {
        if let Some(p) = nodes[i].parent {
            let (sub, dur) = (nodes[i].subtree, nodes[i].end - nodes[i].start);
            nodes[p].subtree += sub;
            nodes[p].child_ns += dur;
        }
    }

    let name = |id: NameId| -> Arc<str> { Arc::from(snap.timeline.name(id)) };
    // The child with the largest subtree per parent, in one pass (the last of equals wins).
    let mut largest_of: Vec<Option<usize>> = vec![None; nodes.len()];
    for (c, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent
            && largest_of[p].is_none_or(|b| nodes[b].subtree <= n.subtree)
        {
            largest_of[p] = Some(c);
        }
    }
    // One verdict per distinct name: a frame has far fewer names than nodes.
    let mut verdicts: HashMap<NameId, bool> = HashMap::new();
    let mut is_user = |id: NameId| -> bool {
        *verdicts.entry(id).or_insert_with(|| {
            let name = snap.timeline.name(id);
            user.contains(name) || !is_framework_widget(name)
        })
    };
    let mut child_count = vec![0u32; nodes.len()];
    let mut depth = vec![0u32; nodes.len()];
    let mut root_of: Vec<usize> = (0..nodes.len()).collect();
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            child_count[p] += 1;
            depth[i] = depth[p] + 1;
            root_of[i] = root_of[p];
        }
    }
    // Per root, the user widget below it with the largest subtree; ties go to the shallowest.
    let mut user_best: Vec<Option<usize>> = vec![None; nodes.len()];
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.parent.is_some()) {
        if !is_user(n.name) {
            continue;
        }
        let r = root_of[i];
        let better = user_best[r].is_none_or(|b| {
            (n.subtree, std::cmp::Reverse(depth[i]))
                > (nodes[b].subtree, std::cmp::Reverse(depth[b]))
        });
        if better {
            user_best[r] = Some(i);
        }
    }
    let mut roots: Vec<RootRebuild> = Vec::new();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.parent.is_none()) {
        let largest = user_best[i].or_else(|| {
            // Skip single-child chains down to the first branch.
            let mut at = i;
            while child_count[at] == 1 {
                at = largest_of[at]?;
            }
            largest_of[at]
        });
        roots.push(RootRebuild {
            name: name(n.name),
            widgets: n.subtree,
            total_ns: n.end - n.start,
            largest_child: largest.map(|c| (name(nodes[c].name), nodes[c].subtree)),
        });
    }
    roots.sort_by(|a, b| b.widgets.cmp(&a.widgets).then(b.total_ns.cmp(&a.total_ns)));

    let mut by_name: HashMap<NameId, WidgetCount> = HashMap::new();
    for n in &nodes {
        let w = by_name.entry(n.name).or_insert_with(|| WidgetCount {
            name: name(n.name),
            builds: 0,
            frames: 1,
            max_in_frame: 0,
            self_ns: 0,
            total_ns: 0,
        });
        w.builds += 1;
        w.max_in_frame = w.builds;
        w.total_ns += n.end - n.start;
        w.self_ns += (n.end - n.start).saturating_sub(n.child_ns);
    }
    let mut widgets: Vec<WidgetCount> = by_name.into_values().collect();
    sort_widgets(&mut widgets);
    Some(FrameRebuilds {
        total: nodes.len() as u32,
        roots,
        widgets,
    })
}

/// Class names of the app's own code, from the CPU function table (`Class.method` in a user
/// library). A State class also gives its widget: `_FooState` adds `Foo`.
pub fn user_classes(snap: &Snapshot) -> HashSet<String> {
    let mut set = HashSet::new();
    for i in 0..snap.cpu.function_count() {
        let f = snap.cpu.function(FunctionId(i as u32));
        if !is_user_library(f.library.as_deref()) {
            continue;
        }
        let Some((class, _)) = f.name.split_once('.') else {
            continue;
        };
        if class.is_empty() || class.contains(' ') {
            continue;
        }
        set.insert(class.to_string());
        if let Some(widget) = class.strip_suffix("State").filter(|w| w.len() > 1) {
            set.insert(widget.trim_start_matches('_').to_string());
            set.insert(widget.to_string());
        }
    }
    set
}

fn sort_widgets(v: &mut [WidgetCount]) {
    v.sort_by(|a, b| {
        b.builds
            .cmp(&a.builds)
            .then(b.total_ns.cmp(&a.total_ns))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// `frame_rebuilds` merged over the frames in `range` that built widgets. `None` when `cancel`
/// was set.
pub fn range_rebuilds(
    snap: &Snapshot,
    range: Option<TimeRange>,
    cancel: &AtomicBool,
) -> Option<Vec<WidgetCount>> {
    let seqs = range.map_or(0..snap.frames.len(), |r| snap.frames.range(r));
    let merged = fold_frames(snap, seqs, &user_classes(snap), cancel)?;
    let mut out: Vec<WidgetCount> = merged.into_values().collect();
    sort_widgets(&mut out);
    Some(out)
}

/// How long a frame waits for its phases before it counts as final without them (ring loss,
/// no timeline). Phases normally land within a poll, about 500 ms.
const SETTLE_NS: u64 = 60_000_000_000;

/// Counts over a time range (or the whole session) kept between snapshots, so that a newer
/// snapshot folds in only the frames added since. Frames are only ever appended, and a frame's
/// widget slices land together with its phases, so the frames before `settled` never change.
#[derive(Clone, Debug, Default)]
pub struct RebuildTally {
    merged: HashMap<Arc<str>, WidgetCount>,
    /// The frames before this seq are in `merged` and final.
    settled: usize,
}

/// [`range_rebuilds`], starting from `prev`, an earlier tally of the same scope in the same
/// session: only the frames from `prev`'s end on are walked. Returns the new tally and the
/// widgets to show (the tally plus the frames that may still change). `None` when `cancel` was
/// set.
pub fn tally_rebuilds(
    snap: &Snapshot,
    range: Option<TimeRange>,
    prev: Option<&RebuildTally>,
    cancel: &AtomicBool,
) -> Option<(RebuildTally, Vec<WidgetCount>)> {
    let seqs = range.map_or(0..snap.frames.len(), |r| snap.frames.range(r));
    let from = prev.map_or(seqs.start, |p| p.settled.clamp(seqs.start, seqs.end));
    let latest = snap
        .frames
        .len()
        .checked_sub(1)
        .and_then(|i| snap.frames.get(i))
        .map_or(0, |f| f.start.0);
    let horizon = latest.saturating_sub(SETTLE_NS);
    // The first frame that may still get its phases (and with them its widget slices).
    let settled = (from..seqs.end)
        .find(|&i| {
            snap.frames
                .get(i)
                .is_some_and(|f| f.ui.is_none() && f.start.0 > horizon)
        })
        .unwrap_or(seqs.end);
    let user = user_classes(snap);
    let mut kept = prev.map_or_else(HashMap::new, |p| p.merged.clone());
    for w in fold_frames(snap, from..settled, &user, cancel)?.into_values() {
        merge(&mut kept, w);
    }
    let mut shown = kept.clone();
    for w in fold_frames(snap, settled..seqs.end, &user, cancel)?.into_values() {
        merge(&mut shown, w);
    }
    let mut out: Vec<WidgetCount> = shown.into_values().collect();
    sort_widgets(&mut out);
    Some((
        RebuildTally {
            merged: kept,
            settled,
        },
        out,
    ))
}

/// The widgets of the frames `seqs` that built widgets, merged by name. `None` when `cancel` was
/// set.
fn fold_frames(
    snap: &Snapshot,
    seqs: std::ops::Range<usize>,
    user: &HashSet<String>,
    cancel: &AtomicBool,
) -> Option<HashMap<Arc<str>, WidgetCount>> {
    let merged = seqs
        .into_par_iter()
        .filter(|&i| {
            snap.frames
                .get(i)
                .and_then(|f| f.ui)
                .is_some_and(|u| u.widgets_built.is_some_and(|n| n > 0))
        })
        .fold(HashMap::<Arc<str>, WidgetCount>::new, |mut acc, i| {
            if !cancel.load(Ordering::Relaxed)
                && let Some(fr) = frame_rebuilds_with(snap, i, user)
            {
                for w in fr.widgets {
                    merge(&mut acc, w);
                }
            }
            acc
        })
        .reduce(HashMap::new, |mut a, b| {
            for w in b.into_values() {
                merge(&mut a, w);
            }
            a
        });
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(merged)
}

fn merge(acc: &mut HashMap<Arc<str>, WidgetCount>, w: WidgetCount) {
    match acc.get_mut(&w.name) {
        Some(t) => {
            t.builds += w.builds;
            t.frames += w.frames;
            t.max_in_frame = t.max_in_frame.max(w.max_in_frame);
            t.self_ns += w.self_ns;
            t.total_ns += w.total_ns;
        }
        None => {
            acc.insert(w.name.clone(), w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{MS, frame, writer};
    use fdt_store::SessionWriter;
    use fdt_store::frames::UiPhases;
    use fdt_store::time::{RunId, SessionTime};
    use fdt_store::timeline::{Slice, SliceCat, TrackKind};
    use std::sync::atomic::Ordering;

    /// Frame `number` at `start_ms` with a UI thread whose BUILD holds `CartScreen > {CartBadge,
    /// ProductGrid > tiles x ProductTile > Card > Text}`.
    fn cart_frame(w: &mut SessionWriter, run: RunId, number: u64, start_ms: u64, tiles: u32) {
        frame(w, run, number, start_ms, 30_000);
        let begin = start_ms * MS;
        w.set_frame_ui(
            run,
            number,
            UiPhases {
                begin: SessionTime(begin),
                ui_us: 20_000,
                build_us: 15_000,
                layout_us: 2000,
                paint_us: 2000,
                composite_us: 500,
                finalize_us: 500,
                widgets_built: Some(2 + 3 * tiles),
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let put = |w: &mut SessionWriter, depth: u16, name: &str, at_us: u64, dur_us: u64| {
            let s = Slice {
                start: SessionTime(begin + at_us * 1000),
                dur_ns: dur_us * 1000,
                name: w.intern_name(name),
                cat: SliceCat::Dart,
            };
            assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
        };
        put(w, 0, "VsyncProcessCallback", 0, 20_000);
        put(w, 1, "Animator::BeginFrame", 0, 20_000);
        put(w, 2, "BUILD", 0, 15_000);
        put(w, 3, "CartScreen", 0, 14_000);
        put(w, 4, "CartBadge", 0, 500);
        put(w, 4, "ProductGrid", 1000, 12_000);
        for i in 0..u64::from(tiles) {
            let at = 1000 + i * 100;
            put(w, 5, "ProductTile", at, 90);
            put(w, 6, "Card", at, 60);
            put(w, 7, "Text", at, 30);
        }
    }

    #[test]
    fn frame_rebuilds_counts_roots_and_children() {
        let (mut w, run) = writer();
        w.intern_function("ProductGrid.build", Some("package:app/cart.dart"));
        cart_frame(&mut w, run, 1, 10, 64);
        let s = w.publish();
        let r = frame_rebuilds(&s, 0).expect("rebuilds");
        assert_eq!(r.total, 195);
        assert_eq!(r.roots.len(), 1);
        let root = &r.roots[0];
        assert_eq!(&*root.name, "CartScreen");
        assert_eq!(root.widgets, 195);
        assert_eq!(root.total_ns, 14_000_000);
        let (name, n) = root.largest_child.clone().unwrap();
        assert_eq!((&*name, n), ("ProductGrid", 193));
        let tile = r
            .widgets
            .iter()
            .find(|w| &*w.name == "ProductTile")
            .unwrap();
        assert_eq!((tile.builds, tile.frames, tile.max_in_frame), (64, 1, 64));
        assert_eq!(tile.total_ns, 64 * 90_000);
        // Self time excludes the Card inside each tile.
        assert_eq!(tile.self_ns, 64 * 30_000);
        // Sorted by builds, then total time.
        assert!(
            r.widgets
                .windows(2)
                .all(|p| (p[0].builds, p[0].total_ns) >= (p[1].builds, p[1].total_ns))
        );
    }

    /// A frame whose BUILD holds `tree` (preorder `(depth, name)`, depth 3 is the root); every
    /// slice is 100 us per node of its subtree.
    fn tree_frame(w: &mut SessionWriter, run: RunId, tree: &[(u16, &str)]) {
        frame(w, run, 1, 10, 30_000);
        let begin = 10 * MS;
        w.set_frame_ui(
            run,
            1,
            UiPhases {
                begin: SessionTime(begin),
                ui_us: 20_000,
                build_us: 15_000,
                layout_us: 2000,
                paint_us: 2000,
                composite_us: 500,
                finalize_us: 500,
                widgets_built: Some(tree.len() as u32),
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let total = tree.len() as u64 * 100;
        let mut all: Vec<(u16, &str, u64, u64)> = vec![
            (0, "VsyncProcessCallback", 0, total + 1000),
            (1, "Animator::BeginFrame", 0, total + 1000),
            (2, "BUILD", 0, total + 500),
        ];
        for (i, &(depth, name)) in tree.iter().enumerate() {
            let end = tree[i + 1..]
                .iter()
                .position(|&(d, _)| d <= depth)
                .map_or(tree.len(), |k| i + 1 + k);
            all.push((depth, name, i as u64 * 100, (end - i) as u64 * 100 - 10));
        }
        put_slices(w, ui, begin, all);
    }

    fn put_slices(
        w: &mut SessionWriter,
        ui: fdt_store::timeline::TrackId,
        begin: u64,
        all: Vec<(u16, &str, u64, u64)>,
    ) {
        for (depth, name, at_us, dur_us) in all {
            let s = Slice {
                start: SessionTime(begin + at_us * 1000),
                dur_ns: dur_us * 1000,
                name: w.intern_name(name),
                cat: SliceCat::Dart,
            };
            assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
        }
    }

    /// `CartScreen > Scaffold > {Padding > <grid> > 3 x SizedBox, Center}`.
    fn chain_frame(w: &mut SessionWriter, run: RunId, grid: &str) {
        tree_frame(
            w,
            run,
            &[
                (3, "CartScreen"),
                (4, "Scaffold"),
                (5, "Padding"),
                (6, grid),
                (7, "SizedBox"),
                (7, "SizedBox"),
                (7, "SizedBox"),
                (5, "Center"),
            ],
        );
    }

    fn child(s: &Snapshot) -> Option<(String, u32)> {
        let r = frame_rebuilds(s, 0)?;
        r.roots[0]
            .largest_child
            .clone()
            .map(|(n, m)| (n.to_string(), m))
    }

    #[test]
    fn largest_child_skips_framework_chains() {
        let (mut w, run) = writer();
        chain_frame(&mut w, run, "ProductGrid");
        assert_eq!(child(&w.publish()), Some(("ProductGrid".into(), 4)));
    }

    #[test]
    fn largest_child_falls_back_to_first_branch() {
        // No user widget below CartScreen: Scaffold branches into Padding and Center.
        let (mut w, run) = writer();
        chain_frame(&mut w, run, "Wrap");
        assert_eq!(child(&w.publish()), Some(("Padding".into(), 5)));
    }

    #[test]
    fn a_cpu_identified_class_overrides_the_framework_list() {
        let (mut w, run) = writer();
        w.intern_function("Wrap.build", Some("package:app/wrap.dart"));
        chain_frame(&mut w, run, "Wrap");
        assert_eq!(child(&w.publish()), Some(("Wrap".into(), 4)));
    }

    #[test]
    fn the_live_jank_lab_tree_names_product_grid() {
        let (mut w, run) = writer();
        let mut t = vec![
            (3, "CartScreen"),
            (4, "Scaffold"),
            (5, "Column"),
            (6, "CartBadge"),
            (7, "Padding"),
            (8, "Badge"),
            (9, "Text"),
            (6, "Expanded"),
            (7, "ProductGrid"),
            (8, "SingleChildScrollView"),
            (9, "Wrap"),
        ];
        for _ in 0..3 {
            t.extend([(10, "SizedBox"), (11, "ProductTile"), (12, "Card")]);
        }
        t.push((5, "AppBar"));
        tree_frame(&mut w, run, &t);
        assert_eq!(child(&w.publish()), Some(("ProductGrid".into(), 12)));
    }

    #[test]
    fn slice_starting_at_build_end_is_not_a_rebuild() {
        let (mut w, run) = writer();
        frame(&mut w, run, 1, 10, 30_000);
        let begin = 10 * MS;
        w.set_frame_ui(
            run,
            1,
            UiPhases {
                begin: SessionTime(begin),
                ui_us: 20_000,
                build_us: 15_000,
                layout_us: 2000,
                paint_us: 2000,
                composite_us: 500,
                finalize_us: 500,
                widgets_built: None,
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        // BUILD has no widget children; LAYOUT (a zero-length one, one lane down) starts at the
        // exact microsecond BUILD ends.
        put_slices(
            &mut w,
            ui,
            begin,
            vec![
                (0, "VsyncProcessCallback", 0, 20_000),
                (1, "Animator::BeginFrame", 0, 20_000),
                (2, "BUILD", 0, 15_000),
                (2, "LAYOUT (root)", 15_000, 2_000),
                (3, "LAYOUT", 15_000, 0),
            ],
        );
        let s = w.publish();
        assert!(frame_rebuilds(&s, 0).is_none());
        let e = crate::explain::explain(&s, 0);
        assert!(
            !matches!(e.cause, crate::explain::Cause::Rebuilt { .. }),
            "{:?}",
            e.cause
        );
    }

    #[test]
    fn a_gc_slice_inside_build_is_not_a_rebuild() {
        let (mut w, run) = writer();
        cart_frame(&mut w, run, 1, 10, 0);
        // A GC pause (not a Dart slice) one lane below BUILD, beside CartScreen.
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        let s = Slice {
            start: SessionTime(10 * MS + 14_100_000),
            dur_ns: 500_000,
            name: w.intern_name("CollectNewGeneration"),
            cat: SliceCat::Gc,
        };
        assert!(w.push_slice(ui, 3, s, false));
        let snap = w.publish();
        let r = frame_rebuilds(&snap, 0).unwrap();
        assert!(r.roots.iter().all(|r| &*r.name != "CollectNewGeneration"));
    }

    /// A scrolling list frame (reviewer B's probe): BUILD with 2 widgets, then LAYOUT holding 3
    /// lazily built children, each in its own BUILD. Every BUILD counts, as the assembler's
    /// `widgets_built` does.
    #[test]
    fn builds_inside_layout_are_counted() {
        let (mut w, run) = writer();
        frame(&mut w, run, 1, 10, 30_000);
        let begin = 10 * MS;
        w.set_frame_ui(
            run,
            1,
            UiPhases {
                begin: SessionTime(begin),
                ui_us: 24_000,
                build_us: 3000,
                layout_us: 18_000,
                paint_us: 1000,
                composite_us: 500,
                finalize_us: 500,
                widgets_built: Some(2 + 9),
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let mut all = vec![
            (0, "VsyncProcessCallback", 0, 24_000),
            (1, "Animator::BeginFrame", 0, 24_000),
            (2, "BUILD", 0, 3000),
            (3, "CartBadge", 0, 2000),
            (4, "Text", 0, 1000),
            (2, "LAYOUT (root)", 3000, 18_000),
        ];
        for k in 0..3u64 {
            let at = 3100 + k * 5000;
            all.extend([
                (3, "BUILD", at, 4800),
                (4, "ProductTile", at, 4700),
                (5, "Card", at, 4600),
                (6, "Text", at, 100),
            ]);
        }
        put_slices(&mut w, ui, begin, all);
        let s = w.publish();
        let r = frame_rebuilds(&s, 0).unwrap();
        let built = s.frames.get(0).unwrap().ui.unwrap().widgets_built;
        assert_eq!(Some(r.total), built);
        assert_eq!(r.widgets.iter().map(|w| w.builds).sum::<u32>(), r.total);
        let builds = |n: &str| r.widgets.iter().find(|w| &*w.name == n).map(|w| w.builds);
        assert_eq!(
            (builds("ProductTile"), builds("Card"), builds("Text")),
            (Some(3), Some(3), Some(4))
        );
        // Each lazily built child is its own root.
        assert_eq!(r.roots.len(), 4);
        // Only the frame's BUILDs count: a BUILD of the next frame is outside the window.
        let all = range_rebuilds(&s, None, &AtomicBool::new(false)).unwrap();
        assert_eq!(all.iter().map(|w| w.builds).sum::<u32>(), 11);
    }

    #[test]
    fn a_frame_without_widget_slices_has_no_rebuilds() {
        let (mut w, run) = writer();
        cart_frame(&mut w, run, 1, 10, 0);
        frame(&mut w, run, 2, 100, 10_000);
        let s = w.publish();
        assert!(frame_rebuilds(&s, 1).is_none());
        assert!(frame_rebuilds(&s, 99).is_none());
    }

    /// Folding only the frames added since the last tally gives the same counts as counting the
    /// whole scope again, also when a frame's phases (and so its widget slices) land late.
    #[test]
    fn incremental_merge_equals_full() {
        let (mut w, run) = writer();
        cart_frame(&mut w, run, 1, 10, 10);
        cart_frame(&mut w, run, 2, 100, 30);
        // Frame 3's phases have not arrived yet.
        frame(&mut w, run, 3, 200, 30_000);
        let cancel = AtomicBool::new(false);
        let a = w.publish();
        let (tally, shown) = tally_rebuilds(&a, None, None, &cancel).unwrap();
        assert_eq!(shown, range_rebuilds(&a, None, &cancel).unwrap());
        assert_eq!(tally.settled, 2, "frame 3 may still change");
        // Frame 3's phases and slices land, and frames 4 and 5 follow.
        cart_frame(&mut w, run, 3, 200, 5);
        cart_frame(&mut w, run, 4, 300, 7);
        cart_frame(&mut w, run, 5, 400, 2);
        let b = w.publish();
        let (tally, shown) = tally_rebuilds(&b, None, Some(&tally), &cancel).unwrap();
        assert_eq!(shown, range_rebuilds(&b, None, &cancel).unwrap());
        assert_eq!(tally.settled, 5);
        let tile = shown.iter().find(|w| &*w.name == "ProductTile").unwrap();
        assert_eq!((tile.builds, tile.frames), (54, 5));
        // A range open at the end.
        let r = TimeRange::new(SessionTime(150 * MS), SessionTime(10_000 * MS));
        let (rt, _) = tally_rebuilds(&b, Some(r), None, &cancel).unwrap();
        cart_frame(&mut w, run, 6, 500, 4);
        let c = w.publish();
        let (_, shown) = tally_rebuilds(&c, None, Some(&tally), &cancel).unwrap();
        assert_eq!(shown, range_rebuilds(&c, None, &cancel).unwrap());
        let (_, in_range) = tally_rebuilds(&c, Some(r), Some(&rt), &cancel).unwrap();
        assert_eq!(in_range, range_rebuilds(&c, Some(r), &cancel).unwrap());
        cancel.store(true, Ordering::Relaxed);
        assert!(tally_rebuilds(&c, None, Some(&tally), &cancel).is_none());
    }

    #[test]
    fn range_rebuilds_merges_frames_and_cancels() {
        let (mut w, run) = writer();
        cart_frame(&mut w, run, 1, 10, 10);
        cart_frame(&mut w, run, 2, 100, 30);
        cart_frame(&mut w, run, 3, 200, 5);
        let s = w.publish();
        let cancel = AtomicBool::new(false);
        let all = range_rebuilds(&s, None, &cancel).unwrap();
        let tile = all.iter().find(|w| &*w.name == "ProductTile").unwrap();
        assert_eq!((tile.builds, tile.frames, tile.max_in_frame), (45, 3, 30));
        let some = range_rebuilds(
            &s,
            Some(TimeRange::new(SessionTime(50 * MS), SessionTime(150 * MS))),
            &cancel,
        )
        .unwrap();
        let tile = some.iter().find(|w| &*w.name == "ProductTile").unwrap();
        assert_eq!((tile.builds, tile.frames), (30, 1));
        cancel.store(true, Ordering::Relaxed);
        assert!(range_rebuilds(&s, None, &cancel).is_none());
    }
}
