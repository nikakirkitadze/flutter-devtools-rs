//! App startup: global setup, window options and the run loop.

use crate::actions::{Hide, HideOthers, NewWindow, OpenPalette, Quit, ShowAll};
use crate::capture_host::CaptureHost;
use crate::keymap;
use crate::session_model::SessionModel;
use crate::settings::{SettingsStore, flush_settings, init_settings, settings};
use crate::shell::MainWindow;
use crate::startup::{StartupPhases, StartupReport, WATCHDOG, emit, mark};
use fdt_capture::ConnStatus;
use fdt_store::Snapshot;
use fdt_ui::assets::FdtAssets;
use fdt_ui::theme;
use fdt_ui::tokens::layout;
use fdt_vm::discovery::DiscoveryConfig;
use gpui_kit::component::{TitleBar, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Bounds, Entity, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::cell::Cell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

/// Setup shared by the real app and the tests.
pub fn init(cx: &mut App) {
    init_with(cx, &mut None);
}

fn init_with(cx: &mut App, phases: &mut Option<&mut StartupPhases>) {
    theme::seed_fonts(cx);
    gpui_kit::init(cx);
    mark(phases, "gpui_kit_init");
    theme::install(cx);
    mark(phases, "theme_installed");
    cx.bind_keys(keymap::bindings());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &NewWindow, cx| {
        if let Err(err) = open_main_window(cx) {
            log::error!("could not open a new window: {err:#}");
        }
    });
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
        // The title bar is custom drawn, but Alt-Tab, taskbars and screen readers read this.
        titlebar: Some(TitlebarOptions {
            title: Some("fdt".into()),
            ..TitleBar::title_bar_options()
        }),
        // Linux has no server-side frame here: draw it on the client (unverified, built on macOS).
        #[cfg(target_os = "linux")]
        window_decorations: Some(gpui_kit::WindowDecorations::Client),
        ..TitleBar::window_options()
    }
}

pub fn open_main_window(cx: &mut App) -> anyhow::Result<Entity<MainWindow>> {
    let options = main_window_options(cx);
    let (_, shell) = gpui_kit::open_window(options, cx, |window, cx| {
        // Detached until the capture host exists; see `SessionModel::new`.
        let session = cx.new(SessionModel::new);
        cx.new(|cx| MainWindow::new(window, cx, session))
    })?;
    Ok(shell)
}

/// Global setup shared by every launch mode: gpui-kit, fonts, settings, theme and motion.
fn setup(cx: &mut App, store: SettingsStore, phases: &mut Option<&mut StartupPhases>) {
    init_with(cx, phases);
    if let Err(err) = fdt_ui::fonts::load(cx) {
        log::warn!("loading bundled fonts failed, using system fonts: {err:#}");
    }
    mark(phases, "fonts_loaded");
    init_settings(cx, store);
    cx.on_app_quit(|cx| {
        flush_settings(cx);
        std::future::ready(())
    })
    .detach();
    let pref = settings(cx).theme;
    theme::apply(pref, None, cx);
    mark(phases, "theme_applied");
    fdt_ui::os::sync_reduced_motion(cx);
    mark(phases, "motion_synced");
}

/// Common startup: global setup, settings, theme, fonts, menus and the main window.
fn start(
    cx: &mut App,
    store: SettingsStore,
    mut phases: Option<&mut StartupPhases>,
    discovery: DiscoveryChoice,
) -> anyhow::Result<Entity<MainWindow>> {
    let phases = &mut phases;
    setup(cx, store, phases);
    let shell = open_main_window(cx)?;
    mark(phases, "window_opened");
    defer_menus_and_host(cx, discovery);
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    cx.activate(true);
    Ok(shell)
}

/// Startup for `fdt bench-ui`: global setup with in-memory settings, then the main window over a
/// fixed session. No menus, no capture host and no discovery, so nothing else runs while a
/// scenario is measured.
pub(crate) fn start_fixed(
    cx: &mut App,
    snapshot: Arc<Snapshot>,
    status: ConnStatus,
) -> anyhow::Result<Entity<MainWindow>> {
    setup(cx, SettingsStore::in_memory(), &mut None);
    let options = main_window_options(cx);
    let (_, shell) = gpui_kit::open_window(options, cx, |window, cx| {
        let session = cx.new(|_| SessionModel::fixed(snapshot, status));
        cx.new(|cx| MainWindow::new(window, cx, session))
    })?;
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    cx.activate(true);
    Ok(shell)
}

