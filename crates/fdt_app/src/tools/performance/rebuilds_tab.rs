//! The Rebuilds tab: which widgets rebuilt, in the selected frame or the shared time range.
//!
//! The counts come from widget-build timeline slices, which exist only while the app traces
//! widget builds. A switch in the toolbar turns that on and off; it shows the app's actual state,
//! so it moves when the run reports the change, not when it is clicked. The counts are computed
//! off the UI thread; the name filter only walks the distinct widgets, never the session.

use crate::actions::ToggleTraceWidgetBuilds;
use crate::nav::TimeRange;
use crate::session_model::SessionModel;
use crate::tools::network::model::format_duration_ns;
use fdt_analysis::rebuilds::{RebuildTally, WidgetCount, frame_rebuilds, tally_rebuilds};
use fdt_capture::ConnStatus;
use fdt_store::Snapshot;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, motion, row, space, table};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::{
    Action as _, AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, Window, div,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What the toggle's note says.
pub const OVERHEAD_NOTE: &str = "Adds about 30 % to build time while on.";
/// The tooltip of the switch while no app is connected.
pub const DISCONNECTED_TIP: &str = "Connect to an app to trace widget builds.";
/// Shown when the store stopped keeping raw widget builds.
pub const CAP_NOTE: &str =
    "Build detail stopped after 8,000,000 widget builds. Counts per frame are kept.";
const OFF_STATE: &str =
    "Widget builds aren't traced. Turn on Trace widget builds to count rebuilds per widget.";
const NONE_IN_SCOPE: &str = "No widgets rebuilt in this range.";
const NO_MATCH: &str = "No widgets match your filter.";

/// The least time between two recomputes for the same scope while snapshots keep arriving: the
/// counts are a summary, and fdt shares the machine with the app it profiles.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2); // token-check: allow: throttle, not a visual duration

/// What the counts are over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The selected frame's seq.
    Frame(usize),
    Range(TimeRange),
    Whole,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Frame(_) => "Selected frame",
            Scope::Range(_) => "Selected range",
            Scope::Whole => "Whole session",
        }
    }
}

/// What a job was computed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    session: u64,
    generation: u64,
    scope: Scope,
}

impl Key {
    fn same_scope(&self, other: &Key) -> bool {
        self.session == other.session && self.scope == other.scope
    }
}

struct Computed {
    widgets: Arc<[WidgetCount]>,
}

/// A tally of a range or the whole session, with what it was computed for.
struct Kept {
    session: u64,
    scope: Scope,
    tally: Arc<RebuildTally>,
}

/// The widgets of `scope`, busiest first, and for a range or the whole session the tally the
/// next snapshot can start from (`prev`, an earlier tally of the same scope and session: only
/// the frames added since are walked). `None` when `cancel` was set.
fn compute(
    snap: &Snapshot,
    scope: Scope,
    prev: Option<&RebuildTally>,
    cancel: &AtomicBool,
) -> Option<(Option<RebuildTally>, Vec<WidgetCount>)> {
    let range = match scope {
        Scope::Frame(seq) => {
            let widgets = frame_rebuilds(snap, seq).map_or_else(Vec::new, |f| f.widgets);
            return Some((None, widgets));
        }
        Scope::Range(r) => Some(r),
        Scope::Whole => None,
    };
    let (tally, widgets) = tally_rebuilds(snap, range, prev, cancel)?;
    Some((Some(tally), widgets))
}

/// Indices into `all` of the widgets whose name contains `needle` (lower case, may be empty).
fn filtered(all: &[WidgetCount], needle: &str) -> Vec<u32> {
    all.iter()
        .enumerate()
        .filter(|(_, w)| needle.is_empty() || w.name.to_lowercase().contains(needle))
        .map(|(i, _)| i as u32)
        .collect()
}

struct RebuildsTable {
    all: Arc<[WidgetCount]>,
    shown: Vec<u32>,
}

