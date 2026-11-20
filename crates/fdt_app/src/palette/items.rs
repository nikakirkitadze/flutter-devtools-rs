//! What the palette lists: commands, requests and "search logs" items, and their ranking. No
//! GPUI here, so everything is tested directly.

use super::rank::rank;
use super::registry::{Group, commands};
use crate::nav::{Location, Page, Selection, Tool};
use crate::tools::network::model::status_text;
use fdt_store::Snapshot;
use fdt_store::frames::{FrameRecord, FrameStatus, frame_status};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

/// How many requests the palette offers.
pub const REQUEST_ITEM_LIMIT: usize = 2_000;

/// What running an item does.
#[derive(Clone, Debug)]
pub enum ItemKind {
    Action(fn() -> Box<dyn gpui_kit::Action>),
    Navigate(Location),
    SearchLogs(String),
}

#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
    pub keywords: Vec<String>,
    pub group: Group,
    pub kind: ItemKind,
}

impl PaletteItem {
    /// The text the palette ranks against: label, keywords, then detail.
    fn haystack(&self) -> String {
        let mut s = self.label.clone();
        for k in &self.keywords {
            s.push(' ');
            s.push_str(k);
        }
        if let Some(d) = &self.detail {
            s.push(' ');
            s.push_str(d);
        }
        s
    }

    /// Commands are the only items that are remembered as recent.
    pub fn is_command(&self) -> bool {
        matches!(self.kind, ItemKind::Action(_))
    }
}

/// M1's commands as items.
pub fn command_items() -> Vec<PaletteItem> {
    commands()
        .into_iter()
        .map(|c| PaletteItem {
            id: c.id.to_string(),
            label: c.label.to_string(),
            detail: None,
            keywords: c.keywords.iter().map(|k| k.to_string()).collect(),
            group: c.group,
            kind: ItemKind::Action(c.action),
        })
        .collect()
}

/// The newest request per `(method, path)`, newest first, at most `limit`.
///
/// Scans the whole request list in the worst case, so it runs off the UI thread.
pub fn request_items(snap: &Snapshot, limit: usize) -> Vec<PaletteItem> {
    let net = &snap.net;
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut out = Vec::new();
    for seq in (0..net.len()).rev() {
        if out.len() >= limit {
            break;
        }
        let Some(r) = net.get(seq) else { continue };
        if !seen.insert((r.method.as_str(), r.path.as_str())) {
            continue;
        }
        out.push(PaletteItem {
            id: format!("request.{}.{}", r.id.run.0, r.id.seq),
            label: format!("{} {}", r.method, r.path),
            detail: Some(format!("{} \u{b7} {}", r.host, status_text(r))),
            keywords: Vec::new(),
            group: Group::Data,
            kind: ItemKind::Navigate(Location {
                page: Page::Tool(Tool::Network),
                selection: Some(Selection::Request(r.id)),
                range: None,
            }),
        });
    }
    out
}

/// How many of the slowest frames the palette offers.
pub const SLOW_FRAME_ITEM_LIMIT: usize = 20;

/// The frame number a query asks for: `7`, `#7`, `frame 7`, `Frame #7` or `frame7`.
fn frame_number(query: &str) -> Option<u64> {
    let q = query.trim();
    let rest = match q.get(..5) {
        Some(head) if head.eq_ignore_ascii_case("frame") => q[5..].trim_start(),
        _ => q,
    };
    let digits = rest.strip_prefix('#').unwrap_or(rest);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn status_word(status: FrameStatus) -> &'static str {
    match status {
        FrameStatus::Ok => "OK",
        FrameStatus::Slow => "Slow",
        FrameStatus::Janky => "Janky",
    }
}

/// `42.3 ms`, from microseconds.
fn frame_time(us: u32) -> String {
    format!("{:.1} ms", f64::from(us) / 1_000.0)
}

fn frame_item(f: &FrameRecord, label: String, detail: Option<String>) -> PaletteItem {
    PaletteItem {
        id: format!("frame.{}.{}", f.id.run.0, f.id.seq),
        label,
        detail,
        keywords: Vec::new(),
        group: Group::Data,
        kind: ItemKind::Navigate(Location {
            page: Page::Tool(Tool::Performance),
            selection: Some(Selection::Frame(f.id)),
            range: None,
        }),
    }
}

