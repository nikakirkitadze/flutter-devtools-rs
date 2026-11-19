//! The Network requests table: tabs, a filtered `DataTable`, and selection that syncs with the
//! shell's location.
//!
//! Requests are updated in place (a pending one completes, an error appears), so there is no
//! incremental append: every snapshot, filter, tab or range change recomputes the filter in
//! full, off the UI thread ([`compute`]). At most one job runs; inputs that arrive meanwhile are
//! picked up when it finishes. The UI thread only maps the visible rows.

use super::details::DetailsState;
use super::table::NetTable;
use crate::nav::{EntryId, SessionTime, TimeRange};
use crate::session_model::SessionModel;
use crate::shell::strip::session_span;
use fdt_store::Snapshot;
use fdt_store::net::StatusClass;
use fdt_store::net_query::{
    ClassSet, MethodSet, NetFilter, NetTab, count_requests, filter_requests,
};
use fdt_ui::tokens::{motion, row};
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, TestSupportExt as _, Window, div,
};
use std::sync::Arc;

/// What the Network table tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkEvent {
    /// The selection changed by a click (`None`: cleared). The shell pushes a history entry.
    Selected(Option<EntryId>),
    /// The selection moved by keyboard (or was dropped by a filter). The shell replaces the
    /// current history entry instead of pushing.
    Moved(Option<EntryId>),
    /// "Clear filters" also asks for the shared time range to go.
    ClearRange,
}

pub(super) const METHOD_CHIPS: [(&str, &str); 6] = [
    ("get", "GET"),
    ("post", "POST"),
    ("put", "PUT"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
    ("other", "OTHER"),
];

pub(super) const STATUS_CHIPS: [(&str, &str, StatusClass); 6] = [
    ("2xx", "2xx", StatusClass::Success),
    ("3xx", "3xx", StatusClass::Redirect),
    ("4xx", "4xx", StatusClass::ClientError),
    ("5xx", "5xx", StatusClass::ServerError),
    ("failed", "Failed", StatusClass::Failed),
    ("pending", "Pending", StatusClass::Pending),
];

const TABS: [NetTab; 3] = [NetTab::Requests, NetTab::WebSockets, NetTab::Errors];

/// What a filter job was computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    generation: u64,
    /// Bumped on every filter, tab or range change.
    version: u64,
}

struct JobResult {
    filtered: Arc<[u32]>,
    /// Whether any listed request is still pending (its waterfall bar grows).
    has_pending: bool,
    /// Requests matching every filter but with the Errors tab, for the tab's count.
    errors: usize,
    tab: NetTab,
}

/// Filters `snap` and counts its errors. O(records), parallel; runs off the UI thread.
fn compute(snap: &Snapshot, filter: &NetFilter) -> JobResult {
    let filtered: Arc<[u32]> = filter_requests(&snap.net, filter).into();
    let errors = count_requests(
        &snap.net,
        &NetFilter {
            tab: NetTab::Errors,
            ..filter.clone()
        },
    );
    let has_pending = filtered
        .iter()
        .any(|&seq| snap.net.request(seq as usize).status_class() == StatusClass::Pending);
    JobResult {
        filtered,
        has_pending,
        errors,
        tab: filter.tab,
    }
}

pub struct NetworkView {
    pub(super) session: Entity<SessionModel>,
    pub(super) filter: NetFilter,
    version: u64,
    /// The snapshot `filtered` refers to.
    pub(super) snap: Arc<Snapshot>,
    pub(super) filtered: Arc<[u32]>,
    pub(super) errors: usize,
    /// Whether a listed request is pending.
    has_pending: bool,
    pub(super) table: Entity<TableState<NetTable>>,
    /// The sequence number of the selected request.
    pub(super) selected: Option<u32>,
    /// The details pane and what it caches.
    pub(super) details: DetailsState,
    /// A selection from the location, waiting for the filter result it depends on.
    pending: Option<EntryId>,
    applied: Option<Key>,
    /// Whether the Network page is on screen; the filter only runs then.
    active: bool,
    running: bool,
    task: Option<Task<()>>,
    /// Set after `LOADING_DELAY` of a running job.
    pub(super) show_skeleton: bool,
    skeleton_task: Option<Task<()>>,
    /// Focus the table the next time it is drawn.
    focus_wanted: bool,
    pub(super) input: Entity<InputState>,
    /// The text of the field the filter was last started for.
    query: SharedString,
    /// The recording clock's whole seconds at the last redraw, so pending bars grow.
    tick: Option<u64>,
    _table_sub: Subscription,
    _input_sub: Subscription,
    _session: Subscription,
}

impl EventEmitter<NetworkEvent> for NetworkView {}

