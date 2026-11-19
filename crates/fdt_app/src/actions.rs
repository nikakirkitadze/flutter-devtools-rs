//! Every GPUI action the app defines.

use crate::nav::Tool;

gpui_kit::actions!(
    fdt,
    [
        OpenPalette,
        ShowShortcuts,
        GoBack,
        GoForward,
        ClearSelection,
        ShowTimeline,
        ShowPerformance,
        ShowMemory,
        ShowNetwork,
        ShowLogs,
        ShowWidgets,
        ShowState,
        OpenSettings,
        ToggleFrameHud,
        UseSystemTheme,
        UseDarkTheme,
        UseLightTheme,
        About,
        Quit,
        Minimize,
        Zoom,
        ToggleFullScreen,
        CloseWindow,
        Hide,
        HideOthers,
        ShowAll,
        ToggleRecording,
        NewSession,
        NewWindow,
        ConnectToApp,
        Disconnect,
        StopWaiting
    ]
);

/// The action that switches to `tool`.
pub fn show_tool(tool: Tool) -> Box<dyn gpui_kit::Action> {
    match tool {
        Tool::Timeline => Box::new(ShowTimeline),
        Tool::Performance => Box::new(ShowPerformance),
        Tool::Memory => Box::new(ShowMemory),
        Tool::Network => Box::new(ShowNetwork),
        Tool::Logs => Box::new(ShowLogs),
        Tool::Widgets => Box::new(ShowWidgets),
        Tool::State => Box::new(ShowState),
    }
}
