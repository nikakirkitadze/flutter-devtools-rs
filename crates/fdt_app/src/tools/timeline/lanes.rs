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
            LaneKind::Gc => snap
                .timeline
                .gc_lod()
                .level(0)
                .is_some_and(|l| !l.is_empty()),
            LaneKind::Findings => janky_frames(snap, 0..snap.frames.len()).next().is_some(),
        }
    }
}

/// What a point on a lane is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneHit {
    /// A frame, by index into the frames.
    Frame(usize),
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

/// Requests fetched in the background for the request lane's spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spans {
    /// What was asked for: every request overlapping this range.
    pub range: TimeRange,
    pub seqs: Arc<[u32]>,
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

/// The indices of the janky frames among `frames`, ascending.
fn janky_frames(
    snap: &Snapshot,
    frames: std::ops::Range<usize>,
) -> impl Iterator<Item = usize> + '_ {
    let slow = snap.frames.slow();
    let (mut lo, mut hi) = (0, slow.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if slow.get(mid).is_some_and(|&s| (s as usize) < frames.start) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    (lo..slow.len())
        .map_while(move |i| slow.get(i).map(|&s| s as usize))
        .take_while(move |&s| s < frames.end)
        .filter(|&s| {
            snap.frames.get(s).is_some_and(|f| {
                frame_status(f.time_us(), snap.frame_budget_us(f.run)) == FrameStatus::Janky
            })
        })
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
            let Some(f) = snap.frames.get(i) else {
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
        // The worst frame of the cell, the earliest among equals.
        let mut worst: Option<(usize, u32)> = None;
        for i in snap.frames.range(cell_range(c)) {
            if let Some(f) = snap.frames.get(i)
                && worst.is_none_or(|(_, t)| f.time_us() > t)
            {
                worst = Some((i, f.time_us()));
            }
        }
        if let Some((i, _)) = worst {
            g.hits.push((full, LaneHit::Frame(i)));
        }
    }
    g
}

