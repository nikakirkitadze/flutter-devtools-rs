//! The Classes tab: instances and bytes per class from the allocation profile. With a range it
//! shows the change between the profiles nearest the range's two ends. The profile is fetched
//! on demand only (opening the tab, Refresh), never because a range moved.

use fdt_store::memory::{AllocClassId, MemorySnapshot};
use fdt_store::time::{SessionTime, TimeRange};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Said when a range holds one profile only.
pub const ONE_PROFILE_NOTE: &str = "One allocation profile covers this range. Refresh at the other end of the period you want to compare.";
/// Said when the compared profiles are from different isolates.
pub const RESTART_NOTE: &str = "The app restarted in this range, so classes are matched by name.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassRow {
    pub class: AllocClassId,
    pub name: Arc<str>,
    pub library: Option<Arc<str>>,
    pub user: bool,
    pub instances: u64,
    pub bytes: u64,
    /// Change in (instances, bytes) over the range.
    pub delta: Option<(i64, i64)>,
}

#[derive(Clone, Debug, Default)]
pub struct ClassRows {
    pub rows: Vec<ClassRow>,
    /// The two profiles compared, when a range has two.
    pub compared: Option<(SessionTime, SessionTime)>,
    pub note: Option<&'static str>,
    /// The time of the profile the counts are from.
    pub as_of: Option<SessionTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Class,
    Library,
    Instances,
    Size,
    DeltaInstances,
    DeltaSize,
}

/// Sorts `rows` by `key`; ties by class name.
pub fn sort_rows(rows: &mut [ClassRow], key: SortKey, desc: bool) {
    rows.sort_by(|a, b| {
        let o = match key {
            SortKey::Class => a.name.cmp(&b.name),
            SortKey::Library => a.library.cmp(&b.library),
            SortKey::Instances => a.instances.cmp(&b.instances),
            SortKey::Size => a.bytes.cmp(&b.bytes),
            SortKey::DeltaInstances => a.delta.map(|d| d.0).cmp(&b.delta.map(|d| d.0)),
            SortKey::DeltaSize => a.delta.map(|d| d.1).cmp(&b.delta.map(|d| d.1)),
        };
        let o = if desc { o.reverse() } else { o };
        o.then_with(|| a.name.cmp(&b.name))
    });
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

/// The rows of the latest profile, or with a range the rows of the profile nearest its end with
/// deltas from the one nearest its start. Sorted by size, largest first.
pub fn class_rows(mem: &MemorySnapshot, range: Option<TimeRange>, filter: &str) -> ClassRows {
    let profiles = mem.profiles();
    let needle = filter.trim().to_lowercase();
    let mut out = ClassRows::default();
    let (end_ix, start_ix) = match range {
        None => (profiles.len().checked_sub(1), None),
        Some(r) => (mem.nearest_profile(r.end), mem.nearest_profile(r.start)),
    };
    let Some(end_ix) = end_ix else {
        return out;
    };
    let end = &profiles[end_ix];
    out.as_of = Some(end.time);
    let base = match start_ix {
        Some(s) if s != end_ix => Some(&profiles[s]),
        Some(_) => {
            out.note = Some(ONE_PROFILE_NOTE);
            None
        }
        None => None,
    };
    let keep = |name: &str, library: Option<&str>| {
        needle.is_empty()
            || contains_ci(name, &needle)
            || library.is_some_and(|l| contains_ci(l, &needle))
    };
    let mk = |class: AllocClassId, instances: u64, bytes: u64, delta| {
        let c = mem.alloc_class(class);
        ClassRow {
            class,
            name: c.name.clone(),
            library: c.library.clone(),
            user: fdt_analysis::user_code::is_user_library(c.library.as_deref()),
            instances,
            bytes,
            delta,
        }
    };
    let d = |now: u64, then: u64| now as i64 - then as i64;
    if let Some(base) = base {
        out.compared = Some((base.time, end.time));
        let by_id = base.run == end.run && base.isolate == end.isolate;
        if !by_id {
            out.note = Some(RESTART_NOTE);
        }
        // Keys: class id, or (library, name) across a restart. The flag marks keys seen in `end`.
        let mut then_id: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut then_name: HashMap<(Option<&str>, &str), (AllocClassId, u64, u64)> = HashMap::new();
        for r in base.rows.iter() {
            if by_id {
                then_id.insert(r.class.0, (r.instances, r.bytes));
            } else {
                let c = mem.alloc_class(r.class);
                then_name.insert(
                    (c.library.as_deref(), &c.name),
                    (r.class, r.instances, r.bytes),
                );
            }
        }
        let mut seen_id: HashSet<u32> = HashSet::new();
        let mut seen_name: HashSet<(Option<&str>, &str)> = HashSet::new();
        for r in end.rows.iter() {
            let c = mem.alloc_class(r.class);
            if !keep(&c.name, c.library.as_deref()) {
                // Still mark it seen, so it is not listed as gone.
                if by_id {
                    seen_id.insert(r.class.0);
                } else {
                    seen_name.insert((c.library.as_deref(), &c.name));
                }
                continue;
            }
            let (ti, tb) = if by_id {
                seen_id.insert(r.class.0);
                then_id.get(&r.class.0).copied().unwrap_or((0, 0))
            } else {
                let k = (c.library.as_deref(), &*c.name);
                seen_name.insert(k);
                then_name.get(&k).map_or((0, 0), |&(_, i, b)| (i, b))
            };
            out.rows.push(mk(
                r.class,
                r.instances,
                r.bytes,
                Some((d(r.instances, ti), d(r.bytes, tb))),
            ));
        }
        // Classes that had instances then and none left.
        if by_id {
            for (&id, &(ti, tb)) in &then_id {
                let class = AllocClassId(id);
                let c = mem.alloc_class(class);
                if !seen_id.contains(&id)
                    && (ti > 0 || tb > 0)
                    && keep(&c.name, c.library.as_deref())
                {
                    out.rows
                        .push(mk(class, 0, 0, Some((-(ti as i64), -(tb as i64)))));
                }
            }
        } else {
            for (k, &(class, ti, tb)) in &then_name {
                if !seen_name.contains(k) && (ti > 0 || tb > 0) && keep(k.1, k.0) {
                    out.rows
                        .push(mk(class, 0, 0, Some((-(ti as i64), -(tb as i64)))));
                }
            }
        }
    } else {
        for r in end.rows.iter() {
            let c = mem.alloc_class(r.class);
            if keep(&c.name, c.library.as_deref()) {
                out.rows.push(mk(r.class, r.instances, r.bytes, None));
            }
        }
    }
    sort_rows(&mut out.rows, SortKey::Size, true);
    out
}

// ---- the tab ----

use super::details::{HistoryPoint, class_history, held_has_class};
use super::group_digits;
use crate::nav::{Location, Page, Selection, Tool};
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use crate::tools::network::model::format_bytes;
use crate::tools::{OpenLocation, ReplaceLocation};
use fdt_capture::ConnStatus;
use fdt_ui::empty_state::EmptyState;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, motion, radius, row, space, table};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState,
};
use gpui_kit::component::{Disableable as _, Sizable as _, Size};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, UniformListScrollHandle, Window, div,
    uniform_list,
};
use std::ops::Range;
use std::time::{Duration, Instant};

