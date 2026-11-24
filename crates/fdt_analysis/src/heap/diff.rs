//! The "go there and back" diff: the classes whose live count grew with objects the baseline
//! snapshot didn't have, matched by identity hash (H12), each with sample retaining paths.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use fdt_store::heap::{
    Growth, GrowthSample, HeapDiff, HeapId, HeapIndex, HeapRecord, HeapStage, HeapSummary, Tracked,
};

use super::details::labelled_path;

/// Most classes one section lists.
const MAX_LISTED: usize = 500;
/// Most sample objects per class.
const MAX_SAMPLES: usize = 3;

const RESTARTED: &str =
    "The app restarted between these snapshots, so their objects can't be matched.";

/// One class of a snapshot, with every snapshot class that shares its `(library, name)` folded in.
struct Group<'a> {
    /// The first snapshot class id with this name.
    class: u32,
    count: u64,
    tracked: Vec<&'a Tracked>,
}

fn groups(summary: &HeapSummary) -> HashMap<(&str, &str), Group<'_>> {
    let mut out: HashMap<(&str, &str), Group<'_>> = HashMap::new();
    for (id, c) in summary.classes.iter().enumerate() {
        let g = out
            .entry((&c.library[..], &c.name[..]))
            .or_insert_with(|| Group {
                class: id as u32,
                count: 0,
                tracked: Vec::new(),
            });
        g.count += c.count;
    }
    for t in summary.tracked.iter() {
        let Some(c) = summary.classes.get(t.class as usize) else {
            continue;
        };
        if let Some(g) = out.get_mut(&(&c.library[..], &c.name[..])) {
            g.tracked.push(t);
        }
    }
    out
}

/// What the diff keeps of a class before its samples are read.
struct Candidate {
    growth: Growth,
    /// `(node, retained)` of the new objects.
    new: Vec<(u32, u64)>,
}

/// Compares `after` (whose held `index` is given) with `baseline`. The paths are read from the
/// snapshot file, so an I/O error is returned rather than hidden in a sample without a path.
///
/// # Errors
/// When either record has no summary (`InvalidInput`) or a sample's path can't be read.
pub fn diff(after: &HeapRecord, index: &HeapIndex, baseline: &HeapRecord) -> io::Result<HeapDiff> {
    let (Some(now), Some(before)) = (&after.summary, &baseline.summary) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a heap diff needs both snapshots analysed",
        ));
    };
    let empty = |comparable| HeapDiff {
        baseline: baseline.id,
        comparable,
        user: Arc::from([]),
        lifecycle: Arc::from([]),
    };
    if after.run != baseline.run || after.isolate != baseline.isolate {
        return Ok(empty(Err(RESTARTED.into())));
    }

    let was = groups(before);
    let mut user = Vec::new();
    let mut lifecycle = Vec::new();
    for ((library, name), g) in groups(now) {
        let Some(class) = now.classes.get(g.class as usize) else {
            continue;
        };
        let old = was.get(&(library, name));
        let count_before = old.map_or(0, |o| o.count);
        if g.count <= count_before {
            continue;
        }
        let is_known = |hash: u32| {
            old.is_some_and(|o| {
                o.tracked
                    .iter()
                    .any(|t| t.hashes.binary_search(&hash).is_ok())
            })
        };
        let mut new = Vec::new();
        for t in &g.tracked {
            for (&hash, &node) in t.hashes.iter().zip(t.nodes.iter()) {
                if hash == 0 || !is_known(hash) {
                    new.push((node, index.retained(node)));
                }
            }
        }
        if new.is_empty() {
            continue;
        }
        let new_bytes = new.iter().map(|&(n, _)| u64::from(index.shallow(n))).sum();
        let candidate = Candidate {
            growth: Growth {
                class: g.class,
                before: count_before,
                after: g.count,
                new_objects: new.len() as u64,
                new_bytes,
                samples: Arc::from([]),
            },
            new,
        };
        if class.user {
            user.push(candidate);
        } else if class.lifecycle.is_some() {
            lifecycle.push(candidate);
        }
    }

    Ok(HeapDiff {
        baseline: baseline.id,
        comparable: Ok(()),
        user: finish(user, now, index)?,
        lifecycle: finish(lifecycle, now, index)?,
    })
}

