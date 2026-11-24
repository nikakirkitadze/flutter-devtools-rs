//! Synthetic heaps and the heap analysis budgets (§10.2): a 5,000,000-object snapshot analysed
//! in at most 3 s, with a peak of at most 80 B per object and a held index of at most 32 B per
//! reachable object. The budget tests run in release; this binary counts allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use fdt_analysis::heap::synth::{self, FLUTTER_VERSION, ROOT_PACKAGE};
use fdt_analysis::heap::{Analysis, AnalyzeInput, analyze, decode_full, encode_full, parse};
use fdt_store::heap::{HeapFile, HeapStage, LeakKind};
use fdt_store::run::Mode;

/// Counts the bytes allocated now and at the peak. Only this test binary uses it.
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn add(bytes: usize) {
    let now = CURRENT.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                add(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The counters are process-wide, so the tests that allocate a lot run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/heap/jank_lab-profile-after-leaks.dartheap"
);

fn input(file: Arc<HeapFile>, mode: Mode) -> AnalyzeInput {
    AnalyzeInput {
        file,
        flutter_version: Some(FLUTTER_VERSION.into()),
        mode,
        root_package: Some(ROOT_PACKAGE.into()),
    }
}

/// Writes `bytes` to a temp file and analyses it.
fn analyse_bytes(bytes: &[u8], mode: Mode) -> (tempfile::TempDir, Analysis) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("synthetic.dartheap");
    std::fs::write(&path, bytes).expect("write");
    let a = analyze(
        &input(Arc::new(HeapFile::new(path)), mode),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .expect("analyze");
    (dir, a)
}

fn dominator_depth(a: &Analysis) -> usize {
    let ix = &a.index;
    let mut depth = vec![0usize; ix.len()];
    let mut seen = vec![false; ix.len()];
    seen[0] = true;
    let mut deepest = 0;
    for start in 0..ix.len() as u32 {
        let mut chain = Vec::new();
        let mut at = start;
        while !seen[at as usize] {
            chain.push(at);
            at = ix.idom(at);
        }
        let mut d = depth[at as usize];
        for &v in chain.iter().rev() {
            d += 1;
            depth[v as usize] = d;
            seen[v as usize] = true;
        }
        deepest = deepest.max(depth[start as usize]);
    }
    deepest
}

fn kinds(a: &Analysis) -> Vec<LeakKind> {
    let mut k: Vec<LeakKind> = a.summary.findings.iter().map(|f| f.kind).collect();
    k.sort_by_key(|k| format!("{k:?}"));
    k
}

#[test]
fn synthetic_shape() {
    let _one_at_a_time = serial();
    let n = 100_000u32;
    for mode in [Mode::Profile, Mode::Debug] {
        let bytes = synth::flutter_heap(7, n, mode);
        assert_eq!(
            bytes,
            synth::flutter_heap(7, n, mode),
            "the same seed gives the same bytes"
        );
        let g = parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .expect("parse");
        assert_eq!(g.object_count(), n as usize);
        let per_object = g.refs.len() as f64 / f64::from(n);
        assert!(
            (4.5..=5.7).contains(&per_object),
            "{per_object} refs per object"
        );

        let (_dir, a) = analyse_bytes(&bytes, mode);
        let s = &a.summary;
        let fraction = s.reachable as f64 / s.objects as f64;
        assert!((0.6..=0.7).contains(&fraction), "reachable {fraction}");
        let depth = dominator_depth(&a);
        assert!(depth >= 200, "dominator depth {depth}");

        let mut found = kinds(&a);
        let mut expected = vec![LeakKind::UnmountedState, LeakKind::DefunctElement];
        if mode == Mode::Debug {
            expected.push(LeakKind::DisposedNotifier);
        }
        expected.sort_by_key(|k| format!("{k:?}"));
        found.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(found, expected, "{mode:?}");
        assert_eq!(s.findings_total as usize, expected.len(), "{mode:?}");
        eprintln!("{mode:?}: {per_object:.2} refs/object, reachable {fraction:.3}, depth {depth}");
    }
}

#[test]
fn replicate_scales() {
    let _one_at_a_time = serial();
    let raw = std::fs::read(FIXTURE).expect("fixture");
    let full = decode_full(&raw).expect("decode");
    let (_d1, one) = analyse_bytes(&raw, Mode::Profile);
    let copies = replicate_bytes(&full, 3);
    let (_d3, three) = analyse_bytes(&copies, Mode::Profile);
    assert_eq!(three.summary.objects, 3 * one.summary.objects - 2);
    assert_eq!(three.summary.findings_total, 3 * one.summary.findings_total);
    assert!(one.summary.findings_total >= 1);
    assert_eq!(
        three.summary.reachable_bytes + three.summary.unreachable_bytes,
        3 * (one.summary.reachable_bytes + one.summary.unreachable_bytes)
    );
}

fn replicate_bytes(full: &fdt_analysis::heap::FullSnapshot, copies: u32) -> Vec<u8> {
    encode_full(&synth::replicate(full, copies))
}

/// What one analysis cost: wall time, the time of each stage, and the allocation peak above
/// what was live when it started.
struct Timing {
    total_ms: f64,
    /// Parse, reachability and the leak scan, dominators, the index and the findings.
    stage_ms: [f64; 3],
    peak: usize,
    /// The allocation peak above the start of each stage, and the bytes live as it began.
    stage_peaks: [usize; 3],
    stage_live: [usize; 3],
}

fn run_once(file: &Arc<HeapFile>) -> (Timing, Analysis) {
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let started = Instant::now();
    let mut marks: Vec<(u8, Instant)> = Vec::new();
    let mut stage_peaks = [0usize; 3];
    let mut stage_live = [0usize; 3];
    let mut stage = 0usize;
    let a = analyze(
        &input(file.clone(), Mode::Profile),
        &AtomicBool::new(false),
        &mut |s| {
            let k = match s {
                HeapStage::Reachability => 1,
                HeapStage::Dominators => 2,
                _ => return,
            };
            marks.push((k, Instant::now()));
            // Close the stage that just ended and start measuring the next from what is live.
            stage_peaks[stage] = PEAK.load(Relaxed) - base;
            stage = k as usize;
            stage_live[stage] = CURRENT.load(Relaxed) - base;
            PEAK.store(CURRENT.load(Relaxed), Relaxed);
        },
    )
    .expect("analyze");
    let done = Instant::now();
    stage_peaks[stage] = PEAK.load(Relaxed) - base;
    let peak = stage_peaks.into_iter().max().unwrap_or(0);
    let ms = |from: Instant, to: Instant| to.duration_since(from).as_secs_f64() * 1e3;
    let at = |k: u8| marks.iter().find(|m| m.0 == k).map_or(done, |m| m.1);
    let timing = Timing {
        total_ms: ms(started, done),
        stage_ms: [ms(started, at(1)), ms(at(1), at(2)), ms(at(2), done)],
        peak,
        stage_peaks,
        stage_live,
    };
    (timing, a)
}

/// Writes `bytes` to a file, drops them, and analyses the file three times. Prints one JSON
/// line, then checks the budgets: time (p50 of 3) through `budget::check`, bytes as asserts.
fn budget(label: &str, bytes: Vec<u8>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("budget.dartheap");
    std::fs::write(&path, &bytes).expect("write");
    let file_bytes = bytes.len();
    drop(bytes);
    // Dropping the last handle deletes the file, so all three runs share one.
    let file = Arc::new(HeapFile::new(path));

    let mut timings: Vec<Timing> = Vec::new();
    let mut last: Option<Analysis> = None;
    for _ in 0..3 {
        // The previous result is dropped before the peak counter is reset.
        drop(last.take());
        let (t, a) = run_once(&file);
        timings.push(t);
        last = Some(a);
    }
    let a = last.expect("three runs");
    let objects = a.summary.objects;
    let reachable = a.summary.reachable;
    let held = a.index.heap_bytes();
    let peak = timings.iter().map(|t| t.peak).max().unwrap();
    let mut by_time: Vec<&Timing> = timings.iter().collect();
    by_time.sort_by(|x, y| x.total_ms.total_cmp(&y.total_ms));
    let p50 = by_time[1];
    println!(
        "{{\"test\":\"{label}\",\"objects\":{objects},\"reachable\":{reachable},\
         \"file_bytes\":{file_bytes},\"parse_ms\":{:.0},\"reach_leaks_ms\":{:.0},\
         \"dominators_index_ms\":{:.0},\"total_ms\":{:.0},\"peak_bytes\":{peak},\
         \"peak_per_object\":{:.1},\"held_bytes\":{held},\"held_per_reachable\":{:.2},\
         \"findings\":{},\"stage_peak_bytes\":{:?},\"stage_start_live_bytes\":{:?}}}",
        p50.stage_ms[0],
        p50.stage_ms[1],
        p50.stage_ms[2],
        p50.total_ms,
        peak as f64 / objects as f64,
        held as f64 / reachable as f64,
        a.summary.findings_total,
        p50.stage_peaks,
        p50.stage_live,
    );

    fdt_store::budget::check(p50.total_ms <= 3_000.0, || {
        format!(
            "{label}: analysing {objects} objects took {:.0} ms (limit 3000)",
            p50.total_ms
        )
    });
    assert!(
        peak as u64 <= 80 * objects,
        "{label}: peak {peak} B is {:.1} B per object (limit 80)",
        peak as f64 / objects as f64
    );
    assert!(
        held as u64 <= 32 * reachable,
        "{label}: held index {held} B is {:.2} B per reachable object (limit 32)",
        held as f64 / reachable as f64
    );
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn heap_budget_5m_synthetic() {
    let _one_at_a_time = serial();
    budget(
        "heap_budget_5m_synthetic",
        synth::flutter_heap(1, 5_000_000, Mode::Profile),
    );
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn heap_budget_5m_replicated() {
    let _one_at_a_time = serial();
    let raw = std::fs::read(FIXTURE).expect("fixture");
    let bytes = {
        let full = decode_full(&raw).expect("decode");
        drop(raw);
        replicate_bytes(&full, 24)
    };
    budget("heap_budget_5m_replicated", bytes);
}
