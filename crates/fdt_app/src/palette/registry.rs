//! The command registry the palette, menus and cheat sheet are built from.

use crate::actions::{
    About, CancelHeapSnapshot, ClearSelection, CloseWindow, Collapse, ConnectToApp, Disconnect,
    Expand, FetchCpuSamples, FocusSearch, FollowLatest, GoBack, GoForward, GoToFirst, LoadFullBody,
    Minimize, NewSession, NewWindow, NextFrame, NextSlowFrame, OpenDetails, OpenSettings,
    PreviousFrame, PreviousSlowFrame, Quit, RefreshAllocationProfile, RemoveHeapSnapshot,
    SearchEverywhere, SelectNext, SelectPrevious, ShowClassesTab, ShowCpuTab, ShowFramesTab,
    ShowLeaksTab, ShowLogs, ShowMemory, ShowNetwork, ShowPerformance, ShowRebuildsTab,
    ShowShortcuts, ShowSnapshotTab, ShowState, ShowTimeline, ShowTraceTab, ShowWidgets,
    StopWaiting, TakeHeapSnapshot, ToggleExpand, ToggleFrameHud, ToggleFullScreen, ToggleRecording,
    ToggleTraceWidgetBuilds, UseAsBaseline, UseDarkTheme, UseLightTheme, UseSystemTheme, Zoom,
    ZoomIn, ZoomOut, ZoomToFit,
};
#[cfg(target_os = "macos")]
use crate::actions::{Hide, HideOthers, ShowAll};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Navigation,
    View,
    Window,
    App,
    /// Items built from live data (requests, log searches), never from the command list.
    Data,
}

#[derive(Clone)]
pub struct CommandSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub keywords: &'static [&'static str],
    pub group: Group,
    pub action: fn() -> Box<dyn gpui_kit::Action>,
}

