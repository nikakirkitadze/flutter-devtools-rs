//! `fdt bench-ui <scenario> [--frames N]`: scripted UI scenarios over a stress session.
//!
//! The harness opens the main window over a fixed session (1M logs, 100k requests, 72k frames),
//! steps a script once per frame and prints one JSON line with frame statistics and budget
//! verdicts.
//!
//! # What "drawn" means
//!
//! A step runs in `Window::on_next_frame`. Despite the GPUI doc comment ("directly after the
//! current frame is rendered"), those callbacks run at the START of the next frame request,
//! before that frame draws (gpui-pre `window.rs`, the request-frame closure: callbacks run at
//! about line 2008, `should_draw` and the draw follow at about line 2027). So everything a step
//! observes or changes is drawn by the frame that starts right after it.
//!
//! "Drawn" is therefore detected from view model state (first-phase rows or the full result
//! applied; the new page current and its rows ready) observed in a step. The frame that drew it
//! is the first frame whose `draw_start` is at or after the step's clock, and the latency ends
//! at that frame's `draw_end` (from `FrameTimingCollector`). The latency in frames counts the
//! frames from the keystroke's own frame to that frame.
//!
//! Keystrokes go through the views' public APIs (`LogsView::set_search`,
//! `NetworkView::set_filter_text`), the same entry points the palette and the field use, rather
//! than synthesised key events; tool switches go through `dispatch_action` and the strip drag
//! through synthesised mouse events.

use crate::actions::show_tool;
use crate::app::{start_fixed, start_live};
use crate::nav::{Location, Page, Tool};
use crate::perf::{Counter, N as COUNTERS, snapshot};
use crate::shell::MainWindow;
use fdt_capture::ConnStatus;
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::RunId;
use fdt_ui::assets::FdtAssets;
use gpui_kit::profiler::{FrameEvent, FrameTimingCollector, set_trace_enabled};
use gpui_kit::{
    App, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Window, WindowId, point,
};
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Seed of the stress session.
const SEED: u64 = 42;
/// How long a run may take before it gives up with an error line and exit code 1.
const TIMEOUT: Duration = Duration::from_secs(120); // token-check: allow: watchdog timeout
/// Frames the script waits, at least, before it starts measuring.
const WARMUP_FRAMES: usize = 30;
/// A pending sample still unresolved after this is recorded as seen now, so it counts as a
/// (very) late result instead of being dropped.
const LATENCY_CAP: Duration = Duration::from_secs(1); // token-check: allow: sample cap, not motion
/// Frames between keystrokes of the search scenarios (the next one also waits for the full result).
const KEY_EVERY: usize = 5;

const LOG_QUERY: &str = "connection timeout";
const NET_QUERY: &str = "products";

pub const USAGE: &str = "usage: fdt bench-ui <scenario> [--frames N]\n\
scenarios: logs-scroll, logs-search, net-scroll, net-filter, tool-switch, strip-drag, live-logs";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    LogsScroll,
    LogsSearch,
    NetScroll,
    NetFilter,
    ToolSwitch,
    StripDrag,
    /// The Logs view following a session that grows by 50 lines every 16 ms.
    LiveLogs,
}

impl Scenario {
    pub const ALL: [Scenario; 7] = [
        Scenario::LogsScroll,
        Scenario::LogsSearch,
        Scenario::NetScroll,
        Scenario::NetFilter,
        Scenario::ToolSwitch,
        Scenario::StripDrag,
        Scenario::LiveLogs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::LogsScroll => "logs-scroll",
            Scenario::LogsSearch => "logs-search",
            Scenario::NetScroll => "net-scroll",
            Scenario::NetFilter => "net-filter",
            Scenario::ToolSwitch => "tool-switch",
            Scenario::StripDrag => "strip-drag",
            Scenario::LiveLogs => "live-logs",
        }
    }

    pub fn parse(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The tool whose page the scenario starts on.
    fn home(self) -> Tool {
        match self {
            Scenario::NetScroll | Scenario::NetFilter => Tool::Network,
            _ => Tool::Logs,
        }
    }

    /// The scripted length: frames for scrolls and the drag, switches for `tool-switch`.
    fn default_len(self) -> usize {
        match self {
            Scenario::LogsScroll | Scenario::NetScroll | Scenario::LiveLogs => 600,
            Scenario::StripDrag => 300,
            Scenario::ToolSwitch => 40,
            Scenario::LogsSearch => LOG_QUERY.len(),
            Scenario::NetFilter => NET_QUERY.len(),
        }
    }

    /// The p99 budget of the latency samples, in ms.
    fn latency_budget_ms(self) -> Option<f64> {
        match self {
            Scenario::LogsSearch | Scenario::NetFilter => Some(16.0),
            Scenario::ToolSwitch => Some(50.0),
            _ => None,
        }
    }
}

// ---- statistics ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pct {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Nearest-rank percentiles of `samples` (any order); `None` when there are none.
pub fn percentiles(samples: &[f64]) -> Option<Pct> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let at = |p: usize| sorted[(p * n).div_ceil(100).max(1) - 1];
    Some(Pct {
        p50: at(50),
        p95: at(95),
        p99: at(99),
        max: sorted[n - 1],
    })
}

/// Milliseconds between consecutive instants.
pub fn intervals_ms(starts: &[Instant]) -> Vec<f64> {
    starts
        .windows(2)
        .map(|w| w[1].saturating_duration_since(w[0]).as_nanos() as f64 / 1e6)
        .collect()
}

