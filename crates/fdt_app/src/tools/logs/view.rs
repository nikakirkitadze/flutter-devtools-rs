//! The Logs list: a virtualised `uniform_list` over the filtered records, with inline
//! expansion, follow mode and selection that syncs with the shell's location.
//!
//! Filtering and counting run on a background thread ([`compute`]); the UI thread only maps
//! visible rows. At most one job runs at a time; inputs that arrive meanwhile are picked up when
//! it finishes.

use super::model::{Follow, RowMap, RowRef, display_line};
use crate::actions::{
    Collapse, Expand, FollowLatest, GoToFirst, OpenDetails, SelectNext, SelectPrevious,
    ToggleExpand,
};
use crate::keymap::LOGS_LIST_CONTEXT;
use crate::nav::{EntryId, TimeRange};
use crate::session_model::SessionModel;
use crate::shell::strip::format_offset;
use fdt_store::Snapshot;
use fdt_store::log_query::{LogFilter, filter_logs};
use fdt_store::logs::{LogLevel, LogRow, LogSource};
use fdt_store::time::SessionTime;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, motion, radius, row, space, table};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Render,
    ScrollEvent, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, TestSupportExt as _, UniformListScrollHandle, Window, div, relative,
    uniform_list,
};
use std::ops::Range;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Rows built by the list's item closure, for the "only visible rows" test.
    pub(crate) static ITEMS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What the Logs list tells the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogsEvent {
    /// The selected record changed (`None`: cleared).
    Selected(Option<EntryId>),
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
}

/// First and last record times, to tell a grown store from a replaced one.
fn fingerprint(snap: &Snapshot) -> Option<(SessionTime, SessionTime)> {
    let n = snap.logs.len();
    (n > 0).then(|| (snap.logs.row(0).time, snap.logs.row(n - 1).time))
}

/// Filters `snap` and counts levels in the filter's range. O(records) in the worst case; runs
/// off the UI thread. With a `prior` result of the same filter over a store this one extends,
/// only the new records are scanned.
fn compute(
    snap: &Snapshot,
    filter: &LogFilter,
    version: u64,
    prior: Option<(Applied, Arc<[u32]>)>,
) -> JobResult {
    let counts = snap.logs.level_counts(filter.range);
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
        return JobResult {
            filtered,
            counts,
            incremental: true,
        };
    }
    JobResult {
        filtered: filter_logs(&snap.logs, filter, 0).into(),
        counts,
        incremental: false,
    }
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
    selected: Option<u32>,
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
    _session: Subscription,
}

impl EventEmitter<LogsEvent> for LogsView {}

impl LogsView {
    pub fn new(
        session: Entity<SessionModel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observed = cx.observe(&session, |this, _, cx| this.request(cx));
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
                    Some((key, snap, this.filter.clone(), prior))
                });
                let Ok(Some((key, snap, filter, prior))) = next else {
                    break;
                };
                let job_snap = snap.clone();
                let result = cx
                    .background_spawn(
                        async move { compute(&job_snap, &filter, key.version, prior) },
                    )
                    .await;
                done = Some(key);
                let applied = this.update(cx, |this, cx| {
                    // A result for an older filter is dropped. An older snapshot is not: the
                    // store only grows, and the next round picks up what was missed.
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

    /// Takes a finished job's result.
    fn apply(&mut self, key: Key, snap: Arc<Snapshot>, result: JobResult, cx: &mut Context<Self>) {
        let at_end = self.scroll.is_scrolled_to_end();
        let first_load = self.applied.is_none();
        let old_rows = self.rows.row_count();
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
            cx.emit(LogsEvent::Selected(None));
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

    /// Whether the rows on screen are for the current filter and nothing is being computed.
    fn settled(&self) -> bool {
        !self.running && self.applied.is_some_and(|a| a.key.version == self.version)
    }

    fn bump(&mut self, cx: &mut Context<Self>) {
        self.version += 1;
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

    /// Resets every filter of the list; the shared time range is left alone.
    pub fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.filter = LogFilter {
            range: self.filter.range,
            ..LogFilter::default()
        };
        self.bump(cx);
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

    fn select_seq(&mut self, seq: u32, cx: &mut Context<Self>) {
        self.selected = Some(seq);
        self.follow.on_select();
        self.reveal(seq, ScrollStrategy::Nearest);
        let id = self.snap.logs.row(seq as usize).id;
        cx.emit(LogsEvent::Selected(Some(id)));
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
        self.select_seq(filtered[target], cx);
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
    pub fn open_details(&mut self, _cx: &mut Context<Self>) {}

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
            self.select_seq(first, cx);
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
fn format_time(t: SessionTime) -> String {
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
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.select_seq(seq, cx);
                }),
            );
        let indent = table::COL_TIME + table::COL_LEVEL + table::COL_LOGGER + space::S8 * 3.0;
        match rr {
            RowRef::Record { .. } => base
                .child(self.record_cells(&rec, seq, t, cx))
                .into_any_element(),
            RowRef::Continuation { line, .. } => {
                let text = rec.text.split('\n').nth(line as usize).unwrap_or("");
                let (shown, cut) = display_line(text);
                let shown = if cut {
                    format!("{shown}\u{2026}")
                } else {
                    shown.to_string()
                };
                base.text_color(Hsla::from(t.text_muted))
                    .child(div().flex_none().w(indent))
                    .child(div().flex_1().min_w_0().truncate().child(shown))
                    .into_any_element()
            }
            RowRef::ShowAll { .. } => base
                .text_color(Hsla::from(t.accent))
                .child(div().flex_none().w(indent))
                .child(format!("Show all {} lines in details", rec.line_count))
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
            .child(div().flex_1().min_w_0().truncate().child(message))
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
                }
            }))
            .child(content)
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
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx)))
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
        if std::mem::take(&mut self.focus_wanted) {
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(self.list_area(body, cx))
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
}
