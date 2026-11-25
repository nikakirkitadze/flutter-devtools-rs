//! The Snapshot tab: what holds the app's memory. A header states the snapshot's totals, a
//! lazily built dominator tree lists the objects by retained size, a class search leads to the
//! largest instances and a reveal in the tree, and a details pane shows one object's value,
//! fields and retaining path.
//!
//! The tree is built only for the nodes the user has opened, at most [`MAX_CHILDREN`] children
//! per node, so a node with a million children costs the same as one with five hundred. The
//! object details and the instance scan read the whole index or the snapshot file, so they run
//! in `background_spawn`, and a result for a selection that has moved on is dropped.

use super::leaks_tab::{edge_label, stage_text};
use super::{group_digits, heap_number};
use crate::actions::OpenDetails;
use crate::focus::PendingFocus;
use crate::keymap::HEAP_TREE_CONTEXT;
use crate::nav::{Location, Page, Selection, Tool};
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use crate::tools::network::model::format_bytes;
use crate::tools::{OpenLocation, ReplaceLocation};
use fdt_analysis::heap::details::object_details;
use fdt_analysis::heap::leaks::path_text;
use fdt_store::Snapshot;
use fdt_store::heap::{HeapId, HeapIndex, HeapRecord, HeapStage, HeapSummary};
use fdt_store::memory::MemorySnapshot;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, motion, row, space, table};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    TestSupportExt as _, UniformListScrollHandle, Window, div, uniform_list,
};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::ops::Range;
use std::sync::Arc;

/// The most children one node shows; the rest are a "{n} more" row.
pub const MAX_CHILDREN: usize = 500;
/// The most classes the search lists.
pub const MAX_CLASSES: usize = 200;
/// The most instances of a class the search lists.
pub const MAX_INSTANCES: usize = 1_000;
/// The most fields the details list.
pub const MAX_FIELDS: usize = 100;
/// A reveal that would open more levels than this only selects the object: the tree widget
/// flattens and drops its items recursively.
const MAX_REVEAL_DEPTH: usize = 400;
/// The most characters of a class name a row shows.
const MAX_NAME_CHARS: usize = 80;
/// The rows of the search results shown without scrolling.
const RESULT_ROWS: usize = 8;
/// The rows of the path and the fields shown without scrolling.
const DETAIL_ROWS: usize = 10;

pub const EMPTY_HEADING: &str = "No heap snapshot yet";
/// The empty state's body, with the platform's keys for taking a snapshot.
pub fn empty_body() -> String {
    format!(
        "Take one ({}) to browse what holds your app's memory.",
        crate::keymap::platform("\u{2318}\u{21e7}S", "Ctrl+Shift+S")
    )
}
pub const NO_MATCH: &str = "No classes match your search.";
pub const DISCONNECTED_TIP: &str = "Connect to an app to open an older snapshot.";

// ---- the model ----

/// What a snapshot's header says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderText {
    pub title: String,
    pub unreachable: Option<String>,
    pub http: Option<String>,
}

pub fn header_text(record: &HeapRecord, summary: &HeapSummary) -> HeaderText {
    let time = format_offset(record.time.unwrap_or(record.requested));
    let unreachable = summary.objects.saturating_sub(summary.reachable);
    HeaderText {
        title: format!(
            "Snapshot #{} \u{b7} {time} \u{b7} {} objects \u{b7} {}",
            heap_number(record.id),
            group_digits(summary.reachable),
            format_bytes(summary.reachable_bytes)
        ),
        unreachable: (unreachable > 0).then(|| {
            format!(
                "{} unreachable objects ({}) were waiting for garbage collection and are left out.",
                group_digits(unreachable),
                format_bytes(summary.unreachable_bytes)
            )
        }),
        http: summary.http.map(|h| {
            format!(
                "Held by HTTP recording: {} in {} requests.",
                format_bytes(h.bytes),
                group_digits(h.requests)
            )
        }),
    }
}

/// A name cut to [`MAX_NAME_CHARS`] characters: the first 79 and an ellipsis.
pub fn elide(name: &str) -> String {
    if name.chars().nth(MAX_NAME_CHARS).is_none() {
        return name.to_string();
    }
    let mut s: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    s.push('\u{2026}');
    s
}

fn basename(library: &str) -> &str {
    library.rsplit('/').next().unwrap_or(library)
}

/// The name of a node: its synthetic name ("Root", "Isolate statics", a root slice) or its class.
pub fn node_name(index: &HeapIndex, summary: &HeapSummary, node: u32) -> String {
    if let Some(name) = index.synthetic_name(node) {
        return name.to_string();
    }
    summary
        .classes
        .get(index.class_of(node) as usize)
        .map_or_else(|| "Object".to_string(), |c| c.name.to_string())
}

/// The basename of a node's library, for a node that has a class with one.
pub fn node_library(index: &HeapIndex, summary: &HeapSummary, node: u32) -> Option<String> {
    let class = summary.classes.get(index.class_of(node) as usize)?;
    (!class.library.is_empty()).then(|| basename(&class.library).to_string())
}

pub fn percent_text(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole.max(1) as f64)
}

/// The columns of one tree row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowText {
    pub name: String,
    pub library: Option<String>,
    pub retained: String,
    pub percent: String,
    pub shallow: String,
}

pub fn row_text(index: &HeapIndex, summary: &HeapSummary, node: u32) -> RowText {
    RowText {
        name: elide(&node_name(index, summary, node)),
        library: node_library(index, summary, node),
        retained: format_bytes(index.retained(node)),
        percent: percent_text(index.retained(node), index.retained(0)),
        shallow: format_bytes(u64::from(index.shallow(node))),
    }
}

/// The id of the item of a node.
fn node_id(node: u32) -> SharedString {
    node.to_string().into()
}

/// The node an item stands for; `None` for a "more" row or a placeholder.
fn item_node(id: &str) -> Option<u32> {
    id.parse().ok()
}

/// The items for the children of `parent`: the first [`MAX_CHILDREN`], the pinned ones past them,
/// and a "{n} more" row for the rest. Children of the nodes in `open` are built too; the others
/// get one placeholder, so they can be opened.
pub fn level_items(
    index: &HeapIndex,
    summary: &HeapSummary,
    parent: u32,
    open: &HashSet<u32>,
    pinned: &HashSet<u32>,
) -> Vec<TreeItem> {
    let kids = index.children(parent);
    let len = (kids.end - kids.start) as usize;
    let first_end = kids.start + len.min(MAX_CHILDREN) as u32;
    let mut nodes: Vec<u32> = (kids.start..first_end).collect();
    let mut extra: Vec<u32> = pinned
        .iter()
        .copied()
        .filter(|n| (first_end..kids.end).contains(n))
        .collect();
    extra.sort_unstable();
    nodes.extend(extra);
    let mut items = Vec::with_capacity(nodes.len() + 1);
    for &n in &nodes {
        let mut item = TreeItem::new(node_id(n), elide(&node_name(index, summary, n)));
        if !index.children(n).is_empty() {
            if open.contains(&n) {
                item = item
                    .expanded(true)
                    .children(level_items(index, summary, n, open, pinned));
            } else {
                item = item.child(TreeItem::new(format!("\u{2026}{n}"), "").disabled(true));
            }
        }
        items.push(item);
    }
    let more = len - nodes.len();
    if more > 0 {
        items.push(
            TreeItem::new(
                format!("more{parent}"),
                format!("{} more", group_digits(more as u64)),
            )
            .disabled(true),
        );
    }
    items
}

/// The classes whose name or library contains `query` (lowercase), the biggest first, at most
/// [`MAX_CLASSES`]. `keys` holds each class's lowercase name and library.
pub fn matching_classes(summary: &HeapSummary, keys: &[(String, String)], query: &str) -> Vec<u32> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<u32> = keys
        .iter()
        .enumerate()
        .filter(|(_, (name, lib))| name.contains(query) || lib.contains(query))
        .map(|(i, _)| i as u32)
        .collect();
    found.sort_by_key(|&i| Reverse(summary.classes[i as usize].shallow));
    found.truncate(MAX_CLASSES);
    found
}

pub fn class_keys(summary: &HeapSummary) -> Vec<(String, String)> {
    summary
        .classes
        .iter()
        .map(|c| (c.name.to_lowercase(), c.library.to_lowercase()))
        .collect()
}

/// The `limit` nodes of `class` with the most retained bytes, the biggest first. Reads every
/// node, so it runs off the UI thread.
pub fn largest_instances(index: &HeapIndex, class: u32, limit: usize) -> Vec<(u32, u64)> {
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<u32>)>> = BinaryHeap::with_capacity(limit + 1);
    for node in 0..index.len() as u32 {
        if index.class_of(node) != class {
            continue;
        }
        heap.push(Reverse((index.retained(node), Reverse(node))));
        if heap.len() > limit {
            heap.pop();
        }
    }
    let mut out: Vec<(u32, u64)> = heap
        .into_iter()
        .map(|Reverse((retained, Reverse(node)))| (node, retained))
        .collect();
    out.sort_by_key(|&(node, retained)| (Reverse(retained), node));
    out
}

