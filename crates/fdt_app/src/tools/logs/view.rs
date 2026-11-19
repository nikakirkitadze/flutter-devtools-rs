//! The Logs list: a virtualised `uniform_list` over the filtered records, with inline
//! expansion, follow mode and selection that syncs with the shell's location.
//!
//! Filtering and counting run on a background thread ([`compute`]); the UI thread only maps
//! visible rows. At most one job runs at a time; inputs that arrive meanwhile are picked up when
//! it finishes.

use super::model::{Follow, RowMap, RowRef, display_line, short_query_error};
use crate::actions::{
    Collapse, Expand, FollowLatest, GoToFirst, OpenDetails, SelectNext, SelectPrevious,
    ToggleExpand,
};
use crate::keymap::LOGS_LIST_CONTEXT;
use crate::nav::{EntryId, TimeRange};
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use fdt_store::Snapshot;
use fdt_store::log_query::{LogFilter, Matcher, compile_query, filter_logs, filter_logs_two_phase};
use fdt_store::logs::{LogLevel, LogRow, LogSource};
use fdt_store::time::SessionTime;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, motion, radius, row, space, table};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::resizable::{ResizablePanelEvent, ResizableState};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels,
    Render, ScrollEvent, ScrollStrategy, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, UniformListScrollHandle, Window, div,
    relative, uniform_list,
};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

#[cfg(test)]
thread_local! {
    /// Rows built by the list's item closure, for the "only visible rows" test.
    pub(crate) static ITEMS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What the Logs list tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogsEvent {
    /// The selection changed by a click or Enter (`None`: cleared). The shell pushes a history
    /// entry.
    Selected(Option<EntryId>),
    /// The selection moved by keyboard (or was dropped by a filter). The shell replaces the
    /// current history entry instead of pushing, so holding `j` does not fill Back.
    Moved(Option<EntryId>),
    /// "Clear filters" also asks for the shared time range to go.
    ClearRange,
}

pub(super) const ALL_LEVELS: [LogLevel; 4] = [
    LogLevel::Debug,
    LogLevel::Info,
    LogLevel::Warning,
    LogLevel::Error,
];
pub(super) const ALL_SOURCES: [LogSource; 5] = [
    LogSource::Log,
    LogSource::Stdout,
    LogSource::Stderr,
    LogSource::FlutterError,
    LogSource::Navigation,
];

/// What a filter job was computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    generation: u64,
    /// Bumped on every filter change.
    version: u64,
}

/// The state a finished job left behind, so the next one can append.
#[derive(Clone, Copy, Debug)]
struct Applied {
    key: Key,
    len: usize,
    first: SessionTime,
    last: SessionTime,
}

struct JobResult {
    filtered: Arc<[u32]>,
    counts: [u64; 4],
    incremental: bool,
    /// The store under the rows is not the one they were computed from: a new session or run
    /// replaced it, so expansion and selection no longer mean anything.
    replaced: bool,
}

/// First and last record times, to tell a grown store from a replaced one.
fn fingerprint(snap: &Snapshot) -> Option<(SessionTime, SessionTime)> {
    let n = snap.logs.len();
    (n > 0).then(|| (snap.logs.row(0).time, snap.logs.row(n - 1).time))
}

/// Filters `snap` and counts levels in the filter's range. O(records) in the worst case; runs
/// off the UI thread. With a `prior` result of the same filter over a store this one extends,
/// only the new records are scanned.
///
/// A text search runs in two phases: `first` gets the newest rows as soon as they are known. When
/// `cancel` is set the scan stops and the result is `None`.
fn compute(
    snap: &Snapshot,
    filter: &LogFilter,
    version: u64,
    prior: Option<(Applied, Arc<[u32]>)>,
    cancel: &AtomicBool,
    first: oneshot::Sender<Vec<u32>>,
) -> Option<JobResult> {
    let counts = snap.logs.level_counts(filter.range);
    let same_filter = prior
        .as_ref()
        .is_some_and(|(p, _)| p.key.version == version);
    if let Some((p, old)) = prior
        && p.key.version == version
        && p.len > 0
        && snap.logs.len() >= p.len
        && fingerprint(snap).is_some_and(|(first, _)| first == p.first)
        && snap.logs.row(p.len - 1).time == p.last
    {
        let new = filter_logs(&snap.logs, filter, p.len);
        let filtered = if new.is_empty() {
            old
        } else {
            let mut all = Vec::with_capacity(old.len() + new.len());
            all.extend_from_slice(&old);
            all.extend_from_slice(&new);
            all.into()
        };
        return Some(JobResult {
            filtered,
            counts,
            incremental: true,
            replaced: false,
        });
    }
    let filtered: Arc<[u32]> = if filter.text.is_some() {
        let mut first = Some(first);
        filter_logs_two_phase(&snap.logs, filter, cancel, |rows| {
            if let Some(tx) = first.take() {
                tx.send(rows).ok();
            }
        })?
        .into()
    } else {
        filter_logs(&snap.logs, filter, 0).into()
    };
    Some(JobResult {
        filtered,
        counts,
        incremental: false,
        replaced: same_filter,
    })
}

pub struct LogsView {
    session: Entity<SessionModel>,
    pub(super) filter: LogFilter,
    version: u64,
    /// The snapshot `rows` refers to.
    pub(super) snap: Arc<Snapshot>,
    pub(super) rows: RowMap,
    pub(super) counts: [u64; 4],
    follow: Follow,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    pub(super) selected: Option<u32>,
    /// A selection from the location, waiting for the filter result it depends on.
    pending: Option<EntryId>,
    applied: Option<Applied>,
    running: bool,
    task: Option<Task<()>>,
    /// Set after `LOADING_DELAY` of a running job.
    pub(super) show_skeleton: bool,
    skeleton_task: Option<Task<()>>,
    /// The rows the list built last frame.
    rendered: Range<usize>,
    /// Focus the list the next time it is drawn (its element may not exist yet).
    focus_wanted: bool,
    /// The search field.
    pub(super) search: Entity<InputState>,
    /// The text of the field the filter was last started for (or tried to).
    query: SharedString,
    pub(super) regex: bool,
    /// The query and mode the current text filter was compiled from.
    applied_query: Option<(SharedString, bool)>,
    /// Shown instead of the match count while the query does not compile; the rows stay.
    pub(super) search_error: Option<SharedString>,
    /// The text filter, prepared once for highlighting rendered rows.
    pub(super) matcher: Option<Arc<Matcher>>,
    /// Set when the filter changes so a running scan stops; replaced by a fresh flag.
    cancel: Arc<AtomicBool>,
    /// Waits for a job's first-phase rows.
    first_task: Option<Task<()>>,
    #[cfg(test)]
    first_phases: usize,
    pub(super) details_open: bool,
    pub(super) details: super::details::DetailsState,
    split: Option<Entity<ResizableState>>,
    pub(super) details_width: Pixels,
    _split_sub: Option<Subscription>,
    _search_sub: Subscription,
    _session: Subscription,
}

impl EventEmitter<LogsEvent> for LogsView {}