impl RebuildsTable {
    fn text(&self, row: usize, col: usize) -> String {
        let Some(w) = self.shown.get(row).and_then(|&i| self.all.get(i as usize)) else {
            return String::new();
        };
        match col {
            0 => w.name.to_string(),
            1 => w.builds.to_string(),
            2 => w.frames.to_string(),
            3 => w.max_in_frame.to_string(),
            _ => format_duration_ns(w.self_ns),
        }
    }
}

impl TableDelegate for RebuildsTable {
    fn columns_count(&self, _: &App) -> usize {
        5
    }

    fn rows_count(&self, _: &App) -> usize {
        self.shown.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let num = |id, label, w| Column::new(id, label).width(w).text_right();
        match col_ix {
            0 => Column::new("widget", "Widget").width(table::COL_WIDGET),
            1 => num("builds", "Builds", table::COL_COUNT),
            2 => num("frames", "Frames", table::COL_COUNT),
            3 => num("max", "Max in a frame", table::COL_COUNT),
            _ => num("self", "Self time", table::COL_MS),
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
        div()
            .w_full()
            .truncate()
            .text_size(font::DENSE)
            .font_features(font::tabular_figures())
            .text_color(Hsla::from(if col_ix == 0 { t.text } else { t.text_muted }))
            .child(self.text(row_ix, col_ix))
    }
}

pub struct RebuildsTab {
    session: Entity<SessionModel>,
    table: Entity<TableState<RebuildsTable>>,
    input: Entity<InputState>,
    /// The filter, lower case.
    filter: String,
    range: Option<TimeRange>,
    /// The selected frame's seq.
    selection: Option<usize>,
    active: bool,
    result: Option<Arc<Computed>>,
    /// The latest range or whole-session tally, which the next recompute of that scope extends.
    kept: Option<Kept>,
    started: Option<Key>,
    last_start: Option<Instant>,
    trailing: Option<Task<()>>,
    skeleton: bool,
    cancel: Arc<AtomicBool>,
    job: Option<Task<()>>,
    timer: Option<Task<()>>,
    /// Jobs started (read by tests).
    #[cfg(test)]
    runs: usize,
    _subs: Vec<Subscription>,
}

impl RebuildsTab {
    pub fn new(session: Entity<SessionModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by widget name"));
        let table = cx.new(|cx| {
            TableState::new(
                RebuildsTable {
                    all: Arc::from(Vec::new()),
                    shown: Vec::new(),
                },
                window,
                cx,
            )
            .loop_selection(false)
            .col_movable(false)
            .sortable(false)
        });
        let subs = vec![
            cx.observe(&session, |this, _, cx| {
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
        ];
        Self {
            session,
            table,
            input,
            filter: String::new(),
            range: None,
            selection: None,
            active: false,
            result: None,
            kept: None,
            started: None,
            last_start: None,
            trailing: None,
            skeleton: false,
            cancel: Arc::new(AtomicBool::new(false)),
            job: None,
            timer: None,
            #[cfg(test)]
            runs: 0,
            _subs: subs,
        }
    }

    /// Whether the Rebuilds tab is on screen. Hidden, nothing is computed; shown, it catches up.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            if active {
                self.sync(cx);
                cx.notify();
            }
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

    /// The selected frame's seq, from the location.
    pub fn set_selection(&mut self, seq: Option<usize>, cx: &mut Context<Self>) {
        if self.selection != seq {
            self.selection = seq;
            self.sync(cx);
            cx.notify();
        }
    }

    pub fn scope(&self) -> Scope {
        match (self.selection, self.range) {
            (Some(seq), _) => Scope::Frame(seq),
            (None, Some(r)) => Scope::Range(r),
            (None, None) => Scope::Whole,
        }
    }

    /// Sets the filter field, as typing would.
    pub fn set_filter_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
        self.filter_changed(cx);
    }

    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_lowercase();
        if value != self.filter {
            self.filter = value;
            self.show(cx);
            cx.notify();
        }
    }

    /// Whether the app is connected and its latest run traces widget builds now.
    pub fn tracing_on(&self, cx: &App) -> bool {
        let session = self.session.read(cx);
        matches!(session.status(), ConnStatus::Connected { .. })
            && session.snapshot().runs.last().is_some_and(|r| r.tracing)
    }

