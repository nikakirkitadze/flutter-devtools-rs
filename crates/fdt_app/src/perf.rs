//! Render counters. Each view bumps its counter when it builds its elements, so a test or
//! `fdt bench-ui` can tell which parts of the window were rebuilt in a frame.

/// How many counters there are.
pub const N: usize = 19;

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
    FramesList,
    FramesRows,
    DetailsPane,
    MemoryView,
    MemoryChart,
    HeapTree,
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
        Counter::FramesList,
        Counter::FramesRows,
        Counter::DetailsPane,
        Counter::MemoryView,
        Counter::MemoryChart,
        Counter::HeapTree,
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
            Counter::FramesList => "frames_list",
            Counter::FramesRows => "frames_rows",
            Counter::DetailsPane => "details_pane",
            Counter::MemoryView => "memory_view",
            Counter::MemoryChart => "memory_chart",
            Counter::HeapTree => "heap_tree",
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

thread_local! {
    static LAYOUT_NANOS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Adds time spent in the memory chart's layout, for `fdt bench-ui memory-chart`.
pub fn add_layout_time(d: std::time::Duration) {
    LAYOUT_NANOS.with(|c| c.set(c.get().saturating_add(d.as_nanos() as u64)));
}

/// The layout time added since the last call, in nanoseconds.
pub fn take_layout_nanos() -> u64 {
    LAYOUT_NANOS.with(|c| c.replace(0))
}