/// The nodes from the top level of the tree down to `node`, or `None` when that is deeper than
/// [`MAX_REVEAL_DEPTH`].
pub fn ancestry(index: &HeapIndex, node: u32) -> Option<Vec<u32>> {
    let mut chain = Vec::new();
    let mut at = node;
    while at != 0 {
        chain.push(at);
        if chain.len() > MAX_REVEAL_DEPTH {
            return None;
        }
        at = index.idom(at);
    }
    chain.reverse();
    Some(chain)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRow {
    pub class: String,
    pub edge: String,
    pub retained: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldRow {
    pub edge: String,
    pub class: String,
    /// The referent's node, when it is reachable.
    pub node: Option<u32>,
    pub value: Option<String>,
}

/// Everything the details pane shows for one object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Details {
    pub heap: HeapId,
    pub node: u32,
    pub class: String,
    pub library: Option<String>,
    pub shallow: String,
    pub retained: String,
    pub percent: String,
    pub value: Option<String>,
    pub path: Vec<PathRow>,
    pub path_text: String,
    pub fields: Vec<FieldRow>,
    pub fields_more: u32,
}

/// Reads the object from the snapshot file: one linear pass over the index.
pub fn build_details(
    index: &HeapIndex,
    summary: &HeapSummary,
    heap: HeapId,
    node: u32,
) -> std::io::Result<Details> {
    let d = object_details(index, summary, node, MAX_FIELDS)?;
    let class_name = |class: u32| {
        summary
            .classes
            .get(class as usize)
            .map_or_else(|| "Object".to_string(), |c| c.name.to_string())
    };
    Ok(Details {
        heap,
        node,
        class: node_name(index, summary, node),
        library: node_library(index, summary, node),
        shallow: format_bytes(u64::from(index.shallow(node))),
        retained: format_bytes(index.retained(node)),
        percent: percent_text(index.retained(node), index.retained(0)),
        value: d.value,
        path: d
            .path
            .iter()
            .map(|h| PathRow {
                class: class_name(h.class),
                edge: edge_label(&h.edge),
                retained: format_bytes(h.retained),
            })
            .collect(),
        path_text: path_text(summary, &d.path),
        fields: d
            .fields
            .iter()
            .map(|(edge, class, node, value)| FieldRow {
                edge: edge_label(edge),
                class: class_name(*class),
                node: *node,
                value: value.clone(),
            })
            .collect(),
        fields_more: d.fields_total.saturating_sub(MAX_FIELDS as u32),
    })
}

/// The record the tab shows: the selected one, else the held one, else the newest analysed.
pub fn shown_record(mem: &MemorySnapshot, selection: Option<Selection>) -> Option<&HeapRecord> {
    let chosen = match selection {
        Some(Selection::Heap(h) | Selection::HeapNode { heap: h, .. }) => mem.heap(h),
        _ => None,
    };
    chosen
        .or_else(|| mem.held().and_then(|h| mem.heap(h)))
        .or_else(|| mem.heaps().iter().rev().find(|h| h.summary.is_some()))
}

// ---- the tab ----

/// Which held snapshot a cache was built for. Run ids, `HeapId`s and node ids restart in every
/// session, and an index is replaced when another snapshot is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewKey {
    session: u64,
    heap: HeapId,
    index: usize,
}

#[derive(Clone)]
struct Ready {
    key: ViewKey,
    index: Arc<HeapIndex>,
    summary: Arc<HeapSummary>,
}

fn ready(snap: &Snapshot, selection: Option<Selection>) -> Option<Ready> {
    let record = shown_record(&snap.memory, selection)?;
    if record.stage != HeapStage::Ready {
        return None;
    }
    let (index, summary) = (record.index.clone()?, record.summary.clone()?);
    Some(Ready {
        key: ViewKey {
            session: snap.session,
            heap: record.id,
            index: Arc::as_ptr(&index) as usize,
        },
        index,
        summary,
    })
}

/// Each class's lowercase name and library, for the search.
type ClassKeys = Arc<Vec<(String, String)>>;
/// The largest instances of a class: node and retained bytes.
type Instances = Arc<Vec<(u32, u64)>>;

struct TreeCtx {
    key: ViewKey,
    open: HashSet<u32>,
    pinned: HashSet<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DetailsKey {
    view: ViewKey,
    node: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanKey {
    view: ViewKey,
    class: u32,
}

pub struct SnapshotTab {
    session: Entity<SessionModel>,
    range: Option<fdt_store::time::TimeRange>,
    selection: Option<Selection>,
    active: bool,
    focus: FocusHandle,
    focus_pending: PendingFocus,
    /// The tree holds the keyboard focus (set by us), so it must hand it back when it goes.
    tree_focus: bool,
    tree_state: Entity<TreeState>,
    tree: Option<TreeCtx>,
    /// The selection whose reveal in the tree is done.
    revealed: Option<(ViewKey, u32)>,
    input: Entity<InputState>,
    /// The search, lowercase and trimmed.
    query: String,
    /// Text for the search field, set on the next render (it needs the window).
    fill_input: Option<String>,
    keys: Option<(ViewKey, ClassKeys)>,
    matches: Vec<u32>,
    chosen_class: Option<u32>,
    instances: Option<(ScanKey, Instances)>,
    scan_started: Option<ScanKey>,
    scan_job: Option<Task<()>>,
    results_scroll: UniformListScrollHandle,
    details: Option<(DetailsKey, Arc<Details>)>,
    details_started: Option<DetailsKey>,
    details_job: Option<Task<()>>,
    details_failed: bool,
    skeleton: bool,
    skeleton_timer: Option<Task<()>>,
    path_scroll: UniformListScrollHandle,
    fields_scroll: UniformListScrollHandle,
    failed_open: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenLocation> for SnapshotTab {}
impl EventEmitter<ReplaceLocation> for SnapshotTab {}

impl SnapshotTab {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find a class"));
        let tree_state = cx.new(|cx| TreeState::new(cx));
        let subs = vec![
            cx.observe(&session, |this, _, cx| {
                if this.active {
                    this.sync(cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&input, window, |this, _, e: &InputEvent, _, cx| {
                if matches!(e, InputEvent::Change) {
                    let q = this.input.read(cx).value().trim().to_lowercase();
                    this.set_query(q, cx);
                }
            }),
            cx.subscribe(&tree_state, |this, _, e: &TreeEvent, cx| match e {
                TreeEvent::Expanded(id) => {
                    if let (Some(node), Some(t)) = (item_node(id), this.tree.as_mut())
                        && t.open.insert(node)
                    {
                        this.rebuild_items(cx);
                    }
                }
                TreeEvent::Collapsed(id) => {
                    if let (Some(node), Some(t)) = (item_node(id), this.tree.as_mut()) {
                        t.open.remove(&node);
                    }
                }
            }),
        ];
        Self {
            session,
            range: None,
            selection: None,
            active: false,
            focus: cx.focus_handle(),
            focus_pending: PendingFocus::default(),
            tree_focus: false,
            tree_state,
            tree: None,
            revealed: None,
            input,
            query: String::new(),
            fill_input: None,
            keys: None,
            matches: Vec::new(),
            chosen_class: None,
            instances: None,
            scan_started: None,
            scan_job: None,
            results_scroll: UniformListScrollHandle::new(),
            details: None,
            details_started: None,
            details_job: None,
            details_failed: false,
            skeleton: false,
            skeleton_timer: None,
            path_scroll: UniformListScrollHandle::new(),
            fields_scroll: UniformListScrollHandle::new(),
            failed_open: false,
            _subs: subs,
        }
    }

    fn snap(&self, cx: &App) -> Arc<Snapshot> {
        self.session.read(cx).snapshot().clone()
    }

    fn ready(&self, cx: &App) -> Option<Ready> {
        ready(&self.snap(cx), self.selection)
    }

    /// A new session: nothing of the old one stays.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.range = None;
        self.tree = None;
        self.revealed = None;
        self.clear_search_results();
        self.query.clear();
        self.fill_input = Some(String::new());
        self.keys = None;
        self.clear_details();
        self.failed_open = false;
        self.tree_state
            .update(cx, |s, cx| s.set_items(Vec::new(), cx));
        cx.notify();
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if active {
                self.focus_pending.request();
                self.sync(cx);
            } else {
                self.focus_pending.cancel();
            }
            cx.notify();
        }
    }

    pub fn set_range(&mut self, range: Option<fdt_store::time::TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            cx.notify();
        }
    }

    /// The location's selection. Only a heap snapshot or one of its objects is the tab's;
    /// anything else is ignored (and not cleared: other tabs own it). A snapshot that is not the
    /// held one is opened, once for each new selection.
    pub fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        let mine =
            selection.filter(|s| matches!(s, Selection::Heap(_) | Selection::HeapNode { .. }));
        if selection.is_some() && mine.is_none() {
            return;
        }
        if mine == self.selection {
            return;
        }
        self.selection = mine;
        if let Some(heap) = mine.and_then(Selection::heap) {
            self.open_if_needed(heap, cx);
        }
        self.sync(cx);
        cx.notify();
    }

    /// Asks the capture to analyse `heap` again when it is not the held snapshot and nothing is
    /// being done to it.
    fn open_if_needed(&mut self, heap: HeapId, cx: &mut Context<Self>) {
        let snap = self.snap(cx);
        let needs = snap.memory.heap(heap).is_some_and(|r| {
            r.index.is_none() && r.file.is_some() && r.summary.is_some() && !r.stage.is_busy()
        });
        if needs && let Some(control) = self.session.read(cx).control() {
            control.open_heap_snapshot(heap);
        }
    }

    /// Chooses a snapshot in the picker.
    pub fn pick(&mut self, heap: HeapId, cx: &mut Context<Self>) {
        let sel = Selection::Heap(heap);
        if self.selection == Some(sel) {
            return;
        }
        self.selection = Some(sel);
        self.open_if_needed(heap, cx);
        self.sync(cx);
        self.emit(Some(sel), false, cx);
        cx.notify();
    }

    fn emit(&mut self, selection: Option<Selection>, replace: bool, cx: &mut Context<Self>) {
        let loc = Location {
            page: Page::Tool(Tool::Memory),
            selection,
            range: self.range,
        };
        if replace {
            cx.emit(ReplaceLocation(loc));
        } else {
            cx.emit(OpenLocation(loc));
        }
    }

    /// Starts the search with `text`, as "Show in snapshot" asks.
    pub fn set_search_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.fill_input = Some(text.to_string());
        self.set_query(text.trim().to_lowercase(), cx);
    }

