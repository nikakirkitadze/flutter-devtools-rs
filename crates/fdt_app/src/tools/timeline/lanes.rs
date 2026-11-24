//! The Timeline page's lane layouts: pure functions from a snapshot and a viewport to quads,
//! labels and hit targets in lane-local pixels (x from the lane's left edge, y from its top).
//!
//! Every lane draws at most one quad per pixel column (cells of a level of detail series), or
//! one per record when few records are in view, and never more than the quad budget it is
//! given. The view paints each lane in a layer of its own.

use crate::nav::{Location, Page, Selection, Tool};
use crate::shell::strip::format_offset;
use crate::tools::network::model::{format_duration_ns, status_text};
use crate::tools::performance::trace_tab::Rect;
use fdt_store::Snapshot;
use fdt_store::frames::{FrameStatus, StripBar, frame_status, lod_cells};
use fdt_store::lanes::lane_cells;
use fdt_store::log_query::{LogFilter, filter_logs};
use fdt_store::net_query::{NetFilter, filter_requests};
use fdt_store::time::{SessionTime, TimeRange};
use fdt_ui::canvas::CANVAS_MAX_LABELS;
use fdt_ui::tokens::{Color, Palette, layout};
use fdt_ui::viewport::Viewport;
use std::sync::Arc;

/// Rows the request spans are packed into.
pub const SPAN_ROWS: usize = 3;
/// The most requests (by the lane's cells) for which spans are drawn instead of cells.
pub const SPAN_LIMIT: u64 = 2_000;
/// How far from a marker, in pixels, a pointer still hits it.
const HIT_SLOP_PX: f32 = 2.0; // token-check: allow: pointer tolerance, not a visual size
/// The width of a marker (a navigation or finding).
const MARKER_PX: f32 = 2.0; // token-check: allow: marker width, not a visual size
/// The room between a marker and its label.
const LABEL_GAP_PX: f32 = 4.0; // token-check: allow: label gap, not a visual size

/// The lanes, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaneKind {
    Frames,
    Network,
    Logs,
    Navigation,
    Gc,
    Findings,
}

impl LaneKind {
    pub const ALL: [LaneKind; 6] = [
        LaneKind::Frames,
        LaneKind::Network,
        LaneKind::Logs,
        LaneKind::Navigation,
        LaneKind::Gc,
        LaneKind::Findings,
    ];

    /// The name in the label column.
    pub fn label(self) -> &'static str {
        match self {
            LaneKind::Frames => "Frames",
            LaneKind::Network => "Network",
            LaneKind::Logs => "Logs",
            LaneKind::Navigation => "Navigation",
            LaneKind::Gc => "GC",
            LaneKind::Findings => "Findings",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            LaneKind::Frames => "frames",
            LaneKind::Network => "network",
            LaneKind::Logs => "logs",
            LaneKind::Navigation => "navigation",
            LaneKind::Gc => "gc",
            LaneKind::Findings => "findings",
        }
    }

    /// Whether the lane has anything to show. The Frames lane is always shown.
    pub fn has_data(self, snap: &Snapshot) -> bool {
        match self {
            LaneKind::Frames => true,
            LaneKind::Network => !snap.net.is_empty(),
            LaneKind::Logs => !snap.logs.is_empty(),
            LaneKind::Navigation => !snap.logs.navigation().is_empty(),
            LaneKind::Gc => [snap.memory.gc_lod(), snap.timeline.gc_lod()]
                .iter()
                .any(|lod| lod.level(0).is_some_and(|l| !l.is_empty())),
            // The session's worst frame, from the coarsest cells: no frame is read.
            LaneKind::Findings => {
                let all = TimeRange::new(SessionTime(0), SessionTime(u64::MAX));
                let janky = 2 * u64::from(snap.latest_budget_us());
                lod_cells(snap.frames.lod(), all, 1)
                    .iter()
                    .any(|c| u64::from(c.max_us) > janky)
            }
        }
    }
}

/// What a point on a lane is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneHit {
    /// A frame, by index into the frames.
    Frame(usize),
    /// A pixel column of the Findings lane holding at least one janky frame; the view looks
    /// its worst frame up for the point under the pointer only.
    FindingCell(TimeRange),
    /// One request, by seq.
    Request(u32),
    /// The requests of a pixel column of the request lane.
    RequestCell(TimeRange),
    /// The log lines of a pixel column.
    LogCell(TimeRange),
    /// A navigation record, by log seq.
    Navigation(u32),
    /// The garbage collections of a pixel column.
    Gc(TimeRange),
    /// A janky frame, by index into the frames.
    Finding(usize),
    /// The frames of a pixel column; the page looks up the worst of them under the pointer
    /// ([`worst_frame`]), so the layout never reads individual frames in cell mode.
    FrameCell(TimeRange),
}

/// What a quad is coloured by; the view maps it to a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Status(FrameStatus),
    Network,
    Danger,
    Warning,
    Muted,
    Accent,
}

/// The colour of a quad.
pub fn hue_colour(t: &Palette, hue: Hue) -> Color {
    match hue {
        Hue::Status(FrameStatus::Ok) => t.success,
        Hue::Status(FrameStatus::Slow) | Hue::Warning => t.warning,
        Hue::Status(FrameStatus::Janky) | Hue::Danger => t.danger,
        Hue::Network => t.lane_network,
        Hue::Muted => t.text_muted,
        Hue::Accent => t.accent,
    }
}

/// The colour of a marker's label: text on the lane's own background, never on a marker.
pub fn label_colour(t: &Palette) -> Color {
    t.text
}

/// The colour of a lane's name in the label column.
pub fn name_colour(t: &Palette) -> Color {
    t.text_muted
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneQuad {
    pub rect: Rect,
    pub hue: Hue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaneLabel {
    pub rect: Rect,
    pub text: String,
}

/// A lane laid out in pixels from its top-left corner.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneGeometry {
    pub quads: Vec<LaneQuad>,
    pub labels: Vec<LaneLabel>,
    pub hits: Vec<(Rect, LaneHit)>,
}

/// One request, packed for drawing by the background job that fetched it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedSpan {
    pub start: SessionTime,
    pub end: SessionTime,
    pub seq: u32,
    /// The row it was packed into, below [`SPAN_ROWS`].
    pub row: u8,
    pub error: bool,
}

/// Requests fetched and packed in the background for the request lane's spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spans {
    /// What was asked for: every request overlapping this range.
    pub range: TimeRange,
    /// Sorted by start; never more than `SPAN_CAP`.
    pub items: Arc<[PackedSpan]>,
    /// A request was still pending: its span ends at the fetch's latest time, which moves on.
    pub pending: bool,
}

/// The most spans a fetch keeps, so drawing them is bounded by this and not by the data.
pub const SPAN_CAP: usize = 2 * SPAN_LIMIT as usize;