/// The plan's frame-interval p99 budget: 8.3 ms on a display that refreshes at 120 Hz or
/// faster, otherwise 16.7 ms.
pub fn interval_plan_budget_ms(refresh_ms: f64) -> f64 {
    if refresh_ms <= 8.4 { 8.3 } else { 16.7 }
}

/// The interval p99 budget actually applied. The plan's 8.3 ms is below the 8.333 ms period of a
/// 120 Hz display, so no run could meet it; the effective budget is the larger of the plan and
/// one refresh period, plus half a millisecond for timestamp jitter.
pub fn interval_effective_budget_ms(refresh_ms: f64) -> f64 {
    r3(interval_plan_budget_ms(refresh_ms).max(refresh_ms) + INTERVAL_JITTER_MS)
}

/// Frames a result may take after the keystroke's own frame for a `budget_ms` budget:
/// `ceil(budget / refresh)`. A frame period is the finest a draw can be observed at.
pub fn budget_frames(budget_ms: f64, refresh_ms: f64) -> f64 {
    // The small offset keeps 50 ms at 8.333 ms (6.0002 frames) at 6 frames.
    (budget_ms / refresh_ms.max(0.1) - 0.01).ceil()
}

fn ms(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1e6
}

fn median(samples: &[f64]) -> Option<f64> {
    percentiles(samples).map(|p| p.p50)
}

/// What one run measured, before it is judged.
#[derive(Clone, Debug)]
pub struct Measured {
    pub draw_ms: Vec<f64>,
    /// Per drawn frame: request layout (render), prepaint and paint, in ms.
    pub phases_ms: [Vec<f64>; 3],
    /// Renders per counter over the measured frames.
    pub renders: Vec<(&'static str, u64)>,
    pub interval_ms: Vec<f64>,
    /// Keystroke to drawn (first results for `logs-search`), or switch to drawn.
    pub latency_ms: Option<Vec<f64>>,
    /// The same in frames after the keystroke's frame.
    pub latency_frames: Option<Vec<f64>>,
    /// Keystroke to the full result drawn (`logs-search` only).
    pub full_ms: Option<Vec<f64>>,
    pub full_frames: Option<Vec<f64>>,
    pub refresh_ms: f64,
    /// `"display"` when GPUI reported the refresh interval, `"inferred"` otherwise.
    pub refresh_source: &'static str,
    /// Script steps run.
    pub frames: usize,
}

pub struct Report {
    json: String,
    pub pass: bool,
}

impl Report {
    pub fn to_json(&self) -> &str {
        &self.json
    }
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn pct_json(p: Option<Pct>) -> String {
    match p {
        Some(p) => format!(
            "{{\"p50\":{:?},\"p95\":{:?},\"p99\":{:?},\"max\":{:?}}}",
            r3(p.p50),
            r3(p.p95),
            r3(p.p99),
            r3(p.max)
        ),
        None => "null".to_string(),
    }
}

/// The full-scan budget for a text search over 1M lines, in ms.
const FULL_SCAN_BUDGET_MS: f64 = 100.0;
const DRAW_BUDGET_MS: f64 = 4.0;
/// No frame may take longer than this on a 120 Hz display.
const MAX_FRAME_MS: f64 = 16.7;
/// Frame timestamps jitter by a fraction of a millisecond around the display period.
const INTERVAL_JITTER_MS: f64 = 0.5;

impl Measured {
    pub fn report(&self, scenario: Scenario) -> Report {
        let draw = percentiles(&self.draw_ms);
        let interval = percentiles(&self.interval_ms);
        let latency = self.latency_ms.as_deref().and_then(percentiles);
        let latency_f = self.latency_frames.as_deref().and_then(percentiles);
        let full = self.full_ms.as_deref().and_then(percentiles);
        let full_f = self.full_frames.as_deref().and_then(percentiles);
        let plan = interval_plan_budget_ms(self.refresh_ms);
        let effective = interval_effective_budget_ms(self.refresh_ms);
        let strict = plan < 10.0;
        let mut pass = draw.is_some_and(|p| p.p99 <= DRAW_BUDGET_MS)
            && interval.is_some_and(|p| p.p99 <= effective && (!strict || p.max <= MAX_FRAME_MS));
        let latency_budget = scenario.latency_budget_ms();
        let latency_frames_budget = latency_budget.map(|b| budget_frames(b, self.refresh_ms));
        let full_frames_budget = budget_frames(FULL_SCAN_BUDGET_MS, self.refresh_ms);
        if let Some(b) = latency_frames_budget {
            pass &= latency_f.is_some_and(|p| p.p99 <= b);
        }
        if self.full_ms.is_some() {
            pass &= full_f.is_some_and(|p| p.p99 <= full_frames_budget);
        }
        let mut budget =
            format!("\"draw_p99_ms\":{DRAW_BUDGET_MS:?},\"interval_p99_ms\":{effective:?}");
        if strict {
            budget.push_str(&format!(",\"interval_max_ms\":{MAX_FRAME_MS:?}"));
        }
        if let (Some(b), Some(f)) = (latency_budget, latency_frames_budget) {
            budget.push_str(&format!(
                ",\"latency_p99_ms\":{b:?},\"latency_budget_frames\":{f:?}"
            ));
        }
        if self.full_ms.is_some() {
            budget.push_str(&format!(
                ",\"full_p99_ms\":{FULL_SCAN_BUDGET_MS:?},\"full_budget_frames\":{full_frames_budget:?}"
            ));
        }
        let mut json = format!(
            "{{\"scenario\":\"{}\",\"frames\":{},\"draws\":{},\"draw_ms\":{},\"interval_ms\":{}",
            scenario.name(),
            self.frames,
            self.draw_ms.len(),
            pct_json(draw),
            pct_json(interval)
        );
        if let Some(l) = &self.latency_ms {
            json.push_str(&format!(
                ",\"latency_ms\":{},\"latency_frames\":{}",
                pct_json(latency),
                pct_json(latency_f)
            ));
            let n_full = self.full_ms.as_ref().map(Vec::len);
            match n_full {
                Some(n) => json.push_str(&format!(
                    ",\"full_ms\":{},\"full_frames\":{},\"samples\":{{\"first\":{},\"full\":{n}}}",
                    pct_json(full),
                    pct_json(full_f),
                    l.len()
                )),
                None => json.push_str(&format!(",\"samples\":{{\"latency\":{}}}", l.len())),
            }
        }
        json.push_str(&format!(
            ",\"phases_ms\":{{\"request_layout\":{},\"prepaint\":{},\"paint\":{}}}",
            pct_json(percentiles(&self.phases_ms[0])),
            pct_json(percentiles(&self.phases_ms[1])),
            pct_json(percentiles(&self.phases_ms[2])),
        ));
        let draws = self.draw_ms.len().max(1) as f64;
        let renders: Vec<String> = self
            .renders
            .iter()
            .map(|(name, n)| format!("\"{name}\":{:?}", r3(*n as f64 / draws)))
            .collect();
        json.push_str(&format!(",\"renders_per_frame\":{{{}}}", renders.join(",")));
        json.push_str(&format!(
            ",\"budget\":{{{budget}}},\"interval_budget_ms\":{effective:?},\"interval_plan_budget_ms\":{plan:?},\"refresh_ms\":{:?},\"refresh_source\":\"{}\",\"pass\":{pass}}}",
            r3(self.refresh_ms),
            self.refresh_source
        ));
        Report { json, pass }
    }
}

// ---- scripts ----

/// One keystroke or switch: when it happened and when its result was seen in a step.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub key_at: Instant,
    /// A step observed the first results (the new page, for a switch).
    pub first_seen: Instant,
    /// A step observed the full result (`logs-search` only).
    pub full_seen: Option<Instant>,
}

/// What a script has done so far; the headless test reads it.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Measured steps run.
    pub steps: usize,
    /// The last row index a scroll scenario asked for.
    pub last_position: Option<usize>,
    pub keystrokes: usize,
    pub samples: Vec<Sample>,
    /// Set when the script could not do what it claims (the run then fails).
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Warmup,
    Running,
    Done,
}