/// Every command, in display order.
pub fn commands() -> Vec<CommandSpec> {
    #[allow(unused_mut)]
    let mut v = vec![
        CommandSpec {
            id: "tool.timeline",
            label: "Go to Timeline",
            keywords: &["lanes", "overview"],
            group: Group::Navigation,
            action: || Box::new(ShowTimeline),
        },
        CommandSpec {
            id: "tool.performance",
            label: "Go to Performance",
            keywords: &["frames", "jank", "cpu"],
            group: Group::Navigation,
            action: || Box::new(ShowPerformance),
        },
        CommandSpec {
            id: "tool.memory",
            label: "Go to Memory",
            keywords: &["heap", "leaks", "gc"],
            group: Group::Navigation,
            action: || Box::new(ShowMemory),
        },
        CommandSpec {
            id: "tool.network",
            label: "Go to Network",
            keywords: &["requests", "http"],
            group: Group::Navigation,
            action: || Box::new(ShowNetwork),
        },
        CommandSpec {
            id: "tool.logs",
            label: "Go to Logs",
            keywords: &["console", "errors"],
            group: Group::Navigation,
            action: || Box::new(ShowLogs),
        },
        CommandSpec {
            id: "tool.widgets",
            label: "Go to Widget Tree",
            keywords: &["widgets", "inspector", "rebuilds"],
            group: Group::Navigation,
            action: || Box::new(ShowWidgets),
        },
        CommandSpec {
            id: "tool.state",
            label: "Go to State",
            keywords: &["bloc", "riverpod"],
            group: Group::Navigation,
            action: || Box::new(ShowState),
        },
        CommandSpec {
            id: "nav.back",
            label: "Go Back",
            keywords: &["history"],
            group: Group::Navigation,
            action: || Box::new(GoBack),
        },
        CommandSpec {
            id: "nav.forward",
            label: "Go Forward",
            keywords: &["history"],
            group: Group::Navigation,
            action: || Box::new(GoForward),
        },
        CommandSpec {
            id: "nav.clear",
            label: "Clear Selection",
            keywords: &["range", "escape"],
            group: Group::Navigation,
            action: || Box::new(ClearSelection),
        },
        CommandSpec {
            id: "search.focus",
            label: "Search in Current Tool",
            keywords: &["find", "filter"],
            group: Group::View,
            action: || Box::new(FocusSearch),
        },
        CommandSpec {
            id: "search.everywhere",
            label: "Search Everywhere",
            keywords: &["find", "requests", "logs", "commands"],
            group: Group::View,
            action: || Box::new(SearchEverywhere),
        },
        CommandSpec {
            id: "connect.open",
            label: "Connect to an App…",
            keywords: &["attach", "start", "vm service", "url"],
            group: Group::App,
            action: || Box::new(ConnectToApp),
        },
        CommandSpec {
            id: "connect.disconnect",
            label: "Disconnect",
            keywords: &["detach", "stop"],
            group: Group::App,
            action: || Box::new(Disconnect),
        },
        CommandSpec {
            id: "connect.stop_waiting",
            label: "Stop Waiting for the App",
            keywords: &["reconnect", "cancel"],
            group: Group::App,
            action: || Box::new(StopWaiting),
        },
        CommandSpec {
            id: "session.new",
            label: "New Session",
            keywords: &["clear", "reset", "empty"],
            group: Group::App,
            action: || Box::new(NewSession),
        },
        CommandSpec {
            id: "app.settings",
            label: "Open Settings",
            keywords: &["preferences"],
            group: Group::App,
            action: || Box::new(OpenSettings),
        },
        CommandSpec {
            id: "app.about",
            label: "About fdt",
            keywords: &["version", "licence"],
            group: Group::App,
            action: || Box::new(About),
        },
        CommandSpec {
            id: "app.quit",
            label: "Quit fdt",
            keywords: &["exit"],
            group: Group::App,
            action: || Box::new(Quit),
        },
    ];
    #[cfg(target_os = "macos")]
    v.extend([
        CommandSpec {
            id: "app.hide",
            label: "Hide fdt",
            keywords: &[],
            group: Group::App,
            action: || Box::new(Hide),
        },
        CommandSpec {
            id: "app.hide_others",
            label: "Hide Others",
            keywords: &[],
            group: Group::App,
            action: || Box::new(HideOthers),
        },
        CommandSpec {
            id: "app.show_all",
            label: "Show All",
            keywords: &[],
            group: Group::App,
            action: || Box::new(ShowAll),
        },
    ]);
    v.extend([
        CommandSpec {
            id: "view.frame_times",
            label: "Show Frame Times",
            keywords: &["hud", "fps", "performance"],
            group: Group::View,
            action: || Box::new(ToggleFrameHud),
        },
        CommandSpec {
            id: "view.recording",
            label: "Start or Stop Recording",
            keywords: &["record", "pause", "resume"],
            group: Group::View,
            action: || Box::new(ToggleRecording),
        },
        CommandSpec {
            id: "view.shortcuts",
            label: "Show Keyboard Shortcuts",
            keywords: &["keys", "help", "cheat sheet"],
            group: Group::View,
            action: || Box::new(ShowShortcuts),
        },
        CommandSpec {
            id: "logs.next",
            label: "Next Row",
            keywords: &["logs", "network", "down", "select"],
            group: Group::View,
            action: || Box::new(SelectNext),
        },
        CommandSpec {
            id: "logs.previous",
            label: "Previous Row",
            keywords: &["logs", "network", "up", "select"],
            group: Group::View,
            action: || Box::new(SelectPrevious),
        },
        CommandSpec {
            id: "logs.toggle_expand",
            label: "Toggle Record Expansion",
            keywords: &["logs", "lines", "multi-line"],
            group: Group::View,
            action: || Box::new(ToggleExpand),
        },
        CommandSpec {
            id: "logs.expand",
            label: "Expand Record",
            keywords: &["logs", "lines", "multi-line"],
            group: Group::View,
            action: || Box::new(Expand),
        },
        CommandSpec {
            id: "logs.collapse",
            label: "Collapse Record",
            keywords: &["logs", "lines"],
            group: Group::View,
            action: || Box::new(Collapse),
        },
        CommandSpec {
            id: "logs.open_details",
            label: "Open Details",
            keywords: &["logs", "inspect", "network", "request"],
            group: Group::View,
            action: || Box::new(OpenDetails),
        },
        CommandSpec {
            id: "net.load_full_body",
            label: "Load Full Response Body",
            keywords: &["network", "request", "truncated", "rest"],
            group: Group::View,
            action: || Box::new(LoadFullBody),
        },
        CommandSpec {
            id: "logs.follow",
            label: "Follow Latest Logs",
            keywords: &["logs", "tail", "end", "bottom"],
            group: Group::View,
            action: || Box::new(FollowLatest),
        },
        CommandSpec {
            id: "logs.first",
            label: "Go to First Log",
            keywords: &["logs", "top", "home"],
            group: Group::View,
            action: || Box::new(GoToFirst),
        },
        CommandSpec {
            id: "perf.frames",
            label: "Show Frames",
            keywords: &["performance", "frame chart", "jank"],
            group: Group::Navigation,
            action: || Box::new(ShowFramesTab),
        },
        CommandSpec {
            id: "perf.cpu",
            label: "Show CPU Profile",
            keywords: &["performance", "flame", "samples"],
            group: Group::Navigation,
            action: || Box::new(ShowCpuTab),
        },
        CommandSpec {
            id: "perf.rebuilds",
            label: "Show Rebuilds",
            keywords: &["performance", "widgets", "build"],
            group: Group::Navigation,
            action: || Box::new(ShowRebuildsTab),
        },
        CommandSpec {
            id: "perf.trace",
            label: "Show Raw Trace",
            keywords: &["performance", "timeline", "events"],
            group: Group::Navigation,
            action: || Box::new(ShowTraceTab),
        },
        CommandSpec {
            id: "perf.trace_builds",
            label: "Trace Widget Builds",
            keywords: &["performance", "rebuilds", "toggle"],
            group: Group::View,
            action: || Box::new(ToggleTraceWidgetBuilds),
        },
        CommandSpec {
            id: "perf.next_slow",
            label: "Next Slow Frame",
            keywords: &["performance", "jank", "frame"],
            group: Group::View,
            action: || Box::new(NextSlowFrame),
        },
        CommandSpec {
            id: "perf.previous_slow",
            label: "Previous Slow Frame",
            keywords: &["performance", "jank", "frame"],
            group: Group::View,
            action: || Box::new(PreviousSlowFrame),
        },
        CommandSpec {
            id: "perf.fetch_cpu",
            label: "Fetch CPU Samples Now",
            keywords: &["performance", "profile", "refresh"],
            group: Group::View,
            action: || Box::new(FetchCpuSamples),
        },
        CommandSpec {
            id: "memory.classes",
            label: "Show Classes",
            keywords: &["memory", "allocations", "profile"],
            group: Group::View,
            action: || Box::new(ShowClassesTab),
        },
        CommandSpec {
            id: "memory.leaks",
            label: "Show Leaks",
            keywords: &["memory", "suspects", "retained"],
            group: Group::View,
            action: || Box::new(ShowLeaksTab),
        },
        CommandSpec {
            id: "memory.snapshot",
            label: "Show Heap Snapshot",
            keywords: &["memory", "objects", "dominators"],
            group: Group::View,
            action: || Box::new(ShowSnapshotTab),
        },
        CommandSpec {
            id: "memory.take_snapshot",
            label: "Take Heap Snapshot",
            keywords: &["memory", "leaks", "heap", "dump"],
            group: Group::App,
            action: || Box::new(TakeHeapSnapshot),
        },
        CommandSpec {
            id: "memory.cancel_snapshot",
            label: "Cancel Heap Snapshot",
            keywords: &["memory", "stop", "abort"],
            group: Group::App,
            action: || Box::new(CancelHeapSnapshot),
        },
        CommandSpec {
            id: "memory.refresh_profile",
            label: "Refresh Allocation Profile",
            keywords: &["memory", "classes", "allocations", "reload"],
            group: Group::App,
            action: || Box::new(RefreshAllocationProfile),
        },
        CommandSpec {
            id: "memory.use_baseline",
            label: "Use as Baseline",
            keywords: &["memory", "compare", "diff", "snapshot"],
            group: Group::App,
            action: || Box::new(UseAsBaseline),
        },
        CommandSpec {
            id: "memory.remove_snapshot",
            label: "Remove Heap Snapshot",
            keywords: &["memory", "delete", "discard"],
            group: Group::App,
            action: || Box::new(RemoveHeapSnapshot),
        },
        CommandSpec {
            id: "lanes.zoom_in",
            label: "Zoom In",
            keywords: &["timeline", "time", "magnify"],
            group: Group::View,
            action: || Box::new(ZoomIn),
        },
        CommandSpec {
            id: "lanes.zoom_out",
            label: "Zoom Out",
            keywords: &["timeline", "time"],
            group: Group::View,
            action: || Box::new(ZoomOut),
        },
        CommandSpec {
            id: "lanes.zoom_fit",
            label: "Zoom to Fit",
            keywords: &["timeline", "time", "reset"],
            group: Group::View,
            action: || Box::new(ZoomToFit),
        },
        CommandSpec {
            id: "frames.previous",
            label: "Previous Frame",
            keywords: &["performance", "left"],
            group: Group::View,
            action: || Box::new(PreviousFrame),
        },
        CommandSpec {
            id: "frames.next",
            label: "Next Frame",
            keywords: &["performance", "right"],
            group: Group::View,
            action: || Box::new(NextFrame),
        },
        CommandSpec {
            id: "theme.system",
            label: "Theme: Match System",
            keywords: &["appearance"],
            group: Group::View,
            action: || Box::new(UseSystemTheme),
        },
        CommandSpec {
            id: "theme.dark",
            label: "Theme: Dark",
            keywords: &["appearance"],
            group: Group::View,
            action: || Box::new(UseDarkTheme),
        },
        CommandSpec {
            id: "theme.light",
            label: "Theme: Light",
            keywords: &["appearance"],
            group: Group::View,
            action: || Box::new(UseLightTheme),
        },
        CommandSpec {
            id: "window.close",
            label: "Close Window",
            keywords: &[],
            group: Group::Window,
            action: || Box::new(CloseWindow),
        },
        CommandSpec {
            id: "window.new",
            label: "New Window",
            keywords: &["second", "another"],
            group: Group::Window,
            action: || Box::new(NewWindow),
        },
        CommandSpec {
            id: "window.minimize",
            label: "Minimize Window",
            keywords: &[],
            group: Group::Window,
            action: || Box::new(Minimize),
        },
        CommandSpec {
            id: "window.zoom",
            label: if cfg!(target_os = "macos") {
                "Zoom Window"
            } else {
                "Maximize Window"
            },
            keywords: &[],
            group: Group::Window,
            action: || Box::new(Zoom),
        },
        CommandSpec {
            id: "window.fullscreen",
            label: "Toggle Full Screen",
            keywords: &[],
            group: Group::Window,
            action: || Box::new(ToggleFullScreen),
        },
    ]);
    v
}