/// The tooltip of Refresh while an app is connected.
pub const REFRESH_TIP: &str = "Refreshing pauses your app briefly (about 15 ms in profile mode).";
/// The tooltip of Refresh while no app is connected.
pub const DISCONNECTED_TIP: &str = "Connect to an app to refresh the allocation profile.";
/// The tooltip of Refresh while a profile is being fetched.
pub const FETCHING_TIP: &str = "Waiting for the allocation profile.";
pub const LIVE_ONLY_NOTE: &str = "Runs a full garbage collection in the app before counting.";
pub const GARBAGE_NOTE: &str = "Counts include objects waiting for garbage collection.";
const EMPTY_BODY: &str =
    "Refresh to count instances per class. Refreshing pauses your app for a moment.";
const NO_MATCH: &str = "No classes match your filter.";

/// A profile newer than this is not fetched again when the tab opens.
const STALE_AFTER: Duration = Duration::from_secs(5); // token-check: allow: age limit, not a visual duration
/// How long a fetch is waited for before the tab stops showing it as running.
const FETCH_GIVE_UP: Duration = Duration::from_secs(15); // token-check: allow: timeout, not a visual duration

/// Asks the Memory page to open the Snapshot tab with this class searched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShowInSnapshot(pub SharedString);

#[derive(Clone, Debug, PartialEq)]
struct Key {
    session: u64,
    profiles: usize,
    oldest: Option<SessionTime>,
    newest: Option<SessionTime>,
    range: Option<TimeRange>,
    filter: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistKey {
    session: u64,
    class: AllocClassId,
    profiles: usize,
    newest: Option<SessionTime>,
}

const COLS: [(SortKey, &str, &str); 6] = [
    (SortKey::Class, "class", "Class"),
    (SortKey::Library, "library", "Library"),
    (SortKey::Instances, "instances", "Instances"),
    (SortKey::Size, "size", "Size"),
    (SortKey::DeltaInstances, "d-instances", "\u{394}Instances"),
    (SortKey::DeltaSize, "d-size", "\u{394}Size"),
];

fn signed(n: i64) -> String {
    match n {
        0 => "0".into(),
        n if n > 0 => format!("+{}", group_digits(n as u64)),
        n => format!("\u{2212}{}", group_digits(n.unsigned_abs())),
    }
}

fn signed_bytes(n: i64) -> String {
    match n {
        0 => "0 B".into(),
        n if n > 0 => format!("+{}", format_bytes(n as u64)),
        n => format!("\u{2212}{}", format_bytes(n.unsigned_abs())),
    }
}

struct ClassTable {
    rows: Vec<ClassRow>,
    deltas: bool,
    sort: (SortKey, bool),
}

impl ClassTable {
    fn text(&self, row: usize, col: usize) -> String {
        let Some(r) = self.rows.get(row) else {
            return String::new();
        };
        match col {
            0 => r.name.to_string(),
            1 => r.library.as_deref().unwrap_or("").to_string(),
            2 => group_digits(r.instances),
            3 => format_bytes(r.bytes),
            4 => r.delta.map_or(String::new(), |d| signed(d.0)),
            _ => r.delta.map_or(String::new(), |d| signed_bytes(d.1)),
        }
    }
}

impl TableDelegate for ClassTable {
    fn columns_count(&self, _: &App) -> usize {
        if self.deltas { 6 } else { 4 }
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let (key, id, label) = COLS[col_ix];
        let c = Column::new(id, label).width(match col_ix {
            0 => table::COL_WIDGET,
            1 => table::COL_PATH,
            2 | 4 => table::COL_COUNT,
            _ => table::COL_BYTES,
        });
        let c = if col_ix >= 2 { c.text_right() } else { c };
        if self.sort.0 == key {
            if self.sort.1 {
                c.descending()
            } else {
                c.ascending()
            }
        } else {
            c.sortable()
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.sort = match sort {
            ColumnSort::Default => (SortKey::Size, true),
            ColumnSort::Ascending => (COLS[col_ix].0, false),
            ColumnSort::Descending => (COLS[col_ix].0, true),
        };
        sort_rows(&mut self.rows, self.sort.0, self.sort.1);
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        Some(self.text(row_ix, col_ix).into())
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = *cx.tokens();
        let text = self.text(row_ix, col_ix);
        if col_ix == 0 {
            let user = self.rows.get(row_ix).is_some_and(|r| r.user);
            let c = Hsla::from(t.accent);
            return div()
                .w_full()
                .flex()
                .items_center()
                .gap(space::S8)
                .text_size(font::DENSE)
                .text_color(Hsla::from(t.text))
                .child(div().flex_1().min_w_0().truncate().child(text))
                .children(user.then(|| {
                    div()
                        .flex_none()
                        .px(space::S4)
                        .rounded(radius::CHIP)
                        .bg(c.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
                        .text_size(font::LABEL)
                        .text_color(c)
                        .child("your code")
                }))
                .into_any_element();
        }
        let delta = self.rows.get(row_ix).and_then(|r| r.delta);
        let colour = match (col_ix, delta) {
            (4, Some((d, _))) | (5, Some((_, d))) if d > 0 => t.warning,
            (4, Some((d, _))) | (5, Some((_, d))) if d < 0 => t.success,
            _ if col_ix == 1 => t.text_muted,
            _ => t.text,
        };
        let cell = div()
            .w_full()
            .truncate()
            .text_size(font::DENSE)
            .font_features(font::tabular_figures())
            .text_color(Hsla::from(colour));
        let cell = if col_ix >= 2 { cell.text_right() } else { cell };
        cell.child(text).into_any_element()
    }
}

pub struct ClassesTab {
    session: Entity<SessionModel>,
    table: Entity<TableState<ClassTable>>,
    input: Entity<InputState>,
    filter: String,
    range: Option<TimeRange>,
    selection: Option<AllocClassId>,
    live_only: bool,
    active: bool,
    compared: Option<(SessionTime, SessionTime)>,
    note: Option<&'static str>,
    as_of: Option<SessionTime>,
    /// Whether the rows came from a job yet.
    loaded: bool,
    started: Option<Key>,
    job: Option<Task<()>>,
    /// When the profile was last asked for, and the newest profile then.
    fetched: Option<(Instant, Option<SessionTime>)>,
    fetching: bool,
    skeleton: bool,
    fetch_timer: Option<Task<()>>,
    history: Option<(HistKey, Arc<Vec<HistoryPoint>>)>,
    history_started: Option<HistKey>,
    history_job: Option<Task<()>>,
    history_scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenLocation> for ClassesTab {}
impl EventEmitter<ReplaceLocation> for ClassesTab {}
impl EventEmitter<ShowInSnapshot> for ClassesTab {}

impl ClassesTab {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by class or library"));
        let table = cx.new(|cx| {
            TableState::new(
                ClassTable {
                    rows: Vec::new(),
                    deltas: false,
                    sort: (SortKey::Size, true),
                },
                window,
                cx,
            )
            .loop_selection(false)
            .col_movable(false)
        });
        let subs = vec![
            cx.observe(&session, |this, _, cx| {
                if !this.connected(cx) {
                    this.clear_fetch();
                }
                if this.active {
                    this.sync(cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&input, window, |this, _, e: &InputEvent, _, cx| {
                if matches!(e, InputEvent::Change) {
                    let v = this.input.read(cx).value().trim().to_lowercase();
                    if v != this.filter {
                        this.filter = v;
                        this.sync(cx);
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(
                &table,
                window,
                |this, _, e: &TableEvent, window, cx| match e {
                    TableEvent::SelectRow(row) => {
                        this.row_selected(*row, window.last_input_was_keyboard(), cx)
                    }
                    TableEvent::ClearSelection if this.selection.take().is_some() => {
                        this.emit_selection(None, true, cx);
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        Self {
            session,
            table,
            input,
            filter: String::new(),
            range: None,
            selection: None,
            live_only: false,
            active: false,
            compared: None,
            note: None,
            as_of: None,
            loaded: false,
            started: None,
            job: None,
            fetched: None,
            fetching: false,
            skeleton: false,
            fetch_timer: None,
            history: None,
            history_started: None,
            history_job: None,
            history_scroll: UniformListScrollHandle::new(),
            _subs: subs,
        }
    }

    pub fn live_only(&self) -> bool {
        self.live_only
    }

    fn connected(&self, cx: &App) -> bool {
        matches!(self.session.read(cx).status(), ConnStatus::Connected { .. })
    }

    /// Whether Refresh cannot be used.
    pub fn refresh_disabled(&self, cx: &App) -> bool {
        !self.connected(cx) || self.fetching
    }

    /// Forgets a fetch in flight: it was answered, abandoned or can no longer be answered.
    fn clear_fetch(&mut self) {
        self.fetching = false;
        self.skeleton = false;
        self.fetch_timer = None;
    }

    /// A new session: nothing of the old one stays.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.range = None;
        self.selection = None;
        self.compared = None;
        self.note = None;
        self.as_of = None;
        self.loaded = false;
        self.started = None;
        self.job = None;
        self.fetched = None;
        self.fetching = false;
        self.skeleton = false;
        self.fetch_timer = None;
        self.history = None;
        self.history_started = None;
        self.history_job = None;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().rows.clear();
            cx.notify();
        });
        cx.notify();
    }

    /// Whether the Classes tab is on screen. Showing it fetches a profile when there is none or
    /// the newest is over 5 s old.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            let snap = self.session.read(cx).snapshot().clone();
            let limit = STALE_AFTER.as_nanos() as u64;
            let stale = snap
                .memory
                .profiles()
                .last()
                .is_none_or(|p| snap.latest_time().0.saturating_sub(p.time.0) > limit);
            // The answer to an earlier request may not have been published yet.
            if stale {
                self.request(false, cx);
            }
            self.sync(cx);
            cx.notify();
        }
    }

    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.sync(cx);
            cx.notify();
        }
    }

    /// The location's selection. Only a class of the profile is the tab's; anything else is
    /// ignored (and not cleared: other tabs own it).
    pub fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        let class = match selection {
            Some(Selection::AllocClass(id)) => Some(id),
            _ => None,
        };
        if class == self.selection {
            return;
        }
        self.selection = class;
        self.show_selection(cx);
        self.sync_history(cx);
        cx.notify();
    }

    pub fn set_live_only(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.live_only != on {
            self.live_only = on;
            cx.notify();
        }
    }

    /// Asks the app for a profile; with a full GC only when "Live objects only" is on. Without
    /// an app it does nothing.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request(self.live_only, cx);
    }

    /// Sends one request for a profile. Only an explicit Refresh passes `gc`; the fetch on
    /// opening never forces a collection. Ignored while a request is in flight (the timer below
    /// bounds that) and without an app.
    fn request(&mut self, gc: bool, cx: &mut Context<Self>) {
        if !self.connected(cx) || self.fetching {
            return;
        }
        let snap = self.session.read(cx).snapshot().clone();
        let newest = snap.memory.profiles().last().map(|p| p.time);
        let now = cx.background_executor().now();
        if let Some(control) = self.session.read(cx).control() {
            control.fetch_allocation_profile(gc);
        }
        self.fetched = Some((now, newest));
        self.fetching = true;
        self.skeleton = false;
        self.fetch_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if this.fetching && this.table.read(cx).delegate().rows.is_empty() {
                    this.skeleton = true;
                    cx.notify();
                }
            })
            .ok();
            cx.background_executor().timer(FETCH_GIVE_UP).await;
            this.update(cx, |this, cx| {
                this.clear_fetch();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn key(&self, cx: &App) -> Key {
        let snap = self.session.read(cx).snapshot();
        let p = snap.memory.profiles();
        Key {
            session: snap.session,
            profiles: p.len(),
            oldest: p.first().map(|p| p.time),
            newest: p.last().map(|p| p.time),
            range: self.range,
            filter: self.filter.clone(),
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let key = self.key(cx);
        if let Some((_, base)) = self.fetched
            && self.fetching
            && key.newest != base
        {
            self.clear_fetch();
        }
        self.sync_history(cx);
        if self.started.as_ref() == Some(&key) {
            return;
        }
        self.started = Some(key.clone());
        let snap = self.session.read(cx).snapshot().clone();
        let (range, filter) = (key.range, key.filter.clone());
        self.job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn(async move { class_rows(&snap.memory, range, &filter) })
                .await;
            this.update(cx, |this, cx| {
                this.finish(&key, out, cx);
            })
            .ok();
        }));
    }

    /// Takes the rows of the job for `key`, unless a newer job replaced it.
    fn finish(&mut self, key: &Key, out: ClassRows, cx: &mut Context<Self>) {
        if self.started.as_ref() == Some(key) {
            self.apply(out, cx);
        }
    }

    fn apply(&mut self, out: ClassRows, cx: &mut Context<Self>) {
        self.compared = out.compared;
        self.note = out.note;
        self.as_of = out.as_of;
        self.loaded = true;
        let deltas = out.compared.is_some();
        self.table.update(cx, |t, cx| {
            let changed = t.delegate().deltas != deltas;
            let d = t.delegate_mut();
            d.deltas = deltas;
            d.rows = out.rows;
            if !deltas && matches!(d.sort.0, SortKey::DeltaInstances | SortKey::DeltaSize) {
                d.sort = (SortKey::Size, true);
            }
            sort_rows(&mut d.rows, d.sort.0, d.sort.1);
            if changed {
                t.refresh(cx);
            }
            cx.notify();
        });
        self.show_selection(cx);
        cx.notify();
    }

    fn show_selection(&mut self, cx: &mut Context<Self>) {
        let row = self.selection.and_then(|id| {
            self.table
                .read(cx)
                .delegate()
                .rows
                .iter()
                .position(|r| r.class == id)
        });
        self.table.update(cx, |t, cx| match row {
            Some(r) if t.selected_row() != Some(r) => t.set_selected_row(r, cx),
            None if t.selected_row().is_some() => t.clear_selection(cx),
            _ => {}
        });
    }

    fn sync_history(&mut self, cx: &mut Context<Self>) {
        let Some(class) = self.selection else {
            self.history = None;
            self.history_started = None;
            return;
        };
        let snap = self.session.read(cx).snapshot().clone();
        let key = HistKey {
            session: snap.session,
            class,
            profiles: snap.memory.profiles().len(),
            newest: snap.memory.profiles().last().map(|p| p.time),
        };
        if self.history_started.as_ref() == Some(&key) {
            return;
        }
        self.history_started = Some(key.clone());
        self.history_job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn(async move { class_history(&snap.memory, class) })
                .await;
            this.update(cx, |this, cx| {
                if this.history_started.as_ref() == Some(&key) {
                    this.history = Some((key, Arc::new(out)));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn row_selected(&mut self, row: usize, by_key: bool, cx: &mut Context<Self>) {
        let Some(id) = self
            .table
            .read(cx)
            .delegate()
            .rows
            .get(row)
            .map(|r| r.class)
        else {
            return;
        };
        if self.selection == Some(id) {
            return;
        }
        self.selection = Some(id);
        self.sync_history(cx);
        self.emit_selection(Some(id), by_key, cx);
        cx.notify();
    }

    fn emit_selection(&mut self, id: Option<AllocClassId>, replace: bool, cx: &mut Context<Self>) {
        let loc = Location {
            page: Page::Tool(Tool::Memory),
            selection: id.map(Selection::AllocClass),
            range: self.range,
        };
        if replace {
            cx.emit(ReplaceLocation(loc));
        } else {
            cx.emit(OpenLocation(loc));
        }
    }

    fn show_in_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selection else { return };
        let snap = self.session.read(cx).snapshot().clone();
        if id.0 as usize >= snap.memory.alloc_class_count() {
            return;
        }
        let name = snap.memory.alloc_class(id).name.to_string();
        cx.emit(ShowInSnapshot(name.into()));
    }

    #[cfg(test)]
    pub fn fetch_state(&self) -> (bool, bool, bool) {
        (self.fetching, self.skeleton, self.fetch_timer.is_some())
    }

    #[cfg(test)]
    fn set_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.filter = filter.to_string();
        self.sync(cx);
    }

    #[cfg(test)]
    fn started_key(&self) -> Option<Key> {
        self.started.clone()
    }

    #[cfg(test)]
    pub fn is_settled(&self, cx: &App) -> bool {
        self.started.as_ref() == Some(&self.key(cx)) && self.loaded
    }

    #[cfg(test)]
    pub fn cells(&self, cx: &App) -> Vec<Vec<String>> {
        let d = self.table.read(cx).delegate();
        (0..d.rows.len())
            .map(|r| (0..d.columns_count(cx)).map(|c| d.text(r, c)).collect())
            .collect()
    }

    #[cfg(test)]
    pub fn note(&self) -> Option<&'static str> {
        self.note
    }

    #[cfg(test)]
    pub fn history_points(&self) -> Option<Vec<(u64, u64, u64)>> {
        self.history.as_ref().map(|(_, h)| {
            h.iter()
                .map(|p| (p.time.0 / 1_000_000, p.instances, p.bytes))
                .collect()
        })
    }

    #[cfg(test)]
    pub fn selected_class(&self) -> Option<AllocClassId> {
        self.selection
    }
}

impl ClassesTab {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let connected = self.connected(cx);
        let view = cx.weak_entity();
        let mut refresh = Button::new("classes-refresh")
            .label("Refresh")
            .compact()
            .tooltip(if !connected {
                DISCONNECTED_TIP
            } else if self.fetching {
                FETCHING_TIP
            } else {
                REFRESH_TIP
            })
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        if !connected || self.fetching {
            refresh = refresh.disabled(true);
        }
        let live = Switch::new("classes-live-only")
            .checked(self.live_only)
            .label("Live objects only")
            .tooltip(LIVE_ONLY_NOTE)
            .on_change(move |on: &bool, _, cx| {
                let on = *on;
                view.update(cx, |v, cx| v.set_live_only(on, cx)).ok();
            });
        let note = |id: &'static str, text: String| {
            div()
                .id(id)
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(text)
        };
        let mut bar = div()
            .id("classes-toolbar")
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
            .child(refresh)
            .child(live)
            .child(note("classes-live-only-note", LIVE_ONLY_NOTE.to_string()));
        if let Some(at) = self.as_of {
            bar = bar.child(note(
                "classes-as-of",
                format!("As of {}", format_offset(at)),
            ));
        }
        if let Some((a, b)) = self.compared {
            bar = bar.child(note(
                "classes-compared",
                format!("Compared {} to {}", format_offset(a), format_offset(b)),
            ));
        }
        if !self.live_only && self.as_of.is_some() {
            bar = bar.child(note("classes-garbage-note", GARBAGE_NOTE.to_string()));
        }
        bar.child(div().flex_1())
            .child(
                div()
                    .id("classes-filter")
                    .test_support()
                    .flex_none()
                    .w(layout::SEARCH_WIDTH)
                    .min_w(layout::SEARCH_MIN)
                    .child(Input::new(&self.input).cleanable(true)),
            )
            .into_any_element()
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let empty = self.table.read(cx).delegate().rows.is_empty();
        let body = if empty && self.skeleton {
            div()
                .id("classes-loading")
                .test_support()
                .flex_1()
                .flex()
                .flex_col()
                .gap(space::S8)
                .p(space::S16)
                .children((0..6).map(|_| Skeleton::new().h(row::TABLE)))
                .into_any_element()
        } else if empty && self.as_of.is_none() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "memory-classes-empty",
                    crate::shell::sidebar::tool_icon(Tool::Memory),
                    "No allocation profile yet",
                    EMPTY_BODY,
                ))
                .into_any_element()
        } else if empty {
            div()
                .id("classes-no-match")
                .test_support()
                .aria_label(NO_MATCH)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child(NO_MATCH)
                .into_any_element()
        } else {
            div()
                .id("classes-table")
                .test_support()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    DataTable::new(&self.table)
                        .bordered(false)
                        .with_size(Size::Size(row::TABLE)),
                )
                .into_any_element()
        };
        let mut col = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx));
        if let Some(note) = self.note {
            col = col.child(
                div()
                    .id("classes-range-note")
                    .test_support()
                    .aria_label(note)
                    .flex_none()
                    .px(space::S16)
                    .py(space::S4)
                    .border_b(layout::BORDER)
                    .border_color(Hsla::from(t.border))
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(note),
            );
        }
        col.child(body).into_any_element()
    }

    fn render_history_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = *cx.tokens();
        let Some((_, h)) = &self.history else {
            return Vec::new();
        };
        let end = range.end.min(h.len());
        (range.start.min(end)..end)
            .map(|i| {
                let p = h[i];
                div()
                    .h(row::LOG)
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .px(space::S12)
                    .text_size(font::DENSE)
                    .font_features(font::tabular_figures())
                    .child(
                        div()
                            .w(table::COL_COUNT)
                            .text_color(Hsla::from(t.text_muted))
                            .child(format_offset(p.time)),
                    )
                    .child(div().flex_1().text_right().child(p.instances_text()))
                    .child(div().w(table::COL_BYTES).text_right().child(p.size_text()))
                    .into_any_element()
            })
            .collect()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.selection?;
        let t = *cx.tokens();
        let snap = self.session.read(cx).snapshot().clone();
        if id.0 as usize >= snap.memory.alloc_class_count() {
            return None;
        }
        let class = snap.memory.alloc_class(id).clone();
        let count = self.history.as_ref().map_or(0, |(_, h)| h.len());
        let can_show = held_has_class(&snap.memory, &class.name, class.library.as_deref());
        let mut pane = div()
            .id("class-details")
            .test_support()
            .w(layout::DETAILS_WIDTH)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_l(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .bg(Hsla::from(t.surface))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text))
            .child(
                div()
                    .id("class-details-title")
                    .test_support()
                    .aria_label(SharedString::from(class.name.to_string()))
                    .p(space::S12)
                    .font_weight(font::SEMIBOLD)
                    .child(class.name.to_string()),
            );
        if let Some(lib) = &class.library {
            pane = pane.child(
                div()
                    .px(space::S12)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(lib.to_string()),
            );
        }
        if let Some(loc) = &class.location {
            pane = pane.child(
                div()
                    .id("class-details-location")
                    .test_support()
                    .aria_label(SharedString::from(loc.to_string()))
                    .px(space::S12)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(loc.to_string()),
            );
        }
        if can_show {
            pane = pane.child(
                div().p(space::S12).child(
                    Button::new("class-show-in-snapshot")
                        .label("Show in snapshot")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.show_in_snapshot(cx))),
                ),
            );
        }
        Some(
            pane.child(
                div()
                    .px(space::S12)
                    .py(space::S4)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child("Instances over time"),
            )
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list(
                        "class-details-history",
                        count,
                        cx.processor(|this, range: Range<usize>, _w, cx| {
                            this.render_history_rows(range, cx)
                        }),
                    )
                    .track_scroll(&self.history_scroll)
                    .size_full(),
                ),
            )
            .into_any_element(),
        )
    }
}

