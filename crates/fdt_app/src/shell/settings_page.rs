//! The settings page: theme preference, network capture and a pointer to the shortcut list.

use crate::actions::{ShowShortcuts, UseDarkTheme, UseLightTheme, UseSystemTheme};
use crate::capture_host::CaptureHost;
use crate::session_model::for_each_control;
use crate::settings::update_settings;
use crate::shell::MainWindow;
use crate::tools::performance::rebuilds_tab::OVERHEAD_NOTE;
use fdt_ui::theme::{ThemePreference, TokensExt as _};
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use fdt_vm::devices::{PlatformState, WatchStatus};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::switch::Switch;
use gpui_kit::{
    Action, AnyElement, App, Hsla, InteractiveElement as _, IntoElement, Keystroke,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, WeakEntity, Window, div,
};

fn dispatcher(
    action: impl Action + Clone,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) {
    move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
}

fn control_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    action: impl Action + Clone,
    cx: &App,
) -> AnyElement {
    let t = cx.tokens();
    let el = div()
        .id(id)
        .test_support()
        .px(space::S12)
        .h(row::TABLE)
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(font::BODY)
        .font_weight(if selected {
            font::MEDIUM
        } else {
            font::REGULAR
        })
        .text_color(Hsla::from(if selected { t.accent } else { t.text }))
        .hover(|s| s.bg(t.border.with_alpha(alpha::ACTIVE_ROW)))
        .on_click(dispatcher(action))
        .child(label);
    if selected {
        el.bg(t.accent.with_alpha(alpha::ACTIVE_ROW))
            .into_any_element()
    } else {
        el.into_any_element()
    }
}

fn segment_row(buttons: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .rounded(radius::CONTROL)
        .border(layout::BORDER)
        .border_color(Hsla::from(t.border))
        .overflow_hidden()
        .children(buttons)
}

fn row(label: &'static str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S16)
        .py(space::S8)
        .child(
            div()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text))
                .child(label),
        )
        .child(control)
}

fn section(title: &'static str, rows: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    let t = cx.tokens();
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .pb(space::S16)
        .child(
            div()
                .text_size(font::SECTION)
                .font_weight(font::MEDIUM)
                .text_color(Hsla::from(t.text))
                .pb(space::S4)
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .child(title),
        )
        .children(rows)
}

/// Switches "free app memory" for the setting and for every open window's capture.
fn set_free_http_memory(on: bool, shell: &WeakEntity<MainWindow>, cx: &mut App) {
    update_settings(cx, |s| s.free_app_http_memory = on);
    for_each_control(cx, |control| control.set_free_app_http_memory(on));
    let _ = shell.update(cx, |_, cx| cx.notify());
}

fn free_http_memory_row(on: bool, shell: WeakEntity<MainWindow>, cx: &App) -> AnyElement {
    let t = cx.tokens();
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .py(space::S8)
        .child(
            Switch::new("settings-free-http-memory")
                .checked(on)
                .label("Free app memory after copying requests")
                .on_change(move |checked, _, cx| set_free_http_memory(*checked, &shell, cx)),
        )
        .child(
            div()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child("Clears the app's own HTTP profile after fdt copies each request. Other tools attached to the app lose those requests."),
        )
        .into_any_element()
}

/// Switches the tracing default for the setting and for every open window's capture: each
/// one's next connection starts with it.
fn set_trace_by_default(on: bool, shell: &WeakEntity<MainWindow>, cx: &mut App) {
    update_settings(cx, |s| s.trace_widget_builds = on);
    for_each_control(cx, |control| control.set_trace_default(on));
    let _ = shell.update(cx, |_, cx| cx.notify());
}

fn trace_by_default_row(on: bool, shell: WeakEntity<MainWindow>, cx: &App) -> AnyElement {
    let t = cx.tokens();
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .py(space::S8)
        .child(
            Switch::new("settings-trace-widget-builds")
                .checked(on)
                .label("Trace widget builds by default")
                .on_change(move |checked, _, cx| set_trace_by_default(*checked, &shell, cx)),
        )
        .child(
            div()
                .text_size(font::BODY)
                .text_color(Hsla::from(t.text_muted))
                .child(OVERHEAD_NOTE),
        )
        .into_any_element()
}

/// Switches finding apps opened on simulators and devices, for the setting and the host's
/// watch (if the host exists yet).
fn set_find_device_apps(on: bool, shell: &WeakEntity<MainWindow>, cx: &mut App) {
    update_settings(cx, |s| s.find_device_apps = on);
    if cx.has_global::<CaptureHost>() {
        cx.global_mut::<CaptureHost>().set_find_device_apps(on);
    }
    let _ = shell.update(cx, |_, cx| cx.notify());
}

fn state_line(name: &str, state: &PlatformState, unit: &str) -> String {
    match state {
        PlatformState::Off => format!("{name}: off"),
        PlatformState::Watching { devices } => {
            let unit = if unit == "devices" && *devices == 1 {
                "device"
            } else {
                unit
            };
            format!("{name}: watching {devices} {unit}")
        }
        PlatformState::Unavailable(reason) => format!("{name}: {reason}"),
    }
}

/// The status lines under the switch: the iOS simulator line on macOS only, Android everywhere.
fn status_lines(status: &WatchStatus) -> Vec<(&'static str, String)> {
    let mut lines = Vec::new();
    if cfg!(target_os = "macos") {
        lines.push((
            "settings-status-simulator",
            state_line("iOS simulators", &status.simulator, "booted"),
        ));
    }
    lines.push((
        "settings-status-android",
        state_line("Android", &status.android, "devices"),
    ));
    lines
}