/// "Frame #N" when `query` is a frame number: the newest run that has that frame. Binary
/// searches per run, so it is cheap enough for every keystroke.
pub fn frame_items(snap: &Snapshot, query: &str) -> Vec<PaletteItem> {
    let Some(number) = frame_number(query) else {
        return Vec::new();
    };
    snap.runs
        .iter()
        .rev()
        .find_map(|run| {
            let f = snap.frames.get(snap.frames.by_number(run.id, number)?)?;
            let status = frame_status(f.time_us(), snap.frame_budget_us(f.run));
            let detail = format!("{} \u{b7} {}", frame_time(f.time_us()), status_word(status));
            Some(frame_item(f, format!("Frame #{number}"), Some(detail)))
        })
        .into_iter()
        .collect()
}

/// The `limit` slowest slow or janky frames, slowest first, as "Frame #N · 42.3 ms · Janky".
///
/// Reads every slow frame, so it runs off the UI thread.
pub fn slow_frame_items(snap: &Snapshot, limit: usize) -> Vec<PaletteItem> {
    let slow = snap.frames.slow();
    // The `limit` largest by time, ties going to the newer frame.
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
    for i in 0..slow.len() {
        let Some(&seq) = slow.get(i) else { continue };
        let Some(f) = snap.frames.get(seq as usize) else {
            continue;
        };
        heap.push(Reverse((f.time_us(), seq)));
        if heap.len() > limit {
            heap.pop();
        }
    }
    let mut top: Vec<(u32, u32)> = heap.into_iter().map(|Reverse(k)| k).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    top.into_iter()
        .filter_map(|(us, seq)| {
            let f = snap.frames.get(seq as usize)?;
            let status = frame_status(us, snap.frame_budget_us(f.run));
            let label = format!(
                "Frame #{} \u{b7} {} \u{b7} {}",
                f.number,
                frame_time(us),
                status_word(status)
            );
            Some(frame_item(f, label, None))
        })
        .collect()
}

/// "Search logs for “query”".
pub fn search_logs_item(query: &str) -> PaletteItem {
    PaletteItem {
        id: "search.logs".to_string(),
        label: format!("Search logs for \u{201c}{query}\u{201d}"),
        detail: None,
        keywords: Vec::new(),
        group: Group::Data,
        kind: ItemKind::SearchLogs(query.to_string()),
    }
}

/// The items, the current query and the ranked order of the items.
pub struct PaletteModel {
    items: Vec<PaletteItem>,
    haystacks: Vec<String>,
    query: String,
    ranked: Vec<usize>,
    search: Option<PaletteItem>,
    /// Items the query itself asks for (a frame number), listed before the ranked ones.
    exact: Vec<PaletteItem>,
}

impl PaletteModel {
    pub fn new(items: Vec<PaletteItem>, recent_ids: &[String]) -> Self {
        let haystacks = items.iter().map(PaletteItem::haystack).collect();
        let mut model = Self {
            items,
            haystacks,
            query: String::new(),
            ranked: Vec::new(),
            search: None,
            exact: Vec::new(),
        };
        model.set_query("", recent_ids);
        model
    }

    /// Sets the items the current query asks for; they come first, whatever the ranking says.
    pub fn set_exact(&mut self, exact: Vec<PaletteItem>) {
        self.exact = exact;
    }

    /// Re-ranks for `query`. Synchronous: it stays inside the per-keystroke budget.
    pub fn set_query(&mut self, query: &str, recent_ids: &[String]) {
        let recent: Vec<usize> = recent_ids
            .iter()
            .filter_map(|id| {
                self.items
                    .iter()
                    .position(|i| i.is_command() && &i.id == id)
            })
            .collect();
        self.query = query.to_string();
        self.ranked = rank(query, &self.haystacks, &recent);
        let trimmed = query.trim();
        self.search = (!trimmed.is_empty()).then(|| search_logs_item(trimmed));
        // A new query clears what the last one asked for; the caller sets it again.
        self.exact.clear();
    }

