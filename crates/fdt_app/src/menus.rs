//! The application menu bar, built from the same actions the palette lists.

use crate::actions::show_tool;
use crate::actions::{
    About, CloseWindow, ConnectToApp, Disconnect, FetchCpuSamples, GoBack, GoForward, Minimize,
    NewSession, NewWindow, NextFrame, NextSlowFrame, OpenPalette, OpenSettings, PreviousFrame,
    PreviousSlowFrame, Quit, ShowCpuTab, ShowFramesTab, ShowRebuildsTab, ShowShortcuts,
    ShowTraceTab, StopWaiting, ToggleFrameHud, ToggleFullScreen, ToggleRecording,
    ToggleTraceWidgetBuilds, UseDarkTheme, UseLightTheme, UseSystemTheme, Zoom, ZoomIn, ZoomOut,
    ZoomToFit,
};
#[cfg(target_os = "macos")]
use crate::actions::{Hide, HideOthers, ShowAll};
use crate::nav::Tool;
use crate::palette::registry::commands;
use gpui_kit::{Menu, MenuItem, SystemMenuType};

/// The menus, left to right.
pub fn app_menus() -> Vec<Menu> {
    let mut app = vec![
        MenuItem::action("About fdt", About),
        MenuItem::separator(),
        MenuItem::action("Settings…", OpenSettings),
        MenuItem::separator(),
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
    ];
    #[cfg(target_os = "macos")]
    app.extend([
        MenuItem::action("Hide fdt", Hide),
        MenuItem::action("Hide Others", HideOthers),
        MenuItem::action("Show All", ShowAll),
        MenuItem::separator(),
    ]);
    app.push(MenuItem::action("Quit fdt", Quit));

    let mut go = vec![
        MenuItem::action("Back", GoBack),
        MenuItem::action("Forward", GoForward),
        MenuItem::separator(),
    ];
    // Labels come from the registry so the menu and the palette never drift.
    let cmds = commands();
    go.extend(Tool::ALL.iter().map(|t| {
        let action = show_tool(*t);
        let label = cmds
            .iter()
            .find(|c| (c.action)().name() == action.name())
            .map_or(t.label(), |c| c.label);
        MenuItem::Action {
            name: label.into(),
            action,
            os_action: None,
            checked: false,
            disabled: false,
        }
    }));

    vec![
        Menu::new("fdt").items(app),
        Menu::new("File").items([
            MenuItem::action("New Session", NewSession),
            MenuItem::action("New Window", NewWindow),
            MenuItem::separator(),
            MenuItem::action("Connect to an App…", ConnectToApp),
            MenuItem::action("Disconnect", Disconnect),
            MenuItem::action("Stop Waiting for the App", StopWaiting),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", OpenPalette),
            MenuItem::action("Start or Stop Recording", ToggleRecording),
            MenuItem::action("Show Frame Times", ToggleFrameHud),
            MenuItem::separator(),
            MenuItem::action("Show Frames", ShowFramesTab),
            MenuItem::action("Show CPU Profile", ShowCpuTab),
            MenuItem::action("Show Rebuilds", ShowRebuildsTab),
            MenuItem::action("Show Raw Trace", ShowTraceTab),
            MenuItem::action("Trace Widget Builds", ToggleTraceWidgetBuilds),
            MenuItem::action("Next Slow Frame", NextSlowFrame),
            MenuItem::action("Previous Slow Frame", PreviousSlowFrame),
            MenuItem::action("Fetch CPU Samples Now", FetchCpuSamples),
            MenuItem::separator(),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Zoom to Fit", ZoomToFit),
            MenuItem::action("Previous Frame", PreviousFrame),
            MenuItem::action("Next Frame", NextFrame),
            MenuItem::separator(),
            MenuItem::action("Theme: Match System", UseSystemTheme),
            MenuItem::action("Theme: Dark", UseDarkTheme),
            MenuItem::action("Theme: Light", UseLightTheme),
            MenuItem::separator(),
            MenuItem::action("Toggle Full Screen", ToggleFullScreen),
        ]),
        Menu::new("Go").items(go),
        Menu::new("Window").items([
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("Keyboard Shortcuts", ShowShortcuts)]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::OpenPalette;
    use crate::palette::registry::commands;
    use gpui_kit::Action as _;

    fn walk(items: &[MenuItem], out: &mut Vec<&'static str>) {
        for item in items {
            match item {
                MenuItem::Action { action, .. } => out.push(action.name()),
                MenuItem::Submenu(m) => walk(&m.items, out),
                _ => {}
            }
        }
    }

    #[test]
    fn menus_only_use_registered_commands() {
        let menus = app_menus();
        let names: Vec<_> = menus.iter().map(|m| m.name.to_string()).collect();
        assert_eq!(names, ["fdt", "File", "View", "Go", "Window", "Help"]);
        let registered: Vec<_> = commands().iter().map(|c| (c.action)().name()).collect();
        let mut used = Vec::new();
        for m in &menus {
            walk(&m.items, &mut used);
        }
        assert!(!used.is_empty());
        for name in used {
            if name == OpenPalette.name() {
                continue;
            }
            assert!(registered.contains(&name), "no command for {name}");
        }
    }
}
