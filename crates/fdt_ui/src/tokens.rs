//! Design tokens: the only place visual values (colours, sizes, radii,
//! durations) are defined. Views read everything from here.

use gpui_kit::{FontFeatures, FontWeight, Hsla, Pixels, Size, px, rgb, size};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// An opaque colour as `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

impl From<Color> for Hsla {
    fn from(c: Color) -> Self {
        c.hsla()
    }
}

impl Color {
    pub fn hsla(self) -> Hsla {
        rgb(self.0).into()
    }

    pub fn with_alpha(self, alpha: u8) -> Hsla {
        let mut c = self.hsla();
        c.a = f32::from(alpha) / 255.0;
        c
    }

    pub fn hex(self) -> String {
        format!("#{:06X}", self.0 & 0xFF_FFFF)
    }

    pub fn hex_with_alpha(self, alpha: u8) -> String {
        format!("#{:06X}{alpha:02X}", self.0 & 0xFF_FFFF)
    }

    /// WCAG 2.x relative luminance.
    pub fn relative_luminance(self) -> f64 {
        let channel = |shift: u32| {
            let c = f64::from((self.0 >> shift) & 0xFF) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    /// WCAG contrast ratio, `(L1 + 0.05) / (L2 + 0.05)` with L1 the lighter.
    pub fn contrast(self, other: Color) -> f64 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub surface: Color,
    pub elevated: Color,
    pub border: Color,
    pub text: Color,
    pub text_muted: Color,
    pub text_faint: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub lane_network: Color,
    pub lane_state: Color,
    pub scrim: Color,
}

pub const DARK: Palette = Palette {
    bg: Color(0x0D0E11),
    surface: Color(0x131519),
    elevated: Color(0x1A1C22),
    border: Color(0x23262D),
    text: Color(0xE7E8EB),
    text_muted: Color(0x8A8F99),
    text_faint: Color(0x626772),
    accent: Color(0x7B8CFF),
    success: Color(0x3FB27F),
    warning: Color(0xE2A336),
    danger: Color(0xE5534B),
    lane_network: Color(0x4EA1FF),
    lane_state: Color(0xB07CFF),
    scrim: Color(0x000000),
};

pub const LIGHT: Palette = Palette {
    bg: Color(0xFBFBFC),
    surface: Color(0xF4F5F7),
    elevated: Color(0xFFFFFF),
    border: Color(0xE3E5E9),
    text: Color(0x16181D),
    text_muted: Color(0x5C6370),
    text_faint: Color(0x888E9A),
    accent: Color(0x4E60EF),
    success: Color(0x198051),
    warning: Color(0x99651A),
    danger: Color(0xD0362E),
    lane_network: Color(0x146CE2),
    lane_state: Color(0x804CF5),
    scrim: Color(0x16181D),
};

pub fn palette(appearance: Appearance) -> &'static Palette {
    match appearance {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    }
}

/// Alpha values (0-255) for translucent fills.
pub mod alpha {
    pub const SELECTION: u8 = 0x55;
    pub const ACTIVE_ROW: u8 = 0x33;
    pub const HOVER_FILL: u8 = 0xE6;
    pub const PRESS_FILL: u8 = 0xCC;
    pub const SCRIM_DARK: u8 = 0x99;
    pub const SCRIM_LIGHT: u8 = 0x33;
}

pub mod font {
    use super::{Arc, FontFeatures, FontWeight, Pixels, px};

    pub const UI: &str = "Inter";
    pub const MONO: &str = "JetBrains Mono";

    pub const LABEL: Pixels = px(11.);
    pub const DENSE: Pixels = px(12.);
    pub const BODY: Pixels = px(13.);
    pub const SECTION: Pixels = px(14.);
    pub const PAGE: Pixels = px(16.);
    pub const EMPTY_HEADING: Pixels = px(20.);

    pub const REGULAR: FontWeight = FontWeight(400.0);
    pub const MEDIUM: FontWeight = FontWeight(500.0);
    pub const SEMIBOLD: FontWeight = FontWeight(600.0);

    pub const LINE_HEIGHT_UI: f32 = 1.4;
    pub const LINE_HEIGHT_LOG: f32 = 1.5;

    /// OpenType `tnum`: tabular figures, so digits align in columns.
    pub fn tabular_figures() -> FontFeatures {
        FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
    }
}

pub mod space {
    use super::{Pixels, px};

    pub const S2: Pixels = px(2.);
    pub const S4: Pixels = px(4.);
    pub const S6: Pixels = px(6.);
    pub const S8: Pixels = px(8.);
    pub const S12: Pixels = px(12.);
    pub const S16: Pixels = px(16.);
    pub const S20: Pixels = px(20.);
    pub const S24: Pixels = px(24.);
    pub const S32: Pixels = px(32.);
}

pub mod radius {
    use super::{Pixels, px};

    pub const CHIP: Pixels = px(4.);
    pub const CONTROL: Pixels = px(6.);
    pub const PANEL: Pixels = px(8.);
    pub const DIALOG: Pixels = px(12.);
}

pub mod row {
    use super::{Pixels, px};

    pub const TABLE: Pixels = px(26.);
    pub const LOG: Pixels = px(22.);
}

pub mod motion {
    use super::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Motion {
        Hover,
        Panel,
        Overlay,
    }

    pub const HOVER: Duration = Duration::from_millis(100);
    pub const PANEL: Duration = Duration::from_millis(160);
    pub const OVERLAY: Duration = Duration::from_millis(200);

    /// Ease-out cubic bezier control points.
    pub const EASE_OUT: [f32; 4] = [0.0, 0.0, 0.2, 1.0];

