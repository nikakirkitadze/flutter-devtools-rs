//! Cross-platform key bindings.

use crate::actions::{
    AppListConnect, AppListNext, AppListPrev, ClearSelection, CloseWindow, Collapse, Expand,
    FocusSearch, FollowLatest, GoBack, GoForward, GoToFirst, Hide, HideOthers, Minimize,
    NewSession, NewWindow, NextFrame, OpenDetails, OpenPalette, OpenSettings, PreviousFrame, Quit,
    SearchEverywhere, SelectNext, SelectPrevious, ShowShortcuts, ToggleExpand, ToggleFullScreen,
    ToggleRecording, ZoomIn, ZoomOut, ZoomToFit,
};
use crate::nav::Tool;
use gpui_kit::KeyBinding;
use gpui_kit::base::actions::{SelectDown, SelectUp};

/// Key context of the main shell.
pub const SHELL_CONTEXT: &str = "Shell";

/// The shell, but not while a text input has focus, so typing `?` or pressing Escape in a field
/// is left to the field.
pub const SHELL_NOT_INPUT: &str = "Shell && !Input";

/// Key context of the start page's app list. The URL field is outside it, so these keys never
/// fire while typing.
pub const APP_LIST_CONTEXT: &str = "AppList";

/// Key context of the Logs list. The toolbar's search field is outside it, so these keys never
/// fire while typing.
pub const LOGS_LIST_CONTEXT: &str = "LogsList";

/// Key context of the command palette.
pub const PALETTE_CONTEXT: &str = "Palette";

/// Key context of gpui-kit's data tables.
pub const TABLE_CONTEXT: &str = "DataTable";

/// Key context of time-axis views (the Timeline lanes, the raw trace and the flame chart), where
/// the zoom keys apply. A view sets it on its focusable element.
pub const LANES_CONTEXT: &str = "Lanes";

/// Key context of the frame chart, where the arrow keys step between frames.
pub const FRAME_CHART_CONTEXT: &str = "FrameChart";

