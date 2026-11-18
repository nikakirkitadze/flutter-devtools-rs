//! The command registry the palette, menus and cheat sheet are built from.

use crate::actions::{
    About, ClearSelection, GoBack, GoForward, Minimize, OpenSettings, Quit, ShowLogs, ShowMemory,
    ShowNetwork, ShowPerformance, ShowShortcuts, ShowState, ShowTimeline, ShowWidgets,
    ToggleFrameHud, ToggleFullScreen, UseDarkTheme, UseLightTheme, UseSystemTheme, Zoom,
};
#[cfg(target_os = "macos")]
use crate::actions::{Hide, HideOthers, ShowAll};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Navigation,
    View,
    Window,
    App,
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
            id: "view.shortcuts",
            label: "Show Keyboard Shortcuts",
            keywords: &["keys", "help", "cheat sheet"],
            group: Group::View,
            action: || Box::new(ShowShortcuts),
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
            if name == crate::actions::OpenPalette.name() {
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
}
