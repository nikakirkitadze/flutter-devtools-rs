//! What the palette lists: commands, requests and "search logs" items, and their ranking. No
//! GPUI here, so everything is tested directly.

use super::rank::rank;
use super::registry::{Group, commands};
use crate::nav::{Location, Page, Selection, Tool};
use crate::tools::network::model::status_text;
use fdt_store::Snapshot;
use std::collections::HashSet;

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
        };
        model.set_query("", recent_ids);
        model
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
    }

    /// Adds items that arrived later (the requests) and re-ranks the current query.
    pub fn extend(&mut self, more: Vec<PaletteItem>, recent_ids: &[String]) {
        self.haystacks
            .extend(more.iter().map(PaletteItem::haystack));
        self.items.extend(more);
        let query = std::mem::take(&mut self.query);
        self.set_query(&query, recent_ids);
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Indices into the items, best match first. The "search logs" item is not among them.
    pub fn ranked(&self) -> &[usize] {
        &self.ranked
    }

    /// Rows shown: the ranked items, then the "search logs" item when there is a query.
    pub fn row_count(&self) -> usize {
        self.ranked.len() + usize::from(self.search.is_some())
    }

    /// The item shown in `row`.
    pub fn item(&self, row: usize) -> &PaletteItem {
        match self.ranked.get(row) {
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
}