#[cfg(test)]
thread_local! {
    /// Frames the lane layouts read one by one (the raw bar path, the findings).
    pub(super) static FRAME_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn read_frame(snap: &Snapshot, i: usize) -> Option<&fdt_store::frames::FrameRecord> {
    #[cfg(test)]
    FRAME_READS.with(|c| c.set(c.get() + 1));
    snap.frames.get(i)
}

fn lane_h() -> f32 {
    f32::from(layout::LANE_HEIGHT)
}

fn pad() -> f32 {
    f32::from(layout::BORDER)
}

fn range_of(vp: Viewport) -> TimeRange {
    let (a, z) = vp.range();
    TimeRange::new(SessionTime(a), SessionTime(z))
}

/// The cell of `c` as a rectangle that spans `top..top + h`, at least one pixel wide.
fn cell_rect(vp: Viewport, width: f32, c: &StripBar, top: f32, h: f32) -> Option<Rect> {
    let x0 = vp.x_of(c.start.0, width).max(0.0);
    let x1 = vp.x_of(c.end.0, width).min(width);
    if !(x0.is_finite() && x1.is_finite()) || x1 < x0 || x0 > width {
        return None;
    }
    let w = (x1 - x0).max(1.0).min(width - x0).max(f32::MIN_POSITIVE);
    Some(Rect {
        x: x0,
        y: top,
        w,
        h,
    })
}

/// The time range a cell covers, inclusive at both ends.
fn cell_range(c: &StripBar) -> TimeRange {
    TimeRange::new(
        c.start,
        SessionTime(c.end.0.saturating_sub(1).max(c.start.0)),
    )
}

/// The slowest frame with a start in `range` (the earliest among equals). Scans the frames of
/// one cell, for one point; layouts never call it.
pub fn worst_frame(snap: &Snapshot, range: TimeRange) -> Option<usize> {
    let mut worst: Option<(usize, u32)> = None;
    for i in snap.frames.range(range) {
        if let Some(f) = snap.frames.get(i)
            && worst.is_none_or(|(_, t)| f.time_us() > t)
        {
            worst = Some((i, f.time_us()));
        }
    }
    worst.map(|(i, _)| i)
}

/// Lays `kind` out in a lane `width` pixels wide, with at most `budget` quads. `spans` are the
/// request lane's fetched requests, if any.
pub fn lane_geometry(
    kind: LaneKind,
    snap: &Snapshot,
    vp: Viewport,
    width: f32,
    budget: usize,
    spans: Option<&Spans>,
) -> LaneGeometry {
    if !(width.is_finite() && width > 0.0) || budget == 0 {
        return LaneGeometry::default();
    }
    match kind {
        LaneKind::Frames => frames_lane(snap, vp, width, budget),
        LaneKind::Network => network_lane(snap, vp, width, budget, spans),
        LaneKind::Logs => logs_lane(snap, vp, width, budget),
        LaneKind::Navigation => navigation_lane(snap, vp, width, budget),
        LaneKind::Gc => gc_lane(snap, vp, width, budget),
        LaneKind::Findings => findings_lane(snap, vp, width, budget),
    }
}

/// Cells of `lod` over the view, one quad each, hued by `hue` and hit as `hit`.
fn cells_lane(
    lod: &fdt_store::frames::LodSnapshot,
    vp: Viewport,
    width: f32,
    budget: usize,
    hue: impl Fn(&StripBar) -> Hue,
    hit: impl Fn(TimeRange) -> LaneHit,
) -> LaneGeometry {
    let mut g = LaneGeometry::default();
    let (top, h) = (pad(), lane_h() - 2.0 * pad());
    for c in lane_cells(lod, range_of(vp), width as usize)
        .iter()
        .take(budget)
    {
        let Some(rect) = cell_rect(vp, width, c, top, h) else {
            continue;
        };
        g.quads.push(LaneQuad { rect, hue: hue(c) });
        g.hits.push((rect, hit(cell_range(c))));
    }
    g
}

/// Bars per frame when each has a pixel or more to itself, else the worst frame of each cell.
/// A bar is as tall as its time over twice the frame budget.
pub fn frames_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    let mut g = LaneGeometry::default();
    let range = range_of(vp);
    let h = lane_h();
    let idx = snap.frames.range(range);
    let bar_h = |time_us: u32, budget_us: u32| {
        let frac = time_us as f32 / (2.0 * budget_us.max(1) as f32);
        (frac.clamp(0.0, 1.0) * h).max(1.0)
    };
    if idx.len() <= (width as usize).min(budget) {
        for i in idx {
            let Some(f) = read_frame(snap, i) else {
                continue;
            };
            let x0 = vp.x_of(f.start.0, width).max(0.0);
            let x1 = vp.x_of(f.end().0, width).min(width);
            if !(x0.is_finite() && x1.is_finite()) || x0 > width {
                continue;
            }
            let w = (x1 - x0).max(1.0).min(width - x0).max(f32::MIN_POSITIVE);
            let b = snap.frame_budget_us(f.run);
            let bh = bar_h(f.time_us(), b);
            let status = frame_status(f.time_us(), b);
            g.quads.push(LaneQuad {
                rect: Rect {
                    x: x0,
                    y: h - bh,
                    w,
                    h: bh,
                },
                hue: Hue::Status(status),
            });
            g.hits.push((
                Rect {
                    x: x0,
                    y: 0.0,
                    w,
                    h,
                },
                LaneHit::Frame(i),
            ));
        }
        return g;
    }
    let b = snap.latest_budget_us();
    for c in lane_cells(snap.frames.lod(), range, width as usize)
        .iter()
        .take(budget)
    {
        let bh = bar_h(c.max_us, b);
        let Some(full) = cell_rect(vp, width, c, 0.0, h) else {
            continue;
        };
        g.quads.push(LaneQuad {
            rect: Rect {
                y: h - bh,
                h: bh,
                ..full
            },
            hue: Hue::Status(frame_status(c.max_us, b)),
        });
        g.hits.push((full, LaneHit::FrameCell(cell_range(c))));
    }
    g
}

/// The range to fetch requests for, so a pan or a small zoom does not need a new fetch: the view
/// and a view's width on each side.
pub fn span_fetch_range(vp: Viewport) -> TimeRange {
    let (a, z) = vp.range();
    TimeRange::new(
        SessionTime(a.saturating_sub(vp.span_ns)),
        SessionTime(z.saturating_add(vp.span_ns)),
    )
}

/// Whether the request lane draws spans: the requests over the whole fetch range (three views
/// wide, so at three times the pixels) are few enough to tell apart.
pub fn wants_spans(snap: &Snapshot, vp: Viewport, width: f32) -> bool {
    if snap.net.is_empty() || !(width.is_finite() && width >= 1.0) {
        return false;
    }
    let total: u64 = lane_cells(snap.net.lod(), span_fetch_range(vp), width as usize * 3)
        .iter()
        .map(|c| u64::from(c.count))
        .sum();
    total <= SPAN_LIMIT
}

/// Fetches the requests overlapping `range` and packs them into rows, one pixel (`ns_per_px`
/// nanoseconds) apart. Runs off the UI thread; the result holds at most `SPAN_CAP` requests.
pub fn fetch_spans(snap: &Snapshot, range: TimeRange, ns_per_px: u64) -> Spans {
    let mut seqs = filter_requests(
        &snap.net,
        &NetFilter {
            range: Some(range),
            ..NetFilter::default()
        },
    );
    seqs.truncate(SPAN_CAP);
    let latest = snap.net.latest();
    let mut pending = false;
    let mut found: Vec<PackedSpan> = seqs
        .into_iter()
        .filter_map(|seq| {
            let r = snap.net.get(seq as usize)?;
            pending |= r.end.is_none();
            Some(PackedSpan {
                start: r.start,
                end: r.end.unwrap_or(latest).max(r.start),
                seq,
                row: 0,
                error: r.is_error(),
            })
        })
        .collect();
    found.sort_by_key(|s| (s.start, s.seq));
    let gap = ns_per_px.max(1);
    let mut ends = [0u64; SPAN_ROWS];
    let mut used = [false; SPAN_ROWS];
    for s in &mut found {
        // The first row that is free, else the one that frees up first.
        let row = (0..SPAN_ROWS)
            .find(|&r| !used[r] || ends[r].saturating_add(gap) <= s.start.0)
            .unwrap_or_else(|| (0..SPAN_ROWS).min_by_key(|&r| ends[r]).unwrap_or(0));
        used[row] = true;
        ends[row] = s.end.0.max(s.start.0 + gap);
        s.row = row as u8;
    }
    Spans {
        range,
        items: found.into(),
        pending,
    }
}