    /// Hands the focus to `to` (an element that stays on screen) if the tree has it.
    pub fn release_focus(&mut self, to: &FocusHandle, window: &mut Window, cx: &mut App) {
        if std::mem::take(&mut self.tree_focus) {
            window.focus(to, cx);
        }
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    // ---- sync ----

    /// Brings the tree, the search and the details up to date with the shown snapshot.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let ready = self.ready(cx);
        match (&ready, &self.tree) {
            (Some(r), Some(t)) if t.key == r.key => {}
            (Some(r), _) => {
                self.tree = Some(TreeCtx {
                    key: r.key,
                    open: HashSet::new(),
                    pinned: HashSet::new(),
                });
                self.revealed = None;
                self.keys = None;
                self.clear_details();
                self.rebuild_items(cx);
                let q = self.query.clone();
                self.set_query(q, cx);
            }
            (None, Some(_)) => {
                self.tree = None;
                self.revealed = None;
                self.keys = None;
                self.matches.clear();
                self.clear_search_results();
                self.clear_details();
                self.tree_state
                    .update(cx, |s, cx| s.set_items(Vec::new(), cx));
            }
            (None, None) => {}
        }
        if let (Some(r), Some(Selection::HeapNode { heap, node })) = (&ready, self.selection)
            && r.key.heap == heap
            && self.revealed != Some((r.key, node))
        {
            self.revealed = Some((r.key, node));
            self.reveal(node, cx);
        }
        self.sync_details(cx);
    }

    fn rebuild_items(&mut self, cx: &mut Context<Self>) {
        let (Some(ready), Some(ctx)) = (self.ready(cx), self.tree.as_ref()) else {
            return;
        };
        let items = level_items(&ready.index, &ready.summary, 0, &ctx.open, &ctx.pinned);
        self.tree_state.update(cx, |state, cx| {
            let selected = state.selected_item().map(|i| i.id.clone());
            state.set_items(items, cx);
            if let Some(ix) = selected.and_then(|id| state.index_of(&id)) {
                state.set_selected_index(Some(ix), cx);
            }
        });
        cx.notify();
    }

    /// Opens the `idom` ancestors of `node`, selects its row and scrolls to it.
    fn reveal(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(ready) = self.ready(cx) else { return };
        if node == 0 || node as usize >= ready.index.len() {
            return;
        }
        let Some(chain) = ancestry(&ready.index, node) else {
            return;
        };
        let Some(ctx) = self.tree.as_mut() else {
            return;
        };
        for (i, &n) in chain.iter().enumerate() {
            ctx.pinned.insert(n);
            if i + 1 < chain.len() {
                ctx.open.insert(n);
            }
        }
        self.rebuild_items(cx);
        let id = node_id(node);
        self.tree_state.update(cx, |state, cx| {
            if let Some(ix) = state.index_of(&id) {
                state.set_selected_index(Some(ix), cx);
                state.scroll_to_item(ix, ScrollStrategy::Center);
            }
        });
    }

    /// Selects an object of the tree, from a click or Enter.
    fn select_node(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(ready) = self.ready(cx) else { return };
        let sel = Selection::HeapNode {
            heap: ready.key.heap,
            node,
        };
        if self.selection == Some(sel) {
            return;
        }
        self.selection = Some(sel);
        self.revealed = Some((ready.key, node));
        self.sync_details(cx);
        self.emit(Some(sel), true, cx);
        cx.notify();
    }

    /// Goes to an object from the search or a field: reveals it and selects it.
    fn go_to(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(ready) = self.ready(cx) else { return };
        let sel = Selection::HeapNode {
            heap: ready.key.heap,
            node,
        };
        self.selection = Some(sel);
        self.revealed = Some((ready.key, node));
        self.reveal(node, cx);
        self.sync_details(cx);
        self.emit(Some(sel), false, cx);
        cx.notify();
    }

    /// Enter: the details of the tree's selected row.
    fn open_details(&mut self, cx: &mut Context<Self>) {
        let node = self
            .tree_state
            .read(cx)
            .selected_item()
            .and_then(|i| item_node(&i.id));
        if let Some(node) = node {
            self.select_node(node, cx);
        }
    }

    // ---- search ----