struct Pending {
    at: Instant,
    first_base: usize,
    first_seen: Option<Instant>,
    target: Option<Tool>,
}

pub struct Script {
    scenario: Scenario,
    len: usize,
    phase: Phase,
    warm: usize,
    prepared: bool,
    frame: usize,
    /// Frames since the last keystroke.
    since_key: usize,
    started: Option<Instant>,
    pending: Option<Pending>,
    switches: usize,
    outcome: Outcome,
}

impl Script {
    /// `len` overrides the scripted length (frames, or switches for `tool-switch`); the two
    /// search scenarios always type their whole query.
    pub fn new(scenario: Scenario, len: Option<usize>) -> Self {
        let len = match scenario {
            Scenario::LogsSearch | Scenario::NetFilter => scenario.default_len(),
            _ => len.unwrap_or(scenario.default_len()),
        };
        Self {
            scenario,
            len,
            phase: Phase::Warmup,
            warm: 0,
            prepared: false,
            frame: 0,
            since_key: KEY_EVERY,
            started: None,
            pending: None,
            switches: 0,
            outcome: Outcome::default(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    /// When measuring began; frames drawn before it are not counted.
    pub fn measuring_since(&self) -> Option<Instant> {
        self.started
    }

    fn fail(&mut self, message: &str) {
        self.outcome.error = Some(message.to_string());
        self.phase = Phase::Done;
    }

    /// Runs one scripted step. Call it once per frame, from the next-frame callback.
    pub fn step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        match self.phase {
            Phase::Warmup => self.warm_up(shell, window, cx),
            Phase::Running => {
                match self.scenario {
                    Scenario::LogsScroll => self.scroll_step(shell, cx, 3),
                    Scenario::NetScroll => self.scroll_step(shell, cx, 2),
                    Scenario::LogsSearch | Scenario::NetFilter => {
                        self.search_step(shell, window, cx)
                    }
                    Scenario::ToolSwitch => self.switch_step(shell, window, cx),
                    Scenario::StripDrag => self.drag_step(shell, window, cx),
                    Scenario::LiveLogs => self.live_step(shell, cx),
                }
                self.outcome.steps += 1;
            }
            Phase::Done => {}
        }
    }

    fn warm_up(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        if !self.prepared {
            self.prepared = true;
            let page = Page::Tool(self.scenario.home());
            shell.update(cx, |s, cx| s.navigate(Location::at(page), window, cx));
            let focus = shell.read(cx).focus_handle();
            window.focus(&focus, cx);
        }
        self.warm += 1;
        if self.warm >= WARMUP_FRAMES && ready(shell, self.scenario.home(), cx) {
            self.phase = Phase::Running;
            self.started = Some(Instant::now());
            self.frame = 0;
        }
    }

    fn scroll_step(&mut self, shell: &Entity<MainWindow>, cx: &mut App, stride: usize) {
        let i = self.frame * stride;
        match self.scenario {
            Scenario::LogsScroll => shell.read(cx).logs_view().read(cx).scroll_to_row(i),
            _ => shell
                .read(cx)
                .network_view()
                .clone()
                .update(cx, |net, cx| net.scroll_to_row(i, cx)),
        }
        self.outcome.last_position = Some(i);
        self.frame += 1;
        if self.frame >= self.len {
            self.phase = Phase::Done;
        }
    }

    /// Nothing to do but let the feed grow the session and the view follow it; the rows the list
    /// has at each step go in `last_position`.
    fn live_step(&mut self, shell: &Entity<MainWindow>, cx: &mut App) {
        self.outcome.last_position = Some(shell.read(cx).logs_view().read(cx).row_count());
        self.frame += 1;
        if self.frame >= self.len {
            self.phase = Phase::Done;
        }
    }

    /// The pending keystroke's state: `(first-phase count, settled)`.
    fn probe(&self, shell: &Entity<MainWindow>, cx: &App) -> (usize, bool) {
        if self.scenario == Scenario::LogsSearch {
            let logs = shell.read(cx).logs_view().read(cx);
            (logs.first_phases(), logs.is_settled())
        } else {
            (0, shell.read(cx).network_view().read(cx).is_settled())
        }
    }

    /// Types the query one character at a time. The next character goes in once
    /// [`KEY_EVERY`] frames have passed AND the previous one's full result has been seen, so
    /// every keystroke yields both a first-results and a full-result sample.
    fn search_step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        let now = Instant::now();
        let logs = self.scenario == Scenario::LogsSearch;
        let text = if logs { LOG_QUERY } else { NET_QUERY };
        let (first_phases, settled) = self.probe(shell, cx);
        if let Some(p) = &mut self.pending {
            if p.first_seen.is_none() && (first_phases > p.first_base || settled) {
                p.first_seen = Some(now);
            }
            if settled || now.saturating_duration_since(p.at) > LATENCY_CAP {
                self.outcome.samples.push(Sample {
                    key_at: p.at,
                    first_seen: p.first_seen.unwrap_or(now),
                    full_seen: logs.then_some(now),
                });
                self.pending = None;
            }
        }
        self.since_key += 1;
        if self.pending.is_none()
            && self.since_key >= KEY_EVERY
            && self.outcome.keystrokes < text.len()
        {
            let prefix = &text[..=self.outcome.keystrokes];
            let first_base = first_phases;
            let at = Instant::now();
            if logs {
                shell.read(cx).logs_view().clone().update(cx, |view, cx| {
                    view.set_search(prefix, false, window, cx);
                });
            } else {
                shell
                    .read(cx)
                    .network_view()
                    .clone()
                    .update(cx, |net, cx| net.set_filter_text(prefix, window, cx));
            }
            self.pending = Some(Pending {
                at,
                first_base,
                first_seen: None,
                target: None,
            });
            self.outcome.keystrokes += 1;
            self.since_key = 0;
        }
        self.frame += 1;
        if self.outcome.keystrokes == text.len() && self.pending.is_none() {
            self.phase = Phase::Done;
        }
    }

    fn switch_step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        let now = Instant::now();
        if let Some(p) = &self.pending {
            let target = p.target.unwrap_or(Tool::Logs);
            let shown =
                shell.read(cx).current().page == Page::Tool(target) && ready(shell, target, cx);
            if shown || now.saturating_duration_since(p.at) > LATENCY_CAP {
                self.outcome.samples.push(Sample {
                    key_at: p.at,
                    first_seen: now,
                    full_seen: None,
                });
                self.pending = None;
            }
        }
        if self.pending.is_none() && self.switches < self.len {
            // The script starts on Logs, so the first switch goes to Network.
            let target = if self.switches.is_multiple_of(2) {
                Tool::Network
            } else {
                Tool::Logs
            };
            let at = Instant::now();
            // As the app's own shortcuts do: focus the shell, then dispatch, so the action
            // reaches it whatever had focus a moment ago.
            let focus = shell.read(cx).focus_handle();
            window.focus(&focus, cx);
            window.dispatch_action(show_tool(target), cx);
            self.pending = Some(Pending {
                at,
                first_base: 0,
                first_seen: None,
                target: Some(target),
            });
            self.switches += 1;
            self.outcome.keystrokes += 1;
        }
        self.frame += 1;
        if self.switches >= self.len && self.pending.is_none() {
            self.phase = Phase::Done;
        }
    }

