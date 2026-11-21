//! The CPU tab: a toolbar over the bottom-up and top-down call trees or the flame chart.
//!
//! Everything is computed in `cx.background_spawn` from the CPU store for the current time
//! range: the call tree (or the flame spans) and the filter. The `Tree` component's items hold
//! `Rc`s and cannot cross threads, so the UI thread only turns the finished tree into items, and
//! only for the nodes the user has opened (a closed node holds one placeholder child, so it
//! shows as a folder). Opening a node reports `TreeEvent::Expanded`, which rebuilds the items
//! with that node's children. The open nodes are kept by id, so they stay open through every
//! recompute.

use super::flame::{FlameChart, FlameData, FlameEvent, MAX_SPANS, RECENT_NOTE, RECENT_NS};
use crate::nav::TimeRange;
use crate::session_model::SessionModel;
use crate::tools::network::model::format_duration_ns;
use fdt_analysis::cpu_tree::{CallTree, bottom_up, flame_spans, period_for, top_down};
use fdt_analysis::user_code::is_user_library;
use fdt_capture::ConnStatus;
use fdt_store::Snapshot;
use fdt_store::cpu::{CpuSnapshot, CpuStatus, DEFAULT_PERIOD_US};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, motion, radius, row, space, table};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, anchored, deferred, div, point,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The most children a node lists; the rest collapse into one "{n} more" row.
const MAX_CHILDREN: usize = 500;
/// The shortest time between two recomputes for new samples, as for the sidebar counts.
const REFRESH_INTERVAL: Duration = Duration::from_millis(250); // token-check: allow: throttle, not a visual duration
/// How many nodes are visited between two looks at the cancel flag.
const CANCEL_STRIDE: usize = 4096;
const NONE_COPY: &str =
    "No CPU samples in this range. Samples are taken while your app's UI thread is busy.";
const SEP: &str = " \u{b7} ";

/// What the tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuMode {
    BottomUp,
    TopDown,
    Flame,
}

impl CpuMode {
    const ALL: [(CpuMode, &'static str, &'static str); 3] = [
        (CpuMode::BottomUp, "cpu-view-bottom-up", "Bottom up"),
        (CpuMode::TopDown, "cpu-view-top-down", "Top down"),
        (CpuMode::Flame, "cpu-view-flame", "Flame chart"),
    ];
}

/// What a recompute depends on besides the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    /// Run ids restart in every session.
    session: u64,
    range: Option<TimeRange>,
    mode: CpuMode,
    /// Lower case; empty for no filter, and always empty for the flame chart.
    filter: String,
}

/// A call tree with the filter applied, and what its rows need.
struct TreeData {
    snap: Arc<Snapshot>,
    tree: CallTree,
    /// Per node: it matches the filter or has a descendant that does. `None` for no filter.
    keep: Option<Vec<bool>>,
    period_us: u32,
}

enum Body {
    Tree(TreeData),
    Flame(Arc<FlameData>),
}

struct Computed {
    key: Key,
    snap: Arc<Snapshot>,
    body: Body,
}

/// What a tree row shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: SharedString,
    /// The library's file name, for the muted text after the name.
    pub library: Option<SharedString>,
    pub user: bool,
    pub self_pct: f32,
    pub total_pct: f32,
    pub self_ms: f64,
    pub total_ms: f64,
    /// `Some(n)` for the "{n} more" row; `Some(0)` for the placeholder of a closed node.
    pub more: Option<usize>,
}

