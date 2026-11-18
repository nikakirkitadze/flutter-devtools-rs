//! OS accessibility preferences that fdt follows.

use gpui_kit::App;

/// Parses the output of `gsettings get org.gnome.desktop.interface enable-animations`.
/// Returns the animations-enabled flag, or `None` when the output is not a boolean.
pub fn parse_gnome_enable_animations(stdout: &str) -> Option<bool> {
    match stdout.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Whether the OS asks for reduced motion. On Linux this runs a subprocess and may
/// block briefly, so call it off the UI thread.
pub fn prefers_reduced_motion() -> bool {
    platform_prefers_reduced_motion()
}

#[cfg(target_os = "macos")]
fn platform_prefers_reduced_motion() -> bool {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

#[cfg(target_os = "windows")]
fn platform_prefers_reduced_motion() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut animations_on: i32 = 1;
    // SAFETY: the pointer refers to a live i32, which is what SPI_GETCLIENTAREAANIMATION writes.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut animations_on).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    // Reduced motion means client-area animations are off. A failed query means "not reduced".
    ok.is_ok() && animations_on == 0
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_prefers_reduced_motion() -> bool {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_gnome_enable_animations(&String::from_utf8_lossy(&out.stdout)) == Some(false)
        }
        _ => false,
    }
}

/// Reads the OS preference off the UI thread and applies it to GPUI.
pub fn sync_reduced_motion(cx: &mut App) {
    sync_reduced_motion_with(cx, prefers_reduced_motion);
}

/// Like [`sync_reduced_motion`] with an injectable reader: `read` runs on the background
/// executor, and the result is applied with `cx.set_reduce_motion` on the main thread.
pub fn sync_reduced_motion_with(cx: &mut App, read: fn() -> bool) {
    let task = cx.background_executor().spawn(async move { read() });
    cx.spawn(async move |cx| {
        let reduced = task.await;
        cx.update(|cx| cx.set_reduce_motion(reduced));
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    #[test]
    fn parses_gsettings_output() {
        assert_eq!(parse_gnome_enable_animations("true\n"), Some(true));
        assert_eq!(parse_gnome_enable_animations("false"), Some(false));
        assert_eq!(parse_gnome_enable_animations("'yes'"), None);
        assert_eq!(parse_gnome_enable_animations(""), None);
    }

    #[gpui_kit::test]
    fn sync_applies_reader_result(cx: &mut TestAppContext) {
        cx.update(|cx| sync_reduced_motion_with(cx, || true));
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.reduce_motion()));
        cx.update(|cx| sync_reduced_motion_with(cx, || false));
        cx.run_until_parked();
        assert!(!cx.update(|cx| cx.reduce_motion()));
    }
}
