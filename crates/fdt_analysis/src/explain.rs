//! The one-sentence explanation of a slow frame.

use crate::breakdown::{Breakdown, Phase, breakdown};
use crate::correlate::{Related, related_to_frame};
use crate::cpu_tree::{FunctionCost, busiest, leaf_counts};
use crate::rebuilds::{FrameRebuilds, frame_rebuilds};
use crate::user_code::is_user_library;
use fdt_store::Snapshot;
use fdt_store::cpu::{CpuStatus, FunctionId, NodeId};
use fdt_store::frames::{FrameRecord, FrameStatus};
use fdt_store::time::{SessionTime, TimeRange};
use fdt_store::timeline::SliceCat;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpuState {
    Covered,
    Pending,
    Off(Arc<str>),
    NoSamples,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cause {
    WithinBudget,
    Raster {
        save_layers: u32,
    },
    Gc {
        gc_ns: u64,
    },
    WidgetBuild {
        widget: Arc<str>,
        self_ns: u64,
        share: f32,
    },
    Rebuilt {
        root: Arc<str>,
        widgets: u32,
        child: Option<(Arc<str>, u32)>,
    },
    HotFunction {
        function: Arc<str>,
        phase: Phase,
        share: f32,
    },
    Phase(Phase),
}

#[derive(Clone, Debug)]
pub struct Explanation {
    pub sentence: String,
    pub cause: Cause,
    pub breakdown: Breakdown,
    pub rebuilds: Option<FrameRebuilds>,
    /// At most [`MAX_FUNCTIONS`] entries from the frame's UI window, user code first.
    pub functions: Vec<FunctionCost>,
    pub cpu: CpuState,
    pub related: Related,
}

pub const MAX_FUNCTIONS: usize = 10;
const MAX_NAME_CHARS: usize = 80;
const SAVE_LAYER: &str = "Canvas::saveLayer";
const SAVE_LAYER_RULE: u32 = 10;

/// `{:.1} ms` under 10 ms, `{:.0} ms` otherwise.
pub fn format_ms(us: u64) -> String {
    let ms = us as f64 / 1000.0;
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// `1,345`.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Names longer than 80 characters are cut to 79 plus an ellipsis.
fn cut(name: &str) -> String {
    if name.chars().nth(MAX_NAME_CHARS).is_none() {
        return name.to_string();
    }
    let mut s: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    s.push('…');
    s
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Build => "build",
        Phase::Layout => "layout",
        Phase::Paint => "paint",
        Phase::Composite => "compositing",
        Phase::Finalize => "frame finalization",
        Phase::OtherUi => "UI work outside build, layout and paint",
        Phase::Raster => "raster",
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn phase_us(bd: &Breakdown, p: Phase) -> u32 {
    bd.segments
        .iter()
        .find(|s| s.phase == p)
        .map_or(0, |s| s.us)
}

fn ns_range(start: SessionTime, dur_us: u32) -> TimeRange {
    TimeRange::new(start, SessionTime(start.0 + u64::from(dur_us) * 1_000))
}

fn ui_window(frame: &FrameRecord) -> TimeRange {
    match frame.ui {
        Some(u) => ns_range(u.begin, u.ui_us),
        None => ns_range(frame.start, frame.build_us),
    }
}

fn raster_window(frame: &FrameRecord) -> TimeRange {
    ns_range(
        frame.raster.map_or(frame.start, |r| r.start),
        frame.raster_us,
    )
}

/// The window of a UI phase: the phases run one after the other from the UI thread's start, with
/// the unattributed remainder last.
fn phase_window(frame: &FrameRecord, bd: &Breakdown, phase: Phase) -> Option<TimeRange> {
    let u = frame.ui?;
    let order = [
        Phase::Build,
        Phase::Layout,
        Phase::Paint,
        Phase::Composite,
        Phase::Finalize,
        Phase::OtherUi,
    ];
    let mut offset = 0u64;
    for p in order {
        let us = u64::from(phase_us(bd, p));
        if p == phase {
            let start = u.begin.0 + offset * 1_000;
            return Some(TimeRange::new(
                SessionTime(start),
                SessionTime(start + us * 1_000),
            ));
        }
        offset += us;
    }
    None
}

fn save_layers(snap: &Snapshot, frame: &FrameRecord) -> u32 {
    let window = raster_window(frame);
    let mut n = 0u32;
    for t in snap
        .timeline
        .tracks()
        .iter()
        .filter(|t| t.run == frame.run && t.order == 1)
    {
        for lane in snap.timeline.lanes(t.id) {
            for i in lane.range_indices(window) {
                let s = lane.get(i);
                if s.end() >= window.start
                    && s.start <= window.end
                    && snap.timeline.name(s.name) == SAVE_LAYER
                {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Nanoseconds of garbage collection overlapping `window`, clipped to it.
fn gc_ns(snap: &Snapshot, frame: &FrameRecord, window: TimeRange) -> u64 {
    let mut total = 0u64;
    for t in snap.timeline.tracks().iter().filter(|t| t.run == frame.run) {
        let Some(lane) = snap.timeline.lanes(t.id).first() else {
            continue;
        };
        for i in lane.range_indices(window) {
            let s = lane.get(i);
            if s.cat != SliceCat::Gc {
                continue;
            }
            let (a, b) = (s.start.0.max(window.start.0), s.end().0.min(window.end.0));
            total += b.saturating_sub(a);
        }
    }
    total
}

struct Hot {
    function: FunctionId,
    share: f32,
}

/// Among user-code functions, the deepest of those within 90 % of the best total in `window`;
/// with no user code, the busiest function.
fn hot_function(snap: &Snapshot, window: TimeRange) -> Option<Hot> {
    let cpu = &snap.cpu;
    let never = AtomicBool::new(false);
    let (counts, samples) = leaf_counts(cpu, Some(window), &never)?;
    if samples == 0 {
        return None;
    }
    let costs = busiest(cpu, Some(window), usize::MAX);
    let is_user = |f: FunctionId| is_user_library(cpu.function(f).library.as_deref());
    let best = costs
        .iter()
        .filter(|c| is_user(c.function))
        .map(|c| c.total_count)
        .max();
    let Some(best) = best else {
        let top = costs.first()?;
        return Some(Hot {
            function: top.function,
            share: top.total_count as f32 / samples as f32,
        });
    };
    // The deepest trie node holding each function among the nodes that have samples below.
    let mut totals = counts;
    for i in (1..totals.len()).rev() {
        let parent = cpu.node(NodeId(i as u32)).parent.0 as usize;
        totals[parent] += totals[i];
    }
    let mut depth: HashMap<FunctionId, u16> = HashMap::new();
    for (i, &t) in totals.iter().enumerate().skip(1) {
        if t > 0 {
            let n = cpu.node(NodeId(i as u32));
            let d = depth.entry(n.function).or_insert(0);
            *d = (*d).max(n.depth);
        }
    }
    let pick = costs
        .iter()
        .filter(|c| is_user(c.function) && c.total_count * 10 >= best * 9)
        .max_by(|a, b| {
            let key = |c: &FunctionCost| {
                (
                    depth.get(&c.function).copied().unwrap_or(0),
                    c.total_count,
                    std::cmp::Reverse(c.function.0),
                )
            };
            key(a).cmp(&key(b))
        })?;
    Some(Hot {
        function: pick.function,
        share: pick.total_count as f32 / samples as f32,
    })
}

fn cpu_state(snap: &Snapshot, frame: &FrameRecord, window: TimeRange) -> CpuState {
    if let CpuStatus::Off(reason) = snap.cpu.status(frame.run) {
        return CpuState::Off(reason.clone());
    }
    match snap.cpu.coverage_end(frame.run) {
        Some(end) if end >= frame.end() => {
            if snap.cpu.range(window).is_empty() {
                CpuState::NoSamples
            } else {
                CpuState::Covered
            }
        }
        _ => CpuState::Pending,
    }
}

fn functions(snap: &Snapshot, window: TimeRange) -> Vec<FunctionCost> {
    let mut all = busiest(&snap.cpu, Some(window), usize::MAX);
    // Stable: user code first, each group keeps the busiest-first order.
    all.sort_by_key(|c| !is_user_library(snap.cpu.function(c.function).library.as_deref()));
    all.truncate(MAX_FUNCTIONS);
    all
}

pub fn explain(snap: &Snapshot, frame_seq: usize) -> Explanation {
    let Some(frame) = snap.frames.get(frame_seq) else {
        return Explanation {
            sentence: "No such frame.".to_string(),
            cause: Cause::WithinBudget,
            breakdown: Breakdown {
                segments: Vec::new(),
                ui_us: 0,
                raster_us: 0,
                budget_us: snap.latest_budget_us(),
                status: FrameStatus::Ok,
                over: None,
                has_timeline: false,
            },
            rebuilds: None,
            functions: Vec::new(),
            cpu: CpuState::Pending,
            related: Related::default(),
        };
    };
    let budget = snap.frame_budget_us(frame.run);
    let bd = breakdown(frame, budget);
    let rebuilds = frame_rebuilds(snap, frame_seq);
    let related = related_to_frame(snap, frame_seq);
    let window = ui_window(frame);
    let cpu = cpu_state(snap, frame, window);
    let functions = functions(snap, window);
    let (sentence, cause) = pick(snap, frame, &bd, rebuilds.as_ref(), &cpu, window);
    Explanation {
        sentence,
        cause,
        breakdown: bd,
        rebuilds,
        functions,
        cpu,
        related,
    }
}

/// `{us}` over the budget as `{:.1}×`.
fn times_budget(us: u32, budget: u32) -> String {
    format!("{:.1}×", f64::from(us) / f64::from(budget.max(1)))
}

/// The first matching rule of the table.
fn pick(
    snap: &Snapshot,
    frame: &FrameRecord,
    bd: &Breakdown,
    rebuilds: Option<&FrameRebuilds>,
    cpu: &CpuState,
    window: TimeRange,
) -> (String, Cause) {
    // 1
    let Some(over) = bd.over.filter(|_| bd.status != FrameStatus::Ok) else {
        let s = format!(
            "Within budget ({} of {}).",
            format_ms(u64::from(frame.time_us())),
            format_ms(u64::from(bd.budget_us))
        );
        return (s, Cause::WithinBudget);
    };
    let budget = bd.budget_us;
    if over == Phase::Raster {
        let r = format_ms(u64::from(bd.raster_us));
        let x = times_budget(bd.raster_us, budget);
        // 2
        let n = save_layers(snap, frame);
        if n >= SAVE_LAYER_RULE {
            let s = format!(
                "Raster took {r}, {x} the budget: the scene used {} saveLayer calls (opacity, blurs or clips).",
                group(u64::from(n))
            );
            return (s, Cause::Raster { save_layers: n });
        }
        // 3
        return (
            format!("Raster took {r}, {x} the budget."),
            Cause::Phase(Phase::Raster),
        );
    }
    // 4
    let gc = gc_ns(snap, frame, window);
    if bd.ui_us > 0 && gc * 4 >= u64::from(bd.ui_us) * 1_000 {
        let s = format!(
            "Garbage collection ran for {} during this frame's {} of UI work.",
            format_ms(gc / 1_000),
            format_ms(u64::from(bd.ui_us))
        );
        return (s, Cause::Gc { gc_ns: gc });
    }
    if over == Phase::Build
        && let Some(r) = rebuilds
    {
        // 5
        let build_ns = u64::from(phase_us(bd, Phase::Build)) * 1_000;
        if let Some(w) = r.widgets.iter().max_by_key(|w| w.self_ns)
            && build_ns > 0
            && w.self_ns * 2 >= build_ns
        {
            let share = w.self_ns as f32 / build_ns as f32;
            let s = format!(
                "{} took {} to build ({:.0}% of the build phase).",
                cut(&w.name),
                format_ms(w.self_ns / 1_000),
                share * 100.0
            );
            return (
                s,
                Cause::WidgetBuild {
                    widget: w.name.clone(),
                    self_ns: w.self_ns,
                    share,
                },
            );
        }
        // 6
        if let Some(root) = r.roots.first() {
            let n = group(u64::from(root.widgets));
            let s = match &root.largest_child {
                Some((c, m)) => format!(
                    "{} rebuilt {n} widgets, including {} ({}).",
                    cut(&root.name),
                    cut(c),
                    group(u64::from(*m))
                ),
                None => format!("{} rebuilt {n} widgets.", cut(&root.name)),
            };
            return (
                s,
                Cause::Rebuilt {
                    root: root.name.clone(),
                    widgets: root.widgets,
                    child: root.largest_child.clone(),
                },
            );
        }
    }
    // 7
    if *cpu == CpuState::Covered
        && let Some(pw) = phase_window(frame, bd, over)
        && let Some(hot) = hot_function(snap, pw)
    {
        let name = snap.cpu.function(hot.function).name.clone();
        let s = format!(
            "{} ran for most of the {} ({:.0}% of samples).",
            cut(&name),
            phase_name(over),
            hot.share * 100.0
        );
        return (
            s,
            Cause::HotFunction {
                function: name,
                phase: over,
                share: hot.share,
            },
        );
    }
    // 8
    let mut s = format!(
        "{} took {}, {} the budget.",
        capitalise(phase_name(over)),
        format_ms(u64::from(phase_us(bd, over))),
        times_budget(phase_us(bd, over), budget)
    );
    if over == Phase::Build {
        s.push_str(" Turn on Trace widget builds to see which widgets rebuilt.");
    }
    (s, Cause::Phase(over))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{MS, writer};
    use fdt_store::SessionWriter;
    use fdt_store::cpu::CpuSample;
    use fdt_store::frames::{FrameInput, RasterPhase, UiPhases};
    use fdt_store::time::RunId;
    use fdt_store::timeline::{Slice, TrackId, TrackKind};

    const APP: &str = "file:///app/lib/screen.dart";
    const FW: &str = "file:///sdk/flutter/packages/flutter/lib/src/widgets/framework.dart";

    /// Frame `number` at 10 ms * number with the given UI phases `[build, layout, paint,
    /// composite, finalize]` in `ui_us`, and a raster thread of `raster_us`.
    fn put_frame(
        w: &mut SessionWriter,
        run: RunId,
        number: u64,
        ui: ([u32; 5], u32),
        raster_us: u32,
    ) -> u64 {
        let start = number * 1000 * MS;
        let ([b, l, p, c, f], ui_us) = ui;
        w.push_frame(FrameInput {
            run,
            number,
            start: SessionTime(start),
            elapsed_us: ui_us.max(raster_us),
            build_us: ui_us,
            raster_us,
            vsync_overhead_us: 0,
        });
        w.set_frame_ui(
            run,
            number,
            UiPhases {
                begin: SessionTime(start),
                ui_us,
                build_us: b,
                layout_us: l,
                paint_us: p,
                composite_us: c,
                finalize_us: f,
                widgets_built: None,
            },
        );
        w.set_frame_raster(
            run,
            number,
            RasterPhase {
                start: SessionTime(start),
                raster_us,
            },
        );
        start
    }

    struct Tracks {
        ui: TrackId,
        raster: TrackId,
        worker: TrackId,
    }

    fn tracks(w: &mut SessionWriter, run: RunId) -> Tracks {
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let raster = w.timeline_track(run, TrackKind::Thread, 2, "io.flutter.raster");
        w.set_track_order(raster, 1);
        let worker = w.timeline_track(run, TrackKind::Thread, 3, "DartWorker");
        Tracks { ui, raster, worker }
    }

    #[allow(clippy::too_many_arguments)]
    fn put(
        w: &mut SessionWriter,
        track: TrackId,
        depth: u16,
        name: &str,
        start: u64,
        at_us: u64,
        dur_us: u64,
        cat: SliceCat,
    ) {
        let s = Slice {
            start: SessionTime(start + at_us * 1000),
            dur_ns: dur_us * 1000,
            name: w.intern_name(name),
            cat,
        };
        assert!(w.push_slice(track, depth, s, depth > 2), "{name} stored");
    }

    fn explain_of(w: &mut SessionWriter, seq: usize) -> Explanation {
        explain(&w.publish(), seq)
    }

    const BUILD_OVER: ([u32; 5], u32) = ([14_000, 2000, 2000, 500, 500], 24_000);

    #[test]
    fn rule_1_within_budget() {
        let (mut w, run) = writer();
        w.push_frame(FrameInput {
            run,
            number: 1,
            start: SessionTime(MS),
            elapsed_us: 8200,
            build_us: 8200,
            raster_us: 4000,
            vsync_overhead_us: 0,
        });
        let e = explain_of(&mut w, 0);
        assert_eq!(e.sentence, "Within budget (8.2 ms of 17 ms).");
        assert_eq!(e.cause, Cause::WithinBudget);
    }

    #[test]
    fn rule_2_raster_with_save_layers() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, ([2000, 1000, 1000, 500, 500], 5000), 40_000);
        let t = tracks(&mut w, run);
        for i in 0..12 {
            put(
                &mut w,
                t.raster,
                1,
                "Canvas::saveLayer",
                start,
                i * 2000,
                1000,
                SliceCat::Embedder,
            );
        }
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "Raster took 40 ms, 2.4× the budget: the scene used 12 saveLayer calls (opacity, blurs or clips)."
        );
        assert_eq!(e.cause, Cause::Raster { save_layers: 12 });
    }

    #[test]
    fn rule_3_raster() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, ([2000, 1000, 1000, 500, 500], 5000), 40_000);
        let t = tracks(&mut w, run);
        // Nine saveLayer calls are not enough.
        for i in 0..9 {
            put(
                &mut w,
                t.raster,
                1,
                "Canvas::saveLayer",
                start,
                i * 2000,
                1000,
                SliceCat::Embedder,
            );
        }
        let e = explain_of(&mut w, 0);
        assert_eq!(e.sentence, "Raster took 40 ms, 2.4× the budget.");
        assert_eq!(e.cause, Cause::Phase(Phase::Raster));
    }

    #[test]
    fn rule_4_garbage_collection() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        // 8 ms of the 24 ms of UI work, one third.
        put(
            &mut w,
            t.worker,
            0,
            "CollectNewGeneration",
            start,
            2000,
            8000,
            SliceCat::Gc,
        );
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "Garbage collection ran for 8.0 ms during this frame's 24 ms of UI work."
        );
        assert_eq!(e.cause, Cause::Gc { gc_ns: 8_000_000 });
    }

    /// BUILD (14 ms) holding `HeavyList` (12 ms, 2 ms of it in `Small`).
    fn heavy_build(w: &mut SessionWriter, t: &Tracks, start: u64) {
        put(
            w,
            t.ui,
            0,
            "VsyncProcessCallback",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(
            w,
            t.ui,
            1,
            "Animator::BeginFrame",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(w, t.ui, 2, "BUILD", start, 0, 14_000, SliceCat::Dart);
        put(w, t.ui, 3, "HeavyList", start, 0, 12_000, SliceCat::Dart);
        put(w, t.ui, 4, "Small", start, 100, 2000, SliceCat::Dart);
        put(w, t.ui, 3, "Other", start, 12_500, 500, SliceCat::Dart);
    }

    #[test]
    fn rule_5_one_widget_dominates_the_build() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        heavy_build(&mut w, &t, start);
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "HeavyList took 10 ms to build (71% of the build phase)."
        );
        match e.cause {
            Cause::WidgetBuild {
                widget,
                self_ns,
                share,
            } => {
                assert_eq!((&*widget, self_ns), ("HeavyList", 10_000_000));
                assert!((share - 0.714).abs() < 0.01);
            }
            c => panic!("{c:?}"),
        }
        assert!(e.rebuilds.is_some());
    }

    #[test]
    fn rule_6_rebuilt_roots() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        put(
            &mut w,
            t.ui,
            0,
            "VsyncProcessCallback",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(
            &mut w,
            t.ui,
            1,
            "Animator::BeginFrame",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(&mut w, t.ui, 2, "BUILD", start, 0, 14_000, SliceCat::Dart);
        // Four widgets, none above half of the build.
        put(&mut w, t.ui, 3, "Cart", start, 0, 4000, SliceCat::Dart);
        put(&mut w, t.ui, 4, "Row", start, 0, 2000, SliceCat::Dart);
        put(&mut w, t.ui, 4, "Badge", start, 2000, 1000, SliceCat::Dart);
        put(&mut w, t.ui, 5, "Dot", start, 2000, 500, SliceCat::Dart);
        put(&mut w, t.ui, 3, "Lone", start, 5000, 500, SliceCat::Dart);
        let e = explain_of(&mut w, 0);
        assert_eq!(e.sentence, "Cart rebuilt 4 widgets, including Badge (2).");
        assert_eq!(
            e.cause,
            Cause::Rebuilt {
                root: "Cart".into(),
                widgets: 4,
                child: Some(("Badge".into(), 2)),
            }
        );

        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        put(
            &mut w,
            t.ui,
            0,
            "VsyncProcessCallback",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(
            &mut w,
            t.ui,
            1,
            "Animator::BeginFrame",
            start,
            0,
            24_000,
            SliceCat::Dart,
        );
        put(&mut w, t.ui, 2, "BUILD", start, 0, 14_000, SliceCat::Dart);
        put(&mut w, t.ui, 3, "A", start, 0, 3000, SliceCat::Dart);
        put(&mut w, t.ui, 3, "B", start, 4000, 3000, SliceCat::Dart);
        let e = explain_of(&mut w, 0);
        assert_eq!(e.sentence, "A rebuilt 1 widgets.");
    }

    /// Ten samples in the BUILD phase of frame 1: `a` with the stack `[framework, outer]` and `b`
    /// with `[framework, outer, inner]`.
    fn cpu_frame(a: u64, b: u64) -> (Explanation, SessionWriter) {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let fw = w.intern_function("buildScope", Some(FW));
        let outer = w.intern_function("outerWork", Some(APP));
        let inner = w.intern_function("innerWork", Some(APP));
        let n_a = w.intern_stack(&[fw, outer]);
        let n_b = w.intern_stack(&[fw, outer, inner]);
        let samples = (0..a + b)
            .map(|i| CpuSample {
                time: SessionTime(start + 100_000 * (i + 1)),
                leaf: if i < a { n_a } else { n_b },
            })
            .collect();
        w.push_cpu_samples(run, samples);
        w.set_cpu_coverage(run, SessionTime(start + 100 * MS));
        w.set_cpu_status(run, CpuStatus::On);
        let e = explain_of(&mut w, 0);
        (e, w)
    }

    #[test]
    fn rule_7_hot_function() {
        // outer holds 10 of 10 samples, inner 6: only outer is within 90 %.
        let (e, _) = cpu_frame(4, 6);
        assert_eq!(
            e.sentence,
            "outerWork ran for most of the build (100% of samples)."
        );
        assert!(matches!(
            e.cause,
            Cause::HotFunction {
                phase: Phase::Build,
                ..
            }
        ));
        assert_eq!(e.cpu, CpuState::Covered);
        // inner holds 9 of 10, within 90 % of outer's 10: the deeper one wins.
        let (e, _) = cpu_frame(1, 9);
        assert_eq!(
            e.sentence,
            "innerWork ran for most of the build (90% of samples)."
        );
        assert_eq!(e.functions.len(), 3);
    }

    #[test]
    fn rule_7_without_user_code_takes_the_busiest_function() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let fw = w.intern_function("buildScope", Some(FW));
        let nat = w.intern_function("memcpy", None);
        let leaf = w.intern_stack(&[fw, nat]);
        let samples = (0..4)
            .map(|i| CpuSample {
                time: SessionTime(start + 100_000 * (i + 1)),
                leaf,
            })
            .collect();
        w.push_cpu_samples(run, samples);
        w.set_cpu_coverage(run, SessionTime(start + 100 * MS));
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "memcpy ran for most of the build (100% of samples)."
        );
    }

    #[test]
    fn rule_8_phase() {
        let (mut w, run) = writer();
        put_frame(
            &mut w,
            run,
            1,
            ([2000, 30_000, 500, 250, 250], 33_000),
            3000,
        );
        let e = explain_of(&mut w, 0);
        assert_eq!(e.sentence, "Layout took 30 ms, 1.8× the budget.");
        assert_eq!(e.cause, Cause::Phase(Phase::Layout));

        let (mut w, run) = writer();
        put_frame(&mut w, run, 1, ([20_000, 500, 500, 500, 500], 22_000), 3000);
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "Build took 20 ms, 1.2× the budget. Turn on Trace widget builds to see which widgets rebuilt."
        );

        let (mut w, run) = writer();
        put_frame(&mut w, run, 1, ([1000, 500, 500, 500, 500], 25_000), 3000);
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "UI work outside build, layout and paint took 22 ms, 1.3× the budget."
        );
    }

    #[test]
    fn rules_are_ordered() {
        // Raster beats GC.
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 40_000);
        let t = tracks(&mut w, run);
        put(
            &mut w,
            t.worker,
            0,
            "CollectNewGeneration",
            start,
            2000,
            8000,
            SliceCat::Gc,
        );
        assert_eq!(
            explain_of(&mut w, 0).sentence,
            "Raster took 40 ms, 2.4× the budget."
        );
        // GC beats a dominant widget.
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        heavy_build(&mut w, &t, start);
        put(
            &mut w,
            t.worker,
            0,
            "CollectNewGeneration",
            start,
            14_000,
            8000,
            SliceCat::Gc,
        );
        assert!(matches!(explain_of(&mut w, 0).cause, Cause::Gc { .. }));
        // A dominant widget beats the rebuilt roots (both match), and rebuilt beats a hot
        // function.
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let t = tracks(&mut w, run);
        heavy_build(&mut w, &t, start);
        let e = explain_of(&mut w, 0);
        assert!(matches!(e.cause, Cause::WidgetBuild { .. }));
        assert!(!e.rebuilds.unwrap().roots.is_empty());
    }

    #[test]
    fn cpu_state_cases() {
        let state = |setup: &dyn Fn(&mut SessionWriter, RunId, u64)| {
            let (mut w, run) = writer();
            let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
            setup(&mut w, run, start);
            explain_of(&mut w, 0).cpu
        };
        assert_eq!(state(&|_, _, _| {}), CpuState::Pending);
        assert_eq!(
            state(&|w, run, s| w.set_cpu_coverage(run, SessionTime(s + MS))),
            CpuState::Pending,
            "coverage ends before the frame does"
        );
        assert_eq!(
            state(&|w, run, s| w.set_cpu_coverage(run, SessionTime(s + 100 * MS))),
            CpuState::NoSamples
        );
        assert_eq!(
            state(&|w, run, s| {
                w.set_cpu_coverage(run, SessionTime(s + 100 * MS));
                let f = w.intern_function("f", Some(APP));
                let leaf = w.intern_stack(&[f]);
                w.push_cpu_samples(
                    run,
                    vec![CpuSample {
                        time: SessionTime(s + MS),
                        leaf,
                    }],
                );
            }),
            CpuState::Covered
        );
        assert_eq!(
            state(&|w, run, _| w.set_cpu_status(run, CpuStatus::Off("profiler disabled".into()))),
            CpuState::Off("profiler disabled".into())
        );
    }

    #[test]
    fn functions_list_user_code_first_and_is_capped() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, BUILD_OVER, 3000);
        let mut samples = Vec::new();
        // Framework functions are busier than user ones.
        for i in 0..14u64 {
            let (lib, n) = if i < 7 {
                (FW, 20 - i)
            } else {
                (APP, 5 - i % 5)
            };
            let f = w.intern_function(&format!("fn{i}"), Some(lib));
            let leaf = w.intern_stack(&[f]);
            for k in 0..n {
                samples.push(CpuSample {
                    time: SessionTime(start + 1000 * (samples.len() as u64 + 1) + k),
                    leaf,
                });
            }
        }
        w.push_cpu_samples(run, samples);
        w.set_cpu_coverage(run, SessionTime(start + 100 * MS));
        let s = w.publish();
        let e = explain(&s, 0);
        assert_eq!(e.functions.len(), MAX_FUNCTIONS);
        let user =
            |c: &FunctionCost| is_user_library(s.cpu.function(c.function).library.as_deref());
        let first_fw = e.functions.iter().position(|c| !user(c)).unwrap();
        assert_eq!(first_fw, 7, "all seven user functions come first");
        assert!(e.functions[first_fw..].iter().all(|c| !user(c)));
    }

    /// The Task 12 CartScreen fixture with every widget slice holding 50 us of self time.
    #[test]
    fn spec_example_shape() {
        let (mut w, run) = writer();
        let start = put_frame(
            &mut w,
            run,
            1,
            ([15_000, 2000, 2000, 500, 500], 20_000),
            3000,
        );
        let t = tracks(&mut w, run);
        let d = SliceCat::Dart;
        put(&mut w, t.ui, 0, "VsyncProcessCallback", start, 0, 20_000, d);
        put(&mut w, t.ui, 1, "Animator::BeginFrame", start, 0, 20_000, d);
        put(&mut w, t.ui, 2, "BUILD", start, 0, 15_000, d);
        // Text 50, Card 100, ProductTile 150, ProductGrid 64 * 150 + 50, CartScreen 9,750.
        put(&mut w, t.ui, 3, "CartScreen", start, 0, 9750, d);
        put(&mut w, t.ui, 4, "CartBadge", start, 0, 50, d);
        put(&mut w, t.ui, 4, "ProductGrid", start, 50, 9650, d);
        for i in 0..64u64 {
            let at = 50 + i * 150;
            put(&mut w, t.ui, 5, "ProductTile", start, at, 150, d);
            put(&mut w, t.ui, 6, "Card", start, at, 100, d);
            put(&mut w, t.ui, 7, "Text", start, at, 50, d);
        }
        let e = explain_of(&mut w, 0);
        assert_eq!(
            e.sentence,
            "CartScreen rebuilt 195 widgets, including ProductGrid (193)."
        );
        assert_eq!(e.rebuilds.unwrap().total, 195);
    }

    #[test]
    fn grouping_and_cut_helpers() {
        assert_eq!(group(7), "7");
        assert_eq!(group(1345), "1,345");
        assert_eq!(group(1_000_000), "1,000,000");
        assert_eq!(format_ms(9_940), "9.9 ms");
        assert_eq!(format_ms(10_000), "10 ms");
        assert_eq!(format_ms(12_400), "12 ms");
        let long = "é".repeat(81);
        assert_eq!(cut(&long), format!("{}…", "é".repeat(79)));
        assert_eq!(cut(&"é".repeat(80)), "é".repeat(80));
    }

    #[test]
    fn a_missing_frame_has_a_plain_sentence() {
        let (mut w, _) = writer();
        let e = explain_of(&mut w, 5);
        assert_eq!(e.sentence, "No such frame.");
    }

    /// 100,000 widget builds in a 10 s frame under a 4 KB widget name.
    #[test]
    fn huge_frame_and_long_names() {
        let (mut w, run) = writer();
        let start = put_frame(&mut w, run, 1, ([10_000_000, 0, 0, 0, 0], 10_000_000), 3000);
        let t = tracks(&mut w, run);
        let d = SliceCat::Dart;
        let big = "W".repeat(4096);
        put(
            &mut w,
            t.ui,
            0,
            "VsyncProcessCallback",
            start,
            0,
            10_000_000,
            d,
        );
        put(
            &mut w,
            t.ui,
            1,
            "Animator::BeginFrame",
            start,
            0,
            10_000_000,
            d,
        );
        put(&mut w, t.ui, 2, "BUILD", start, 0, 10_000_000, d);
        put(&mut w, t.ui, 3, &big, start, 0, 10_000_000, d);
        for i in 0..99_999u64 {
            put(&mut w, t.ui, 4, "Child", start, i * 100, 40, d);
        }
        let s = w.publish();
        let t0 = std::time::Instant::now();
        let e = explain(&s, 0);
        let took = t0.elapsed();
        println!("huge_frame_and_long_names: explain took {took:?}");
        assert_eq!(e.rebuilds.as_ref().unwrap().total, 100_000);
        assert!(
            e.sentence
                .starts_with(&format!("{}… took ", "W".repeat(79))),
            "{}",
            &e.sentence[..100]
        );
        assert!(matches!(e.cause, Cause::WidgetBuild { .. }));
        #[cfg(not(debug_assertions))]
        assert!(took.as_millis() <= 50, "explain took {took:?}");
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn explain_budget() {
        #[cfg(not(debug_assertions))]
        {
            let (mut w, run) = fdt_store::synth::stress_writer(42);
            w.set_cpu_coverage(run, SessionTime(u64::MAX / 2));
            w.set_cpu_status(run, CpuStatus::On);
            let s = w.publish();
            let mut order: Vec<usize> = (0..s.frames.len()).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(s.frames.get(i).unwrap().time_us()));
            let mut times: Vec<std::time::Duration> = order[..100]
                .iter()
                .map(|&i| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(explain(&s, i));
                    t.elapsed()
                })
                .collect();
            times.sort();
            let (p50, p99, max) = (times[49], times[98], times[99]);
            println!("explain_budget: 100 slowest frames p50 {p50:?}, p99 {p99:?}, max {max:?}");
            assert!(p99.as_millis() <= 50, "p99 {p99:?}");
        }
    }
}