    fn clear_search_results(&mut self) {
        self.matches.clear();
        self.chosen_class = None;
        self.instances = None;
        self.scan_started = None;
        self.scan_job = None;
    }

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        let changed = query != self.query;
        self.query = query;
        if changed {
            self.chosen_class = None;
            self.instances = None;
            self.scan_started = None;
            self.scan_job = None;
        }
        self.matches.clear();
        if let Some(ready) = self.ready(cx)
            && !self.query.is_empty()
        {
            let keys = match &self.keys {
                Some((k, keys)) if *k == ready.key => keys.clone(),
                _ => {
                    let keys = Arc::new(class_keys(&ready.summary));
                    self.keys = Some((ready.key, keys.clone()));
                    keys
                }
            };
            self.matches = matching_classes(&ready.summary, &keys, &self.query);
        }
        cx.notify();
    }

    /// Lists the largest instances of a class, scanning the index in the background.
    fn choose_class(&mut self, class: u32, cx: &mut Context<Self>) {
        let Some(ready) = self.ready(cx) else { return };
        let key = ScanKey {
            view: ready.key,
            class,
        };
        self.chosen_class = Some(class);
        if self.instances.as_ref().is_some_and(|(k, _)| *k == key) || self.scan_started == Some(key)
        {
            cx.notify();
            return;
        }
        self.instances = None;
        self.scan_started = Some(key);
        let index = ready.index.clone();
        self.scan_job = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { largest_instances(&index, class, MAX_INSTANCES) })
                .await;
            this.update(cx, |this, cx| {
                if this.scan_started == Some(key) {
                    this.instances = Some((key, Arc::new(found)));
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn back_to_classes(&mut self, cx: &mut Context<Self>) {
        self.chosen_class = None;
        self.instances = None;
        self.scan_started = None;
        self.scan_job = None;
        cx.notify();
    }

    // ---- details ----

    fn clear_details(&mut self) {
        self.details = None;
        self.details_started = None;
        self.details_job = None;
        self.details_failed = false;
        self.skeleton = false;
        self.skeleton_timer = None;
    }

    /// Starts reading the selected object, unless that is the one already read or being read.
    fn sync_details(&mut self, cx: &mut Context<Self>) {
        let Some(Selection::HeapNode { heap, node }) = self.selection else {
            self.clear_details();
            return;
        };
        let Some(ready) = self.ready(cx) else {
            return;
        };
        if ready.key.heap != heap || node as usize >= ready.index.len() {
            self.clear_details();
            return;
        }
        let key = DetailsKey {
            view: ready.key,
            node,
        };
        if self.details_started == Some(key) {
            return;
        }
        self.clear_details();
        self.details_started = Some(key);
        let (index, summary) = (ready.index.clone(), ready.summary.clone());
        self.skeleton_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if this.details_started == Some(key) && this.details.is_none() {
                    this.skeleton = true;
                    cx.notify();
                }
            })
            .ok();
        }));
        self.details_job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn(
                    async move { build_details(&index, &summary, key.view.heap, node) },
                )
                .await;
            this.update(cx, |this, cx| {
                if this.details_started == Some(key) {
                    this.skeleton = false;
                    match out {
                        Ok(d) => this.details = Some((key, Arc::new(d))),
                        Err(_) => this.details_failed = true,
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn copy_path(&mut self, cx: &mut Context<Self>) {
        if let Some((_, d)) = &self.details {
            cx.write_to_clipboard(ClipboardItem::new_string(d.path_text.clone()));
        }
    }

    // ---- test hooks ----

    #[cfg(test)]
    pub fn details(&self) -> Option<Arc<Details>> {
        self.details.as_ref().map(|(_, d)| d.clone())
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    pub fn matches(&self) -> &[u32] {
        &self.matches
    }

    #[cfg(test)]
    pub fn instances(&self) -> Option<Arc<Vec<(u32, u64)>>> {
        self.instances.as_ref().map(|(_, v)| v.clone())
    }

    #[cfg(test)]
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The visible tree rows: id, depth and label.
    #[cfg(test)]
    pub fn tree_rows(&self, cx: &App) -> Vec<(String, usize, String)> {
        let state = self.tree_state.read(cx);
        (0..)
            .map_while(|ix| state.entry(ix))
            .map(|e| {
                (
                    e.item().id.to_string(),
                    e.depth(),
                    e.item().label.to_string(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub fn tree_selected(&self, cx: &App) -> Option<String> {
        self.tree_state
            .read(cx)
            .selected_item()
            .map(|i| i.id.to_string())
    }

    #[cfg(test)]
    pub fn open_nodes(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .tree
            .as_ref()
            .map(|t| t.open.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }
}

// ---- rendering ----

impl SnapshotTab {
    fn text_line(
        &self,
        id: &'static str,
        text: String,
        colour: Hsla,
    ) -> impl gpui_kit::IntoElement + use<> {
        div()
            .id(id)
            .test_support()
            .aria_label(SharedString::from(text.clone()))
            .px(space::S16)
            .text_size(font::BODY)
            .text_color(colour)
            .child(text)
    }

    fn render_picker(&self, current: Option<HeapId>, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.snap(cx);
        let connected = self.session.read(cx).control().is_some();
        let label = current.map_or_else(
            || "Snapshot".to_string(),
            |h| format!("Snapshot #{}", heap_number(h)),
        );
        let weak = cx.weak_entity();
        let held = snap.memory.held();
        let mut button = Button::new("heap-pick").label(label).ghost().compact();
        if !connected {
            button = button.tooltip(DISCONNECTED_TIP);
        }
        let button = button.dropdown_menu(move |mut menu, _window, cx| {
            let snap = weak
                .read_with(cx, |this, cx| this.snap(cx))
                .unwrap_or_else(|_| Arc::new(Snapshot::empty()));
            for record in snap.memory.heaps().iter().rev() {
                let id = record.id;
                let text = match &record.summary {
                    Some(s) if record.stage == HeapStage::Ready => format!(
                        "#{} \u{b7} {} \u{b7} {} objects",
                        heap_number(id),
                        format_offset(record.time.unwrap_or(record.requested)),
                        group_digits(s.reachable)
                    ),
                    _ => format!(
                        "#{} \u{b7} {}",
                        heap_number(id),
                        stage_text(&record.stage).unwrap_or_else(|| "Not available".into())
                    ),
                };
                let usable = record.summary.is_some()
                    && !record.stage.is_busy()
                    && (held == Some(id) || connected);
                let weak = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new(text)
                        .checked(current == Some(id))
                        .disabled(!usable)
                        .on_click(move |_, _, cx| {
                            weak.update(cx, |this, cx| this.pick(id, cx)).ok();
                        }),
                );
            }
            menu
        });
        div()
            .id("heap-picker")
            .test_support()
            .flex_none()
            .child(button)
            .into_any_element()
    }

    fn render_header(&self, record: &HeapRecord, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let mut col = div()
            .id("heap-header")
            .test_support()
            .flex_none()
            .flex()
            .flex_col()
            .w_full()
            .gap(space::S4)
            .py(space::S8)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border));
        let search = div()
            .flex_none()
            .w(layout::SEARCH_WIDTH)
            .min_w(layout::SEARCH_MIN)
            .child(Input::new(&self.input).cleanable(true));
        let mut top = div()
            .flex()
            .items_center()
            .gap(space::S8)
            .px(space::S16)
            .child(self.render_picker(Some(record.id), cx));
        match &record.summary {
            Some(summary) => {
                let h = header_text(record, summary);
                top = top.child(
                    div()
                        .id("heap-header-title")
                        .test_support()
                        .aria_label(SharedString::from(h.title.clone()))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(font::BODY)
                        .font_weight(font::SEMIBOLD)
                        .child(h.title.clone()),
                );
                top = top.child(search);
                col = col.child(top);
                if let Some(text) = h.unreachable {
                    col = col.child(self.text_line(
                        "heap-header-unreachable",
                        text,
                        Hsla::from(t.text_muted),
                    ));
                }
                if let Some(text) = h.http {
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::S8)
                            .child(self.text_line("heap-header-http", text, Hsla::from(t.text)))
                            .child(
                                Button::new("heap-http-settings")
                                    .label("Free app memory after copying requests")
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(OpenLocation(Location::at(Page::Settings)));
                                    })),
                            ),
                    );
                }
            }
            None => {
                top = top.child(div().flex_1()).child(search);
                col = col.child(top);
            }
        }
        col.into_any_element()
    }

    /// The search results, or the instances of the chosen class.
    fn render_results(&self, ready: &Ready, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let shell = |id: &'static str, rows: usize| {
            div()
                .id(id)
                .test_support()
                .flex_none()
                .w_full()
                .flex()
                .flex_col()
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .h(row::TABLE * rows as f32)
        };
        let name_of = |class: u32| {
            ready
                .summary
                .classes
                .get(class as usize)
                .map_or_else(|| "Object".to_string(), |c| c.name.to_string())
        };
        if let Some(class) = self.chosen_class {
            let title = format!("{} \u{b7} largest instances", name_of(class));
            let header = div()
                .flex()
                .items_center()
                .gap(space::S8)
                .flex_none()
                .h(row::TABLE)
                .px(space::S16)
                .child(
                    Button::new("heap-instances-back")
                        .label("Back")
                        .ghost()
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.back_to_classes(cx))),
                )
                .child(
                    div()
                        .id("heap-instances-title")
                        .test_support()
                        .aria_label(SharedString::from(title.clone()))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(font::BODY)
                        .child(title),
                );
            let Some((_, nodes)) = &self.instances else {
                let mut body = shell("heap-instances", RESULT_ROWS).child(header);
                if self.scan_started.is_some() {
                    body = body.child(
                        div()
                            .id("heap-instances-scanning")
                            .test_support()
                            .px(space::S16)
                            .text_size(font::BODY)
                            .text_color(Hsla::from(t.text_muted))
                            .child("Looking for the largest instances\u{2026}"),
                    );
                }
                return Some(body.into_any_element());
            };
            let nodes = nodes.clone();
            let rows = nodes.len().clamp(1, RESULT_ROWS - 1);
            let list = uniform_list(
                "heap-instance-rows",
                nodes.len(),
                cx.processor(move |this, range: Range<usize>, _w, cx| {
                    let Some(ready) = this.ready(cx) else {
                        return Vec::new();
                    };
                    let nodes = match &this.instances {
                        Some((_, n)) => n.clone(),
                        None => return Vec::new(),
                    };
                    let t = *cx.tokens();
                    let end = range.end.min(nodes.len());
                    (range.start.min(end)..end)
                        .map(|i| {
                            let (node, retained) = nodes[i];
                            let text = format!(
                                "{} \u{b7} {} \u{b7} #{node}",
                                elide(&node_name(&ready.index, &ready.summary, node)),
                                format_bytes(retained)
                            );
                            div()
                                .id(SharedString::from(format!("heap-instance-{i}")))
                                .test_support()
                                .aria_label(SharedString::from(text.clone()))
                                .flex()
                                .items_center()
                                .h(row::TABLE)
                                .px(space::S16)
                                .cursor_pointer()
                                .text_size(font::DENSE)
                                .text_color(Hsla::from(t.text))
                                .on_click(cx.listener(move |this, _, _, cx| this.go_to(node, cx)))
                                .child(div().min_w_0().truncate().child(text))
                                .into_any_element()
                        })
                        .collect()
                }),
            )
            .track_scroll(&self.results_scroll)
            .h(row::TABLE * rows as f32);
            return Some(
                shell("heap-instances", RESULT_ROWS)
                    .child(header)
                    .child(list)
                    .into_any_element(),
            );
        }
        if self.query.is_empty() {
            return None;
        }
        if self.matches.is_empty() {
            return Some(
                shell("heap-classes", 1)
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("heap-classes-none")
                            .test_support()
                            .aria_label(NO_MATCH)
                            .text_size(font::BODY)
                            .text_color(Hsla::from(t.text_muted))
                            .child(NO_MATCH),
                    )
                    .into_any_element(),
            );
        }
        let rows = self.matches.len().min(RESULT_ROWS);
        let list = uniform_list(
            "heap-class-rows",
            self.matches.len(),
            cx.processor(move |this, range: Range<usize>, _w, cx| {
                let Some(ready) = this.ready(cx) else {
                    return Vec::new();
                };
                let t = *cx.tokens();
                let end = range.end.min(this.matches.len());
                (range.start.min(end)..end)
                    .map(|i| {
                        let class = this.matches[i];
                        let c = &ready.summary.classes[class as usize];
                        let text = format!(
                            "{} \u{b7} {} \u{b7} {}",
                            elide(&c.name),
                            c.library,
                            group_digits(c.count)
                        );
                        div()
                            .id(SharedString::from(format!("heap-class-{i}")))
                            .test_support()
                            .aria_label(SharedString::from(text.clone()))
                            .flex()
                            .items_center()
                            .h(row::TABLE)
                            .px(space::S16)
                            .cursor_pointer()
                            .text_size(font::DENSE)
                            .text_color(Hsla::from(t.text))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.choose_class(class, cx)),
                            )
                            .child(div().min_w_0().truncate().child(text))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.results_scroll)
        .size_full();
        Some(shell("heap-classes", rows).child(list).into_any_element())
    }

    fn render_tree(&self, ready: &Ready, cx: &mut Context<Self>) -> AnyElement {
        crate::perf::bump(crate::perf::Counter::HeapTree, 1);
        let t = *cx.tokens();
        let header = |label: &'static str, w: gpui_kit::Pixels| {
            div().flex_none().w(w).text_right().child(label)
        };
        let (index, summary) = (ready.index.clone(), ready.summary.clone());
        let weak = cx.weak_entity();
        let list = tree(&self.tree_state, move |ix, entry, selected, _, cx| {
            let t = *cx.tokens();
            let item = entry.item();
            let Some(node) = item_node(&item.id) else {
                // A "more" row, or the placeholder of a closed node.
                let content = if item.label.is_empty() {
                    div().into_any_element()
                } else {
                    div()
                        .id(SharedString::from(format!("heap-more-{}", item.id)))
                        .test_support()
                        .aria_label(item.label.clone())
                        .pl(layout::TREE_INDENT * entry.depth() as f32 + layout::ICON_SM)
                        .text_size(font::DENSE)
                        .text_color(Hsla::from(t.text_muted))
                        .child(item.label.clone())
                        .into_any_element()
                };
                return ListItem::new(ix).child(content);
            };
            let r = row_text(&index, &summary, node);
            let glyph = if !entry.is_folder() {
                ""
            } else if entry.is_expanded() {
                "\u{25be}"
            } else {
                "\u{25b8}"
            };
            let num = |text: String, w: gpui_kit::Pixels| {
                div()
                    .flex_none()
                    .w(w)
                    .text_right()
                    .font_features(font::tabular_figures())
                    .text_color(Hsla::from(t.text_muted))
                    .child(text)
            };
            let mut cell = div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(space::S4)
                .pl(layout::TREE_INDENT * entry.depth() as f32)
                .child(
                    div()
                        .flex_none()
                        .w(layout::ICON_SM)
                        .text_color(Hsla::from(t.text_faint))
                        .child(glyph),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(font::MONO)
                        .text_color(Hsla::from(t.text))
                        .child(r.name.clone()),
                );
            if let Some(lib) = r.library.clone() {
                cell = cell.child(
                    div()
                        .truncate()
                        .text_color(Hsla::from(t.text_faint))
                        .child(lib),
                );
            }
            let label = format!("{} {} {} {}", r.name, r.retained, r.percent, r.shallow);
            let weak = weak.clone();
            let line = div()
                .id(SharedString::from(format!("heap-row-{node}")))
                .test_support()
                .aria_label(SharedString::from(label))
                .on_click(move |_, _, cx| {
                    weak.update(cx, |this, cx| this.select_node(node, cx)).ok();
                })
                .flex()
                .items_center()
                .gap(space::S8)
                .w_full()
                .text_size(font::DENSE)
                .child(cell)
                .child(num(r.retained, table::COL_BYTES))
                .child(num(r.percent, table::COL_PERCENT))
                .child(num(r.shallow, table::COL_BYTES));
            ListItem::new(ix).selected(selected).child(line)
        });
        div()
            .id("heap-tree-body")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(space::S8)
                    .h(row::LOG)
                    .px(space::S16)
                    .border_b(layout::BORDER)
                    .border_color(Hsla::from(t.border))
                    .text_size(font::LABEL)
                    .font_weight(font::SEMIBOLD)
                    .text_color(Hsla::from(t.text_muted))
                    .child(div().flex_1().child("Class"))
                    .child(header("Retained", table::COL_BYTES))
                    .child(header("Retained %", table::COL_PERCENT))
                    .child(header("Shallow", table::COL_BYTES)),
            )
            .child(
                div()
                    .id("heap-tree")
                    .test_support()
                    .key_context(HEAP_TREE_CONTEXT)
                    .on_action(cx.listener(|this, _: &OpenDetails, _, cx| this.open_details(cx)))
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px(space::S8)
                    .child(list),
            )
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        self.selection
            .filter(|s| matches!(s, Selection::HeapNode { .. }))?;
        let mut pane = div()
            .id("heap-details")
            .test_support()
            .w(layout::DETAILS_WIDTH)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .border_l(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .bg(Hsla::from(t.surface))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text));
        let Some((_, d)) = self.details.clone() else {
            if self.skeleton {
                pane = pane.child(
                    div()
                        .id("heap-details-skeleton")
                        .test_support()
                        .flex()
                        .flex_col()
                        .gap(space::S8)
                        .p(space::S12)
                        .children((0..6).map(|_| Skeleton::new().h(row::TABLE))),
                );
            } else if self.details_failed {
                pane = pane.child(self.text_line(
                    "heap-details-failed",
                    "Couldn't read this object from the snapshot file.".to_string(),
                    Hsla::from(t.danger),
                ));
            }
            return Some(pane.into_any_element());
        };
        let label = |text: &str| {
            div()
                .px(space::S12)
                .pt(space::S8)
                .pb(space::S4)
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(text.to_string())
        };
        let title = match &d.library {
            Some(lib) => format!("{} \u{b7} {lib}", d.class),
            None => d.class.clone(),
        };
        let sizes = format!(
            "Shallow {} \u{b7} Retained {} \u{b7} {} of the heap",
            d.shallow, d.retained, d.percent
        );
        pane = pane
            .child(
                div()
                    .id("heap-details-title")
                    .test_support()
                    .aria_label(SharedString::from(title.clone()))
                    .p(space::S12)
                    .font_weight(font::SEMIBOLD)
                    .child(elide(&title)),
            )
            .child(
                div()
                    .id("heap-details-sizes")
                    .test_support()
                    .aria_label(SharedString::from(sizes.clone()))
                    .px(space::S12)
                    .text_color(Hsla::from(t.text_muted))
                    .child(sizes),
            );
        if let Some(value) = &d.value {
            pane = pane.child(
                div()
                    .id("heap-details-value")
                    .test_support()
                    .aria_label(SharedString::from(value.clone()))
                    .px(space::S12)
                    .pt(space::S4)
                    .font_family(font::MONO)
                    .child(value.clone()),
            );
        }
        if !d.path.is_empty() {
            let shown = d.path.len().min(DETAIL_ROWS);
            pane = pane
                .child(label("Retaining path"))
                .child(
                    div().px(space::S12).child(
                        Button::new("heap-copy-path")
                            .label("Copy path")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| this.copy_path(cx))),
                    ),
                )
                .child(
                    div()
                        .id("heap-path")
                        .test_support()
                        .flex_none()
                        .w_full()
                        .h(row::LOG * shown as f32)
                        .child(
                            uniform_list(
                                "heap-path-rows",
                                d.path.len(),
                                cx.processor(|this, range: Range<usize>, _w, cx| {
                                    let Some((_, d)) = this.details.clone() else {
                                        return Vec::new();
                                    };
                                    let t = *cx.tokens();
                                    let end = range.end.min(d.path.len());
                                    (range.start.min(end)..end)
                                        .map(|i| {
                                            let h = &d.path[i];
                                            div()
                                                .id(SharedString::from(format!("heap-hop-{i}")))
                                                .test_support()
                                                .aria_label(SharedString::from(format!(
                                                    "{} {} {}",
                                                    h.class, h.edge, h.retained
                                                )))
                                                .flex()
                                                .items_center()
                                                .gap(space::S8)
                                                .h(row::LOG)
                                                .px(space::S12)
                                                .text_size(font::DENSE)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .child(elide(&h.class)),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .text_color(Hsla::from(t.text_muted))
                                                        .child(h.edge.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .font_features(font::tabular_figures())
                                                        .child(h.retained.clone()),
                                                )
                                                .into_any_element()
                                        })
                                        .collect()
                                }),
                            )
                            .track_scroll(&self.path_scroll)
                            .size_full(),
                        ),
                );
        }
        if !d.fields.is_empty() {
            let shown = d.fields.len().min(DETAIL_ROWS);
            pane = pane.child(label("Fields")).child(
                div()
                    .id("heap-fields")
                    .test_support()
                    .flex_none()
                    .w_full()
                    .h(row::LOG * shown as f32)
                    .child(
                        uniform_list(
                            "heap-field-rows",
                            d.fields.len(),
                            cx.processor(|this, range: Range<usize>, _w, cx| {
                                let Some((_, d)) = this.details.clone() else {
                                    return Vec::new();
                                };
                                let t = *cx.tokens();
                                let end = range.end.min(d.fields.len());
                                (range.start.min(end)..end)
                                    .map(|i| {
                                        let f = &d.fields[i];
                                        let value = f.value.clone().unwrap_or_default();
                                        let mut line = div()
                                            .id(SharedString::from(format!("heap-field-{i}")))
                                            .test_support()
                                            .aria_label(SharedString::from(format!(
                                                "{} {} {}",
                                                f.edge, f.class, value
                                            )))
                                            .flex()
                                            .items_center()
                                            .gap(space::S8)
                                            .h(row::LOG)
                                            .px(space::S12)
                                            .text_size(font::DENSE)
                                            .child(div().flex_none().child(f.edge.clone()))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(Hsla::from(t.text_muted))
                                                    .child(if value.is_empty() {
                                                        elide(&f.class)
                                                    } else {
                                                        format!("{} {}", elide(&f.class), value)
                                                    }),
                                            );
                                        if let Some(node) = f.node {
                                            line = line
                                                .cursor_pointer()
                                                .text_color(Hsla::from(t.accent))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.go_to(node, cx)
                                                }));
                                        }
                                        line.into_any_element()
                                    })
                                    .collect()
                            }),
                        )
                        .track_scroll(&self.fields_scroll)
                        .size_full(),
                    ),
            );
        }
        if d.fields_more > 0 {
            pane = pane.child(self.text_line(
                "heap-fields-more",
                format!("{} more fields", group_digits(u64::from(d.fields_more))),
                Hsla::from(t.text_muted),
            ));
        }
        Some(pane.into_any_element())
    }

    /// What the tab says while the shown snapshot has no index to browse.
    fn render_status(&self, record: &HeapRecord, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let mut col = div()
            .id("heap-status")
            .test_support()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::S8)
            .text_size(font::BODY);
        match &record.stage {
            HeapStage::Failed { message, details } => {
                col = col
                    .child(
                        div()
                            .id("heap-failed")
                            .test_support()
                            .aria_label(SharedString::from(message.to_string()))
                            .text_color(Hsla::from(t.danger))
                            .child(message.to_string()),
                    )
                    .child(
                        Button::new("heap-failed-toggle")
                            .label(if self.failed_open {
                                "Hide details"
                            } else {
                                "Details"
                            })
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.failed_open = !this.failed_open;
                                cx.notify();
                            })),
                    );
                if self.failed_open {
                    col = col.child(
                        div()
                            .id("heap-failed-details")
                            .test_support()
                            .text_size(font::LABEL)
                            .font_family(font::MONO)
                            .text_color(Hsla::from(t.text_muted))
                            .children(details.lines().take(20).map(|l| div().child(l.to_string()))),
                    );
                }
            }
            stage => {
                let text = stage_text(stage).unwrap_or_else(|| match stage {
                    HeapStage::Cancelled => "This snapshot was cancelled.".to_string(),
                    _ => "Opening this snapshot\u{2026}".to_string(),
                });
                col = col.child(
                    div()
                        .id("heap-stage")
                        .test_support()
                        .aria_label(SharedString::from(text.clone()))
                        .text_color(Hsla::from(t.text_muted))
                        .child(text),
                );
                let id = record.id;
                if !stage.is_busy()
                    && record.summary.is_some()
                    && record.index.is_none()
                    && matches!(stage, HeapStage::Ready)
                {
                    col = col.child(
                        Button::new("heap-open")
                            .label("Open snapshot")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selection = None;
                                this.pick(id, cx);
                            })),
                    );
                }
            }
        }
        col.into_any_element()
    }
}