    /// The duration for `m`, or zero when the user asked for reduced motion.
    pub fn duration(m: Motion, reduce_motion: bool) -> Duration {
        if reduce_motion {
            return Duration::ZERO;
        }
        match m {
            Motion::Hover => HOVER,
            Motion::Panel => PANEL,
            Motion::Overlay => OVERLAY,
        }
    }
}

pub mod layout {
    use super::{Pixels, Size, px, size};

    pub const SIDEBAR_WIDTH: Pixels = px(208.);
    pub const WINDOW_DEFAULT: Size<Pixels> = size(px(1280.), px(800.));
    pub const WINDOW_MIN: Size<Pixels> = size(px(960.), px(600.));
    pub const PALETTE_WIDTH: Pixels = px(640.);
    pub const PALETTE_TOP_OFFSET: Pixels = px(96.);
    pub const PALETTE_MAX_HEIGHT: Pixels = px(420.);
    pub const HUD_WIDTH: Pixels = px(232.);
    pub const HUD_GRAPH_HEIGHT: Pixels = px(40.);
    pub const FOCUS_RING: Pixels = px(2.);
    pub const BORDER: Pixels = px(1.);
    pub const ICON_SM: Pixels = px(14.);
    pub const ICON_MD: Pixels = px(16.);
    pub const EMPTY_STATE_MAX_WIDTH: Pixels = px(360.);
    pub const PALETTE_BUTTON_WIDTH: Pixels = px(280.);
}

#[cfg(test)]
mod tests {
    use super::{Color, DARK, LIGHT, Palette, font, motion, radius, row, space};
    use gpui_kit::px;
    use std::time::Duration;

    fn rows(p: &Palette) -> [Color; 14] {
        [
            p.bg,
            p.surface,
            p.elevated,
            p.border,
            p.text,
            p.text_muted,
            p.text_faint,
            p.accent,
            p.success,
            p.warning,
            p.danger,
            p.lane_network,
            p.lane_state,
            p.scrim,
        ]
    }

    #[test]
    fn palettes_match_spec_values() {
        let dark = [
            0x0D0E11, 0x131519, 0x1A1C22, 0x23262D, 0xE7E8EB, 0x8A8F99, 0x626772, 0x7B8CFF,
            0x3FB27F, 0xE2A336, 0xE5534B, 0x4EA1FF, 0xB07CFF, 0x000000,
        ];
        let light = [
            0xFBFBFC, 0xF4F5F7, 0xFFFFFF, 0xE3E5E9, 0x16181D, 0x5C6370, 0x888E9A, 0x4E60EF,
            0x198051, 0x99651A, 0xD0362E, 0x146CE2, 0x804CF5, 0x16181D,
        ];
        for (p, want) in [(&DARK, dark), (&LIGHT, light)] {
            for (i, c) in rows(p).iter().enumerate() {
                assert_eq!(*c, Color(want[i]), "field index {i}");
            }
        }
    }

    fn check(fields: impl Fn(&Palette) -> Vec<Color>, min: f64) {
        for p in [&DARK, &LIGHT] {
            for s in [p.bg, p.surface, p.elevated] {
                for c in fields(p) {
                    let r = c.contrast(s);
                    assert!(r >= min, "{} on {} = {r:.2}", c.hex(), s.hex());
                }
            }
        }
    }

    #[test]
    fn body_text_meets_4_5_on_every_surface() {
        check(|p| vec![p.text, p.text_muted], 4.5);
    }

    #[test]
    fn accent_status_and_lanes_meet_4_5_on_every_surface() {
        check(
            |p| {
                vec![
                    p.accent,
                    p.success,
                    p.warning,
                    p.danger,
                    p.lane_network,
                    p.lane_state,
                ]
            },
            4.5,
        );
    }

    #[test]
    fn text_faint_meets_3_on_every_surface() {
        check(|p| vec![p.text_faint], 3.0);
    }

    #[test]
    fn black_on_white_contrast_is_21() {
        assert!((Color(0).contrast(Color(0xFFFFFF)) - 21.0).abs() < 1e-9);
    }

    #[test]
    fn hex_formatting() {
        assert_eq!(Color(0x0D0E11).hex(), "#0D0E11");
        assert_eq!(Color(0x0D0E11).hex_with_alpha(0x99), "#0D0E1199");
    }

    #[test]
    fn scales_match_spec() {
        assert_eq!(
            [
                font::LABEL,
                font::DENSE,
                font::BODY,
                font::SECTION,
                font::PAGE,
                font::EMPTY_HEADING
            ],
            [px(11.), px(12.), px(13.), px(14.), px(16.), px(20.)]
        );
        assert_eq!(
            [
                space::S2,
                space::S4,
                space::S6,
                space::S8,
                space::S12,
                space::S16,
                space::S20,
                space::S24,
                space::S32
            ],
            [
                px(2.),
                px(4.),
                px(6.),
                px(8.),
                px(12.),
                px(16.),
                px(20.),
                px(24.),
                px(32.)
            ]
        );
        assert_eq!(
            [radius::CHIP, radius::CONTROL, radius::PANEL, radius::DIALOG],
            [px(4.), px(6.), px(8.), px(12.)]
        );
        assert_eq!((row::TABLE, row::LOG), (px(26.), px(22.)));
        assert!(
            font::tabular_figures()
                .tag_value_list()
                .contains(&("tnum".to_string(), 1))
        );
    }

    #[test]
    fn motion_durations_and_reduced_motion() {
        use motion::Motion::{Hover, Overlay, Panel};
        for (m, ms) in [(Hover, 100), (Panel, 160), (Overlay, 200)] {
            assert_eq!(motion::duration(m, false), Duration::from_millis(ms));
            assert_eq!(motion::duration(m, true), Duration::ZERO);
        }
    }
}