    fn drag_step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        let total = self.len.max(2);
        let bounds = shell.read(cx).strip_bounds();
        if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
            self.fail("the session strip has not been laid out, so there is nothing to drag");
            return;
        }
        let y = bounds.origin.y + bounds.size.height / 2.0;
        let frac = 0.1 + 0.8 * self.frame as f32 / (total - 1) as f32;
        let position = point(bounds.origin.x + bounds.size.width * frac, y);
        let modifiers = Modifiers::default();
        let event = if self.frame == 0 {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                }),
                cx,
            );
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            })
        } else if self.frame + 1 >= total {
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
            })
        } else {
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers,
            })
        };
        window.dispatch_event(event, cx);
        self.frame += 1;
        if self.frame >= total {
            self.phase = Phase::Done;
            if shell.read(cx).current().range.is_none() {
                self.fail("the drag did not select a time range on the strip");
            }
        }
    }
}

/// Whether `tool`'s page is showing its rows for the current filter.
fn ready(shell: &Entity<MainWindow>, tool: Tool, cx: &App) -> bool {
    match tool {
        Tool::Network => {
            let net = shell.read(cx).network_view().read(cx);
            net.is_settled() && net.filtered_len() > 0
        }
        _ => shell.read(cx).logs_view().read(cx).is_settled(),
    }
}

