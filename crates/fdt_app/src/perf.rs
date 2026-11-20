//! Render counters. Each view bumps its counter when it builds its elements, so a test or
//! `fdt bench-ui` can tell which parts of the window were rebuilt in a frame.

/// How many counters there are.
pub const N: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    MainWindow,
    TitleBar,
    Sidebar,
    Strip,
    LogsView,
    LogsToolbar,
    LogsRows,
    NetworkView,
    NetworkToolbar,
    PerformanceView,
    FrameChart,
    TimelineView,
    Lanes,
}

impl Counter {
    pub const ALL: [Counter; N] = [
        Counter::MainWindow,
        Counter::TitleBar,
        Counter::Sidebar,
        Counter::Strip,
        Counter::LogsView,
        Counter::LogsToolbar,
        Counter::LogsRows,
        Counter::NetworkView,
        Counter::NetworkToolbar,
        Counter::PerformanceView,
        Counter::FrameChart,
        Counter::TimelineView,
        Counter::Lanes,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Counter::MainWindow => "main_window",
            Counter::TitleBar => "title_bar",
            Counter::Sidebar => "sidebar",
            Counter::Strip => "strip",
            Counter::LogsView => "logs_view",
            Counter::LogsToolbar => "logs_toolbar",
            Counter::LogsRows => "logs_rows",
            Counter::NetworkView => "network_view",
            Counter::NetworkToolbar => "network_toolbar",
            Counter::PerformanceView => "performance_view",
            Counter::FrameChart => "frame_chart",
            Counter::TimelineView => "timeline_view",
            Counter::Lanes => "lanes",
        }
    }
}

thread_local! {
    // The UI thread draws, and each test draws on its own thread, so the counters never mix.
    static COUNTS: std::cell::Cell<[u64; N]> = const { std::cell::Cell::new([0; N]) };
}

/// Records `n` renders of `counter`.
pub fn bump(counter: Counter, n: u64) {
    COUNTS.with(|c| {
        let mut all = c.get();
        all[counter as usize] += n;
        c.set(all);
    });
}

/// Renders of `counter` so far.
pub fn count(counter: Counter) -> u64 {
    COUNTS.with(|c| c.get()[counter as usize])
}

/// Every counter's value.
pub fn snapshot() -> [u64; N] {
    COUNTS.with(|c| c.get())
}
