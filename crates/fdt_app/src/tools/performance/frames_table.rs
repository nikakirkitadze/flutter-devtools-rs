//! The frames table: a `TableDelegate` over the frame seqs shown, a toolbar (Slow only,
//! Slowest first, a frame-number filter) and the view that keeps them current.
//!
//! The rows are the frames in the shared range. Which frames those are, and their order, is
//! computed in the background ([`frame_rows`]); the UI thread only draws the visible rows, each
//! cell derived from the stored record when drawn.

use crate::facet::Facet;
use crate::nav::TimeRange;
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use fdt_store::Snapshot;
use fdt_store::frames::{FrameRecord, FrameStatus, frame_status};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space, table};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    div,
};
use std::collections::HashMap;
use std::sync::Arc;

const DASH: &str = "\u{2013}";

/// The frame seqs to list: those starting in `range` (all with `None`), only the slow and janky
/// ones with `slow_only`, whose number contains `query`, in frame order or slowest first (ties
/// in frame order). O(frames in range); not for the UI thread.
pub fn frame_rows(
    snap: &Snapshot,
    range: Option<TimeRange>,
    slow_only: bool,
    slowest_first: bool,
    query: &str,
) -> Vec<u32> {
    frame_rows_from(snap, range, slow_only, slowest_first, query, 0)
}

/// [`frame_rows`] for the frames with seq `from` and later only: the tail an incremental job
/// appends to the rows it already has.
pub fn frame_rows_from(
    snap: &Snapshot,
    range: Option<TimeRange>,
    slow_only: bool,
    slowest_first: bool,
    query: &str,
    from: usize,
) -> Vec<u32> {
    let frames = &snap.frames;
    let mut window = range.map_or(0..frames.len(), |r| frames.range(r));
    window.start = window.start.max(from).min(window.end);
    let query = query.trim().trim_start_matches('#');
    let wanted = |seq: u32| {
        query.is_empty()
            || frames
                .get(seq as usize)
                .is_some_and(|f| f.number.to_string().contains(query))
    };
    let mut rows: Vec<u32> = if slow_only {
        let slow = frames.slow();
        // The first slow frame at or after the window's start.
        let (mut lo, mut hi) = (0, slow.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if slow.get(mid).is_some_and(|&s| (s as usize) < window.start) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..slow.len())
            .map_while(|i| slow.get(i).copied().filter(|&s| (s as usize) < window.end))
            .filter(|&s| wanted(s))
            .collect()
    } else {
        window.map(|s| s as u32).filter(|&s| wanted(s)).collect()
    };
    if slowest_first {
        let time = |s: u32| frames.get(s as usize).map_or(0, FrameRecord::time_us);
        rows.sort_by(|&a, &b| time(b).cmp(&time(a)).then(a.cmp(&b)));
    }
    rows
}

/// The rows a finished job hands the UI thread, ready to swap in.
struct JobResult {
    rows: Arc<[u32]>,
    /// The row of the frame that was selected when the job started.
    selected_row: Option<usize>,
    /// Seq to row, built only when the rows are not in frame order (Slowest first).
    index: Option<Arc<HashMap<u32, usize>>>,
    /// Whether the rows extend the previous ones (read by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    incremental: bool,
}

/// Rows a job may extend instead of recomputing.
struct Prev {
    rows: Arc<[u32]>,
    frames: usize,
}