impl Render for SnapshotTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(text) = self.fill_input.take() {
            self.input
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        let snap = self.snap(cx);
        let mut root = div()
            .id("snapshot-tab")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col();
        if snap.memory.heaps().is_empty() {
            return root.child(div().flex_1().flex().items_center().justify_center().child(
                EmptyState::new(
                    "memory-snapshot-empty",
                    crate::shell::sidebar::tool_icon(Tool::Memory),
                    EMPTY_HEADING,
                    empty_body(),
                ),
            ));
        }
        let Some(record) = shown_record(&snap.memory, self.selection).cloned() else {
            // Snapshots exist, but none has been analysed: say what the newest is doing.
            let newest = snap.memory.heaps().last().cloned();
            return match newest {
                Some(r) => root.child(self.render_status(&r, cx)),
                None => root,
            };
        };
        root = root.child(self.render_header(&record, cx));
        let ready = self.ready(cx);
        let tree_shown = ready.is_some();
        if self.focus_pending.due(window, cx, tree_shown) {
            self.tree_focus = true;
            self.tree_state.update(cx, |s, cx| s.focus(window, cx));
        } else if !tree_shown {
            let to = self.focus.clone();
            self.release_focus(&to, window, cx);
        }
        let Some(ready) = ready else {
            return root.child(self.render_status(&record, cx));
        };
        let mut left = div()
            .id("heap-main")
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col();
        left = left.children(self.render_results(&ready, cx));
        left = left.child(self.render_tree(&ready, cx));
        root.child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .child(left)
                .children(self.render_details(cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowSnapshotTab;
    use crate::test_support::{connected, exists, heap_record, open_shell_fake, press, snapshot};
    use fdt_analysis::heap::encode::SnapshotBuilder;
    use fdt_analysis::heap::format::Value;
    use fdt_store::heap::{HeapFile, HeapIndexParts, HttpHeld, LeakChecks};
    use fdt_store::run::Mode;
    use fdt_store::time::SessionTime;
    use gpui_kit::Focusable as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    /// A holder of three items (shallow 100, 300 and 200); the first holds a string.
    fn fixture(b: &mut SnapshotBuilder) {
        let holder = b.class("Holder", "package:app/holder.dart", &["items"]);
        let item = b.class("Item", "package:app/item.dart", &["name"]);
        let string = b.class("_OneByteString", "dart:core", &[]);
        let s = b.object(
            string,
            40,
            Value::Latin1 {
                len: 5,
                bytes: b"hello".to_vec(),
            },
            &[],
        );
        let a = b.object(item, 100, Value::None, &[s]);
        let big = b.object(item, 300, Value::None, &[]);
        let c = b.object(item, 200, Value::None, &[]);
        let h = b.object(holder, 50, Value::None, &[a, big, c]);
        b.static_field("holder", h);
    }

    fn class_ix(s: &HeapSummary, name: &str) -> u32 {
        s.classes
            .iter()
            .position(|c| &*c.name == name)
            .expect("the class") as u32
    }

    fn nodes_of(index: &HeapIndex, class: u32) -> Vec<u32> {
        (0..index.len() as u32)
            .filter(|&n| index.class_of(n) == class)
            .collect()
    }

    fn held(snap: &Snapshot) -> (Arc<HeapIndex>, Arc<HeapSummary>, HeapId) {
        let id = snap.memory.held().expect("a held snapshot");
        let r = snap.memory.heap(id).expect("the record");
        (
            r.index.clone().expect("index"),
            r.summary.clone().expect("summary"),
            id,
        )
    }

    fn connect(
        cx: &mut TestAppContext,
        snap: Arc<Snapshot>,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        &mut VisualTestContext,
        crate::test_support::FakeHandle,
    ) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.snaps.send(snap).unwrap();
        fake.status.send(connected("demo", false)).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-3", "ctrl-3");
        cx.dispatch_action(ShowSnapshotTab);
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        (shell, cx, fake)
    }

    fn tab(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
    ) -> Entity<SnapshotTab> {
        shell.read_with(cx, |s, cx| s.memory_view().read(cx).snapshot().clone())
    }

    fn go(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
        selection: Option<Selection>,
    ) {
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = selection;
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn label(cx: &mut VisualTestContext, id: &'static str) -> Option<String> {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find(id.to_string())
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn click(cx: &mut VisualTestContext, id: &str) {
        let id = id.to_string();
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn rows(t: &Entity<SnapshotTab>, cx: &mut VisualTestContext) -> Vec<(String, usize, String)> {
        t.read_with(cx, |t, cx| t.tree_rows(cx))
    }

    fn one() -> Arc<Snapshot> {
        snapshot(|w, run| {
            heap_record(w, run, fixture);
        })
    }

    /// An index of `kids` leaves under one node under the root, for the cap.
    fn wide_index(kids: u32) -> HeapIndex {
        let n = kids as usize + 2;
        let mut retained = vec![1u64; n];
        retained[0] = u64::from(kids) + 2;
        retained[1] = u64::from(kids) + 1;
        let dir = std::env::temp_dir().join(format!("fdt-app-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        HeapIndex::from_parts(HeapIndexParts {
            orig: (0..n as u32).collect(),
            class: vec![0; n],
            shallow: vec![1; n],
            retained,
            first_child: {
                // node 1 has children 2..kids+2; the leaves have none.
                let mut f = vec![0u32; n + 1];
                f[0] = 1;
                f[1] = 2;
                for slot in f.iter_mut().skip(2) {
                    *slot = kids + 2;
                }
                f
            },
            retainer: vec![0; n],
            offsets: Vec::new(),
            mint_class: None,
            names: Vec::new(),
            file: Arc::new(HeapFile::new(dir.join("wide.dartheap"))),
        })
    }

    fn bare_summary() -> HeapSummary {
        HeapSummary {
            objects: 10,
            reachable: 10,
            unreachable_bytes: 0,
            reachable_bytes: 4_096,
            header_shallow: 0,
            header_capacity: 0,
            header_external: 0,
            mode: Mode::Profile,
            root_package: None,
            classes: Vec::new().into(),
            findings: Vec::new().into(),
            findings_total: 0,
            checks: LeakChecks {
                states: fdt_store::heap::Check::Done,
                elements: fdt_store::heap::Check::Done,
                notifiers: fdt_store::heap::Check::Done,
            },
            http: None,
            tracked: Vec::new().into(),
        }
    }

    #[test]
    fn expand_caps_at_500() {
        let index = wide_index(1_000_000);
        let summary = bare_summary();
        let none = HashSet::new();
        let top = level_items(&index, &summary, 0, &none, &none);
        assert_eq!(top.len(), 1, "the root's one child");
        let open: HashSet<u32> = [1].into();
        let top = level_items(&index, &summary, 0, &open, &none);
        let kids = &top[0].children;
        assert_eq!(kids.len(), MAX_CHILDREN + 1, "500 children and a more row");
        assert_eq!(kids[MAX_CHILDREN].label, "999,500 more");
        assert!(kids[MAX_CHILDREN].is_disabled());
        assert_eq!(kids[0].id, "2");
        assert_eq!(kids[MAX_CHILDREN - 1].id, "501");
    }

    #[test]
    fn a_pinned_child_past_the_cap_is_shown_too() {
        let index = wide_index(1_000);
        let summary = bare_summary();
        let open: HashSet<u32> = [1].into();
        let pinned: HashSet<u32> = [1, 700].into();
        let top = level_items(&index, &summary, 0, &open, &pinned);
        let ids: Vec<&str> = top[0].children.iter().map(|c| &*c.id).collect();
        assert_eq!(ids.len(), MAX_CHILDREN + 2);
        assert_eq!(ids[MAX_CHILDREN], "700");
        assert_eq!(top[0].children[MAX_CHILDREN + 1].label, "499 more");
    }

    #[test]
    fn largest_instances_keep_the_biggest_by_retained() {
        let snap = one();
        let (index, summary, _) = held(&snap);
        let item = class_ix(&summary, "Item");
        let all = largest_instances(&index, item, 10);
        let retained: Vec<u64> = all.iter().map(|&(_, r)| r).collect();
        assert_eq!(all.len(), 3);
        assert!(retained.windows(2).all(|w| w[0] >= w[1]), "{retained:?}");
        let two = largest_instances(&index, item, 2);
        assert_eq!(two, all[..2]);
    }

    #[test]
    fn matching_ignores_case_and_looks_at_libraries() {
        let snap = one();
        let (_, summary, _) = held(&snap);
        let keys = class_keys(&summary);
        let item = class_ix(&summary, "Item");
        let holder = class_ix(&summary, "Holder");
        assert_eq!(
            matching_classes(&summary, &keys, "ITEM".to_lowercase().as_str()),
            [item]
        );
        assert_eq!(matching_classes(&summary, &keys, "holder.dart"), [holder]);
        assert!(matching_classes(&summary, &keys, "").is_empty());
        let both = matching_classes(&summary, &keys, "package:app");
        assert_eq!(both.len(), 2);
    }

    #[gpui_kit::test]
    fn root_children_by_retained(cx: &mut TestAppContext) {
        let snap = one();
        let (index, _, _) = held(&snap);
        let (shell, cx, _fake) = connect(cx, snap);
        let tab = tab(&shell, cx);
        let rows = rows(&tab, cx);
        let top: Vec<String> = index.children(0).map(|n| n.to_string()).collect();
        let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        assert_eq!(ids, top);
        assert!(rows.iter().all(|r| r.1 == 0));
        let retained: Vec<u64> = index.children(0).map(|n| index.retained(n)).collect();
        assert!(retained.windows(2).all(|w| w[0] >= w[1]), "{retained:?}");
        assert!(exists(cx, "heap-tree"));
        assert!(exists(cx, "heap-header-title"));
    }

    #[gpui_kit::test]
    fn the_tree_shows_a_node_with_a_million_children_as_500_and_more(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let at = SessionTime(1_000_000_000);
            let id = w.heap_requested(run, "isolates/1", at);
            let dir = std::env::temp_dir().join(format!("fdt-app-wide-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let file = Arc::new(HeapFile::new(dir.join("ui.dartheap")));
            w.set_heap_received(id, at, 1, file);
            let index = wide_index(1_000_000);
            w.set_heap_analysed(id, Arc::new(bare_summary()), Arc::new(index));
            w.set_heap_stage(id, HeapStage::Ready);
        });
        let (shell, cx, _fake) = connect(cx, snap);
        let tab = tab(&shell, cx);
        assert_eq!(rows(&tab, cx).len(), 1);
        click(cx, "heap-row-1");
        let rows = rows(&tab, cx);
        assert_eq!(rows.len(), 1 + MAX_CHILDREN + 1);
        assert_eq!(rows.last().unwrap().2, "999,500 more");
        assert_eq!(rows[1].1, 1);
    }

    #[gpui_kit::test]
    fn search_lists_classes_then_instances(cx: &mut TestAppContext) {
        let snap = one();
        let (index, summary, _) = held(&snap);
        let item = class_ix(&summary, "Item");
        let (shell, cx, _fake) = connect(cx, snap);
        let tab = tab(&shell, cx);
        tab.update(cx, |t, cx| t.set_search_text("  ITEM ", cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(tab.read_with(cx, |t, _| t.query().to_string()), "item");
        assert_eq!(tab.read_with(cx, |t, _| t.matches().to_vec()), [item]);
        let text = label(cx, "heap-class-0").expect("a class row");
        assert_eq!(text, "Item \u{b7} package:app/item.dart \u{b7} 3");
        click(cx, "heap-class-0");
        let found = tab.read_with(cx, |t, _| t.instances()).expect("scanned");
        assert_eq!(found.len(), 3);
        let retained: Vec<u64> = found.iter().map(|&(_, r)| r).collect();
        assert_eq!(retained[0], index.retained(found[0].0));
        assert!(retained.windows(2).all(|w| w[0] >= w[1]));
        assert!(exists(cx, "heap-instance-0"));
        assert!(label(cx, "heap-instance-0").unwrap().starts_with("Item"));
        click(cx, "heap-instances-back");
        assert!(exists(cx, "heap-class-0"));
        tab.update(cx, |t, cx| t.set_search_text("zzz", cx));
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(label(cx, "heap-classes-none").as_deref(), Some(NO_MATCH));
    }

    #[gpui_kit::test]
    fn choosing_an_instance_reveals_it(cx: &mut TestAppContext) {
        let snap = one();
        let (index, summary, heap) = held(&snap);
        let item = class_ix(&summary, "Item");
        let (shell, cx, _fake) = connect(cx, snap);
        let tab = tab(&shell, cx);
        tab.update(cx, |t, cx| t.set_search_text("item", cx));
        click(cx, "heap-class-0");
        let (node, _) = tab.read_with(cx, |t, _| t.instances()).unwrap()[0];
        assert_eq!(index.class_of(node), item);
        click(cx, "heap-instance-0");
        let chain = ancestry(&index, node).unwrap();
        assert!(chain.len() >= 2, "{chain:?}");
        let open = tab.read_with(cx, |t, _| t.open_nodes());
        for n in &chain[..chain.len() - 1] {
            assert!(open.contains(n), "ancestor {n} is open: {open:?}");
        }
        let want = Selection::HeapNode { heap, node };
        assert_eq!(tab.read_with(cx, |t, _| t.selection()), Some(want));
        assert_eq!(
            tab.read_with(cx, |t, cx| t.tree_selected(cx)),
            Some(node.to_string())
        );
        let rows = rows(&tab, cx);
        let at = rows
            .iter()
            .position(|r| r.0 == node.to_string())
            .expect("the row");
        assert_eq!(rows[at].1, chain.len() - 1, "indented by its depth");
        // The location carries the selection, so Back and links work.
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(want)
        );
        // The details follow.
        assert_eq!(
            tab.read_with(cx, |t, _| t.details().map(|d| d.node)),
            Some(node)
        );
    }

    #[gpui_kit::test]
    fn a_reveal_from_another_tab_waits_for_the_snapshot(cx: &mut TestAppContext) {
        let (mut w, run) = crate::test_support::writer();
        let before = w.publish();
        let heap = heap_record(&mut w, run, fixture);
        let after = w.publish();
        let (index, summary, _) = held(&after);
        let node = nodes_of(&index, class_ix(&summary, "Item"))[0];
        let (shell, cx, fake) = connect(cx, before);
        go(&shell, cx, Some(Selection::HeapNode { heap, node }));
        let t = tab(&shell, cx);
        assert!(rows(&t, cx).is_empty());
        // The record arrives and is analysed.
        fake.snaps.send(after).unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(
            t.read_with(cx, |t, cx| t.tree_selected(cx)),
            Some(node.to_string())
        );
    }

    #[gpui_kit::test]
    fn details_read_fields_and_path_off_the_ui_thread(cx: &mut TestAppContext) {
        let snap = one();
        let (index, summary, heap) = held(&snap);
        let item = class_ix(&summary, "Item");
        let with_field = nodes_of(&index, item)
            .into_iter()
            .find(|&n| index.retained(n) == 140)
            .expect("the item that holds the string");
        let (shell, cx, _fake) = connect(cx, snap);
        let t = tab(&shell, cx);
        go(
            &shell,
            cx,
            Some(Selection::HeapNode {
                heap,
                node: with_field,
            }),
        );
        let d = t.read_with(cx, |t, _| t.details()).expect("read");
        assert_eq!(d.class, "Item");
        assert_eq!(d.library.as_deref(), Some("item.dart"));
        assert_eq!(d.retained, format_bytes(140));
        assert_eq!(d.shallow, format_bytes(100));
        assert_eq!(d.fields.len(), 1);
        assert_eq!(d.fields[0].value.as_deref(), Some("\"hello\""));
        assert!(d.fields[0].node.is_some());
        assert_eq!(d.fields_more, 0);
        assert!(d.path.len() >= 3, "{:?}", d.path);
        assert!(d.path_text.ends_with("Item"), "{}", d.path_text);
        assert_eq!(d.path.last().unwrap().class, "Item");
        assert!(label(cx, "heap-details-title").unwrap().starts_with("Item"));
        assert!(exists(cx, "heap-copy-path"));
        assert!(exists(cx, "heap-field-0"));
        // A field with a node links to it.
        let target = d.fields[0].node.unwrap();
        click(cx, "heap-field-0");
        assert_eq!(
            t.read_with(cx, |t, _| t.selection()),
            Some(Selection::HeapNode { heap, node: target })
        );
        assert_eq!(
            t.read_with(cx, |t, _| t.details().map(|d| d.class.clone()))
                .as_deref(),
            Some("_OneByteString")
        );
        assert_eq!(
            label(cx, "heap-details-value").as_deref(),
            Some("\"hello\"")
        );
    }

    #[gpui_kit::test]
    fn the_details_job_is_not_run_on_the_ui_thread_and_stale_results_drop(cx: &mut TestAppContext) {
        let snap = one();
        let (index, summary, heap) = held(&snap);
        let items = nodes_of(&index, class_ix(&summary, "Item"));
        let (shell, cx, _fake) = connect(cx, snap);
        let t = tab(&shell, cx);
        t.update(cx, |t, cx| {
            t.set_selection(
                Some(Selection::HeapNode {
                    heap,
                    node: items[0],
                }),
                cx,
            );
            t.set_active(false, cx);
            t.set_active(true, cx);
        });
        assert!(
            t.read_with(cx, |t, _| t.details()).is_none(),
            "the read has not returned yet: it runs off the UI thread"
        );
        t.update(cx, |t, cx| {
            t.set_selection(
                Some(Selection::HeapNode {
                    heap,
                    node: items[1],
                }),
                cx,
            )
        });
        cx.run_until_parked();
        let d = t.read_with(cx, |t, _| t.details()).expect("read");
        assert_eq!(d.node, items[1], "the older selection's result was dropped");
    }

    #[test]
    fn header_lines_for_unreachable_and_http() {
        let snap = snapshot(|w, run| {
            heap_record(w, run, fixture);
        });
        let (_, summary, id) = held(&snap);
        let record = snap.memory.heap(id).unwrap();
        let h = header_text(record, &summary);
        assert!(h.title.starts_with("Snapshot #1 \u{b7} "), "{}", h.title);
        assert!(
            h.title.ends_with(&format!(
                " \u{b7} {} objects \u{b7} {}",
                group_digits(summary.reachable),
                format_bytes(summary.reachable_bytes)
            )),
            "{}",
            h.title
        );
        assert_eq!(h.http, None);
        let mut s = (*summary).clone();
        s.objects = s.reachable + 1_204;
        s.unreachable_bytes = 2 * 1024 * 1024;
        s.http = Some(HttpHeld {
            bytes: 3 * 1024 * 1024,
            requests: 12,
        });
        let h = header_text(record, &s);
        assert_eq!(
            h.unreachable.as_deref(),
            Some(
                format!(
                    "1,204 unreachable objects ({}) were waiting for garbage collection and are left out.",
                    format_bytes(2 * 1024 * 1024)
                )
                .as_str()
            )
        );
        assert_eq!(
            h.http.as_deref(),
            Some(
                format!(
                    "Held by HTTP recording: {} in 12 requests.",
                    format_bytes(3 * 1024 * 1024)
                )
                .as_str()
            )
        );
        // Without garbage there is no line.
        s.objects = s.reachable;
        assert_eq!(header_text(record, &s).unreachable, None);
    }

    #[gpui_kit::test]
    fn the_header_shows_its_lines_and_the_link_opens_settings(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            crate::test_support::heap_record_with(w, run, fixture, |s| {
                s.objects = s.reachable + 7;
                s.unreachable_bytes = 1_024;
                s.http = Some(HttpHeld {
                    bytes: 2_048,
                    requests: 3,
                });
            });
        });
        let (shell, cx, _fake) = connect(cx, snap);
        assert!(
            label(cx, "heap-header-unreachable")
                .unwrap()
                .starts_with("7 unreachable objects")
        );
        assert_eq!(
            label(cx, "heap-header-http").unwrap(),
            format!(
                "Held by HTTP recording: {} in 3 requests.",
                format_bytes(2_048)
            )
        );
        click(cx, "heap-http-settings");
        assert_eq!(shell.read_with(cx, |s, _| s.current().page), Page::Settings);
    }

    #[gpui_kit::test]
    fn picking_an_older_snapshot_opens_it(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            heap_record(w, run, fixture);
            heap_record(w, run, fixture);
        });
        assert_eq!(snap.memory.held(), Some(HeapId(1)));
        let (shell, cx, fake) = connect(cx, snap);
        let t = tab(&shell, cx);
        assert!(fake.commands().iter().all(|c| !c.starts_with("heap_open")));
        t.update(cx, |t, cx| t.pick(HeapId(0), cx));
        cx.run_until_parked();
        for _ in 0..3 {
            cx.update(|window, cx| window.render_frame(cx));
        }
        let opens: Vec<String> = fake
            .commands()
            .into_iter()
            .filter(|c| c.starts_with("heap_open"))
            .collect();
        assert_eq!(opens, ["heap_open 0"], "once, not on every render");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(Selection::Heap(HeapId(0)))
        );
        // The held snapshot is not opened again.
        t.update(cx, |t, cx| t.pick(HeapId(1), cx));
        let opens = fake
            .commands()
            .into_iter()
            .filter(|c| c.starts_with("heap_open"))
            .count();
        assert_eq!(opens, 1);
    }

