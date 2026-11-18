//! Maps the design tokens onto the component theme and installs it.

use std::rc::Rc;

use gpui_kit::base::Easing;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeConfig, ThemeMode};
use gpui_kit::{App, Window, WindowAppearance};
use serde_json::{Map, Value, json};

use crate::tokens::{Appearance, Palette, alpha, font, motion, palette, radius};

/// What the user picked in settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Dark,
    Light,
}

/// The appearance to show: a pinned preference wins, "system" follows the OS.
pub fn resolve(pref: ThemePreference, os: Appearance) -> Appearance {
    match pref {
        ThemePreference::System => os,
        ThemePreference::Dark => Appearance::Dark,
        ThemePreference::Light => Appearance::Light,
    }
}

/// The component theme config (as JSON) for one appearance.
pub fn component_theme_json(appearance: Appearance) -> Value {
    let p = palette(appearance);
    let dark = appearance == Appearance::Dark;
    let on_accent = if dark { p.bg } else { p.elevated };
    let mut colors = Map::new();
    let mut set = |keys: &[&str], value: String| {
        for key in keys {
            colors.insert((*key).to_string(), Value::String(value.clone()));
        }
    };

    set(
        &[
            "background",
            "list.background",
            "list.even.background",
            "table.background",
            "table.even.background",
            "tab.active.background",
        ],
        p.bg.hex(),
    );
    set(
        &[
            "muted.background",
            "secondary.background",
            "sidebar.background",
            "title_bar.background",
            "status_bar.background",
            "tab_bar.background",
            "tab.background",
            "list.head.background",
            "table.head.background",
            "list.hover.background",
            "table.hover.background",
            "skeleton.background",
            "group_box.background",
            "accordion.background",
        ],
        p.surface.hex(),
    );
    set(
        &[
            "popover.background",
            "accent.background",
            "secondary.hover.background",
            "sidebar.accent.background",
        ],
        p.elevated.hex(),
    );
    set(
        &[
            "border",
            "input.border",
            "sidebar.border",
            "title_bar.border",
            "status_bar.border",
            "table.row.border",
            "window.border",
            "secondary.active.background",
            "chart.grid",
        ],
        p.border.hex(),
    );
    set(
        &[
            "foreground",
            "popover.foreground",
            "accent.foreground",
            "secondary.foreground",
            "sidebar.foreground",
            "sidebar.accent.foreground",
            "tab.active.foreground",
            "group_box.foreground",
        ],
        p.text.hex(),
    );
    set(
        &[
            "muted.foreground",
            "tab.foreground",
            "table.head.foreground",
            "description_list_label.foreground",
        ],
        p.text_muted.hex(),
    );
    set(
        &[
            "primary.background",
            "ring",
            "caret",
            "link.foreground",
            "link.hover.foreground",
            "link.active.foreground",
            "list.active.border",
            "table.active.border",
            "drag_border",
            "info.background",
            "chart.1",
            "sidebar.primary.background",
            "progress_bar.background",
            "slider.bar.background",
        ],
        p.accent.hex(),
    );
    set(
        &["primary.hover.background"],
        p.accent.hex_with_alpha(alpha::HOVER_FILL),
    );
    set(
        &["primary.active.background"],
        p.accent.hex_with_alpha(alpha::PRESS_FILL),
    );
    set(
        &[
            "primary.foreground",
            "sidebar.primary.foreground",
            "info.foreground",
            "success.foreground",
            "warning.foreground",
            "danger.foreground",
        ],
        on_accent.hex(),
    );
    set(
        &["selection.background"],
        p.accent.hex_with_alpha(alpha::SELECTION),
    );
    set(
        &[
            "list.active.background",
            "table.active.background",
            "drop_target.background",
        ],
        p.accent.hex_with_alpha(alpha::ACTIVE_ROW),
    );
    set(&["success.background", "chart.bullish"], p.success.hex());
    set(&["warning.background"], p.warning.hex());
    set(&["danger.background", "chart.bearish"], p.danger.hex());
    set(&["chart.2"], p.lane_network.hex());
    set(&["chart.3"], p.success.hex());
    set(&["chart.4"], p.warning.hex());
    set(&["chart.5"], p.lane_state.hex());
    set(&["scrollbar.thumb.background"], p.text_faint.hex());
    set(
        &["overlay"],
        p.scrim.hex_with_alpha(if dark {
            alpha::SCRIM_DARK
        } else {
            alpha::SCRIM_LIGHT
        }),
    );

    json!({
        "name": if dark { "fdt Dark" } else { "fdt Light" },
        "mode": if dark { "dark" } else { "light" },
        "is_default": false,
        "font.family": font::UI,
        "font.size": f32::from(font::BODY),
        "mono_font.family": font::MONO,
        "mono_font.size": f32::from(font::DENSE),
        "radius": f32::from(radius::CONTROL) as usize,
        "radius.lg": f32::from(radius::PANEL) as usize,
        "shadow": false,
        "colors": colors,
    })
}

fn config(appearance: Appearance) -> ThemeConfig {
    serde_json::from_value(component_theme_json(appearance))
        .expect("the fdt theme JSON matches the component theme schema")
}

fn apply_motion(theme: &mut Theme) {
    let [x1, y1, x2, y2] = motion::EASE_OUT;
    let ease = Easing::cubic_bezier(x1, y1, x2, y2).expect("EASE_OUT is a valid curve");
    theme.motion.duration_fast = motion::HOVER;
    theme.motion.duration_normal = motion::PANEL;
    theme.motion.duration_slow = motion::OVERLAY;
    theme.motion.easing_enter = ease.clone();
    theme.motion.easing_exit = ease.clone();
    theme.motion.easing_move = ease;
    theme.focus_ring = true;
}

