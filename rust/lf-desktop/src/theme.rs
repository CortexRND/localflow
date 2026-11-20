//! Design tokens: the only place colours and spacing values are defined.
//!
//! Role names follow Astryx semantic tokens (`bg.body`, `text.secondary`,
//! `border.control`, ...), translated to a native struct. Two instances exist
//! (light and dark); the app follows the system appearance.

use gpui::{px, rgb, rgba, App, Global, Hsla, Pixels, Rgba, SharedString, WindowAppearance};

/// `0xRRGGBB` as an opaque colour.
fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// `0xRRGGBB` at `alpha` (0..=1).
fn hex_a(value: u32, alpha: f32) -> Hsla {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((value << 8) | a).into()
}

/// Raw sRGB of a token, for contrast checks.
pub fn srgb(value: u32) -> (f64, f64, f64) {
    let c: Rgba = rgb(value);
    (c.r as f64, c.g as f64, c.b as f64)
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub bg_body: u32,
    pub bg_surface: u32,
    pub bg_card: u32,
    pub bg_popover: u32,
    pub bg_muted: u32,
    pub bg_inverted: u32,
    pub overlay_base: u32,
    pub hover_alpha: f32,
    pub pressed_alpha: f32,
    pub selected_alpha: f32,
    pub scrim: u32,
    pub scrim_alpha: f32,
    pub text_primary: u32,
    pub text_secondary: u32,
    pub text_disabled: u32,
    pub text_accent: u32,
    pub text_on_accent: u32,
    pub text_inverted: u32,
    pub icon_secondary: u32,
    pub icon_accent: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub accent_muted: u32,
    pub border: u32,
    pub border_control: u32,
    pub success: (u32, u32),
    pub warning: (u32, u32),
    pub error: (u32, u32),
    pub info: (u32, u32),
    pub recording: u32,
    pub sandbox: (u32, u32),
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        bg_body: 0xF4F2EE,
        bg_surface: 0xFCFBF9,
        bg_card: 0xFFFFFF,
        bg_popover: 0xFFFFFF,
        bg_muted: 0xECE9E3,
        bg_inverted: 0x1F1E1B,
        overlay_base: 0x1F1E1B,
        hover_alpha: 0.05,
        pressed_alpha: 0.10,
        selected_alpha: 0.08,
        scrim: 0x1F1E1B,
        scrim_alpha: 0.45,
        text_primary: 0x1F1E1B,
        text_secondary: 0x5E5B54,
        text_disabled: 0xA8A49B,
        text_accent: 0x2E5A3E,
        text_on_accent: 0xFFFFFF,
        text_inverted: 0xEDEAE4,
        icon_secondary: 0x77736A,
        icon_accent: 0x2E5A3E,
        accent: 0x2E5A3E,
        accent_hover: 0x264C34,
        accent_muted: 0xDDE9DF,
        border: 0xE4E0D8,
        border_control: 0x8C877C,
        success: (0x354F31, 0xD2ECCF),
        warning: (0x524818, 0xF4E1B7),
        error: (0x8A2F24, 0xFBDAD4),
        info: (0x344861, 0xD3E4FC),
        recording: 0xB23A2B,
        sandbox: (0x524818, 0xF4E1B7),
    };

    pub const DARK: Palette = Palette {
        bg_body: 0x161513,
        bg_surface: 0x1C1B18,
        bg_card: 0x232220,
        bg_popover: 0x2B2A27,
        bg_muted: 0x2B2A27,
        bg_inverted: 0xEDEAE4,
        overlay_base: 0xEDEAE4,
        hover_alpha: 0.06,
        pressed_alpha: 0.11,
        selected_alpha: 0.09,
        scrim: 0x000000,
        scrim_alpha: 0.60,
        text_primary: 0xEDEAE4,
        text_secondary: 0xA9A59C,
        text_disabled: 0x6B6861,
        text_accent: 0xA5D0AE,
        text_on_accent: 0x12241A,
        text_inverted: 0x1F1E1B,
        icon_secondary: 0x9A968D,
        icon_accent: 0x8FBF9A,
        accent: 0x8FBF9A,
        accent_hover: 0xA5D0AE,
        accent_muted: 0x24382B,
        border: 0x32302C,
        border_control: 0x7A766D,
        success: (0xD5E9D2, 0x2F412C),
        warning: (0xF1E1B9, 0x433B1B),
        error: (0xF7DBD7, 0x50332E),
        info: (0xD5E4F8, 0x2C3C52),
        recording: 0xE0786A,
        sandbox: (0xF1E1B9, 0x433B1B),
    };
}

