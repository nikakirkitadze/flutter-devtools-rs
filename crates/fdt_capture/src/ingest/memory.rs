//! Memory polling, GC events and on-demand allocation profiles of one connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fdt_store::memory::{GcKind, GcReason, GcRecord};
use fdt_store::time::RunId;
use fdt_vm::events::{GcEvent, HeapSpace};
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::{Ingest, SharedSink};

/// What the memory task needs, from `CaptureConfig`.
#[derive(Clone, Debug)]
pub(crate) struct MemoryConfig {
    pub(crate) poll: Duration,
    pub(crate) profile_timeout: Duration,
    /// Used by the heap snapshot transfer.
    #[allow(dead_code)]
    pub(crate) request_timeout: Duration,
    #[allow(dead_code)]
    pub(crate) chunk_timeout: Duration,
    #[allow(dead_code)]
    pub(crate) heap_dir: PathBuf,
}

enum Msg {
    FetchProfile(bool),
}

/// The connection's memory commands.
pub(crate) struct MemoryControl {
    tx: mpsc::UnboundedSender<Msg>,
}

impl MemoryControl {
    /// Fetches the allocation profile of the main isolate. `gc` forces a full GC first, which
    /// every client sees; the caller passes it only when the user switched it on.
    pub(crate) fn fetch_profile(&self, gc: bool) {
        let _ = self.tx.send(Msg::FetchProfile(gc));
    }
}

/// The isolate and group the task works on; the router updates them at a hot restart.
struct Target {
    main: String,
    group: String,
}

/// Owns the memory task of one connection; aborts it when dropped.
pub(crate) struct MemoryHub {
    tx: mpsc::UnboundedSender<Msg>,
    task: JoinHandle<()>,
    target: Arc<Mutex<Target>>,
}

impl MemoryHub {
    /// `root_package` names the app's package, for classes of the app.
    pub(crate) fn new(
        sink: SharedSink,
        svc: VmService,
        cfg: MemoryConfig,
        main: String,
        group: String,
        _root_package: Option<String>,
    ) -> MemoryHub {
        let (tx, rx) = mpsc::unbounded_channel();
        let target = Arc::new(Mutex::new(Target { main, group }));
        let task = tokio::spawn(run(sink, svc, cfg, rx, target.clone()));
        MemoryHub { tx, task, target }
    }

    pub(crate) fn control(&self) -> Arc<MemoryControl> {
        Arc::new(MemoryControl {
            tx: self.tx.clone(),
        })
    }

    /// A hot restart: polling and profiles move to the new main isolate and its group.
    pub(crate) fn set_main(&self, isolate: &str, group: &str) {
        let mut t = self.target.lock().unwrap();
        t.main = isolate.to_string();
        t.group = group.to_string();
    }
}

impl Drop for MemoryHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    sink: SharedSink,
    svc: VmService,
    cfg: MemoryConfig,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    target: Arc<Mutex<Target>>,
) {
    let mut tick = tokio::time::interval(cfg.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut rss_works = true;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let group = target.lock().unwrap().group.clone();
                if group.is_empty() {
                    continue;
                }
                let (usage, rss) = tokio::join!(svc.get_isolate_group_memory_usage(&group), async {
                    if rss_works { Some(svc.get_process_rss().await) } else { None }
                });
                let rss = match rss {
                    Some(Ok(v)) => Some(v),
                    Some(Err(RpcError::Closed)) => return,
                    Some(Err(e)) => {
                        log::debug!("getProcessMemoryUsage failed; not asking again: {e}");
                        rss_works = false;
                        None
                    }
                    None => None,
                };
                match usage {
                    Ok(usage) => {
                        let s = sink.get();
                        s.send(Ingest::MemorySample { run: s.run, usage, rss });
                    }
                    Err(RpcError::Closed) => return,
                    Err(e) => log::debug!("getIsolateGroupMemoryUsage failed: {e}"),
                }
            }
            msg = rx.recv() => match msg {
                Some(Msg::FetchProfile(gc)) => {
                    let isolate = target.lock().unwrap().main.clone();
                    let svc = svc.clone();
                    let sink = sink.get();
                    let timeout = cfg.profile_timeout;
                    // Its own task: a slow profile must not hold up the polling.
                    tokio::spawn(async move {
                        match svc.get_allocation_profile(&isolate, gc, timeout).await {
                            Ok(profile) => sink.send(Ingest::AllocProfile {
                                run: sink.run,
                                isolate,
                                gc,
                                profile: Box::new(profile),
                            }),
                            Err(e) => log::warn!("getAllocationProfile failed: {e}"),
                        }
                    });
                }
                None => return,
            },
        }
    }
}