/// Where discovery looks for running apps.
pub enum DiscoveryChoice {
    /// The real DTD registry, with `dart` from `PATH` as the fallback.
    System,
    /// A fixed config. Tests pass an empty temporary directory and no `dart`.
    Fixed(DiscoveryConfig),
}

/// The deferred work: the menu bar and the capture host.
fn install(cx: &mut App, discovery: DiscoveryChoice) {
    cx.set_menus(crate::menus::app_menus());
    match discovery {
        DiscoveryChoice::System => CaptureHost::install(cx),
        DiscoveryChoice::Fixed(config) => CaptureHost::install_with(cx, config),
    }
}

/// Installs the app menus and the capture host a few frames after the window opens instead of
/// before it.
///
/// Building the native menu bar costs about 12 ms on macOS, and starting the capture runtime and
/// discovery thread adds more; the first frame needs neither. The shortcuts are key bindings, so
/// they work in the meantime. The extra frames keep this after the callback that
/// `run_with_startup_trace` uses to time first paint.
fn defer_menus_and_host(cx: &mut App, discovery: DiscoveryChoice) {
    let Some(handle) = cx.windows().first().copied() else {
        install(cx, discovery);
        return;
    };
    let pending = Rc::new(Cell::new(Some(discovery)));
    let later = pending.clone();
    let result = handle.update(cx, |_, window, _| {
        window.on_next_frame(|window, _| {
            window.on_next_frame(|window, _| {
                window.on_next_frame(move |_, cx| {
                    if let Some(discovery) = later.take() {
                        install(cx, discovery);
                    }
                });
            });
        });
    });
    if result.is_err()
        && let Some(discovery) = pending.take()
    {
        install(cx, discovery);
    }
}

fn load_store() -> SettingsStore {
    // The one main-thread file read: settings, before the event loop starts.
    match SettingsStore::default_path() {
        Some(path) => SettingsStore::load(path),
        None => SettingsStore::in_memory(),
    }
}

/// Reports a startup failure that leaves the user without a window, then exits non-zero.
///
/// `cx.quit()` ends in AppKit's `exit(0)` on macOS, so a failure has to exit directly.
fn fatal(message: &str) -> ! {
    eprintln!("fdt: {message}");
    std::process::exit(1);
}

/// Runs the app until the last window closes.
pub fn run() -> ExitCode {
    let store = load_store();
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(|cx: &mut App| {
            if let Err(err) = start(cx, store, None, DiscoveryChoice::System) {
                fatal(&format!("could not open the main window: {err:#}"));
            }
        });
    ExitCode::SUCCESS
}