/// `mac` on macOS, `other` everywhere else.
pub fn platform(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

/// Every key binding the app registers.
pub fn bindings() -> Vec<KeyBinding> {
    let shell = Some(SHELL_CONTEXT);
    let not_input = Some(SHELL_NOT_INPUT);
    let mut out = vec![KeyBinding::new(
        platform("cmd-k", "ctrl-k"),
        OpenPalette,
        None,
    )];
    const DIGITS: [(&str, &str); 7] = [
        ("cmd-1", "ctrl-1"),
        ("cmd-2", "ctrl-2"),
        ("cmd-3", "ctrl-3"),
        ("cmd-4", "ctrl-4"),
        ("cmd-5", "ctrl-5"),
        ("cmd-6", "ctrl-6"),
        ("cmd-7", "ctrl-7"),
    ];
    for (tool, (mac, other)) in Tool::ALL.into_iter().zip(DIGITS) {
        out.push(tool_binding(platform(mac, other), tool, shell));
    }
    out.extend([
        KeyBinding::new(platform("cmd-[", "ctrl-["), GoBack, not_input),
        KeyBinding::new(platform("cmd-]", "ctrl-]"), GoForward, not_input),
        KeyBinding::new("escape", ClearSelection, not_input),
        KeyBinding::new("?", ShowShortcuts, not_input),
        KeyBinding::new(platform("cmd-f", "ctrl-f"), FocusSearch, shell),
        KeyBinding::new(
            platform("cmd-shift-f", "ctrl-shift-f"),
            SearchEverywhere,
            shell,
        ),
        KeyBinding::new(platform("cmd-r", "ctrl-r"), ToggleRecording, shell),
        KeyBinding::new(platform("cmd-n", "ctrl-n"), NewSession, shell),
        KeyBinding::new(platform("cmd-shift-n", "ctrl-shift-n"), NewWindow, None),
        KeyBinding::new(platform("cmd-,", "ctrl-,"), OpenSettings, None),
        KeyBinding::new(platform("cmd-q", "ctrl-q"), Quit, None),
        KeyBinding::new(platform("cmd-w", "ctrl-w"), CloseWindow, None),
        KeyBinding::new(platform("ctrl-cmd-f", "f11"), ToggleFullScreen, None),
    ]);
    let list = Some(APP_LIST_CONTEXT);
    out.extend([
        KeyBinding::new("up", AppListPrev, list),
        KeyBinding::new("k", AppListPrev, list),
        KeyBinding::new("down", AppListNext, list),
        KeyBinding::new("j", AppListNext, list),
        KeyBinding::new("enter", AppListConnect, list),
    ]);
    let logs = Some(LOGS_LIST_CONTEXT);
    out.extend([
        KeyBinding::new("j", SelectNext, logs),
        KeyBinding::new("down", SelectNext, logs),
        KeyBinding::new("k", SelectPrevious, logs),
        KeyBinding::new("up", SelectPrevious, logs),
        KeyBinding::new("space", ToggleExpand, logs),
        KeyBinding::new("right", Expand, logs),
        KeyBinding::new("left", Collapse, logs),
        KeyBinding::new("enter", OpenDetails, logs),
        KeyBinding::new("end", FollowLatest, logs),
        KeyBinding::new("home", GoToFirst, logs),
    ]);
    // J and K move through every table's rows, like the arrow keys the table binds itself.
    let table = Some(TABLE_CONTEXT);
    out.extend([
        KeyBinding::new("j", SelectDown, table),
        KeyBinding::new("k", SelectUp, table),
        // Enter opens the selected request's details, as it does a log line's.
        KeyBinding::new("enter", OpenDetails, table),
    ]);
    let lanes = Some(LANES_CONTEXT);
    out.extend([
        KeyBinding::new(platform("cmd-=", "ctrl-="), ZoomIn, lanes),
        KeyBinding::new(platform("cmd--", "ctrl--"), ZoomOut, lanes),
        KeyBinding::new(platform("cmd-0", "ctrl-0"), ZoomToFit, lanes),
    ]);
    let chart = Some(FRAME_CHART_CONTEXT);
    out.extend([
        KeyBinding::new("left", PreviousFrame, chart),
        KeyBinding::new("right", NextFrame, chart),
    ]);
    // The palette's own movement; Enter and Escape reach it through the input's events.
    let palette = Some(PALETTE_CONTEXT);
    out.extend([
        KeyBinding::new("up", SelectUp, palette),
        KeyBinding::new("down", SelectDown, palette),
    ]);
    if cfg!(target_os = "macos") {
        out.extend([
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
        ]);
    }
    out
}

fn tool_binding(keys: &'static str, tool: Tool, ctx: Option<&str>) -> KeyBinding {
    use crate::actions::*;
    match tool {
        Tool::Timeline => KeyBinding::new(keys, ShowTimeline, ctx),
        Tool::Performance => KeyBinding::new(keys, ShowPerformance, ctx),
        Tool::Memory => KeyBinding::new(keys, ShowMemory, ctx),
        Tool::Network => KeyBinding::new(keys, ShowNetwork, ctx),
        Tool::Logs => KeyBinding::new(keys, ShowLogs, ctx),
        Tool::Widgets => KeyBinding::new(keys, ShowWidgets, ctx),
        Tool::State => KeyBinding::new(keys, ShowState, ctx),
    }
}

/// The shortcut hint for `action`: its binding in the shell context, else any global binding.
pub fn shortcut_hint(
    action: &dyn gpui_kit::Action,
    shell_focus: &gpui_kit::FocusHandle,
    window: &gpui_kit::Window,
) -> Option<gpui_kit::component::kbd::Kbd> {
    use gpui_kit::component::kbd::Kbd;
    Kbd::binding_for_action_in(action, shell_focus, window)
        .or_else(|| Kbd::binding_for_action(action, None, window))
        // Keys that only a focused view has (zoom, the frame arrows) are listed all the same.
        .or_else(|| Kbd::binding_for_action(action, Some(LANES_CONTEXT), window))
        .or_else(|| Kbd::binding_for_action(action, Some(FRAME_CHART_CONTEXT), window))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os() -> gpui_kit::OperatingSystem {
        if cfg!(target_os = "macos") {
            gpui_kit::OperatingSystem::Mac
        } else {
            gpui_kit::OperatingSystem::Linux
        }
    }

    fn keys_of(action: &str) -> Vec<(String, Option<String>)> {
        bindings()
            .iter()
            .filter(|b| b.action().name() == action)
            .map(|b| {
                let keys = b
                    .keystrokes()
                    .iter()
                    .map(|k| k.inner().unparse(os()))
                    .collect::<Vec<_>>()
                    .join(" ");
                (keys, b.predicate().map(|p| p.to_string()))
            })
            .collect()
    }

    #[test]
    fn zoom_and_frame_keys_live_in_their_own_contexts() {
        use crate::actions::*;
        use gpui_kit::Action as _;
        let want = |action: &str, keys: &str, ctx: &str| {
            let found = keys_of(action);
            assert_eq!(
                found,
                [(keys.to_string(), Some(ctx.to_string()))],
                "{action}"
            );
        };
        want(ZoomIn.name(), platform("cmd-=", "ctrl-="), LANES_CONTEXT);
        want(ZoomOut.name(), platform("cmd--", "ctrl--"), LANES_CONTEXT);
        want(ZoomToFit.name(), platform("cmd-0", "ctrl-0"), LANES_CONTEXT);
        want(PreviousFrame.name(), "left", FRAME_CHART_CONTEXT);
        want(NextFrame.name(), "right", FRAME_CHART_CONTEXT);
        assert_eq!(
            (LANES_CONTEXT, FRAME_CHART_CONTEXT),
            ("Lanes", "FrameChart")
        );
    }

    #[test]
    fn the_new_keys_clash_with_no_other_binding() {
        use crate::actions::*;
        use gpui_kit::Action as _;
        let mine = [
            ZoomIn.name(),
            ZoomOut.name(),
            ZoomToFit.name(),
            PreviousFrame.name(),
            NextFrame.name(),
        ];
        for b in bindings() {
            if mine.contains(&b.action().name()) {
                continue;
            }
            let keys = b
                .keystrokes()
                .iter()
                .map(|k| k.inner().unparse(os()))
                .collect::<Vec<_>>()
                .join(" ");
            for action in mine {
                let theirs = keys_of(action);
                // A shared key is fine in another context, never in the same one.
                assert!(
                    !theirs.iter().any(|(k, c)| *k == keys
                        && (c.is_none()
                            || b.predicate().is_none()
                            || *c == b.predicate().map(|p| p.to_string()))),
                    "{keys} for {} clashes with {action}",
                    b.action().name()
                );
            }
        }
    }

    #[test]
    fn platform_picks_the_os_variant() {
        let expected = if cfg!(target_os = "macos") { "a" } else { "b" };
        assert_eq!(platform("a", "b"), expected);
    }
}