/// Cells of the request lane (red where one holds an error), or, with `spans` (which the page
/// fetches only while few requests are around), one span each in the rows they were packed
/// into. Spans that do not cover the whole view (a fetch is under way) are drawn clipped.
pub fn network_lane(
    snap: &Snapshot,
    vp: Viewport,
    width: f32,
    budget: usize,
    spans: Option<&Spans>,
) -> LaneGeometry {
    if let Some(spans) = spans {
        return spans_lane(vp, width, budget, spans);
    }
    cells_lane(
        snap.net.lod(),
        vp,
        width,
        budget,
        |c| {
            if c.max_us == 1 {
                Hue::Danger
            } else {
                Hue::Network
            }
        },
        LaneHit::RequestCell,
    )
}

/// Clips the packed spans to the view and stops at the budget: bounded by `SPAN_CAP`.
fn spans_lane(vp: Viewport, width: f32, budget: usize, spans: &Spans) -> LaneGeometry {
    let mut g = LaneGeometry::default();
    let range = range_of(vp);
    let gap = pad();
    let row_h = ((lane_h() - gap * (SPAN_ROWS as f32 + 1.0)) / SPAN_ROWS as f32).max(1.0);
    for s in spans
        .items
        .iter()
        .filter(|s| range.overlaps(s.start, s.end))
        .take(budget)
    {
        let x0 = vp.x_of(s.start.0, width).max(0.0);
        let x1 = vp.x_of(s.end.0, width).min(width);
        if !(x0.is_finite() && x1.is_finite()) || x0 > width {
            continue;
        }
        let w = (x1 - x0).max(1.0).min(width - x0).max(f32::MIN_POSITIVE);
        let rect = Rect {
            x: x0,
            y: gap + f32::from(s.row.min(SPAN_ROWS as u8 - 1)) * (row_h + gap),
            w,
            h: row_h,
        };
        let hue = if s.error { Hue::Danger } else { Hue::Network };
        g.quads.push(LaneQuad { rect, hue });
        g.hits.push((rect, LaneHit::Request(s.seq)));
    }
    g
}

/// Cells of the log lane, hued by the worst level in each.
pub fn logs_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    cells_lane(
        snap.logs.lod(),
        vp,
        width,
        budget,
        |c| match c.max_us {
            0 | 1 => Hue::Muted,
            2 => Hue::Warning,
            _ => Hue::Danger,
        },
        LaneHit::LogCell,
    )
}

/// The garbage collections the GC lane shows: the memory sampler's GC events when the session
/// has any (they belong to the app's isolates only), else the timeline's GC slices.
pub fn gc_source(snap: &Snapshot) -> &fdt_store::frames::LodSnapshot {
    if snap.memory.gcs().is_empty() {
        snap.timeline.gc_lod()
    } else {
        snap.memory.gc_lod()
    }
}

/// Cells of the garbage collections.
pub fn gc_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    cells_lane(
        gc_source(snap),
        vp,
        width,
        budget,
        |_| Hue::Muted,
        LaneHit::Gc,
    )
}

/// Markers at `xs` (pixels), each with `text` if there is room before the next marker.
/// `xs` ascending; markers in the same pixel column collapse into the first.
fn marker_lane(
    items: impl Iterator<Item = (f32, LaneHit, String)>,
    width: f32,
    budget: usize,
) -> LaneGeometry {
    let mut g = LaneGeometry::default();
    let (top, h) = (pad(), lane_h() - 2.0 * pad());
    let mut markers: Vec<(f32, LaneHit, String)> = Vec::new();
    let mut last_col: Option<i64> = None;
    for (x, hit, text) in items {
        if !x.is_finite() || x < 0.0 || x > width {
            continue;
        }
        let col = x.floor() as i64;
        if last_col == Some(col) {
            continue;
        }
        last_col = Some(col);
        markers.push((x, hit, text));
        if markers.len() >= budget {
            break;
        }
    }
    let min_label = f32::from(layout::LABEL_MIN_WIDTH);
    for (i, (x, hit, text)) in markers.iter().enumerate() {
        let left = (x - MARKER_PX / 2.0).max(0.0);
        let w = MARKER_PX.min(width - left).max(f32::MIN_POSITIVE);
        let rect = Rect {
            x: left,
            y: top,
            w,
            h,
        };
        g.quads.push(LaneQuad {
            rect,
            hue: Hue::Accent,
        });
        g.hits.push((rect, *hit));
        let lx = x + LABEL_GAP_PX;
        let next = markers.get(i + 1).map_or(width, |m| m.0 - LABEL_GAP_PX);
        let room = next - lx;
        if !text.is_empty() && room >= min_label && g.labels.len() < CANVAS_MAX_LABELS {
            g.labels.push(LaneLabel {
                rect: Rect {
                    x: lx,
                    y: top,
                    w: room,
                    h,
                },
                text: text.clone(),
            });
        }
    }
    g
}

/// The first line of a record's text, for a label.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// The indices `lo..hi` taken one in `stride`, where `stride` leaves at most `cap` of them.
fn strided(lo: usize, hi: usize, cap: usize) -> impl Iterator<Item = usize> {
    let stride = (hi.saturating_sub(lo)).div_ceil(cap.max(1)).max(1);
    (lo..hi).step_by(stride)
}

/// A marker per navigation record, labelled with its route where it fits. With more records in
/// view than the lane has pixels, one in every few: bounded by the width, not by the data.
pub fn navigation_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    let nav = snap.logs.navigation();
    let range = range_of(vp);
    let time_of = |i: usize| {
        nav.get(i)
            .and_then(|&s| snap.logs.get_row(s as usize))
            .map(|r| r.time)
    };
    let first = |keep_before: &dyn Fn(SessionTime) -> bool| {
        let (mut lo, mut hi) = (0, nav.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if time_of(mid).is_some_and(keep_before) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let lo = first(&|t| t < range.start);
    let hi = first(&|t| t <= range.end);
    let cap = (width as usize).min(budget).saturating_mul(2);
    let items = strided(lo, hi, cap).filter_map(|i| {
        let seq = *nav.get(i)?;
        let row = snap.logs.get_row(seq as usize)?;
        Some((
            vp.x_of(row.time.0, width),
            LaneHit::Navigation(seq),
            first_line(row.text),
        ))
    });
    marker_lane(items, width, budget)
}

/// A `Jank` marker on every pixel column that holds a janky frame: the frames' level-of-detail
/// cells over the view, kept where the worst frame is over twice the budget. Bounded by the
/// width, never by the data, and never a sample: every frame is in some cell.
pub fn findings_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    let janky = 2 * u64::from(snap.latest_budget_us());
    let cells = lane_cells(snap.frames.lod(), range_of(vp), width as usize);
    let items = cells
        .into_iter()
        .filter(|c| u64::from(c.max_us) > janky)
        .filter_map(|c| {
            let rect = cell_rect(vp, width, &c, 0.0, 1.0)?;
            Some((
                rect.x + rect.w / 2.0,
                LaneHit::FindingCell(cell_range(&c)),
                "Jank".to_string(),
            ))
        });
    marker_lane(items, width, budget)
}

