//! Cross-platform key bindings.

use crate::actions::{
    ClearSelection, GoBack, GoForward, Hide, HideOthers, Minimize, OpenPalette, OpenSettings, Quit,
    ShowShortcuts, ToggleFullScreen,
};
use crate::nav::Tool;
use gpui_kit::KeyBinding;

/// Key context of the main shell.
pub const SHELL_CONTEXT: &str = "Shell";

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
        KeyBinding::new(platform("cmd-[", "ctrl-["), GoBack, shell),
        KeyBinding::new(platform("cmd-]", "ctrl-]"), GoForward, shell),
        KeyBinding::new("escape", ClearSelection, shell),
        KeyBinding::new("?", ShowShortcuts, shell),
        KeyBinding::new(platform("cmd-,", "ctrl-,"), OpenSettings, None),
        KeyBinding::new(platform("cmd-q", "ctrl-q"), Quit, None),
        KeyBinding::new(platform("ctrl-cmd-f", "f11"), ToggleFullScreen, None),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_picks_the_os_variant() {
        let expected = if cfg!(target_os = "macos") { "a" } else { "b" };
        assert_eq!(platform("a", "b"), expected);
    }
}
