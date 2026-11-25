//! The Leaks tab: the heap snapshots taken, what the selected one holds that should be gone
//! (level-1 findings with readable retaining paths), the checks that could not run, and what
//! grew since another snapshot. A details pane shows one finding or growth sample's path, with
//! links to the screen changes in between (Logs) and the Timeline.
//!
//! The model functions here are pure; the heavy ones ([`build_details`]) run in a background
//! job. The tab renders its lines through a `uniform_list`.

use super::{group_digits, heap_number};
use crate::actions::{SelectNext, SelectPrevious, TakeHeapSnapshot};
use crate::keymap::LEAKS_CONTEXT;
use crate::nav::{Location, Page, Selection, Tool};
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use crate::tools::network::model::format_bytes;
use crate::tools::{OpenLocation, ReplaceLocation};
use fdt_analysis::heap::leaks::path_text;
use fdt_capture::ConnStatus;
use fdt_store::heap::{
    Check, Edge, Growth, HeapId, HeapRecord, HeapStage, HeapSummary, LeakFinding, LeakKind, PathHop,
};
use fdt_store::memory::MemorySnapshot;
use fdt_store::run::Mode;
use fdt_store::time::{EntryId, SessionTime, TimeRange};
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{
    Action as _, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    TestSupportExt as _, UniformListScrollHandle, Window, div, uniform_list,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

/// The tooltip of the snapshot button while an app is connected.
pub const PAUSE_TIP: &str = "Your app pauses while it writes the snapshot: a fraction of a second for a small app, a few seconds for a large one.";
/// Said while the snapshot is being received.
pub const RECEIVING_NOTE: &str =
    "Your app is paused until it finishes writing. Cancelling stops fdt from reading the rest.";
pub const EMPTY_HEADING: &str = "No heap snapshot yet";
/// The empty state's body, with the platform's keys for taking a snapshot.
pub fn empty_body() -> String {
    format!(
        "Take one ({}) to look for objects that are still in memory after their screen closed. To check a screen, take a snapshot, open the screen and leave it, then take another.",
        crate::keymap::platform("\u{2318}\u{21e7}S", "Ctrl+Shift+S")
    )
}
pub const ONE_SNAPSHOT_NOTE: &str =
    "Take another snapshot after visiting a screen to see what it left behind.";
pub const PROFILE_NOTE: &str = "Profile mode can't tell a disposed controller or notifier from an unused one. Compare two snapshots, or run in debug mode to check them directly.";
pub const NOTHING_FOUND: &str = "No leaked objects found.";
const DISCONNECTED_TIP: &str = super::TAKE_SNAPSHOT_TIP;
/// The most screen changes the details list.
const MAX_SCREENS: usize = 20;
/// The screen changes listed when there is no baseline.
const RECENT_SCREENS: usize = 3;
/// How far around a snapshot the Timeline link looks when there is no baseline.
const TIMELINE_PAD_NS: u64 = 2_000_000_000; // token-check: allow: time span, not a visual duration
/// The most rows of the retaining path shown without scrolling.
const PATH_ROWS_SHOWN: usize = 10;
/// The most lines of a failure's details shown.
const MAX_DETAIL_LINES: usize = 20;

// ---- the model ----

/// The text of a busy stage, or `None` for a stage that is not busy.
pub fn stage_text(stage: &HeapStage) -> Option<String> {
    Some(match stage {
        HeapStage::Requested => "Receiving snapshot".to_string(),
        HeapStage::Receiving { bytes, .. } => {
            format!("Receiving snapshot \u{b7} {}", format_bytes(*bytes))
        }
        HeapStage::Parsing { fraction } => {
            let p = (fraction.clamp(0.0, 1.0) * 100.0).round() as u32;
            format!("Reading \u{b7} {p} %")
        }
        HeapStage::Reachability => "Finding reachable objects".to_string(),
        HeapStage::Leaks => "Checking for leaks".to_string(),
        HeapStage::Dominators => "Computing retained sizes".to_string(),
        HeapStage::Ready | HeapStage::Cancelled | HeapStage::Failed { .. } => return None,
    })
}

/// Whether the app is paused while the record is in this stage.
fn app_paused(stage: &HeapStage) -> bool {
    matches!(stage, HeapStage::Requested | HeapStage::Receiving { .. })
}

fn class_name(summary: &HeapSummary, class: u32) -> &str {
    summary
        .classes
        .get(class as usize)
        .map_or("Object", |c| &c.name)
}

pub fn finding_sentence(summary: &HeapSummary, f: &LeakFinding) -> String {
    let class = class_name(summary, f.class);
    match f.kind {
        LeakKind::UnmountedState => {
            format!("{class} is still in memory after leaving the widget tree.")
        }
        LeakKind::DefunctElement => format!("{class} is still in memory after it was unmounted."),
        LeakKind::DisposedNotifier => {
            format!("{class} is still in memory after dispose() was called.")
        }
    }
}

/// One line per check that did not run. In profile mode the notifiers line is the profile-mode
/// note.
pub fn check_notes(summary: &HeapSummary) -> Vec<String> {
    let mut out = Vec::new();
    for (name, check) in [
        ("states", &summary.checks.states),
        ("elements", &summary.checks.elements),
    ] {
        if *check != Check::Done {
            out.push(format!("Couldn't check {name} in this snapshot."));
        }
    }
    if summary.mode == Mode::Profile {
        out.push(PROFILE_NOTE.to_string());
    } else if summary.checks.notifiers != Check::Done {
        out.push("Couldn't check notifiers in this snapshot.".to_string());
    }
    out
}

/// The growth at `index`, which counts the user classes, then the lifecycle classes.
pub fn growth_at(diff: &fdt_store::heap::HeapDiff, index: u32) -> Option<&Growth> {
    let i = index as usize;
    diff.user
        .get(i)
        .or_else(|| diff.lifecycle.get(i.checked_sub(diff.user.len())?))
}

fn grew_by(g: &Growth) -> u64 {
    g.after.saturating_sub(g.before)
}

pub fn growth_sentence(summary: &HeapSummary, g: &Growth, baseline: HeapId) -> String {
    format!(
        "{} more {} than in snapshot #{}.",
        group_digits(grew_by(g)),
        class_name(summary, g.class),
        heap_number(baseline)
    )
}

/// What the main list shows, top to bottom. Indices refer to the shown record.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Heading(&'static str),
    /// "Grew since snapshot #k", with the baseline picker.
    Baseline(String),
    Sub(&'static str),
    Finding(usize),
    Growth(u32),
    Note(String),
}

/// The findings, then the checks that did not run, then the growth, of `record`.
pub fn lines(record: &HeapRecord) -> Vec<Line> {
    let mut out = Vec::new();
    let Some(summary) = &record.summary else {
        return out;
    };
    out.push(Line::Heading("Found in this snapshot"));
    if summary.findings.is_empty() {
        out.push(Line::Note(NOTHING_FOUND.to_string()));
    }
    out.extend((0..summary.findings.len()).map(Line::Finding));
    if (summary.findings.len() as u64) < u64::from(summary.findings_total) {
        out.push(Line::Note(format!(
            "Showing {} of {} findings.",
            group_digits(summary.findings.len() as u64),
            group_digits(u64::from(summary.findings_total))
        )));
    }
    out.extend(check_notes(summary).into_iter().map(Line::Note));
    match &record.diff {
        None => out.push(Line::Note(ONE_SNAPSHOT_NOTE.to_string())),
        Some(diff) => {
            let k = heap_number(diff.baseline);
            out.push(Line::Baseline(format!("Grew since snapshot #{k}")));
            match &diff.comparable {
                Err(reason) => out.push(Line::Note(reason.to_string())),
                Ok(()) if diff.user.is_empty() && diff.lifecycle.is_empty() => {
                    out.push(Line::Note(format!("Nothing grew since snapshot #{k}.")));
                }
                Ok(()) => {
                    if !diff.user.is_empty() {
                        out.push(Line::Sub("Your classes"));
                        out.extend((0..diff.user.len() as u32).map(Line::Growth));
                    }
                    if !diff.lifecycle.is_empty() {
                        out.push(Line::Sub("Objects with a lifecycle"));
                        let at = diff.user.len() as u32;
                        out.extend((at..at + diff.lifecycle.len() as u32).map(Line::Growth));
                    }
                }
            }
        }
    }
    out
}

/// The record the tab shows: the selected one, else the held one, else the newest analysed.
pub fn shown_record(mem: &MemorySnapshot, selection: Option<Selection>) -> Option<&HeapRecord> {
    let chosen = match selection {
        Some(
            Selection::LeakSnapshot(h)
            | Selection::Leak { heap: h, .. }
            | Selection::Growth { heap: h, .. },
        ) => mem.heap(h),
        _ => None,
    };
    chosen
        .or_else(|| mem.held().and_then(|h| mem.heap(h)))
        .or_else(|| mem.heaps().iter().rev().find(|h| h.summary.is_some()))
}

/// "#N · 2:41 · 1,204,331 objects · 2 leak suspects".
pub fn snapshot_text(r: &HeapRecord) -> String {
    let n = heap_number(r.id);
    let time = format_offset(r.time.unwrap_or(r.requested));
    match (&r.summary, &r.stage) {
        (Some(s), HeapStage::Ready) => {
            let k = r.suspects();
            let suspects = match k {
                0 => "no leak suspects".to_string(),
                1 => "1 leak suspect".to_string(),
                k => format!("{} leak suspects", group_digits(u64::from(k))),
            };
            format!(
                "#{n} \u{b7} {time} \u{b7} {} objects \u{b7} {suspects}",
                group_digits(s.objects)
            )
        }
        (_, HeapStage::Cancelled) => format!("#{n} \u{b7} {time} \u{b7} Cancelled"),
        (_, HeapStage::Failed { .. }) => format!("#{n} \u{b7} {time} \u{b7} Failed"),
        _ => format!("#{n} \u{b7} {time}"),
    }
}

/// One hop of a retaining path, as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HopRow {
    /// The first row of a path that does not start at the root.
    pub truncated: bool,
    pub class: String,
    /// `file.dart:line` for a user class the allocation profiles know, else the library's
    /// basename.
    pub library: String,
    pub edge: String,
    pub retained: String,
    pub cause: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenChange {
    pub id: EntryId,
    pub text: String,
}

/// Everything the details pane shows for a finding or a growth sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Details {
    pub heap: HeapId,
    pub sentence: String,
    pub retains: String,
    /// "Held by AppEvents._listeners" for the likely cause.
    pub held_by: Option<String>,
    pub hops: Vec<HopRow>,
    pub keeps: Vec<(String, String)>,
    pub path_text: String,
    /// The object "Show in snapshot" opens.
    pub node: Option<u32>,
    pub screens_heading: String,
    pub screens: Vec<ScreenChange>,
    pub timeline: TimeRange,
}

