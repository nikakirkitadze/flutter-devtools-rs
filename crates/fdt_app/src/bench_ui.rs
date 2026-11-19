//! `fdt bench-ui <scenario> [--frames N]`: scripted UI scenarios over a stress session.
//!
//! The harness opens the main window over a fixed session (1M logs, 100k requests, 72k frames),
//! steps a script once per frame and prints one JSON line with frame statistics and budget
//! verdicts. A scenario step runs in `Window::on_next_frame`, which GPUI calls right after a
//! frame is drawn, so the clock read at the start of a step is the end of the frame that drew
//! the previous step's changes. That is how latencies are measured.
//!
//! Keystrokes go through the views' public APIs (`LogsView::set_search`,
//! `NetworkView::set_filter_text`), the same entry points the palette and the field use, rather
//! than synthesised key events; tool switches go through `dispatch_action` and the strip drag
//! through synthesised mouse events.

use crate::actions::show_tool;
use crate::app::start_fixed;
use crate::nav::{Location, Page, Tool};
use crate::shell::MainWindow;
use fdt_capture::ConnStatus;
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::RunId;
use fdt_ui::assets::FdtAssets;
use gpui_kit::profiler::{FrameEvent, FrameTimingCollector, set_trace_enabled};
use gpui_kit::{
    App, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Window, WindowId, point, px,
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
/// A pending latency sample that is still unresolved after this is recorded as this long.
const LATENCY_CAP: Duration = Duration::from_secs(1); // token-check: allow: sample cap, not motion
/// Frames between keystrokes of the search scenarios.
const KEY_EVERY: usize = 5;
/// Frames a search scenario waits after its last keystroke for the result.
const TAIL_FRAMES: usize = 600;

const LOG_QUERY: &str = "connection timeout";
const NET_QUERY: &str = "products";

pub const USAGE: &str = "usage: fdt bench-ui <scenario> [--frames N]\n\
scenarios: logs-scroll, logs-search, net-scroll, net-filter, tool-switch, strip-drag";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    LogsScroll,
    LogsSearch,
    NetScroll,
    NetFilter,
    ToolSwitch,
    StripDrag,
}

