//! The bundled UI and monospace fonts.

use std::borrow::Cow;

use gpui_kit::App;

/// A font file embedded in the binary.
pub struct FontFile {
    pub name: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! font {
    ($name:literal) => {
        FontFile {
            name: $name,
            bytes: include_bytes!(concat!("../assets/fonts/", $name)),
        }
    };
}

pub const FONT_FILES: [FontFile; 5] = [
    font!("Inter-Regular.ttf"),
    font!("Inter-Medium.ttf"),
    font!("Inter-SemiBold.ttf"),
    font!("JetBrainsMono-Regular.ttf"),
    font!("JetBrainsMono-Medium.ttf"),
];

/// Registers the bundled fonts with the text system.
pub fn load(cx: &mut App) -> anyhow::Result<()> {
    let fonts = FONT_FILES
        .iter()
        .map(|f| Cow::Borrowed(f.bytes))
        .collect::<Vec<_>>();
    cx.text_system().add_fonts(fonts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_files_are_bundled_truetype() {
        let names: Vec<_> = FONT_FILES.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "Inter-Regular.ttf",
                "Inter-Medium.ttf",
                "Inter-SemiBold.ttf",
                "JetBrainsMono-Regular.ttf",
                "JetBrainsMono-Medium.ttf"
            ]
        );
        for f in &FONT_FILES {
            assert_eq!(&f.bytes[..4], &[0, 1, 0, 0], "{}", f.name);
        }
    }
}