/// `basename:line` for a class's location.
pub(crate) fn location_text(uri: &str, line: Option<u32>) -> String {
    let base = Path::new(uri)
        .file_name()
        .and_then(|n| n.to_str())
        .or_else(|| uri.rsplit('/').next())
        .unwrap_or(uri);
    match line {
        Some(l) => format!("{base}:{l}"),
        None => base.to_string(),
    }
}

/// Turns GC events into records: one per GC (the VM sends one event per isolate), with the
/// duration from the growth of the cumulative `time`.
#[derive(Default)]
pub(crate) struct GcTracker {
    last: HashMap<String, (HeapSpace, HeapSpace)>,
}

impl GcTracker {
    pub(crate) fn record(&mut self, sink: &super::Sink, ev: &GcEvent) -> Option<(RunId, GcRecord)> {
        let prev = self
            .last
            .insert(ev.isolate_group.clone(), (ev.new_space, ev.old_space));
        let dur_us = match prev {
            Some((pn, po)) => {
                if pn.collections == ev.new_space.collections
                    && po.collections == ev.old_space.collections
                {
                    return None;
                }
                let grew = |a: &HeapSpace, b: &HeapSpace| {
                    (b.collections > a.collections).then(|| (b.time_s - a.time_s).max(0.0))
                };
                let secs = grew(&pn, &ev.new_space).unwrap_or(0.0)
                    + grew(&po, &ev.old_space).unwrap_or(0.0);
                Some((secs * 1e6).round().min(f64::from(u32::MAX)) as u32)
            }
            None => None,
        };
        Some((
            sink.run,
            GcRecord {
                time: sink.clock.from_epoch_ms(ev.timestamp_ms).time,
                dur_us,
                kind: GcKind::from_vm(&ev.gc_type),
                reason: ev
                    .reason
                    .as_deref()
                    .map_or(GcReason::Other, GcReason::from_vm),
                heap_used: ev.new_space.used + ev.old_space.used,
                heap_capacity: ev.new_space.capacity + ev.old_space.capacity,
                external: ev.new_space.external + ev.old_space.external,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::clock::RunClock;

    fn sink() -> super::super::Sink {
        let (tx, _rx) = mpsc::unbounded_channel();
        let clock = RunClock {
            origin_host_us: 0,
            skew_us: 0,
            timeline_to_wall_us: 0,
        };
        super::super::Sink::new(RunId(1), clock, 1, 0, tx)
    }

    fn space(collections: u64, time_s: f64) -> HeapSpace {
        HeapSpace {
            used: 10,
            capacity: 20,
            external: 1,
            collections,
            time_s,
        }
    }

    fn ev(group: &str, new: HeapSpace, old: HeapSpace) -> GcEvent {
        GcEvent {
            isolate_group: group.into(),
            gc_type: "Scavenge".into(),
            reason: None,
            timestamp_ms: 5,
            new_space: new,
            old_space: old,
        }
    }

    #[test]
    fn duration_sums_the_spaces_that_grew() {
        let (mut t, s) = (GcTracker::default(), sink());
        let first = ev("g", space(1, 0.001), space(1, 0.002));
        assert_eq!(t.record(&s, &first).unwrap().1.dur_us, None);
        // Both spaces collected: 0.0004 + 0.0003 s.
        let both = ev("g", space(2, 0.0014), space(2, 0.0023));
        assert_eq!(t.record(&s, &both).unwrap().1.dur_us, Some(700));
    }

    #[test]
    fn duration_of_an_old_space_only_gc_ignores_new_space_time() {
        let (mut t, s) = (GcTracker::default(), sink());
        t.record(&s, &ev("g", space(1, 0.001), space(1, 0.002)));
        // New space's time moved but its count did not: only old space grew.
        let old_only = ev("g", space(1, 0.0015), space(2, 0.0027));
        assert_eq!(t.record(&s, &old_only).unwrap().1.dur_us, Some(700));
    }

    #[test]
    fn a_new_group_starts_without_a_duration() {
        let (mut t, s) = (GcTracker::default(), sink());
        t.record(&s, &ev("g1", space(5, 0.5), space(5, 0.5)));
        t.record(&s, &ev("g1", space(6, 0.6), space(5, 0.5)));
        // After a hot restart the new group's counters start over.
        let first = t.record(&s, &ev("g2", space(1, 0.0006), space(1, 0.0)));
        assert_eq!(first.unwrap().1.dur_us, None);
        // The same GC seen again is dropped.
        assert!(
            t.record(&s, &ev("g2", space(1, 0.0006), space(1, 0.0)))
                .is_none()
        );
    }
}