    fn connected(&self, cx: &App) -> bool {
        matches!(self.session.read(cx).status(), ConnStatus::Connected { .. })
    }

    fn key(&self, cx: &App) -> Key {
        let snap = self.session.read(cx).snapshot();
        Key {
            session: snap.session,
            generation: snap.generation,
            scope: self.scope(),
        }
    }

    /// Starts a recompute when the scope or the snapshot changed. A change of the snapshot alone
    /// waits out `REFRESH_INTERVAL` since the last start; a new scope starts at once and cancels
    /// the running job.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let key = self.key(cx);
        if self.started == Some(key) {
            return;
        }
        let same_scope = self.started.is_some_and(|k| k.same_scope(&key));
        if same_scope {
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
            // The old scope's rows are not this scope's; show nothing until the new ones land.
            self.result = None;
            self.show(cx);
            self.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(motion::LOADING_DELAY).await;
                this.update(cx, |this, cx| {
                    if this.result.is_none() {
                        this.skeleton = true;
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
        #[cfg(test)]
        {
            self.runs += 1;
        }
        self.trailing = None;
        self.last_start = Some(cx.background_executor().now());
        self.started = Some(key);
        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let snap = self.session.read(cx).snapshot().clone();
        let prev = self
            .kept
            .as_ref()
            .filter(|k| k.session == key.session && k.scope == key.scope)
            .map(|k| k.tally.clone());
        self.job = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_spawn(
                    async move { compute(&snap, key.scope, prev.as_deref(), &cancel) },
                )
                .await;
            this.update(cx, |this, cx| {
                // A newer job replaced this one.
                if this.started == Some(key)
                    && let Some((tally, widgets)) = out
                {
                    if let Some(tally) = tally {
                        this.kept = Some(Kept {
                            session: key.session,
                            scope: key.scope,
                            tally: Arc::new(tally),
                        });
                    }
                    this.result = Some(Arc::new(Computed {
                        widgets: widgets.into(),
                    }));
                    this.skeleton = false;
                    this.timer = None;
                    this.show(cx);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Hands the table the widgets that pass the filter.
    fn show(&mut self, cx: &mut Context<Self>) {
        let all: Arc<[WidgetCount]> = self
            .result
            .as_ref()
            .map_or_else(|| Arc::from(Vec::new()), |r| r.widgets.clone());
        let shown = filtered(&all, &self.filter);
        self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.all = all;
            d.shown = shown;
            cx.notify();
        });
    }

    #[cfg(test)]
    pub fn is_settled(&self, cx: &App) -> bool {
        self.started == Some(self.key(cx)) && self.result.is_some()
    }

    #[cfg(test)]
    pub fn runs(&self) -> usize {
        self.runs
    }

    /// The table's text, one `Vec` of cells per row shown.
    #[cfg(test)]
    pub fn cells(&self, cx: &App) -> Vec<Vec<String>> {
        let table = self.table.read(cx);
        let d = table.delegate();
        (0..d.shown.len())
            .map(|r| (0..5).map(|c| d.text(r, c)).collect())
            .collect()
    }

    #[cfg(test)]
    pub fn switch_disabled(&self, cx: &App) -> bool {
        !self.connected(cx)
    }
}

impl RebuildsTab {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let connected = self.connected(cx);
        let mut toggle = Switch::new("rebuilds-trace-toggle")
            .checked(self.tracing_on(cx))
            .label("Trace widget builds")
            .on_change(|_, window, cx| {
                window.dispatch_action(ToggleTraceWidgetBuilds.boxed_clone(), cx)
            });
        if !connected {
            toggle =
                gpui_kit::component::Disableable::disabled(toggle, true).tooltip(DISCONNECTED_TIP);
        }
        div()
            .id("rebuilds-toolbar")
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
            .child(toggle)
            .child(
                div()
                    .id("rebuilds-overhead-note")
                    .test_support()
                    .aria_label(OVERHEAD_NOTE)
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(OVERHEAD_NOTE),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("rebuilds-scope")
                    .test_support()
                    .aria_label(self.scope().label())
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(self.scope().label()),
            )
            .child(
                div()
                    .id("rebuilds-filter")
                    .test_support()
                    .flex_none()
                    .w(layout::SEARCH_WIDTH)
                    .min_w(layout::SEARCH_MIN)
                    .child(Input::new(&self.input).cleanable(true)),
            )
            .into_any_element()
    }

    fn message(&self, id: &'static str, text: &'static str, cx: &App) -> AnyElement {
        let t = cx.tokens();
        div()
            .id(id)
            .test_support()
            .aria_label(text)
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .px(space::S16)
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text_muted))
            .child(text)
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = &self.result else {
            if !self.skeleton {
                return div().flex_1().into_any_element();
            }
            return div()
                .id("rebuilds-loading")
                .test_support()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(space::S8)
                .p(space::S16)
                .children((0..6).map(|_| Skeleton::new().h(row::TABLE)))
                .into_any_element();
        };
        if result.widgets.is_empty() {
            return if self.tracing_on(cx) {
                self.message("rebuilds-empty", NONE_IN_SCOPE, cx)
            } else {
                self.message("rebuilds-off", OFF_STATE, cx)
            };
        }
        if self.table.read(cx).delegate().shown.is_empty() {
            return self.message("rebuilds-no-match", NO_MATCH, cx);
        }
        div()
            .id("rebuilds-table")
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
    }

    /// The note shown once the store stopped keeping raw widget builds.
    fn render_cap_note(&self, cx: &App) -> Option<AnyElement> {
        if self.session.read(cx).snapshot().timeline.drops().build_cap == 0 {
            return None;
        }
        let t = cx.tokens();
        Some(
            div()
                .id("rebuilds-cap-note")
                .test_support()
                .aria_label(CAP_NOTE)
                .flex_none()
                .w_full()
                .px(space::S16)
                .py(space::S4)
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(CAP_NOTE)
                .into_any_element(),
        )
    }
}

impl Render for RebuildsTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rebuilds-tab")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .children(self.render_cap_note(cx))
            .child(self.render_body(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowPerformance;
    use crate::nav::{Location, Page, Selection, Tool};
    use crate::test_support::{
        connected, frame_at, open_shell_fake, open_shell_snapshot, snapshot, writer,
    };
    use crate::tools::performance::PerfTab;
    use fdt_store::SessionWriter;
    use fdt_store::frames::UiPhases;
    use fdt_store::time::{RunId, SessionTime};
    use fdt_store::timeline::{Slice, SliceCat, TrackKind};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};

    const MS: u64 = 1_000_000;

    /// Frame `number` at `start_ms` whose UI thread builds `CartScreen > {CartBadge, ProductGrid
    /// > tiles x ProductTile > Card > Text}`.
    fn cart_frame(w: &mut SessionWriter, run: RunId, number: u64, start_ms: u64, tiles: u32) {
        w.push_frame(frame_at(run, number, start_ms, 30, 1));
        let begin = start_ms * MS;
        w.set_frame_ui(
            run,
            number,
            UiPhases {
                begin: SessionTime(begin),
                ui_us: 20_000,
                build_us: 15_000,
                layout_us: 2000,
                paint_us: 2000,
                composite_us: 500,
                finalize_us: 500,
                widgets_built: Some(2 + 3 * tiles),
            },
        );
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let put = |w: &mut SessionWriter, depth: u16, name: &str, at_us: u64, dur_us: u64| {
            let s = Slice {
                start: SessionTime(begin + at_us * 1000),
                dur_ns: dur_us * 1000,
                name: w.intern_name(name),
                cat: SliceCat::Dart,
            };
            assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
        };
        put(w, 0, "VsyncProcessCallback", 0, 20_000);
        put(w, 1, "Animator::BeginFrame", 0, 20_000);
        put(w, 2, "BUILD", 0, 15_000);
        put(w, 3, "CartScreen", 0, 14_000);
        put(w, 4, "CartBadge", 0, 500);
        put(w, 4, "ProductGrid", 1000, 12_000);
        for i in 0..u64::from(tiles) {
            let at = 1000 + i * 100;
            put(w, 5, "ProductTile", at, 90);
            put(w, 6, "Card", at, 60);
            put(w, 7, "Text", at, 30);
        }
    }

    /// Frame 1 at 10 ms with 64 tiles (195 builds) and frame 2 at 100 ms with 8 tiles (26).
    fn two_frames() -> Arc<Snapshot> {
        snapshot(|w, run| {
            cart_frame(w, run, 1, 10, 64);
            cart_frame(w, run, 2, 100, 8);
        })
    }

    fn open_tab(
        cx: &mut TestAppContext,
        snap: Arc<Snapshot>,
    ) -> (
        Entity<crate::shell::MainWindow>,
        Entity<RebuildsTab>,
        &mut VisualTestContext,
    ) {
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.dispatch_action(ShowPerformance);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).rebuilds().clone());
        (shell, tab, cx)
    }

