//! The Memory details pane. For a class of the allocation profile it lists the instance count and
//! size in every profile taken, so growth over time is visible, and where the class is defined.

use crate::tools::memory::group_digits;
use crate::tools::network::model::format_bytes;
use fdt_store::memory::{AllocClassId, MemorySnapshot};
use fdt_store::time::SessionTime;

/// One profile's count of a class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryPoint {
    pub time: SessionTime,
    pub instances: u64,
    pub bytes: u64,
}

impl HistoryPoint {
    pub fn instances_text(&self) -> String {
        group_digits(self.instances)
    }

    pub fn size_text(&self) -> String {
        format_bytes(self.bytes)
    }
}

/// The counts of class `id` in every profile, oldest first. A profile from the same isolate is
/// matched by class id; one from another isolate (after a restart) by library and name. A
/// profile without the class has no point.
pub fn class_history(mem: &MemorySnapshot, id: AllocClassId) -> Vec<HistoryPoint> {
    if id.0 as usize >= mem.alloc_class_count() {
        return Vec::new();
    }
    let class = mem.alloc_class(id);
    let mut out = Vec::with_capacity(mem.profiles().len());
    for p in mem.profiles() {
        let same = p.run == class.run && p.isolate == class.isolate;
        let found = if same {
            p.rows.iter().find(|r| r.class == id)
        } else {
            p.rows.iter().find(|r| {
                let c = mem.alloc_class(r.class);
                c.name == class.name && c.library == class.library
            })
        };
        if let Some(r) = found {
            out.push(HistoryPoint {
                time: p.time,
                instances: r.instances,
                bytes: r.bytes,
            });
        }
    }
    out
}

/// Whether the held heap snapshot's summary has a class named `name` in `library`.
pub fn held_has_class(mem: &MemorySnapshot, name: &str, library: Option<&str>) -> bool {
    mem.held()
        .and_then(|id| mem.heap(id))
        .and_then(|h| h.summary.as_ref())
        .is_some_and(|s| {
            s.classes
                .iter()
                .any(|c| &*c.name == name && library.is_none_or(|l| &*c.library == l))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::writer;
    use fdt_store::memory::{AllocProfile, AllocRow};
    use fdt_store::time::RunId;
    use std::sync::Arc;

    fn push(
        w: &mut fdt_store::SessionWriter,
        run: RunId,
        isolate: &str,
        ms: u64,
        rows: Vec<AllocRow>,
    ) {
        w.push_alloc_profile(AllocProfile {
            time: SessionTime(ms * 1_000_000),
            run,
            isolate: isolate.into(),
            gc: false,
            heap_used: 0,
            rows: Arc::from(rows),
        });
    }

    #[test]
    fn history_matches_by_id_then_by_name_after_a_restart() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(run, "i/1", "c/1", "Foo", Some("package:app/a.dart"), None);
        let b = w.intern_alloc_class(run, "i/2", "c/9", "Foo", Some("package:app/a.dart"), None);
        let row = |class, instances, bytes| AllocRow {
            class,
            instances,
            bytes,
        };
        push(&mut w, run, "i/1", 100, vec![row(a, 1, 16)]);
        push(&mut w, run, "i/1", 200, vec![row(a, 5, 80)]);
        push(&mut w, run, "i/2", 300, vec![row(b, 2, 32)]);
        let snap = w.publish();
        let h = class_history(&snap.memory, a);
        let got: Vec<_> = h
            .iter()
            .map(|p| (p.time.0 / 1_000_000, p.instances))
            .collect();
        assert_eq!(got, [(100, 1), (200, 5), (300, 2)]);
    }
}