impl Scenario {
    pub const ALL: [Scenario; 6] = [
        Scenario::LogsScroll,
        Scenario::LogsSearch,
        Scenario::NetScroll,
        Scenario::NetFilter,
        Scenario::ToolSwitch,
        Scenario::StripDrag,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::LogsScroll => "logs-scroll",
            Scenario::LogsSearch => "logs-search",
            Scenario::NetScroll => "net-scroll",
            Scenario::NetFilter => "net-filter",
            Scenario::ToolSwitch => "tool-switch",
            Scenario::StripDrag => "strip-drag",
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
            Scenario::LogsScroll | Scenario::NetScroll => 600,
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

/// The frame-interval p99 budget: 8.3 ms on a display that refreshes at 120 Hz or faster,
/// otherwise 16.7 ms.
pub fn interval_budget_ms(refresh_ms: f64) -> f64 {
    if refresh_ms <= 8.4 { 8.3 } else { 16.7 }
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
    pub interval_ms: Vec<f64>,
    /// Keystroke-to-drawn (first results for `logs-search`), or switch-to-drawn.
    pub latency_ms: Option<Vec<f64>>,
    /// Keystroke to the full result drawn (`logs-search` only).
    pub full_ms: Option<Vec<f64>>,
    pub refresh_ms: f64,
    /// `"display"` when GPUI reported the refresh interval, `"inferred"` otherwise.
    pub refresh_source: &'static str,
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
/// Frame timestamps jitter by a fraction of a millisecond around the display period, so the
/// interval p99 may exceed its budget by this much (8.3 ms is below a 120 Hz period).
const INTERVAL_JITTER_MS: f64 = 0.5;

impl Measured {
    pub fn report(&self, scenario: Scenario) -> Report {
        let draw = percentiles(&self.draw_ms);
        let interval = percentiles(&self.interval_ms);
        let latency = self.latency_ms.as_deref().and_then(percentiles);
        let full = self.full_ms.as_deref().and_then(percentiles);
        let interval_budget = interval_budget_ms(self.refresh_ms);
        let strict = interval_budget < 10.0;
        let mut pass = draw.is_some_and(|p| p.p99 <= DRAW_BUDGET_MS)
            && interval.is_some_and(|p| {
                p.p99 <= interval_budget + INTERVAL_JITTER_MS && (!strict || p.max <= MAX_FRAME_MS)
            });
        let latency_budget = scenario.latency_budget_ms();
        if let Some(budget) = latency_budget {
            pass &= latency.is_some_and(|p| p.p99 <= budget);
        }
        if self.full_ms.is_some() {
            pass &= full.is_some_and(|p| p.p99 <= FULL_SCAN_BUDGET_MS);
        }
        let mut budget = format!(
            "\"draw_p99_ms\":{:?},\"interval_p99_ms\":{:?},\"interval_jitter_ms\":{:?}",
            DRAW_BUDGET_MS, interval_budget, INTERVAL_JITTER_MS
        );
        if strict {
            budget.push_str(&format!(",\"interval_max_ms\":{MAX_FRAME_MS:?}"));
        }
        if let Some(b) = latency_budget {
            budget.push_str(&format!(",\"latency_p99_ms\":{b:?}"));
        }
        if self.full_ms.is_some() {
            budget.push_str(&format!(",\"full_p99_ms\":{FULL_SCAN_BUDGET_MS:?}"));
        }
        let mut json = format!(
            "{{\"scenario\":\"{}\",\"frames\":{},\"draw_ms\":{},\"interval_ms\":{}",
            scenario.name(),
            self.frames,
            pct_json(draw),
            pct_json(interval)
        );
        if self.latency_ms.is_some() {
            json.push_str(&format!(",\"latency_ms\":{}", pct_json(latency)));
        }
        if self.full_ms.is_some() {
            json.push_str(&format!(",\"full_ms\":{}", pct_json(full)));
        }
        json.push_str(&format!(
            ",\"budget\":{{{budget}}},\"interval_budget_ms\":{interval_budget:?},\"refresh_ms\":{:?},\"refresh_source\":\"{}\",\"pass\":{pass}}}",
            r3(self.refresh_ms),
            self.refresh_source
        ));
        Report { json, pass }
    }
}

// ---- scripts ----

/// What a script has done so far; the headless test reads it.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Measured steps run.
    pub steps: usize,
    /// The last row index a scroll scenario asked for.
    pub last_position: Option<usize>,
    pub keystrokes: usize,
    /// Latency of each keystroke or switch (first results for `logs-search`).
    pub first: Vec<Duration>,
    /// Latency to the full result, for `logs-search`.
    pub full: Vec<Duration>,
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
    first: Option<Duration>,
    target: Option<Tool>,
}

pub struct Script {
    scenario: Scenario,
    len: usize,
    phase: Phase,
    warm: usize,
    prepared: bool,
    frame: usize,
    started: Option<Instant>,
    pending: Option<Pending>,
    switches: usize,
    tail: usize,
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
            started: None,
            pending: None,
            switches: 0,
            tail: 0,
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

    /// Runs one scripted step. Call it once per frame, right after the frame was drawn.
    pub fn step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        match self.phase {
            Phase::Warmup => self.warm_up(shell, window, cx),
            Phase::Running => {
                match self.scenario {
                    Scenario::LogsScroll => self.scroll_step(shell, cx, 3),
                    Scenario::NetScroll => self.scroll_step(shell, cx, 2),
                    Scenario::LogsSearch => self.search_step(shell, window, cx),
                    Scenario::NetFilter => self.search_step(shell, window, cx),
                    Scenario::ToolSwitch => self.switch_step(shell, window, cx),
                    Scenario::StripDrag => self.drag_step(shell, window, cx),
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

    /// Whether the pending keystroke's first results and full result are on screen:
    /// `(first-phase count, settled)`.
    fn probe(&self, shell: &Entity<MainWindow>, cx: &App) -> (usize, bool) {
        if self.scenario == Scenario::LogsSearch {
            let logs = shell.read(cx).logs_view().read(cx);
            (logs.first_phases(), logs.is_settled())
        } else {
            (0, shell.read(cx).network_view().read(cx).is_settled())
        }
    }

    fn search_step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        let now = Instant::now();
        let logs = self.scenario == Scenario::LogsSearch;
        let text = if logs { LOG_QUERY } else { NET_QUERY };
        let (first_phases, settled) = self.probe(shell, cx);
        if let Some(p) = &mut self.pending {
            let age = now.saturating_duration_since(p.at);
            if p.first.is_none() && (first_phases > p.first_base || settled) {
                p.first = Some(age);
            }
            if settled || age > LATENCY_CAP {
                self.outcome.first.push(p.first.unwrap_or(age));
                if logs {
                    self.outcome.full.push(age);
                }
                self.pending = None;
            }
        }
        if self.frame.is_multiple_of(KEY_EVERY) && self.frame / KEY_EVERY < text.len() {
            if let Some(p) = self.pending.take() {
                // Superseded before its full result: the first results still count.
                let age = now.saturating_duration_since(p.at);
                self.outcome.first.push(p.first.unwrap_or(age));
            }
            let prefix = &text[..=self.frame / KEY_EVERY];
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
                first: None,
                target: None,
            });
            self.outcome.keystrokes += 1;
        }
        self.frame += 1;
        let typed_all = self.outcome.keystrokes == text.len();
        if typed_all && self.pending.is_none() {
            self.phase = Phase::Done;
        } else if typed_all {
            self.tail += 1;
            if self.tail > TAIL_FRAMES
                && let Some(p) = self.pending.take()
            {
                let age = now.saturating_duration_since(p.at);
                self.outcome.first.push(p.first.unwrap_or(age));
                if logs {
                    self.outcome.full.push(age);
                }
                self.phase = Phase::Done;
            }
        }
    }

    fn switch_step(&mut self, shell: &Entity<MainWindow>, window: &mut Window, cx: &mut App) {
        let now = Instant::now();
        if let Some(p) = &self.pending {
            let target = p.target.unwrap_or(Tool::Logs);
            let age = now.saturating_duration_since(p.at);
            let shown =
                shell.read(cx).current().page == Page::Tool(target) && ready(shell, target, cx);
            if shown || age > LATENCY_CAP {
                self.outcome.first.push(age);
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
                first: None,
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
        let viewport = window.viewport_size();
        let (left, width) = if f32::from(bounds.size.width) > 0.0 {
            (bounds.origin.x, bounds.size.width)
        } else {
            (px(0.0), viewport.width)
        };
        let y = if f32::from(bounds.size.height) > 0.0 {
            bounds.origin.y + bounds.size.height / 2.0
        } else {
            px(100.0) // token-check: allow: simulated pointer position, not a layout size
        };
        let frac = 0.1 + 0.8 * self.frame as f32 / (total - 1) as f32;
        let position = point(left + width * frac, y);
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

// ---- the real run ----

struct Driver {
    script: Script,
    shell: Entity<MainWindow>,
    scenario: Scenario,
    collector: FrameTimingCollector,
    window_id: WindowId,
    /// `(draw_start, draw_end)` of each frame drawn while measuring.
    draws: Vec<(Instant, Instant)>,
    reported_refresh: Option<Duration>,
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
                }
            }
        }
    }

    fn measured(&self, window_refresh: Option<Duration>) -> Measured {
        let starts: Vec<Instant> = self.draws.iter().map(|d| d.0).collect();
        let interval_ms = intervals_ms(&starts);
        let (refresh_ms, refresh_source) = match window_refresh.or(self.reported_refresh) {
            Some(r) => (ms(r), "display"),
            None => (median(&interval_ms).unwrap_or(16.7), "inferred"),
        };
        let o = self.script.outcome();
        let latency = |v: &[Duration]| v.iter().map(|d| ms(*d)).collect::<Vec<f64>>();
        let has_latency = self.scenario.latency_budget_ms().is_some();
        Measured {
            draw_ms: self
                .draws
                .iter()
                .map(|(a, b)| ms(b.saturating_duration_since(*a)))
                .collect(),
            interval_ms,
            latency_ms: has_latency.then(|| latency(&o.first)),
            full_ms: (self.scenario == Scenario::LogsSearch).then(|| latency(&o.full)),
            refresh_ms,
            refresh_source,
            frames: o.steps,
        }
    }
}

fn tick(driver: Rc<RefCell<Driver>>, window: &mut Window, cx: &mut App) {
    let done = {
        let mut d = driver.borrow_mut();
        let shell = d.shell.clone();
        d.script.step(&shell, window, cx);
        d.collect();
        d.script.is_done()
    };
    if done {
        let d = driver.borrow();
        let report = d.measured(window.refresh_interval(cx)).report(d.scenario);
        println!("{}", report.to_json());
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        // `cx.quit()` ends in `exit(0)` on macOS, so the verdict has to exit directly.
        std::process::exit(if report.pass { 0 } else { 1 });
    }
    window.refresh();
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

/// Builds the stress session, opens the window, runs `scenario` and exits the process with the
/// verdict. Never returns normally.
pub fn run(scenario: Scenario, frames: Option<usize>) -> ExitCode {
    // Building the session is not timed (and not covered by the timeout).
    let snapshot = fdt_store::synth::stress_session(SEED);
    std::thread::spawn(move || {
        std::thread::sleep(TIMEOUT);
        fail(scenario, "timed out");
    });
    let status = ConnStatus::Connected {
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
    };
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(move |cx: &mut App| {
            set_trace_enabled(true);
            let shell = match start_fixed(cx, snapshot, status) {
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
                reported_refresh: None,
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
    fn interval_budget_follows_the_display_refresh() {
        assert_eq!(interval_budget_ms(8.333), 8.3);
        assert_eq!(interval_budget_ms(8.4), 8.3);
        assert_eq!(interval_budget_ms(16.667), 16.7);
    }

    #[test]
    fn scenario_names_round_trip() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::parse(s.name()), Some(s));
        }
        assert_eq!(Scenario::parse("nope"), None);
    }

    #[test]
    fn report_passes_only_when_every_budget_holds() {
        let ok = Measured {
            draw_ms: vec![1.0; 100],
            interval_ms: vec![8.3; 100],
            latency_ms: None,
            full_ms: None,
            refresh_ms: 8.333,
            refresh_source: "display",
            frames: 100,
        };
        assert!(ok.report(Scenario::LogsScroll).pass);
        let jittery = Measured {
            interval_ms: vec![8.6; 100],
            ..ok.clone()
        };
        assert!(jittery.report(Scenario::LogsScroll).pass);
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
        let slow_search = Measured {
            latency_ms: Some(vec![30.0; 18]),
            ..ok.clone()
        };
        assert!(!slow_search.report(Scenario::LogsSearch).pass);
        let json = ok.report(Scenario::LogsScroll).to_json().to_string();
        assert!(json.contains("\"scenario\":\"logs-scroll\""));
        assert!(json.contains("\"interval_budget_ms\":8.3"));
        assert!(!json.contains("latency_ms\":{"));
    }

    /// Runs `scenario` on a small session without frames: each `step` stands in for a frame,
    /// and the executor settles between steps.
    fn run_script(cx: &mut TestAppContext, scenario: Scenario, frames: Option<usize>) -> Script {
        let snapshot = fdt_store::synth::session(3, 100, 100, 200);
        let (shell, vcx) = open_shell_with(cx, |_| {
            SessionModel::fixed(snapshot, connected("stress", true))
        });
        let mut script = Script::new(scenario, frames);
        for _ in 0..3_000 {
            vcx.update(|window, cx| script.step(&shell, window, cx));
            vcx.run_until_parked();
            if script.is_done() {
                break;
            }
        }
        script
    }

    #[gpui_kit::test]
    fn scenario_scripts_complete_headless(cx: &mut TestAppContext) {
        let s = run_script(cx, Scenario::LogsScroll, Some(10));
        assert!(s.is_done());
        assert_eq!(s.outcome().steps, 10);
        assert_eq!(s.outcome().last_position, Some(27));

        let s = run_script(cx, Scenario::NetScroll, Some(10));
        assert!(s.is_done());
        assert_eq!(s.outcome().last_position, Some(18));

        let s = run_script(cx, Scenario::LogsSearch, None);
        assert!(s.is_done());
        assert_eq!(s.outcome().keystrokes, 18);
        assert_eq!(s.outcome().first.len(), 18);

        let s = run_script(cx, Scenario::NetFilter, None);
        assert!(s.is_done());
        assert_eq!(s.outcome().keystrokes, 8);
        assert_eq!(s.outcome().first.len(), 8);

        let s = run_script(cx, Scenario::ToolSwitch, Some(6));
        assert!(s.is_done());
        assert_eq!(s.outcome().first.len(), 6);
        assert!(s.outcome().first.iter().all(|d| d.as_millis() < 1_000));

        let s = run_script(cx, Scenario::StripDrag, Some(12));
        assert!(s.is_done());
        assert_eq!(s.outcome().steps, 12);
    }
}
