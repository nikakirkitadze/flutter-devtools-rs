//! App startup: global setup, window options and the run loop.

use crate::actions::{Hide, HideOthers, OpenPalette, Quit, ShowAll};
use crate::keymap;
use crate::settings::{SettingsStore, init_settings, settings};
use crate::shell::MainWindow;
use crate::startup::StartupReport;
use fdt_ui::assets::FdtAssets;
use fdt_ui::theme;
use fdt_ui::tokens::layout;
use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext as _, Bounds, Entity, Window, WindowBounds, WindowOptions};
use std::time::Instant;

/// Setup shared by the real app and the tests.
pub fn init(cx: &mut App) {
    gpui_kit::init(cx);
    theme::install(cx);
    cx.bind_keys(keymap::bindings());
    cx.on_action(|_: &Quit, cx| cx.quit());
    #[cfg(target_os = "macos")]
    {
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (Hide, HideOthers, ShowAll);
    }
}

pub fn main_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            layout::WINDOW_DEFAULT,
            cx,
        ))),
        window_min_size: Some(layout::WINDOW_MIN),
        app_id: Some("dev.fdt.fdt".into()),
        // Linux has no server-side frame here: draw it on the client (unverified, built on macOS).
        #[cfg(target_os = "linux")]
        window_decorations: Some(gpui_kit::WindowDecorations::Client),
        ..TitleBar::window_options()
    }
}

pub fn open_main_window(cx: &mut App) -> anyhow::Result<Entity<MainWindow>> {
    let options = main_window_options(cx);
    let (_, shell) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| MainWindow::new(window, cx))
    })?;
    Ok(shell)
}

/// Common startup: global setup, settings, theme, fonts, menus and the main window.
fn start(cx: &mut App, store: SettingsStore) -> anyhow::Result<Entity<MainWindow>> {
    init(cx);
    if let Err(err) = fdt_ui::fonts::load(cx) {
        log::warn!("loading bundled fonts failed, using system fonts: {err:#}");
    }
    cx.set_menus(crate::menus::app_menus());
    init_settings(cx, store);
    let pref = settings(cx).theme;
    theme::apply(pref, None, cx);
    fdt_ui::os::sync_reduced_motion(cx);
    let shell = open_main_window(cx)?;
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    cx.activate(true);
    Ok(shell)
}

fn load_store() -> SettingsStore {
    // The one main-thread file read: settings, before the event loop starts.
    match SettingsStore::default_path() {
        Some(path) => SettingsStore::load(path),
        None => SettingsStore::in_memory(),
    }
}

/// Runs the app until the last window closes.
pub fn run() {
    let store = load_store();
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(|cx: &mut App| {
            if let Err(err) = start(cx, store) {
                log::error!("opening the main window failed: {err:#}");
                cx.quit();
            }
        });
}

/// Like [`run`], but measures cold start, prints one `StartupReport` JSON line and quits.
///
/// Times are measured from `t0` (the first statement of `main()`), not from process exec, and
/// presentation is approximated by the next-frame callback: a nested `on_next_frame` runs after
/// the first frame was drawn. "Interactive" is when the command palette has been opened and drawn.
pub fn run_with_startup_trace(t0: Instant) {
    let store = load_store();
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(move |cx: &mut App| {
            let shell = match start(cx, store) {
                Ok(shell) => shell,
                Err(err) => {
                    log::error!("opening the main window failed: {err:#}");
                    cx.quit();
                    return;
                }
            };
            let Some(handle) = cx.windows().first().copied() else {
                cx.quit();
                return;
            };
            let result = handle.update(cx, move |_, window, cx| {
                after_next_paint(window, cx, move |window, cx| {
                    let first_paint = t0.elapsed();
                    window.focus(&shell.read(cx).focus_handle(), cx);
                    window.dispatch_action(Box::new(OpenPalette), cx);
                    after_next_paint(window, cx, move |_, cx| {
                        let report = StartupReport {
                            first_paint,
                            interactive: t0.elapsed(),
                        };
                        println!("{}", report.to_json());
                        cx.quit();
                    });
                });
            });
            if let Err(err) = result {
                log::error!("startup trace could not reach the window: {err:#}");
                cx.quit();
            }
        });
}

/// Runs `callback` after the next frame has been drawn (a nested `on_next_frame`).
fn after_next_paint(
    window: &mut Window,
    _cx: &mut App,
    callback: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(callback);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    #[gpui_kit::test]
    fn window_options_use_layout_tokens(cx: &mut TestAppContext) {
        let opts = cx.update(|cx| main_window_options(cx));
        assert_eq!(opts.window_min_size, Some(layout::WINDOW_MIN));
        assert_eq!(opts.app_id.as_deref(), Some("dev.fdt.fdt"));
        #[cfg(target_os = "linux")]
        assert_eq!(
            opts.window_decorations,
            Some(gpui_kit::WindowDecorations::Client)
        );
    }
}