pub fn edge_label(edge: &Edge) -> String {
    match edge {
        Edge::None => String::new(),
        Edge::Field(name) => name.to_string(),
        Edge::Static(name) => format!("static {name}"),
        Edge::Element(i) => format!("[{i}]"),
        Edge::ClosureContext => "closure context".to_string(),
        Edge::Captured(i) => format!("captured {i}"),
        Edge::RootSlice(name) if &**name == "isolate" => "root".to_string(),
        Edge::RootSlice(name) => format!("root ({name})"),
        Edge::Internal(pos) => format!("ref {pos}"),
    }
}

fn basename(library: &str) -> &str {
    library.rsplit('/').next().unwrap_or(library)
}

fn held_by(summary: &HeapSummary, path: &[PathHop], culprit: Option<u16>) -> Option<String> {
    let hop = path.get(usize::from(culprit?))?;
    match &hop.edge {
        Edge::Field(name) => Some(format!("Held by {}.{name}", class_name(summary, hop.class))),
        Edge::Static(name) => Some(format!("Held by the static field {name}")),
        _ => None,
    }
}

fn hop_rows(
    mem: &MemorySnapshot,
    summary: &HeapSummary,
    path: &[PathHop],
    culprit: Option<u16>,
) -> Vec<HopRow> {
    // `file.dart:line` of the classes the allocation profiles know.
    let mut places: HashMap<(&str, &str), &str> = HashMap::new();
    if path.iter().any(|h| {
        summary
            .classes
            .get(h.class as usize)
            .is_some_and(|c| c.user)
    }) {
        for i in 0..mem.alloc_class_count() {
            let c = mem.alloc_class(fdt_store::memory::AllocClassId(i as u32));
            if let (Some(lib), Some(loc)) = (&c.library, &c.location) {
                places.entry((&c.name, lib)).or_insert(loc);
            }
        }
    }
    let mut out = Vec::with_capacity(path.len() + 1);
    if path.first().is_some_and(|h| h.node != 0) {
        out.push(HopRow {
            truncated: true,
            class: "\u{2026}".to_string(),
            library: String::new(),
            edge: String::new(),
            retained: String::new(),
            cause: false,
        });
    }
    for (i, hop) in path.iter().enumerate() {
        let class = summary.classes.get(hop.class as usize);
        let library = class.map_or(String::new(), |c| {
            if c.user
                && let Some(loc) = places.get(&(&*c.name, &*c.library))
            {
                return (*loc).to_string();
            }
            basename(&c.library).to_string()
        });
        out.push(HopRow {
            truncated: false,
            class: class.map_or("Object".to_string(), |c| c.name.to_string()),
            library,
            edge: edge_label(&hop.edge),
            retained: format_bytes(hop.retained),
            cause: culprit.is_some_and(|c| usize::from(c) == i),
        });
    }
    out
}