/// Whether the request lane draws spans for this view, which it does while there are few
/// enough requests in view to tell apart.
pub fn wants_spans(snap: &Snapshot, vp: Viewport, width: f32) -> bool {
    if snap.net.is_empty() || !(width.is_finite() && width >= 1.0) {
        return false;
    }
    let total: u64 = lane_cells(snap.net.lod(), range_of(vp), width as usize)
        .iter()
        .map(|c| u64::from(c.count))
        .sum();
    total <= SPAN_LIMIT
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

/// Fetches the requests overlapping `range`. Runs off the UI thread.
pub fn fetch_spans(snap: &Snapshot, range: TimeRange) -> Spans {
    let seqs = filter_requests(
        &snap.net,
        &NetFilter {
            range: Some(range),
            ..NetFilter::default()
        },
    );
    Spans {
        range,
        seqs: seqs.into(),
    }
}

/// Cells of the request lane (red where one holds an error), or, with few requests in view and
/// `spans` that cover it, one span each packed into `SPAN_ROWS` rows.
pub fn network_lane(
    snap: &Snapshot,
    vp: Viewport,
    width: f32,
    budget: usize,
    spans: Option<&Spans>,
) -> LaneGeometry {
    let range = range_of(vp);
    if let Some(spans) = spans
        && spans.range.start <= range.start
        && spans.range.end >= range.end
        && wants_spans(snap, vp, width)
    {
        return spans_lane(snap, vp, width, budget, spans);
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

fn spans_lane(
    snap: &Snapshot,
    vp: Viewport,
    width: f32,
    budget: usize,
    spans: &Spans,
) -> LaneGeometry {
    let mut g = LaneGeometry::default();
    let range = range_of(vp);
    let latest = snap.net.latest();
    let mut found: Vec<(SessionTime, SessionTime, u32)> = spans
        .seqs
        .iter()
        .filter_map(|&seq| {
            let r = snap.net.get(seq as usize)?;
            let end = r.end.unwrap_or(latest).max(r.start);
            range.overlaps(r.start, end).then_some((r.start, end, seq))
        })
        .collect();
    found.sort_by_key(|&(start, _, seq)| (start, seq));
    let gap = pad();
    let row_h = ((lane_h() - gap * (SPAN_ROWS as f32 + 1.0)) / SPAN_ROWS as f32).max(1.0);
    let mut ends = [f32::NEG_INFINITY; SPAN_ROWS];
    for (start, end, seq) in found.into_iter().take(budget) {
        let x0 = vp.x_of(start.0, width).max(0.0);
        let x1 = vp.x_of(end.0, width).min(width);
        if !(x0.is_finite() && x1.is_finite()) || x0 > width {
            continue;
        }
        let w = (x1 - x0).max(1.0).min(width - x0).max(f32::MIN_POSITIVE);
        // The first row that is free, else the one that frees up first.
        let row = (0..SPAN_ROWS)
            .find(|&r| ends[r] + gap <= x0)
            .unwrap_or_else(|| {
                (0..SPAN_ROWS)
                    .min_by(|&a, &b| ends[a].total_cmp(&ends[b]))
                    .unwrap_or(0)
            });
        ends[row] = x0 + w;
        let rect = Rect {
            x: x0,
            y: gap + row as f32 * (row_h + gap),
            w,
            h: row_h,
        };
        let hue = match snap.net.get(seq as usize) {
            Some(r) if r.is_error() => Hue::Danger,
            _ => Hue::Network,
        };
        g.quads.push(LaneQuad { rect, hue });
        g.hits.push((rect, LaneHit::Request(seq)));
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

/// Cells of the garbage collections.
pub fn gc_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    cells_lane(
        snap.timeline.gc_lod(),
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

/// A marker per navigation record, labelled with its route where it fits.
pub fn navigation_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    let nav = snap.logs.navigation();
    let range = range_of(vp);
    let time_of = |i: usize| {
        nav.get(i)
            .and_then(|&s| snap.logs.get_row(s as usize))
            .map(|r| r.time)
    };
    let (mut lo, mut hi) = (0, nav.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if time_of(mid).is_some_and(|t| t < range.start) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let items = (lo..nav.len()).map_while(|i| {
        let seq = *nav.get(i)?;
        let row = snap.logs.get_row(seq as usize)?;
        (row.time <= range.end).then(|| {
            (
                vp.x_of(row.time.0, width),
                LaneHit::Navigation(seq),
                first_line(row.text),
            )
        })
    });
    marker_lane(items, width, budget)
}

/// A `Jank` marker per janky frame.
pub fn findings_lane(snap: &Snapshot, vp: Viewport, width: f32, budget: usize) -> LaneGeometry {
    let idx = snap.frames.range(range_of(vp));
    let items = janky_frames(snap, idx).filter_map(|i| {
        let f = snap.frames.get(i)?;
        Some((
            vp.x_of(f.start.0, width),
            LaneHit::Finding(i),
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
        LaneHit::LogCell(_) | LaneHit::RequestCell(_) => None,
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
        LaneHit::Gc(range) => {
            let worst_us = lod_cells(snap.timeline.gc_lod(), range, 1)
                .iter()
                .map(|c| c.max_us)
                .max()?;
            Some(format!(
                "Garbage collection \u{b7} {}",
                format_duration_ns(u64::from(worst_us) * 1_000)
            ))
        }
        LaneHit::LogCell(_) | LaneHit::RequestCell(_) => None,
    }
}

/// The hover text of a cell of requests or log lines, which counts them in the cell's range. Runs
/// off the UI thread. `None` for any other hit.
pub fn cell_tooltip(snap: &Snapshot, hit: &LaneHit) -> Option<String> {
    match *hit {
        LaneHit::LogCell(range) => {
            let counts = snap.logs.level_counts(Some(range));
            let n: u64 = counts.iter().sum();
            Some(format!("{n} lines, {} errors", counts[3]))
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
            Some(format!("{} requests ({failed} failed)", seqs.len()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Page, Selection, Tool};
    use crate::test_support::{frame_at, log_at, request, snapshot};
    use fdt_store::Snapshot;
    use fdt_store::logs::{LogLevel, LogSource};
    use fdt_store::net_query::{NetFilter, filter_requests};
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
        let seqs = filter_requests(
            &snap.net,
            &NetFilter {
                range: Some(all),
                ..NetFilter::default()
            },
        );
        let spans = Spans {
            range: all,
            seqs: seqs.into(),
        };
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
        // Spans fetched for a range that does not cover the view are not used either.
        let narrow = Spans {
            range: TimeRange::new(SessionTime(0), SessionTime(1_000 * MS)),
            seqs: Arc::from(vec![0u32]),
        };
        let g = lane_geometry(
            LaneKind::Network,
            &snap,
            vp,
            1400.0,
            CANVAS_MAX_QUADS,
            Some(&narrow),
        );
        assert!(
            g.hits
                .iter()
                .all(|(_, h)| matches!(h, LaneHit::RequestCell(_)))
        );
    }

    #[test]
    fn dense_requests_stay_cells_even_with_spans() {
        let snap = stress();
        let vp = full(&snap);
        let all = TimeRange::new(SessionTime(0), SessionTime(vp.range().1));
        let spans = Spans {
            range: all,
            seqs: (0..5_000u32).collect::<Vec<_>>().into(),
        };
        assert!(!wants_spans(&snap, vp, 1400.0));
        let g = lane_geometry(
            LaneKind::Network,
            &snap,
            vp,
            1400.0,
            CANVAS_MAX_QUADS,
            Some(&spans),
        );
        assert!(
            g.hits
                .iter()
                .all(|(_, h)| matches!(h, LaneHit::RequestCell(_)))
        );
        assert!(
            g.quads.iter().any(|q| q.hue == Hue::Danger),
            "errors show in the cells"
        );
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
        assert_eq!(f.hits[0].1, LaneHit::Finding(5));
        assert_eq!(f.labels.len(), 1);
        assert_eq!(f.labels[0].text, "Jank");
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
        assert_eq!(
            tip(LaneHit::Gc(cell)).as_deref(),
            Some("Garbage collection \u{b7} 4 ms")
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
            Some("1 requests (0 failed)")
        );
        assert_eq!(tip(LaneHit::Frame(5)), None);
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
            assert!(median < 8.0, "median {median:.2} ms");
        }
    }
}