impl NetworkView {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observed = cx.observe(&session, |this, _, cx| {
            this.clock_ticked(cx);
            this.details_session_changed(cx);
            this.request(cx);
        });
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by URL"));
        let input_sub = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.query_changed(cx),
                InputEvent::PressEnter { .. } => this.focus_table(window, cx),
                _ => {}
            },
        );
        let snap = session.read(cx).snapshot().clone();
        let table = cx.new(|cx| {
            TableState::new(NetTable::new(snap.clone()), window, cx)
                .loop_selection(false)
                .col_movable(false)
                .sortable(false)
        });
        let table_sub = cx.subscribe_in(
            &table,
            window,
            |this, _, event: &TableEvent, window, cx| match event {
                TableEvent::SelectRow(row) => {
                    this.row_selected(*row, window.last_input_was_keyboard(), cx);
                }
                TableEvent::ClearSelection => this.selection_cleared(cx),
                TableEvent::DoubleClickedRow(_) => this.open_details(cx),
                _ => {}
            },
        );
        Self {
            session,
            filter: NetFilter::default(),
            version: 0,
            snap,
            filtered: Arc::from(Vec::new()),
            errors: 0,
            has_pending: false,
            table,
            selected: None,
            details: DetailsState::new(),
            pending: None,
            applied: None,
            active: false,
            running: false,
            task: None,
            show_skeleton: false,
            skeleton_task: None,
            focus_wanted: false,
            input,
            query: SharedString::default(),
            tick: None,
            _table_sub: table_sub,
            _input_sub: input_sub,
            _session: observed,
        }
    }

    pub fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }

    fn focus_table(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Puts focus on the table when it is next drawn, so its keys work.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_wanted = true;
        cx.notify();
    }

    /// Whether the Network page is on screen. The filter only runs while it is.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if active {
                self.request(cx);
            }
        }
    }

    // ---- filter job ----

    /// Starts a filter job for the newest snapshot and filter unless one is current or running.
    fn request(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let key = Key {
            generation: self.session.read(cx).snapshot().generation,
            version: self.version,
        };
        // A running job re-reads the newest snapshot and filter when it finishes a round.
        if self.running || self.applied == Some(key) {
            return;
        }
        self.running = true;
        self.skeleton_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                this.show_skeleton = true;
                cx.notify();
            })
            .ok();
        }));
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut done: Option<Key> = None;
            loop {
                let next = this.update(cx, |this, cx| {
                    let snap = this.session.read(cx).snapshot().clone();
                    let key = Key {
                        generation: snap.generation,
                        version: this.version,
                    };
                    if !this.active || done == Some(key) || this.applied == Some(key) {
                        this.finish_job(cx);
                        return None;
                    }
                    Some((key, snap, this.filter.clone()))
                });
                let Ok(Some((key, snap, filter))) = next else {
                    break;
                };
                let job_snap = snap.clone();
                let result = cx
                    .background_spawn(async move { compute(&job_snap, &filter) })
                    .await;
                done = Some(key);
                let applied = this.update(cx, |this, cx| {
                    // A result for an older filter, tab or range is dropped. An older snapshot
                    // is not: it is at most a frame behind, and the next round replaces it.
                    if this.version == key.version {
                        this.apply(key, snap, result, cx);
                    }
                });
                if applied.is_err() {
                    break;
                }
            }
        }));
    }

    fn finish_job(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        self.skeleton_task = None;
        if self.show_skeleton {
            self.show_skeleton = false;
            cx.notify();
        }
    }

    /// Whether the table's last row is on screen (or there are no rows).
    fn at_bottom(&self, cx: &gpui_kit::App) -> bool {
        self.filtered.is_empty()
            || self.table.read(cx).visible_range().rows().end >= self.filtered.len()
    }

    /// Takes a finished job's result.
    fn apply(&mut self, key: Key, snap: Arc<Snapshot>, result: JobResult, cx: &mut Context<Self>) {
        let at_bottom = self.at_bottom(cx);
        let tab_changed = self.table.read(cx).delegate().tab != result.tab;
        self.applied = Some(key);
        self.errors = result.errors;
        self.has_pending = result.has_pending;
        self.filtered = result.filtered;
        self.snap = snap;
        let (snap, rows) = (self.snap.clone(), self.filtered.clone());
        self.table.update(cx, |table, cx| {
            table.delegate_mut().replace(snap, rows, result.tab);
            if tab_changed {
                table.refresh(cx);
            }
            cx.notify();
        });
        if let Some(id) = self.pending.take() {
            self.select_from_location(id, cx);
        } else if let Some(sel) = self.selected {
            match self.row_of(sel) {
                // The row may have moved (a filter dropped rows above it).
                Some(row) => {
                    if self.table.read(cx).selected_row() != Some(row) {
                        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
                    }
                }
                None => {
                    self.selected = None;
                    self.table.update(cx, |t, cx| t.clear_selection(cx));
                    cx.emit(NetworkEvent::Moved(None));
                }
            }
        }
        if self.selected.is_none() && !self.filtered.is_empty() {
            let last = self.filtered.len() - 1;
            if tab_changed {
                self.table.update(cx, |t, cx| t.scroll_to_row(0, cx));
            } else if at_bottom {
                self.table.update(cx, |t, cx| t.scroll_to_row(last, cx));
            }
        }
        cx.notify();
    }

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.version += 1;
        self.request(cx);
        cx.notify();
    }

    /// A redraw per recording second keeps the bars of pending requests growing.
    fn clock_ticked(&mut self, cx: &mut Context<Self>) {
        let tick = self
            .session
            .read(cx)
            .recording_elapsed()
            .map(|d| d.as_secs());
        if tick != self.tick {
            self.tick = tick;
            // Only a pending request's bar grows with the clock; otherwise nothing changes.
            if self.active && self.has_pending {
                cx.notify();
            }
        }
    }

    // ---- filters ----

    pub fn set_tab(&mut self, tab: NetTab, cx: &mut Context<Self>) {
        if self.filter.tab != tab {
            self.filter.tab = tab;
            self.bump(cx);
        }
    }

    pub(super) fn tab_clicked(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(&tab) = TABS.get(index) {
            self.set_tab(tab, cx);
        }
    }

    /// The tab strip's labels; "Errors" carries its count when there is one.
    pub(super) fn tab_labels(&self) -> [String; 3] {
        let errors = if self.errors > 0 {
            format!("Errors {}", self.errors)
        } else {
            "Errors".to_string()
        };
        ["Requests".to_string(), "WebSockets".to_string(), errors]
    }

    pub(super) fn tab_index(&self) -> usize {
        TABS.iter().position(|t| *t == self.filter.tab).unwrap_or(0)
    }

    pub(super) fn method_clicked(&mut self, method: &'static str, cx: &mut Context<Self>) {
        // From "all", a click shows only that method; after that clicks add and remove methods,
        // and removing the last one goes back to all.
        if self.filter.methods == MethodSet::ALL {
            self.filter.methods = MethodSet::only(&[method]);
        } else {
            self.filter.methods.toggle(method);
            if !METHOD_CHIPS
                .iter()
                .any(|(_, m)| self.filter.methods.contains(m))
            {
                self.filter.methods = MethodSet::ALL;
            }
        }
        self.bump(cx);
    }

    pub(super) fn status_clicked(&mut self, class: StatusClass, cx: &mut Context<Self>) {
        if self.filter.classes == ClassSet::ALL {
            for c in StatusClass::ALL {
                if c != class {
                    self.filter.classes.toggle(c);
                }
            }
        } else {
            self.filter.classes.toggle(class);
            let any = STATUS_CHIPS
                .iter()
                .any(|(_, _, c)| self.filter.classes.contains(*c));
            if !any {
                self.filter.classes = ClassSet::ALL;
            }
        }
        self.bump(cx);
    }

    pub(super) fn set_host(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        self.filter.host = host;
        self.bump(cx);
    }

    /// Whether anything but the tab narrows the list.
    pub(super) fn filters_active(&self) -> bool {
        let f = &self.filter;
        !f.text.is_empty()
            || f.methods != MethodSet::ALL
            || f.classes != ClassSet::ALL
            || f.host.is_some()
            || f.range.is_some()
    }

    /// Resets every filter, the text field and, when one is set, the shared time range.
    pub fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = NetFilter {
            tab: self.filter.tab,
            range: self.filter.range,
            ..NetFilter::default()
        };
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.query = SharedString::default();
        if self.filter.range.is_some() {
            cx.emit(NetworkEvent::ClearRange);
        }
        self.bump(cx);
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value();
        // Enter repeats the last change; only a different value is a new query.
        if value == self.query {
            return;
        }
        self.query = value.clone();
        self.filter.text = value.to_string();
        self.bump(cx);
    }

    /// Whether the filter field is on screen.
    fn has_toolbar(&self, cx: &gpui_kit::App) -> bool {
        !self.session.read(cx).snapshot().net.is_empty()
    }

    /// Focuses the filter field (⌘F).
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_toolbar(cx) {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    // ---- location ----

    /// Takes the shared range and selection from the shell's location.
    pub fn set_location(
        &mut self,
        range: Option<TimeRange>,
        selection: Option<EntryId>,
        cx: &mut Context<Self>,
    ) {
        if range != self.filter.range {
            self.filter.range = range;
            self.bump(cx);
        }
        match selection {
            None => {
                self.pending = None;
                if self.selected.take().is_some() {
                    self.table.update(cx, |t, cx| t.clear_selection(cx));
                    cx.notify();
                }
            }
            Some(id) => {
                if self.selected_id() == Some(id) {
                    self.pending = None;
                    return;
                }
                self.pending = Some(id);
                if self.settled() {
                    self.pending = None;
                    self.select_from_location(id, cx);
                }
            }
        }
    }

    /// Whether the rows on screen are for the current filter and nothing is being computed.
    fn settled(&self) -> bool {
        !self.running && self.applied.is_some_and(|k| k.version == self.version)
    }

    fn selected_id(&self) -> Option<EntryId> {
        let seq = self.selected? as usize;
        self.snap.net.get(seq).map(|r| r.id)
    }

    /// The table row showing request `seq`.
    fn row_of(&self, seq: u32) -> Option<usize> {
        self.filtered.binary_search(&seq).ok()
    }

    /// Selects and reveals `id`; clears the selection, silently, if it is not in the list.
    fn select_from_location(&mut self, id: EntryId, cx: &mut Context<Self>) {
        let seq = u32::try_from(id.seq).ok().filter(|&s| {
            self.row_of(s).is_some() && self.snap.net.get(s as usize).is_some_and(|r| r.id == id)
        });
        match seq.and_then(|s| self.row_of(s).map(|row| (s, row))) {
            Some((seq, row)) => {
                self.selected = Some(seq);
                // The table reports the selection back; it matches, so nothing more is sent.
                self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
            }
            None => {
                if self.selected.take().is_some() {
                    self.table.update(cx, |t, cx| t.clear_selection(cx));
                }
            }
        }
        cx.notify();
    }

    // ---- selection and keys ----

    /// The table selected a row: by a click, or by its own keys (`by_key`).
    fn row_selected(&mut self, row: usize, by_key: bool, cx: &mut Context<Self>) {
        let Some(&seq) = self.filtered.get(row) else {
            return;
        };
        if self.selected == Some(seq) {
            return;
        }
        self.selected = Some(seq);
        let id = self.snap.net.request(seq as usize).id;
        cx.emit(if by_key {
            NetworkEvent::Moved(Some(id))
        } else {
            NetworkEvent::Selected(Some(id))
        });
        cx.notify();
    }

    /// The table dropped its selection (Escape in the table). Like a key press, it replaces the
    /// history entry rather than pushing one.
    fn selection_cleared(&mut self, cx: &mut Context<Self>) {
        if self.selected.take().is_some() {
            cx.emit(NetworkEvent::Moved(None));
            cx.notify();
        }
    }

    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(last) = self.filtered.len().checked_sub(1) else {
            return;
        };
        let current = self.selected.and_then(|s| self.row_of(s));
        let row = match current {
            Some(i) if forward => (i + 1).min(last),
            Some(i) => i.saturating_sub(1),
            None if forward => 0,
            None => last,
        };
        let seq = self.filtered[row];
        self.selected = Some(seq);
        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
        let id = self.snap.net.request(seq as usize).id;
        cx.emit(NetworkEvent::Moved(Some(id)));
        cx.notify();
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.move_selection(true, cx);
    }

    pub fn select_previous(&mut self, cx: &mut Context<Self>) {
        self.move_selection(false, cx);
    }

    // ---- test access ----

    #[cfg(test)]
    pub fn details_open(&self) -> bool {
        self.details.open
    }

    #[cfg(test)]
    pub fn visible_rows(&self, cx: &gpui_kit::App) -> usize {
        self.table.read(cx).visible_range().rows().len()
    }

    #[cfg(test)]
    pub fn filtered_len(&self) -> usize {
        self.filtered.len()
    }

    #[cfg(test)]
    pub fn error_count(&self) -> usize {
        self.errors
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    #[cfg(test)]
    pub fn table_selected_row(&self, cx: &gpui_kit::App) -> Option<usize> {
        self.table.read(cx).selected_row()
    }

    #[cfg(test)]
    pub fn column_count(&self, cx: &gpui_kit::App) -> usize {
        use gpui_kit::component::table::TableDelegate as _;
        self.table.read(cx).delegate().columns_count(cx)
    }

    #[cfg(test)]
    pub fn cell_text(&self, row: usize, col: usize, cx: &gpui_kit::App) -> String {
        use gpui_kit::component::table::TableDelegate as _;
        self.table
            .read(cx)
            .delegate()
            .cell_label(row, col, cx)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn filter_text(&self, cx: &gpui_kit::App) -> SharedString {
        self.input.read(cx).value()
    }

    #[cfg(test)]
    pub fn filter_focused(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        gpui_kit::Focusable::focus_handle(self.input.read(cx), cx).is_focused(window)
    }

    #[cfg(test)]
    pub fn visible_end(&self, cx: &gpui_kit::App) -> usize {
        self.table.read(cx).visible_range().rows().end
    }
}

impl Render for NetworkView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The table is not on screen until a filter job has produced rows, so the request waits.
        if self.focus_wanted && !self.filtered.is_empty() {
            self.focus_wanted = false;
            // Not while the user is typing: a range drag or a status change must not pull
            // focus out of the filter field or the palette.
            if !window.context_stack().iter().any(|c| c.contains("Input")) {
                self.focus_table(window, cx);
            }
        }
        // The waterfall's axis: the whole session, with pending requests running to now.
        let session = self.session.read(cx);
        let recording_now = session.recording_elapsed().and_then(|e| {
            let run = self.snap.runs.last()?;
            Some(SessionTime(run.started.0 + e.as_nanos() as u64))
        });
        let span = session_span(&self.snap, recording_now);
        let now = recording_now.unwrap_or_else(|| self.snap.latest_time());
        self.table.update(cx, |table, _| {
            let d = table.delegate_mut();
            d.span = span;
            d.now = now;
        });
        let body = if self.applied.is_none() {
            div().flex_1().into_any_element()
        } else if self.filtered.is_empty() {
            self.render_empty(cx)
        } else {
            div()
                .id("net-list")
                .test_support()
                .flex_1()
                .min_h_0()
                .w_full()
                // The table takes Escape for itself, so details close before it sees the key.
                .capture_action(cx.listener(|this, _: &Cancel, _, cx| {
                    if this.close_details(cx) {
                        cx.stop_propagation();
                    }
                }))
                .child(
                    DataTable::new(&self.table)
                        .bordered(false)
                        .with_size(Size::Size(row::TABLE)),
                )
                .into_any_element()
        };
        let content = if self.details.open && !self.filtered.is_empty() {
            self.with_details(body, cx)
        } else {
            body
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(cx))
            .children(self.has_toolbar(cx).then(|| self.render_toolbar(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(content),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Location, Page, Selection, SessionTime, Tool};
    use crate::test_support::{
        app_info, connected, exists, open_shell_fake, open_shell_snapshot, press, request,
        snapshot, writer,
    };
    use fdt_store::net_query::{NetFilter, count_requests, filter_requests};
    use fdt_store::time::RunId;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    const MS: u64 = 1_000_000;

    fn frame(cx: &mut VisualTestContext) {
        for _ in 0..2 {
            cx.update(|window, cx| window.render_frame(cx));
        }
    }

    /// The fields of the `i`-th test request: host, method, status (`None` = pending).
    fn spec(i: usize) -> (&'static str, &'static str, Option<u16>) {
        let host = if i.is_multiple_of(2) {
            "a.com"
        } else {
            "b.com"
        };
        let method = if i % 3 == 1 { "POST" } else { "GET" };
        let status = match i {
            _ if i % 17 == 16 => None,
            _ if i % 10 == 9 => Some(500),
            _ if i % 10 == 4 => Some(404),
            _ => Some(200),
        };
        (host, method, status)
    }

    fn url(i: usize) -> String {
        format!("http://{}/item/{i}", spec(i).0)
    }

    fn feed(w: &mut fdt_store::SessionWriter, run: RunId, from: usize, to: usize) {
        for i in from..to {
            let (_, method, status) = spec(i);
            let start = i as u64 * 100;
            let end = status.map(|_| start + 50);
            request(w, run, method, &url(i), status, start, end);
        }
    }

    fn count(n: usize, f: impl Fn(usize) -> bool) -> usize {
        (0..n).filter(|&i| f(i)).count()
    }

    fn open_network(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<NetworkView>,
        &mut VisualTestContext,
    ) {
        let snap = snapshot(|w, run| feed(w, run, 0, n));
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        frame(cx);
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        (shell, view, cx)
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        frame(cx);
        cx.update(|window, cx| window.click(id, cx));
        cx.run_until_parked();
        frame(cx);
    }

    fn click_tab(cx: &mut VisualTestContext, ix: usize) {
        frame(cx);
        cx.update(|window, cx| window.within("net-tabs").click(ix, cx));
        cx.run_until_parked();
        frame(cx);
    }

    fn click_row(cx: &mut VisualTestContext, row: usize) {
        frame(cx);
        cx.update(|window, cx| window.click(("row", row), cx));
        cx.run_until_parked();
        frame(cx);
    }

    fn label(cx: &mut VisualTestContext, id: &'static str) -> Option<String> {
        frame(cx);
        cx.update(|window, _| {
            window
                .try_find(id)
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|i| i.text()))
    }

    fn filtered(view: &gpui_kit::Entity<NetworkView>, cx: &mut VisualTestContext) -> usize {
        view.read_with(cx, |v, _| v.filtered_len())
    }

    /// Cells built by one frame with `n` requests, and the rows the table shows.
    fn cells_in_a_frame(cx: &mut TestAppContext, n: usize) -> (usize, usize) {
        let snap = snapshot(|w, run| fdt_store::synth::requests(w, run, n, 7));
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        frame(cx);
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        assert_eq!(filtered(&view, cx), n);
        super::super::table::CELLS.with(|c| c.set(0));
        cx.update(|window, cx| window.render_frame(cx));
        let calls = super::super::table::CELLS.with(|c| c.get());
        let columns = view.read_with(cx, |v, cx| v.column_count(cx));
        let visible = view.read_with(cx, |v, cx| v.visible_rows(cx));
        println!("{n} requests: {calls} cells for {visible} visible rows of {columns} columns");
        (calls, visible * columns)
    }

    #[gpui_kit::test]
    fn renders_only_visible_rows_at_100k(cx: &mut TestAppContext) {
        // The table draws (visible rows + overscan) x columns cells whatever the window size,
        // and never a number that follows the 100,000 requests.
        const OVERSCAN_ROWS: usize = 4;
        let (small, small_visible) = cells_in_a_frame(cx, 1_000);
        let (large, large_visible) = cells_in_a_frame(cx, 100_000);
        assert!(small > 0 && large > 0);
        assert_eq!(
            small_visible, large_visible,
            "same window, same visible rows"
        );
        assert!(
            large <= small_visible + OVERSCAN_ROWS * 7,
            "{large} cells for {large_visible} visible cells"
        );
        assert_eq!(small, large, "1k and 100k requests build the same cells");
    }

    #[gpui_kit::test]
    fn filter_text_narrows_rows(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_network(cx, 200);
        assert_eq!(filtered(&view, cx), 200);
        press(cx, "cmd-f", "ctrl-f");
        cx.simulate_input("item/19");
        cx.run_until_parked();
        assert_eq!(
            filtered(&view, cx),
            count(200, |i| url(i).contains("item/19"))
        );
        assert_eq!(filtered(&view, cx), 11);
    }

    #[gpui_kit::test]
    fn status_and_method_chips_filter(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_network(cx, 200);
        click(cx, "net-status-5xx");
        assert_eq!(filtered(&view, cx), count(200, |i| spec(i).2 == Some(500)));
        click(cx, "net-status-5xx");
        assert_eq!(filtered(&view, cx), 200, "removing the last chip shows all");
        click(cx, "net-status-pending");
        assert_eq!(filtered(&view, cx), count(200, |i| spec(i).2.is_none()));
        click(cx, "net-status-pending");
        click(cx, "net-method-post");
        assert_eq!(filtered(&view, cx), count(200, |i| spec(i).1 == "POST"));
    }

    #[gpui_kit::test]
    fn errors_tab_shows_only_errors_and_its_count(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_network(cx, 200);
        let errors = count(200, |i| matches!(spec(i).2, Some(404 | 500)));
        assert!(errors > 0);
        assert_eq!(
            view.read_with(cx, |v, _| v.tab_labels()),
            [
                "Requests".to_string(),
                "WebSockets".to_string(),
                format!("Errors {errors}")
            ]
        );
        click_tab(cx, 2);
        assert_eq!(filtered(&view, cx), errors);
        assert_eq!(view.read_with(cx, |v, _| v.error_count()), errors);
        click_tab(cx, 0);
        assert_eq!(filtered(&view, cx), 200);
    }

    #[gpui_kit::test]
    fn errors_label_is_plain_without_errors(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            request(w, run, "GET", "http://a.com/ok", Some(200), 0, Some(5));
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        assert_eq!(view.read_with(cx, |v, _| v.tab_labels())[2], "Errors");
    }

    #[gpui_kit::test]
    fn websockets_tab_columns(cx: &mut TestAppContext) {
        use fdt_store::net::{HttpUpdate, SocketUpdate};
        let snap = snapshot(|w, run| {
            request(w, run, "GET", "http://a.com/plain", Some(200), 0, Some(5));
            for (n, (port, end)) in [(8001u16, None), (8002, Some(900u64)), (8003, None)]
                .into_iter()
                .enumerate()
            {
                let start = 100 * (n as u64 + 1);
                w.upsert_request(HttpUpdate {
                    run,
                    isolate: "i".into(),
                    vm_id: format!("ws{n}"),
                    method: "GET".into(),
                    url: format!("ws://a.com:{port}/socket"),
                    start: SessionTime(start * MS),
                    start_clamped: false,
                    request_sent: None,
                    response_start: None,
                    end: Some(SessionTime((start + 5) * MS)),
                    status: Some(101),
                    reason: None,
                    request_headers: vec![],
                    response_headers: vec![],
                    request_content_length: None,
                    response_content_length: None,
                    events: vec![],
                    error: None,
                    websocket: true,
                    remote: Some(("127.0.0.1".into(), port)),
                });
                // The third connection has no socket yet.
                if port != 8003 {
                    w.upsert_socket(SocketUpdate {
                        run,
                        isolate: "i".into(),
                        vm_id: format!("s{n}"),
                        address: "127.0.0.1".into(),
                        port,
                        kind: "tcp".into(),
                        start: SessionTime(start * MS),
                        end: end.map(|e| SessionTime(e * MS)),
                        read_bytes: 2048 * (n as u64 + 1),
                        write_bytes: 12,
                        last_read: Some(SessionTime(65_000 * MS)),
                        last_write: None,
                    });
                }
            }
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        let view = shell.read_with(cx, |v, _| v.network_view().clone());
        click_tab(cx, 1);
        assert_eq!(filtered(&view, cx), 3);
        assert_eq!(view.read_with(cx, |v, cx| v.column_count(cx)), 5);
        let text = |cx: &mut VisualTestContext, row, col| {
            view.read_with(cx, |v, cx| v.cell_text(row, col, cx))
        };
        assert_eq!(text(cx, 0, 0), "ws://a.com:8001/socket");
        assert_eq!(text(cx, 0, 1), "Open");
        assert_eq!(text(cx, 1, 1), "Closed");
        assert_eq!(text(cx, 2, 1), "Unknown");
        assert_eq!(text(cx, 0, 2), "12 B");
        assert_eq!(text(cx, 0, 3), "2.0 KB");
        assert_eq!(text(cx, 0, 4), "1:05");
        assert_eq!(text(cx, 2, 2), "\u{2013}");
        click_tab(cx, 0);
        assert_eq!(view.read_with(cx, |v, cx| v.column_count(cx)), 7);
    }

    #[gpui_kit::test]
    fn j_and_k_move_the_selection(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_network(cx, 50);
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(0));
        cx.simulate_keystrokes("j j");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(2));
        cx.simulate_keystrokes("k");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(1));
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(0));
        cx.simulate_keystrokes("k");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |v, _| v.selected()),
            Some(0),
            "selection does not wrap"
        );
        let (selection, now_back) =
            shell.read_with(cx, |s, _| (s.current().selection, s.history().back_len()));
        assert!(matches!(selection, Some(Selection::Request(id)) if id.seq == 0));
        assert_eq!(now_back, back, "keyboard movement adds no Back step");
    }

    #[gpui_kit::test]
    fn selection_round_trips_through_location(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_network(cx, 12);
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        click_row(cx, 3);
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(3));
        let (selection, now_back) =
            shell.read_with(cx, |s, _| (s.current().selection, s.history().back_len()));
        let id = match selection {
            Some(Selection::Request(id)) => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(id.seq, 3);
        assert_eq!(now_back, back + 1, "a click pushes");
        // Navigate to another request: the table follows.
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.selection = Some(Selection::Request(fdt_store::time::EntryId {
            run: id.run,
            seq: 7,
        }));
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(7));
        assert_eq!(
            view.read_with(cx, |v, cx| v.table_selected_row(cx)),
            Some(7)
        );
        // Back returns to the clicked row; clearing the location clears the table.
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(3));
        let to = Location {
            page: Page::Tool(Tool::Network),
            selection: None,
            range: None,
        };
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
        assert_eq!(view.read_with(cx, |v, cx| v.table_selected_row(cx)), None);
    }

    #[gpui_kit::test]
    fn range_filters_requests(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_network(cx, 200);
        let range = crate::nav::TimeRange::new(SessionTime(1_000 * MS), SessionTime(2_000 * MS));
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.range = Some(range);
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        let expected = {
            let snap = shell.read_with(cx, |s, cx| s.session().read(cx).snapshot().clone());
            let f = NetFilter {
                range: Some(range),
                ..NetFilter::default()
            };
            assert_eq!(
                filter_requests(&snap.net, &f).len(),
                count_requests(&snap.net, &f)
            );
            count_requests(&snap.net, &f)
        };
        assert!(expected > 0 && expected < 200);
        assert_eq!(filtered(&view, cx), expected);
    }

    #[gpui_kit::test]
    fn copy_url_from_context_menu(cx: &mut TestAppContext) {
        let (_shell, _view, cx) = open_network(cx, 20);
        frame(cx);
        cx.update(|window, cx| window.right_click(("row", 1usize), cx));
        cx.run_until_parked();
        frame(cx);
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(clipboard(cx), Some(url(1)));
        cx.update(|window, cx| window.right_click(("row", 2usize), cx));
        cx.run_until_parked();
        frame(cx);
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        let json = clipboard(cx).unwrap();
        let (_, method, status) = spec(2);
        assert_eq!(
            json,
            format!(
                r#"{{"method":"{method}","url":"{}","status":{},"duration_ms":50,"size":null,"start":"0:00.200"}}"#,
                url(2),
                status.unwrap()
            )
        );
    }

    #[gpui_kit::test]
    fn empty_states_per_tab(cx: &mut TestAppContext) {
        use super::super::model::empty_label;
        use fdt_store::net_query::NetTab;
        let (shell, cx, h) = open_shell_fake(cx);
        h.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        let _ = shell;
        assert_eq!(label(cx, "net-empty"), Some(empty_label(NetTab::Requests)));
        click_tab(cx, 1);
        assert_eq!(
            label(cx, "net-empty"),
            Some(empty_label(NetTab::WebSockets))
        );
        click_tab(cx, 2);
        assert_eq!(
            label(cx, "net-empty").as_deref(),
            Some("No failed requests.")
        );
    }

    #[gpui_kit::test]
    fn filters_that_exclude_everything_offer_to_clear(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_network(cx, 50);
        press(cx, "cmd-f", "ctrl-f");
        cx.simulate_input("zzz-nothing");
        cx.run_until_parked();
        assert_eq!(filtered(&view, cx), 0);
        assert_eq!(
            label(cx, "net-empty").as_deref(),
            Some("No requests match your filters.")
        );
        assert!(exists(cx, "net-clear-filters"));
        click(cx, "net-clear-filters");
        assert_eq!(filtered(&view, cx), 50);
        assert!(!exists(cx, "net-clear-filters"));
        assert_eq!(view.read_with(cx, |v, cx| v.filter_text(cx)), "");
    }

    #[gpui_kit::test]
    fn cmd_f_focuses_the_filter_only_on_network(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_network(cx, 20);
        assert!(!cx.update(|w, cx| view.read(cx).filter_focused(w, cx)));
        press(cx, "cmd-f", "ctrl-f");
        assert!(cx.update(|w, cx| view.read(cx).filter_focused(w, cx)));
    }

    #[gpui_kit::test]
    fn new_requests_do_not_move_the_selection_and_follow_at_the_bottom(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 100);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        frame(cx);
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        let end = |cx: &mut VisualTestContext| view.read_with(cx, |v, cx| v.visible_end(cx));
        assert_eq!(end(cx), 100, "the last row is on screen");
        feed(&mut w, run, 100, 160);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        assert_eq!(filtered(&view, cx), 160);
        assert_eq!(
            end(cx),
            160,
            "nothing selected and at the bottom: stays there"
        );
        // Select the last row: later requests leave the selection and the view alone.
        click_row(cx, 159);
        feed(&mut w, run, 160, 220);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(159));
        assert_eq!(end(cx), 160, "a selection keeps its place");
    }

    #[gpui_kit::test]
    fn a_pending_request_completes_in_place(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        request(&mut w, run, "GET", "http://a.com/slow", None, 0, None);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        assert_eq!(view.read_with(cx, |v, cx| v.cell_text(0, 3, cx)), "Pending");
        request(
            &mut w,
            run,
            "GET",
            "http://a.com/slow",
            Some(500),
            0,
            Some(400),
        );
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, cx| v.cell_text(0, 3, cx)), "500");
        assert_eq!(view.read_with(cx, |v, _| v.error_count()), 1);
        let _ = app_info;
    }

    // ---- details (Task 19) and the Task 18 carry-overs ----

    use super::super::details::{RENDER_JOBS, load_full_hint};
    use fdt_store::net::{BodyKind, BodyState, HttpUpdate};
    use fdt_store::time::EntryId;
    use std::sync::Arc as StdArc;

    fn body(bytes: &[u8], full: u64) -> BodyState {
        BodyState::Captured(fdt_store::net::Body {
            bytes: StdArc::from(bytes.to_vec()),
            full_len: full,
        })
    }

    /// Adds request `seq` (started at `seq` seconds) after `tweak` has shaped it.
    fn put(
        w: &mut fdt_store::SessionWriter,
        run: RunId,
        seq: u64,
        tweak: impl FnOnce(&mut HttpUpdate),
    ) -> EntryId {
        let mut u = HttpUpdate {
            run,
            isolate: "i".into(),
            vm_id: format!("r{seq}"),
            method: "POST".into(),
            url: format!("http://a.com/item/{seq}?x=1"),
            start: SessionTime(seq * 1_000 * MS),
            start_clamped: false,
            request_sent: None,
            response_start: None,
            end: Some(SessionTime((seq * 1_000 + 182) * MS)),
            status: Some(200),
            reason: None,
            request_headers: vec![("Content-Type".into(), vec!["application/json".into()])],
            response_headers: vec![
                ("x-b".into(), vec!["1".into(), "2".into()]),
                ("Content-Type".into(), vec!["application/json".into()]),
                ("a-first".into(), vec!["z".into()]),
            ],
            request_content_length: None,
            response_content_length: None,
            events: vec![
                (
                    "Connection established".into(),
                    SessionTime((seq * 1_000 + 20) * MS),
                ),
                ("Request sent".into(), SessionTime((seq * 1_000 + 40) * MS)),
            ],
            error: None,
            websocket: false,
            remote: None,
        };
        tweak(&mut u);
        w.upsert_request(u).id
    }

    /// Opens Network over `snap`, selects row `row` and presses Enter.
    fn open_details_over(
        cx: &mut TestAppContext,
        snap: StdArc<fdt_store::Snapshot>,
        row: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<NetworkView>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        frame(cx);
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        click_row(cx, row);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        frame(cx);
        (shell, view, cx)
    }

    fn present(cx: &mut VisualTestContext, id: &str) -> bool {
        frame(cx);
        cx.update(|window, _| window.try_find(id.to_string()).is_some())
    }

    fn text_of(cx: &mut VisualTestContext, id: &str) -> Option<String> {
        frame(cx);
        cx.update(|window, _| {
            window
                .try_find(id.to_string())
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn one_request(tweak: impl FnOnce(&mut HttpUpdate)) -> StdArc<fdt_store::Snapshot> {
        snapshot(|w, run| {
            put(w, run, 0, tweak);
        })
    }

    #[gpui_kit::test]
    fn details_open_on_selection_with_sections(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |u| {
                u.error = None;
            });
            w.set_body_state(id, BodyKind::Response, body(br#"{"a":1,"b":[true]}"#, 18));
            put(w, run, 1, |u| {
                u.status = None;
                u.end = None;
                u.error = Some("SocketException: refused".into());
            });
        });
        let (_shell, view, cx) = open_details_over(cx, snap, 0);
        assert!(view.read_with(cx, |v, _| v.details_open()));
        assert!(present(cx, "net-details"));
        assert_eq!(
            text_of(cx, "net-details-url").as_deref(),
            Some("http://a.com/item/0?x=1")
        );
        assert_eq!(text_of(cx, "net-details-method").as_deref(), Some("POST"));
        assert_eq!(text_of(cx, "net-details-status").as_deref(), Some("200"));
        assert_eq!(
            text_of(cx, "net-details-duration").as_deref(),
            Some("182 ms")
        );
        for sec in [
            "overview",
            "request-headers",
            "response-headers",
            "request-body",
            "response-body",
            "timing",
        ] {
            assert!(present(cx, &format!("net-sec-{sec}")), "{sec}");
        }
        assert!(!present(cx, "net-sec-error"), "no error, no section");
        assert_eq!(
            text_of(cx, "net-overview-start").as_deref(),
            Some("0:00.000")
        );
        assert_eq!(text_of(cx, "net-overview-run").as_deref(), Some("Run 1"));
        assert!(!present(cx, "net-before-capture"));
        // Headers are sorted by name, multi-values joined.
        assert_eq!(
            text_of(cx, "net-response-headers-0").as_deref(),
            Some("a-first: z")
        );
        assert_eq!(
            text_of(cx, "net-response-headers-1").as_deref(),
            Some("Content-Type: application/json")
        );
        assert_eq!(
            text_of(cx, "net-response-headers-2").as_deref(),
            Some("x-b: 1, 2")
        );
        // Timing offsets and the total.
        assert_eq!(
            text_of(cx, "net-timing-0").as_deref(),
            Some("Connection established: +20 ms")
        );
        assert_eq!(
            text_of(cx, "net-timing-1").as_deref(),
            Some("Request sent: +40 ms")
        );
        assert_eq!(
            text_of(cx, "net-timing-total").as_deref(),
            Some("Total: 182 ms")
        );
        // The JSON response body is pretty-printed, off the UI thread.
        cx.run_until_parked();
        assert_eq!(text_of(cx, "net-body-line-0").as_deref(), Some("{"));
        assert_eq!(
            text_of(cx, "net-body-line-1").as_deref(),
            Some("  \"a\": 1,")
        );
        assert_eq!(text_of(cx, "net-body-line-4").as_deref(), Some("  ]"));
        // Sections collapse.
        click(cx, "net-sec-response-headers");
        assert!(!present(cx, "net-response-headers-0"));
        click(cx, "net-sec-response-headers");
        assert!(present(cx, "net-response-headers-0"));
        // A failed request shows its error.
        click_row(cx, 1);
        assert!(present(cx, "net-sec-error"));
        assert_eq!(
            text_of(cx, "net-error").as_deref(),
            Some("SocketException: refused")
        );
        assert_eq!(text_of(cx, "net-details-status").as_deref(), Some("Failed"));
        assert_eq!(text_of(cx, "net-body-empty").as_deref(), Some("No body."));
        assert_eq!(
            text_of(cx, "net-timing-total").as_deref(),
            Some("Total: Pending")
        );
    }

    #[gpui_kit::test]
    fn before_capture_is_noted(cx: &mut TestAppContext) {
        let snap = one_request(|u| u.start_clamped = true);
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        assert_eq!(
            text_of(cx, "net-before-capture").as_deref(),
            Some("Started before fdt connected")
        );
    }

    #[gpui_kit::test]
    fn rendered_lines_are_cached_per_body(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            for i in 0..2 {
                let id = put(w, run, i, |_| {});
                w.set_body_state(id, BodyKind::Response, body(br#"{"i":0}"#, 7));
            }
        });
        RENDER_JOBS.with(|c| c.set(0));
        let (_s, view, cx) = open_details_over(cx, snap, 0);
        for _ in 0..3 {
            frame(cx);
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        assert_eq!(
            RENDER_JOBS.with(|c| c.get()),
            1,
            "one job, however often it redraws"
        );
        click_row(cx, 1);
        assert_eq!(RENDER_JOBS.with(|c| c.get()), 2);
    }

    #[gpui_kit::test]
    fn a_truncated_json_body_is_text_without_the_invalid_note(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, body(b"{\"a\": [1, 2", 4_000));
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        cx.run_until_parked();
        assert_eq!(
            text_of(cx, "net-body-line-0").as_deref(),
            Some("{\"a\": [1, 2")
        );
        assert!(
            !present(cx, "net-body-note"),
            "the truncation notice explains it"
        );
        assert!(present(cx, "net-body-truncated"));
    }

    #[gpui_kit::test]
    fn invalid_json_and_binary_bodies_show_notes(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, body(b"{oops}", 6));
            let id = put(w, run, 1, |u| {
                u.response_headers = vec![(
                    "content-type".into(),
                    vec!["application/octet-stream".into()],
                )];
            });
            w.set_body_state(
                id,
                BodyKind::Response,
                body(&[0x89, 0x50, 0x4e, 0x47, 0, 1], 6),
            );
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        cx.run_until_parked();
        assert_eq!(
            text_of(cx, "net-body-note").as_deref(),
            Some("Not valid JSON. Shown as text.")
        );
        assert_eq!(text_of(cx, "net-body-line-0").as_deref(), Some("{oops}"));
        click_row(cx, 1);
        cx.run_until_parked();
        assert_eq!(
            text_of(cx, "net-body-note").as_deref(),
            Some("Binary data, 6 B. Showing the first 4 KB.")
        );
        assert!(
            text_of(cx, "net-body-line-0")
                .unwrap()
                .starts_with("00000000  89 50 4e 47")
        );
    }

    #[gpui_kit::test]
    fn pending_body_shows_skeleton_after_delay(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, BodyState::Pending);
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        assert!(!present(cx, "net-body-skeleton"), "not before the delay");
        cx.executor()
            .advance_clock(fdt_ui::tokens::motion::LOADING_DELAY);
        cx.run_until_parked();
        assert!(present(cx, "net-body-skeleton"));
    }

    fn connected_with(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut fdt_store::SessionWriter, RunId),
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<NetworkView>,
        &mut VisualTestContext,
        crate::test_support::FakeHandle,
        fdt_store::SessionWriter,
        RunId,
    ) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        build(&mut w, run);
        h.status.send(connected("demo", false)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        frame(cx);
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        click_row(cx, 0);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        frame(cx);
        (shell, view, cx, h, w, run)
    }

    #[gpui_kit::test]
    fn truncated_body_offers_load_full_body_and_sends_the_command(cx: &mut TestAppContext) {
        let (_s, _v, cx, h, mut w, run) = connected_with(cx, |w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, body(&[b'a'; 1024], 4096));
        });
        assert_eq!(
            text_of(cx, "net-body-truncated").as_deref(),
            Some("Showing the first 1.0 KB of 4.0 KB.")
        );
        assert!(present(cx, "net-load-full"));
        click(cx, "net-load-full");
        let id = EntryId { run, seq: 0 };
        assert_eq!(
            h.commands().last().map(String::as_str),
            Some(format!("load_full_body({id:?}, Response)").as_str())
        );
        // A second click while waiting sends nothing more.
        click(cx, "net-load-full");
        assert_eq!(
            h.commands()
                .iter()
                .filter(|c| c.starts_with("load_full_body"))
                .count(),
            1
        );
        // The spinner shows after the loading delay.
        assert!(!present(cx, "net-load-full-spinner"));
        cx.executor()
            .advance_clock(fdt_ui::tokens::motion::LOADING_DELAY);
        cx.run_until_parked();
        assert!(present(cx, "net-load-full-spinner"));
        // The full body arrives: notice, button and spinner go.
        w.store_full_body(id, BodyKind::Response, vec![b'a'; 4096]);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        assert!(!present(cx, "net-body-truncated"));
        assert!(!present(cx, "net-load-full"));
    }

    #[gpui_kit::test]
    fn load_full_spinner_gives_up_after_the_timeout(cx: &mut TestAppContext) {
        let (_s, _v, cx, _h, _w, _run) = connected_with(cx, |w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, body(&[b'a'; 16], 4096));
        });
        click(cx, "net-load-full");
        cx.executor()
            .advance_clock(fdt_ui::tokens::motion::LOADING_DELAY);
        cx.run_until_parked();
        assert!(present(cx, "net-load-full-spinner"));
        cx.executor()
            .advance_clock(fdt_ui::tokens::motion::FULL_BODY_TIMEOUT);
        cx.run_until_parked();
        assert!(
            !present(cx, "net-load-full-spinner"),
            "the capture kept the stored body"
        );
        assert!(present(cx, "net-load-full"), "and the user may try again");
    }

    #[gpui_kit::test]
    fn too_large_and_evicted_messages(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, BodyState::TooLarge { len: 5 << 20 });
            let id = put(w, run, 1, |_| {});
            w.set_body_state(id, BodyKind::Response, BodyState::Evicted { len: 300 });
            let id = put(w, run, 2, |_| {});
            w.set_body_state(
                id,
                BodyKind::Response,
                BodyState::Unavailable {
                    reason: "The app closed.".into(),
                },
            );
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        assert_eq!(
            text_of(cx, "net-body-too-large").as_deref(),
            Some("This body wasn't copied automatically (5.0 MB).")
        );
        assert!(present(cx, "net-load-full"));
        click_row(cx, 1);
        assert_eq!(
            text_of(cx, "net-body-evicted").as_deref(),
            Some("This body was removed to stay within the 200 MB body budget.")
        );
        assert!(!present(cx, "net-load-full"));
        click_row(cx, 2);
        assert_eq!(
            text_of(cx, "net-body-unavailable").as_deref(),
            Some("The app closed.")
        );
    }

    #[gpui_kit::test]
    fn load_full_body_disabled_when_disconnected(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |_| {});
            w.set_body_state(id, BodyKind::Response, BodyState::TooLarge { len: 5 << 20 });
        });
        // A fixed session is not connected: the recorded run is over.
        let (_s, view, cx) = open_details_over(cx, snap, 0);
        assert!(present(cx, "net-load-full"));
        assert!(view.read_with(cx, |v, cx| v.load_full_disabled(cx)));
        assert_eq!(
            load_full_hint(false),
            Some("Reconnect to the app to load the rest.")
        );
        assert_eq!(load_full_hint(true), None);
        click(cx, "net-load-full");
        assert!(!present(cx, "net-load-full-spinner"));
        cx.executor()
            .advance_clock(fdt_ui::tokens::motion::LOADING_DELAY);
        cx.run_until_parked();
        assert!(
            !present(cx, "net-load-full-spinner"),
            "a disabled button sends nothing"
        );
    }

    #[gpui_kit::test]
    fn copy_buttons_write_the_clipboard(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |u| {
                u.request_headers = vec![("Accept".into(), vec!["*/*".into()])];
            });
            w.set_body_state(id, BodyKind::Response, body(b"caf\xc3\xa9 \xff", 6));
            w.set_body_state(id, BodyKind::Request, body(b"{\"q\":1}", 7));
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        click(cx, "net-copy-url");
        assert_eq!(clipboard(cx).as_deref(), Some("http://a.com/item/0?x=1"));
        click(cx, "net-copy-headers");
        assert_eq!(
            clipboard(cx).as_deref(),
            Some("Accept: */*\n\na-first: z\nContent-Type: application/json\nx-b: 1, 2")
        );
        click(cx, "net-copy-body");
        assert_eq!(clipboard(cx).as_deref(), Some("caf\u{e9} \u{fffd}"));
        click(cx, "net-request-copy-body");
        assert_eq!(clipboard(cx).as_deref(), Some("{\"q\":1}"));
    }

    #[gpui_kit::test]
    fn image_body_renders_an_image_element(cx: &mut TestAppContext) {
        let png = crate::test_support::PNG;
        let snap = snapshot(|w, run| {
            let id = put(w, run, 0, |u| {
                u.response_headers = vec![("content-type".into(), vec!["image/png".into()])];
            });
            w.set_body_state(id, BodyKind::Response, body(&png, png.len() as u64));
            // Not an image at all, served as one: the hex preview takes over.
            let id = put(w, run, 1, |u| {
                u.response_headers = vec![("content-type".into(), vec!["image/png".into()])];
            });
            w.set_body_state(id, BodyKind::Response, body(b"not a png at all", 16));
        });
        let (_s, _v, cx) = open_details_over(cx, snap, 0);
        cx.run_until_parked();
        assert!(present(cx, "net-body-image"));
        click_row(cx, 1);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        frame(cx);
        assert!(
            present(cx, "net-body-image-fallback"),
            "an image that fails to decode shows its bytes"
        );
        assert!(
            text_of(cx, "net-body-line-0")
                .unwrap_or_default()
                .starts_with("00000000  6e 6f 74 20")
        );
    }

    #[gpui_kit::test]
    fn escape_closes_details_then_clears_selection_then_range(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| feed(w, run, 0, 20));
        let (shell, view, cx) = open_details_over(cx, snap, 3);
        let range = crate::nav::TimeRange::new(SessionTime(0), SessionTime(1_000 * MS));
        let mut to = shell.read_with(cx, |s, _| s.current().clone());
        to.range = Some(range);
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        frame(cx);
        let back = shell.read_with(cx, |s, _| s.history().back_len());
        // 1. Details close first; the selection and range stay.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, _| v.details_open()));
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(3));
        // 2. Then the selection goes, replacing the history entry rather than pushing.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
        let (selection, range_now, now_back) = shell.read_with(cx, |s, _| {
            (
                s.current().selection,
                s.current().range,
                s.history().back_len(),
            )
        });
        assert_eq!(selection, None);
        assert_eq!(range_now, Some(range), "the range is still there");
        assert_eq!(now_back, back, "Esc replaced the entry");
        // 3. Then the range.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(shell.read_with(cx, |s, _| s.current().range), None);
        assert_eq!(shell.read_with(cx, |s, _| s.history().back_len()), back + 1);
    }

    #[gpui_kit::test]
    fn the_websocket_lookup_happens_once_per_visible_row(cx: &mut TestAppContext) {
        use fdt_store::net::SocketUpdate;
        let snap = snapshot(|w, run| {
            for n in 0..6u16 {
                let start = 100 * (u64::from(n) + 1);
                put(w, run, u64::from(n), |u| {
                    u.url = format!("ws://a.com:{}/socket", 8000 + n);
                    u.websocket = true;
                    u.status = Some(101);
                    u.remote = Some(("127.0.0.1".into(), 8000 + n));
                    u.start = SessionTime(start * MS);
                });
                w.upsert_socket(SocketUpdate {
                    run,
                    isolate: "i".into(),
                    vm_id: format!("s{n}"),
                    address: "127.0.0.1".into(),
                    port: 8000 + n,
                    kind: "tcp".into(),
                    start: SessionTime(start * MS),
                    end: None,
                    read_bytes: 1,
                    write_bytes: 2,
                    last_read: None,
                    last_write: None,
                });
            }
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-4", "ctrl-4");
        cx.run_until_parked();
        let view = shell.read_with(cx, |s, _| s.network_view().clone());
        super::super::table::SOCKET_LOOKUPS.with(|c| c.set(0));
        super::super::table::CELLS.with(|c| c.set(0));
        click_tab(cx, 1);
        assert_eq!(filtered(&view, cx), 6);
        cx.update(|window, cx| window.render_frame(cx));
        let cells = super::super::table::CELLS.with(|c| c.get());
        let lookups = super::super::table::SOCKET_LOOKUPS.with(|c| c.get());
        assert!(cells >= 5 * 6, "{cells} cells");
        assert!(
            lookups > 0 && lookups <= 6,
            "{lookups} lookups for {cells} cells"
        );
    }

    #[gpui_kit::test]
    fn the_clock_only_redraws_while_a_pending_request_is_listed(cx: &mut TestAppContext) {
        let redraws = |cx: &mut TestAppContext, pending: bool| {
            let (shell, cx, h) = open_shell_fake(cx);
            let (mut w, run) = writer();
            request(&mut w, run, "GET", "http://a.com/ok", Some(200), 0, Some(5));
            if pending {
                request(&mut w, run, "GET", "http://a.com/slow", None, 10, None);
            }
            h.status.send(connected("demo", true)).unwrap();
            h.snaps.send(w.publish()).unwrap();
            cx.run_until_parked();
            press(cx, "cmd-4", "ctrl-4");
            cx.run_until_parked();
            frame(cx);
            let view = shell.read_with(cx, |s, _| s.network_view().clone());
            let count = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = count.clone();
            let _sub = cx.update(|_, cx| {
                cx.observe(&view, move |_, _| {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            });
            for _ in 0..3 {
                cx.executor()
                    .advance_clock(std::time::Duration::from_secs(1));
                cx.run_until_parked();
            }
            count.load(std::sync::atomic::Ordering::SeqCst)
        };
        assert_eq!(
            redraws(cx, false),
            0,
            "nothing is growing, so nothing redraws"
        );
        assert!(redraws(cx, true) >= 3, "a pending bar grows each second");
    }
}