impl LogsView {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observed = cx.observe(&session, |this, _, cx| this.request(cx));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search logs"));
        let search_sub = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.query_changed(cx),
                InputEvent::PressEnter { .. } => window.focus(&this.focus, cx),
                _ => {}
            },
        );
        let snap = session.read(cx).snapshot().clone();
        let mut this = Self {
            session,
            filter: LogFilter::default(),
            version: 0,
            snap,
            rows: RowMap::new(Arc::from(Vec::new())),
            counts: [0; 4],
            follow: Follow::default(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            selected: None,
            pending: None,
            applied: None,
            running: false,
            task: None,
            show_skeleton: false,
            skeleton_task: None,
            rendered: 0..0,
            focus_wanted: false,
            search,
            query: SharedString::default(),
            regex: false,
            applied_query: None,
            search_error: None,
            matcher: None,
            cancel: Arc::new(AtomicBool::new(false)),
            first_task: None,
            #[cfg(test)]
            first_phases: 0,
            details_open: false,
            details: super::details::DetailsState::default(),
            split: None,
            details_width: fdt_ui::tokens::layout::DETAILS_WIDTH,
            _split_sub: None,
            _search_sub: search_sub,
            _session: observed,
        };
        this.request(cx);
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Puts focus on the list when it is next drawn, so its keys work.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_wanted = true;
        cx.notify();
    }

    // ---- filter job ----

    /// Starts a filter job for the newest snapshot and filter unless one is current or running.
    fn request(&mut self, cx: &mut Context<Self>) {
        let key = Key {
            generation: self.session.read(cx).snapshot().generation,
            version: self.version,
        };
        // A running job re-reads the newest snapshot and filter when it finishes a round.
        if self.running || self.applied.is_some_and(|a| a.key == key) {
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
                    if done == Some(key) || this.applied.is_some_and(|a| a.key == key) {
                        this.finish_job(cx);
                        return None;
                    }
                    let prior = this.applied.map(|a| (a, this.rows.filtered().clone()));
                    // The first-phase rows of a text search arrive on a channel the view
                    // awaits, so the UI thread never blocks on the scan.
                    let (first_tx, first_rx) = oneshot::channel();
                    this.first_task = Some(cx.spawn(async move |this, cx| {
                        if let Ok(rows) = first_rx.await {
                            this.update(cx, |this, cx| this.apply_first(key, rows, cx))
                                .ok();
                        }
                    }));
                    Some((
                        key,
                        snap,
                        this.filter.clone(),
                        prior,
                        this.cancel.clone(),
                        first_tx,
                    ))
                });
                let Ok(Some((key, snap, filter, prior, cancel, first_tx))) = next else {
                    break;
                };
                let job_snap = snap.clone();
                let result = cx
                    .background_spawn(async move {
                        compute(&job_snap, &filter, key.version, prior, &cancel, first_tx)
                    })
                    .await;
                done = Some(key);
                let applied = this.update(cx, |this, cx| {
                    // A result for an older filter is dropped, and so is a cancelled scan. An
                    // older snapshot is not: the store only grows, and the next round picks up
                    // what was missed.
                    if this.version == key.version
                        && let Some(result) = result
                    {
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
        self.first_task = None;
        self.skeleton_task = None;
        if self.show_skeleton {
            self.show_skeleton = false;
            cx.notify();
        }
    }

    /// Takes a finished job's result.
    fn apply(&mut self, key: Key, snap: Arc<Snapshot>, result: JobResult, cx: &mut Context<Self>) {
        let at_end = self.scroll.is_scrolled_to_end();
        let first_load = self.applied.is_none();
        let old_rows = self.rows.row_count();
        if result.replaced {
            // A new session or run replaced the store: nothing expanded or selected before
            // refers to these records.
            self.rows.clear_expanded();
            self.pending = None;
            if self.selected.take().is_some() {
                cx.emit(LogsEvent::Moved(None));
            }
            self.details.reset();
        }
        self.rows.set_filtered(result.filtered);
        self.counts = result.counts;
        let (first, last) = fingerprint(&snap).unwrap_or((SessionTime(0), SessionTime(0)));
        self.applied = Some(Applied {
            key,
            len: snap.logs.len(),
            first,
            last,
        });
        self.snap = snap;
        if let Some(id) = self.pending.take() {
            self.select_from_location(id, cx);
        } else if let Some(sel) = self.selected
            && self.rows.row_of(sel).is_none()
        {
            self.selected = None;
            cx.emit(LogsEvent::Moved(None));
        }
        if first_load || !result.incremental {
            if self.follow.following {
                self.scroll.scroll_to_bottom();
            }
        } else {
            let added = self.rows.row_count().saturating_sub(old_rows);
            if added > 0 && self.follow.on_new_rows(added, at_end) {
                self.scroll.scroll_to_bottom();
            }
        }
        cx.notify();
    }

    /// Shows the newest matches of a text search before the full scan ends. Ignored when the
    /// full result of the same job has already arrived, or the filter has moved on.
    fn apply_first(&mut self, key: Key, rows: Vec<u32>, cx: &mut Context<Self>) {
        if self.version != key.version || self.applied.is_some_and(|a| a.key == key) {
            return;
        }
        self.rows.set_filtered(rows.into());
        if self.follow.following {
            self.scroll.scroll_to_bottom();
        }
        #[cfg(test)]
        {
            self.first_phases += 1;
        }
        cx.notify();
    }

    /// Whether the rows on screen are for the current filter and nothing is being computed.
    fn settled(&self) -> bool {
        !self.running && self.applied.is_some_and(|a| a.key.version == self.version)
    }

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.version += 1;
        // The running scan is for a filter nobody wants any more.
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.request(cx);
        cx.notify();
    }

    // ---- filters ----

    pub(super) fn level_clicked(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        // From "all", a click shows only that level; after that clicks add and remove levels,
        // and removing the last one goes back to all.
        if self.filter.levels == fdt_store::log_query::LevelSet::ALL {
            for l in ALL_LEVELS {
                if l != level {
                    self.filter.levels.toggle(l);
                }
            }
        } else {
            self.filter.levels.toggle(level);
            if !ALL_LEVELS.iter().any(|l| self.filter.levels.contains(*l)) {
                self.filter.levels = fdt_store::log_query::LevelSet::ALL;
            }
        }
        self.bump(cx);
    }

    pub(super) fn toggle_source(&mut self, source: LogSource, cx: &mut Context<Self>) {
        self.filter.sources.toggle(source);
        if !ALL_SOURCES.iter().any(|s| self.filter.sources.contains(*s)) {
            self.filter.sources = fdt_store::log_query::SourceSet::ALL;
        }
        self.bump(cx);
    }

    /// From "all loggers", a click shows only that logger; after that it adds or removes it.
    pub(super) fn toggle_logger(&mut self, name: Arc<str>, cx: &mut Context<Self>) {
        let mut set = self.filter.loggers.as_deref().cloned().unwrap_or_default();
        if self.filter.loggers.is_none() || !set.remove(&name) {
            set.insert(name);
        }
        self.filter.loggers = (!set.is_empty()).then(|| Arc::new(set));
        self.bump(cx);
    }

    pub(super) fn all_loggers(&mut self, cx: &mut Context<Self>) {
        self.filter.loggers = None;
        self.bump(cx);
    }

    /// Resets every filter of the list, the search query and, when one is set, the shared time
    /// range (the shell is asked to drop it).
    pub fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = LogFilter {
            range: self.filter.range,
            ..LogFilter::default()
        };
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.query = SharedString::default();
        self.applied_query = None;
        self.search_error = None;
        self.matcher = None;
        if self.filter.range.is_some() {
            cx.emit(LogsEvent::ClearRange);
        }
        self.bump(cx);
    }

    // ---- search ----

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let value = self.search.read(cx).value();
        // Enter repeats the last change; only a different value is a new query.
        if value == self.query {
            return;
        }
        self.query = value;
        self.apply_query(cx);
    }

    /// Compiles the field's text and, if valid and new, starts a scan for it. An invalid query
    /// shows its error and leaves the rows as they were.
    fn apply_query(&mut self, cx: &mut Context<Self>) {
        match compile_query(&self.query, self.regex) {
            Err(e) => {
                self.search_error = Some(short_query_error(&e.message).into());
                cx.notify();
            }
            Ok(q) => {
                self.search_error = None;
                // An empty query is "no query", whatever the mode.
                let key = q.is_some().then(|| (self.query.clone(), self.regex));
                if self.applied_query == key {
                    cx.notify();
                    return;
                }
                self.applied_query = key;
                self.matcher = q.as_ref().map(|q| Arc::new(q.matcher()));
                self.filter.text = q;
                self.bump(cx);
            }
        }
    }

    /// Sets the search field and regex mode and runs the search. The palette uses this.
    pub fn set_search(
        &mut self,
        query: &str,
        regex: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.regex = regex;
        self.search.update(cx, |input, cx| {
            input.set_value(query.to_string(), window, cx)
        });
        self.query = query.to_string().into();
        self.apply_query(cx);
    }

    #[cfg(test)]
    pub(crate) fn search_query(&self) -> &str {
        &self.query
    }

    pub(super) fn toggle_regex(&mut self, cx: &mut Context<Self>) {
        self.regex = !self.regex;
        self.apply_query(cx);
        cx.notify();
    }

    /// Whether the search field is on screen: the list view, not the empty state.
    fn has_toolbar(&self, cx: &Context<Self>) -> bool {
        !self.session.read(cx).snapshot().logs.is_empty()
    }

    /// Focuses the search field (⌘F).
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_toolbar(cx) {
            self.search.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// The matches of the current text filter, once its scan is the one on screen.
    pub(super) fn match_count(&self) -> Option<usize> {
        (self.filter.text.is_some() && self.settled() && self.search_error.is_none())
            .then(|| self.rows.filtered().len())
    }

    /// Whether a text search is scanning and has been for longer than the loading delay.
    pub(super) fn searching(&self) -> bool {
        self.show_skeleton && self.filter.text.is_some()
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

    fn selected_id(&self) -> Option<EntryId> {
        let seq = self.selected? as usize;
        (seq < self.snap.logs.len()).then(|| self.snap.logs.row(seq).id)
    }

    /// Selects and reveals `id`; clears the selection, silently, if it is not in the list.
    fn select_from_location(&mut self, id: EntryId, cx: &mut Context<Self>) {
        let seq = u32::try_from(id.seq)
            .ok()
            .filter(|&s| self.rows.row_of(s).is_some() && self.snap.logs.row(s as usize).id == id);
        match seq {
            Some(seq) => {
                self.selected = Some(seq);
                self.follow.on_select();
                self.reveal(seq, ScrollStrategy::Center);
            }
            None => {
                self.selected = None;
            }
        }
        cx.notify();
    }

    fn reveal(&self, seq: u32, strategy: ScrollStrategy) {
        if let Some(row) = self.rows.row_of(seq) {
            self.scroll.scroll_to_item(row, strategy);
        }
    }

    // ---- selection and keys ----

    /// Selects `seq`. Keyboard movement (`replace`) rewrites the current history entry; a click
    /// pushes a new one.
    fn select_seq(&mut self, seq: u32, replace: bool, cx: &mut Context<Self>) {
        self.selected = Some(seq);
        self.follow.on_select();
        self.reveal(seq, ScrollStrategy::Nearest);
        let id = self.snap.logs.row(seq as usize).id;
        cx.emit(if replace {
            LogsEvent::Moved(Some(id))
        } else {
            LogsEvent::Selected(Some(id))
        });
        cx.notify();
    }

    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        let filtered = self.rows.filtered().clone();
        if filtered.is_empty() {
            return;
        }
        let last = filtered.len() - 1;
        let target = match self.selected.and_then(|s| filtered.binary_search(&s).ok()) {
            Some(i) if forward => (i + 1).min(last),
            Some(i) => i.saturating_sub(1),
            None => {
                // Nothing selected: start from the edge of what is on screen.
                let row = if forward {
                    self.rendered.start
                } else {
                    self.rendered.end.saturating_sub(1)
                };
                let row = row.min(self.rows.row_count().saturating_sub(1));
                let (RowRef::Record { seq }
                | RowRef::Continuation { seq, .. }
                | RowRef::ShowAll { seq }) = self.rows.resolve(row);
                filtered.binary_search(&seq).unwrap_or(0)
            }
        };
        self.select_seq(filtered[target], true, cx);
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.move_selection(true, cx);
    }

    pub fn select_previous(&mut self, cx: &mut Context<Self>) {
        self.move_selection(false, cx);
    }

    /// Toggles the selected record's expansion; with `only_in`, only when it is in that state.
    fn toggle_selected(&mut self, only: Option<bool>, cx: &mut Context<Self>) {
        let Some(seq) = self.selected else { return };
        if only.is_some_and(|current| current != self.rows.is_expanded(seq)) {
            return;
        }
        let extra = self
            .snap
            .logs
            .row(seq as usize)
            .line_count
            .saturating_sub(1);
        self.rows.toggle(seq, extra);
        cx.notify();
    }

    pub fn toggle_expand(&mut self, cx: &mut Context<Self>) {
        self.toggle_selected(None, cx);
    }

    pub fn expand(&mut self, cx: &mut Context<Self>) {
        self.toggle_selected(Some(false), cx);
    }

    pub fn collapse(&mut self, cx: &mut Context<Self>) {
        self.toggle_selected(Some(true), cx);
    }

    /// Shows the details pane for the selected record.
    pub fn open_details(&mut self, cx: &mut Context<Self>) {
        let Some(seq) = self.selected else { return };
        self.details_open = true;
        // Enter commits the selection as a history step (nothing new if it already is one).
        let id = self.snap.logs.row(seq as usize).id;
        cx.emit(LogsEvent::Selected(Some(id)));
        cx.notify();
    }

    /// Closes the details pane; true when it was open (Esc closes it before the selection).
    pub fn close_details(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::take(&mut self.details_open);
        if was {
            cx.notify();
        }
        was
    }

    pub fn follow_latest(&mut self, cx: &mut Context<Self>) {
        self.follow.resume();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(super) fn toggle_follow(&mut self, cx: &mut Context<Self>) {
        if self.follow.following {
            self.follow.on_user_scrolled_up();
            cx.notify();
        } else {
            self.follow_latest(cx);
        }
    }

    pub fn go_to_first(&mut self, cx: &mut Context<Self>) {
        if let Some(&first) = self.rows.filtered().first() {
            self.select_seq(first, true, cx);
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
    }

    pub(super) fn follow_state(&self) -> Follow {
        self.follow
    }

    #[cfg(test)]
    pub fn filtered_len(&self) -> usize {
        self.rows.filtered().len()
    }

    #[cfg(test)]
    pub fn row_count(&self) -> usize {
        self.rows.row_count()
    }

    #[cfg(test)]
    pub fn rendered(&self) -> Range<usize> {
        self.rendered.clone()
    }

    #[cfg(test)]
    pub fn follow(&self) -> Follow {
        self.follow
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    #[cfg(test)]
    pub fn is_filtering(&self) -> bool {
        self.running
    }

    #[cfg(test)]
    pub fn last_cancel(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    #[cfg(test)]
    pub fn search_value(&self, cx: &gpui_kit::App) -> SharedString {
        self.search.read(cx).value()
    }

    #[cfg(test)]
    pub fn search_focused(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        gpui_kit::Focusable::focus_handle(self.search.read(cx), cx).is_focused(window)
    }

    #[cfg(test)]
    pub fn details_open(&self) -> bool {
        self.details_open
    }

    #[cfg(test)]
    pub fn is_expanded(&self, seq: u32) -> bool {
        self.rows.is_expanded(seq)
    }

    #[cfg(test)]
    pub fn show_skeleton_for_test(&mut self, cx: &mut Context<Self>) {
        self.show_skeleton = true;
        cx.notify();
    }

    #[cfg(test)]
    pub fn searching_for_test(&self) -> bool {
        self.searching()
    }

    #[cfg(test)]
    pub fn first_phases(&self) -> usize {
        self.first_phases
    }

    #[cfg(test)]
    pub fn scroll_to_top(&self) {
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    #[cfg(test)]
    pub fn details_lines(&self) -> usize {
        self.details.line_count()
    }

    #[cfg(test)]
    pub fn apply_first_for_test(&mut self, rows: Vec<u32>, cx: &mut Context<Self>) {
        let key = Key {
            generation: self.snap.generation,
            version: self.version,
        };
        self.apply_first(key, rows, cx);
    }
}

// ---- rendering ----

/// The colour of a level's badge.
pub(super) fn level_color(level: LogLevel, t: &fdt_ui::tokens::Palette) -> Hsla {
    Hsla::from(match level {
        LogLevel::Debug => t.text_muted,
        LogLevel::Info => t.text,
        LogLevel::Warning => t.warning,
        LogLevel::Error => t.danger,
    })
}

/// `m:ss.mmm`.
pub(super) fn format_time(t: SessionTime) -> String {
    format!("{}.{:03}", format_offset(t), (t.0 / 1_000_000) % 1_000)
}

impl LogsView {
    /// The rows `range` of the list. Only called for the rows on screen.
    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        #[cfg(test)]
        ITEMS.with(|c| c.set(c.get() + range.len()));
        let count = self.rows.row_count();
        let range = range.start.min(count)..range.end.min(count);
        self.rendered = range.clone();
        let t = *cx.tokens();
        range.map(|r| self.render_row(r, &t, cx)).collect()
    }

    fn render_row(
        &self,
        row_ix: usize,
        t: &fdt_ui::tokens::Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rr = self.rows.resolve(row_ix);
        let (RowRef::Record { seq } | RowRef::Continuation { seq, .. } | RowRef::ShowAll { seq }) =
            rr;
        let rec = self.snap.logs.row(seq as usize);
        let selected = self.selected == Some(seq);
        let base = div()
            .id(SharedString::from(format!("log-row-{row_ix}")))
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .w_full()
            .h(row::LOG)
            .px(space::S12)
            .gap(space::S8)
            .font_family(font::MONO)
            .text_size(font::DENSE)
            .line_height(relative(font::LINE_HEIGHT_LOG))
            .font_features(font::tabular_figures())
            .whitespace_nowrap()
            .overflow_hidden()
            .when_selected(selected, rr, t)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.select_seq(seq, false, cx);
                    if ev.click_count >= 2 || matches!(rr, RowRef::ShowAll { .. }) {
                        this.open_details(cx);
                    }
                }),
            );
        let view = cx.weak_entity();
        let menu =
            move |menu: gpui_kit::component::menu::PopupMenu,
                  _window: &mut Window,
                  _cx: &mut Context<gpui_kit::component::menu::PopupMenu>| {
                let copy = view.clone();
                let json = view.clone();
                menu.item(PopupMenuItem::new("Copy").on_click(move |_, _, cx| {
                    copy.update(cx, |v, cx| v.copy_text(seq, cx)).ok();
                }))
                .item(
                    PopupMenuItem::new("Copy as JSON").on_click(move |_, _, cx| {
                        json.update(cx, |v, cx| v.copy_json(seq, cx)).ok();
                    }),
                )
            };
        let indent = table::COL_TIME + table::COL_LEVEL + table::COL_LOGGER + space::S8 * 3.0;
        match rr {
            RowRef::Record { .. } => base
                .child(self.record_cells(&rec, seq, t, cx))
                .context_menu(menu)
                .into_any_element(),
            RowRef::Continuation { line, .. } => {
                let text = rec.text.split('\n').nth(line as usize).unwrap_or("");
                let (shown, cut) = display_line(text);
                let ranges = self.matcher_ranges(text);
                let shown = if cut {
                    format!("{shown}\u{2026}")
                } else {
                    shown.to_string()
                };
                let bg = t.accent.with_alpha(alpha::SELECTION);
                base.text_color(Hsla::from(t.text_muted))
                    .child(div().flex_none().w(indent))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(self.highlighted(shown, ranges, bg)),
                    )
                    .context_menu(menu)
                    .into_any_element()
            }
            RowRef::ShowAll { .. } => base
                .text_color(Hsla::from(t.accent))
                .child(div().flex_none().w(indent))
                .child(format!("Show all {} lines in details", rec.line_count))
                .context_menu(menu)
                .into_any_element(),
        }
    }

    fn record_cells(
        &self,
        rec: &LogRow<'_>,
        seq: u32,
        t: &fdt_ui::tokens::Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (shown, cut) = display_line(rec.text);
        let ranges = self.matcher_ranges(rec.text);
        let bg = t.accent.with_alpha(alpha::SELECTION);
        let message = if cut {
            format!("{shown}\u{2026}")
        } else {
            shown.to_string()
        };
        let source = if rec.logger.is_empty() {
            rec.source.label().to_string()
        } else {
            rec.logger.to_string()
        };
        let extra = rec.line_count.saturating_sub(1);
        let more = (extra > 0).then(|| {
            div()
                .id(SharedString::from(format!("log-more-{seq}")))
                .test_support()
                .flex_none()
                .px(space::S6)
                .rounded(radius::CHIP)
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .bg(Hsla::from(t.elevated))
                .child(format!("+{extra} lines"))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    // Keep the row from also taking the press as a selection.
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.rows.toggle(seq, extra);
                    cx.notify();
                }))
        });
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(space::S8)
            .child(
                div()
                    .flex_none()
                    .w(table::COL_TIME)
                    .text_color(Hsla::from(t.text_muted))
                    .child(format_time(rec.time)),
            )
            .child(
                div()
                    .flex_none()
                    .w(table::COL_LEVEL)
                    .font_weight(font::MEDIUM)
                    .text_color(level_color(rec.level, t))
                    .child(rec.level.short()),
            )
            .child(
                div()
                    .flex_none()
                    .w(table::COL_LOGGER)
                    .truncate()
                    .text_color(Hsla::from(t.text_muted))
                    .child(source),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.highlighted(message, ranges, bg)),
            )
            .children(more)
            .into_any_element()
    }

    /// The rows as a virtualised list.
    fn rows_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.rows.row_count();
        uniform_list(
            "logs-list-items",
            count,
            cx.processor(|this, range: Range<usize>, _window, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    /// The area below the toolbar. It owns the list's keys and focus even while it shows an
    /// empty state, so focus never points at an element that is not on screen.
    fn list_area(&self, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("logs-list")
            .test_support()
            .key_context(LOGS_LIST_CONTEXT)
            .track_focus(&self.focus)
            .tab_stop(true)
            .flex_1()
            .min_h_0()
            .w_full()
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select_next(cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.select_previous(cx)))
            .on_action(cx.listener(|this, _: &ToggleExpand, _, cx| this.toggle_expand(cx)))
            .on_action(cx.listener(|this, _: &Expand, _, cx| this.expand(cx)))
            .on_action(cx.listener(|this, _: &Collapse, _, cx| this.collapse(cx)))
            .on_action(cx.listener(|this, _: &OpenDetails, _, cx| this.open_details(cx)))
            .on_action(cx.listener(|this, _: &FollowLatest, _, cx| this.follow_latest(cx)))
            .on_action(cx.listener(|this, _: &GoToFirst, _, cx| this.go_to_first(cx)))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                // Scrolling up leaves the end of the list, so following stops.
                if ev.delta.pixel_delta(row::LOG).y > gpui_kit::px(0.) {
                    this.follow.on_user_scrolled_up();
                    cx.notify();
                } else {
                    // Scrolling back down to the end resumes following. Checked once the list
                    // has moved.
                    let view = cx.entity();
                    cx.defer(move |cx| {
                        view.update(cx, |this, cx| {
                            let at_end = this.scroll.is_scrolled_to_end();
                            let before = this.follow;
                            this.follow.on_scrolled(at_end);
                            if this.follow != before {
                                cx.notify();
                            }
                        });
                    });
                }
            }))
            .child(content)
            .into_any_element()
    }

    /// The list beside the details pane, in a split the user can resize.
    fn with_details(&mut self, list: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        use fdt_ui::tokens::layout;
        use gpui_kit::component::resizable::{h_resizable, resizable_panel};
        let state = match &self.split {
            Some(state) => state.clone(),
            None => {
                let state = cx.new(|_| ResizableState::default());
                self._split_sub = Some(cx.subscribe(
                    &state,
                    |this, state, _: &ResizablePanelEvent, cx| {
                        if let Some(&w) = state.read(cx).sizes().last() {
                            this.details_width = w;
                        }
                    },
                ));
                self.split = Some(state.clone());
                state
            }
        };
        let details = self.render_details(cx);
        h_resizable("logs-split")
            .with_state(&state)
            .child(resizable_panel().child(list))
            .child(
                resizable_panel()
                    .size(self.details_width)
                    .size_range(layout::DETAILS_MIN..layout::DETAILS_MAX)
                    .flex_none()
                    .child(details),
            )
            .into_any_element()
    }

    fn no_match(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::S12)
            .child(
                div()
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text_muted))
                    .child("No lines match your filters."),
            )
            .child(
                div()
                    .id("logs-clear-filters")
                    .test_support()
                    .aria_label("Clear filters")
                    .px(space::S12)
                    .py(space::S4)
                    .rounded(radius::CONTROL)
                    .bg(Hsla::from(t.elevated))
                    .text_size(font::BODY)
                    .text_color(Hsla::from(t.text))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx)))
                    .child("Clear filters"),
            )
            .into_any_element()
    }
}