/// Status tone of a dot, token or banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Success,
    Warning,
    Error,
    Info,
    Accent,
}

/// Resolved tokens installed as a GPUI global.
#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    /// UI font family chosen at startup from the installed fonts.
    pub font_family: SharedString,
    pub palette: Palette,
    pub bg_body: Hsla,
    pub bg_surface: Hsla,
    pub bg_card: Hsla,
    pub bg_popover: Hsla,
    pub bg_muted: Hsla,
    pub bg_inverted: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub selected: Hsla,
    pub scrim: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_disabled: Hsla,
    pub text_accent: Hsla,
    pub text_on_accent: Hsla,
    pub text_inverted: Hsla,
    pub icon_secondary: Hsla,
    pub icon_accent: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_muted: Hsla,
    pub border: Hsla,
    pub border_control: Hsla,
    pub recording: Hsla,
    pub sandbox_text: Hsla,
    pub sandbox_bg: Hsla,
}

impl Global for Theme {}

impl Theme {
    pub fn from_palette(palette: Palette, dark: bool) -> Self {
        let p = &palette;
        Theme {
            dark,
            font_family: ".SystemUIFont".into(),
            bg_body: hex(p.bg_body),
            bg_surface: hex(p.bg_surface),
            bg_card: hex(p.bg_card),
            bg_popover: hex(p.bg_popover),
            bg_muted: hex(p.bg_muted),
            bg_inverted: hex(p.bg_inverted),
            hover: hex_a(p.overlay_base, p.hover_alpha),
            pressed: hex_a(p.overlay_base, p.pressed_alpha),
            selected: hex_a(p.overlay_base, p.selected_alpha),
            scrim: hex_a(p.scrim, p.scrim_alpha),
            text: hex(p.text_primary),
            text_secondary: hex(p.text_secondary),
            text_disabled: hex(p.text_disabled),
            text_accent: hex(p.text_accent),
            text_on_accent: hex(p.text_on_accent),
            text_inverted: hex(p.text_inverted),
            icon_secondary: hex(p.icon_secondary),
            icon_accent: hex(p.icon_accent),
            accent: hex(p.accent),
            accent_hover: hex(p.accent_hover),
            accent_muted: hex(p.accent_muted),
            border: hex(p.border),
            border_control: hex(p.border_control),
            recording: hex(p.recording),
            sandbox_text: hex(p.sandbox.0),
            sandbox_bg: hex(p.sandbox.1),
            palette,
        }
    }

    pub fn light() -> Self {
        Self::from_palette(Palette::LIGHT, false)
    }