/// Label, then keywords, space-separated: the text the palette ranks against.
pub fn haystack(spec: &CommandSpec) -> String {
    let mut s = spec.label.to_string();
    for k in spec.keywords {
        s.push(' ');
        s.push_str(k);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::show_tool;
    use crate::keymap::bindings;
    use crate::nav::Tool;
    use gpui_kit::Action as _;
    use std::collections::HashSet;

    #[test]
    fn command_ids_and_labels_are_unique() {
        let cmds = commands();
        let ids: HashSet<_> = cmds.iter().map(|c| c.id).collect();
        let labels: HashSet<_> = cmds.iter().map(|c| c.label).collect();
        assert_eq!(ids.len(), cmds.len());
        assert_eq!(labels.len(), cmds.len());
    }

    #[test]
    fn every_bound_action_is_in_the_palette() {
        let cmds = commands();
        for b in bindings() {
            let name = b.action().name();
            // The palette opens the palette; the app-list keys only mean something while the
            // start page's list has focus, so they are not commands. J and K in a table are the
            // table's own row movement; the palette's Next Row and Previous Row do the same.
            let list_local = [
                crate::actions::AppListPrev.name(),
                crate::actions::AppListNext.name(),
                crate::actions::AppListConnect.name(),
                gpui_kit::base::actions::SelectDown.name(),
                gpui_kit::base::actions::SelectUp.name(),
            ];
            if name == crate::actions::OpenPalette.name() || list_local.contains(&name) {
                continue;
            }
            assert!(
                cmds.iter().any(|c| (c.action)().name() == name),
                "no command for {name}"
            );
        }
    }

    #[test]
    fn every_tool_has_a_command_and_a_shortcut() {
        let cmds = commands();
        let bound: Vec<&str> = bindings().iter().map(|b| b.action().name()).collect();
        for tool in Tool::ALL {
            let name = show_tool(tool).name();
            let id = format!("tool.{}", tool.slug());
            assert!(
                cmds.iter()
                    .any(|c| c.id == id && (c.action)().name() == name)
            );
            assert!(bound.contains(&name), "no shortcut for {id}");
        }
    }

    #[test]
    fn haystack_joins_label_and_keywords() {
        let cmds = commands();
        let memory = cmds.iter().find(|c| c.id == "tool.memory").unwrap();
        assert_eq!(haystack(memory), "Go to Memory heap leaks gc");
    }

    #[test]
    fn registry_lists_m4_commands() {
        use crate::keymap::bindings;
        use crate::menus::app_menus;
        use gpui_kit::MenuItem;
        let cmds = commands();
        let bound: Vec<&str> = bindings().iter().map(|b| b.action().name()).collect();
        let mut menu_actions = Vec::new();
        for m in app_menus() {
            for item in &m.items {
                if let MenuItem::Action { action, .. } = item {
                    menu_actions.push(action.name());
                }
            }
        }
        for (id, label, group, shortcut) in [
            ("memory.classes", "Show Classes", Group::View, false),
            ("memory.leaks", "Show Leaks", Group::View, false),
            ("memory.snapshot", "Show Heap Snapshot", Group::View, false),
            (
                "memory.take_snapshot",
                "Take Heap Snapshot",
                Group::App,
                true,
            ),
            (
                "memory.cancel_snapshot",
                "Cancel Heap Snapshot",
                Group::App,
                false,
            ),
            (
                "memory.refresh_profile",
                "Refresh Allocation Profile",
                Group::App,
                false,
            ),
            ("memory.use_baseline", "Use as Baseline", Group::App, false),
            (
                "memory.remove_snapshot",
                "Remove Heap Snapshot",
                Group::App,
                false,
            ),
        ] {
            let c = cmds
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id}"));
            assert_eq!(c.label, label);
            assert_eq!(c.group, group, "{id}");
            let name = (c.action)().name();
            assert_eq!(bound.contains(&name), shortcut, "{id}");
            assert!(menu_actions.contains(&name), "{id} is not in a menu");
        }
    }

    #[test]
    fn registry_lists_m3_commands() {
        use crate::keymap::bindings;
        use crate::menus::app_menus;
        use gpui_kit::MenuItem;
        let cmds = commands();
        let bound: Vec<&str> = bindings().iter().map(|b| b.action().name()).collect();
        let mut menu_actions = Vec::new();
        for m in app_menus() {
            for item in &m.items {
                if let MenuItem::Action { action, .. } = item {
                    menu_actions.push(action.name());
                }
            }
        }
        for (id, label, shortcut) in [
            ("perf.frames", "Show Frames", false),
            ("perf.cpu", "Show CPU Profile", false),
            ("perf.rebuilds", "Show Rebuilds", false),
            ("perf.trace", "Show Raw Trace", false),
            ("perf.trace_builds", "Trace Widget Builds", false),
            ("perf.next_slow", "Next Slow Frame", false),
            ("perf.previous_slow", "Previous Slow Frame", false),
            ("perf.fetch_cpu", "Fetch CPU Samples Now", false),
            ("lanes.zoom_in", "Zoom In", true),
            ("lanes.zoom_out", "Zoom Out", true),
            ("lanes.zoom_fit", "Zoom to Fit", true),
            ("frames.previous", "Previous Frame", true),
            ("frames.next", "Next Frame", true),
        ] {
            let c = cmds
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id}"));
            assert_eq!(c.label, label);
            let name = (c.action)().name();
            assert_eq!(bound.contains(&name), shortcut, "{id}");
            assert!(menu_actions.contains(&name), "{id} is not in a menu");
        }
    }
}
