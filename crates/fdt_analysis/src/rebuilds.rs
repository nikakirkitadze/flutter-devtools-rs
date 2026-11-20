//! Rebuild attribution from widget-build timeline slices.

use crate::user_code::is_user_library;
use fdt_store::Snapshot;
use fdt_store::cpu::FunctionId;
use fdt_store::time::{RunId, SessionTime, TimeRange};
use fdt_store::timeline::{LaneSnapshot, NameId};
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
    let frame = snap.frames.get(frame_seq)?;
    let ui = frame.ui?;
    let lanes = ui_lanes(snap, frame.run);
    let window = TimeRange::new(
        ui.begin,
        SessionTime(ui.begin.0 + u64::from(ui.ui_us) * 1_000),
    );
    // BUILD is found by name and time, at whatever depth it sits.
    let (build_depth, build) = lanes.iter().enumerate().find_map(|(d, lane)| {
        lane.range_indices(window)
            .map(|i| lane.get(i))
            .find(|s| {
                s.start >= window.start
                    && s.end() <= window.end
                    && snap.timeline.name(s.name) == BUILD
            })
            .map(|s| (d, *s))
    })?;
    let span = TimeRange::new(build.start, build.end());

    // `levels[k]` holds the slices of lane `build_depth + 1 + k` inside BUILD.
    let mut nodes: Vec<Node> = Vec::new();
    let mut prev: Vec<usize> = Vec::new();
    for lane in lanes.iter().skip(build_depth + 1) {
        let mut cur = Vec::new();
        let mut p = 0;
        for i in lane.range_indices(span) {
            let s = lane.get(i);
            if s.start < span.start || s.end() > span.end {
                continue;
            }
            let parent = if prev.is_empty() {
                None
            } else {
                // The enclosing slice one lane up; both lists are sorted by start.
                while p < prev.len() && nodes[prev[p]].end < s.end().0 {
                    p += 1;
                }
                let found = prev
                    .get(p)
                    .copied()
                    .filter(|&n| nodes[n].start <= s.start.0 && nodes[n].end >= s.end().0);
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
    let user = user_classes(snap);
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
    if !user.is_empty() {
        for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.parent.is_some()) {
            if !user.contains(snap.timeline.name(n.name)) {
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
fn user_classes(snap: &Snapshot) -> HashSet<String> {
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
                && let Some(fr) = frame_rebuilds(snap, i)
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
    let mut out: Vec<WidgetCount> = merged.into_values().collect();
    sort_widgets(&mut out);
    Some(out)
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

    /// `BUILD > CartScreen > Scaffold > {Body > ProductGrid > 3 x Tile, Bar}`.
    fn chain_frame(w: &mut SessionWriter, run: RunId) {
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
                widgets_built: Some(9),
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let nodes: [(u16, &str, u64, u64); 12] = [
            (0, "VsyncProcessCallback", 0, 20_000),
            (1, "Animator::BeginFrame", 0, 20_000),
            (2, "BUILD", 0, 15_000),
            (3, "CartScreen", 0, 14_000),
            (4, "Scaffold", 0, 13_000),
            (5, "Body", 0, 10_000),
            (6, "ProductGrid", 0, 9_000),
            (7, "Tile", 0, 100),
            (7, "Tile", 200, 100),
            (7, "Tile", 400, 100),
            (5, "Bar", 11_000, 1_000),
            (5, "Dummy", 12_500, 100),
        ];
        for (depth, name, at_us, dur_us) in nodes {
            let s = Slice {
                start: SessionTime(begin + at_us * 1000),
                dur_ns: dur_us * 1000,
                name: w.intern_name(name),
                cat: SliceCat::Dart,
            };
            assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
        }
    }

    #[test]
    fn largest_child_skips_framework_chains() {
        let (mut w, run) = writer();
        w.intern_function("_ProductGridState.build", Some("package:app/cart.dart"));
        w.intern_function(
            "Scaffold.build",
            Some("package:flutter/src/material/scaffold.dart"),
        );
        chain_frame(&mut w, run);
        let s = w.publish();
        let r = frame_rebuilds(&s, 0).unwrap();
        assert_eq!(&*r.roots[0].name, "CartScreen");
        assert_eq!(
            r.roots[0]
                .largest_child
                .clone()
                .map(|(n, m)| (n.to_string(), m)),
            Some(("ProductGrid".to_string(), 4))
        );
    }

    #[test]
    fn largest_child_falls_back_to_first_branch() {
        let (mut w, run) = writer();
        chain_frame(&mut w, run);
        let s = w.publish();
        let r = frame_rebuilds(&s, 0).unwrap();
        // No CPU names: CartScreen > Scaffold is a chain; Scaffold branches into Body and Bar.
        assert_eq!(
            r.roots[0]
                .largest_child
                .clone()
                .map(|(n, m)| (n.to_string(), m)),
            Some(("Body".to_string(), 5))
        );
        // CPU names without a match behave the same.
        let (mut w, run) = writer();
        w.intern_function("Other.build", Some("package:app/other.dart"));
        chain_frame(&mut w, run);
        let r = frame_rebuilds(&w.publish(), 0).unwrap();
        assert_eq!(&*r.roots[0].largest_child.clone().unwrap().0, "Body");
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