    #[gpui_kit::test]
    fn a_palette_selection_of_an_older_snapshot_opens_it_and_shows_its_stages(
        cx: &mut TestAppContext,
    ) {
        let snap = snapshot(|w, run| {
            heap_record(w, run, fixture);
            heap_record(w, run, fixture);
        });
        let (shell, cx, fake) = connect(cx, snap.clone());
        go(&shell, cx, Some(Selection::Heap(HeapId(0))));
        for _ in 0..3 {
            cx.update(|window, cx| window.render_frame(cx));
        }
        assert_eq!(
            fake.commands()
                .iter()
                .filter(|c| *c == "heap_open 0")
                .count(),
            1
        );
        // The capture analyses it: its stage shows while it does.
        let busy = snapshot(|w, run| {
            heap_record(w, run, fixture);
            let second = heap_record(w, run, fixture);
            w.set_heap_stage(second, HeapStage::Parsing { fraction: 0.5 });
        });
        fake.snaps.send(busy).unwrap();
        cx.run_until_parked();
        go(&shell, cx, Some(Selection::Heap(HeapId(1))));
        assert_eq!(
            label(cx, "heap-stage").as_deref(),
            Some("Reading \u{b7} 50 %")
        );
    }

    #[gpui_kit::test]
    fn keyboard_moves_and_expands(cx: &mut TestAppContext) {
        let snap = one();
        let (index, _, heap) = held(&snap);
        let (shell, cx, _fake) = connect(cx, snap);
        let t = tab(&shell, cx);
        let top: Vec<u32> = index.children(0).collect();
        // The tree took the focus when the tab opened; Down and Up move the selection.
        assert!(top.len() > 1);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        // With nothing selected the tree's first Down lands on the second row.
        assert_eq!(
            t.read_with(cx, |t, cx| t.tree_selected(cx)),
            Some(top[1].to_string())
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            t.read_with(cx, |t, cx| t.tree_selected(cx)),
            Some(top[0].to_string())
        );
        let first = top[0];
        let expandable = !index.children(first).is_empty();
        assert!(expandable, "the first root slice has children");
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert!(rows(&t, cx).len() > top.len(), "right expands");
        assert_eq!(t.read_with(cx, |t, _| t.open_nodes()), [first]);
        // Enter opens the details of the selected row.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            t.read_with(cx, |t, _| t.selection()),
            Some(Selection::HeapNode { heap, node: first })
        );
        assert_eq!(
            t.read_with(cx, |t, _| t.details().map(|d| d.node)),
            Some(first)
        );
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        assert_eq!(rows(&t, cx).len(), top.len(), "left collapses");
    }

    #[gpui_kit::test]
    fn command_f_focuses_the_search(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, one());
        let t = tab(&shell, cx);
        press(cx, "cmd-f", "ctrl-f");
        let focused = cx.update(|window, cx| {
            let input = t.read(cx).input.read(cx).focus_handle(cx);
            input.is_focused(window)
        });
        assert!(focused);
    }

    #[test]
    fn long_names_elide() {
        let long = "W".repeat(200);
        let e = elide(&long);
        assert_eq!(e.chars().count(), 80);
        assert!(e.ends_with('\u{2026}'));
        assert_eq!(elide("short"), "short");
        assert_eq!(elide(&"\u{e9}".repeat(80)), "\u{e9}".repeat(80));
        assert_eq!(elide(&"\u{e9}".repeat(81)).chars().count(), 80);
    }

    #[gpui_kit::test]
    fn long_names_elide_in_the_rows(cx: &mut TestAppContext) {
        let long = "W".repeat(200);
        let snap = snapshot(|w, run| {
            heap_record(w, run, |b| {
                let c = b.class(&long, "package:app/w.dart", &[]);
                let o = b.object(c, 64, Value::None, &[]);
                b.static_field("w", o);
            });
        });
        let (index, summary, _) = held(&snap);
        let node = nodes_of(&index, class_ix(&summary, &long))[0];
        let (shell, cx, _fake) = connect(cx, snap);
        let t = tab(&shell, cx);
        go(
            &shell,
            cx,
            Some(Selection::HeapNode {
                heap: HeapId(0),
                node,
            }),
        );
        let row = rows(&t, cx)
            .into_iter()
            .find(|r| r.0 == node.to_string())
            .expect("the row is revealed");
        assert_eq!(row.2.chars().count(), 80, "{}", row.2);
        assert!(row.2.ends_with('\u{2026}'));
        let name = row_text(&index, &summary, node).name;
        assert_eq!(name, row.2);
    }

    #[gpui_kit::test]
    fn the_empty_state_says_what_to_do(cx: &mut TestAppContext) {
        let (_shell, cx, _fake) = connect(cx, snapshot(|_, _| {}));
        assert!(exists(cx, "memory-snapshot-empty"));
        assert_eq!(EMPTY_HEADING, "No heap snapshot yet");
        let keys = if cfg!(target_os = "macos") {
            "\u{2318}\u{21e7}S"
        } else {
            "Ctrl+Shift+S"
        };
        assert_eq!(
            empty_body(),
            format!("Take one ({keys}) to browse what holds your app's memory.")
        );
    }

    #[gpui_kit::test]
    fn removing_the_selected_snapshot_clears_the_tab(cx: &mut TestAppContext) {
        let (mut w, run) = crate::test_support::writer();
        let heap = heap_record(&mut w, run, fixture);
        let with = w.publish();
        w.remove_heap(heap);
        let without = w.publish();
        let (shell, cx, fake) = connect(cx, with);
        let t = tab(&shell, cx);
        go(&shell, cx, Some(Selection::Heap(heap)));
        assert_eq!(
            t.read_with(cx, |t, _| t.selection()),
            Some(Selection::Heap(heap))
        );
        fake.snaps.send(without).unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(t.read_with(cx, |t, _| t.selection()), None);
        assert!(rows(&t, cx).is_empty());
        assert!(exists(cx, "memory-snapshot-empty"));
    }
}