    pub fn dark() -> Self {
        Self::from_palette(Palette::DARK, true)
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            _ => Self::light(),
        }
    }

    /// `(text, background)` of a status tone.
    pub fn tone(&self, tone: Tone) -> (Hsla, Hsla) {
        let p = &self.palette;
        let pair = |(t, b): (u32, u32)| (hex(t), hex(b));
        match tone {
            Tone::Neutral => (self.text_secondary, self.bg_muted),
            Tone::Success => pair(p.success),
            Tone::Warning => pair(p.warning),
            Tone::Error => pair(p.error),
            Tone::Info => pair(p.info),
            Tone::Accent => (self.text_accent, self.accent_muted),
        }
    }

    /// Dot colour of a status tone (stronger than the text pair).
    pub fn dot(&self, tone: Tone) -> Hsla {
        match tone {
            Tone::Neutral => self.icon_secondary,
            Tone::Success | Tone::Accent => self.accent,
            Tone::Warning => hex(if self.dark { 0xE0B85A } else { 0xB7861B }),
            Tone::Error => self.recording,
            Tone::Info => hex(if self.dark { 0x8DB4E8 } else { 0x3F6CA8 }),
        }
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// Spacing scale (Astryx `--spacing-*`, with half steps).
pub mod space {
    use super::*;
    pub fn s1() -> Pixels {
        px(2.)
    }
    pub fn s2() -> Pixels {
        px(4.)
    }
    pub fn s3() -> Pixels {
        px(6.)
    }
    pub fn s4() -> Pixels {
        px(8.)
    }
    pub fn s5() -> Pixels {
        px(12.)
    }
    pub fn s6() -> Pixels {
        px(16.)
    }
    pub fn s7() -> Pixels {
        px(20.)
    }
    pub fn s8() -> Pixels {
        px(24.)
    }
    pub fn s10() -> Pixels {
        px(32.)
    }
    pub fn s12() -> Pixels {
        px(48.)
    }
}

/// Control heights, radii and structural widths.
pub mod size {
    use super::*;
    pub fn control_sm() -> Pixels {
        px(28.)
    }
    pub fn control_md() -> Pixels {
        px(32.)
    }
    pub fn control_lg() -> Pixels {
        px(36.)
    }
    pub fn radius_inner() -> Pixels {
        px(4.)
    }
    pub fn radius_element() -> Pixels {
        px(8.)
    }
    pub fn radius_container() -> Pixels {
        px(12.)
    }
    pub fn icon() -> Pixels {
        px(16.)
    }
    pub fn icon_lg() -> Pixels {
        px(20.)
    }
    /// Structural widths (the only raw widths pages may use).
    pub const SIDEBAR: f32 = 232.;
    pub const COLUMN: f32 = 760.;
    pub const LIST_PANE: f32 = 340.;
    pub const READING: f32 = 640.;
}

/// Type scale (base 14, ratio 1.2) as `(size, line height)`.
pub mod text {
    use super::*;
    pub fn title() -> (Pixels, Pixels) {
        (px(24.), px(32.))
    }
    pub fn heading() -> (Pixels, Pixels) {
        (px(17.), px(24.))
    }
    pub fn body() -> (Pixels, Pixels) {
        (px(14.), px(20.))
    }
    pub fn reading() -> (Pixels, Pixels) {
        (px(15.), px(24.))
    }
    pub fn supporting() -> (Pixels, Pixels) {
        (px(12.), px(18.))
    }
    pub fn mono() -> (Pixels, Pixels) {
        (px(13.), px(20.))
    }
    pub fn stat() -> (Pixels, Pixels) {
        (px(24.), px(28.))
    }
    pub const MONO_FAMILY: &str = "JetBrains Mono";
}

/// WCAG relative luminance contrast of two `0xRRGGBB` colours.
pub fn contrast(a: u32, b: u32) -> f64 {
    fn channel(c: f64) -> f64 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn lum(v: u32) -> f64 {
        let (r, g, b) = srgb(v);
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_requirements_hold_in_both_palettes() {
        for p in [Palette::LIGHT, Palette::DARK] {
            for bg in [p.bg_body, p.bg_surface, p.bg_card] {
                assert!(contrast(p.text_primary, bg) >= 4.5);
                assert!(
                    contrast(p.text_secondary, bg) >= 4.5,
                    "secondary on {bg:06x}"
                );
            }
            assert!(contrast(p.text_on_accent, p.accent) >= 4.5);
            for (t, b) in [p.success, p.warning, p.error, p.info, p.sandbox] {
                assert!(contrast(t, b) >= 4.5, "{t:06x} on {b:06x}");
            }
            for bg in [p.bg_card, p.bg_surface] {
                assert!(contrast(p.border_control, bg) >= 3.0);
            }
            for bg in [p.bg_body, p.bg_surface] {
                assert!(contrast(p.accent, bg) >= 3.0);
            }
            assert!(contrast(p.text_accent, p.bg_surface) >= 4.5);
        }
    }

    /// Pages and components take colours only from tokens.
    #[test]
    fn no_colour_literals_outside_the_theme() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let hex = regex_lite(r"0x[0-9A-Fa-f]{6}\b");
        for file in files.iter().filter(|f| !f.ends_with("theme.rs")) {
            let text = std::fs::read_to_string(file).unwrap();
            for needle in ["rgb(", "rgba(", "hsla(", "gpui::red(", "gpui::blue("] {
                assert!(!text.contains(needle), "{needle} in {}", file.display());
            }
            assert!(!hex(&text), "hex colour literal in {}", file.display());
        }
    }

    /// Minimal matcher for `0x` + 6 hex digits (no regex dependency).
    fn regex_lite(_: &str) -> impl Fn(&str) -> bool {
        |text: &str| {
            let b = text.as_bytes();
            (0..b.len().saturating_sub(7)).any(|i| {
                b[i] == b'0'
                    && b[i + 1] == b'x'
                    && b[i + 2..i + 8].iter().all(u8::is_ascii_hexdigit)
                    && b.get(i + 8).is_none_or(|c| !c.is_ascii_hexdigit())
            })
        }
    }
}