#[cfg(test)]
thread_local! {
    /// Jobs started, jobs that appended, and probes made by `row_of` on the UI thread.
    pub(super) static JOBS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static INCREMENTAL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Builds the final rows off the UI thread. With `prev` (no sort, rows of the same inputs, only
/// frames added) only the new tail is filtered and appended.
fn run_job(
    snap: &Snapshot,
    range: Option<TimeRange>,
    slow_only: bool,
    slowest_first: bool,
    query: &str,
    selected: Option<usize>,
    prev: Option<Prev>,
) -> JobResult {
    let (rows, incremental): (Arc<[u32]>, bool) = match prev {
        Some(p) if !slowest_first && range.is_none() && p.frames <= snap.frames.len() => {
            let tail = frame_rows_from(snap, range, slow_only, false, query, p.frames);
            (p.rows.iter().copied().chain(tail).collect(), true)
        }
        _ => (
            frame_rows(snap, range, slow_only, slowest_first, query).into(),
            false,
        ),
    };
    let index = slowest_first.then(|| {
        Arc::new(
            rows.iter()
                .enumerate()
                .map(|(row, &seq)| (seq, row))
                .collect::<HashMap<_, _>>(),
        )
    });
    let selected_row = selected.and_then(|s| lookup(&rows, index.as_deref(), s));
    JobResult {
        rows,
        selected_row,
        index,
        incremental,
    }
}

/// The row of frame `seq`: a hash lookup when the rows are sorted by time, else a binary search
/// (they are in frame order).
fn lookup(rows: &[u32], index: Option<&HashMap<u32, usize>>, seq: usize) -> Option<usize> {
    let seq = u32::try_from(seq).ok()?;
    match index {
        Some(map) => map.get(&seq).copied(),
        None => {
            let (mut lo, mut hi) = (0, rows.len());
            while lo < hi {
                #[cfg(test)]
                PROBES.with(|c| c.set(c.get() + 1));
                let mid = lo + (hi - lo) / 2;
                match rows[mid].cmp(&seq) {
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                    std::cmp::Ordering::Equal => return Some(mid),
                }
            }
            None
        }
    }
}

pub(super) struct FramesTable {
    pub snap: Arc<Snapshot>,
    /// The frame seqs shown, in display order.
    pub rows: Arc<[u32]>,
}

fn ms(us: u32) -> String {
    format!("{:.1} ms", f64::from(us) / 1000.0)
}

impl FramesTable {
    fn new(snap: Arc<Snapshot>) -> Self {
        Self {
            snap,
            rows: Arc::from(Vec::new()),
        }
    }

    fn record(&self, row: usize) -> Option<&FrameRecord> {
        self.snap.frames.get(*self.rows.get(row)? as usize)
    }

    fn status(&self, f: &FrameRecord) -> FrameStatus {
        frame_status(f.time_us(), self.snap.latest_budget_us())
    }

    /// The text of a cell.
    fn text(&self, row: usize, col: usize) -> String {
        let Some(f) = self.record(row) else {
            return String::new();
        };
        let phase = |v: Option<u32>| v.map_or_else(|| DASH.to_string(), ms);
        match col {
            0 => format!("#{}", f.number),
            1 => format_offset(f.start),
            2 => ms(f.time_us()),
            3 => ms(f.build_us),
            4 => ms(f.raster_us),
            5 => phase(f.ui.as_ref().map(|u| u.layout_us)),
            6 => phase(f.ui.as_ref().map(|u| u.paint_us)),
            7 => {
                f.ui.as_ref()
                    .and_then(|u| u.widgets_built)
                    .map_or_else(|| DASH.to_string(), |n| n.to_string())
            }
            _ => match self.status(f) {
                FrameStatus::Ok => "OK",
                FrameStatus::Slow => "Slow",
                FrameStatus::Janky => "Janky",
            }
            .to_string(),
        }
    }
}

impl TableDelegate for FramesTable {
    fn columns_count(&self, _: &App) -> usize {
        9
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let num = |id, label, w| Column::new(id, label).width(w).text_right();
        match col_ix {
            0 => Column::new("frame", "Frame").width(table::COL_FRAME),
            1 => Column::new("time", "Time").width(table::COL_TIME),
            2 => num("total", "Total", table::COL_MS),
            3 => num("build", "Build", table::COL_MS),
            4 => num("raster", "Raster", table::COL_MS),
            5 => num("layout", "Layout", table::COL_MS),
            6 => num("paint", "Paint", table::COL_MS),
            7 => num("widgets", "Widgets", table::COL_COUNT),
            _ => Column::new("status", "Status").width(table::COL_STATUS),
        }
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
        let cell = div()
            .w_full()
            .truncate()
            .text_size(font::DENSE)
            .font_features(font::tabular_figures());
        let text = self.text(row_ix, col_ix);
        let status = self.record(row_ix).map(|f| self.status(f));
        let colour = match (col_ix, status) {
            (8, Some(FrameStatus::Ok)) => Hsla::from(t.success),
            (8 | 2, Some(FrameStatus::Slow)) => Hsla::from(t.warning),
            (8 | 2, Some(FrameStatus::Janky)) => Hsla::from(t.danger),
            (0, _) => Hsla::from(t.text),
            _ => Hsla::from(t.text_muted),
        };
        cell.text_color(colour).child(text)
    }
}

/// What the list tells the Performance page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListEvent {
    /// A row was picked: by a click, or (`by_key`) by J, K or an arrow.
    Selected { seq: usize, by_key: bool },
    /// Enter, or a double click: open the frame.
    Open(usize),
}

/// What a row job was computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    session: u64,
    frames: usize,
    version: u64,
}