/// Sorts by new bytes, keeps the first 500 and reads their samples.
fn finish(
    mut list: Vec<Candidate>,
    summary: &HeapSummary,
    index: &HeapIndex,
) -> io::Result<Arc<[Growth]>> {
    // The class id breaks ties, so the order doesn't depend on hash-map iteration.
    list.sort_by(|a, b| {
        b.growth
            .new_bytes
            .cmp(&a.growth.new_bytes)
            .then(a.growth.class.cmp(&b.growth.class))
    });
    list.truncate(MAX_LISTED);
    list.into_iter()
        .map(|mut c| {
            c.new.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let samples = c
                .new
                .iter()
                .take(MAX_SAMPLES)
                .map(|&(node, retained)| -> io::Result<GrowthSample> {
                    let path = labelled_path(index, summary, node)?;
                    Ok((node, retained, Arc::from(path)))
                })
                .collect::<io::Result<Vec<_>>>()?;
            c.growth.samples = samples.into();
            Ok(c.growth)
        })
        .collect()
}

/// `chosen` when it is `Ready`; otherwise the latest `Ready` record before `after` with the same
/// run and isolate.
pub fn default_baseline(
    heaps: &[HeapRecord],
    after: HeapId,
    chosen: Option<HeapId>,
) -> Option<HeapId> {
    let ready = |r: &HeapRecord| r.stage == HeapStage::Ready;
    if let Some(c) = chosen
        && heaps.iter().any(|r| r.id == c && ready(r))
    {
        return Some(c);
    }
    let target = heaps.iter().find(|r| r.id == after)?;
    heaps
        .iter()
        .filter(|r| r.id < after && ready(r) && r.run == target.run && r.isolate == target.isolate)
        .map(|r| r.id)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::dominators::{Analysis, AnalyzeInput, analyze};
    use crate::heap::encode::{SnapshotBuilder, encode_full};
    use crate::heap::format::Value;
    use fdt_store::heap::{Edge, HeapFile};
    use fdt_store::run::Mode;
    use fdt_store::time::{RunId, SessionTime};
    use std::sync::atomic::AtomicBool;

    const APP: &str = "package:app/screen.dart";
    const FW: &str = "package:flutter/src/widgets/framework.dart";

    /// `(class name, library, shallow, identity hash)` of objects all held by one static list.
    type Obj<'a> = (&'a str, &'a str, u64, u32);

    struct Snap {
        _dir: tempfile::TempDir,
        a: Analysis,
    }

    fn snapshot(objects: &[Obj]) -> Snap {
        let mut b = SnapshotBuilder::new();
        let list = b.class("_List", "dart:core", &["type_arguments", "length_"]);
        let mut classes: HashMap<(&str, &str), u32> = HashMap::new();
        let mut made = Vec::new();
        for &(name, lib, shallow, hash) in objects {
            let c = *classes
                .entry((name, lib))
                .or_insert_with(|| b.class(name, lib, &["type_arguments", "_widget", "_element"]));
            let o = b.object(c, shallow, Value::None, &[0, 0, 0]);
            if hash != 0 {
                b.hash(o, hash);
            }
            made.push(o);
        }
        let mut refs = vec![0, 0];
        refs.extend(&made);
        let l = b.object(list, 16, Value::Length(made.len() as u64), &refs);
        b.static_field("held", l);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.dartheap");
        std::fs::write(&path, encode_full(&b.finish())).unwrap();
        let input = AnalyzeInput {
            file: Arc::new(HeapFile::new(path)),
            flutter_version: None,
            mode: Mode::Profile,
            root_package: Some("app".to_owned()),
        };
        let a = analyze(&input, &AtomicBool::new(false), &mut |_| {}).expect("analyze");
        Snap { _dir: dir, a }
    }

    fn record(id: u32, run: u32, isolate: &str, s: Option<&Snap>) -> HeapRecord {
        HeapRecord {
            id: HeapId(id),
            run: RunId(run),
            isolate: isolate.into(),
            requested: SessionTime(0),
            time: None,
            bytes: 0,
            stage: HeapStage::Ready,
            file: None,
            summary: s.map(|s| Arc::new(s.a.summary.clone())),
            diff: None,
            index: None,
        }
    }

    fn name(d: &HeapDiff, g: &Growth, s: &Snap) -> String {
        let _ = d;
        s.a.summary.classes[g.class as usize].name.to_string()
    }

    #[test]
    fn grown_user_class_lists_only_new_objects() {
        let before = snapshot(&[("Screen", APP, 40, 11), ("Screen", APP, 40, 12)]);
        let after = snapshot(&[
            ("Screen", APP, 40, 11),
            ("Screen", APP, 40, 12),
            ("Screen", APP, 40, 13),
            ("Screen", APP, 60, 14),
            ("Other", APP, 8, 21),
        ]);
        let d = diff(
            &record(1, 1, "i/1", Some(&after)),
            &after.a.index,
            &record(0, 1, "i/1", Some(&before)),
        )
        .unwrap();
        assert_eq!(d.comparable, Ok(()));
        assert_eq!(d.baseline, HeapId(0));
        assert!(d.lifecycle.is_empty());
        let screen = d
            .user
            .iter()
            .find(|g| name(&d, g, &after) == "Screen")
            .unwrap();
        assert_eq!((screen.before, screen.after), (2, 4));
        assert_eq!(screen.new_objects, 2);
        assert_eq!(screen.new_bytes, 100);
        // `Other` is new, so it is listed too, and the larger class comes first.
        assert_eq!(d.user.len(), 2);
        assert_eq!(name(&d, &d.user[0], &after), "Screen");
    }

    #[test]
    fn lifecycle_classes_have_their_own_section() {
        let before = snapshot(&[("State", FW, 32, 5)]);
        let after = snapshot(&[("State", FW, 32, 5), ("State", FW, 32, 6)]);
        let d = diff(
            &record(1, 1, "i/1", Some(&after)),
            &after.a.index,
            &record(0, 1, "i/1", Some(&before)),
        )
        .unwrap();
        assert!(d.user.is_empty());
        assert_eq!(d.lifecycle.len(), 1);
        assert_eq!(d.lifecycle[0].new_objects, 1);
    }

    #[test]
    fn same_count_with_replaced_objects_is_not_listed() {
        let before = snapshot(&[("Screen", APP, 40, 11), ("Screen", APP, 40, 12)]);
        let after = snapshot(&[("Screen", APP, 40, 31), ("Screen", APP, 40, 32)]);
        let d = diff(
            &record(1, 1, "i/1", Some(&after)),
            &after.a.index,
            &record(0, 1, "i/1", Some(&before)),
        )
        .unwrap();
        assert_eq!(d.comparable, Ok(()));
        assert!(d.user.is_empty() && d.lifecycle.is_empty());
    }

    #[test]
    fn hash_zero_counts_as_new() {
        let before = snapshot(&[("Screen", APP, 40, 11)]);
        let after = snapshot(&[("Screen", APP, 40, 11), ("Screen", APP, 40, 0)]);
        let mut rec = record(1, 1, "i/1", Some(&after));
        // The analysis tracks only hashed objects; a hash of 0 in the list still counts as new.
        let mut summary = (**rec.summary.as_ref().unwrap()).clone();
        let t = &summary.tracked[0];
        let node = t.nodes[0];
        summary.tracked = Arc::from(vec![Tracked {
            class: t.class,
            hashes: Arc::from(vec![0, 11]),
            nodes: Arc::from(vec![node, node]),
        }]);
        rec.summary = Some(Arc::new(summary));
        let d = diff(&rec, &after.a.index, &record(0, 1, "i/1", Some(&before))).unwrap();
        assert_eq!(d.user.len(), 1);
        assert_eq!(d.user[0].new_objects, 1);
    }

    #[test]
    fn restart_is_not_comparable() {
        let before = snapshot(&[("Screen", APP, 40, 11)]);
        let after = snapshot(&[("Screen", APP, 40, 11), ("Screen", APP, 40, 12)]);
        for (run, isolate) in [(2, "i/1"), (1, "i/2")] {
            let d = diff(
                &record(1, run, isolate, Some(&after)),
                &after.a.index,
                &record(0, 1, "i/1", Some(&before)),
            )
            .unwrap();
            assert_eq!(
                d.comparable.as_ref().unwrap_err().as_ref(),
                "The app restarted between these snapshots, so their objects can't be matched."
            );
            assert!(d.user.is_empty() && d.lifecycle.is_empty());
        }
        assert!(
            diff(
                &record(1, 1, "i/1", None),
                &after.a.index,
                &record(0, 1, "i/1", Some(&before))
            )
            .is_err()
        );
    }

    #[test]
    fn samples_are_the_largest_new_with_paths() {
        let before = snapshot(&[("Screen", APP, 40, 1)]);
        let after = snapshot(&[
            ("Screen", APP, 40, 1),
            ("Screen", APP, 10, 2),
            ("Screen", APP, 50, 3),
            ("Screen", APP, 30, 4),
            ("Screen", APP, 20, 5),
        ]);
        let d = diff(
            &record(1, 1, "i/1", Some(&after)),
            &after.a.index,
            &record(0, 1, "i/1", Some(&before)),
        )
        .unwrap();
        let g = &d.user[0];
        assert_eq!(g.new_objects, 4);
        assert_eq!(g.samples.len(), 3);
        let retained: Vec<u64> = g.samples.iter().map(|s| s.1).collect();
        assert_eq!(retained, vec![50, 30, 20]);
        for (node, _, path) in g.samples.iter() {
            assert_eq!(path.first().unwrap().node, 0);
            assert_eq!(path.last().unwrap().node, *node);
            assert_eq!(path[0].edge, Edge::RootSlice("isolate".into()));
            assert_eq!(path[1].edge, Edge::Static("held".into()));
        }
    }

    #[test]
    fn unreadable_snapshot_file_is_an_error_not_an_empty_path() {
        let before = snapshot(&[("Screen", APP, 40, 1)]);
        let after = snapshot(&[("Screen", APP, 40, 1), ("Screen", APP, 40, 2)]);
        std::fs::remove_file(after.a.index.file().path()).unwrap();
        let r = diff(
            &record(1, 1, "i/1", Some(&after)),
            &after.a.index,
            &record(0, 1, "i/1", Some(&before)),
        );
        assert!(r.is_err());
    }

    #[test]
    fn default_baseline_cases() {
        let mut heaps = vec![
            record(0, 1, "i/1", None),
            record(1, 1, "i/2", None),
            record(2, 1, "i/1", None),
            record(3, 1, "i/1", None),
            record(4, 1, "i/1", None),
        ];
        // A chosen one wins.
        assert_eq!(
            default_baseline(&heaps, HeapId(4), Some(HeapId(0))),
            Some(HeapId(0))
        );
        // Otherwise the previous one; another isolate is skipped.
        assert_eq!(default_baseline(&heaps, HeapId(4), None), Some(HeapId(3)));
        assert_eq!(default_baseline(&heaps, HeapId(2), None), Some(HeapId(0)));
        // One that isn't Ready is skipped, chosen or not.
        heaps[3].stage = HeapStage::Cancelled;
        assert_eq!(default_baseline(&heaps, HeapId(4), None), Some(HeapId(2)));
        assert_eq!(
            default_baseline(&heaps, HeapId(4), Some(HeapId(3))),
            Some(HeapId(2))
        );
        // Another run is never a baseline.
        heaps[0].run = RunId(0);
        assert_eq!(default_baseline(&heaps, HeapId(2), None), None);
        assert_eq!(default_baseline(&heaps, HeapId(9), None), None);
    }
}