/// Selected rows get a tint; their continuation rows a lighter one.
trait RowTint: Sized {
    fn when_selected(self, selected: bool, rr: RowRef, t: &fdt_ui::tokens::Palette) -> Self;
}

impl<E: gpui_kit::Styled> RowTint for E {
    fn when_selected(self, selected: bool, rr: RowRef, t: &fdt_ui::tokens::Palette) -> Self {
        match (selected, rr) {
            (true, RowRef::Record { .. }) => self.bg(t.accent.with_alpha(alpha::SELECTION)),
            (true, _) => self.bg(t.accent.with_alpha(alpha::ACTIVE_ROW)),
            _ => self,
        }
    }
}

impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_wanted)
            && !window.context_stack().iter().any(|c| c.contains("Input"))
        {
            // Not while the user is typing: a range drag or a status change must not pull
            // focus out of the search field or the palette.
            window.focus(&self.focus, cx);
        }
        if self.session.read(cx).snapshot().logs.is_empty() {
            let empty = div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(crate::shell::tool_page::empty_state(
                    crate::nav::Tool::Logs,
                    true,
                ))
                .into_any_element();
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.list_area(empty, cx))
                .into_any_element();
        }
        let empty = self.applied.is_some() && self.rows.filtered().is_empty();
        let body = if empty {
            self.no_match(cx)
        } else {
            self.rows_list(cx)
        };
        let list = self.list_area(body, cx);
        let content = if self.details_open {
            self.with_details(list, cx)
        } else {
            list
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(content),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Location, Page, SessionTime, Tool};
    use crate::test_support::{
        connected, exists, log_at, open_shell_fake, open_shell_snapshot, press, snapshot, writer,
    };
    use fdt_store::logs::{LogInput, LogLevel};
    use fdt_store::time::RunId;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    const SEC: u64 = 1_000_000_000;

    fn frame(cx: &mut VisualTestContext) {
        for _ in 0..2 {
            cx.update(|window, cx| window.render_frame(cx));
        }
    }

    /// How many list rows are on screen.
    fn rows_on_screen(cx: &mut VisualTestContext) -> usize {
        frame(cx);
        cx.update(|window, _| {
            (0..400)
                .filter(|i| window.try_find(format!("log-row-{i}")).is_some())
                .count()
        })
    }

    fn present(cx: &mut VisualTestContext, id: &str) -> bool {
        frame(cx);
        let id = id.to_string();
        cx.update(|window, _| window.try_find(id).is_some())
    }

    fn click(cx: &mut VisualTestContext, id: &str) {
        frame(cx);
        let id = id.to_string();
        cx.update(|window, cx| window.click(id, cx));
    }

    fn view_of(
        shell: &gpui_kit::Entity<crate::shell::MainWindow>,
        cx: &mut VisualTestContext,
    ) -> gpui_kit::Entity<LogsView> {
        shell.read_with(cx, |s, _| s.logs_view().clone())
    }

    /// `n` records, every tenth an error, every 25th three lines long.
    fn feed(w: &mut fdt_store::SessionWriter, run: RunId, from: usize, to: usize) {
        for i in from..to {
            let level = if i % 10 == 0 {
                LogLevel::Error
            } else {
                LogLevel::Info
            };
            let mut input = log_at(run, i as u64 * 1_000, level);
            if i % 25 == 5 {
                input.text = format!("record {i}\nsecond\nthird");
            }
            w.push_log(input);
        }
    }

    fn open_logs(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<LogsView>,
        &mut VisualTestContext,
    ) {
        let snap = snapshot(|w, run| feed(w, run, 0, n));
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        (shell, view, cx)
    }

    #[gpui_kit::test]
    fn renders_only_visible_rows_at_one_million(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| fdt_store::synth::logs(w, run, 1_000_000, 7));
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.simulate_resize(fdt_ui::tokens::layout::WINDOW_MIN);
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        assert!(view.read_with(cx, |v, _| v.filtered_len()) >= 1_000_000);
        ITEMS.with(|c| c.set(0));
        cx.update(|window, cx| window.render_frame(cx));
        let calls = ITEMS.with(|c| c.get());
        println!("item-closure calls in one frame at 1M: {calls}");
        assert!(calls > 0 && calls <= 40, "{calls} rows built");
    }

    #[gpui_kit::test]
    fn new_lines_scroll_to_the_bottom_while_following(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 100);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        let (end, count) = view.read_with(cx, |v, _| (v.rendered().end, v.row_count()));
        assert_eq!(count, 100);
        assert_eq!(end, 100, "the last row is on screen");
        feed(&mut w, run, 100, 160);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let (end, count) = view.read_with(cx, |v, _| (v.rendered().end, v.row_count()));
        assert_eq!(count, 160);
        assert_eq!(end, 160, "still following");
    }

    #[gpui_kit::test]
    fn selecting_pauses_follow_and_shows_unseen_count(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 100);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        assert!(!present(cx, "logs-unseen"));
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, _| v.follow().following));
        feed(&mut w, run, 100, 105);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.follow().unseen), 5);
        assert!(present(cx, "logs-unseen"));
        let label = cx.update(|window, _| window.find("logs-unseen").label().map(str::to_string));
        assert_eq!(label.as_deref(), Some("5 new lines \u{2193}"));
    }

    #[gpui_kit::test]
    fn end_resumes_following(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 100);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        let view = view_of(&shell, cx);
        cx.simulate_keystrokes("j");
        feed(&mut w, run, 100, 120);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.follow().unseen), 20);
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        frame(cx);
        let (follow, end) = view.read_with(cx, |v, _| (v.follow(), v.rendered().end));
        assert!(follow.following && follow.unseen == 0);
        assert_eq!(end, 120);
        assert!(!present(cx, "logs-unseen"));
    }

    #[gpui_kit::test]
    fn space_expands_a_multi_line_record(cx: &mut TestAppContext) {
        // Record 5 has three lines.
        let (_shell, view, cx) = open_logs(cx, 8);
        let before = rows_on_screen(cx);
        assert_eq!(before, 8);
        click(cx, "log-row-5");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(5));
        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        assert_eq!(rows_on_screen(cx), before + 2);
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        assert_eq!(rows_on_screen(cx), before);
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(rows_on_screen(cx), before + 2);
    }

    #[gpui_kit::test]
    fn the_more_lines_badge_toggles_expansion(cx: &mut TestAppContext) {
        let (_shell, _view, cx) = open_logs(cx, 8);
        assert!(present(cx, "log-more-5"));
        assert!(!present(cx, "log-more-4"));
        let before = rows_on_screen(cx);
        click(cx, "log-more-5");
        cx.run_until_parked();
        assert_eq!(rows_on_screen(cx), before + 2);
    }

    #[gpui_kit::test]
    fn error_chip_filters_to_errors(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_logs(cx, 100);
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 100);
        click(cx, "logs-level-error");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 10);
        let label =
            cx.update(|window, _| window.find("logs-level-error").label().map(str::to_string));
        assert!(
            label.as_deref().is_some_and(|l| l.contains("10")),
            "{label:?}"
        );
    }

    #[gpui_kit::test]
    fn range_filters_rows(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_logs(cx, 10);
        let mut loc = Location::at(Page::Tool(Tool::Logs));
        loc.range = Some(crate::nav::TimeRange::new(
            SessionTime(4 * SEC),
            SessionTime(6 * SEC),
        ));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 3);
    }

    #[gpui_kit::test]
    fn clear_filters_button(cx: &mut TestAppContext) {
        // No errors among these.
        let snap = snapshot(|w, run| {
            for i in 0..5 {
                w.push_log(log_at(run, i, LogLevel::Info));
            }
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        let view = view_of(&shell, cx);
        assert!(!present(cx, "logs-clear-filters"));
        click(cx, "logs-level-error");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 0);
        assert!(present(cx, "logs-clear-filters"));
        click(cx, "logs-clear-filters");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 5);
        assert!(!present(cx, "logs-clear-filters"));
    }

    #[gpui_kit::test]
    fn selection_round_trips_through_location(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_logs(cx, 30);
        click(cx, "log-row-7");
        cx.run_until_parked();
        let id = shell
            .read_with(cx, |s, _| s.current().selection)
            .expect("the click selects");
        match id {
            crate::nav::Selection::Log(id) => assert_eq!((id.run, id.seq), (RunId(1), 7)),
            other => panic!("unexpected {other:?}"),
        }
        // Navigating to another selection scrolls to it and selects it.
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.selection = Some(crate::nav::Selection::Log(crate::nav::EntryId {
            run: RunId(1),
            seq: 3,
        }));
        shell.update_in(cx, |s, w, cx| s.navigate(loc.clone(), w, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(3));
        // Back goes to the earlier selection.
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(7));
        // A selection the filters hide is cleared.
        click(cx, "logs-level-error");
        cx.run_until_parked();
        loc.selection = Some(crate::nav::Selection::Log(crate::nav::EntryId {
            run: RunId(1),
            seq: 3,
        }));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
    }

    #[gpui_kit::test]
    fn navigating_to_a_selection_scrolls_to_its_row(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_logs(cx, 500);
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.selection = Some(crate::nav::Selection::Log(crate::nav::EntryId {
            run: RunId(1),
            seq: 40,
        }));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        frame(cx);
        let range = view.read_with(cx, |v, _| v.rendered());
        assert!(range.contains(&40), "{range:?}");
    }

    #[gpui_kit::test]
    fn filter_work_runs_off_the_ui_thread(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_logs(cx, 100);
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 100);
        click(cx, "logs-level-error");
        // The click changed the filter, but the work has not run: the old rows stay.
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 100);
        assert!(view.read_with(cx, |v, _| v.is_filtering()));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 10);
        assert!(!view.read_with(cx, |v, _| v.is_filtering()));
    }

    #[gpui_kit::test]
    fn connected_with_no_logs_shows_the_empty_state(cx: &mut TestAppContext) {
        let (_shell, cx, h) = open_shell_fake(cx);
        h.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        assert!(exists(cx, "empty-state-logs"));
        assert!(!exists(cx, "logs-level-error"));
        // Focus stays on something that is on screen, so shell shortcuts keep working.
        press(cx, "cmd-4", "ctrl-4");
        assert!(exists(cx, "tool-page-network"));
    }

    #[gpui_kit::test]
    fn a_pathological_record_stays_cheap(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_log(log_at(run, 0, LogLevel::Info));
            let mut long = log_at(run, 1, LogLevel::Error);
            long.text = (0..10_000)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            w.push_log(long);
            w.push_log(LogInput {
                text: "é".repeat(200_000),
                ..log_at(run, 2, LogLevel::Info)
            });
        });
        let (shell, cx) = open_shell_snapshot(cx, snap);
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        let view = view_of(&shell, cx);
        click(cx, "log-row-1");
        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.row_count()), 3 + 200 + 1);
        assert!(rows_on_screen(cx) < 60);
    }

    #[gpui_kit::test]
    fn log_keys_do_nothing_outside_the_list(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_logs(cx, 10);
        let focus = shell.read_with(cx, |s, _| s.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
    }

    // ---- search, details, history and focus (Task 17) ----

    /// Records 0..n: every fifth says "burst of events", every seventh "Burst" with a capital;
    /// the rest are plain.
    fn search_snapshot(n: usize) -> Arc<Snapshot> {
        snapshot(|w, run| {
            for i in 0..n {
                let mut input = log_at(run, i as u64, LogLevel::Info);
                input.text = if i % 5 == 0 {
                    format!("burst of events {i}")
                } else if i % 7 == 0 {
                    format!("Burst capital {i}")
                } else {
                    format!("plain line {i}")
                };
                w.push_log(input);
            }
        })
    }

    fn open_search(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<LogsView>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, search_snapshot(n));
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        (shell, view, cx)
    }

    fn type_query(cx: &mut VisualTestContext, text: &str) {
        press(cx, "cmd-f", "ctrl-f");
        cx.simulate_input(text);
        cx.run_until_parked();
        frame(cx);
    }

    fn label_of(cx: &mut VisualTestContext, id: &str) -> Option<String> {
        frame(cx);
        let id = id.to_string();
        cx.update(|window, _| {
            window
                .try_find(id)
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|i| i.text()))
    }

    #[gpui_kit::test]
    fn cmd_f_focuses_search(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 20);
        assert!(!cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        press(cx, "cmd-f", "ctrl-f");
        assert!(cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        // Typing goes to the field, not to the list keys.
        cx.simulate_input("j");
        assert_eq!(view.read_with(cx, |v, cx| v.search_value(cx)).as_ref(), "j");
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
        assert!(present(cx, "logs-search"));
    }

    #[gpui_kit::test]
    fn typing_filters_rows_and_counts_matches(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 40);
        assert!(!present(cx, "logs-match-count"));
        type_query(cx, "burst");
        // Multiples of 5 say "burst"; "Burst" rows (multiples of 7 that are not of 5) match
        // case-insensitively too: 8 + 4.
        let n = view.read_with(cx, |v, _| v.filtered_len());
        assert_eq!(n, 12);
        assert_eq!(
            label_of(cx, "logs-match-count").as_deref(),
            Some("12 matches")
        );
        // Clearing the field brings every row back.
        cx.simulate_keystrokes("cmd-a backspace");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 40);
        assert!(!present(cx, "logs-match-count"));
    }

    #[gpui_kit::test]
    fn uppercase_makes_search_case_sensitive(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        type_query(cx, "Burst");
        // Only the capitalised rows: multiples of 7 that are not multiples of 5 (7, 14, 21,
        // 28, 42 is out of range): 7, 14, 21, 28.
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 4);
        assert_eq!(
            label_of(cx, "logs-match-count").as_deref(),
            Some("4 matches")
        );
    }

    #[gpui_kit::test]
    fn regex_toggle_enables_patterns(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        type_query(cx, r"events (10|20)$");
        // As plain text it matches nothing.
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 0);
        click(cx, "logs-regex");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 2);
        assert_eq!(
            label_of(cx, "logs-match-count").as_deref(),
            Some("2 matches")
        );
        click(cx, "logs-regex");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 0);
    }

    #[gpui_kit::test]
    fn invalid_regex_shows_error_and_keeps_rows(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        click(cx, "logs-regex");
        type_query(cx, "burst");
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 12);
        cx.simulate_input("(");
        cx.run_until_parked();
        frame(cx);
        assert!(present(cx, "logs-search-error"));
        assert!(!present(cx, "logs-match-count"));
        assert_eq!(
            view.read_with(cx, |v, _| v.filtered_len()),
            12,
            "the previous rows stay"
        );
        let message = label_of(cx, "logs-search-error").unwrap();
        assert!(
            !message.is_empty() && !message.contains('\n'),
            "{message:?}"
        );
        // Fixing the pattern clears the error.
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert!(!present(cx, "logs-search-error"));
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 12);
        // A hostile pattern is still bounded.
        cx.simulate_input("(a+)+$");
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn typing_quickly_cancels_the_previous_scan(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search("bur", false, window, cx));
        });
        let first = view.read_with(cx, |v, _| v.last_cancel());
        assert!(!first.load(Ordering::Relaxed));
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search("burst", false, window, cx));
        });
        assert!(
            first.load(Ordering::Relaxed),
            "the first scan was told to stop"
        );
        let second = view.read_with(cx, |v, _| v.last_cancel());
        assert!(!second.load(Ordering::Relaxed));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 12);
        assert_eq!(
            view.read_with(cx, |v, cx| v.search_value(cx)).as_ref(),
            "burst"
        );
    }

    #[gpui_kit::test]
    fn set_search_applies_text_and_mode(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search(r"events 1\d", true, window, cx));
        });
        cx.run_until_parked();
        // 10 and 15 are multiples of five with a two-digit number starting with 1.
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 2);
    }

    #[gpui_kit::test]
    fn a_late_first_phase_does_not_replace_the_full_result(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        type_query(cx, "burst");
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 12);
        let before = view.read_with(cx, |v, _| v.first_phases());
        view.update(cx, |v, cx| v.apply_first_for_test(vec![1, 2], cx));
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 12);
        assert_eq!(view.read_with(cx, |v, _| v.first_phases()), before);
    }

    #[gpui_kit::test]
    fn the_searching_label_waits_for_the_loading_delay(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search("burst", false, window, cx));
        });
        // The scan has not run; the delay has not passed.
        assert!(view.read_with(cx, |v, _| v.is_filtering()));
        assert!(!present(cx, "logs-searching"));
        // The delay timer is Task 16's (the scan finishes inside `run_until_parked`, so it
        // cannot be held open); here the label's rendering is checked once it has fired.
        view.update(cx, |v, cx| v.show_skeleton_for_test(cx));
        assert!(view.read_with(cx, |v, _| v.searching_for_test()));
        assert!(present(cx, "logs-searching"));
        cx.run_until_parked();
        assert!(!present(cx, "logs-searching"));
        assert!(present(cx, "logs-match-count"));
    }

    #[gpui_kit::test]
    fn matches_are_highlighted_in_rendered_rows(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        type_query(cx, "burst");
        // Highlighting must not change what the rows say or how many there are.
        assert_eq!(rows_on_screen(cx), 12);
        let ranges = view.read_with(cx, |v, _| v.matcher_ranges("a burst b"));
        assert_eq!((ranges.len(), ranges[0].clone()), (1, 2..7));
    }

    fn details_snapshot() -> Arc<Snapshot> {
        snapshot(|w, run| {
            let mut a = log_at(run, 12_345, LogLevel::Warning);
            a.text = "alpha\nbeta\ngamma".to_string();
            a.logger = "app.net".to_string();
            a.source = fdt_store::logs::LogSource::Stdout;
            w.push_log(a);
            let mut b = log_at(run, 20_000, LogLevel::Info);
            b.text = "second record".to_string();
            b.clamped = true;
            w.push_log(b);
            let mut c = log_at(run, 21_000, LogLevel::Error);
            c.source = fdt_store::logs::LogSource::FlutterError;
            c.text = "The following assertion was thrown".to_string();
            c.error = Some(fdt_store::logs::ErrorDetail {
                description: "Exception caught by widgets library".to_string(),
                summary: "A RenderFlex overflowed by 12 pixels".to_string(),
                widget: Some("Row".to_string()),
                location: Some("file:///app/lib/main.dart:10:5".to_string()),
                stack: vec![
                    "#0 build (package:app/main.dart:10)".to_string(),
                    "#1 Element.rebuild (package:flutter/element.dart:1)".to_string(),
                    "#2 runApp (package:flutter/binding.dart:5)".to_string(),
                ],
                rendered: String::new(),
            });
            w.push_log(c);
            let mut d = log_at(run, 22_000, LogLevel::Info);
            d.text = (0..10_000)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            w.push_log(d);
        })
    }

    fn open_details_for(
        cx: &mut TestAppContext,
        row: usize,
    ) -> (
        gpui_kit::Entity<crate::shell::MainWindow>,
        gpui_kit::Entity<LogsView>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, details_snapshot());
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        let view = view_of(&shell, cx);
        click(cx, &format!("log-row-{row}"));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        frame(cx);
        (shell, view, cx)
    }

    #[gpui_kit::test]
    fn details_show_full_text_and_metadata(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_details_for(cx, 0);
        assert!(present(cx, "logs-details"));
        assert!(view.read_with(cx, |v, _| v.details_open()));
        assert_eq!(label_of(cx, "details-time").as_deref(), Some("0:12.345"));
        assert_eq!(label_of(cx, "details-level").as_deref(), Some("Warning"));
        assert_eq!(label_of(cx, "details-source").as_deref(), Some("Stdout"));
        assert_eq!(label_of(cx, "details-logger").as_deref(), Some("app.net"));
        assert_eq!(label_of(cx, "details-run").as_deref(), Some("Run 1"));
        assert!(!present(cx, "details-clamped"));
        // All of the message, line by line, not the 512-character row cut.
        for (i, line) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
            assert_eq!(
                label_of(cx, &format!("details-line-{i}")).as_deref(),
                Some(line)
            );
        }
        assert!(!present(cx, "details-line-3"));
        assert!(!present(cx, "details-section-summary"));
        // The pane follows the selection once open, including the "before connect" note.
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        frame(cx);
        assert_eq!(label_of(cx, "details-level").as_deref(), Some("Info"));
        assert_eq!(
            label_of(cx, "details-clamped").as_deref(),
            Some("Before fdt connected")
        );
        assert_eq!(
            label_of(cx, "details-line-0").as_deref(),
            Some("second record")
        );
    }

    #[gpui_kit::test]
    fn a_ten_thousand_line_record_stays_virtualised_in_details(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_details_for(cx, 3);
        assert_eq!(view.read_with(cx, |v, _| v.details_lines()), 10_000);
        let built = cx.update(|window, _| {
            (0..400)
                .filter(|i| window.try_find(format!("details-line-{i}")).is_some())
                .count()
        });
        assert!(built > 0 && built < 60, "{built} detail rows built");
    }

    #[gpui_kit::test]
    fn details_show_flutter_error_sections(cx: &mut TestAppContext) {
        let (_shell, _view, cx) = open_details_for(cx, 2);
        assert_eq!(
            label_of(cx, "details-section-summary").as_deref(),
            Some("Summary")
        );
        assert_eq!(
            label_of(cx, "details-summary").as_deref(),
            Some("A RenderFlex overflowed by 12 pixels")
        );
        assert_eq!(
            label_of(cx, "details-section-widget").as_deref(),
            Some("Caused by widget")
        );
        assert_eq!(label_of(cx, "details-widget").as_deref(), Some("Row"));
        assert_eq!(
            label_of(cx, "details-location").as_deref(),
            Some("file:///app/lib/main.dart:10:5")
        );
        assert_eq!(
            label_of(cx, "details-section-stack").as_deref(),
            Some("Stack trace")
        );
        // One row per frame.
        for i in 0..3 {
            assert!(present(cx, &format!("details-stack-{i}")), "frame {i}");
        }
        assert!(!present(cx, "details-stack-3"));
        assert_eq!(
            label_of(cx, "details-source").as_deref(),
            Some("Flutter error")
        );
    }

    #[gpui_kit::test]
    fn double_click_opens_details(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, details_snapshot());
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        let view = view_of(&shell, cx);
        frame(cx);
        cx.update(|window, cx| window.double_click("log-row-1", cx));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.details_open()));
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(1));
        // The open_details action (the palette's route) works the same.
        view.update(cx, |v, cx| {
            v.close_details(cx);
        });
        assert!(!view.read_with(cx, |v, _| v.details_open()));
        view.update(cx, |v, cx| v.open_details(cx));
        assert!(view.read_with(cx, |v, _| v.details_open()));
    }

    #[gpui_kit::test]
    fn copy_and_copy_as_json_write_the_clipboard(cx: &mut TestAppContext) {
        let (_shell, _view, cx) = open_details_for(cx, 0);
        click(cx, "logs-copy-text");
        assert_eq!(clipboard(cx).as_deref(), Some("alpha\nbeta\ngamma"));
        click(cx, "logs-copy-json");
        assert_eq!(
            clipboard(cx).as_deref(),
            Some(
                r#"{"time":"0:12.345","level":"Warning","source":"Stdout","logger":"app.net","run":1,"message":"alpha\nbeta\ngamma"}"#
            )
        );
    }

    #[gpui_kit::test]
    fn context_menu_copies(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell_snapshot(cx, details_snapshot());
        press(cx, "cmd-5", "ctrl-5");
        cx.run_until_parked();
        let _ = view_of(&shell, cx);
        frame(cx);
        cx.update(|window, cx| window.right_click("log-row-1", cx));
        cx.run_until_parked();
        frame(cx);
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(clipboard(cx).as_deref(), Some("second record"));
        cx.update(|window, cx| window.right_click("log-row-1", cx));
        cx.run_until_parked();
        frame(cx);
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        let json = clipboard(cx).unwrap();
        assert!(
            json.starts_with(r#"{"time":"0:20.000","level":"Info""#),
            "{json}"
        );
        assert!(json.ends_with(r#""message":"second record"}"#), "{json}");
    }

    #[gpui_kit::test]
    fn escape_closes_details_first(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_details_for(cx, 1);
        assert!(view.read_with(cx, |v, _| v.details_open()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, _| v.details_open()));
        assert_eq!(
            view.read_with(cx, |v, _| v.selected()),
            Some(1),
            "still selected"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
    }

    #[gpui_kit::test]
    fn keyboard_movement_replaces_the_history_entry(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_logs(cx, 30);
        let before = shell.read_with(cx, |s, _| s.history().back_len());
        click(cx, "log-row-3");
        cx.run_until_parked();
        let after_click = shell.read_with(cx, |s, _| s.history().back_len());
        assert_eq!(after_click, before + 1, "a click pushes");
        for _ in 0..10 {
            cx.simulate_keystrokes("j");
        }
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(13));
        let after_keys = shell.read_with(cx, |s, _| s.history().back_len());
        assert!(
            after_keys <= after_click + 1,
            "{before} {after_click} {after_keys}"
        );
        assert_eq!(after_keys, after_click, "movement does not push");
        // The location follows the selection.
        match shell.read_with(cx, |s, _| s.current().selection) {
            Some(crate::nav::Selection::Log(id)) => assert_eq!(id.seq, 13),
            other => panic!("{other:?}"),
        }
        // Back goes to the clicked row's entry's predecessor, not through ten steps.
        press(cx, "cmd-[", "ctrl-[");
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
    }

    #[gpui_kit::test]
    fn status_changes_and_range_drags_keep_focus_in_the_search_field(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 50);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        press(cx, "cmd-f", "ctrl-f");
        cx.simulate_input("rec");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        // The app stops recording: a status change.
        h.status.send(connected("demo", false)).unwrap();
        cx.run_until_parked();
        frame(cx);
        assert!(
            cx.update(|window, cx| view.read(cx).search_focused(window, cx)),
            "a status change keeps focus in the field"
        );
        // A range drag navigates within the page.
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.range = Some(crate::nav::TimeRange::new(
            SessionTime(0),
            SessionTime(10 * SEC),
        ));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        frame(cx);
        assert!(cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        cx.simulate_input("o");
        assert_eq!(
            view.read_with(cx, |v, cx| v.search_value(cx)).as_ref(),
            "reco"
        );
    }

    #[gpui_kit::test]
    fn a_replaced_store_resets_expansion_and_selection(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 30);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        click(cx, "log-row-5");
        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.is_expanded(5)));
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(5));
        // A new session: different records, a later generation.
        let (mut w2, run2) = writer();
        feed(&mut w2, run2, 1_000, 1_030);
        let _ = w2.publish();
        feed(&mut w2, run2, 1_030, 1_040);
        h.snaps.send(w2.publish()).unwrap();
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, _| v.is_expanded(5)));
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 40);
    }

    #[gpui_kit::test]
    fn clear_filters_also_clears_search_and_range(cx: &mut TestAppContext) {
        let (shell, view, cx) = open_search(cx, 40);
        let mut loc = shell.read_with(cx, |s, _| s.current().clone());
        loc.range = Some(crate::nav::TimeRange::new(
            SessionTime(100 * SEC),
            SessionTime(200 * SEC),
        ));
        shell.update_in(cx, |s, w, cx| s.navigate(loc, w, cx));
        cx.run_until_parked();
        type_query(cx, "burst");
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 0);
        click(cx, "logs-clear-filters");
        cx.run_until_parked();
        assert_eq!(shell.read_with(cx, |s, _| s.current().range), None);
        assert_eq!(view.read_with(cx, |v, cx| v.search_value(cx)).as_ref(), "");
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 40);
        assert!(!present(cx, "logs-match-count"));
    }

    #[gpui_kit::test]
    fn following_pauses_when_rows_arrive_away_from_the_end(cx: &mut TestAppContext) {
        let (shell, cx, h) = open_shell_fake(cx);
        let (mut w, run) = writer();
        feed(&mut w, run, 0, 100);
        h.status.send(connected("demo", true)).unwrap();
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        frame(cx);
        let view = view_of(&shell, cx);
        assert!(view.read_with(cx, |v, _| v.follow().following));
        // Scrolled away from the end by something other than the wheel.
        view.read_with(cx, |v, _| v.scroll_to_top());
        frame(cx);
        feed(&mut w, run, 100, 104);
        h.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        let f = view.read_with(cx, |v, _| v.follow());
        assert!(!f.following);
        assert_eq!(f.unseen, 4);
        assert!(present(cx, "logs-unseen"));
        // Scrolling back down to the end resumes following.
        cx.update(|window, cx| {
            window.scroll(
                "logs-list",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                    gpui_kit::px(0.),
                    gpui_kit::px(-100_000.),
                )),
                cx,
            )
        });
        cx.run_until_parked();
        frame(cx);
        let f = view.read_with(cx, |v, _| v.follow());
        assert!(f.following && f.unseen == 0, "{f:?}");
        assert!(!present(cx, "logs-unseen"));
    }

    #[gpui_kit::test]
    fn escape_in_the_search_field_moves_focus_to_the_list(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_details_for(cx, 1);
        press(cx, "cmd-f", "ctrl-f");
        cx.simulate_input("rec");
        assert!(cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        frame(cx);
        assert!(!cx.update(|window, cx| view.read(cx).search_focused(window, cx)));
        assert_eq!(
            view.read_with(cx, |v, cx| v.search_value(cx)).as_ref(),
            "rec"
        );
        assert!(
            view.read_with(cx, |v, _| v.details_open()),
            "details stay open"
        );
        // Now the usual order: details, then selection.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, _| v.details_open()));
        assert_eq!(view.read_with(cx, |v, _| v.selected()), Some(1));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.selected()), None);
    }

    #[gpui_kit::test]
    fn empty_queries_do_not_rescan(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        let flag = view.read_with(cx, |v, _| v.last_cancel());
        click(cx, "logs-regex");
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search("", true, window, cx));
        });
        assert!(!flag.load(Ordering::Relaxed), "nothing was cancelled");
        assert!(Arc::ptr_eq(
            &flag,
            &view.read_with(cx, |v, _| v.last_cancel())
        ));
        // Going from a query back to empty is a change.
        type_query(cx, "burst");
        assert!(flag.load(Ordering::Relaxed));
    }

    #[gpui_kit::test]
    fn over_long_queries_show_an_error_and_keep_rows(cx: &mut TestAppContext) {
        let (_shell, view, cx) = open_search(cx, 40);
        let long = "a".repeat(1_001);
        cx.update(|window, cx| {
            view.update(cx, |v, cx| v.set_search(&long, false, window, cx));
        });
        cx.run_until_parked();
        assert_eq!(
            label_of(cx, "logs-search-error").as_deref(),
            Some("Search is limited to 1,000 characters.")
        );
        assert_eq!(view.read_with(cx, |v, _| v.filtered_len()), 40);
    }
}