// ---- the live feed ----

/// Lines added per publish by the `live-logs` feed.
const LIVE_LINES: usize = 50;
/// Time between the feed's publishes.
const LIVE_EVERY: Duration = Duration::from_millis(16); // token-check: allow: feed pacing

/// Appends `n` log lines after `*next_ms` (and moves it on), one millisecond apart.
fn append_live_lines(w: &mut fdt_store::SessionWriter, run: RunId, next_ms: &mut u64, n: usize) {
    use fdt_store::logs::{LogInput, LogLevel, LogSource};
    use fdt_store::time::SessionTime;
    for _ in 0..n {
        w.push_log(LogInput {
            run,
            time: SessionTime(*next_ms * 1_000_000),
            clamped: false,
            level: LogLevel::Info,
            source: LogSource::Log,
            logger: "live".into(),
            text: format!("live line at {} ms: request handled, status=200", *next_ms),
            error: None,
        });
        *next_ms += 1;
    }
}

/// A capture that only replays what a thread publishes: it ignores every command.
struct LiveFeed {
    status: tokio::sync::watch::Receiver<ConnStatus>,
    snaps: tokio::sync::watch::Receiver<std::sync::Arc<fdt_store::Snapshot>>,
}

impl crate::capture_host::CaptureControl for LiveFeed {
    fn connect(&self, _: fdt_capture::Target) {}
    fn disconnect(&self) {}
    fn stop_waiting(&self) {}
    fn set_recording(&self, _: bool) {}
    fn new_session(&self) {}
    fn load_full_body(&self, _: fdt_store::time::EntryId, _: fdt_store::net::BodyKind) {}
    fn set_free_app_http_memory(&self, _: bool) {}
    fn set_body_limits(&self, _: fdt_store::net::BodyLimits) {}
    fn fetch_cpu_samples(&self) {}
    fn status(&self) -> tokio::sync::watch::Receiver<ConnStatus> {
        self.status.clone()
    }
    fn snapshots(&self) -> tokio::sync::watch::Receiver<std::sync::Arc<fdt_store::Snapshot>> {
        self.snaps.clone()
    }
}

/// Starts the `live-logs` feed: the stress session, then [`LIVE_LINES`] more log lines every
/// [`LIVE_EVERY`], published from a thread that owns the writer.
fn live_feed() -> LiveFeed {
    let (mut w, run) = fdt_store::synth::stress_writer(SEED);
    let first = w.publish();
    let mut next_ms = first.latest_time().0 / 1_000_000 + 1;
    let (status_tx, status) = tokio::sync::watch::channel(stress_status());
    let (snaps_tx, snaps) = tokio::sync::watch::channel(first);
    std::thread::spawn(move || {
        let _keep = status_tx;
        loop {
            std::thread::sleep(LIVE_EVERY);
            append_live_lines(&mut w, run, &mut next_ms, LIVE_LINES);
            if snaps_tx.send(w.publish()).is_err() {
                break;
            }
        }
    });
    LiveFeed { status, snaps }
}

fn stress_status() -> ConnStatus {
    ConnStatus::Connected {
        run: RunId(1),
        app: AppInfo {
            name: "stress".into(),
            device: Some("bench".into()),
            mode: Mode::Debug,
            flutter_version: None,
            workspace: None,
            vm_uri: "ws://127.0.0.1:1/ws".into(),
            pid: None,
        },
        recording: false,
    }
}

// ---- the real run ----

struct Driver {
    script: Script,
    shell: Entity<MainWindow>,
    scenario: Scenario,
    collector: FrameTimingCollector,
    window_id: WindowId,
    /// `(draw_start, draw_end)` of each frame drawn while measuring.
    draws: Vec<(Instant, Instant)>,
    /// The phase split of each frame in `draws`, in ms.
    phases: Vec<[f64; 3]>,
    /// Render counters when measuring began.
    renders_start: Option<[u64; COUNTERS]>,
    /// Render counters when the last measured frame was recorded.
    renders_end: [u64; COUNTERS],
    reported_refresh: Option<Duration>,
    /// The script is done; one more frame runs so the last result's draw is recorded.
    finishing: bool,
}

impl Driver {
    fn collect(&mut self) {
        let since = self.script.measuring_since();
        for event in self.collector.collect_unseen() {
            if let FrameEvent::Draw(t) = event
                && t.window_id == self.window_id
            {
                if let Some(r) = t.refresh_interval {
                    self.reported_refresh = Some(r);
                }
                if since.is_some_and(|s| t.draw_start >= s) {
                    self.draws.push((t.draw_start, t.draw_end));
                    self.phases.push([
                        ms(t.phases.request_layout),
                        ms(t.phases.prepaint),
                        ms(t.phases.paint),
                    ]);
                }
            }
        }
    }

    /// Index of the first drawn frame that started at or after `at`.
    fn frame_at(&self, at: Instant) -> Option<usize> {
        let i = self.draws.partition_point(|d| d.0 < at);
        (i < self.draws.len()).then_some(i)
    }