    /// Adds items that arrived later (the requests) and re-ranks the current query.
    pub fn extend(&mut self, more: Vec<PaletteItem>, recent_ids: &[String]) {
        self.haystacks
            .extend(more.iter().map(PaletteItem::haystack));
        self.items.extend(more);
        let query = std::mem::take(&mut self.query);
        let exact = std::mem::take(&mut self.exact);
        self.set_query(&query, recent_ids);
        self.exact = exact;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Indices into the items, best match first. The "search logs" item is not among them.
    pub fn ranked(&self) -> &[usize] {
        &self.ranked
    }

    /// Rows shown: the items the query asks for, the ranked items, then the "search logs" item
    /// when there is a query.
    pub fn row_count(&self) -> usize {
        self.exact.len() + self.ranked.len() + usize::from(self.search.is_some())
    }

    /// The item shown in `row`.
    pub fn item(&self, row: usize) -> &PaletteItem {
        if let Some(item) = self.exact.get(row) {
            return item;
        }
        match self.ranked.get(row - self.exact.len()) {
            Some(&i) => &self.items[i],
            None => self.search.as_ref().expect("row in range"),
        }
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{request, snapshot};

    fn snap_with(reqs: &[(&str, &str, u16)]) -> std::sync::Arc<Snapshot> {
        snapshot(|w, run| {
            for (i, (method, url, status)) in reqs.iter().enumerate() {
                request(
                    w,
                    run,
                    method,
                    url,
                    Some(*status),
                    i as u64 + 1,
                    Some(i as u64 + 2),
                );
            }
        })
    }

    #[test]
    fn requests_keep_the_latest_per_method_and_path_newest_first() {
        let snap = snap_with(&[
            ("GET", "https://api.test/a", 200),
            ("GET", "https://api.test/b", 200),
            ("GET", "https://api.test/a", 404),
            ("POST", "https://api.test/a", 201),
        ]);
        let items = request_items(&snap, 10);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["POST /a", "GET /a", "GET /b"]);
        assert_eq!(items[1].detail.as_deref(), Some("api.test \u{b7} 404"));
        assert!(items.iter().all(|i| i.group == Group::Data));
        let ItemKind::Navigate(loc) = &items[0].kind else {
            panic!("navigates")
        };
        assert_eq!(loc.page, Page::Tool(Tool::Network));
        assert!(matches!(loc.selection, Some(Selection::Request(_))));
        assert_eq!(loc.range, None);
    }

    #[test]
    fn requests_are_limited() {
        let reqs: Vec<(&str, String, u16)> = (0..50)
            .map(|i| ("GET", format!("https://api.test/p{i}"), 200))
            .collect();
        let snap = snapshot(|w, run| {
            for (i, (m, u, s)) in reqs.iter().enumerate() {
                request(w, run, m, u, Some(*s), i as u64 + 1, None);
            }
        });
        let items = request_items(&snap, 7);
        assert_eq!(items.len(), 7);
        assert_eq!(items[0].label, "GET /p49");
    }

    #[test]
    fn search_logs_item_is_last_and_not_ranked() {
        let mut m = PaletteModel::new(command_items(), &[]);
        assert_eq!(m.row_count(), m.ranked().len(), "none without a query");
        m.set_query("memory", &[]);
        assert_eq!(m.row_count(), m.ranked().len() + 1);
        let last = m.item(m.row_count() - 1);
        assert_eq!(last.label, "Search logs for \u{201c}memory\u{201d}");
        assert!(matches!(&last.kind, ItemKind::SearchLogs(q) if q == "memory"));
        m.set_query("zzqq", &[]);
        assert!(m.ranked().is_empty());
        assert_eq!(m.row_count(), 1);
    }

    #[test]
    fn ranking_covers_the_detail_and_late_items_rerank() {
        let mut m = PaletteModel::new(command_items(), &[]);
        m.set_query("api.test", &[]);
        assert!(m.ranked().is_empty());
        m.extend(
            request_items(&snap_with(&[("GET", "https://api.test/x", 200)]), 10),
            &[],
        );
        assert_eq!(m.query(), "api.test");
        assert_eq!(m.item(0).label, "GET /x");
    }

    #[test]
    fn recent_commands_come_first_and_requests_are_never_recent() {
        let recents = vec!["tool.logs".to_string()];
        let m = PaletteModel::new(command_items(), &recents);
        assert_eq!(m.item(0).id, "tool.logs");
    }

    /// Frames at 60 Hz in two runs. Run 1 has frames 0..=3, run 2 has frames 2 and 7.
    fn frames_in_two_runs() -> std::sync::Arc<Snapshot> {
        use crate::test_support::frame_at;
        snapshot(|w, run| {
            for (n, build) in [8u64, 20, 40, 9].into_iter().enumerate() {
                w.push_frame(frame_at(run, n as u64, 1_000 + n as u64 * 100, build, 1));
            }
            w.end_run(run, fdt_store::time::SessionTime(5_000_000_000));
            let clock = fdt_store::clock::RunClock::new(
                0,
                fdt_store::clock::Calibration {
                    skew_us: 0,
                    timeline_to_wall_us: 0,
                },
            );
            let second = w.start_run(
                crate::test_support::app_info("demo"),
                clock,
                fdt_store::time::SessionTime(10_000_000_000),
            );
            w.push_frame(frame_at(second, 2, 12_000, 30, 1));
            w.push_frame(frame_at(second, 7, 12_100, 5, 1));
        })
    }

    #[test]
    fn frame_number_query_finds_the_frame() {
        let snap = frames_in_two_runs();
        for query in ["#1", "1", "frame 1", "Frame #1", "frame#1", "  #1 "] {
            let items = frame_items(&snap, query);
            assert_eq!(items.len(), 1, "{query:?}");
            assert_eq!(items[0].label, "Frame #1");
            assert_eq!(items[0].group, Group::Data);
            let ItemKind::Navigate(loc) = &items[0].kind else {
                panic!("navigates")
            };
            assert_eq!(loc.page, Page::Tool(Tool::Performance));
            assert_eq!(loc.range, None);
            let Some(Selection::Frame(id)) = loc.selection else {
                panic!("selects a frame")
            };
            assert_eq!((id.run.0, id.seq), (1, 1));
        }
        // The newest run that has the number wins.
        let items = frame_items(&snap, "#2");
        let ItemKind::Navigate(loc) = &items[0].kind else {
            panic!("navigates")
        };
        let Some(Selection::Frame(id)) = loc.selection else {
            panic!("selects a frame")
        };
        assert_eq!(id.run.0, 2);
        // Only a frame number is a frame query, and only a frame that exists is offered.
        for query in ["", "frame", "#", "abc", "1a", "#99", "-1", "frames 1"] {
            assert!(frame_items(&snap, query).is_empty(), "{query:?}");
        }
    }

    #[test]
    fn slow_frame_items_are_slowest_first() {
        let snap = frames_in_two_runs();
        let items = slow_frame_items(&snap, 20);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Frame #2 \u{b7} 40.0 ms \u{b7} Janky",
                "Frame #2 \u{b7} 30.0 ms \u{b7} Slow",
                "Frame #1 \u{b7} 20.0 ms \u{b7} Slow",
            ]
        );
        // Ids are unique even though two frames share a number.
        let ids: HashSet<_> = items.iter().map(|i| i.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(slow_frame_items(&snap, 2).len(), 2);
        assert!(slow_frame_items(&Snapshot::empty(), 20).is_empty());
        let ItemKind::Navigate(loc) = &items[1].kind else {
            panic!("navigates")
        };
        assert!(matches!(loc.selection, Some(Selection::Frame(id)) if id.run.0 == 2));
    }

    #[test]
    fn exact_items_come_before_ranked_ones() {
        let snap = frames_in_two_runs();
        let mut m = PaletteModel::new(command_items(), &[]);
        m.set_query("#1", &[]);
        let before = m.row_count();
        m.set_exact(frame_items(&snap, "#1"));
        assert_eq!(m.row_count(), before + 1);
        assert_eq!(m.item(0).label, "Frame #1");
        m.set_exact(Vec::new());
        assert_eq!(m.row_count(), before);
    }
}