/// Like [`run`], but measures cold start, prints one `StartupReport` JSON line and quits.
///
/// Times are measured from `t0` (the start of `main()`); the report also carries the wall-clock
/// time of `main()` entry so a script can add the time before it. Presentation is approximated by the next-frame callback: a nested `on_next_frame` runs after
/// the first frame was drawn. "Interactive" is when the command palette has been opened and drawn.
///
/// Exits 1 directly when the palette did not open; a watchdog ends the process with
/// exit code 1 if no report was printed within [`WATCHDOG`].
pub fn run_with_startup_trace(t0: Instant, main_entry_unix_ms: f64) -> ExitCode {
    std::thread::spawn(|| {
        std::thread::sleep(WATCHDOG);
        eprintln!("startup-trace: timed out");
        std::process::exit(1);
    });
    let ok = Rc::new(Cell::new(false));
    let ok_in_run = ok.clone();
    let store = load_store();
    let mut phases = StartupPhases::new(t0);
    phases.mark("settings_loaded");
    gpui_kit::application()
        .with_assets(FdtAssets)
        .run(move |cx: &mut App| {
            let ok = ok_in_run;
            phases.mark("app_started");
            let shell = match start(cx, store, Some(&mut phases), DiscoveryChoice::System) {
                Ok(shell) => shell,
                Err(err) => fatal(&format!("could not open the main window: {err:#}")),
            };
            let Some(handle) = cx.windows().first().copied() else {
                fatal("the main window is missing");
            };
            let result = handle.update(cx, move |_, window, cx| {
                after_next_paint(window, cx, move |window, cx| {
                    let first_paint = t0.elapsed();
                    phases.mark("first_paint");
                    window.focus(&shell.read(cx).focus_handle(), cx);
                    window.dispatch_action(Box::new(OpenPalette), cx);
                    after_next_paint(window, cx, move |window, cx| {
                        let report = StartupReport {
                            main_entry_unix_ms,
                            first_paint,
                            interactive: t0.elapsed(),
                            phases: phases.into_marks(),
                        };
                        let palette_open = window.has_active_dialog(cx);
                        let printed = emit(
                            &report,
                            palette_open,
                            &mut std::io::stdout(),
                            &mut std::io::stderr(),
                        );
                        ok.set(printed);
                        if !printed {
                            // `cx.quit()` would exit 0 on macOS.
                            std::process::exit(1);
                        }
                        cx.quit();
                    });
                });
            });
            if let Err(err) = result {
                fatal(&format!(
                    "startup trace could not reach the window: {err:#}"
                ));
            }
        });
    if ok.get() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
pub(crate) mod tests {
    use super::*;
    use crate::settings::SettingsStore;
    use gpui_kit::TestAppContext;

    /// A discovery config that reads an empty temporary registry and never runs `dart`, so no
    /// test touches the real tooling daemon.
    pub(crate) fn empty_discovery(dir: &std::path::Path) -> DiscoveryConfig {
        DiscoveryConfig {
            registry_dir: Some(dir.to_path_buf()),
            dart_cli: None,
            ..DiscoveryConfig::default()
        }
    }

    /// What this proves: opening the window does not build the host (absent after `start` and
    /// after the executor settles), and the deferred install step, once it runs, builds it and
    /// attaches the first window. That the step runs only after the first frame rests on
    /// `on_next_frame`, which gpui's public test API cannot drive; it was checked by hand.
    #[gpui_kit::test]
    fn host_is_absent_at_window_open_and_attached_by_the_deferred_install(cx: &mut TestAppContext) {
        // The start page awaits the host's discovery, which publishes from the capture thread.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = empty_discovery(dir.path());
        let shell = cx.update(|cx| {
            let shell = start(
                cx,
                SettingsStore::in_memory(),
                None,
                DiscoveryChoice::Fixed(config),
            )
            .unwrap();
            assert!(
                !cx.has_global::<CaptureHost>(),
                "the host must not be built while the window opens"
            );
            shell
        });
        let session = shell.read_with(cx, |s, _| s.session().clone());
        assert!(session.read_with(cx, |s, _| s.control().is_none()));
        // gpui's public test API cannot deliver the platform frame request that runs
        // `on_next_frame` callbacks, so the frame-driven timing is checked in the real app (see
        // the task report). Here: nothing the executor runs installs the host early...
        cx.run_until_parked();
        assert!(!cx.update(|cx| cx.has_global::<CaptureHost>()));
        // ...and the deferred step, once it runs, builds the host and attaches the first window.
        cx.update(|cx| install(cx, DiscoveryChoice::Fixed(empty_discovery(dir.path()))));
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.has_global::<CaptureHost>()));
        assert!(session.read_with(cx, |s, _| s.control().is_some()));
    }

    #[gpui_kit::test]
    fn window_options_use_layout_tokens(cx: &mut TestAppContext) {
        let opts = cx.update(|cx| main_window_options(cx));
        assert_eq!(opts.window_min_size, Some(layout::WINDOW_MIN));
        assert_eq!(opts.app_id.as_deref(), Some("dev.fdt.fdt"));
        let title = opts.titlebar.and_then(|t| t.title);
        assert_eq!(title.as_deref(), Some("fdt"));
        #[cfg(target_os = "linux")]
        assert_eq!(
            opts.window_decorations,
            Some(gpui_kit::WindowDecorations::Client)
        );
    }
}