    /// `(ms, frames)` from a keystroke to the end of the frame that drew `seen`.
    fn latency(&self, key_at: Instant, seen: Instant) -> Option<(f64, f64)> {
        let key_frame = self.frame_at(key_at)?;
        let drawn = self.frame_at(seen)?;
        let end = self.draws[drawn].1;
        Some((
            end.saturating_duration_since(key_at).as_nanos() as f64 / 1e6,
            drawn.saturating_sub(key_frame) as f64,
        ))
    }

    fn measured(&self, window_refresh: Option<Duration>) -> Measured {
        let starts: Vec<Instant> = self.draws.iter().map(|d| d.0).collect();
        let interval_ms = intervals_ms(&starts);
        let (refresh_ms, refresh_source) = match window_refresh.or(self.reported_refresh) {
            Some(r) => (ms(r), "display"),
            None => (median(&interval_ms).unwrap_or(16.7), "inferred"),
        };
        let o = self.script.outcome();
        let (mut first_ms, mut first_frames) = (Vec::new(), Vec::new());
        let (mut full_ms, mut full_frames) = (Vec::new(), Vec::new());
        for s in &o.samples {
            if let Some((m, f)) = self.latency(s.key_at, s.first_seen) {
                first_ms.push(m);
                first_frames.push(f);
            }
            if let Some(seen) = s.full_seen
                && let Some((m, f)) = self.latency(s.key_at, seen)
            {
                full_ms.push(m);
                full_frames.push(f);
            }
        }
        let has_latency = self.scenario.latency_budget_ms().is_some();
        let has_full = self.scenario == Scenario::LogsSearch;
        Measured {
            draw_ms: self
                .draws
                .iter()
                .map(|(a, b)| ms(b.saturating_duration_since(*a)))
                .collect(),
            phases_ms: std::array::from_fn(|i| self.phases.iter().map(|p| p[i]).collect()),
            renders: Counter::ALL
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let start = self.renders_start.map_or(0, |s| s[i]);
                    (c.name(), self.renders_end[i].saturating_sub(start))
                })
                .collect(),
            interval_ms,
            latency_ms: has_latency.then_some(first_ms),
            latency_frames: has_latency.then_some(first_frames),
            full_ms: has_full.then_some(full_ms),
            full_frames: has_full.then_some(full_frames),
            refresh_ms,
            refresh_source,
            frames: o.steps,
        }
    }
}

fn tick(driver: Rc<RefCell<Driver>>, window: &mut Window, cx: &mut App) {
    {
        let mut d = driver.borrow_mut();
        if !d.finishing {
            let shell = d.shell.clone();
            d.script.step(&shell, window, cx);
        }
        if d.renders_start.is_none() && d.script.measuring_since().is_some() {
            d.renders_start = Some(snapshot());
        }
        d.collect();
        d.renders_end = snapshot();
        if let Some(error) = d.script.outcome().error.clone() {
            fail(d.scenario, &error);
        }
        if d.finishing {
            let measured = d.measured(window.refresh_interval(cx));
            let report = measured.report(d.scenario);
            if measured.refresh_source == "inferred" {
                eprintln!(
                    "bench-ui: the display refresh interval was not reported; inferred {:.3} ms from the median frame interval",
                    measured.refresh_ms
                );
            }
            println!("{}", report.to_json());
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            // `cx.quit()` ends in `exit(0)` on macOS, so the verdict has to exit directly.
            std::process::exit(if report.pass { 0 } else { 1 });
        }
        d.finishing = d.script.is_done();
    }
    // Ask for the next frame the way the app does: notify the view the script is working in, as a
    // scroll or a keystroke would, and the window's root so a frame is drawn even before that
    // view is on screen. `window.refresh()` would redraw every view and defeat the views that are
    // cached between frames, so it would measure a frame the app never draws.
    let (shell, scenario) = {
        let d = driver.borrow();
        (d.shell.clone(), d.scenario)
    };
    match scenario {
        Scenario::ToolSwitch => {}
        _ if scenario.home() == Tool::Network => {
            cx.notify(shell.read(cx).network_view().entity_id())
        }
        _ => cx.notify(shell.read(cx).logs_view().entity_id()),
    }
    // Notifying the shell each frame is a cost a real wheel scroll does not have (it notifies only
    // the scrolled view). That cost is outside the measured draw: it is paid when the frame is
    // requested, not inside the frame's draw.
    cx.notify(shell.entity_id());
    window.on_next_frame(move |window, cx| tick(driver, window, cx));
}

fn fail(scenario: Scenario, error: &str) -> ! {
    println!(
        "{{\"scenario\":\"{}\",\"error\":\"{}\",\"pass\":false}}",
        scenario.name(),
        error.replace('\\', "\\\\").replace('"', "\\\"")
    );
    std::process::exit(1);
}

/// Parses `bench-ui` arguments (without the subcommand).
pub fn parse_args(args: &[String]) -> Result<(Scenario, Option<usize>), String> {
    let mut scenario = None;
    let mut frames = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--frames" {
            let n = it
                .next()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .ok_or("--frames needs a positive number")?;
            frames = Some(n);
        } else if a.starts_with('-') {
            return Err(format!("unknown option {a}"));
        } else if scenario.is_none() {
            scenario = Some(Scenario::parse(a).ok_or_else(|| format!("unknown scenario {a}"))?);
        } else {
            return Err(format!("unexpected argument {a}"));
        }
    }
    Ok((scenario.ok_or("missing scenario")?, frames))
}