/// The nodes that match `text` (case-insensitive, in the function name) and their ancestors.
/// `None` when cancelled.
pub(super) fn filter_keep(
    tree: &CallTree,
    cpu: &CpuSnapshot,
    text: &str,
    cancel: &AtomicBool,
) -> Option<Vec<bool>> {
    let needle = text.to_lowercase();
    let mut matches: Vec<Option<bool>> = vec![None; cpu.function_count()];
    let mut keep = vec![false; tree.nodes.len()];
    // Post-order without recursion: a node is decided after its children.
    let mut stack: Vec<(u32, bool)> = tree.roots.iter().map(|&r| (r, false)).collect();
    let mut visited = 0usize;
    while let Some((id, done)) = stack.pop() {
        visited += 1;
        if visited.is_multiple_of(CANCEL_STRIDE) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let node = &tree.nodes[id as usize];
        if !done {
            stack.push((id, true));
            stack.extend(node.children.iter().map(|&c| (c, false)));
            continue;
        }
        let f = node.function.0 as usize;
        let named = *matches[f].get_or_insert_with(|| {
            cpu.function(node.function)
                .name
                .to_lowercase()
                .contains(&needle)
        });
        keep[id as usize] = named || node.children.iter().any(|&c| keep[c as usize]);
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(keep)
}

/// The sample period of `range`, in microseconds.
fn period_us(snap: &Snapshot, range: Option<TimeRange>) -> u32 {
    match range {
        Some(r) => period_for(snap, r),
        None => snap
            .runs
            .last()
            .map_or(DEFAULT_PERIOD_US, |r| snap.cpu.period_us(r.id)),
    }
}

/// The first and last sample time of the whole session.
fn sample_span(snap: &Snapshot) -> Option<TimeRange> {
    let samples = snap.cpu.samples();
    let first = samples.get(0)?.time;
    let last = samples.get(samples.len() - 1)?.time;
    Some(TimeRange::new(first, last))
}

fn compute(snap: &Arc<Snapshot>, key: &Key, cancel: &AtomicBool) -> Option<Computed> {
    let period = period_us(snap, key.range);
    let body = match key.mode {
        CpuMode::TopDown | CpuMode::BottomUp => {
            let tree = if key.mode == CpuMode::TopDown {
                top_down(&snap.cpu, key.range, cancel)?
            } else {
                bottom_up(&snap.cpu, key.range, cancel)?
            };
            let keep = if key.filter.is_empty() {
                None
            } else {
                Some(filter_keep(&tree, &snap.cpu, &key.filter, cancel)?)
            };
            Body::Tree(TreeData {
                snap: snap.clone(),
                tree,
                keep,
                period_us: period,
            })
        }
        CpuMode::Flame => {
            // No range: the last 10 seconds of samples, not the session's first part.
            let (window, recent) = match key.range {
                Some(r) => (Some(r), false),
                None => match sample_span(snap) {
                    None => (None, false),
                    Some(all) => {
                        let start = all.start.0.max(all.end.0.saturating_sub(RECENT_NS));
                        let w = TimeRange::new(fdt_store::time::SessionTime(start), all.end);
                        (Some(w), start > all.start.0)
                    }
                },
            };
            let data = match window {
                None => FlameData::default(),
                Some(r) => {
                    let (spans, truncated) = flame_spans(snap, r, MAX_SPANS);
                    let bounds = match key.range {
                        Some(_) => (r.start.0, r.end.0),
                        None => (r.start.0, r.end.0 + u64::from(period) * 1_000),
                    };
                    let mut data = FlameData::new(snap, spans, truncated, bounds, period);
                    data.recent = recent;
                    data
                }
            };
            Body::Flame(Arc::new(data))
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(Computed {
        key: key.clone(),
        snap: snap.clone(),
        body,
    })
}

/// The file name of a library URI.
fn basename(library: &str) -> &str {
    library.rsplit('/').next().unwrap_or(library)
}

fn placeholder(more: usize) -> Row {
    Row {
        name: SharedString::default(),
        library: None,
        user: false,
        self_pct: 0.0,
        total_pct: 0.0,
        self_ms: 0.0,
        total_ms: 0.0,
        more: Some(more),
    }
}

/// Where the rows of a tree level go, and what they are measured against.
struct Level<'a> {
    data: &'a TreeData,
    open: &'a HashSet<SharedString>,
    rows: &'a mut HashMap<SharedString, Row>,
    total: f32,
}

impl Level<'_> {
    fn visible(&self, node: u32) -> bool {
        self.data.keep.as_ref().is_none_or(|k| k[node as usize])
    }

    /// The items for `siblings` (node indices) under the item `parent`.
    fn items(&mut self, siblings: &[u32], parent: Option<&str>) -> Vec<TreeItem> {
        let shown: Vec<u32> = siblings
            .iter()
            .copied()
            .filter(|&n| self.visible(n))
            .collect();
        let period_ms = f64::from(self.data.period_us) / 1000.0;
        let mut items = Vec::new();
        for &n in shown.iter().take(MAX_CHILDREN) {
            let node = &self.data.tree.nodes[n as usize];
            let f = self.data.snap.cpu.function(node.function);
            let id: SharedString = match parent {
                Some(p) => format!("{p}/{}", node.function.0).into(),
                None => node.function.0.to_string().into(),
            };
            self.rows.insert(
                id.clone(),
                Row {
                    name: SharedString::from(f.name.to_string()),
                    library: f
                        .library
                        .as_deref()
                        .map(|l| SharedString::from(basename(l).to_string())),
                    user: is_user_library(f.library.as_deref()),
                    self_pct: node.self_count as f32 * 100.0 / self.total,
                    total_pct: node.total_count as f32 * 100.0 / self.total,
                    self_ms: node.self_count as f64 * period_ms,
                    total_ms: node.total_count as f64 * period_ms,
                    more: None,
                },
            );
            let mut item = TreeItem::new(id.clone(), f.name.to_string());
            if node.children.iter().any(|&c| self.visible(c)) {
                if self.open.contains(&id) {
                    let children = self.items(&node.children, Some(&id));
                    item = item.expanded(true).children(children);
                } else {
                    let ph: SharedString = format!("{id}/\u{2026}").into();
                    self.rows.insert(ph.clone(), placeholder(0));
                    item = item.child(TreeItem::new(ph, "").disabled(true));
                }
            }
            items.push(item);
        }
        if shown.len() > MAX_CHILDREN {
            let rest = shown.len() - MAX_CHILDREN;
            let id: SharedString = match parent {
                Some(p) => format!("{p}/more").into(),
                None => "more".into(),
            };
            self.rows.insert(id.clone(), placeholder(rest));
            items.push(TreeItem::new(id, format!("{rest} more")).disabled(true));
        }
        items
    }
}

/// The items for `data`, with children only for the nodes in `open`, and the row of each item.
fn build_items(
    data: &TreeData,
    open: &HashSet<SharedString>,
    rows: &mut HashMap<SharedString, Row>,
) -> Vec<TreeItem> {
    let mut level = Level {
        data,
        open,
        rows,
        total: data.tree.samples.max(1) as f32,
    };
    level.items(&data.tree.roots, None)
}

fn pct_text(v: f32) -> String {
    format!("{v:.1}%")
}

fn ms_text(v: f64) -> String {
    format!("{v:.1}")
}

/// A toggle that is tinted when on and plain when off.
fn chip(
    id: &'static str,
    label: &'static str,
    on: bool,
    accent: Hsla,
    off: Hsla,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let base = div()
        .id(id)
        .test_support()
        .aria_label(label)
        .flex_none()
        .px(space::S8)
        .py(space::S2)
        .rounded(radius::CHIP)
        .text_size(font::LABEL)
        .cursor_pointer();
    let base = if on {
        base.bg(accent.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
            .text_color(accent)
    } else {
        base.text_color(off)
    };
    base.on_click(on_click).child(label)
}

pub struct CpuTab {
    session: Entity<SessionModel>,
    snap: Arc<Snapshot>,
    mode: CpuMode,
    /// "Whole session" instead of the shared range.
    whole_session: bool,
    range: Option<TimeRange>,
    /// Whether the CPU tab is on screen; nothing is computed otherwise.
    active: bool,
    focus_pending: bool,
    /// The tree holds the keyboard focus (set by us), so it must hand it back when it goes.
    tree_focus: bool,
    /// Where the focus goes when the tree is no longer shown.
    focus: FocusHandle,
    input: Entity<InputState>,
    filter: String,
    tree_state: Entity<TreeState>,
    /// The row behind every item id, for the render closure.
    rows: Rc<RefCell<HashMap<SharedString, Row>>>,
    /// The ids of the nodes the user opened.
    open: HashSet<SharedString>,
    flame: Entity<FlameChart>,
    flame_data: Option<Arc<FlameData>>,
    /// The latest result, whose key says what it was computed for.
    result: Option<Arc<Computed>>,
    /// The key and snapshot generation the latest job was started for.
    started: Option<(Key, u64)>,
    last_start: Option<Instant>,
    trailing: Option<Task<()>>,
    skeleton: bool,
    runs: usize,
    cancel: Arc<AtomicBool>,
    job: Option<Task<()>>,
    timer: Option<Task<()>>,
    hover: Option<(usize, Point<Pixels>)>,
    selected_span: Option<usize>,
    _subs: Vec<Subscription>,
}

impl CpuTab {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snap = session.read(cx).snapshot().clone();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by function name"));
        let tree_state = cx.new(|cx| TreeState::new(cx));
        let flame = cx.new(FlameChart::new);
        let subs = vec![
            cx.observe(&session, |this, session, cx| {
                this.snap = session.read(cx).snapshot().clone();
                if this.active {
                    this.sync(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter_changed(cx);
                }
            }),
            cx.subscribe(&tree_state, |this, _, event: &TreeEvent, cx| match event {
                TreeEvent::Expanded(id) => {
                    if this.open.insert(id.clone()) {
                        this.rebuild_items(cx);
                    }
                }
                TreeEvent::Collapsed(id) => {
                    this.open.remove(id);
                }
            }),
            cx.subscribe(&flame, |this, _, event: &FlameEvent, cx| match *event {
                FlameEvent::Hover(hover) => {
                    this.hover = hover;
                    cx.notify();
                }
                FlameEvent::Select(span) => {
                    this.selected_span = span;
                    cx.notify();
                }
            }),
        ];
        Self {
            session,
            snap,
            mode: CpuMode::BottomUp,
            whole_session: false,
            range: None,
            active: false,
            focus_pending: false,
            tree_focus: false,
            focus: cx.focus_handle(),
            input,
            filter: String::new(),
            tree_state,
            rows: Rc::new(RefCell::new(HashMap::new())),
            open: HashSet::new(),
            flame,
            flame_data: None,
            result: None,
            started: None,
            last_start: None,
            trailing: None,
            skeleton: false,
            runs: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            job: None,
            timer: None,
            hover: None,
            selected_span: None,
            _subs: subs,
        }
    }

    pub fn mode(&self) -> CpuMode {
        self.mode
    }

    /// The flame chart.
    pub fn flame(&self) -> &Entity<FlameChart> {
        &self.flame
    }

    /// Whether the latest result is for what the tab shows now (its mode, range and filter).
    pub fn is_settled(&self) -> bool {
        self.active
            && self.result.as_ref().is_some_and(|r| r.key == self.key())
            && (self.mode != CpuMode::Flame || self.flame_data.is_some())
    }

    /// Opens the first `fanout` nodes of every level above `depth` in the top-down tree, so its
    /// rows reach `depth + 1` levels. What opening nodes by hand does, in one go.
    pub fn expand_to_depth(&mut self, depth: usize, fanout: usize, cx: &mut Context<Self>) {
        fn walk(
            data: &TreeData,
            nodes: &[u32],
            parent: Option<&str>,
            level: usize,
            (depth, fanout): (usize, usize),
            open: &mut HashSet<SharedString>,
        ) {
            for &n in nodes.iter().take(fanout) {
                let node = &data.tree.nodes[n as usize];
                let id = match parent {
                    Some(p) => format!("{p}/{}", node.function.0),
                    None => node.function.0.to_string(),
                };
                if level < depth && !node.children.is_empty() {
                    walk(
                        data,
                        &node.children,
                        Some(&id),
                        level + 1,
                        (depth, fanout),
                        open,
                    );
                    open.insert(id.into());
                }
            }
        }
        let Some(result) = self.result.clone() else {
            return;
        };
        let Body::Tree(data) = &result.body else {
            return;
        };
        walk(
            data,
            &data.tree.roots,
            None,
            0,
            (depth, fanout),
            &mut self.open,
        );
        self.rebuild_items(cx);
    }

    /// Scrolls the tree so row `ix` is at the top; the list applies it when next drawn.
    pub fn scroll_tree_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.tree_state.update(cx, |s, cx| {
            s.scroll_to_item(ix, gpui_kit::ScrollStrategy::Top);
            cx.notify();
        });
    }

    /// How far the tree's list is scrolled, in pixels (it applies a scroll when drawn).
    pub fn tree_scroll_px(&self, cx: &App) -> f32 {
        let handle = self.tree_state.read(cx).scroll_handle().clone();
        let offset = handle.0.borrow().base_handle.offset();
        -f32::from(offset.y)
    }

    /// How many rows the tree shows.
    pub fn tree_len(&self, cx: &App) -> usize {
        let state = self.tree_state.read(cx);
        (0..).map_while(|ix| state.entry(ix)).count()
    }

    /// How many times a recompute was started.
    #[cfg(test)]
    pub fn runs(&self) -> usize {
        self.runs
    }

    /// The cancel flag of the latest job; it is set when a newer job replaces that one.
    #[cfg(test)]
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// The range the tab computes over.
    fn effective_range(&self) -> Option<TimeRange> {
        if self.whole_session { None } else { self.range }
    }

    /// The samples in the range; a binary search, so cheap on the UI thread.
    pub fn sample_count(&self, _cx: &App) -> usize {
        match self.effective_range() {
            Some(r) => self.snap.cpu.range(r).len(),
            None => self.snap.cpu.samples().len(),
        }
    }

    pub fn set_mode(&mut self, mode: CpuMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.open.clear();
            self.hover = None;
            self.sync(cx);
            cx.notify();
        }
    }

    pub fn set_whole_session(&mut self, whole: bool, cx: &mut Context<Self>) {
        if self.whole_session != whole {
            self.whole_session = whole;
            self.sync(cx);
            cx.notify();
        }
    }

    /// The shared range, from the location.
    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.sync(cx);
            cx.notify();
        }
    }

    /// Whether the CPU tab is on screen. Becoming active computes and focuses the tree.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if active {
                self.focus_pending = true;
                self.snap = self.session.read(cx).snapshot().clone();
                self.sync(cx);
                cx.notify();
            }
        }
    }

    /// Sets the filter field, as typing would.
    #[cfg(test)]
    pub fn set_filter_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
        self.filter_changed(cx);
    }

    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        if value != self.filter {
            self.filter = value;
            self.sync(cx);
        }
    }

    pub fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_focus = true;
        self.tree_state.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Hands the focus to `to` (an element that stays on screen) if the tree has it.
    pub fn release_focus(&mut self, to: &FocusHandle, window: &mut Window, cx: &mut App) {
        if std::mem::take(&mut self.tree_focus) {
            window.focus(to, cx);
        }
    }

    /// Selects tree row `ix`.
    #[cfg(test)]
    pub fn select_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.tree_state
            .update(cx, |s, cx| s.set_selected_index(Some(ix), cx));
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

    /// The row behind item `id`.
    #[cfg(test)]
    pub fn row(&self, id: &str) -> Option<Row> {
        self.rows.borrow().get(id).cloned()
    }

    fn key(&self) -> Key {
        Key {
            session: self.snap.session,
            range: self.effective_range(),
            mode: self.mode,
            filter: if self.mode == CpuMode::Flame {
                String::new()
            } else {
                self.filter.to_lowercase()
            },
        }
    }

    /// Starts a recompute when the range, mode, filter or snapshot changed. A change of the
    /// samples alone waits out `REFRESH_INTERVAL` since the last start (the last one runs last);
    /// the others start at once and cancel the running job.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let snap = self.snap.clone();
        let key = self.key();
        let generation = snap.generation;
        if self
            .started
            .as_ref()
            .is_some_and(|(k, g)| *k == key && *g == generation)
        {
            return;
        }
        let same_key = self.started.as_ref().is_some_and(|(k, _)| *k == key);
        if same_key {
            if let Some(at) = self.last_start {
                let since = cx.background_executor().now().saturating_duration_since(at);
                if since < REFRESH_INTERVAL {
                    if self.trailing.is_none() {
                        let wait = REFRESH_INTERVAL - since;
                        self.trailing = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            this.update(cx, |this, cx| {
                                this.trailing = None;
                                this.sync(cx);
                            })
                            .ok();
                        }));
                    }
                    return;
                }
            }
        } else {
            self.trailing = None;
            self.skeleton = false;
            self.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(motion::LOADING_DELAY).await;
                this.update(cx, |this, cx| {
                    if !this.has_result() {
                        this.skeleton = true;
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
        self.runs += 1;
        self.trailing = None;
        self.last_start = Some(cx.background_executor().now());
        self.started = Some((key.clone(), generation));
        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let job_key = key.clone();
        self.job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn({
                    let key = job_key.clone();
                    async move { compute(&snap, &key, &cancel) }
                })
                .await;
            this.update(cx, |this, cx| {
                // A newer job (another range, or newer samples) replaced this one.
                if this.started.as_ref() == Some(&(job_key, generation))
                    && let Some(out) = out
                {
                    this.apply(out, cx);
                }
            })
            .ok();
        }));
    }

    /// Whether the latest result is for the mode on screen.
    /// Whether the flame chart is on screen (the next render draws it).
    pub fn flame_shown(&self, cx: &App) -> bool {
        self.mode == CpuMode::Flame && self.sample_count(cx) > 0 && self.has_result()
    }

    fn has_result(&self) -> bool {
        self.result
            .as_ref()
            .is_some_and(|r| r.key.mode == self.mode)
    }

    fn apply(&mut self, out: Computed, cx: &mut Context<Self>) {
        let refit = self
            .result
            .as_ref()
            .is_none_or(|r| r.key.range != out.key.range || r.key.session != out.key.session);
        let out = Arc::new(out);
        self.result = Some(out.clone());
        self.skeleton = false;
        self.timer = None;
        match &out.body {
            Body::Tree(_) => self.rebuild_items(cx),
            Body::Flame(data) => {
                self.flame_data = Some(data.clone());
                self.selected_span = None;
                self.hover = None;
                let snap = out.snap.clone();
                let data = data.clone();
                self.flame
                    .update(cx, |f, cx| f.set_data(snap, data, refit, cx));
            }
        }
        cx.notify();
    }

    /// Builds the tree's items from the latest result and the open nodes, keeping the selection.
    fn rebuild_items(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.result.clone() else {
            return;
        };
        let Body::Tree(data) = &result.body else {
            return;
        };
        let mut rows = HashMap::new();
        let items = build_items(data, &self.open, &mut rows);
        *self.rows.borrow_mut() = rows;
        self.tree_state.update(cx, |state, cx| {
            let selected = state.selected_item().map(|i| i.id.clone());
            state.set_items(items, cx);
            if let Some(ix) = selected.and_then(|id| state.index_of(&id)) {
                state.set_selected_index(Some(ix), cx);
            }
        });
    }
}