/// The screen changes (navigation log records) the details link to: between `from` and `at`,
/// or without a baseline the last few before `at`.
fn screen_changes(
    snap: &fdt_store::Snapshot,
    from: Option<SessionTime>,
    at: SessionTime,
) -> Vec<ScreenChange> {
    let nav = snap.logs.navigation();
    let time_of = |i: usize| {
        nav.get(i)
            .and_then(|&s| snap.logs.get_row(s as usize))
            .map(|r| r.time)
    };
    // The first index whose record fails `before` (records are in time order).
    let first = |before: &dyn Fn(SessionTime) -> bool| {
        let (mut lo, mut hi) = (0, nav.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if time_of(mid).is_some_and(before) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let hi = first(&|t| t <= at);
    let lo = from.map_or(0, |f| first(&|t| t < f));
    let cap = if from.is_some() {
        MAX_SCREENS
    } else {
        RECENT_SCREENS
    };
    // The records nearest `at`.
    let lo = lo.max(hi.saturating_sub(cap));
    (lo..hi)
        .filter_map(|i| {
            let row = snap.logs.get_row(*nav.get(i)? as usize)?;
            let line = row.text.lines().next().unwrap_or("").trim();
            Some(ScreenChange {
                id: row.id,
                text: format!("{} \u{b7} {line}", format_offset(row.time)),
            })
        })
        .collect()
}

/// The details of a finding or growth selection, or `None` for anything else or a record that
/// is gone.
pub fn build_details(snap: &fdt_store::Snapshot, selection: Selection) -> Option<Details> {
    let mem = &snap.memory;
    let growth = matches!(selection, Selection::Growth { .. });
    let (heap, sentence, retained, path, culprit, keeps, node, baseline) = match selection {
        Selection::Leak { heap, finding } => {
            let record = mem.heap(heap)?;
            let summary = record.summary.as_ref()?;
            let f = summary.findings.get(finding as usize)?;
            (
                heap,
                finding_sentence(summary, f),
                f.retained,
                f.path.clone(),
                f.culprit,
                f.keeps.clone(),
                Some(f.node),
                record.diff.as_ref().map(|d| d.baseline),
            )
        }
        Selection::Growth { heap, index } => {
            let record = mem.heap(heap)?;
            let summary = record.summary.as_ref()?;
            let diff = record.diff.as_ref()?;
            let g = growth_at(diff, index)?;
            let sample = g.samples.first();
            (
                heap,
                growth_sentence(summary, g, diff.baseline),
                g.new_bytes,
                sample.map_or_else(|| Vec::new().into(), |s| s.2.clone()),
                None,
                Vec::new().into(),
                sample.map(|s| s.0),
                Some(diff.baseline),
            )
        }
        _ => return None,
    };
    let record = mem.heap(heap)?;
    let summary = record.summary.as_ref()?;
    let at = record.time.unwrap_or(record.requested);
    let from = baseline
        .and_then(|b| mem.heap(b))
        .map(|b| b.time.unwrap_or(b.requested));
    let screens_heading = match baseline {
        Some(k) if from.is_some() => format!(
            "Screen changes between snapshot #{} and #{}",
            heap_number(k),
            heap_number(heap)
        ),
        _ => "Recent screen changes".to_string(),
    };
    let timeline = match from {
        Some(b) => TimeRange::new(b.min(at), b.max(at)),
        None => TimeRange::new(
            SessionTime(at.0.saturating_sub(TIMELINE_PAD_NS)),
            SessionTime(at.0.saturating_add(TIMELINE_PAD_NS)),
        ),
    };
    Some(Details {
        heap,
        sentence,
        retains: if growth {
            format!("New objects use {}", format_bytes(retained))
        } else {
            format!("Retains {}", format_bytes(retained))
        },
        held_by: held_by(summary, &path, culprit),
        hops: hop_rows(mem, summary, &path, culprit),
        keeps: keeps
            .iter()
            .map(|&(c, b)| (class_name(summary, c).to_string(), format_bytes(b)))
            .collect(),
        path_text: path_text(summary, &path),
        node,
        screens_heading,
        screens: screen_changes(snap, from, at),
        timeline,
    })
}

// ---- the tab ----

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailsKey {
    session: u64,
    selection: Selection,
    heaps: usize,
    summary: usize,
    diff: usize,
    classes: usize,
    navigation: usize,
}

pub struct LeaksTab {
    session: Entity<SessionModel>,
    range: Option<TimeRange>,
    selection: Option<Selection>,
    active: bool,
    lines: Arc<Vec<Line>>,
    details: Option<(DetailsKey, Arc<Details>)>,
    details_started: Option<DetailsKey>,
    details_job: Option<Task<()>>,
    /// The selected growth and the diff it was picked from. A new diff (the baseline changed
    /// by any route) makes its index mean another class, so the selection is dropped.
    growth_diff: Option<(Selection, usize)>,
    /// Failed records whose details are open.
    failed_open: HashSet<HeapId>,
    focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    hop_scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenLocation> for LeaksTab {}
impl EventEmitter<ReplaceLocation> for LeaksTab {}

impl LeaksTab {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.observe(&session, |this, _, cx| {
            this.sync_details(cx);
            cx.notify();
        })];
        Self {
            session,
            range: None,
            selection: None,
            active: false,
            lines: Arc::new(Vec::new()),
            details: None,
            details_started: None,
            details_job: None,
            growth_diff: None,
            failed_open: HashSet::new(),
            focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            hop_scroll: UniformListScrollHandle::new(),
            _subs: subs,
        }
    }

    fn connected(&self, cx: &App) -> bool {
        matches!(self.session.read(cx).status(), ConnStatus::Connected { .. })
    }

    /// A new session: nothing of the old one stays.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.range = None;
        self.selection = None;
        self.details = None;
        self.details_started = None;
        self.details_job = None;
        self.failed_open.clear();
        self.lines = Arc::new(Vec::new());
        cx.notify();
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            self.sync_details(cx);
            cx.notify();
        }
    }

    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            cx.notify();
        }
    }

    /// The location's selection. Only a heap snapshot, a finding or a growth is the tab's;
    /// anything else is ignored (and not cleared: other tabs own it).
    pub fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        let mine = selection.filter(|s| {
            matches!(
                s,
                Selection::LeakSnapshot(_) | Selection::Leak { .. } | Selection::Growth { .. }
            )
        });
        if selection.is_some() && mine.is_none() {
            return;
        }
        if mine != self.selection {
            self.selection = mine;
            self.sync_details(cx);
            cx.notify();
        }
    }

    fn emit_location(
        &mut self,
        selection: Option<Selection>,
        replace: bool,
        cx: &mut Context<Self>,
    ) {
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

    fn select(&mut self, selection: Selection, replace: bool, cx: &mut Context<Self>) {
        if self.selection == Some(selection) {
            return;
        }
        self.selection = Some(selection);
        self.sync_details(cx);
        self.emit_location(Some(selection), replace, cx);
        cx.notify();
    }

    /// Starts the job for the details of the selection, unless it is the one running or done.
    fn sync_details(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            self.details = None;
            self.details_started = None;
            self.details_job = None;
            return;
        };
        let snap = self.session.read(cx).snapshot().clone();
        let heap = selection.heap().and_then(|h| snap.memory.heap(h));
        let ptr = |a: Option<usize>| a.unwrap_or(0);
        if let Selection::Growth { heap: id, .. } = selection {
            let diff = ptr(heap
                .and_then(|h| h.diff.as_ref())
                .map(|d| Arc::as_ptr(d) as usize));
            match self.growth_diff {
                Some((s, d)) if s == selection && d != diff => {
                    self.drop_growth(id, cx);
                    return;
                }
                Some((s, _)) if s == selection => {}
                _ => self.growth_diff = Some((selection, diff)),
            }
        } else {
            self.growth_diff = None;
        }
        let key = DetailsKey {
            session: snap.session,
            selection,
            heaps: snap.memory.heaps().len(),
            summary: ptr(heap
                .and_then(|h| h.summary.as_ref())
                .map(|s| Arc::as_ptr(s) as usize)),
            diff: ptr(heap
                .and_then(|h| h.diff.as_ref())
                .map(|d| Arc::as_ptr(d) as usize)),
            classes: snap.memory.alloc_class_count(),
            navigation: snap.logs.navigation().len(),
        };
        if self.details_started.as_ref() == Some(&key) {
            return;
        }
        self.details_started = Some(key.clone());
        self.details_job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn(async move { build_details(&snap, selection) })
                .await;
            this.update(cx, |this, cx| {
                if this.details_started.as_ref() == Some(&key) {
                    this.details = out.map(|d| (key, Arc::new(d)));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Picks the snapshot the growth is counted from (`None`: the previous one). A selected
    /// growth is cleared, since its numbers change.
    pub fn set_baseline(&mut self, id: Option<HeapId>, cx: &mut Context<Self>) {
        if let Some(control) = self.session.read(cx).control() {
            control.set_heap_baseline(id);
        }
        if let Some(Selection::Growth { heap, .. }) = self.selection {
            self.drop_growth(heap, cx);
        }
        cx.notify();
    }

    /// Replaces a growth selection with its snapshot's.
    fn drop_growth(&mut self, heap: HeapId, cx: &mut Context<Self>) {
        self.selection = Some(Selection::LeakSnapshot(heap));
        self.growth_diff = None;
        self.details = None;
        self.details_started = None;
        self.details_job = None;
        self.emit_location(Some(Selection::LeakSnapshot(heap)), true, cx);
        self.sync_details(cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(control) = self.session.read(cx).control() {
            control.cancel_heap_snapshot();
        }
    }

    fn copy_path(&mut self, cx: &mut Context<Self>) {
        if let Some((_, d)) = &self.details {
            cx.write_to_clipboard(ClipboardItem::new_string(d.path_text.clone()));
        }
    }

    /// Opens the object in the Snapshot tab, opening the snapshot first if it is not the held
    /// one.
    fn show_in_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some((_, d)) = &self.details else { return };
        let (heap, Some(node)) = (d.heap, d.node) else {
            return;
        };
        let held = self.session.read(cx).snapshot().memory.held();
        if held != Some(heap)
            && let Some(control) = self.session.read(cx).control()
        {
            control.open_heap_snapshot(heap);
        }
        self.emit_location(Some(Selection::HeapNode { heap, node }), false, cx);
    }

    fn open_log(&mut self, id: EntryId, cx: &mut Context<Self>) {
        cx.emit(OpenLocation(Location {
            page: Page::Tool(Tool::Logs),
            selection: Some(Selection::Log(id)),
            range: None,
        }));
    }

    fn show_on_timeline(&mut self, cx: &mut Context<Self>) {
        let Some((_, d)) = &self.details else { return };
        cx.emit(OpenLocation(Location {
            page: Page::Tool(Tool::Timeline),
            selection: None,
            range: Some(d.timeline),
        }));
    }

    /// Up and Down move through the findings and growths.
    pub fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let snap = self.session.read(cx).snapshot().clone();
        let Some(record) = shown_record(&snap.memory, self.selection) else {
            return;
        };
        let heap = record.id;
        let selectable: Vec<Selection> = lines(record)
            .iter()
            .filter_map(|l| match *l {
                Line::Finding(i) => Some(Selection::Leak {
                    heap,
                    finding: i as u32,
                }),
                Line::Growth(index) => Some(Selection::Growth { heap, index }),
                _ => None,
            })
            .collect();
        let at = self
            .selection
            .and_then(|s| selectable.iter().position(|x| *x == s));
        let next = match (at, down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(selectable.len().saturating_sub(1)),
            (Some(i), false) => i.saturating_sub(1),
        };
        if let Some(&s) = selectable.get(next) {
            self.select(s, true, cx);
        }
    }

    #[cfg(test)]
    pub fn details(&self) -> Option<Arc<Details>> {
        self.details.as_ref().map(|(_, d)| d.clone())
    }

    #[cfg(test)]
    pub fn is_settled(&self, cx: &App) -> bool {
        match self.selection {
            None => true,
            Some(s) => {
                let snap = self.session.read(cx).snapshot();
                !matches!(s, Selection::Leak { .. } | Selection::Growth { .. })
                    || build_details(snap, s).is_none()
                    || self.details.is_some()
            }
        }
    }
}

impl LeaksTab {
    fn text_row(
        &self,
        id: impl Into<SharedString>,
        text: String,
        colour: Hsla,
    ) -> impl gpui_kit::StatefulInteractiveElement
    + gpui_kit::ParentElement
    + gpui_kit::Styled
    + IntoElement {
        div()
            .id(id.into())
            .test_support()
            .aria_label(SharedString::from(text.clone()))
            .flex()
            .items_center()
            .w_full()
            .h(row::TABLE)
            .px(space::S16)
            .text_size(font::BODY)
            .text_color(colour)
            .child(div().flex_1().min_w_0().truncate().child(text))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let connected = self.connected(cx);
        let mut take = Button::new("heap-take")
            .label("Take heap snapshot")
            .compact()
            .tooltip(if connected {
                PAUSE_TIP
            } else {
                DISCONNECTED_TIP
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(TakeHeapSnapshot.boxed_clone(), cx);
            });
        if !connected {
            take = take.disabled(true);
        }
        div()
            .id("leaks-toolbar")
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .w_full()
            .h(layout::TOOLBAR_HEIGHT)
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(take)
            .into_any_element()
    }

    fn render_snapshots(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let snap = self.session.read(cx).snapshot().clone();
        let heaps = snap.memory.heaps();
        if heaps.is_empty() {
            return None;
        }
        let shown = shown_record(&snap.memory, self.selection).map(|r| r.id);
        let mut list = div()
            .id("leaks-snapshots")
            .test_support()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .max_h(row::TABLE * 8.0)
            .overflow_y_scroll()
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border));
        for record in heaps.iter().rev() {
            let n = heap_number(record.id);
            let id = record.id;
            let on = shown == Some(id);
            let stage = stage_text(&record.stage);
            let text = match &stage {
                Some(stage) => format!("#{n} \u{b7} {stage}"),
                None => snapshot_text(record),
            };
            let mut line = self
                .text_row(format!("leaks-snapshot-{n}"), text, Hsla::from(t.text))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.select(Selection::LeakSnapshot(id), false, cx);
                }));
            if on {
                line = line.bg(Hsla::from(t.accent).opacity(f32::from(alpha::ACTIVE_ROW) / 255.0));
            }
            let mut block = div().flex().flex_col().w_full();
            if stage.is_some() {
                block = block.child(
                    line.child(
                        Button::new("heap-cancel")
                            .label("Cancel")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
                );
                if app_paused(&record.stage) {
                    block = block.child(
                        div()
                            .id("leaks-pause-note")
                            .test_support()
                            .aria_label(RECEIVING_NOTE)
                            .px(space::S16)
                            .pb(space::S4)
                            .text_size(font::LABEL)
                            .text_color(Hsla::from(t.text_muted))
                            .child(RECEIVING_NOTE),
                    );
                }
            } else if let HeapStage::Failed { message, details } = &record.stage {
                let open = self.failed_open.contains(&id);
                block = block.child(
                    line.child(
                        Button::new(SharedString::from(format!("leaks-failed-toggle-{n}")))
                            .label(if open { "Hide details" } else { "Details" })
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.failed_open.remove(&id) {
                                    this.failed_open.insert(id);
                                }
                                cx.notify();
                            })),
                    ),
                );
                block = block.child(
                    div()
                        .id(SharedString::from(format!("leaks-failed-{n}")))
                        .test_support()
                        .aria_label(SharedString::from(message.to_string()))
                        .px(space::S16)
                        .text_size(font::BODY)
                        .text_color(Hsla::from(t.danger))
                        .child(message.to_string()),
                );
                if open {
                    block = block.child(
                        div()
                            .id(SharedString::from(format!("leaks-failed-details-{n}")))
                            .test_support()
                            .px(space::S16)
                            .pb(space::S4)
                            .text_size(font::LABEL)
                            .font_family(fdt_ui::tokens::font::MONO)
                            .text_color(Hsla::from(t.text_muted))
                            .children(
                                details
                                    .lines()
                                    .take(MAX_DETAIL_LINES)
                                    .map(|l| div().child(l.to_string())),
                            ),
                    );
                }
            } else {
                block = block.child(line);
            }
            list = list.child(block);
        }
        Some(list.into_any_element())
    }

    fn render_line(&self, line: &Line, record: &HeapRecord, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let heap = record.id;
        let summary = record.summary.as_ref();
        match line {
            Line::Heading(text) => div()
                .id(SharedString::from(format!("leaks-heading-{text}")))
                .test_support()
                .aria_label(*text)
                .flex()
                .items_center()
                .h(row::TABLE)
                .px(space::S16)
                .text_size(font::SECTION)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text))
                .child(*text)
                .into_any_element(),
            Line::Sub(text) => div()
                .id(SharedString::from(format!("leaks-sub-{text}")))
                .test_support()
                .aria_label(*text)
                .flex()
                .items_center()
                .h(row::TABLE)
                .px(space::S16)
                .text_size(font::BODY)
                .font_weight(font::MEDIUM)
                .text_color(Hsla::from(t.text_muted))
                .child(*text)
                .into_any_element(),
            Line::Note(text) => self
                .text_row(
                    format!("leaks-note-{}", text.len()),
                    text.clone(),
                    Hsla::from(t.text_muted),
                )
                .into_any_element(),
            Line::Baseline(text) => {
                let view = cx.weak_entity();
                let current = record.diff.as_ref().map(|d| d.baseline);
                let picker = Button::new("leaks-baseline")
                    .label(match current {
                        Some(k) => format!("Baseline #{}", heap_number(k)),
                        None => "Baseline".to_string(),
                    })
                    .ghost()
                    .compact()
                    .dropdown_menu(move |mut menu, _window, cx| {
                        let snap = view
                            .read_with(cx, |v, cx| v.session.read(cx).snapshot().clone())
                            .ok();
                        let heaps: Vec<HeapId> = snap
                            .iter()
                            .flat_map(|s| s.memory.heaps().iter())
                            .filter(|h| h.id != heap && h.summary.is_some())
                            .map(|h| h.id)
                            .collect();
                        let previous = view.clone();
                        menu = menu.item(PopupMenuItem::new("Previous snapshot").on_click(
                            move |_, _, cx| {
                                previous.update(cx, |v, cx| v.set_baseline(None, cx)).ok();
                            },
                        ));
                        for id in heaps {
                            let view = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(format!("Snapshot #{}", heap_number(id)))
                                    .checked(current == Some(id))
                                    .on_click(move |_, _, cx| {
                                        view.update(cx, |v, cx| v.set_baseline(Some(id), cx)).ok();
                                    }),
                            );
                        }
                        menu
                    });
                div()
                    .id("leaks-grew")
                    .test_support()
                    .aria_label(SharedString::from(text.clone()))
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .h(row::TABLE)
                    .px(space::S16)
                    .text_size(font::SECTION)
                    .font_weight(font::SEMIBOLD)
                    .text_color(Hsla::from(t.text))
                    .child(text.clone())
                    .child(picker)
                    .into_any_element()
            }
            Line::Finding(i) => {
                let Some((summary, f)) = summary.and_then(|s| Some((s, s.findings.get(*i)?)))
                else {
                    return div().into_any_element();
                };
                let sel = Selection::Leak {
                    heap,
                    finding: *i as u32,
                };
                self.selectable_row(
                    format!("leaks-finding-{i}"),
                    finding_sentence(summary, f),
                    Some(format_bytes(f.retained)),
                    sel,
                    cx,
                )
            }
            Line::Growth(index) => {
                let Some((summary, diff)) = summary.zip(record.diff.as_ref()) else {
                    return div().into_any_element();
                };
                let Some(g) = growth_at(diff, *index) else {
                    return div().into_any_element();
                };
                self.selectable_row(
                    format!("leaks-growth-{index}"),
                    growth_sentence(summary, g, diff.baseline),
                    Some(format_bytes(g.new_bytes)),
                    Selection::Growth {
                        heap,
                        index: *index,
                    },
                    cx,
                )
            }
        }
    }

    fn selectable_row(
        &self,
        id: String,
        text: String,
        size: Option<String>,
        sel: Selection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *cx.tokens();
        let on = self.selection == Some(sel);
        let mut r = div()
            .id(SharedString::from(id))
            .test_support()
            .aria_label(SharedString::from(text.clone()))
            .flex()
            .items_center()
            .gap(space::S8)
            .h(row::TABLE)
            .px(space::S16)
            .cursor_pointer()
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text))
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus, cx);
                this.select(sel, false, cx);
            }))
            .child(div().flex_1().min_w_0().truncate().child(text))
            .children(size.map(|s| {
                div()
                    .flex_none()
                    .font_features(font::tabular_figures())
                    .text_color(Hsla::from(t.text_muted))
                    .child(s)
            }));
        if on {
            r = r.bg(Hsla::from(t.accent).opacity(f32::from(alpha::ACTIVE_ROW) / 255.0));
        }
        r.into_any_element()
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let snap = self.session.read(cx).snapshot().clone();
        let record = shown_record(&snap.memory, self.selection);
        self.lines = Arc::new(record.map(lines).unwrap_or_default());
        let mut col = div()
            .id("leaks-main")
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx));
        if snap.memory.heaps().is_empty() {
            return col
                .child(div().flex_1().flex().items_center().justify_center().child(
                    EmptyState::new(
                        "memory-leaks-empty",
                        crate::shell::sidebar::tool_icon(Tool::Memory),
                        EMPTY_HEADING,
                        empty_body(),
                    ),
                ))
                .into_any_element();
        }
        col = col.children(self.render_snapshots(cx));
        let count = self.lines.len();
        if count > 0 {
            col = col.child(
                div().flex_1().min_h_0().w_full().child(
                    uniform_list(
                        "leaks-lines",
                        count,
                        cx.processor(|this, range: Range<usize>, _w, cx| {
                            let snap = this.session.read(cx).snapshot().clone();
                            let Some(record) = shown_record(&snap.memory, this.selection) else {
                                return Vec::new();
                            };
                            let lines = this.lines.clone();
                            let end = range.end.min(lines.len());
                            (range.start.min(end)..end)
                                .map(|i| this.render_line(&lines[i], record, cx))
                                .collect()
                        }),
                    )
                    .track_scroll(&self.list_scroll)
                    .size_full(),
                ),
            );
        } else {
            col = col.child(
                div()
                    .flex_1()
                    .id("leaks-no-lines")
                    .border_color(Hsla::from(t.border)),
            );
        }
        col.into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *cx.tokens();
        let (_, d) = self.details.as_ref()?;
        let d = d.clone();
        let label = |text: &str| {
            div()
                .px(space::S12)
                .pt(space::S8)
                .pb(space::S4)
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(text.to_string())
        };
        let mut pane = div()
            .id("leaks-details")
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
            .text_color(Hsla::from(t.text))
            .child(
                div()
                    .id("leaks-details-title")
                    .test_support()
                    .aria_label(SharedString::from(d.sentence.clone()))
                    .p(space::S12)
                    .font_weight(font::SEMIBOLD)
                    .child(d.sentence.clone()),
            )
            .child(
                div()
                    .id("leaks-details-retains")
                    .test_support()
                    .aria_label(SharedString::from(d.retains.clone()))
                    .px(space::S12)
                    .text_color(Hsla::from(t.text_muted))
                    .child(d.retains.clone()),
            );
        if let Some(held) = &d.held_by {
            pane = pane.child(
                div()
                    .id("leaks-held-by")
                    .test_support()
                    .aria_label(SharedString::from(held.clone()))
                    .px(space::S12)
                    .pt(space::S4)
                    .child(held.clone()),
            );
        }
        pane = pane.child(
            div()
                .flex()
                .gap(space::S8)
                .p(space::S12)
                .child(
                    Button::new("leaks-copy-path")
                        .label("Copy path")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.copy_path(cx))),
                )
                .children(d.node.map(|_| {
                    Button::new("leaks-show-in-snapshot")
                        .label("Show in snapshot")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.show_in_snapshot(cx)))
                })),
        );
        if !d.hops.is_empty() {
            let shown = d.hops.len().min(PATH_ROWS_SHOWN);
            pane = pane.child(label("Retaining path")).child(
                div()
                    .id("leaks-path")
                    .test_support()
                    .flex_none()
                    .w_full()
                    .h(row::LOG * shown as f32)
                    .child(
                        uniform_list(
                            "leaks-path-rows",
                            d.hops.len(),
                            cx.processor(|this, range: Range<usize>, _w, cx| {
                                let Some((_, d)) = this.details.clone() else {
                                    return Vec::new();
                                };
                                let end = range.end.min(d.hops.len());
                                (range.start.min(end)..end)
                                    .map(|i| this.render_hop(&d.hops[i], i, cx))
                                    .collect()
                            }),
                        )
                        .track_scroll(&self.hop_scroll)
                        .size_full(),
                    ),
            );
        }
        if !d.keeps.is_empty() {
            pane = pane
                .child(label("Keeps alive"))
                .children(d.keeps.iter().enumerate().map(|(i, (class, bytes))| {
                    div()
                        .id(SharedString::from(format!("leaks-keeps-{i}")))
                        .test_support()
                        .aria_label(SharedString::from(format!("{class} {bytes}")))
                        .flex()
                        .h(row::LOG)
                        .items_center()
                        .gap(space::S8)
                        .px(space::S12)
                        .text_size(font::DENSE)
                        .child(div().flex_1().min_w_0().truncate().child(class.clone()))
                        .child(
                            div()
                                .flex_none()
                                .font_features(font::tabular_figures())
                                .text_color(Hsla::from(t.text_muted))
                                .child(bytes.clone()),
                        )
                }));
        }
        pane = pane.child(label(&d.screens_heading).id("leaks-screens-heading"));
        if d.screens.is_empty() {
            pane = pane.child(
                div()
                    .px(space::S12)
                    .text_size(font::DENSE)
                    .text_color(Hsla::from(t.text_muted))
                    .child("No screen changes recorded."),
            );
        }
        for (i, s) in d.screens.iter().enumerate() {
            let id = s.id;
            pane = pane.child(
                div()
                    .id(SharedString::from(format!("leaks-screen-{i}")))
                    .test_support()
                    .aria_label(SharedString::from(s.text.clone()))
                    .h(row::LOG)
                    .flex()
                    .items_center()
                    .px(space::S12)
                    .cursor_pointer()
                    .text_size(font::DENSE)
                    .text_color(Hsla::from(t.accent))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_log(id, cx)))
                    .child(div().truncate().child(s.text.clone())),
            );
        }
        Some(
            pane.child(
                div().p(space::S12).child(
                    Button::new("leaks-show-timeline")
                        .label("Show on Timeline")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.show_on_timeline(cx))),
                ),
            )
            .into_any_element(),
        )
    }

    fn render_hop(&self, hop: &HopRow, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let mut r = div()
            .id(SharedString::from(format!("leaks-hop-{i}")))
            .test_support()
            .aria_label(SharedString::from(format!(
                "{} {} {} {}",
                hop.class, hop.library, hop.edge, hop.retained
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
                    .font_weight(if hop.cause {
                        font::SEMIBOLD
                    } else {
                        font::REGULAR
                    })
                    .child(hop.class.clone()),
            );
        if hop.cause {
            r = r.child(
                div()
                    .id("leaks-cause")
                    .test_support()
                    .aria_label("Likely cause")
                    .flex_none()
                    .px(space::S4)
                    .rounded(radius::CHIP)
                    .bg(Hsla::from(t.danger).opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.danger))
                    .child("Likely cause"),
            );
        }
        r.child(
            div()
                .flex_none()
                .max_w(layout::DETAILS_MIN / 3.0)
                .truncate()
                .text_color(Hsla::from(t.text_muted))
                .child(hop.library.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_color(Hsla::from(t.text_muted))
                .child(hop.edge.clone()),
        )
        .child(
            div()
                .flex_none()
                .font_features(font::tabular_figures())
                .child(hop.retained.clone()),
        )
        .into_any_element()
    }
}

impl Render for LeaksTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = self.render_main(cx);
        div()
            .id("leaks-tab")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .key_context(LEAKS_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.step(false, cx)))
            .child(main)
            .children(self.render_details(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{connected, exists, heap_record, heap_record_with, open_shell_fake};
    use crate::test_support::{log_at, press, snapshot};
    use fdt_store::Snapshot;
    use fdt_store::heap::{HeapDiff, LeakChecks};
    use fdt_store::logs::{LogLevel, LogSource};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    const SEC: u64 = 1_000_000_000;

    fn leaky(b: &mut fdt_analysis::heap::encode::SnapshotBuilder) {
        let events = b.class("AppEvents", "package:app/events.dart", &[]);
        let state = b.class("LeakyState", "package:app/leaky.dart", &[]);
        let e = b.object(events, 32, fdt_analysis::heap::format::Value::None, &[]);
        let s = b.object(state, 64, fdt_analysis::heap::format::Value::None, &[]);
        b.static_field("events", e);
        b.static_field("leaky", s);
    }

    fn class_ix(s: &HeapSummary, name: &str) -> u32 {
        s.classes
            .iter()
            .position(|c| &*c.name == name)
            .expect("the class") as u32
    }

    /// One finding of `LeakyState`, held by `AppEvents._listeners`, and debug-mode checks.
    fn with_findings(s: &mut HeapSummary) {
        let (events, state) = (class_ix(s, "AppEvents"), class_ix(s, "LeakyState"));
        let mut classes = s.classes.to_vec();
        classes[events as usize].user = true;
        classes[state as usize].user = true;
        s.classes = classes.into();
        s.mode = Mode::Debug;
        s.checks = LeakChecks {
            states: Check::Done,
            elements: Check::NotFound,
            notifiers: Check::Done,
        };
        let hop = |node, class, retained, edge| PathHop {
            node,
            class,
            retained,
            edge,
        };
        s.findings = vec![LeakFinding {
            kind: LeakKind::UnmountedState,
            node: 4,
            class: state,
            retained: 1_258_291,
            path: vec![
                hop(0, events, 4_000_000, Edge::Static("instance".into())),
                hop(3, events, 3_000_000, Edge::Field("_listeners".into())),
                hop(4, state, 1_258_291, Edge::None),
            ]
            .into(),
            culprit: Some(1),
            keeps: vec![(events, 2_048)].into(),
        }]
        .into();
        s.findings_total = 1;
    }

    fn grown(w: &mut fdt_store::SessionWriter, id: HeapId, baseline: HeapId, ok: bool) {
        let class = {
            let s = w.heap_record(id).unwrap().summary.as_ref().unwrap();
            class_ix(s, "LeakyState")
        };
        w.set_heap_diff(
            id,
            Some(Arc::new(HeapDiff {
                baseline,
                comparable: if ok {
                    Ok(())
                } else {
                    Err(
                        "The app restarted between these snapshots, so they can't be compared."
                            .into(),
                    )
                },
                user: if ok {
                    vec![Growth {
                        class,
                        before: 1,
                        after: 1_204,
                        new_objects: 1_203,
                        new_bytes: 4_096,
                        samples: vec![(
                            4,
                            2_048,
                            vec![PathHop {
                                node: 4,
                                class,
                                retained: 2_048,
                                edge: Edge::None,
                            }]
                            .into(),
                        )]
                        .into(),
                    }]
                    .into()
                } else {
                    Vec::new().into()
                },
                lifecycle: Vec::new().into(),
            })),
        );
    }

    fn two_snapshots(ok: bool) -> Arc<Snapshot> {
        snapshot(|w, run| {
            let a = heap_record(w, run, |_| {});
            let b = heap_record_with(w, run, leaky, with_findings);
            grown(w, b, a, ok);
        })
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
        (shell, cx, fake)
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

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id.to_string(), cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    fn leaks(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
    ) -> Entity<LeaksTab> {
        shell.read_with(cx, |s, cx| s.memory_view().read(cx).leaks().clone())
    }

    fn leak0(heap: u32) -> Selection {
        Selection::Leak {
            heap: HeapId(heap),
            finding: 0,
        }
    }

    #[gpui_kit::test]
    fn empty_state_explains_go_there_and_back(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = connect(cx, snapshot(|_, _| {}));
        fake.status.send(connected("demo", false)).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "memory-leaks-empty"));
        assert!(!exists(cx, "leaks-snapshots"));
        assert_eq!(EMPTY_HEADING, "No heap snapshot yet");
        let keys = if cfg!(target_os = "macos") {
            "\u{2318}\u{21e7}S"
        } else {
            "Ctrl+Shift+S"
        };
        assert_eq!(
            empty_body(),
            format!(
                "Take one ({keys}) to look for objects that are still in memory after their screen closed. To check a screen, take a snapshot, open the screen and leave it, then take another."
            )
        );
        // One snapshot and no diff.
        let snap = snapshot(|w, run| {
            heap_record_with(w, run, leaky, with_findings);
        });
        let record = &snap.memory.heaps()[0];
        assert_eq!(
            lines(record).last(),
            Some(&Line::Note(ONE_SNAPSHOT_NOTE.to_string()))
        );
        assert_eq!(
            ONE_SNAPSHOT_NOTE,
            "Take another snapshot after visiting a screen to see what it left behind."
        );
        assert_eq!(
            PAUSE_TIP,
            "Your app pauses while it writes the snapshot: a fraction of a second for a small app, a few seconds for a large one."
        );
        assert!(exists(cx, "heap-take"));
    }

    #[test]
    fn stages_read_as_the_brief_says() {
        let t = |s: HeapStage| stage_text(&s);
        assert_eq!(
            t(HeapStage::Requested).as_deref(),
            Some("Receiving snapshot")
        );
        assert_eq!(
            t(HeapStage::Receiving {
                bytes: 3 * 1024 * 1024,
                chunks: 3
            }),
            Some(format!(
                "Receiving snapshot \u{b7} {}",
                format_bytes(3 * 1024 * 1024)
            ))
        );
        assert_eq!(
            t(HeapStage::Parsing { fraction: 0.426 }).as_deref(),
            Some("Reading \u{b7} 43 %")
        );
        assert_eq!(
            t(HeapStage::Reachability).as_deref(),
            Some("Finding reachable objects")
        );
        assert_eq!(t(HeapStage::Leaks).as_deref(), Some("Checking for leaks"));
        assert_eq!(
            t(HeapStage::Dominators).as_deref(),
            Some("Computing retained sizes")
        );
        assert_eq!(t(HeapStage::Ready), None);
        assert_eq!(t(HeapStage::Cancelled), None);
    }

    #[gpui_kit::test]
    fn busy_record_shows_stage_and_cancel(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = w.heap_requested(run, "isolates/1", SessionTime(5 * SEC));
            w.set_heap_stage(
                id,
                HeapStage::Receiving {
                    bytes: 2 * 1024 * 1024,
                    chunks: 2,
                },
            );
        });
        let (_shell, cx, fake) = connect(cx, snap);
        assert_eq!(
            label(cx, "leaks-snapshot-1"),
            Some(format!(
                "#1 \u{b7} Receiving snapshot \u{b7} {}",
                format_bytes(2 * 1024 * 1024)
            ))
        );
        assert_eq!(
            label(cx, "leaks-pause-note").as_deref(),
            Some(RECEIVING_NOTE)
        );
        assert_eq!(
            RECEIVING_NOTE,
            "Your app is paused until it finishes writing. Cancelling stops fdt from reading the rest."
        );
        click(cx, "heap-cancel");
        assert!(fake.commands().contains(&"heap_cancel".to_string()));
    }

    #[gpui_kit::test]
    fn failed_record_shows_its_message(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = w.heap_requested(run, "isolates/1", SessionTime(5 * SEC));
            w.set_heap_stage(
                id,
                HeapStage::Failed {
                    message: "The snapshot could not be read.".into(),
                    details: "bad magic\nat byte 0".into(),
                },
            );
        });
        let (_shell, cx, _fake) = connect(cx, snap);
        assert_eq!(
            label(cx, "leaks-failed-1").as_deref(),
            Some("The snapshot could not be read.")
        );
        assert!(!exists(cx, "leaks-failed-details-1"));
        click(cx, "leaks-failed-toggle-1");
        assert!(exists(cx, "leaks-failed-details-1"));
    }

    #[test]
    fn findings_then_checks_then_growth() {
        let snap = two_snapshots(true);
        let record = &snap.memory.heaps()[1];
        assert_eq!(
            lines(record),
            vec![
                Line::Heading("Found in this snapshot"),
                Line::Finding(0),
                Line::Note("Couldn't check elements in this snapshot.".to_string()),
                Line::Baseline("Grew since snapshot #1".to_string()),
                Line::Sub("Your classes"),
                Line::Growth(0),
            ]
        );
        let s = record.summary.as_ref().unwrap();
        assert_eq!(
            finding_sentence(s, &s.findings[0]),
            "LeakyState is still in memory after leaving the widget tree."
        );
        let g = growth_at(record.diff.as_ref().unwrap(), 0).unwrap();
        assert_eq!(
            growth_sentence(s, g, HeapId(0)),
            "1,203 more LeakyState than in snapshot #1."
        );
        let mut f = s.findings[0].clone();
        f.kind = LeakKind::DefunctElement;
        assert_eq!(
            finding_sentence(s, &f),
            "LeakyState is still in memory after it was unmounted."
        );
        f.kind = LeakKind::DisposedNotifier;
        assert_eq!(
            finding_sentence(s, &f),
            "LeakyState is still in memory after dispose() was called."
        );
    }

    #[gpui_kit::test]
    fn the_rows_are_on_screen(cx: &mut TestAppContext) {
        let snap = two_snapshots(true);
        let objects = snap.memory.heaps()[1].summary.as_ref().unwrap().objects;
        let (_shell, cx, _fake) = connect(cx, snap);
        assert_eq!(
            label(cx, "leaks-snapshot-2"),
            Some(format!(
                "#2 \u{b7} 0:02 \u{b7} {objects} objects \u{b7} 2 leak suspects"
            ))
        );
        assert_eq!(
            label(cx, "leaks-finding-0").as_deref(),
            Some("LeakyState is still in memory after leaving the widget tree.")
        );
        assert_eq!(
            label(cx, "leaks-growth-0").as_deref(),
            Some("1,203 more LeakyState than in snapshot #1.")
        );
        assert!(exists(cx, "leaks-baseline"));
    }

    #[test]
    fn profile_mode_note_replaces_the_notifier_line() {
        let snap = snapshot(|w, run| {
            heap_record_with(w, run, leaky, |s| {
                with_findings(s);
                s.mode = Mode::Profile;
                s.checks.notifiers = Check::Undecidable("profile".into());
            });
            heap_record_with(w, run, leaky, |s| {
                with_findings(s);
                s.checks.notifiers = Check::NotFound;
            });
        });
        let notes = |i: usize| check_notes(snap.memory.heaps()[i].summary.as_ref().unwrap());
        assert_eq!(
            notes(0),
            [
                "Couldn't check elements in this snapshot.".to_string(),
                PROFILE_NOTE.to_string()
            ]
        );
        assert_eq!(
            PROFILE_NOTE,
            "Profile mode can't tell a disposed controller or notifier from an unused one. Compare two snapshots, or run in debug mode to check them directly."
        );
        assert_eq!(
            notes(1),
            [
                "Couldn't check elements in this snapshot.".to_string(),
                "Couldn't check notifiers in this snapshot.".to_string()
            ]
        );
    }

    #[gpui_kit::test]
    fn finding_details_show_path_culprit_and_keeps(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.intern_alloc_class(
                run,
                "i/1",
                "c/1",
                "AppEvents",
                Some("package:app/events.dart"),
                Some("events.dart:9"),
            );
            let a = heap_record(w, run, |_| {});
            let b = heap_record_with(w, run, leaky, with_findings);
            grown(w, b, a, true);
        });
        let (shell, cx, _fake) = connect(cx, snap);
        go(&shell, cx, Some(leak0(1)));
        let d = leaks(&shell, cx)
            .read_with(cx, |t, _| t.details())
            .expect("details");
        assert_eq!(
            d.sentence,
            "LeakyState is still in memory after leaving the widget tree."
        );
        assert_eq!(d.retains, format!("Retains {}", format_bytes(1_258_291)));
        assert_eq!(d.held_by.as_deref(), Some("Held by AppEvents._listeners"));
        assert_eq!(d.hops.len(), 3);
        assert_eq!(d.hops[0].edge, "static instance");
        assert_eq!(d.hops[1].library, "events.dart:9");
        assert_eq!(d.hops[2].library, "leaky.dart");
        assert!(d.hops[1].cause && !d.hops[0].cause && !d.hops[2].cause);
        assert_eq!(d.keeps, [("AppEvents".to_string(), format_bytes(2_048))]);
        assert!(exists(cx, "leaks-cause"));
        assert_eq!(
            label(cx, "leaks-held-by").as_deref(),
            Some("Held by AppEvents._listeners")
        );
        assert_eq!(label(cx, "leaks-details-retains"), Some(d.retains.clone()));
        assert!(exists(cx, "leaks-keeps-0"));
        // The selection shows the row too.
        assert!(exists(cx, "leaks-details"));
    }

    #[test]
    fn a_truncated_path_starts_with_an_ellipsis_row_and_statics_read_plainly() {
        let snap = two_snapshots(true);
        let s = snap.memory.heaps()[1].summary.as_ref().unwrap();
        let state = class_ix(s, "LeakyState");
        let path: Vec<PathHop> = vec![
            PathHop {
                node: 9,
                class: state,
                retained: 10,
                edge: Edge::Static("controllers".into()),
            },
            PathHop {
                node: 10,
                class: state,
                retained: 5,
                edge: Edge::None,
            },
        ];
        let rows = hop_rows(&snap.memory, s, &path, Some(0));
        assert_eq!(rows.len(), 3);
        assert!(rows[0].truncated && rows[0].class == "\u{2026}");
        assert!(rows[1].cause);
        assert_eq!(
            held_by(s, &path, Some(0)).as_deref(),
            Some("Held by the static field controllers")
        );
        assert_eq!(held_by(s, &path, None), None);
        assert!(path_text(s, &path).starts_with("\u{2026} \u{2192} "));
    }

    #[gpui_kit::test]
    fn copy_path_uses_path_text(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        go(&shell, cx, Some(leak0(1)));
        click(cx, "leaks-copy-path");
        let snap = shell.read_with(cx, |s, cx| s.memory_view().read(cx).leaks().clone());
        let _ = snap;
        let want = {
            let snap = two_snapshots(true);
            let s = snap.memory.heaps()[1].summary.as_ref().unwrap();
            path_text(s, &s.findings[0].path)
        };
        let got = cx.update(|_, cx| cx.read_from_clipboard().and_then(|i| i.text()));
        assert_eq!(got.as_deref(), Some(want.as_str()));
        assert!(want.contains("AppEvents._listeners"));
    }

    fn with_navigation(w: &mut fdt_store::SessionWriter, run: fdt_store::time::RunId, ms: &[u64]) {
        for &m in ms {
            let mut l = log_at(run, m, LogLevel::Info);
            l.source = LogSource::Navigation;
            l.text = format!("/screen-{m}");
            w.push_log(l);
        }
    }

    #[gpui_kit::test]
    fn related_lists_navigation_between_snapshots(cx: &mut TestAppContext) {
        // The snapshots are at 1 s and 2 s.
        let snap = snapshot(|w, run| {
            with_navigation(w, run, &[500, 1_200, 1_700, 2_500]);
            let a = heap_record(w, run, |_| {});
            let b = heap_record_with(w, run, leaky, with_findings);
            grown(w, b, a, true);
        });
        let (shell, cx, _fake) = connect(cx, snap.clone());
        go(&shell, cx, Some(leak0(1)));
        let d = leaks(&shell, cx).read_with(cx, |t, _| t.details()).unwrap();
        assert_eq!(
            d.screens_heading,
            "Screen changes between snapshot #1 and #2"
        );
        assert_eq!(
            d.screens
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            ["0:01 \u{b7} /screen-1200", "0:01 \u{b7} /screen-1700"]
        );
        assert_eq!(
            label(cx, "leaks-screen-1").as_deref(),
            Some("0:01 \u{b7} /screen-1700")
        );
        click(cx, "leaks-screen-1");
        let loc = shell.read_with(cx, |s, _| s.current().clone());
        assert_eq!(loc.page, Page::Tool(Tool::Logs));
        assert_eq!(loc.selection, Some(Selection::Log(d.screens[1].id)));
    }

    #[gpui_kit::test]
    fn without_a_baseline_the_last_three_screen_changes_show(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            with_navigation(w, run, &[100, 200, 300, 400, 5_000]);
            heap_record_with(w, run, leaky, with_findings);
        });
        let (shell, cx, _fake) = connect(cx, snap);
        go(&shell, cx, Some(leak0(0)));
        let d = leaks(&shell, cx).read_with(cx, |t, _| t.details()).unwrap();
        assert_eq!(d.screens_heading, "Recent screen changes");
        assert_eq!(d.screens.len(), 3);
        assert!(d.screens[0].text.ends_with("/screen-200"));
        assert!(d.screens[2].text.ends_with("/screen-400"));
        // Around the snapshot (at 1 s), 2 s either way, from the start of the session.
        assert_eq!(
            d.timeline,
            TimeRange::new(SessionTime(0), SessionTime(3 * SEC))
        );
    }

    #[gpui_kit::test]
    fn timeline_link_sets_the_range(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        go(&shell, cx, Some(leak0(1)));
        click(cx, "leaks-show-timeline");
        let loc = shell.read_with(cx, |s, _| s.current().clone());
        assert_eq!(loc.page, Page::Tool(Tool::Timeline));
        assert_eq!(
            loc.range,
            Some(TimeRange::new(SessionTime(SEC), SessionTime(2 * SEC)))
        );
    }

    #[gpui_kit::test]
    fn show_in_snapshot_opens_the_snapshot_first_when_it_is_not_held(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            // #1 has the finding but #2 is the held one.
            heap_record_with(w, run, leaky, with_findings);
            heap_record(w, run, |_| {});
        });
        let (shell, cx, fake) = connect(cx, snap);
        go(&shell, cx, Some(leak0(0)));
        click(cx, "leaks-show-in-snapshot");
        assert!(fake.commands().contains(&"heap_open 0".to_string()));
        let loc = shell.read_with(cx, |s, _| s.current().clone());
        assert_eq!(
            loc.selection,
            Some(Selection::HeapNode {
                heap: HeapId(0),
                node: 4
            })
        );
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        assert_eq!(
            view.read_with(cx, |v, _| v.tab()),
            crate::tools::memory::MemoryTab::Snapshot
        );
    }

    #[gpui_kit::test]
    fn show_in_snapshot_on_the_held_snapshot_does_not_reopen_it(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, two_snapshots(true));
        go(&shell, cx, Some(leak0(1)));
        click(cx, "leaks-show-in-snapshot");
        assert!(!fake.commands().iter().any(|c| c.starts_with("heap_open")));
    }

    #[gpui_kit::test]
    fn baseline_picker_calls_capture_and_clears_growth(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, two_snapshots(true));
        let growth = Selection::Growth {
            heap: HeapId(1),
            index: 0,
        };
        go(&shell, cx, Some(growth));
        let tab = leaks(&shell, cx);
        let d = tab
            .read_with(cx, |t, _| t.details())
            .expect("growth details");
        assert_eq!(d.sentence, "1,203 more LeakyState than in snapshot #1.");
        assert_eq!(d.node, Some(4));
        tab.update(cx, |t, cx| t.set_baseline(Some(HeapId(0)), cx));
        cx.run_until_parked();
        assert!(fake.commands().contains(&"heap_baseline 0".to_string()));
        let sel = shell.read_with(cx, |s, _| s.current().selection);
        assert_eq!(sel, Some(Selection::LeakSnapshot(HeapId(1))));
        tab.update(cx, |t, cx| t.set_baseline(None, cx));
        assert!(fake.commands().contains(&"heap_baseline none".to_string()));
    }

    #[gpui_kit::test]
    fn a_new_diff_drops_a_growth_selection_from_the_palette_action(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, two_snapshots(true));
        let growth = Selection::Growth {
            heap: HeapId(1),
            index: 0,
        };
        go(&shell, cx, Some(growth));
        // "Use as Baseline" from the palette or menu does not go through the tab.
        cx.dispatch_action(crate::actions::UseAsBaseline);
        cx.run_until_parked();
        assert!(fake.commands().contains(&"heap_baseline 1".to_string()));
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(growth),
            "nothing changes until the diff does"
        );
        // The capture recomputes the diff.
        fake.snaps.send(two_snapshots(true)).unwrap();
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(Selection::LeakSnapshot(HeapId(1)))
        );
    }

    #[gpui_kit::test]
    fn not_comparable_shows_the_reason(cx: &mut TestAppContext) {
        let snap = two_snapshots(false);
        let reason = "The app restarted between these snapshots, so they can't be compared.";
        assert_eq!(
            lines(&snap.memory.heaps()[1])
                .iter()
                .rev()
                .take(2)
                .cloned()
                .collect::<Vec<_>>(),
            [
                Line::Note(reason.to_string()),
                Line::Baseline("Grew since snapshot #1".to_string())
            ]
        );
        let (_shell, cx, _fake) = connect(cx, snap);
        assert!(exists(cx, "leaks-grew"));
        assert!(!exists(cx, "leaks-growth-0"));
        assert!(exists(cx, "leaks-baseline"));
    }

    #[gpui_kit::test]
    fn removing_clears_selection(cx: &mut TestAppContext) {
        let (mut w, run) = crate::test_support::writer();
        heap_record(&mut w, run, |_| {});
        let b = heap_record_with(&mut w, run, leaky, with_findings);
        let first = w.publish();
        let (shell, cx, fake) = connect(cx, first);
        go(&shell, cx, Some(leak0(1)));
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(leak0(1))
        );
        cx.dispatch_action(crate::actions::RemoveHeapSnapshot);
        cx.run_until_parked();
        assert!(fake.commands().contains(&"heap_remove 1".to_string()));
        // The capture removes the record; the next snapshot no longer has it.
        w.remove_heap(b);
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(shell.read_with(cx, |s, _| s.current().selection), None);
        assert!(
            leaks(&shell, cx)
                .read_with(cx, |t, _| t.details())
                .is_none()
        );
    }

    #[gpui_kit::test]
    fn another_tabs_selection_is_kept(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        go(&shell, cx, Some(leak0(1)));
        let node = Selection::HeapNode {
            heap: HeapId(1),
            node: 4,
        };
        go(&shell, cx, Some(node));
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(node)
        );
        let _ = exists(cx, "leaks-tab");
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        assert_eq!(view.read_with(cx, |v, _| v.selection()), Some(node));
    }

    #[gpui_kit::test]
    fn a_lane_marker_click_opens_leaks_and_a_row_click_stays(cx: &mut TestAppContext) {
        use crate::tools::memory::MemoryTab;
        use crate::tools::timeline::lanes::LaneHit;
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        // Another tab was showing; the timeline's marker brings Leaks up.
        view.update(cx, |v, cx| v.set_tab(MemoryTab::Classes, cx));
        press(cx, "cmd-1", "ctrl-1");
        let timeline = shell.read_with(cx, |s, _| s.timeline_view().clone());
        timeline.update(cx, |t, cx| t.activate(LaneHit::Leak(HeapId(1)), cx));
        cx.run_until_parked();
        let loc = shell.read_with(cx, |s, _| s.current().clone());
        assert_eq!(loc.page, Page::Tool(Tool::Memory));
        assert_eq!(loc.selection, Some(Selection::LeakSnapshot(HeapId(1))));
        assert_eq!(view.read_with(cx, |v, _| v.tab()), MemoryTab::Leaks);
        // Choosing a snapshot in the list does not leave the Leaks tab.
        click(cx, "leaks-snapshot-1");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(Selection::LeakSnapshot(HeapId(0)))
        );
        assert_eq!(view.read_with(cx, |v, _| v.tab()), MemoryTab::Leaks);
    }

    #[gpui_kit::test]
    fn back_from_a_finding_lands_on_the_leaks_tab(cx: &mut TestAppContext) {
        use crate::tools::memory::MemoryTab;
        use crate::tools::timeline::lanes::LaneHit;
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        press(cx, "cmd-1", "ctrl-1");
        let timeline = shell.read_with(cx, |s, _| s.timeline_view().clone());
        timeline.update(cx, |t, cx| t.activate(LaneHit::Leak(HeapId(1)), cx));
        cx.run_until_parked();
        click(cx, "leaks-finding-0");
        let finding = leak0(1);
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(finding)
        );
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(Selection::LeakSnapshot(HeapId(1)))
        );
        assert_eq!(view.read_with(cx, |v, _| v.tab()), MemoryTab::Leaks);
        press(cx, "cmd-]", "ctrl-]");
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(finding)
        );
        assert_eq!(view.read_with(cx, |v, _| v.tab()), MemoryTab::Leaks);
    }

    #[gpui_kit::test]
    fn arrow_keys_move_through_the_rows(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_snapshots(true));
        click(cx, "leaks-finding-0");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(Selection::Growth {
                heap: HeapId(1),
                index: 0
            })
        );
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |s, _| s.current().selection),
            Some(leak0(1))
        );
    }

    #[test]
    fn screen_changes_are_a_range_lookup() {
        let snap = snapshot(|w, run| {
            let ms: Vec<u64> = (1..=100).map(|i| i * 10).collect();
            with_navigation(w, run, &ms);
        });
        let t = |ms: u64| SessionTime(ms * 1_000_000);
        let texts = |v: Vec<ScreenChange>| v.into_iter().map(|s| s.text).collect::<Vec<_>>();
        // Between 300 and 330 ms, inclusive.
        let got = texts(screen_changes(&snap, Some(t(300)), t(330)));
        assert_eq!(got.len(), 4);
        assert!(got[0].ends_with("/screen-300") && got[3].ends_with("/screen-330"));
        // No baseline: the last three at or before 335 ms.
        let got = texts(screen_changes(&snap, None, t(335)));
        assert!(got[0].ends_with("/screen-310") && got[2].ends_with("/screen-330"));
        assert!(screen_changes(&snap, Some(t(5_000)), t(6_000)).is_empty());
    }

    #[gpui_kit::test]
    fn growth_details_say_new_objects_use(cx: &mut TestAppContext) {
        let snap = two_snapshots(true);
        let d = build_details(
            &snap,
            Selection::Growth {
                heap: HeapId(1),
                index: 0,
            },
        )
        .unwrap();
        assert_eq!(
            d.retains,
            format!("New objects use {}", format_bytes(4_096))
        );
        let _ = cx;
    }
}