/// The hit at `(x, y)` in a lane, reaching a couple of pixels past a marker.
pub fn hit_at(g: &LaneGeometry, x: f32, y: f32) -> Option<LaneHit> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let outside = |lo: f32, len: f32, p: f32| (lo - p).max(p - (lo + len)).max(0.0);
    let mut best: Option<(f32, LaneHit)> = None;
    for (r, hit) in &g.hits {
        let (dx, dy) = (outside(r.x, r.w, x), outside(r.y, r.h, y));
        if dx > HIT_SLOP_PX || dy > pad() {
            continue;
        }
        let score = dx + dy;
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, *hit));
        }
    }
    best.map(|(_, h)| h)
}

/// What a hit opens, without a search: a record, or (for a garbage collection) the Performance
/// page. The current `range` is kept. `None` for a cell, which [`resolve_cell`] looks into, and
/// for a record that is gone.
pub fn location_for(snap: &Snapshot, hit: &LaneHit, range: Option<TimeRange>) -> Option<Location> {
    let at = |tool, selection| Location {
        page: Page::Tool(tool),
        selection,
        range,
    };
    match *hit {
        LaneHit::Frame(i) | LaneHit::Finding(i) => {
            let f = snap.frames.get(i)?;
            Some(at(Tool::Performance, Some(Selection::Frame(f.id))))
        }
        LaneHit::Request(seq) => {
            let r = snap.net.get(seq as usize)?;
            Some(at(Tool::Network, Some(Selection::Request(r.id))))
        }
        LaneHit::Navigation(seq) => {
            let row = snap.logs.get_row(seq as usize)?;
            Some(at(Tool::Logs, Some(Selection::Log(row.id))))
        }
        LaneHit::Gc(_) => Some(at(Tool::Performance, None)),
        LaneHit::LogCell(_)
        | LaneHit::RequestCell(_)
        | LaneHit::FrameCell(_)
        | LaneHit::FindingCell(_) => None,
    }
}

/// For a cell: the tool and the first record at or after the cell's start, found by a scan of
/// the cell's range. An empty cell still opens the tool. Runs off the UI thread. `None` for a
/// hit that is not a cell.
pub fn resolve_cell(
    snap: &Snapshot,
    hit: &LaneHit,
) -> Option<(Tool, Option<fdt_store::time::EntryId>)> {
    match *hit {
        LaneHit::LogCell(range) => {
            let first = filter_logs(
                &snap.logs,
                &LogFilter {
                    range: Some(range),
                    ..LogFilter::default()
                },
                0,
            )
            .first()
            .and_then(|&seq| snap.logs.get_row(seq as usize))
            .map(|r| r.id);
            Some((Tool::Logs, first))
        }
        LaneHit::RequestCell(range) => {
            let first = filter_requests(
                &snap.net,
                &NetFilter {
                    range: Some(range),
                    ..NetFilter::default()
                },
            )
            .first()
            .and_then(|&seq| snap.net.get(seq as usize))
            .map(|r| r.id);
            Some((Tool::Network, first))
        }
        _ => None,
    }
}

fn status_label(s: FrameStatus) -> &'static str {
    match s {
        FrameStatus::Ok => "OK",
        FrameStatus::Slow => "Slow",
        FrameStatus::Janky => "Janky",
    }
}

/// The hover text of a hit that needs no search. `None` for a cell of requests or log lines
/// ([`cell_tooltip`] counts them), and for a record that is gone.
pub fn tooltip_text(snap: &Snapshot, hit: &LaneHit) -> Option<String> {
    match *hit {
        LaneHit::Frame(i) => {
            let f = snap.frames.get(i)?;
            let status = frame_status(f.time_us(), snap.frame_budget_us(f.run));
            Some(format!(
                "Frame #{} \u{b7} {} \u{b7} {}",
                f.number,
                format_offset(f.start),
                status_label(status)
            ))
        }
        LaneHit::Finding(i) => Some(format!("Janky frame #{}", snap.frames.get(i)?.number)),
        LaneHit::Request(seq) => {
            let r = snap.net.get(seq as usize)?;
            let took = r
                .duration_ns()
                .map_or_else(|| "pending".to_string(), format_duration_ns);
            Some(format!(
                "{} {} \u{b7} {} \u{b7} {}",
                r.method,
                r.path,
                status_text(r),
                took
            ))
        }
        LaneHit::Navigation(seq) => {
            let row = snap.logs.get_row(seq as usize)?;
            Some(first_line(row.text))
        }
        LaneHit::Gc(_)
        | LaneHit::LogCell(_)
        | LaneHit::RequestCell(_)
        | LaneHit::FrameCell(_)
        | LaneHit::FindingCell(_) => None,
    }
}

/// The hover text of a cell of requests, log lines or garbage collections, which counts them in the cell's range. Runs
/// off the UI thread. `None` for any other hit.
pub fn cell_tooltip(snap: &Snapshot, hit: &LaneHit) -> Option<String> {
    match *hit {
        LaneHit::LogCell(range) => {
            let counts = snap.logs.level_counts(Some(range));
            let n: u64 = counts.iter().sum();
            Some(format!(
                "{}, {}",
                plural(n, "line", "lines"),
                plural(counts[3], "error", "errors")
            ))
        }
        LaneHit::RequestCell(range) => {
            let seqs = filter_requests(
                &snap.net,
                &NetFilter {
                    range: Some(range),
                    ..NetFilter::default()
                },
            );
            let failed = seqs
                .iter()
                .filter(|&&s| snap.net.get(s as usize).is_some_and(|r| r.is_error()))
                .count();
            Some(format!(
                "{} ({failed} failed)",
                plural(seqs.len() as u64, "request", "requests")
            ))
        }
        LaneHit::Gc(range) => {
            let cells = lod_cells(gc_source(snap), range, 1);
            let n: u64 = cells.iter().map(|c| u64::from(c.count)).sum();
            let worst_us = cells.iter().map(|c| c.max_us).max().filter(|&us| us > 0);
            let count = plural(n, "garbage collection", "garbage collections");
            Some(match worst_us {
                Some(us) => format!(
                    "{count}, longest {}",
                    format_duration_ns(u64::from(us) * 1_000)
                ),
                None => count,
            })
        }
        _ => None,
    }
}