impl CpuTab {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let (accent, off) = (Hsla::from(t.accent), Hsla::from(t.text_faint));
        let connected = matches!(self.session.read(cx).status(), ConnStatus::Connected { .. });
        let mut bar = div()
            .id("cpu-toolbar")
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .w_full()
            .h(layout::TOOLBAR_HEIGHT)
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border));
        for (mode, id, label) in CpuMode::ALL {
            bar = bar.child(chip(
                id,
                label,
                self.mode == mode,
                accent,
                off,
                cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)),
            ));
        }
        let whole = self.whole_session;
        bar = bar
            .child(
                div()
                    .w(layout::BORDER)
                    .h(layout::ICON_SM)
                    .bg(Hsla::from(t.border)),
            )
            .child(chip(
                "cpu-scope-range",
                "Selected range",
                !whole,
                accent,
                off,
                cx.listener(|this, _, _, cx| this.set_whole_session(false, cx)),
            ))
            .child(chip(
                "cpu-scope-session",
                "Whole session",
                whole,
                accent,
                off,
                cx.listener(|this, _, _, cx| this.set_whole_session(true, cx)),
            ));
        let samples = format!("{} samples", self.sample_count(cx));
        bar = bar.child(
            div()
                .id("cpu-samples")
                .test_support()
                .aria_label(SharedString::from(samples.clone()))
                .flex_none()
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(samples),
        );
        if self.mode != CpuMode::Flame {
            bar = bar.child(
                div()
                    .id("cpu-filter")
                    .test_support()
                    .flex_none()
                    .w(layout::SEARCH_WIDTH)
                    .min_w(layout::SEARCH_MIN)
                    .child(Input::new(&self.input).cleanable(true)),
            );
        }
        bar = bar.child(div().flex_1());
        if connected {
            bar = bar.child(
                div()
                    .id("cpu-fetch")
                    .test_support()
                    .aria_label("Fetch now")
                    .flex_none()
                    .px(space::S8)
                    .py(space::S2)
                    .rounded(radius::CONTROL)
                    .bg(Hsla::from(t.elevated))
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(control) = this.session.read(cx).control() {
                            control.fetch_cpu_samples();
                        }
                    }))
                    .child("Fetch now"),
            );
        }
        bar.into_any_element()
    }

    fn note(&self, id: &'static str, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        div()
            .id(id)
            .test_support()
            .aria_label(SharedString::from(text.to_string()))
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .px(space::S16)
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text_muted))
            .child(text.to_string())
            .into_any_element()
    }

    fn skeleton(&self) -> AnyElement {
        div()
            .id("cpu-loading")
            .test_support()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S8)
            .p(space::S16)
            .children((0..6).map(|_| Skeleton::new().h(row::TABLE)))
            .into_any_element()
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let header =
            |label: &'static str, w: Pixels| div().flex_none().w(w).text_right().child(label);
        let rows = self.rows.clone();
        let list = tree(&self.tree_state, move |ix, entry, selected, _, cx| {
            let t = *cx.tokens();
            let row = rows.borrow().get(&entry.item().id).cloned();
            let content = match row {
                // The placeholder of a closed node shows nothing.
                None => div(),
                Some(r) if r.more == Some(0) => div(),
                Some(r) => {
                    let folder = entry.is_folder() && r.more.is_none();
                    let glyph = if !folder {
                        ""
                    } else if entry.is_expanded() {
                        "\u{25be}"
                    } else {
                        "\u{25b8}"
                    };
                    let name = match r.more {
                        Some(n) => format!("{n} more"),
                        None => r.name.to_string(),
                    };
                    let num = |text: String, w: Pixels| {
                        div()
                            .flex_none()
                            .w(w)
                            .text_right()
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
                                .text_color(Hsla::from(if r.more.is_some() {
                                    t.text_muted
                                } else {
                                    t.text
                                }))
                                .child(name),
                        );
                    if let Some(lib) = r.library.clone() {
                        cell = cell.child(
                            div()
                                .truncate()
                                .text_color(Hsla::from(t.text_faint))
                                .child(lib),
                        );
                    }
                    if r.user {
                        let accent = Hsla::from(t.accent);
                        cell = cell.child(
                            div()
                                .flex_none()
                                .px(space::S8)
                                .py(space::S2)
                                .rounded(radius::CHIP)
                                .bg(accent.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
                                .text_size(font::LABEL)
                                .text_color(accent)
                                .child("your code"),
                        );
                    }
                    let mut line = div()
                        .flex()
                        .items_center()
                        .gap(space::S8)
                        .w_full()
                        .text_size(font::DENSE)
                        .child(cell);
                    if r.more.is_none() {
                        line = line
                            .child(num(pct_text(r.self_pct), table::COL_PERCENT))
                            .child(num(pct_text(r.total_pct), table::COL_PERCENT))
                            .child(num(ms_text(r.self_ms), table::COL_MS))
                            .child(num(ms_text(r.total_ms), table::COL_MS));
                    }
                    line
                }
            };
            ListItem::new(ix).selected(selected).child(content)
        });
        div()
            .id("cpu-tree-body")
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
                    .child(div().flex_1().child("Function"))
                    .child(header("Self %", table::COL_PERCENT))
                    .child(header("Total %", table::COL_PERCENT))
                    .child(header("Self ms", table::COL_MS))
                    .child(header("Total ms", table::COL_MS)),
            )
            .child(
                div()
                    .id("cpu-tree")
                    .test_support()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.focus_tree(window, cx)),
                    )
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px(space::S8)
                    .child(list),
            )
            .into_any_element()
    }

    /// `function · library · duration · samples`, for the tooltip and the footer.
    fn span_text(&self, span: usize) -> Option<String> {
        let data = self.flame_data.as_ref()?;
        let s = data.spans.get(span)?;
        let info = self.result.as_ref()?.snap.cpu.function(s.function);
        let dur = s.end.0.saturating_sub(s.start.0);
        let period_ns = u64::from(data.period_us.max(1)) * 1_000;
        let samples = (dur + period_ns / 2) / period_ns;
        Some(format!(
            "{}{SEP}{}{SEP}{}{SEP}{} samples",
            info.name,
            info.library.as_deref().unwrap_or("native code"),
            format_duration_ns(dur),
            samples.max(1)
        ))
    }

    fn render_flame(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let footer = self.selected_span.and_then(|s| self.span_text(s));
        let truncated = self.flame_data.as_ref().is_some_and(|d| d.truncated);
        let recent = self.flame_data.as_ref().is_some_and(|d| d.recent);
        let mut col = div().flex_1().min_h_0().flex().flex_col().child(
            div().flex_1().min_h_0().w_full().child(
                self.flame
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            ),
        );
        if recent {
            col = col.child(
                div()
                    .id("cpu-flame-recent")
                    .test_support()
                    .aria_label(RECENT_NOTE)
                    .flex_none()
                    .px(space::S16)
                    .py(space::S4)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(RECENT_NOTE),
            );
        }
        if truncated {
            let text = "Too many calls to draw. Showing the first part; select a shorter range to see the rest.";
            col = col.child(
                div()
                    .id("cpu-flame-truncated")
                    .test_support()
                    .aria_label(text)
                    .flex_none()
                    .px(space::S16)
                    .py(space::S4)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.warning))
                    .child(text),
            );
        }
        col.child(
            div()
                .id("cpu-flame-detail")
                .test_support()
                .aria_label(SharedString::from(footer.clone().unwrap_or_default()))
                .flex_none()
                .h(row::TABLE)
                .flex()
                .items_center()
                .px(space::S16)
                .border_t(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(footer.unwrap_or_default()),
        )
        .into_any_element()
    }

    fn render_tooltip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let (span, at) = self.hover?;
        let text = self.span_text(span)?;
        let offset = layout::TOOLTIP_OFFSET;
        Some(
            deferred(
                anchored()
                    .position(at + point(offset, offset))
                    .snap_to_window_with_margin(space::S8)
                    .child(
                        div()
                            .id("cpu-flame-tooltip")
                            .test_support()
                            .aria_label(SharedString::from(text.clone()))
                            .px(space::S8)
                            .py(space::S4)
                            .rounded(radius::CONTROL)
                            .bg(Hsla::from(t.elevated))
                            .border_1()
                            .border_color(Hsla::from(t.border))
                            .text_size(font::LABEL)
                            .text_color(Hsla::from(t.text))
                            .child(text),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

impl Render for CpuTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let samples = self.sample_count(cx);
        let off = self
            .snap
            .runs
            .last()
            .and_then(|r| match self.snap.cpu.status(r.id) {
                CpuStatus::Off(reason) => Some(reason.to_string()),
                _ => None,
            });
        let body = if samples == 0 {
            match off {
                Some(reason) => self.note("cpu-off", &reason, cx),
                None => self.note("cpu-none", NONE_COPY, cx),
            }
        } else if !self.has_result() {
            if self.skeleton {
                self.skeleton()
            } else {
                div().flex_1().into_any_element()
            }
        } else if self.mode == CpuMode::Flame {
            self.render_flame(cx)
        } else {
            self.render_tree(cx)
        };
        let tree_shown = samples > 0 && self.has_result() && self.mode != CpuMode::Flame;
        if tree_shown && self.focus_pending {
            self.focus_pending = false;
            self.focus_tree(window, cx);
        } else if !tree_shown {
            let to = self.focus.clone();
            self.release_focus(&to, window, cx);
        }
        div()
            .id("cpu-tab")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(body)
            .children(self.render_tooltip(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowCpuTab;
    use crate::test_support::{
        connected, exists, frame_at, open_shell_fake, open_shell_snapshot, snapshot,
    };
    use fdt_store::cpu::{CpuSample, CpuStatus};
    use fdt_store::time::SessionTime;
    use fdt_ui::tokens::layout;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use std::sync::atomic::Ordering;

    const MS: u64 = 1_000_000;

    /// Ten samples, 1 ms apart from 1 ms: `main > Widget.build > RenderObject.paint` six times,
    /// `main > Widget.build` twice and `main > helper` twice.
    fn cpu_snapshot() -> Arc<Snapshot> {
        snapshot(|w, run| {
            // The Performance page shows its tabs once the store has a frame.
            w.push_frame(frame_at(run, 0, 1, 8, 1));
            let a = w.intern_function("main", Some("file:///app/lib/main.dart"));
            let b = w.intern_function(
                "Widget.build",
                Some("package:flutter/src/widgets/framework.dart"),
            );
            let c = w.intern_function("RenderObject.paint", None);
            let d = w.intern_function("helper", Some("file:///app/lib/util.dart"));
            let abc = w.intern_stack(&[a, b, c]);
            let ab = w.intern_stack(&[a, b]);
            let ad = w.intern_stack(&[a, d]);
            let order = [abc, abc, ab, abc, ad, abc, abc, ab, abc, ad];
            let samples = order
                .iter()
                .enumerate()
                .map(|(i, &leaf)| CpuSample {
                    time: SessionTime((i as u64 + 1) * MS),
                    leaf,
                })
                .collect();
            w.push_cpu_samples(run, samples);
            w.set_cpu_status(run, CpuStatus::On);
            w.set_cpu_period(run, 1000);
        })
    }

    fn frame(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn open(
        cx: &mut TestAppContext,
        snap: Arc<Snapshot>,
    ) -> (
        Entity<crate::shell::MainWindow>,
        Entity<CpuTab>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.dispatch_action(ShowCpuTab);
        cx.run_until_parked();
        frame(cx);
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).cpu().clone());
        (shell, tab, cx)
    }

    fn labels(tab: &Entity<CpuTab>, cx: &mut VisualTestContext) -> Vec<String> {
        tab.read_with(cx, |t, cx| {
            t.tree_rows(cx).into_iter().map(|r| r.2).collect()
        })
    }

    fn set_mode(tab: &Entity<CpuTab>, cx: &mut VisualTestContext, mode: CpuMode) {
        tab.update(cx, |t, cx| t.set_mode(mode, cx));
        cx.run_until_parked();
        frame(cx);
    }

    /// Selects row `ix` and presses `key` in the tree.
    fn press(tab: &Entity<CpuTab>, cx: &mut VisualTestContext, ix: usize, key: &str) {
        tab.update_in(cx, |t, window, cx| {
            t.focus_tree(window, cx);
            t.select_row(ix, cx);
        });
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        frame(cx);
    }

    #[gpui_kit::test]
    fn top_down_shows_roots_then_expands_with_arrows(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, cpu_snapshot());
        set_mode(&tab, cx, CpuMode::TopDown);
        assert_eq!(labels(&tab, cx), ["main"]);
        press(&tab, cx, 0, "right");
        assert_eq!(labels(&tab, cx), ["main", "Widget.build", "helper"]);
        let depths = tab.read_with(cx, |t, cx| {
            t.tree_rows(cx).into_iter().map(|r| r.1).collect::<Vec<_>>()
        });
        assert_eq!(depths, [0, 1, 1]);
        // The id is the path of function ids, so it survives a recompute.
        let ids = tab.read_with(cx, |t, cx| {
            t.tree_rows(cx).into_iter().map(|r| r.0).collect::<Vec<_>>()
        });
        assert_eq!(ids, ["0", "0/1", "0/3"]);
        // The expansion survives a recompute (the selection is kept by id).
        tab.update(cx, |t, cx| t.set_whole_session(true, cx));
        cx.run_until_parked();
        assert_eq!(labels(&tab, cx), ["main", "Widget.build", "helper"]);
        press(&tab, cx, 0, "left");
        assert_eq!(labels(&tab, cx), ["main"]);
        // Expanding a second level builds it on demand.
        press(&tab, cx, 0, "right");
        press(&tab, cx, 1, "right");
        assert_eq!(
            labels(&tab, cx),
            ["main", "Widget.build", "RenderObject.paint", "helper"]
        );
        let row = tab.read_with(cx, |t, _| t.row("0/1").expect("row"));
        assert_eq!((row.self_pct, row.total_pct), (20.0, 80.0));
        assert_eq!((row.self_ms, row.total_ms), (2.0, 8.0));
        assert!(!row.user, "the framework is not your code");
        let main = tab.read_with(cx, |t, _| t.row("0").expect("row"));
        assert!(main.user);
    }

    #[gpui_kit::test]
    fn bottom_up_roots_are_sorted_by_self(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, cpu_snapshot());
        assert_eq!(
            labels(&tab, cx),
            ["RenderObject.paint", "Widget.build", "helper"]
        );
        let paint = tab.read_with(cx, |t, _| t.row("2").expect("row"));
        assert_eq!((paint.self_pct, paint.total_pct), (60.0, 60.0));
        assert_eq!(paint.self_ms, 6.0);
        // Callers are built when a node opens, and carry no self time.
        press(&tab, cx, 0, "right");
        assert_eq!(
            labels(&tab, cx),
            [
                "RenderObject.paint",
                "Widget.build",
                "Widget.build",
                "helper"
            ]
        );
        let caller = tab.read_with(cx, |t, _| t.row("2/1").expect("row"));
        assert_eq!((caller.self_pct, caller.total_pct), (0.0, 60.0));
    }

    #[test]
    fn filter_keeps_matches_and_ancestors() {
        let snap = cpu_snapshot();
        let never = AtomicBool::new(false);
        let tree = fdt_analysis::cpu_tree::top_down(&snap.cpu, None, &never).unwrap();
        let keep = filter_keep(&tree, &snap.cpu, "PAINT", &never).unwrap();
        let kept: Vec<&str> = tree
            .nodes
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(n, _)| &*snap.cpu.function(n.function).name)
            .collect();
        assert_eq!(kept.len(), 3);
        for name in ["main", "Widget.build", "RenderObject.paint"] {
            assert!(kept.contains(&name), "{name} kept");
        }
        // A cancelled filter gives nothing.
        assert!(filter_keep(&tree, &snap.cpu, "x", &AtomicBool::new(true)).is_none());
    }

    #[gpui_kit::test]
    fn filter_applies_to_the_tree_rows(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, cpu_snapshot());
        set_mode(&tab, cx, CpuMode::TopDown);
        tab.update_in(cx, |t, window, cx| t.set_filter_text("paint", window, cx));
        cx.run_until_parked();
        frame(cx);
        assert_eq!(labels(&tab, cx), ["main"]);
        press(&tab, cx, 0, "right");
        assert_eq!(labels(&tab, cx), ["main", "Widget.build"]);
        press(&tab, cx, 1, "right");
        assert_eq!(
            labels(&tab, cx),
            ["main", "Widget.build", "RenderObject.paint"]
        );
        // A filter nothing matches leaves no rows.
        tab.update_in(cx, |t, window, cx| t.set_filter_text("zzz", window, cx));
        cx.run_until_parked();
        frame(cx);
        assert!(labels(&tab, cx).is_empty());
    }

    #[gpui_kit::test]
    fn range_change_recomputes_and_cancels(cx: &mut TestAppContext) {
        let (shell, tab, cx) = open(cx, cpu_snapshot());
        let runs = tab.read_with(cx, |t, _| t.runs());
        let flag = tab.read_with(cx, |t, _| t.cancel_flag());
        assert!(!flag.load(Ordering::Relaxed));
        // Samples 1 to 5 ms: paint 3 of 5.
        let range = TimeRange::new(SessionTime(MS), SessionTime(5 * MS));
        shell
            .read_with(cx, |s, _| s.performance_view().clone())
            .update(cx, |v, cx| v.set_location(Some(range), None, cx));
        cx.run_until_parked();
        frame(cx);
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), runs + 1);
        assert!(flag.load(Ordering::Relaxed), "the old job was cancelled");
        let paint = tab.read_with(cx, |t, _| t.row("2").expect("row"));
        assert_eq!(paint.self_pct, 60.0);
        assert_eq!(tab.read_with(cx, |t, cx| t.sample_count(cx)), 5);
        // The whole session again.
        tab.update(cx, |t, cx| t.set_whole_session(true, cx));
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, cx| t.sample_count(cx)), 10);
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), runs + 2);
    }

    #[gpui_kit::test]
    fn profiler_off_shows_the_reason(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 1, 8, 1));
            w.set_cpu_status(
                run,
                CpuStatus::Off("The profiler is turned off in this app.".into()),
            );
        });
        let (_shell, _tab, cx) = open(cx, snap);
        let label = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find("cpu-off")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some("The profiler is turned off in this app.")
        );
        assert!(!exists(cx, "cpu-none"));
    }

    #[gpui_kit::test]
    fn no_samples_shows_the_empty_state(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 1, 8, 1));
        });
        let (_shell, _tab, cx) = open(cx, snap);
        let label = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find("cpu-none")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            label.as_deref(),
            Some(
                "No CPU samples in this range. Samples are taken while your app's UI thread is busy."
            )
        );
    }

    #[gpui_kit::test]
    fn fetch_now_logs_fetch_cpu(cx: &mut TestAppContext) {
        let (shell, cx, handle) = open_shell_fake(cx);
        cx.dispatch_action(ShowCpuTab);
        cx.run_until_parked();
        // Not connected: no button (R4).
        assert!(!exists(cx, "cpu-fetch"));
        handle.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "cpu-fetch"));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("cpu-fetch", cx);
        });
        cx.run_until_parked();
        assert!(handle.commands().contains(&"fetch_cpu".to_string()));
        drop(shell);
    }

    #[gpui_kit::test]
    fn the_toolbar_switches_views(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, cpu_snapshot());
        assert!(exists(cx, "cpu-toolbar"));
        assert!(exists(cx, "cpu-tree"));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("cpu-view-flame", cx);
        });
        cx.run_until_parked();
        frame(cx);
        assert_eq!(tab.read_with(cx, |t, _| t.mode()), CpuMode::Flame);
        assert!(exists(cx, "cpu-flame"));
        assert!(!exists(cx, "cpu-tree"));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("cpu-view-top-down", cx);
        });
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.mode()), CpuMode::TopDown);
    }

    #[gpui_kit::test]
    fn new_samples_are_throttled_and_the_last_one_wins(cx: &mut TestAppContext) {
        let (_shell, cx, handle) = open_shell_fake(cx);
        cx.dispatch_action(ShowCpuTab);
        cx.run_until_parked();
        let tab = _shell.read_with(cx, |s, cx| s.performance_view().read(cx).cpu().clone());
        let (mut w, run) = crate::test_support::writer();
        w.push_frame(frame_at(run, 0, 1, 8, 1));
        let f = w.intern_function("main", Some("file:///app/lib/main.dart"));
        let leaf = w.intern_stack(&[f]);
        let mut base = 0;
        for i in 0..3u64 {
            w.push_cpu_samples(
                run,
                vec![CpuSample {
                    time: SessionTime((i + 1) * MS),
                    leaf,
                }],
            );
            handle.snaps.send(w.publish()).unwrap();
            cx.run_until_parked();
            if i == 0 {
                // A new session is another key: it starts at once.
                base = tab.read_with(cx, |t, _| t.runs());
            }
        }
        // The two that followed in the same session wait.
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), base);
        cx.executor().advance_clock(REFRESH_INTERVAL);
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), base + 1);
        frame(cx);
        assert_eq!(tab.read_with(cx, |t, cx| t.sample_count(cx)), 3);
        let main = tab.read_with(cx, |t, _| t.row("0").expect("row"));
        assert_eq!(main.total_pct, 100.0);
    }

    #[gpui_kit::test]
    fn a_node_lists_at_most_500_children(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 1, 8, 1));
            let samples = (0..600u64)
                .map(|i| {
                    let f = w.intern_function(&format!("fn{i}"), Some("file:///app/lib/a.dart"));
                    CpuSample {
                        time: SessionTime((i + 1) * MS),
                        leaf: w.intern_stack(&[f]),
                    }
                })
                .collect();
            w.push_cpu_samples(run, samples);
        });
        let (_shell, tab, cx) = open(cx, snap);
        let rows = labels(&tab, cx);
        assert_eq!(rows.len(), 501);
        assert_eq!(rows.last().map(String::as_str), Some("100 more"));
    }

    #[gpui_kit::test]
    fn the_flame_chart_selects_zooms_and_fits(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, cpu_snapshot());
        set_mode(&tab, cx, CpuMode::Flame);
        let flame = tab.read_with(cx, |t, _| t.flame.clone());
        let fit = flame.read_with(cx, |f, _| f.viewport());
        assert!(fit.span_ns > 0);
        // A click on the top row, in the middle, picks `main` and shows its details.
        let b = cx.update(|window, cx| {
            window.render_frame(cx);
            window.find("cpu-flame").bounds()
        });
        let at = gpui_kit::point(
            b.origin.x + b.size.width / 2.0,
            b.origin.y + layout::FLAME_ROW / 2.0,
        );
        cx.simulate_mouse_move(at, None, gpui_kit::Modifiers::none());
        cx.simulate_click(at, gpui_kit::Modifiers::none());
        cx.run_until_parked();
        frame(cx);
        let text = cx.update(|window, _| {
            window
                .try_find("cpu-flame-detail")
                .and_then(|e| e.label().map(str::to_string))
        });
        let text = text.expect("footer");
        assert!(
            text.starts_with("main \u{b7} file:///app/lib/main.dart"),
            "{text}"
        );
        assert!(text.ends_with("10 samples"), "{text}");
        cx.dispatch_action(crate::actions::ZoomIn);
        cx.run_until_parked();
        let zoomed = flame.read_with(cx, |f, _| f.viewport());
        assert!(zoomed.span_ns < fit.span_ns);
        cx.dispatch_action(crate::actions::ZoomToFit);
        cx.run_until_parked();
        assert_eq!(flame.read_with(cx, |f, _| f.viewport()), fit);
    }

    /// One sample a millisecond for `secs` seconds, in `main`.
    fn long_snapshot(secs: u64) -> Arc<Snapshot> {
        snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 1, 8, 1));
            let a = w.intern_function("main", Some("file:///app/lib/main.dart"));
            let b = w.intern_function("work", Some("file:///app/lib/main.dart"));
            let ab = w.intern_stack(&[a, b]);
            let samples = (0..secs * 1000)
                .map(|i| CpuSample {
                    time: SessionTime((i + 1) * MS),
                    leaf: ab,
                })
                .collect();
            w.push_cpu_samples(run, samples);
        })
    }

    fn flame_bounds(tab: &Entity<CpuTab>, cx: &mut VisualTestContext) -> (u64, u64) {
        tab.read_with(cx, |t, _| t.flame_data.as_ref().map(|d| d.bounds))
            .expect("flame data")
    }

    /// With no range, the flame chart shows the last 10 seconds (the whole session would be cut
    /// at the span cap and show only its start); with a range, the range.
    #[gpui_kit::test]
    fn the_flame_chart_shows_the_last_ten_seconds_without_a_range(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open(cx, long_snapshot(60));
        set_mode(&tab, cx, CpuMode::Flame);
        let last = 60_000 * MS;
        let (start, end) = flame_bounds(&tab, cx);
        assert_eq!(start, last - 10_000 * MS);
        assert!(end >= last, "{end}");
        let note = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find("cpu-flame-recent")
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(
            note.as_deref(),
            Some("Showing the last 10 seconds. Select a range to see another part.")
        );
        let r = TimeRange::new(SessionTime(5_000 * MS), SessionTime(7_000 * MS));
        tab.update(cx, |t, cx| t.set_range(Some(r), cx));
        cx.run_until_parked();
        frame(cx);
        assert_eq!(flame_bounds(&tab, cx), (r.start.0, r.end.0));
        assert!(!crate::test_support::exists(cx, "cpu-flame-recent"));
    }

    #[gpui_kit::test]
    fn leaving_the_tab_returns_the_keys_to_the_app(cx: &mut TestAppContext) {
        let (shell, tab, cx) = open(cx, cpu_snapshot());
        // The tree took the focus when it appeared; the tab commands still reach the shell.
        cx.dispatch_action(crate::actions::ShowRebuildsTab);
        cx.run_until_parked();
        frame(cx);
        cx.dispatch_action(crate::actions::ShowFramesTab);
        cx.run_until_parked();
        let perf = shell.read_with(cx, |s, _| s.performance_view().clone());
        assert_eq!(
            perf.read_with(cx, |v, _| v.tab()),
            crate::tools::performance::PerfTab::Frames
        );
        // The flame chart is not the tree: switching to it releases the tree's focus too.
        cx.dispatch_action(ShowCpuTab);
        cx.run_until_parked();
        frame(cx);
        set_mode(&tab, cx, CpuMode::Flame);
        cx.dispatch_action(crate::actions::ShowTraceTab);
        assert_eq!(
            perf.read_with(cx, |v, _| v.tab()),
            crate::tools::performance::PerfTab::Trace
        );
    }
}