    fn cells(tab: &Entity<RebuildsTab>, cx: &mut VisualTestContext) -> Vec<Vec<String>> {
        tab.read_with(cx, |t, cx| t.cells(cx))
    }

    fn names(tab: &Entity<RebuildsTab>, cx: &mut VisualTestContext) -> Vec<(String, String)> {
        cells(tab, cx)
            .into_iter()
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect()
    }

    fn go(
        shell: &Entity<crate::shell::MainWindow>,
        selection: Option<Selection>,
        range: Option<TimeRange>,
        cx: &mut VisualTestContext,
    ) {
        let to = Location {
            page: Page::Tool(Tool::Performance),
            selection,
            range,
        };
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn toggle_calls_capture_and_reflects_actual_state(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).rebuilds().clone());
        assert!(crate::test_support::exists(cx, "rebuilds-trace-toggle"));
        assert!(crate::test_support::exists(cx, "rebuilds-overhead-note"));
        assert!(!tab.read_with(cx, |t, cx| t.switch_disabled(cx)));
        assert!(!tab.read_with(cx, |t, cx| t.tracing_on(cx)));
        click(cx, "rebuilds-trace-toggle");
        assert_eq!(
            fake.commands().last().map(String::as_str),
            Some("trace_builds true")
        );
        // The click is a request: the switch moves only when the run reports tracing.
        assert!(!tab.read_with(cx, |t, cx| t.tracing_on(cx)));
        let (mut w, run) = writer();
        w.set_tracing(run, true);
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert!(tab.read_with(cx, |t, cx| t.tracing_on(cx)));
        click(cx, "rebuilds-trace-toggle");
        assert_eq!(
            fake.commands().last().map(String::as_str),
            Some("trace_builds false")
        );
        w.set_tracing(run, false);
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert!(!tab.read_with(cx, |t, cx| t.tracing_on(cx)));
    }

    #[gpui_kit::test]
    fn toggle_disabled_when_disconnected(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        // Data from an earlier connection stays on screen; no app is connected now.
        fake.snaps.send(two_frames()).unwrap();
        cx.dispatch_action(ShowPerformance);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).rebuilds().clone());
        assert!(tab.read_with(cx, |t, cx| t.switch_disabled(cx)));
        assert_eq!(
            DISCONNECTED_TIP,
            "Connect to an app to trace widget builds."
        );
        click(cx, "rebuilds-trace-toggle");
        assert!(
            !fake
                .commands()
                .iter()
                .any(|c| c.starts_with("trace_builds")),
            "{:?}",
            fake.commands()
        );
        // The action is the same request and goes nowhere either with no app to ask.
        cx.dispatch_action(ToggleTraceWidgetBuilds);
        assert!(!tab.read_with(cx, |t, cx| t.tracing_on(cx)));
    }

    #[gpui_kit::test]
    fn frame_scope_lists_that_frames_widgets(cx: &mut TestAppContext) {
        let snap = two_frames();
        let id = snap.frames.get(0).unwrap().id;
        let (shell, tab, cx) = open_tab(cx, snap);
        // Whole session first: both frames.
        let all = names(&tab, cx);
        assert_eq!(all[0].1, "72", "{all:?}");
        go(&shell, Some(Selection::Frame(id)), None, cx);
        assert_eq!(tab.read_with(cx, |t, _| t.scope()), Scope::Frame(0));
        let rows = names(&tab, cx);
        let by_name = |n: &str| rows.iter().find(|r| r.0 == n).map(|r| r.1.clone());
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert_eq!(by_name("ProductTile").as_deref(), Some("64"));
        assert_eq!(by_name("Card").as_deref(), Some("64"));
        assert_eq!(by_name("CartScreen").as_deref(), Some("1"));
        // Sorted by builds, busiest first.
        let builds: Vec<u32> = rows.iter().map(|r| r.1.parse().unwrap()).collect();
        assert!(builds.windows(2).all(|p| p[0] >= p[1]), "{builds:?}");
        assert!(exists_in(cx, "rebuilds-table"));
        assert_eq!(
            tab.read_with(cx, |t, _| t.scope().label()),
            "Selected frame"
        );
    }

    /// Reviewer B's lazy-list frame: BUILD with 2 widgets, then LAYOUT running 3 child BUILDs
    /// of 3 widgets each. The frames table, the details pane and the Rebuilds tab agree on 11.
    #[gpui_kit::test]
    fn builds_inside_layout_count_the_same_everywhere(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 10, 30, 1));
            let begin = 10 * MS;
            w.set_frame_ui(
                run,
                1,
                UiPhases {
                    begin: SessionTime(begin),
                    ui_us: 24_000,
                    build_us: 3000,
                    layout_us: 18_000,
                    paint_us: 1000,
                    composite_us: 500,
                    finalize_us: 500,
                    widgets_built: Some(11),
                },
            );
            let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
            w.set_track_order(ui, 0);
            let mut put = |depth: u16, name: &str, at_us: u64, dur_us: u64| {
                let s = Slice {
                    start: SessionTime(begin + at_us * 1000),
                    dur_ns: dur_us * 1000,
                    name: w.intern_name(name),
                    cat: SliceCat::Dart,
                };
                assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
            };
            put(0, "VsyncProcessCallback", 0, 24_000);
            put(1, "Animator::BeginFrame", 0, 24_000);
            put(2, "BUILD", 0, 3000);
            put(3, "CartBadge", 0, 2000);
            put(4, "Text", 0, 1000);
            put(2, "LAYOUT (root)", 3000, 18_000);
            for k in 0..3u64 {
                let at = 3100 + k * 5000;
                put(3, "BUILD", at, 4800);
                put(4, "ProductTile", at, 4700);
                put(5, "Card", at, 4600);
                put(6, "Text", at, 100);
            }
        });
        let id = snap.frames.get(0).unwrap().id;
        let explained = fdt_analysis::explain::explain(&snap, 0);
        let (shell, tab, cx) = open_tab(cx, snap);
        go(&shell, Some(Selection::Frame(id)), None, cx);
        let builds: u32 = cells(&tab, cx)
            .iter()
            .map(|r| r[1].parse::<u32>().unwrap())
            .sum();
        let list = shell.read_with(cx, |s, cx| s.performance_view().read(cx).list().clone());
        let table = list.read_with(cx, |l, cx| l.cell_text(0, 7, cx));
        assert_eq!(table, "11");
        assert_eq!(explained.rebuilds.map(|r| r.total), Some(11));
        assert_eq!(builds, 11);
    }

    fn exists_in(cx: &mut VisualTestContext, id: &'static str) -> bool {
        crate::test_support::exists(cx, id)
    }

    #[gpui_kit::test]
    fn range_scope_aggregates(cx: &mut TestAppContext) {
        let (shell, tab, cx) = open_tab(cx, two_frames());
        let whole = cells(&tab, cx);
        let tile = whole
            .iter()
            .find(|r| r[0] == "ProductTile")
            .unwrap()
            .clone();
        // 64 + 8 builds over 2 frames, at most 64 in one.
        assert_eq!(&tile[1..4], ["72", "2", "64"], "{whole:?}");
        assert_eq!(tab.read_with(cx, |t, _| t.scope().label()), "Whole session");
        // Frame 2 only (100..130 ms).
        let range = TimeRange::new(SessionTime(95 * MS), SessionTime(135 * MS));
        go(&shell, None, Some(range), cx);
        assert_eq!(
            tab.read_with(cx, |t, _| t.scope().label()),
            "Selected range"
        );
        let rows = names(&tab, cx);
        assert_eq!(
            rows.iter()
                .find(|r| r.0 == "ProductTile")
                .map(|r| r.1.as_str()),
            Some("8"),
            "{rows:?}"
        );
        assert_eq!(rows.iter().filter(|r| r.0 == "CartScreen").count(), 1);
    }

    #[gpui_kit::test]
    fn filter_by_widget_name(cx: &mut TestAppContext) {
        let (_shell, tab, cx) = open_tab(cx, two_frames());
        assert_eq!(names(&tab, cx).len(), 6);
        tab.update_in(cx, |t, w, cx| t.set_filter_text("TILE", w, cx));
        cx.run_until_parked();
        assert_eq!(
            names(&tab, cx),
            vec![("ProductTile".to_string(), "72".to_string())]
        );
        tab.update_in(cx, |t, w, cx| t.set_filter_text("cart", w, cx));
        cx.run_until_parked();
        let rows = names(&tab, cx);
        assert_eq!(rows.len(), 2, "{rows:?}");
        tab.update_in(cx, |t, w, cx| t.set_filter_text("zzz", w, cx));
        cx.run_until_parked();
        assert!(names(&tab, cx).is_empty());
        assert!(exists_in(cx, "rebuilds-no-match"));
        tab.update_in(cx, |t, w, cx| t.set_filter_text("", w, cx));
        cx.run_until_parked();
        assert_eq!(names(&tab, cx).len(), 6);
    }

    #[gpui_kit::test]
    fn cap_note_shows_after_drops(cx: &mut TestAppContext) {
        let (_shell, _tab, cx) = open_tab(cx, two_frames());
        assert!(!exists_in(cx, "rebuilds-cap-note"));
        // Past the cap: raw widget builds stop being kept and the drop is counted.
        let capped = snapshot(|w, run| {
            cart_frame(w, run, 1, 10, 4);
            let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
            let name = w.intern_name("Spam");
            let base = 1_000 * MS;
            for i in 0..=fdt_store::timeline::RAW_BUILD_CAP {
                let s = Slice {
                    start: SessionTime(base + i * 2),
                    dur_ns: 1,
                    name,
                    cat: SliceCat::Dart,
                };
                w.push_slice(ui, 3, s, true);
            }
        });
        assert!(capped.timeline.drops().build_cap > 0);
        let (_shell, _tab, cx) = open_tab(cx, capped);
        assert!(exists_in(cx, "rebuilds-cap-note"));
    }

    #[gpui_kit::test]
    fn off_and_empty_states(cx: &mut TestAppContext) {
        // Frames, but no widget slices, and tracing off.
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 1, 10, 8, 1));
        });
        let (_shell, tab, cx) = open_tab(cx, snap);
        assert!(exists_in(cx, "rebuilds-off"));
        assert!(exists_in(cx, "rebuilds-trace-toggle"));
        assert!(names(&tab, cx).is_empty());
    }

    #[gpui_kit::test]
    fn empty_with_tracing_on_and_hidden_tab_idles(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("demo", true)).unwrap();
        let (mut w, run) = writer();
        w.set_tracing(run, true);
        fake.snaps.send(w.publish()).unwrap();
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).rebuilds().clone());
        // On the Frames tab nothing is computed.
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), 0);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), 1);
        assert!(exists_in(cx, "rebuilds-empty"));
    }

    /// A connected, tracing app streaming frames into a shown or hidden Rebuilds tab.
    fn streaming(
        cx: &mut TestAppContext,
    ) -> (
        Entity<crate::shell::MainWindow>,
        Entity<RebuildsTab>,
        &mut VisualTestContext,
        crate::test_support::FakeHandle,
        SessionWriter,
        RunId,
    ) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.status.send(connected("demo", true)).unwrap();
        let (mut w, run) = writer();
        w.set_tracing(run, true);
        cart_frame(&mut w, run, 1, 10, 4);
        fake.snaps.send(w.publish()).unwrap();
        cx.dispatch_action(ShowPerformance);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        let tab = shell.read_with(cx, |s, cx| s.performance_view().read(cx).rebuilds().clone());
        (shell, tab, cx, fake, w, run)
    }

    #[gpui_kit::test]
    fn the_hidden_tab_does_no_work(cx: &mut TestAppContext) {
        let (shell, tab, cx, fake, mut w, run) = streaming(cx);
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), 1);
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Frames, cx))
        });
        for n in 2..8u64 {
            cart_frame(&mut w, run, n, n * 100, 4);
            fake.snaps.send(w.publish()).unwrap();
            cx.executor().advance_clock(Duration::from_secs(3));
            cx.run_until_parked();
        }
        assert_eq!(
            tab.read_with(cx, |t, _| t.runs()),
            1,
            "nothing while hidden"
        );
        // Shown again, it catches up in one run.
        shell.update(cx, |s, cx| {
            s.performance_view()
                .update(cx, |v, cx| v.set_tab(PerfTab::Rebuilds, cx))
        });
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), 2);
        let tile = names(&tab, cx).into_iter().find(|r| r.0 == "ProductTile");
        assert_eq!(tile.map(|r| r.1).as_deref(), Some("28"));
    }

    #[gpui_kit::test]
    fn streaming_recomputes_at_most_every_two_seconds(cx: &mut TestAppContext) {
        let (_shell, tab, cx, fake, mut w, run) = streaming(cx);
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        let runs0 = tab.read_with(cx, |t, _| t.runs());
        // Ten publishes over 1.5 s: the first starts a run at once, the rest wait.
        for n in 2..12u64 {
            cart_frame(&mut w, run, n, n * 100, 4);
            fake.snaps.send(w.publish()).unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(150));
            cx.run_until_parked();
        }
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), runs0 + 1);
        // One trailing run, two seconds after the last start, with every frame in it.
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(tab.read_with(cx, |t, _| t.runs()), runs0 + 2);
        let tile = names(&tab, cx).into_iter().find(|r| r.0 == "ProductTile");
        assert_eq!(tile.map(|r| r.1).as_deref(), Some("44"));
    }

    /// The default reaches the window's capture when it changes, not only when the window opens:
    /// the next connect starts with it.
    #[gpui_kit::test]
    fn settings_switch_reaches_the_capture(cx: &mut TestAppContext) {
        let (_shell, cx, fake) = open_shell_fake(cx);
        crate::test_support::press(cx, "cmd-,", "ctrl-,");
        click(cx, "settings-trace-widget-builds");
        assert_eq!(
            fake.commands().last().map(String::as_str),
            Some("trace_default true")
        );
        click(cx, "settings-trace-widget-builds");
        assert_eq!(
            fake.commands().last().map(String::as_str),
            Some("trace_default false")
        );
    }

    #[gpui_kit::test]
    fn settings_switch_persists(cx: &mut TestAppContext) {
        let (_shell, cx, _fake) = open_shell_fake(cx);
        crate::test_support::press(cx, "cmd-,", "ctrl-,");
        assert!(exists_in(cx, "settings-page"));
        assert!(exists_in(cx, "settings-trace-widget-builds"));
        assert!(!cx.update(|_, cx| crate::settings::settings(cx).trace_widget_builds));
        click(cx, "settings-trace-widget-builds");
        assert!(cx.update(|_, cx| crate::settings::settings(cx).trace_widget_builds));
        click(cx, "settings-trace-widget-builds");
        assert!(!cx.update(|_, cx| crate::settings::settings(cx).trace_widget_builds));
    }
}