/// Entry point of `fdt bench-ui`.
pub fn main(args: &[String]) -> ExitCode {
    match parse_args(args) {
        Ok((scenario, frames)) => run(scenario, frames),
        Err(message) => {
            eprintln!("{USAGE}");
            eprintln!("fdt bench-ui: {message}");
            ExitCode::from(2)
        }
    }
}

enum Source {
    Fixed(std::sync::Arc<fdt_store::Snapshot>),
    Live(LiveFeed),
}

/// Builds the stress session, opens the window, runs `scenario` and exits the process with the
/// verdict. Never returns normally.
pub fn run(scenario: Scenario, frames: Option<usize>) -> ExitCode {
    // Building the session is not timed (and not covered by the timeout).
    let source = if scenario == Scenario::LiveLogs {
        Source::Live(live_feed())
    } else {
        Source::Fixed(fdt_store::synth::stress_session(SEED))
    };
    std::thread::spawn(move || {
        std::thread::sleep(TIMEOUT);
        fail(scenario, "timed out");
    });
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(move |cx: &mut App| {
            set_trace_enabled(true);
            let started = match source {
                Source::Fixed(snapshot) => start_fixed(cx, snapshot, stress_status()),
                Source::Live(feed) => start_live(cx, Box::new(feed)),
            };
            let shell = match started {
                Ok(shell) => shell,
                Err(err) => fail(scenario, &format!("could not open the window: {err:#}")),
            };
            let Some(handle) = cx.windows().first().copied() else {
                fail(scenario, "the main window is missing");
            };
            let driver = Rc::new(RefCell::new(Driver {
                script: Script::new(scenario, frames),
                shell,
                scenario,
                collector: FrameTimingCollector::new(),
                window_id: handle.window_id(),
                draws: Vec::new(),
                phases: Vec::new(),
                renders_start: None,
                renders_end: [0; COUNTERS],
                reported_refresh: None,
                finishing: false,
            }));
            let result = handle.update(cx, move |_, window, _| {
                window.refresh();
                window.on_next_frame(move |window, cx| tick(driver, window, cx));
            });
            if let Err(err) = result {
                fail(scenario, &format!("could not reach the window: {err:#}"));
            }
        });
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_model::SessionModel;
    use crate::test_support::{connected, open_shell_with};
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt as _;

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = percentiles(&samples).unwrap();
        assert_eq!((p.p50, p.p95, p.p99, p.max), (50.0, 95.0, 99.0, 100.0));
    }

    #[test]
    fn percentiles_ignore_input_order_and_handle_one_sample() {
        let p = percentiles(&[5.0, 1.0, 3.0]).unwrap();
        assert_eq!((p.p50, p.p99, p.max), (3.0, 5.0, 5.0));
        let one = percentiles(&[2.5]).unwrap();
        assert_eq!((one.p50, one.p95, one.p99, one.max), (2.5, 2.5, 2.5, 2.5));
        assert!(percentiles(&[]).is_none());
    }

    #[test]
    fn intervals_are_deltas_of_consecutive_draw_starts() {
        let t0 = std::time::Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        let starts = [at(0), at(8), at(17), at(25)];
        assert_eq!(intervals_ms(&starts), vec![8.0, 9.0, 8.0]);
    }

    #[test]
    fn plan_budget_follows_the_display_refresh() {
        assert_eq!(interval_plan_budget_ms(8.333), 8.3);
        assert_eq!(interval_plan_budget_ms(8.4), 8.3);
        assert_eq!(interval_plan_budget_ms(16.667), 16.7);
    }

    #[test]
    fn effective_budget_is_never_below_one_refresh_period() {
        assert_eq!(interval_effective_budget_ms(8.333), 8.833);
        assert_eq!(interval_effective_budget_ms(16.667), 17.2);
        assert_eq!(interval_effective_budget_ms(13.333), 17.2);
    }

    #[test]
    fn frame_budget_rounds_up() {
        assert_eq!(budget_frames(16.0, 8.333), 2.0);
        assert_eq!(budget_frames(16.0, 16.667), 1.0);
        assert_eq!(budget_frames(50.0, 8.333), 6.0);
    }

    #[test]
    fn scenario_names_round_trip() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::parse(s.name()), Some(s));
        }
        assert_eq!(Scenario::parse("nope"), None);
    }

    fn base() -> Measured {
        Measured {
            draw_ms: vec![1.0; 100],
            phases_ms: [vec![], vec![], vec![]],
            renders: vec![],
            interval_ms: vec![8.3; 100],
            latency_ms: None,
            latency_frames: None,
            full_ms: None,
            full_frames: None,
            refresh_ms: 8.333,
            refresh_source: "display",
            frames: 100,
        }
    }

    #[test]
    fn report_passes_only_when_every_budget_holds() {
        let ok = base();
        assert!(ok.report(Scenario::LogsScroll).pass);
        let slow_draw = Measured {
            draw_ms: vec![5.0; 100],
            ..ok.clone()
        };
        assert!(!slow_draw.report(Scenario::LogsScroll).pass);
        let long_frame = Measured {
            interval_ms: [vec![8.3; 99], vec![20.0]].concat(),
            ..ok.clone()
        };
        assert!(!long_frame.report(Scenario::LogsScroll).pass);
        let json = ok.report(Scenario::LogsScroll).to_json().to_string();
        assert!(json.contains("\"scenario\":\"logs-scroll\""));
        assert!(json.contains("\"frames\":100,\"draws\":100"));
        assert!(json.contains("\"interval_budget_ms\":8.833"));
        assert!(json.contains("\"interval_plan_budget_ms\":8.3"));
        assert!(!json.contains("latency_ms"));
    }

    #[test]
    fn interval_p99_above_the_effective_budget_fails() {
        let m = Measured {
            interval_ms: vec![9.0; 100],
            ..base()
        };
        assert!(!m.report(Scenario::LogsScroll).pass);
    }

    #[test]
    fn search_latency_is_judged_in_frames() {
        let search = |ms: f64, frames: f64| Measured {
            latency_ms: Some(vec![ms; 18]),
            latency_frames: Some(vec![frames; 18]),
            full_ms: Some(vec![30.0; 18]),
            full_frames: Some(vec![4.0; 18]),
            ..base()
        };
        // 16 ms at 120 Hz is two frames: a result in the second frame passes...
        let ok = search(16.6, 2.0).report(Scenario::LogsSearch);
        assert!(ok.pass);
        let json = ok.to_json();
        assert!(json.contains("\"latency_budget_frames\":2.0"));
        assert!(json.contains("\"samples\":{\"first\":18,\"full\":18}"));
        // ...a third frame fails, however short it was in milliseconds.
        assert!(!search(9.0, 3.0).report(Scenario::LogsSearch).pass);
    }

    /// Runs `scenario` on a small session without frames: each `step` stands in for a frame,
    /// and the executor settles between steps. With `draw`, a frame is rendered first so the
    /// strip has bounds.
    fn run_script(
        cx: &mut TestAppContext,
        scenario: Scenario,
        frames: Option<usize>,
        draw: bool,
    ) -> Script {
        let snapshot = fdt_store::synth::session(3, 100, 100, 2_000);
        let (shell, vcx) = open_shell_with(cx, |_| {
            SessionModel::fixed(snapshot, connected("stress", true))
        });
        let mut script = Script::new(scenario, frames);
        for _ in 0..3_000 {
            vcx.update(|window, cx| {
                if draw {
                    window.render_frame(cx);
                }
                script.step(&shell, window, cx);
            });
            vcx.run_until_parked();
            if script.is_done() {
                break;
            }
        }
        script
    }

    #[gpui_kit::test]
    fn scenario_scripts_complete_headless(cx: &mut TestAppContext) {
        let s = run_script(cx, Scenario::LogsScroll, Some(10), false);
        assert!(s.is_done());
        assert_eq!(s.outcome().steps, 10);
        assert_eq!(s.outcome().last_position, Some(27));

        let s = run_script(cx, Scenario::NetScroll, Some(10), false);
        assert!(s.is_done());
        assert_eq!(s.outcome().last_position, Some(18));

        // Every keystroke yields both a first-results and a full-result sample.
        let s = run_script(cx, Scenario::LogsSearch, None, false);
        assert!(s.is_done());
        let o = s.outcome();
        assert_eq!(o.keystrokes, 18);
        assert_eq!(o.samples.len(), 18);
        assert_eq!(
            o.samples.iter().filter(|x| x.full_seen.is_some()).count(),
            18
        );
        assert!(o.samples.iter().all(|x| x.first_seen >= x.key_at));

        let s = run_script(cx, Scenario::NetFilter, None, false);
        assert!(s.is_done());
        assert_eq!(s.outcome().keystrokes, 8);
        assert_eq!(s.outcome().samples.len(), 8);

        let s = run_script(cx, Scenario::LiveLogs, Some(8), false);
        assert!(s.is_done());
        assert_eq!(s.outcome().steps, 8);
        assert_eq!(s.outcome().last_position, Some(100));

        let s = run_script(cx, Scenario::ToolSwitch, Some(6), false);
        assert!(s.is_done());
        assert_eq!(s.outcome().samples.len(), 6);
        assert!(s.outcome().error.is_none());
    }

    #[gpui_kit::test]
    fn strip_drag_selects_a_range_or_reports_an_error(cx: &mut TestAppContext) {
        let s = run_script(cx, Scenario::StripDrag, Some(12), true);
        assert!(s.is_done());
        assert_eq!(s.outcome().error, None);
        assert_eq!(s.outcome().steps, 12);
    }

    #[gpui_kit::test]
    fn strip_drag_errors_when_the_strip_is_not_laid_out(cx: &mut TestAppContext) {
        let snapshot = fdt_store::synth::session(3, 100, 100, 2_000);
        let (shell, vcx) = open_shell_with(cx, |_| {
            SessionModel::fixed(snapshot, connected("stress", true))
        });
        // Settings has no strip, so its bounds were never set.
        vcx.update(|window, cx| {
            shell.update(cx, |s, cx| {
                s.navigate(Location::at(Page::Settings), window, cx)
            });
        });
        vcx.run_until_parked();
        let mut script = Script::new(Scenario::StripDrag, Some(12));
        vcx.update(|window, cx| script.drag_step(&shell, window, cx));
        assert!(script.is_done());
        assert!(
            script
                .outcome()
                .error
                .as_deref()
                .is_some_and(|e| e.contains("strip"))
        );
    }

    #[test]
    fn the_live_feed_appends_fifty_lines_per_publish() {
        let (mut w, run) = fdt_store::synth::writer(1, 0, 0, 0);
        let mut next = 1_000;
        append_live_lines(&mut w, run, &mut next, LIVE_LINES);
        append_live_lines(&mut w, run, &mut next, LIVE_LINES);
        let s = w.publish();
        assert_eq!(s.logs.len(), 100);
        assert_eq!(next, 1_100);
        assert!(s.logs.row(99).time > s.logs.row(0).time);
    }
}