/// Installs the fdt themes into the component theme. Call once, after `gpui_kit::init`.
pub fn install(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    theme.dark_theme = Rc::new(config(Appearance::Dark));
    theme.light_theme = Rc::new(config(Appearance::Light));
    apply_motion(theme);
}

/// The OS appearance, from the window when there is one.
pub fn os_appearance(window: Option<&Window>, cx: &App) -> Appearance {
    let os = match window {
        Some(window) => window.appearance(),
        None => cx.window_appearance(),
    };
    match os {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
    }
}

/// Applies the preference and returns the appearance now showing.
pub fn apply(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) -> Appearance {
    let os = os_appearance(window.as_deref(), cx);
    apply_with_os(pref, os, window, cx)
}

/// Like [`apply`], with the OS appearance given instead of read from the window.
pub fn apply_with_os(
    pref: ThemePreference,
    os: Appearance,
    window: Option<&mut Window>,
    cx: &mut App,
) -> Appearance {
    let appearance = resolve(pref, os);
    let mode = match appearance {
        Appearance::Dark => ThemeMode::Dark,
        Appearance::Light => ThemeMode::Light,
    };
    Theme::change(mode, window, cx);
    // `change` reloads the config, so the motion values go back on afterwards.
    apply_motion(Theme::global_mut(cx));
    appearance
}

/// Reads the active palette from the app.
pub trait TokensExt {
    fn tokens(&self) -> &'static Palette;
    fn appearance(&self) -> Appearance;
}

impl TokensExt for App {
    fn tokens(&self) -> &'static Palette {
        palette(self.appearance())
    }

    fn appearance(&self) -> Appearance {
        if self.theme().is_dark() {
            Appearance::Dark
        } else {
            Appearance::Light
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{DARK, LIGHT};
    use gpui_kit::{TestAppContext, px};

    #[test]
    fn resolve_follows_os_only_for_system() {
        use Appearance::{Dark, Light};
        use ThemePreference::{Dark as PinDark, Light as PinLight, System};
        assert_eq!(resolve(System, Dark), Dark);
        assert_eq!(resolve(System, Light), Light);
        assert_eq!(resolve(PinDark, Light), Dark);
        assert_eq!(resolve(PinDark, Dark), Dark);
        assert_eq!(resolve(PinLight, Dark), Light);
        assert_eq!(resolve(PinLight, Light), Light);
    }

    #[test]
    fn component_theme_maps_tokens() {
        let json = component_theme_json(Appearance::Dark);
        assert_eq!(json["colors"]["background"], "#0D0E11");
        assert_eq!(json["colors"]["muted.foreground"], DARK.text_muted.hex());
        assert_eq!(json["colors"]["primary.background"], DARK.accent.hex());
        assert_eq!(json["colors"]["sidebar.background"], DARK.surface.hex());
        assert_eq!(
            json["colors"]["selection.background"],
            DARK.accent.hex_with_alpha(alpha::SELECTION)
        );
        assert_eq!(json["font.family"], "Inter");
        assert_eq!(json["mono_font.family"], "JetBrains Mono");
        assert_eq!(json["font.size"].as_f64(), Some(13.0));
        assert_eq!(json["radius"].as_u64(), Some(6));
        assert_eq!(json["radius.lg"].as_u64(), Some(8));
        assert_eq!(json["shadow"], false);
        assert_eq!(json["mode"], "dark");
        let light = component_theme_json(Appearance::Light);
        assert_eq!(light["colors"]["background"], "#FBFBFC");
    }

    #[test]
    fn every_color_value_is_a_hex_token() {
        for a in [Appearance::Dark, Appearance::Light] {
            let json = component_theme_json(a);
            for (key, value) in json["colors"].as_object().unwrap() {
                let s = value
                    .as_str()
                    .unwrap_or_else(|| panic!("{key} not a string"));
                assert!(
                    s.starts_with('#') && (s.len() == 7 || s.len() == 9),
                    "{key}: {s}"
                );
            }
        }
    }

    #[gpui_kit::test]
    fn apply_switches_component_theme_and_motion(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            install(cx);
            assert_eq!(apply(ThemePreference::Dark, None, cx), Appearance::Dark);
            let t = Theme::global(cx);
            assert_eq!(t.colors.background, DARK.bg.hsla());
            assert_eq!(t.colors.foreground, DARK.text.hsla());
            assert_eq!(t.colors.border, DARK.border.hsla());
            assert_eq!(t.colors.danger, DARK.danger.hsla());
            assert_eq!(t.font_family.as_ref(), "Inter");
            assert_eq!(t.mono_font_family.as_ref(), "JetBrains Mono");
            assert_eq!(t.font_size, px(13.));
            assert_eq!(t.radius, radius::CONTROL);
            assert_eq!(t.radius_lg, radius::PANEL);
            assert_eq!(t.motion.duration_fast, motion::HOVER);
            assert_eq!(t.motion.duration_normal, motion::PANEL);
            assert_eq!(t.motion.duration_slow, motion::OVERLAY);
            assert_eq!(cx.tokens(), &DARK);
            apply(ThemePreference::Light, None, cx);
            assert_eq!(Theme::global(cx).colors.background, LIGHT.bg.hsla());
            assert_eq!(cx.appearance(), Appearance::Light);
        });
    }
}