pub struct FramesList {
    session: Entity<SessionModel>,
    table: Entity<TableState<FramesTable>>,
    input: Entity<InputState>,
    toolbar: Entity<Facet<FramesList>>,
    slow_only: bool,
    slowest_first: bool,
    range: Option<TimeRange>,
    query: SharedString,
    /// Bumped by every change of what the rows depend on besides the frames.
    version: u64,
    applied: Option<Key>,
    running: bool,
    task: Option<Task<()>>,
    rows: Arc<[u32]>,
    /// Seq to row while the rows are sorted by time.
    index: Option<Arc<HashMap<u32, usize>>>,
    /// Whether the Frames tab is on screen. Nothing is recomputed while it is not; the list goes
    /// stale and catches up when it is shown.
    active: bool,
    /// The selected frame's seq, whether or not it is in the rows.
    selected: Option<usize>,
    _table_sub: Subscription,
    _input_sub: Subscription,
    _session: Subscription,
}

impl EventEmitter<ListEvent> for FramesList {}

impl FramesList {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snap = session.read(cx).snapshot().clone();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by frame number"));
        let input_sub = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.query_changed(cx),
                InputEvent::PressEnter { .. } => this.focus_table(window, cx),
                _ => {}
            },
        );
        let table = cx.new(|cx| {
            TableState::new(FramesTable::new(snap), window, cx)
                .loop_selection(false)
                .col_movable(false)
                .sortable(false)
        });
        let table_sub = cx.subscribe_in(
            &table,
            window,
            |this, _, event: &TableEvent, window, cx| match event {
                TableEvent::SelectRow(row) => {
                    let by_key = window.last_input_was_keyboard();
                    // A click puts the keys (J, K, Enter) on the table. A row the location
                    // selected is not a click.
                    if this.row_selected(*row, by_key, cx) && !by_key {
                        this.focus_table(window, cx);
                    }
                }
                TableEvent::DoubleClickedRow(row) => {
                    if let Some(&seq) = this.rows.get(*row) {
                        cx.emit(ListEvent::Open(seq as usize));
                    }
                }
                _ => {}
            },
        );
        let observed = cx.observe(&session, |this, _, cx| this.request(cx));
        let owner = cx.entity();
        let toolbar = cx.new(|fcx| {
            let mut facet = Facet::new(&owner, toolbar_build, toolbar_key, fcx);
            facet.also_observe(&input, fcx);
            facet
        });
        let mut list = Self {
            session,
            table,
            input,
            toolbar,
            slow_only: false,
            slowest_first: false,
            range: None,
            query: SharedString::default(),
            version: 0,
            applied: None,
            running: false,
            task: None,
            rows: Arc::from(Vec::new()),
            index: None,
            active: false,
            selected: None,
            _table_sub: table_sub,
            _input_sub: input_sub,
            _session: observed,
        };
        list.request(cx);
        list
    }

    pub fn rows(&self) -> &[u32] {
        &self.rows
    }

    /// Whether the rows are for the current inputs and nothing is being computed.
    pub fn is_settled(&self, cx: &App) -> bool {
        !self.running && self.applied == Some(self.key(cx))
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }

    fn focus_table(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn key(&self, cx: &App) -> Key {
        let snap = self.session.read(cx).snapshot();
        Key {
            session: snap.session,
            frames: snap.frames.len(),
            version: self.version,
        }
    }

    // ---- rows job ----

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.version += 1;
        self.request(cx);
        cx.notify();
    }

    /// Whether the Frames tab is showing. Hidden, the list does no work; shown again, it catches
    /// up in one job.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if active {
                self.request(cx);
            }
        }
    }

    /// Starts a job for the newest snapshot and inputs unless one is current or running (or the
    /// tab is hidden); a running job starts the next one when it finishes.
    fn request(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let key = self.key(cx);
        if self.running || self.applied == Some(key) {
            return;
        }
        self.running = true;
        #[cfg(test)]
        JOBS.with(|c| c.set(c.get() + 1));
        let snap = self.session.read(cx).snapshot().clone();
        let (range, slow, slowest) = (self.range, self.slow_only, self.slowest_first);
        let query = self.query.to_string();
        let selected = self.selected;
        // Only the frames changed since the rows were built: append instead of recomputing.
        let prev = self
            .applied
            .filter(|a| a.session == key.session && a.version == key.version)
            .map(|a| Prev {
                rows: self.rows.clone(),
                frames: a.frames,
            });
        self.task = Some(cx.spawn(async move |this, cx| {
            let job = snap.clone();
            let result = cx
                .background_spawn(async move {
                    run_job(&job, range, slow, slowest, &query, selected, prev)
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                this.apply(key, snap, selected, result, cx);
                this.request(cx);
            })
            .ok();
        }));
    }

    /// Swaps a finished job's rows in. No work here grows with the number of rows.
    fn apply(
        &mut self,
        key: Key,
        snap: Arc<Snapshot>,
        job_selected: Option<usize>,
        result: JobResult,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        if result.incremental {
            INCREMENTAL.with(|c| c.set(c.get() + 1));
        }
        self.applied = Some(key);
        self.rows = result.rows.clone();
        self.index = result.index;
        let rows = result.rows;
        self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.snap = snap;
            d.rows = rows;
            cx.notify();
        });
        // The job looked the selection up; if it changed since, look it up again (cheap).
        let row = if self.selected == job_selected {
            result.selected_row
        } else {
            self.row_of(self.selected)
        };
        self.show_row(row, cx);
        cx.notify();
    }

    fn row_of(&self, seq: Option<usize>) -> Option<usize> {
        lookup(&self.rows, self.index.as_deref(), seq?)
    }

    /// Shows the selected frame's row, if it is listed.
    fn sync_table(&mut self, cx: &mut Context<Self>) {
        let row = self.row_of(self.selected);
        self.show_row(row, cx);
    }

    fn show_row(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        match row {
            Some(row) => {
                if self.table.read(cx).selected_row() != Some(row) {
                    self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
                }
            }
            None => {
                if self.table.read(cx).selected_row().is_some() {
                    self.table.update(cx, |t, cx| t.clear_selection(cx));
                }
            }
        }
    }

    // ---- inputs ----

    pub fn set_slow_only(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.slow_only != on {
            self.slow_only = on;
            self.bump(cx);
        }
    }

    pub fn set_slowest_first(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.slowest_first != on {
            self.slowest_first = on;
            self.bump(cx);
        }
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value();
        if value != self.query {
            self.query = value;
            self.bump(cx);
        }
    }

    /// Sets the filter field, as typing would.
    pub fn set_filter_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
        self.query_changed(cx);
    }

    /// Focuses the filter field.
    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The shared range, from the location.
    pub fn set_range(&mut self, range: Option<TimeRange>, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.bump(cx);
        }
    }

    /// The selected frame, from the location.
    pub fn set_selection(&mut self, seq: Option<usize>, cx: &mut Context<Self>) {
        if self.selected != seq {
            self.selected = seq;
            if self.is_settled(cx) {
                self.sync_table(cx);
            }
            cx.notify();
        }
    }

    /// The table picked a row.
    /// Returns whether the selection changed.
    fn row_selected(&mut self, row: usize, by_key: bool, cx: &mut Context<Self>) -> bool {
        let Some(&seq) = self.rows.get(row) else {
            return false;
        };
        let seq = seq as usize;
        if self.selected == Some(seq) {
            return false;
        }
        self.selected = Some(seq);
        cx.emit(ListEvent::Selected { seq, by_key });
        cx.notify();
        true
    }

    /// Enter: opens the selected frame.
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(seq) = self.selected {
            cx.emit(ListEvent::Open(seq));
        }
    }

    /// Scrolls `row` to the top of the table.
    pub fn scroll_to_row(&self, row: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.vertical_scroll_handle
                .scroll_to_item_strict(row, ScrollStrategy::Top);
            cx.notify();
        });
    }

    #[cfg(test)]
    pub fn shares_rows_with_table(&self, cx: &App) -> bool {
        Arc::ptr_eq(&self.rows, &self.table.read(cx).delegate().rows)
    }

    #[cfg(test)]
    pub fn column_count(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().columns_count(cx)
    }

    #[cfg(test)]
    pub fn cell_text(&self, row: usize, col: usize, cx: &App) -> String {
        self.table.read(cx).delegate().text(row, col)
    }

    #[cfg(test)]
    pub fn table_selected_row(&self, cx: &App) -> Option<usize> {
        self.table.read(cx).selected_row()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let t = *cx.tokens();
        let (accent, off) = (Hsla::from(t.accent), Hsla::from(t.text_faint));
        let filter = div()
            .id("frames-filter")
            .test_support()
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.focus_table(window, cx)))
            .flex_none()
            .w(layout::SEARCH_WIDTH)
            .min_w(layout::SEARCH_MIN)
            .child(Input::new(&self.input).cleanable(true));
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .w_full()
            .h(layout::TOOLBAR_HEIGHT)
            .px(space::S16)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(filter)
            .child(chip(
                "frames-slow-only",
                "Slow only",
                self.slow_only,
                accent,
                off,
                cx.listener(|this, _, _, cx| this.set_slow_only(!this.slow_only, cx)),
            ))
            .child(chip(
                "frames-slowest-first",
                "Slowest first",
                self.slowest_first,
                accent,
                off,
                cx.listener(|this, _, _, cx| this.set_slowest_first(!this.slowest_first, cx)),
            ))
            .into_any_element()
    }
}