impl Render for ClassesTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("classes-tab")
            .test_support()
            .size_full()
            .flex()
            .child(self.render_main(cx))
            .children(self.render_details(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::writer;
    use fdt_store::memory::{AllocProfile, AllocRow};
    use fdt_store::time::RunId;

    const MS: u64 = 1_000_000;

    fn push(w: &mut fdt_store::SessionWriter, run: RunId, iso: &str, ms: u64, rows: &[AllocRow]) {
        w.push_alloc_profile(AllocProfile {
            time: SessionTime(ms * MS),
            run,
            isolate: iso.into(),
            gc: false,
            heap_used: 0,
            rows: Arc::from(rows.to_vec()),
        });
    }

    fn row(class: AllocClassId, instances: u64, bytes: u64) -> AllocRow {
        AllocRow {
            class,
            instances,
            bytes,
        }
    }

    fn range(a: u64, b: u64) -> Option<TimeRange> {
        Some(TimeRange::new(SessionTime(a * MS), SessionTime(b * MS)))
    }

    fn names(r: &ClassRows) -> Vec<&str> {
        r.rows.iter().map(|r| &*r.name).collect()
    }

    #[test]
    fn latest_profile_without_range() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(run, "i/1", "c/1", "Small", Some("dart:core"), None);
        let b = w.intern_alloc_class(run, "i/1", "c/2", "Big", Some("package:app/a.dart"), None);
        push(&mut w, run, "i/1", 100, &[row(a, 1, 10), row(b, 1, 10)]);
        push(&mut w, run, "i/1", 200, &[row(a, 3, 30), row(b, 2, 900)]);
        let snap = w.publish();
        let r = class_rows(&snap.memory, None, "");
        assert_eq!(names(&r), ["Big", "Small"]);
        assert_eq!(r.rows[0].bytes, 900);
        assert!(r.rows[0].user && !r.rows[1].user);
        assert!(r.compared.is_none() && r.note.is_none());
        assert!(r.rows.iter().all(|r| r.delta.is_none()));
        assert_eq!(r.as_of, Some(SessionTime(200 * MS)));
        assert!(
            class_rows(&writer().0.publish().memory, None, "")
                .rows
                .is_empty()
        );
    }

    #[test]
    fn range_compares_the_nearest_profiles() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(run, "i/1", "c/1", "Foo", Some("package:app/a.dart"), None);
        let b = w.intern_alloc_class(run, "i/1", "c/2", "Bar", Some("package:app/a.dart"), None);
        let c = w.intern_alloc_class(run, "i/1", "c/3", "Gone", Some("package:app/a.dart"), None);
        push(
            &mut w,
            run,
            "i/1",
            100,
            &[row(a, 1, 16), row(b, 10, 100), row(c, 4, 40)],
        );
        push(&mut w, run, "i/1", 500, &[row(a, 5, 80), row(b, 4, 40)]);
        push(
            &mut w,
            run,
            "i/1",
            900,
            &[row(a, 9, 144), row(b, 4, 40), row(c, 0, 0)],
        );
        let snap = w.publish();
        // Nearest to 120 is the first profile, nearest to 880 the last.
        let r = class_rows(&snap.memory, range(120, 880), "");
        assert_eq!(
            r.compared,
            Some((SessionTime(100 * MS), SessionTime(900 * MS)))
        );
        assert!(r.note.is_none());
        let by = |n: &str| r.rows.iter().find(|r| &*r.name == n).unwrap().delta;
        assert_eq!(by("Foo"), Some((8, 128)));
        assert_eq!(by("Bar"), Some((-6, -60)));
        assert_eq!(by("Gone"), Some((-4, -40)));
    }

    #[test]
    fn one_profile_in_range_says_so() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(run, "i/1", "c/1", "Foo", None, None);
        push(&mut w, run, "i/1", 100, &[row(a, 1, 16)]);
        let snap = w.publish();
        let r = class_rows(&snap.memory, range(50, 400), "");
        assert_eq!(r.note, Some(ONE_PROFILE_NOTE));
        assert!(r.compared.is_none());
        assert_eq!(r.rows.len(), 1);
        assert!(r.rows[0].delta.is_none());
    }

    #[test]
    fn restart_matches_by_name_with_a_note() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(run, "i/1", "c/1", "Foo", Some("package:app/a.dart"), None);
        let b = w.intern_alloc_class(run, "i/2", "c/7", "Foo", Some("package:app/a.dart"), None);
        let other =
            w.intern_alloc_class(run, "i/2", "c/8", "Foo", Some("package:app/b.dart"), None);
        push(&mut w, run, "i/1", 100, &[row(a, 2, 32)]);
        push(&mut w, run, "i/2", 900, &[row(b, 5, 80), row(other, 1, 16)]);
        let snap = w.publish();
        let r = class_rows(&snap.memory, range(100, 900), "");
        assert_eq!(r.note, Some(RESTART_NOTE));
        let foo = r
            .rows
            .iter()
            .find(|r| r.library.as_deref() == Some("package:app/a.dart"))
            .unwrap();
        assert_eq!(foo.delta, Some((3, 48)));
        let same_name = r
            .rows
            .iter()
            .find(|r| r.library.as_deref() == Some("package:app/b.dart"))
            .unwrap();
        assert_eq!(same_name.delta, Some((1, 16)));
    }

    #[test]
    fn filter_and_sort() {
        let (mut w, run) = writer();
        let a = w.intern_alloc_class(
            run,
            "i/1",
            "c/1",
            "Apple",
            Some("package:fruit/a.dart"),
            None,
        );
        let b = w.intern_alloc_class(
            run,
            "i/1",
            "c/2",
            "Banana",
            Some("package:fruit/b.dart"),
            None,
        );
        let c = w.intern_alloc_class(run, "i/1", "c/3", "Car", Some("dart:core"), None);
        push(
            &mut w,
            run,
            "i/1",
            100,
            &[row(a, 3, 30), row(b, 1, 50), row(c, 2, 20)],
        );
        let snap = w.publish();
        assert_eq!(
            names(&class_rows(&snap.memory, None, "")),
            ["Banana", "Apple", "Car"]
        );
        assert_eq!(names(&class_rows(&snap.memory, None, "APP")), ["Apple"]);
        // The library matches too.
        assert_eq!(
            names(&class_rows(&snap.memory, None, "fruit")),
            ["Banana", "Apple"]
        );
        let mut rows = class_rows(&snap.memory, None, "").rows;
        sort_rows(&mut rows, SortKey::Instances, true);
        assert_eq!(
            rows.iter().map(|r| &*r.name).collect::<Vec<_>>(),
            ["Apple", "Car", "Banana"]
        );
        sort_rows(&mut rows, SortKey::Class, false);
        assert_eq!(
            rows.iter().map(|r| &*r.name).collect::<Vec<_>>(),
            ["Apple", "Banana", "Car"]
        );
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::actions::{ShowClassesTab, ShowLeaksTab};
    use crate::nav::Selection;
    use crate::test_support::{connected, exists, heap_record, open_shell_fake, press, snapshot};
    use crate::tools::memory::MemoryTab;
    use fdt_store::Snapshot;
    use fdt_store::memory::{AllocProfile, AllocRow, MemorySample};
    use fdt_store::time::RunId;
    use fdt_ui::tokens::motion;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    const MS: u64 = 1_000_000;

    fn row(class: AllocClassId, instances: u64, bytes: u64) -> AllocRow {
        AllocRow {
            class,
            instances,
            bytes,
        }
    }

    fn profile(w: &mut fdt_store::SessionWriter, run: RunId, ms: u64, rows: &[AllocRow]) {
        w.push_alloc_profile(AllocProfile {
            time: SessionTime(ms * MS),
            run,
            isolate: "i/1".into(),
            gc: false,
            heap_used: 0,
            rows: Arc::from(rows.to_vec()),
        });
    }

    /// A profile at `profile_ms`, and a memory sample at `now_ms` for the session's clock.
    fn with_profile(profile_ms: u64, now_ms: u64) -> Arc<Snapshot> {
        snapshot(|w, run| {
            let a = w.intern_alloc_class(
                run,
                "i/1",
                "c/1",
                "Foo",
                Some("package:app/a.dart"),
                Some("a.dart:3"),
            );
            profile(w, run, profile_ms, &[row(a, 1, 16)]);
            w.push_memory_sample(
                run,
                MemorySample {
                    time: SessionTime(now_ms * MS),
                    heap_used: 1,
                    heap_capacity: 2,
                    external: 0,
                    rss: None,
                },
            );
        })
    }

    fn empty_with_sample() -> Arc<Snapshot> {
        snapshot(|w, run| {
            w.push_memory_sample(
                run,
                MemorySample {
                    time: SessionTime(MS),
                    heap_used: 1,
                    heap_capacity: 2,
                    external: 0,
                    rss: None,
                },
            );
        })
    }

    fn alloc_commands(fake: &crate::test_support::FakeHandle) -> Vec<String> {
        fake.commands()
            .into_iter()
            .filter(|c| c.starts_with("alloc_profile"))
            .collect()
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

    fn classes(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
    ) -> Entity<ClassesTab> {
        shell.read_with(cx, |s, cx| s.memory_view().read(cx).classes().clone())
    }

    fn show_classes(cx: &mut VisualTestContext) {
        cx.dispatch_action(ShowClassesTab);
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn opening_fetches_only_when_stale(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = connect(cx, empty_with_sample());
        assert!(
            alloc_commands(&fake).is_empty(),
            "the Leaks tab asks for nothing"
        );
        show_classes(cx);
        assert_eq!(alloc_commands(&fake), ["alloc_profile false"]);
        assert!(exists(cx, "memory-classes-empty"));
        // A fresh profile arrives; leaving and coming back does not ask again.
        fake.snaps.send(with_profile(900, 1_000)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowLeaksTab);
        cx.run_until_parked();
        show_classes(cx);
        assert_eq!(alloc_commands(&fake).len(), 1);
    }

    #[gpui_kit::test]
    fn opening_with_a_fresh_profile_does_not_fetch(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = connect(cx, with_profile(9_000, 10_000));
        show_classes(cx);
        assert!(alloc_commands(&fake).is_empty());
    }

    #[gpui_kit::test]
    fn opening_with_an_old_profile_fetches(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = connect(cx, with_profile(1_000, 10_000));
        show_classes(cx);
        assert_eq!(alloc_commands(&fake), ["alloc_profile false"]);
    }

    #[gpui_kit::test]
    fn live_only_fetches_with_gc(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, with_profile(9_000, 10_000));
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert!(!tab.read_with(cx, |t, _| t.live_only()));
        tab.update(cx, |t, cx| t.set_live_only(true, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("classes-refresh", cx)
        });
        cx.run_until_parked();
        assert_eq!(alloc_commands(&fake), ["alloc_profile true"]);
        // While it is in flight, a second Refresh (button or palette) sends nothing.
        assert!(tab.read_with(cx, |t, cx| t.refresh_disabled(cx)));
        cx.dispatch_action(crate::actions::RefreshAllocationProfile);
        cx.run_until_parked();
        assert_eq!(alloc_commands(&fake).len(), 1);
        // The profile arrives; the palette command takes the same switch.
        fake.snaps.send(with_profile(11_000, 12_000)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(crate::actions::RefreshAllocationProfile);
        cx.run_until_parked();
        assert_eq!(alloc_commands(&fake).last().unwrap(), "alloc_profile true");
        fake.snaps.send(with_profile(13_000, 14_000)).unwrap();
        cx.run_until_parked();
        tab.update(cx, |t, cx| t.set_live_only(false, cx));
        cx.dispatch_action(crate::actions::RefreshAllocationProfile);
        cx.run_until_parked();
        assert_eq!(alloc_commands(&fake).last().unwrap(), "alloc_profile false");
    }

    #[gpui_kit::test]
    fn the_live_only_note_is_visible_beside_the_switch(cx: &mut TestAppContext) {
        let (_shell, cx, _fake) = connect(cx, with_profile(9_000, 10_000));
        show_classes(cx);
        let label = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find("classes-live-only-note".to_string())
                .and_then(|e| e.label().map(str::to_string))
        });
        assert_eq!(label.as_deref(), Some(LIVE_ONLY_NOTE));
    }

    #[gpui_kit::test]
    fn opening_never_forces_a_gc(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, with_profile(1_000, 10_000));
        let tab = classes(&shell, cx);
        tab.update(cx, |t, cx| t.set_live_only(true, cx));
        show_classes(cx);
        assert_eq!(alloc_commands(&fake), ["alloc_profile false"]);
    }

    #[gpui_kit::test]
    fn the_wait_for_a_profile_gives_up(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, empty_with_sample());
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (true, false, true)
        );
        cx.executor()
            .advance_clock(motion::LOADING_DELAY + Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (true, true, true)
        );
        assert!(exists(cx, "classes-loading"));
        cx.executor().advance_clock(FETCH_GIVE_UP);
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (false, false, false)
        );
        assert!(exists(cx, "memory-classes-empty"));
        // Refresh is available again, and asks again.
        assert!(!tab.read_with(cx, |t, cx| t.refresh_disabled(cx)));
        tab.update(cx, |t, cx| t.refresh(cx));
        assert_eq!(alloc_commands(&fake).len(), 2);
    }

    #[gpui_kit::test]
    fn a_new_session_leaves_no_fetch_timer(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, empty_with_sample());
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert!(tab.read_with(cx, |t, _| t.fetch_state().2));
        press(cx, "cmd-n", "ctrl-n");
        fake.snaps.send(snapshot(|_, _| {})).unwrap();
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (false, false, false)
        );
        // Nothing fires later.
        cx.executor().advance_clock(FETCH_GIVE_UP * 2);
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (false, false, false)
        );
    }

    #[gpui_kit::test]
    fn a_disconnect_ends_the_fetch_and_a_reconnect_fetches_again(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, empty_with_sample());
        show_classes(cx);
        let tab = classes(&shell, cx);
        cx.executor()
            .advance_clock(motion::LOADING_DELAY + Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (true, true, true)
        );
        fake.status.send(fdt_capture::ConnStatus::Idle).unwrap();
        cx.run_until_parked();
        assert_eq!(
            tab.read_with(cx, |t, _| t.fetch_state()),
            (false, false, false)
        );
        assert!(!exists(cx, "classes-loading"));
        fake.status.send(connected("demo", false)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowLeaksTab);
        cx.run_until_parked();
        show_classes(cx);
        assert_eq!(
            alloc_commands(&fake).len(),
            2,
            "no profile exists, so opening asks again"
        );
    }

    fn row_names(tab: &Entity<ClassesTab>, cx: &mut VisualTestContext) -> Vec<String> {
        tab.read_with(cx, |t, cx| {
            t.cells(cx).into_iter().map(|r| r[0].clone()).collect()
        })
    }

    fn two_class_snapshot() -> Arc<Snapshot> {
        snapshot(|w, run| {
            let a =
                w.intern_alloc_class(run, "i/1", "c/1", "Apple", Some("package:app/a.dart"), None);
            let b = w.intern_alloc_class(
                run,
                "i/1",
                "c/2",
                "Banana",
                Some("package:app/a.dart"),
                None,
            );
            profile(w, run, 1_000, &[row(a, 1, 16), row(b, 1, 8)]);
            profile(w, run, 9_000, &[row(a, 5, 80), row(b, 2, 16)]);
            w.push_memory_sample(
                run,
                MemorySample {
                    time: SessionTime(10_000 * MS),
                    heap_used: 1,
                    heap_capacity: 2,
                    external: 0,
                    rss: None,
                },
            );
        })
    }

    #[gpui_kit::test]
    fn only_the_newest_filter_shows(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_class_snapshot());
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert_eq!(row_names(&tab, cx).len(), 2);
        // Filter A, then B, before A's job ran.
        tab.update(cx, |t, cx| {
            t.set_filter("apple", cx);
            t.set_filter("banana", cx);
        });
        cx.run_until_parked();
        assert_eq!(row_names(&tab, cx), ["Banana"]);
        // A result for the earlier filter that arrives late is dropped.
        let stale = tab.read_with(cx, |t, _| t.started_key()).unwrap();
        tab.update(cx, |t, cx| t.set_filter("apple", cx));
        cx.run_until_parked();
        assert_eq!(row_names(&tab, cx), ["Apple"]);
        let late = class_rows(&two_class_snapshot().memory, None, "banana");
        tab.update(cx, |t, cx| t.finish(&stale, late, cx));
        assert_eq!(row_names(&tab, cx), ["Apple"]);
    }

    #[gpui_kit::test]
    fn a_range_or_session_change_in_flight_drops_the_older_result(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, two_class_snapshot());
        show_classes(cx);
        let tab = classes(&shell, cx);
        let range = |a, b| Some(TimeRange::new(SessionTime(a * MS), SessionTime(b * MS)));
        tab.update(cx, |t, cx| {
            t.set_range(range(0, 1_500), cx);
            t.set_range(range(0, 9_500), cx);
        });
        cx.run_until_parked();
        // The two profiles are compared, not the first one alone.
        assert_eq!(tab.read_with(cx, |t, _| t.note()), None);
        assert_eq!(tab.read_with(cx, |t, cx| t.cells(cx)[0].len()), 6);
        // A result from another session is dropped too.
        let mut other = tab.read_with(cx, |t, _| t.started_key()).unwrap();
        other.session += 1;
        let before = tab.read_with(cx, |t, cx| t.cells(cx));
        let late = class_rows(&two_class_snapshot().memory, None, "");
        tab.update(cx, |t, cx| t.finish(&other, late, cx));
        assert_eq!(tab.read_with(cx, |t, cx| t.cells(cx)), before);
    }

    #[gpui_kit::test]
    fn refresh_disabled_when_disconnected(cx: &mut TestAppContext) {
        let (shell, cx, fake) = connect(cx, with_profile(9_000, 10_000));
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert!(!tab.read_with(cx, |t, cx| t.refresh_disabled(cx)));
        fake.status.send(fdt_capture::ConnStatus::Idle).unwrap();
        cx.run_until_parked();
        assert!(tab.read_with(cx, |t, cx| t.refresh_disabled(cx)));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("classes-refresh", cx)
        });
        cx.run_until_parked();
        assert!(alloc_commands(&fake).is_empty());
        // The data stays.
        assert_eq!(tab.read_with(cx, |t, cx| t.cells(cx)).len(), 1);
    }

    #[gpui_kit::test]
    fn class_details_list_counts_over_time(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let a = w.intern_alloc_class(
                run,
                "i/1",
                "c/1",
                "Foo",
                Some("package:app/a.dart"),
                Some("a.dart:3"),
            );
            profile(w, run, 1_000, &[row(a, 1, 16)]);
            profile(w, run, 2_000, &[row(a, 4, 64)]);
            profile(w, run, 3_000, &[]);
            profile(w, run, 4_000, &[row(a, 9, 144)]);
        });
        let (shell, cx, _fake) = connect(cx, snap);
        show_classes(cx);
        let tab = classes(&shell, cx);
        assert!(tab.read_with(cx, |t, cx| t.is_settled(cx)));
        assert!(!exists(cx, "class-details"));
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::AllocClass(AllocClassId(0)));
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert!(exists(cx, "class-details"));
        assert!(exists(cx, "class-details-location"));
        assert_eq!(
            tab.read_with(cx, |t, _| t.history_points()),
            Some(vec![(1_000, 1, 16), (2_000, 4, 64), (4_000, 9, 144)])
        );
    }

    #[gpui_kit::test]
    fn another_tabs_selection_is_ignored_and_kept(cx: &mut TestAppContext) {
        let (shell, cx, _fake) = connect(cx, with_profile(9_000, 10_000));
        show_classes(cx);
        let tab = classes(&shell, cx);
        let heap = fdt_store::heap::HeapId(0);
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::Heap(heap));
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.selected_class()), None);
        assert!(!exists(cx, "class-details"));
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        assert_eq!(
            view.read_with(cx, |v, _| v.selection()),
            Some(Selection::Heap(heap))
        );
    }

    #[gpui_kit::test]
    fn show_in_snapshot_switches_tab_and_sets_the_query(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let a = w.intern_alloc_class(
                run,
                "i/1",
                "c/1",
                "Thing",
                Some("package:app/thing.dart"),
                None,
            );
            profile(w, run, 9_000, &[row(a, 1, 16)]);
            heap_record(w, run, |b| {
                let class = b.class("Thing", "package:app/thing.dart", &[]);
                let o = b.object(class, 32, fdt_analysis::heap::format::Value::None, &[]);
                b.static_field("thing", o);
            });
            w.push_memory_sample(
                run,
                MemorySample {
                    time: SessionTime(10_000 * MS),
                    heap_used: 1,
                    heap_capacity: 2,
                    external: 0,
                    rss: None,
                },
            );
        });
        let (shell, cx, _fake) = connect(cx, snap);
        show_classes(cx);
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::AllocClass(AllocClassId(0)));
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert!(exists(cx, "class-show-in-snapshot"));
        cx.update(|window, cx| window.click("class-show-in-snapshot", cx));
        cx.run_until_parked();
        let view = shell.read_with(cx, |s, _| s.memory_view().clone());
        assert_eq!(view.read_with(cx, |v, _| v.tab()), MemoryTab::Snapshot);
        // The Snapshot tab took the request: its search starts filled with the class.
        assert_eq!(
            view.read_with(cx, |v, _| v.pending_snapshot_search().cloned()),
            None
        );
        let tab = view.read_with(cx, |v, _| v.snapshot().clone());
        assert_eq!(tab.read_with(cx, |t, _| t.query().to_string()), "thing");
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::test_support::writer;
    use fdt_store::memory::{AllocProfile, AllocRow};
    use std::time::Instant;

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn class_rows_budget() {
        let (mut w, run) = writer();
        let classes: Vec<AllocClassId> = (0..6_666)
            .map(|i| {
                w.intern_alloc_class(
                    run,
                    "i/1",
                    &format!("c/{i}"),
                    &format!("Class{i}"),
                    Some(&format!("package:app/lib{}.dart", i % 40)),
                    None,
                )
            })
            .collect();
        for p in 0..200u64 {
            let rows: Vec<AllocRow> = classes
                .iter()
                .map(|&class| AllocRow {
                    class,
                    instances: p + u64::from(class.0),
                    bytes: (p + 1) * 16 * (u64::from(class.0) + 1),
                })
                .collect();
            w.push_alloc_profile(AllocProfile {
                time: SessionTime(p * 1_000_000_000),
                run,
                isolate: "i/1".into(),
                gc: false,
                heap_used: 0,
                rows: Arc::from(rows),
            });
        }
        let snap = w.publish();
        let range = Some(TimeRange::new(
            SessionTime(10_000_000_000),
            SessionTime(190_000_000_000),
        ));
        let mut worst = std::time::Duration::ZERO;
        for (range, filter) in [(None, ""), (range, ""), (range, "class6")] {
            let mut times: Vec<_> = (0..5)
                .map(|_| {
                    let at = Instant::now();
                    std::hint::black_box(class_rows(&snap.memory, range, filter));
                    at.elapsed()
                })
                .collect();
            times.sort();
            worst = worst.max(times[2]);
        }
        println!("class_rows_budget: worst p50 {worst:?}");
        fdt_store::budget::check(worst.as_micros() <= 4_000, || {
            format!("class_rows {worst:?}")
        });
    }
}