fn find_device_apps_row(
    on: bool,
    status: &WatchStatus,
    shell: WeakEntity<MainWindow>,
    cx: &App,
) -> AnyElement {
    let t = cx.tokens();
    let muted = Hsla::from(t.text_muted);
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .py(space::S8)
        .child(
            Switch::new("settings-find-device-apps")
                .checked(on)
                .label("Find apps opened on simulators and devices")
                .on_change(move |checked, _, cx| set_find_device_apps(*checked, &shell, cx)),
        )
        .child(
            div()
                .text_size(font::BODY)
                .text_color(muted)
                .child("Watches booted iOS simulators and Android devices for Flutter apps started without `flutter run`."),
        )
        .children(status_lines(status).into_iter().map(|(id, text)| {
            div()
                .id(id)
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .text_size(font::BODY)
                .text_color(muted)
                .child(text)
        }))
        .into_any_element()
}

pub fn render(
    pref: ThemePreference,
    find_device_apps: bool,
    device_status: &WatchStatus,
    free_http_memory: bool,
    trace_builds: bool,
    shell: WeakEntity<MainWindow>,
    cx: &App,
) -> impl IntoElement {
    let t = cx.tokens();
    let theme = segment_row(
        vec![
            control_button(
                "theme-system",
                "Match system",
                pref == ThemePreference::System,
                UseSystemTheme,
                cx,
            ),
            control_button(
                "theme-dark",
                "Dark",
                pref == ThemePreference::Dark,
                UseDarkTheme,
                cx,
            ),
            control_button(
                "theme-light",
                "Light",
                pref == ThemePreference::Light,
                UseLightTheme,
                cx,
            ),
        ],
        cx,
    );
    let shortcuts = div()
        .flex()
        .items_center()
        .gap(space::S8)
        .child(segment_row(
            vec![control_button(
                "settings-shortcuts",
                "Show",
                false,
                ShowShortcuts,
                cx,
            )],
            cx,
        ))
        .child(match Keystroke::parse("?") {
            Ok(key) => Kbd::new(key).into_any_element(),
            Err(_) => div().into_any_element(),
        });
    div()
        .id("settings-page")
        .test_support()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px(space::S24)
                .py(space::S16)
                .text_size(font::PAGE)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text))
                .child(SharedString::from("Settings")),
        )
        .child(
            div()
                .id("settings-scroll")
                .test_support()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(space::S24)
                .flex()
                .flex_col()
                .max_w(layout::EMPTY_STATE_MAX_WIDTH)
                .child(section(
                    "Connect",
                    vec![find_device_apps_row(
                        find_device_apps,
                        device_status,
                        shell.clone(),
                        cx,
                    )],
                    cx,
                ))
                .child(section(
                    "Appearance",
                    vec![row("Theme", theme, cx).into_any_element()],
                    cx,
                ))
                .child(section(
                    "Network",
                    vec![free_http_memory_row(free_http_memory, shell.clone(), cx)],
                    cx,
                ))
                .child(section(
                    "Performance",
                    vec![trace_by_default_row(trace_builds, shell.clone(), cx)],
                    cx,
                ))
                .child(section(
                    "Keyboard",
                    vec![row("Keyboard shortcuts", shortcuts, cx).into_any_element()],
                    cx,
                )),
        )
}

#[cfg(test)]
mod tests {
    use crate::capture_host::CaptureHost;
    use crate::test_support::{exists, open_shell_fake, press};
    use fdt_vm::devices::{Candidate, Platform};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use std::sync::Arc;

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    #[test]
    fn status_lines_follow_the_watch_state() {
        use super::{PlatformState, WatchStatus, status_lines};
        let lines = status_lines(&WatchStatus {
            simulator: PlatformState::Watching { devices: 2 },
            android: PlatformState::Unavailable("adb not found".into()),
        });
        let texts: Vec<&str> = lines.iter().map(|(_, t)| t.as_str()).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(
                texts,
                [
                    "iOS simulators: watching 2 booted",
                    "Android: adb not found"
                ]
            );
        } else {
            assert_eq!(texts, ["Android: adb not found"]);
        }
        let off = status_lines(&WatchStatus::default());
        assert_eq!(off.last().unwrap().1, "Android: off");
        let android = |n| {
            status_lines(&WatchStatus {
                simulator: PlatformState::Off,
                android: PlatformState::Watching { devices: n },
            })
            .last()
            .unwrap()
            .1
            .clone()
        };
        assert_eq!(android(1), "Android: watching 1 device");
        assert_eq!(android(0), "Android: watching 0 devices");
        assert_eq!(android(3), "Android: watching 3 devices");
    }

    #[gpui_kit::test]
    fn switching_off_removes_device_rows(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::watch::channel::<Arc<[Candidate]>>(Arc::from(Vec::new()));
        let tx = Arc::new(tx);
        let (_shell, cx, _fake) = open_shell_fake(cx);
        cx.update(|_, cx| {
            CaptureHost::install_with_candidates(
                cx,
                crate::app::tests::empty_discovery(dir.path()),
                tx.clone(),
            )
        });
        tx.send_replace(Arc::from(vec![Candidate {
            platform: Platform::Android,
            device_id: "emulator-5554".into(),
            device_name: "sdk gphone64 arm64".into(),
            pid: 1,
            vm_uri: "http://127.0.0.1:1/abc=/".into(),
        }]));
        press(cx, "cmd-,", "ctrl-,");
        assert!(exists(cx, "settings-find-device-apps"));
        assert!(cx.update(|_, cx| crate::settings::settings(cx).find_device_apps));
        assert_eq!(rx.borrow().len(), 1);
        click(cx, "settings-find-device-apps");
        assert!(!cx.update(|_, cx| crate::settings::settings(cx).find_device_apps));
        assert!(rx.borrow().is_empty());
    }
}