fn toolbar_build(
    view: &mut FramesList,
    _: &mut Window,
    cx: &mut Context<FramesList>,
) -> gpui_kit::AnyElement {
    view.render_toolbar(cx)
}

fn toolbar_key(view: &FramesList, cx: &App) -> String {
    format!(
        "{:?}",
        (cx.appearance(), view.slow_only, view.slowest_first)
    )
}

fn toolbar_style() -> gpui_kit::StyleRefinement {
    gpui_kit::StyleRefinement::default()
        .w_full()
        .h(layout::TOOLBAR_HEIGHT)
        .flex_none()
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

impl Render for FramesList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.tokens();
        let body = if self.applied.is_none() {
            div().flex_1().into_any_element()
        } else if self.rows.is_empty() {
            let label = "No frames match your filters.";
            div()
                .id("frames-empty")
                .test_support()
                .aria_label(label)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child(label)
                .into_any_element()
        } else {
            div()
                .id("frames-table")
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.toolbar.clone().cached(toolbar_style()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowPerformance;
    use crate::nav::{Location, Page, Selection, TimeRange, Tool};
    use crate::test_support::open_shell_snapshot;
    use crate::tools::performance::fixtures::frames;
    use fdt_store::time::SessionTime;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    fn open(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<FramesList>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, frames(n));
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
        let list = shell.read_with(cx, |s, cx| s.performance_view().read(cx).list().clone());
        cx.run_until_parked();
        (shell, list, cx)
    }

    fn rows(list: &gpui_kit::Entity<FramesList>, cx: &mut VisualTestContext) -> Vec<u32> {
        list.read_with(cx, |l, _| l.rows().to_vec())
    }

    fn slow_seqs(n: usize) -> Vec<u32> {
        (0..n as u32)
            .filter(|&i| i % 25 == 10 || i % 7 == 3)
            .collect()
    }

    #[gpui_kit::test]
    fn table_follows_range_and_slow_only(cx: &mut TestAppContext) {
        let (shell, list, cx) = open(cx, 60);
        assert_eq!(rows(&list, cx), (0..60).collect::<Vec<u32>>());
        assert_eq!(list.read_with(cx, |l, cx| l.column_count(cx)), 9);
        // Frames 10..=19 start at 300..=480 ms.
        let to = Location {
            page: Page::Tool(Tool::Performance),
            selection: None,
            range: Some(TimeRange::new(
                SessionTime(300 * 1_000_000),
                SessionTime(480 * 1_000_000),
            )),
        };
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert_eq!(rows(&list, cx), (10..20).collect::<Vec<u32>>());
        list.update_in(cx, |l, _, cx| l.set_slow_only(true, cx));
        cx.run_until_parked();
        let expected: Vec<u32> = slow_seqs(60)
            .into_iter()
            .filter(|s| (10..20).contains(s))
            .collect();
        assert_eq!(rows(&list, cx), expected);
        assert!(!expected.is_empty());
        // Cells: frame number and status text.
        let first = list.read_with(cx, |l, cx| (l.cell_text(0, 0, cx), l.cell_text(0, 8, cx)));
        assert_eq!(first.0, format!("#{}", expected[0]));
        assert!(matches!(first.1.as_str(), "Slow" | "Janky"), "{first:?}");
    }

    #[gpui_kit::test]
    fn filter_by_frame_number(cx: &mut TestAppContext) {
        let (_shell, list, cx) = open(cx, 120);
        list.update_in(cx, |l, w, cx| l.set_filter_text("1", w, cx));
        cx.run_until_parked();
        let want: Vec<u32> = (0..120u32)
            .filter(|n| n.to_string().contains('1'))
            .collect();
        assert_eq!(rows(&list, cx), want);
        list.update_in(cx, |l, w, cx| l.set_filter_text("11", w, cx));
        cx.run_until_parked();
        assert_eq!(
            rows(&list, cx),
            vec![11, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119]
        );
        list.update_in(cx, |l, w, cx| l.set_filter_text("", w, cx));
        cx.run_until_parked();
        assert_eq!(rows(&list, cx).len(), 120);
    }

    #[test]
    fn slowest_first_orders_by_time_us() {
        let snap = frames(60);
        let rows = frame_rows(&snap, None, false, true, "");
        assert_eq!(rows.len(), 60);
        let times: Vec<u32> = rows
            .iter()
            .map(|&s| snap.frames.get(s as usize).unwrap().time_us())
            .collect();
        assert!(times.windows(2).all(|w| w[0] >= w[1]), "{times:?}");
        assert_eq!(rows[0], 10, "the 40 ms frame comes first");
        // Ties keep frame order, and the other filters still apply.
        let slow = frame_rows(&snap, None, true, true, "");
        assert_eq!(slow.len(), slow_seqs(60).len());
        assert_eq!(slow[0], 10);
    }

    #[gpui_kit::test]
    fn slowest_first_toggle_reorders_the_table(cx: &mut TestAppContext) {
        let (_shell, list, cx) = open(cx, 60);
        list.update_in(cx, |l, _, cx| l.set_slowest_first(true, cx));
        cx.run_until_parked();
        assert_eq!(rows(&list, cx)[0], 10);
    }

    #[gpui_kit::test]
    fn table_selection_syncs_with_location(cx: &mut TestAppContext) {
        let (shell, list, cx) = open(cx, 60);
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::Frame(frame_id(&shell, cx, 7)));
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert_eq!(
            list.read_with(cx, |l, cx| l.table_selected_row(cx)),
            Some(7)
        );
    }

    fn frame_id(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
        seq: usize,
    ) -> fdt_store::time::EntryId {
        shell.read_with(cx, |s, cx| {
            s.performance_view()
                .read(cx)
                .snapshot_frame(seq, cx)
                .expect("frame")
                .id
        })
    }

    #[gpui_kit::test]
    fn j_and_k_replace_the_location_and_enter_keeps_the_frame(cx: &mut TestAppContext) {
        let (shell, list, cx) = open(cx, 60);
        for _ in 0..3 {
            cx.update(|window, cx| window.render_frame(cx));
            cx.run_until_parked();
        }
        cx.update(|window, cx| window.click(("row", 2usize), cx));
        cx.run_until_parked();
        let seq = |shell: &gpui_kit::Entity<crate::shell::MainWindow>,
                   cx: &mut VisualTestContext| {
            shell.read_with(cx, |s, _| match s.current().selection {
                Some(Selection::Frame(id)) => Some(id.seq),
                _ => None,
            })
        };
        assert_eq!(seq(&shell, cx), Some(2));
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        cx.simulate_keystrokes("j j k");
        cx.run_until_parked();
        assert_eq!(seq(&shell, cx), Some(3));
        assert_eq!(
            list.read_with(cx, |l, cx| l.table_selected_row(cx)),
            Some(3)
        );
        assert_eq!(
            shell.read_with(cx, |s, _| s.history().back_len()),
            back,
            "J and K add no Back step"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(seq(&shell, cx), Some(3));
        // The location already is this frame, so opening it adds no step.
        assert_eq!(shell.read_with(cx, |s, _| s.history().back_len()), back);
    }

    #[test]
    fn appended_frames_filter_incrementally() {
        use crate::tools::performance::fixtures::push_frames;
        for (slow, query) in [(false, ""), (true, ""), (false, "1"), (true, "2")] {
            let (mut w, run) = crate::test_support::writer();
            push_frames(&mut w, run, 0, 50);
            let first = w.publish();
            let r1 = run_job(&first, None, slow, false, query, None, None);
            assert!(!r1.incremental);
            push_frames(&mut w, run, 50, 130);
            let second = w.publish();
            let prev = Prev {
                rows: r1.rows.clone(),
                frames: 50,
            };
            let inc = run_job(&second, None, slow, false, query, None, Some(prev));
            let full = run_job(&second, None, slow, false, query, None, None);
            assert!(inc.incremental && !full.incremental);
            assert_eq!(inc.rows, full.rows, "slow {slow} query {query:?}");
        }
        // A sort or a range end cannot append.
        let (mut w, run) = crate::test_support::writer();
        push_frames(&mut w, run, 0, 20);
        let snap = w.publish();
        let r = run_job(&snap, None, false, true, "", None, None);
        let prev = Prev {
            rows: r.rows,
            frames: 20,
        };
        assert!(!run_job(&snap, None, false, true, "", None, Some(prev)).incremental);
    }

    #[gpui_kit::test]
    fn the_list_appends_new_frames_incrementally(cx: &mut TestAppContext) {
        use crate::tools::performance::fixtures::push_frames;
        let (mut w, run) = crate::test_support::writer();
        push_frames(&mut w, run, 0, 40);
        let (shell, cx, fake) = crate::test_support::open_shell_fake(cx);
        fake.snaps.send(w.publish()).unwrap();
        fake.status
            .send(crate::test_support::connected("demo", true))
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        let list = shell.read_with(cx, |s, cx| s.performance_view().read(cx).list().clone());
        assert_eq!(rows(&list, cx).len(), 40);
        INCREMENTAL.with(|c| c.set(0));
        push_frames(&mut w, run, 40, 70);
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(rows(&list, cx), (0..70).collect::<Vec<u32>>());
        assert_eq!(INCREMENTAL.with(|c| c.get()), 1);
    }

    #[gpui_kit::test]
    fn hidden_tab_does_not_recompute(cx: &mut TestAppContext) {
        let (shell, list, cx) = open(cx, 40);
        let perf = shell.read_with(cx, |s, _| s.performance_view().clone());
        perf.update(cx, |p, cx| {
            p.set_tab(crate::tools::performance::PerfTab::Cpu, cx)
        });
        let jobs = JOBS.with(|c| c.get());
        list.update_in(cx, |l, _, cx| l.set_slow_only(true, cx));
        list.update_in(cx, |l, w, cx| l.set_filter_text("1", w, cx));
        cx.run_until_parked();
        assert_eq!(
            JOBS.with(|c| c.get()),
            jobs,
            "no job while the tab is hidden"
        );
        assert_eq!(rows(&list, cx).len(), 40, "the old rows stay");
        perf.update(cx, |p, cx| {
            p.set_tab(crate::tools::performance::PerfTab::Frames, cx)
        });
        cx.run_until_parked();
        assert_eq!(JOBS.with(|c| c.get()), jobs + 1, "one job catches up");
        assert!(rows(&list, cx).len() < 40);
        assert!(list.read_with(cx, |l, cx| l.is_settled(cx)));
    }

    #[gpui_kit::test]
    fn apply_does_not_scan_rows(cx: &mut TestAppContext) {
        let (shell, list, cx) = open(cx, 5_000);
        assert!(list.read_with(cx, |l, cx| l.shares_rows_with_table(cx)));
        PROBES.with(|c| c.set(0));
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::Frame(frame_id(&shell, cx, 4_321)));
        shell.update_in(cx, |s, w, cx| s.navigate(to.clone(), w, cx));
        cx.run_until_parked();
        // The job found the row; a later selection costs one binary search.
        let mut to2 = to;
        to2.selection = Some(Selection::Frame(frame_id(&shell, cx, 17)));
        shell.update_in(cx, |s, w, cx| s.navigate(to2, w, cx));
        cx.run_until_parked();
        assert_eq!(
            list.read_with(cx, |l, cx| l.table_selected_row(cx)),
            Some(17)
        );
        let probes = PROBES.with(|c| c.get());
        assert!(probes <= 32, "{probes} probes for 5000 rows");
        list.update_in(cx, |l, _, cx| l.set_slowest_first(true, cx));
        cx.run_until_parked();
        PROBES.with(|c| c.set(0));
        let mut to3 = shell.read_with(cx, |s, _| s.current().clone());
        to3.selection = Some(Selection::Frame(frame_id(&shell, cx, 99)));
        shell.update_in(cx, |s, w, cx| s.navigate(to3, w, cx));
        cx.run_until_parked();
        assert_eq!(
            PROBES.with(|c| c.get()),
            0,
            "sorted rows use the hash index"
        );
        assert!(list.read_with(cx, |l, cx| l.shares_rows_with_table(cx)));
    }
}