/// `1 line`, `2 lines`.
fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Page, Selection, Tool};
    use crate::test_support::{frame_at, log_at, request, snapshot};
    use fdt_store::Snapshot;
    use fdt_store::logs::{LogLevel, LogSource};
    use fdt_store::time::{SessionTime, TimeRange};
    use fdt_store::timeline::{Slice, SliceCat, TrackKind};
    use fdt_ui::canvas::CANVAS_MAX_QUADS;
    use fdt_ui::viewport::Viewport;
    use std::sync::Arc;

    const MS: u64 = 1_000_000;

    /// 20k frames, 20k logs (300 of them navigation), 5k requests and 2k GC pauses over a
    /// few minutes.
    fn stress() -> Arc<Snapshot> {
        snapshot(|w, run| {
            for i in 0..20_000u64 {
                w.push_frame(frame_at(run, i + 1, i * 17, 8 + (i % 7) * 5, 6));
            }
            for i in 0..20_000u64 {
                let level = [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Warning,
                    LogLevel::Error,
                ][(i % 4) as usize];
                let mut l = log_at(run, i * 17, level);
                if i % 66 == 0 {
                    l.source = LogSource::Navigation;
                    l.text = format!("/route/{i}");
                }
                w.push_log(l);
            }
            for i in 0..5_000u64 {
                let status = if i % 50 == 0 { 500 } else { 200 };
                request(
                    w,
                    run,
                    "GET",
                    &format!("http://h/p{i}"),
                    Some(status),
                    i * 60,
                    Some(i * 60 + 30),
                );
            }
            let name = w.intern_name("GC");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "gc");
            for i in 0..2_000u64 {
                let slice = Slice {
                    start: SessionTime(i * 150 * MS),
                    dur_ns: 4 * MS,
                    name,
                    cat: SliceCat::Gc,
                };
                w.push_slice(id, 0, slice, false);
            }
        })
    }

    fn full(snap: &Snapshot) -> Viewport {
        Viewport::fit(0, snap.latest_time().0)
    }

    #[test]
    fn lane_layouts_fit_the_width() {
        let snap = stress();
        let vp = full(&snap);
        for width in [300.0_f32, 1400.0] {
            for kind in LaneKind::ALL {
                let g = lane_geometry(kind, &snap, vp, width, CANVAS_MAX_QUADS, None);
                assert!(!g.quads.is_empty(), "{kind:?} drew nothing at {width}");
                assert!(
                    g.quads.len() <= width as usize,
                    "{kind:?}: {} quads in {width} px",
                    g.quads.len()
                );
                assert!(g.hits.len() <= g.quads.len());
                for (r, _) in &g.hits {
                    assert!(r.x.is_finite() && r.w.is_finite() && r.w > 0.0);
                    assert!(r.x >= -1.0 && r.x + r.w <= width + 1.0, "{kind:?} {r:?}");
                }
                for q in &g.quads {
                    let r = q.rect;
                    assert!(r.x >= 0.0 && r.x + r.w <= width + 0.5, "{kind:?} {r:?}");
                    assert!(
                        r.y >= 0.0 && r.y + r.h <= f32::from(fdt_ui::tokens::layout::LANE_HEIGHT)
                    );
                }
            }
        }
        // The budget caps a lane whatever the width.
        let g = lane_geometry(LaneKind::Logs, &snap, vp, 1400.0, 10, None);
        assert_eq!(g.quads.len(), 10);
    }

    #[test]
    fn network_lane_shows_spans_when_sparse() {
        let snap = snapshot(|w, run| {
            for i in 0..40u64 {
                // Overlapping requests, so the spans need more than one row.
                request(
                    w,
                    run,
                    "GET",
                    &format!("http://h/p{i}"),
                    Some(200),
                    i * 100,
                    Some(i * 100 + 250),
                );
            }
        });
        let vp = full(&snap);
        let all = TimeRange::new(SessionTime(0), SessionTime(vp.range().1));
        let spans = fetch_spans(&snap, all, vp.span_ns / 1400);
        let g = lane_geometry(
            LaneKind::Network,
            &snap,
            vp,
            1400.0,
            CANVAS_MAX_QUADS,
            Some(&spans),
        );
        assert_eq!(g.quads.len(), 40);
        assert!(g.hits.iter().all(|(_, h)| matches!(h, LaneHit::Request(_))));
        let mut rows: Vec<i32> = g.quads.iter().map(|q| (q.rect.y * 10.0) as i32).collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(rows.len(), SPAN_ROWS, "packed into three rows");
        // Without the spans (not fetched yet) the lane draws cells.
        let g = lane_geometry(LaneKind::Network, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        assert!(
            g.hits
                .iter()
                .all(|(_, h)| matches!(h, LaneHit::RequestCell(_)))
        );
        // Spans that cover only part of the view (a fetch is under way) are drawn clipped.
        let half = Viewport::fit(0, 2_000 * MS);
        let g = lane_geometry(
            LaneKind::Network,
            &snap,
            half,
            1400.0,
            CANVAS_MAX_QUADS,
            Some(&spans),
        );
        assert!(!g.quads.is_empty() && g.quads.len() < 40);
        assert!(g.quads.iter().all(|q| q.rect.x + q.rect.w <= 1400.5));
    }

    #[test]
    fn dense_requests_stay_cells_and_fetches_are_capped() {
        let snap = stress();
        let vp = full(&snap);
        let all = TimeRange::new(SessionTime(0), SessionTime(vp.range().1));
        assert!(!wants_spans(&snap, vp, 1400.0));
        let g = lane_geometry(LaneKind::Network, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        assert!(
            g.hits
                .iter()
                .all(|(_, h)| matches!(h, LaneHit::RequestCell(_)))
        );
        assert!(
            g.quads.iter().any(|q| q.hue == Hue::Danger),
            "errors show in the cells"
        );
        // Whatever it is asked for, a fetch returns a bounded, sorted, packed set, and drawing
        // it reads no more than that.
        let spans = fetch_spans(&snap, all, vp.span_ns / 1400);
        assert!(spans.items.len() <= SPAN_CAP);
        assert!(spans.items.windows(2).all(|w| w[0].start <= w[1].start));
        assert!(spans.items.iter().all(|s| (s.row as usize) < SPAN_ROWS));
        let g = lane_geometry(LaneKind::Network, &snap, vp, 1400.0, 50, Some(&spans));
        assert_eq!(g.quads.len(), 50);
    }

    #[test]
    fn a_dense_burst_in_the_fetch_margin_turns_spans_off() {
        // 30 requests in the view and 1,000 more in its second half, but another 1,500 in the
        // margin after it, which the fetch also reads.
        let snap = snapshot(|w, run| {
            for i in 0..30u64 {
                request(
                    w,
                    run,
                    "GET",
                    "http://h/a",
                    Some(200),
                    i * 30,
                    Some(i * 30 + 5),
                );
            }
            for i in 0..2_500u64 {
                request(
                    w,
                    run,
                    "GET",
                    "http://h/b",
                    Some(200),
                    1_000 + i,
                    Some(1_010 + i),
                );
            }
        });
        let vp = Viewport::fit(0, 2_000 * MS);
        let in_view: u64 = lane_cells(snap.net.lod(), range_of(vp), 1000)
            .iter()
            .map(|c| u64::from(c.count))
            .sum();
        assert!(
            in_view <= SPAN_LIMIT,
            "the view alone would have asked for spans"
        );
        assert!(!wants_spans(&snap, vp, 1000.0));
        // Zoomed in on the sparse part, the fetch range holds few enough.
        assert!(wants_spans(&snap, Viewport::fit(0, 300 * MS), 1000.0));
    }

    #[test]
    fn frames_cells_do_no_per_frame_work() {
        let snap = stress();
        let vp = full(&snap);
        FRAME_READS.with(|c| c.set(0));
        let g = lane_geometry(LaneKind::Frames, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        assert!(!g.hits.is_empty());
        assert!(
            g.hits
                .iter()
                .all(|(_, h)| matches!(h, LaneHit::FrameCell(_)))
        );
        assert_eq!(
            FRAME_READS.with(|c| c.get()),
            0,
            "cells come from the level of detail"
        );
        // A cell's worst frame is looked up on demand, for the one point.
        let LaneHit::FrameCell(r) = g.hits[3].1 else {
            unreachable!()
        };
        let worst = worst_frame(&snap, r).unwrap();
        let t = |i: usize| snap.frames.get(i).unwrap().time_us();
        assert!(snap.frames.range(r).all(|i| t(i) <= t(worst)));
    }

    #[test]
    fn marker_lanes_stop_at_their_cap() {
        // 50k route changes and 50k slow frames in view: the layouts read a few per pixel.
        let snap = snapshot(|w, run| {
            for i in 0..50_000u64 {
                w.push_frame(frame_at(run, i + 1, i * 4, 40, 4));
                let mut l = log_at(run, i * 4, LogLevel::Info);
                l.source = LogSource::Navigation;
                w.push_log(l);
            }
        });
        let vp = full(&snap);
        FRAME_READS.with(|c| c.set(0));
        let f = lane_geometry(LaneKind::Findings, &snap, vp, 800.0, CANVAS_MAX_QUADS, None);
        assert!(!f.quads.is_empty() && f.quads.len() <= 800);
        assert!(
            FRAME_READS.with(|c| c.get()) <= 2 * 800 + 2,
            "reads per pixel, not per frame"
        );
        let n = lane_geometry(
            LaneKind::Navigation,
            &snap,
            vp,
            800.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert!(!n.quads.is_empty() && n.quads.len() <= 800);
        let n = lane_geometry(LaneKind::Navigation, &snap, vp, 800.0, 20, None);
        assert_eq!(n.quads.len(), 20);
    }

    #[test]
    fn hues_follow_the_data() {
        let snap = stress();
        let vp = full(&snap);
        let g = lane_geometry(LaneKind::Logs, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        // Every cell holds an error, so every cell is `danger`.
        assert!(g.quads.iter().all(|q| q.hue == Hue::Danger));
        let g = lane_geometry(LaneKind::Frames, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        assert!(g.quads.iter().all(|q| matches!(q.hue, Hue::Status(_))));
        let g = lane_geometry(LaneKind::Gc, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
        assert!(g.quads.iter().all(|q| q.hue == Hue::Muted));
        let g = lane_geometry(
            LaneKind::Navigation,
            &snap,
            vp,
            1400.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert!(g.quads.iter().all(|q| q.hue == Hue::Accent));
    }

    #[test]
    fn log_levels_pick_the_hue() {
        let one = |level| {
            snapshot(|w, run| {
                w.push_log(log_at(run, 100, level));
            })
        };
        for (level, hue) in [
            (LogLevel::Debug, Hue::Muted),
            (LogLevel::Info, Hue::Muted),
            (LogLevel::Warning, Hue::Warning),
            (LogLevel::Error, Hue::Danger),
        ] {
            let snap = one(level);
            let g = lane_geometry(
                LaneKind::Logs,
                &snap,
                Viewport::fit(0, 1_000 * MS),
                800.0,
                CANVAS_MAX_QUADS,
                None,
            );
            assert_eq!(g.quads.len(), 1);
            assert_eq!(g.quads[0].hue, hue, "{level:?}");
        }
    }

    #[test]
    fn zoomed_in_frames_are_bars_and_findings_are_labelled() {
        let snap = snapshot(|w, run| {
            // 10 ms apart; frame 5 takes 80 ms (janky at 16.7 ms), frame 8 takes 25 ms (slow).
            for i in 0..20u64 {
                let (b, r) = match i {
                    5 => (80, 6),
                    8 => (25, 6),
                    _ => (4, 4),
                };
                w.push_frame(frame_at(run, i + 1, i * 100, b, r));
            }
        });
        let vp = Viewport::fit(0, 2_000 * MS);
        let g = lane_geometry(LaneKind::Frames, &snap, vp, 1000.0, CANVAS_MAX_QUADS, None);
        assert_eq!(g.quads.len(), 20, "one bar per frame");
        assert!(g.hits.iter().all(|(_, h)| matches!(h, LaneHit::Frame(_))));
        let heights: Vec<f32> = g.quads.iter().map(|q| q.rect.h).collect();
        // Height follows time over twice the budget, so the long frame is the tallest.
        assert!(
            heights[5] > heights[8] && heights[8] > heights[0],
            "{heights:?}"
        );
        assert_eq!(
            g.quads[5].hue,
            Hue::Status(fdt_store::frames::FrameStatus::Janky)
        );
        assert_eq!(
            g.quads[8].hue,
            Hue::Status(fdt_store::frames::FrameStatus::Slow)
        );
        assert_eq!(
            g.quads[0].hue,
            Hue::Status(fdt_store::frames::FrameStatus::Ok)
        );
        let f = lane_geometry(
            LaneKind::Findings,
            &snap,
            vp,
            1000.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert_eq!(f.quads.len(), 1, "only the janky frame is a finding");
        let LaneHit::FindingCell(r) = f.hits[0].1 else {
            panic!("{:?}", f.hits[0].1)
        };
        assert_eq!(worst_frame(&snap, r), Some(5));
        assert_eq!(f.labels.len(), 1);
        assert_eq!(f.labels[0].text, "Jank");
    }

    /// Reviewer C's probe: 10 minutes at 120 Hz, fitted into 1,400 px. Every pixel column with
    /// a janky frame is marked, and only those.
    #[test]
    fn findings_mark_every_column_with_a_janky_frame() {
        // Every 8th frame slow (12 ms against 8.3), every 97th janky (30 ms).
        let snap = snapshot(|w, run| {
            w.set_refresh_rate(run, 120.0);
            for i in 0..72_000u64 {
                let ms = if i % 97 == 0 {
                    30
                } else if i % 8 == 0 {
                    12
                } else {
                    5
                };
                w.push_frame(frame_at(run, i + 1, i * 1000 / 120, ms, 2));
            }
        });
        let width = 1400.0_f32;
        let vp = Viewport::fit(0, snap.latest_time().0);
        let g = lane_geometry(LaneKind::Findings, &snap, vp, width, CANVAS_MAX_QUADS, None);
        assert!(g.quads.len() <= width as usize, "bounded by the width");
        // The time each marker stands for.
        let marked: Vec<TimeRange> = g
            .hits
            .iter()
            .map(|(_, h)| match *h {
                LaneHit::FindingCell(r) => r,
                LaneHit::Finding(i) => {
                    let t = snap.frames.get(i).unwrap().start;
                    TimeRange::new(t, t)
                }
                other => panic!("{other:?}"),
            })
            .collect();
        let budget = snap.frame_budget_us(snap.runs[0].id);
        let mut janky = 0;
        for i in 0..snap.frames.len() {
            let f = snap.frames.get(i).unwrap();
            if frame_status(f.time_us(), budget) == FrameStatus::Janky {
                janky += 1;
                assert!(
                    marked
                        .iter()
                        .any(|r| r.start <= f.start && f.start <= r.end),
                    "janky frame {i} at {:?} is not marked ({} markers for {janky} so far)",
                    f.start,
                    marked.len()
                );
            }
        }
        // Each marker holds a janky frame.
        for r in &marked {
            let worst = worst_frame(&snap, *r).unwrap();
            let f = snap.frames.get(worst).unwrap();
            assert_eq!(frame_status(f.time_us(), budget), FrameStatus::Janky);
        }
    }

    /// Cell tooltips are plain English: "1 line", not "1 lines".
    #[test]
    fn cell_tooltips_pluralise() {
        let all = TimeRange::new(SessionTime(0), SessionTime(10_000 * MS));
        let one = snapshot(|w, run| {
            w.push_log(log_at(run, 10, LogLevel::Info));
            request(w, run, "GET", "http://h/a", Some(200), 10, Some(20));
        });
        assert_eq!(
            cell_tooltip(&one, &LaneHit::LogCell(all)).as_deref(),
            Some("1 line, 0 errors")
        );
        assert_eq!(
            cell_tooltip(&one, &LaneHit::RequestCell(all)).as_deref(),
            Some("1 request (0 failed)")
        );
        let two = snapshot(|w, run| {
            w.push_log(log_at(run, 10, LogLevel::Info));
            w.push_log(log_at(run, 20, LogLevel::Error));
            request(w, run, "GET", "http://h/a", Some(500), 10, Some(20));
            request(w, run, "GET", "http://h/b", Some(200), 30, Some(40));
        });
        assert_eq!(
            cell_tooltip(&two, &LaneHit::LogCell(all)).as_deref(),
            Some("2 lines, 1 error")
        );
        assert_eq!(
            cell_tooltip(&two, &LaneHit::RequestCell(all)).as_deref(),
            Some("2 requests (1 failed)")
        );
    }

    #[test]
    fn navigation_markers_carry_the_route() {
        let snap = snapshot(|w, run| {
            for (ms, route) in [(100, "/home"), (600, "/settings"), (620, "/settings/about")] {
                let mut l = log_at(run, ms, LogLevel::Info);
                l.source = LogSource::Navigation;
                l.text = route.into();
                w.push_log(l);
            }
            w.push_log(log_at(run, 300, LogLevel::Info));
        });
        let vp = Viewport::fit(0, 1_000 * MS);
        let g = lane_geometry(
            LaneKind::Navigation,
            &snap,
            vp,
            1000.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert_eq!(g.quads.len(), 3);
        assert_eq!(
            g.hits.iter().map(|(_, h)| *h).collect::<Vec<_>>(),
            vec![
                LaneHit::Navigation(0),
                LaneHit::Navigation(1),
                LaneHit::Navigation(2)
            ]
        );
        // The first has room for its route; the second is 20 px from the third and has none.
        let texts: Vec<&str> = g.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["/home", "/settings/about"]);
        // A label never reaches the next marker.
        let min = f32::from(fdt_ui::tokens::layout::LABEL_MIN_WIDTH);
        for l in &g.labels {
            assert!(l.rect.w >= min);
            assert!(
                g.quads
                    .iter()
                    .all(|q| q.rect.x < l.rect.x || q.rect.x >= l.rect.x + l.rect.w)
            );
        }
    }

    #[test]
    fn hit_testing_reaches_a_pixel_either_side() {
        let snap = snapshot(|w, run| {
            for ms in [100, 500] {
                let mut l = log_at(run, ms, LogLevel::Info);
                l.source = LogSource::Navigation;
                w.push_log(l);
            }
        });
        let vp = Viewport::fit(0, 1_000 * MS);
        let g = lane_geometry(
            LaneKind::Navigation,
            &snap,
            vp,
            1000.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert_eq!(hit_at(&g, 100.0, 4.0), Some(LaneHit::Navigation(0)));
        assert_eq!(hit_at(&g, 102.0, 4.0), Some(LaneHit::Navigation(0)));
        assert_eq!(hit_at(&g, 300.0, 4.0), None);
        assert_eq!(hit_at(&g, f32::NAN, 4.0), None);
    }

    #[test]
    fn locations_keep_the_range() {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 100, 40, 4));
            request(w, run, "GET", "http://h/a", Some(200), 50, Some(80));
            let mut l = log_at(run, 70, LogLevel::Info);
            l.source = LogSource::Navigation;
            w.push_log(l);
        });
        let range = Some(TimeRange::new(SessionTime(0), SessionTime(500 * MS)));
        let at = |hit| location_for(&snap, &hit, range).expect("a location");
        let frame = snap.frames.get(0).unwrap().id;
        let want = |page, selection| crate::nav::Location {
            page,
            selection,
            range,
        };
        assert_eq!(
            at(LaneHit::Frame(0)),
            want(Page::Tool(Tool::Performance), Some(Selection::Frame(frame)))
        );
        assert_eq!(
            at(LaneHit::Finding(0)),
            want(Page::Tool(Tool::Performance), Some(Selection::Frame(frame)))
        );
        let req = snap.net.request(0).id;
        assert_eq!(
            at(LaneHit::Request(0)),
            want(Page::Tool(Tool::Network), Some(Selection::Request(req)))
        );
        let log = snap.logs.row(0).id;
        assert_eq!(
            at(LaneHit::Navigation(0)),
            want(Page::Tool(Tool::Logs), Some(Selection::Log(log)))
        );
        let cell = TimeRange::new(SessionTime(0), SessionTime(10 * MS));
        assert_eq!(
            at(LaneHit::Gc(cell)),
            want(Page::Tool(Tool::Performance), None)
        );
        // A cell has no one record: it is found in the background.
        assert!(location_for(&snap, &LaneHit::LogCell(cell), range).is_none());
        assert!(location_for(&snap, &LaneHit::RequestCell(cell), range).is_none());
        // A record that is gone gives nothing.
        assert!(location_for(&snap, &LaneHit::Frame(9), range).is_none());
    }

    #[test]
    fn cells_resolve_to_their_first_record() {
        let snap = snapshot(|w, run| {
            for ms in [10, 20, 900] {
                w.push_log(log_at(run, ms, LogLevel::Info));
            }
            request(w, run, "GET", "http://h/a", Some(200), 30, Some(40));
            request(w, run, "GET", "http://h/b", Some(200), 50, Some(60));
        });
        let cell = TimeRange::new(SessionTime(15 * MS), SessionTime(100 * MS));
        let (tool, id) = resolve_cell(&snap, &LaneHit::LogCell(cell)).unwrap();
        assert_eq!((tool, id), (Tool::Logs, Some(snap.logs.row(1).id)));
        let (tool, id) = resolve_cell(&snap, &LaneHit::RequestCell(cell)).unwrap();
        assert_eq!((tool, id), (Tool::Network, Some(snap.net.request(0).id)));
        // An empty cell still opens the tool.
        let empty = TimeRange::new(SessionTime(500 * MS), SessionTime(600 * MS));
        assert_eq!(
            resolve_cell(&snap, &LaneHit::LogCell(empty)),
            Some((Tool::Logs, None))
        );
        assert!(resolve_cell(&snap, &LaneHit::Frame(0)).is_none());
    }

    #[test]
    fn tooltip_texts() {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 7, 61_000, 40, 4));
            request(
                w,
                run,
                "GET",
                "http://h/api/items",
                Some(200),
                50,
                Some(232),
            );
            let mut l = log_at(run, 30, LogLevel::Info);
            l.source = LogSource::Navigation;
            l.text = "/checkout\nsecond line".into();
            w.push_log(l);
            w.push_log(log_at(run, 70, LogLevel::Info));
            let name = w.intern_name("GC");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "gc");
            let slice = Slice {
                start: SessionTime(10 * MS),
                dur_ns: 4 * MS,
                name,
                cat: SliceCat::Gc,
            };
            w.push_slice(id, 0, slice, false);
        });
        let tip = |hit| tooltip_text(&snap, &hit);
        assert_eq!(
            tip(LaneHit::Frame(0)).as_deref(),
            Some("Frame #7 \u{b7} 1:01 \u{b7} Janky")
        );
        assert_eq!(tip(LaneHit::Finding(0)).as_deref(), Some("Janky frame #7"));
        assert_eq!(
            tip(LaneHit::Request(0)).as_deref(),
            Some("GET /api/items \u{b7} 200 \u{b7} 182 ms")
        );
        assert_eq!(tip(LaneHit::Navigation(0)).as_deref(), Some("/checkout"));
        let cell = TimeRange::new(SessionTime(0), SessionTime(100 * MS));
        assert_eq!(tip(LaneHit::Gc(cell)).as_deref(), None);
        assert_eq!(
            cell_tooltip(&snap, &LaneHit::Gc(cell)).as_deref(),
            Some("1 garbage collection, longest 4 ms")
        );
        // Cells need a count of their records, which is done in the background.
        assert_eq!(tip(LaneHit::LogCell(cell)), None);
        assert_eq!(tip(LaneHit::RequestCell(cell)), None);
        assert_eq!(
            cell_tooltip(&snap, &LaneHit::LogCell(cell)).as_deref(),
            Some("2 lines, 0 errors")
        );
        assert_eq!(
            cell_tooltip(&snap, &LaneHit::RequestCell(cell)).as_deref(),
            Some("1 request (0 failed)")
        );
        assert_eq!(tip(LaneHit::Frame(5)), None);
    }

    #[test]
    fn gc_lane_prefers_gc_records_and_falls_back() {
        use fdt_store::memory::{GcKind, GcReason, GcRecord};
        let slice_only = snapshot(|w, run| {
            let name = w.intern_name("GC");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "gc");
            let slice = Slice {
                start: SessionTime(10 * MS),
                dur_ns: 4 * MS,
                name,
                cat: SliceCat::Gc,
            };
            w.push_slice(id, 0, slice, false);
        });
        let record = GcRecord {
            time: SessionTime(2_000 * MS),
            dur_us: Some(7_000),
            kind: GcKind::Scavenge,
            reason: GcReason::NewSpace,
            heap_used: 1,
            heap_capacity: 2,
            external: 0,
        };
        let records_only = snapshot(|w, run| w.push_gc(run, record));
        let both = snapshot(|w, run| {
            let name = w.intern_name("GC");
            let id = w.timeline_track(run, TrackKind::Thread, 1, "gc");
            let slice = Slice {
                start: SessionTime(10 * MS),
                dur_ns: 4 * MS,
                name,
                cat: SliceCat::Gc,
            };
            w.push_slice(id, 0, slice, false);
            w.push_gc(run, record);
        });
        let vp = Viewport::fit(0, 3_000 * MS);
        let quads = |snap: &Snapshot| lane_geometry(LaneKind::Gc, snap, vp, 300.0, 100, None).quads;
        // A session with only timeline slices still shows the lane, as M3 did.
        assert!(LaneKind::Gc.has_data(&slice_only));
        assert_eq!(quads(&slice_only).len(), 1);
        assert!(quads(&slice_only)[0].rect.x < 5.0);
        assert!(LaneKind::Gc.has_data(&records_only));
        assert!(!LaneKind::Gc.has_data(&snapshot(|_, _| {})));
        // With records, the lane is theirs alone: the slice at 10 ms is not drawn.
        let q = quads(&both);
        assert_eq!(q.len(), 1);
        assert!((q[0].rect.x - 200.0).abs() < 2.0, "{:?}", q[0].rect);
        // The tooltip counts the records of the cell and names the longest.
        let cell = TimeRange::new(SessionTime(0), SessionTime(3_000 * MS));
        assert_eq!(
            cell_tooltip(&both, &LaneHit::Gc(cell)).as_deref(),
            Some("1 garbage collection, longest 7 ms")
        );
        assert_eq!(
            cell_tooltip(&slice_only, &LaneHit::Gc(cell)).as_deref(),
            Some("1 garbage collection, longest 4 ms")
        );
    }

    #[test]
    fn lanes_without_data_are_hidden_except_frames() {
        let empty = snapshot(|_, _| {});
        for kind in LaneKind::ALL {
            assert_eq!(kind.has_data(&empty), kind == LaneKind::Frames, "{kind:?}");
        }
        let snap = stress();
        for kind in LaneKind::ALL {
            assert!(kind.has_data(&snap), "{kind:?}");
        }
        // Slow frames are not findings: only janky ones are.
        let slow = snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 0, 25, 4));
        });
        assert!(!LaneKind::Findings.has_data(&slow));
        let janky = snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 0, 60, 4));
        });
        assert!(LaneKind::Findings.has_data(&janky));
    }

    #[test]
    fn label_colours_meet_contrast() {
        for (name, p) in [
            ("dark", &fdt_ui::tokens::DARK),
            ("light", &fdt_ui::tokens::LIGHT),
        ] {
            for bg in [p.bg, p.surface] {
                for (what, c) in [
                    ("marker label", label_colour(p)),
                    ("lane name", name_colour(p)),
                ] {
                    let ratio = c.contrast(bg);
                    assert!(ratio >= 4.5, "{name} {what}: {ratio:.2}");
                }
            }
        }
    }

    #[test]
    fn extreme_viewports_lay_out_finite_geometry() {
        let snap = stress();
        let end = snap.latest_time().0;
        for vp in [
            Viewport {
                start_ns: 5_000 * MS,
                span_ns: 1_000,
            },
            Viewport {
                start_ns: 0,
                span_ns: end * 100,
            },
            Viewport {
                start_ns: end * 10,
                span_ns: 1_000_000,
            },
        ] {
            for kind in LaneKind::ALL {
                let g = lane_geometry(kind, &snap, vp, 1400.0, CANVAS_MAX_QUADS, None);
                for q in &g.quads {
                    let r = q.rect;
                    assert!(
                        r.x.is_finite() && r.w.is_finite() && r.h.is_finite(),
                        "{kind:?} {vp:?} {r:?}"
                    );
                }
            }
        }
        // No width, no geometry.
        let g = lane_geometry(
            LaneKind::Frames,
            &snap,
            full(&snap),
            0.0,
            CANVAS_MAX_QUADS,
            None,
        );
        assert!(g.quads.is_empty());
    }

    /// All six lanes over a big session (72k frames, 100k logs, 50k requests): the layout the
    /// page redoes on every snapshot and every zoom step.
    #[test]
    fn lane_layout_budget() {
        let snap = snapshot(|w, run| {
            for i in 0..72_000u64 {
                w.push_frame(frame_at(run, i + 1, i * 17, 8 + (i % 9) * 5, 6));
            }
            for i in 0..100_000u64 {
                let mut l = log_at(run, i * 5, LogLevel::Info);
                if i % 500 == 0 {
                    l.source = LogSource::Navigation;
                }
                w.push_log(l);
            }
            for i in 0..50_000u64 {
                request(
                    w,
                    run,
                    "GET",
                    "http://h/x",
                    Some(200),
                    i * 25,
                    Some(i * 25 + 10),
                );
            }
        });
        let vp = full(&snap);
        let mut times: Vec<f64> = (0..15)
            .map(|_| {
                let t = std::time::Instant::now();
                for kind in LaneKind::ALL {
                    std::hint::black_box(lane_geometry(
                        kind,
                        &snap,
                        vp,
                        1400.0,
                        CANVAS_MAX_QUADS,
                        None,
                    ));
                }
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        println!(
            "lane layout, 6 lanes, 72k frames: median {median:.2} ms, worst {:.2} ms",
            times[times.len() - 1]
        );
        if !cfg!(debug_assertions) {
            fdt_store::budget::check(median < 8.0, || format!("median {median:.2} ms"));
        }
    }
}
