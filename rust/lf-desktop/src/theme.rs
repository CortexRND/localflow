//! Design tokens: the only place colours and spacing values are defined.
//!
//! Role names follow Astryx semantic tokens (`bg.body`, `text.secondary`,
//! `border.control`, ...), translated to a native struct. Two palettes exist
//! (light and dark), modelled on macOS: calm neutral surfaces, one restrained
//! blue accent for the primary action and selection, a neutral focus edge,
//! and hierarchy from weight and colour rather than borders. Which palette
//! is shown follows the [`AppearanceMode`] (System follows the OS setting).

use gpui::{
    point, px, rgb, rgba, App, BoxShadow, Global, Hsla, Pixels, Rgba, SharedString,
    WindowAppearance,
};

pub use crate::appearance::AppearanceMode;

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
    /// Sidebar and window chrome.
    pub bg_body: u32,
    /// The content pane.
    pub bg_surface: u32,
    /// Grouped containers, one step off the surface.
    pub bg_card: u32,
    /// Menus, dialogs, toasts and tooltips (with shadow and hairline).
    pub bg_popover: u32,
    /// Tracks, keycaps and neutral tokens.
    pub bg_muted: u32,
    pub bg_inverted: u32,
    /// Button and input fill.
    pub bg_control: u32,
    /// A raised segment on a muted track.
    pub bg_raised: u32,
    /// The selected sidebar row.
    pub bg_sidebar_selected: u32,
    /// Switch knob.
    pub knob: u32,
    pub overlay_base: u32,
    pub hover_alpha: f32,
    pub pressed_alpha: f32,
    pub selected_alpha: f32,
    pub scrim: u32,
    pub scrim_alpha: f32,
    pub text_primary: u32,
    pub text_secondary: u32,
    /// Disabled controls only, never content.
    pub text_disabled: u32,
    pub text_accent: u32,
    pub text_on_accent: u32,
    pub text_inverted: u32,
    pub icon_secondary: u32,
    /// Accent for non-text marks (icons, progress fill):
    /// >= 3:1 on every surface it sits on.
    pub icon_accent: u32,
    /// Accent fill (primary buttons, switch track, selection).
    pub accent: u32,
    pub accent_hover: u32,
    pub accent_muted: u32,
    /// Outer hairline of groups, panes and popovers (decorative).
    pub border: u32,
    /// Inset separator between rows inside a group (decorative).
    pub separator: u32,
    /// Boundary of interactive controls (>= 3:1 on card and surface).
    pub border_control: u32,
    /// Neutral keyboard-focus edge: >= 3:1 on every surface a control sits
    /// on or is filled with.
    pub focus: u32,
    /// `(text, background)` of neutral tokens and unselected segments.
    pub neutral: (u32, u32),
    pub success: (u32, u32),
    pub warning: (u32, u32),
    pub error: (u32, u32),
    pub info: (u32, u32),
    /// Status dot colours (non-text indicators, >= 3:1 on card and body).
    pub dot_success: u32,
    pub dot_warning: u32,
    pub dot_info: u32,
    pub recording: u32,
    pub sandbox: (u32, u32),
    /// Shadow colour and its alpha per elevation.
    pub shadow: u32,
    pub shadow_control_alpha: f32,
    pub shadow_raised_alpha: f32,
    pub shadow_popover_alpha: f32,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        bg_body: 0xF5F5F7,
        bg_surface: 0xFFFFFF,
        bg_card: 0xF5F5F7,
        bg_popover: 0xFFFFFF,
        bg_muted: 0xE8E8ED,
        bg_inverted: 0x1D1D1F,
        bg_control: 0xFFFFFF,
        bg_raised: 0xFFFFFF,
        bg_sidebar_selected: 0xDEDEE3,
        knob: 0xFFFFFF,
        overlay_base: 0x000000,
        hover_alpha: 0.05,
        pressed_alpha: 0.09,
        selected_alpha: 0.07,
        scrim: 0x000000,
        scrim_alpha: 0.28,
        text_primary: 0x1D1D1F,
        text_secondary: 0x6E6E73,
        text_disabled: 0xB0B0B5,
        text_accent: 0x0066CC,
        text_on_accent: 0xFFFFFF,
        text_inverted: 0xF5F5F7,
        icon_secondary: 0x7C7C80,
        icon_accent: 0x0066CC,
        accent: 0x0071E3,
        accent_hover: 0x0064CA,
        accent_muted: 0xE6F0FC,
        border: 0xE3E3E8,
        separator: 0xDCDCE1,
        border_control: 0x86868B,
        focus: 0x6E6E73,
        neutral: (0x5E5E63, 0xE8E8ED),
        success: (0x1B6B2F, 0xE3F4E6),
        warning: (0x8A4600, 0xFCEFD8),
        error: (0xB0151E, 0xFDE8E7),
        info: (0x0058B0, 0xE6F0FC),
        dot_success: 0x248A3D,
        dot_warning: 0xC26A00,
        dot_info: 0x0071E3,
        recording: 0xD70015,
        sandbox: (0x8A4600, 0xFCEFD8),
        shadow: 0x000000,
        shadow_control_alpha: 0.07,
        shadow_raised_alpha: 0.16,
        shadow_popover_alpha: 0.16,
    };

    pub const DARK: Palette = Palette {
        bg_body: 0x161618,
        bg_surface: 0x1E1E20,
        bg_card: 0x2A2A2C,
        bg_popover: 0x2C2C2E,
        bg_muted: 0x3A3A3C,
        bg_inverted: 0xE5E5EA,
        bg_control: 0x343437,
        bg_raised: 0x5A5A5E,
        bg_sidebar_selected: 0x323235,
        knob: 0xFFFFFF,
        overlay_base: 0xFFFFFF,
        hover_alpha: 0.06,
        pressed_alpha: 0.10,
        selected_alpha: 0.08,
        scrim: 0x000000,
        scrim_alpha: 0.50,
        text_primary: 0xF5F5F7,
        text_secondary: 0xA1A1A6,
        text_disabled: 0x5C5C60,
        text_accent: 0x409CFF,
        text_on_accent: 0xFFFFFF,
        text_inverted: 0x1D1D1F,
        icon_secondary: 0x8E8E93,
        icon_accent: 0x409CFF,
        accent: 0x0071E3,
        accent_hover: 0x0064CA,
        accent_muted: 0x14304F,
        border: 0x38383A,
        separator: 0x3A3A3C,
        border_control: 0x86868B,
        focus: 0xAEAEB2,
        neutral: (0xAEAEB2, 0x3A3A3C),
        success: (0x63D987, 0x173322),
        warning: (0xFFC864, 0x3A2B12),
        error: (0xFF8D85, 0x401C1C),
        info: (0x73B6FF, 0x14304F),
        dot_success: 0x30D158,
        dot_warning: 0xFF9F0A,
        dot_info: 0x409CFF,
        recording: 0xFF453A,
        sandbox: (0xFFC864, 0x3A2B12),
        shadow: 0x000000,
        shadow_control_alpha: 0.30,
        shadow_raised_alpha: 0.40,
        shadow_popover_alpha: 0.50,
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
    /// The appearance the user chose (System follows the OS).
    pub mode: AppearanceMode,
    /// UI font family chosen at startup from the installed fonts.
    pub font_family: SharedString,
    pub palette: Palette,
    pub bg_body: Hsla,
    pub bg_surface: Hsla,
    pub bg_card: Hsla,
    pub bg_popover: Hsla,
    pub bg_muted: Hsla,
    pub bg_inverted: Hsla,
    pub bg_control: Hsla,
    pub bg_raised: Hsla,
    pub bg_sidebar_selected: Hsla,
    pub knob: Hsla,
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
    pub separator: Hsla,
    pub border_control: Hsla,
    /// Neutral keyboard-focus edge.
    pub focus: Hsla,
    pub recording: Hsla,
    pub sandbox_text: Hsla,
    pub sandbox_bg: Hsla,
    pub shadow_control: Hsla,
    pub shadow_raised: Hsla,
    pub shadow_popover: Hsla,
}

impl Global for Theme {}

/// Whether `mode` shows the dark palette: Light and Dark ignore the OS,
/// System follows it (Dark and VibrantDark are dark, everything else light).
pub fn resolve_dark(mode: AppearanceMode, system: WindowAppearance) -> bool {
    match mode {
        AppearanceMode::Light => false,
        AppearanceMode::Dark => true,
        AppearanceMode::System => matches!(
            system,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    }
}

impl Theme {
    pub fn from_palette(palette: Palette, dark: bool) -> Self {
        let p = &palette;
        Theme {
            dark,
            mode: AppearanceMode::System,
            font_family: ".SystemUIFont".into(),
            bg_body: hex(p.bg_body),
            bg_surface: hex(p.bg_surface),
            bg_card: hex(p.bg_card),
            bg_popover: hex(p.bg_popover),
            bg_muted: hex(p.bg_muted),
            bg_inverted: hex(p.bg_inverted),
            bg_control: hex(p.bg_control),
            bg_raised: hex(p.bg_raised),
            bg_sidebar_selected: hex(p.bg_sidebar_selected),
            knob: hex(p.knob),
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
            separator: hex(p.separator),
            border_control: hex(p.border_control),
            focus: hex(p.focus),
            recording: hex(p.recording),
            sandbox_text: hex(p.sandbox.0),
            sandbox_bg: hex(p.sandbox.1),
            shadow_control: hex_a(p.shadow, p.shadow_control_alpha),
            shadow_raised: hex_a(p.shadow, p.shadow_raised_alpha),
            shadow_popover: hex_a(p.shadow, p.shadow_popover_alpha),
            palette,
        }
    }

    pub fn light() -> Self {
        Self::from_palette(Palette::LIGHT, false)
    }

    pub fn dark() -> Self {
        Self::from_palette(Palette::DARK, true)
    }

    /// The theme the OS appearance asks for (System mode).
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        Self::for_mode(AppearanceMode::System, appearance)
    }

    /// The theme for a chosen mode, given the current OS appearance.
    pub fn for_mode(mode: AppearanceMode, system: WindowAppearance) -> Self {
        let mut theme = if resolve_dark(mode, system) {
            Self::dark()
        } else {
            Self::light()
        };
        theme.mode = mode;
        theme
    }

    /// Re-theme to `mode`, keeping the font chosen at startup.
    pub fn with_mode(&self, mode: AppearanceMode, system: WindowAppearance) -> Self {
        Theme {
            font_family: self.font_family.clone(),
            ..Self::for_mode(mode, system)
        }
    }

    /// The OS appearance changed: the new theme, or `None` when nothing
    /// changes (an explicit Light/Dark choice ignores the OS).
    pub fn after_system_change(&self, system: WindowAppearance) -> Option<Self> {
        (self.mode == AppearanceMode::System && resolve_dark(self.mode, system) != self.dark)
            .then(|| self.with_mode(self.mode, system))
    }

    /// `(text, background)` of a status tone.
    pub fn tone(&self, tone: Tone) -> (Hsla, Hsla) {
        let p = &self.palette;
        let pair = |(t, b): (u32, u32)| (hex(t), hex(b));
        match tone {
            Tone::Neutral => pair(p.neutral),
            Tone::Success => pair(p.success),
            Tone::Warning => pair(p.warning),
            Tone::Error => pair(p.error),
            Tone::Info => pair(p.info),
            Tone::Accent => (self.text_accent, self.accent_muted),
        }
    }

    /// Dot colour of a status tone (stronger than the text pair).
    pub fn dot(&self, tone: Tone) -> Hsla {
        let p = &self.palette;
        match tone {
            Tone::Neutral => self.icon_secondary,
            Tone::Success => hex(p.dot_success),
            Tone::Accent => self.accent,
            Tone::Warning => hex(p.dot_warning),
            Tone::Error => self.recording,
            Tone::Info => hex(p.dot_info),
        }
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// Switch every window to `mode` (System re-resolves from the current OS
/// appearance). Keeps the font chosen at startup.
///
/// Where native chrome follows the choice (macOS), the native appearance is
/// changed first, from a main-thread task outside this update: AppKit can
/// call GPUI's window appearance handler synchronously, which borrows the app
/// again. The palette is resolved afterwards, so System reads the inherited
/// OS appearance rather than the previously forced one. Tasks run in order,
/// so the last choice wins.
pub fn apply_mode(mode: AppearanceMode, cx: &mut App) {
    if crate::native_appearance::SUPPORTED {
        cx.spawn(async move |cx| {
            crate::native_appearance::set(mode);
            let _ = cx.update(|cx| resolve_mode(mode, cx));
        })
        .detach();
    } else {
        resolve_mode(mode, cx);
    }
}

fn resolve_mode(mode: AppearanceMode, cx: &mut App) {
    let next = theme(cx).with_mode(mode, cx.window_appearance());
    cx.set_global(next);
    cx.refresh_windows();
}

/// Elevation: shadows for controls, raised segments/knobs and popovers.
pub mod elevation {
    use super::*;

    /// A faint 1 px drop under secondary buttons and inputs.
    pub fn control(t: &Theme) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: t.shadow_control,
            offset: point(px(0.), px(1.)),
            blur_radius: px(1.),
            spread_radius: px(0.),
        }]
    }

    /// A selected segment or a switch knob above its track.
    pub fn raised(t: &Theme) -> Vec<BoxShadow> {
        vec![
            BoxShadow {
                color: t.shadow_raised,
                offset: point(px(0.), px(1.)),
                blur_radius: px(3.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: t.shadow_control,
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(0.5),
            },
        ]
    }

    /// Menus, dialogs, toasts and tooltips.
    pub fn popover(t: &Theme) -> Vec<BoxShadow> {
        vec![
            BoxShadow {
                color: t.shadow_popover,
                offset: point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: t.shadow_control,
                offset: point(px(0.), px(1.)),
                blur_radius: px(3.),
                spread_radius: px(0.),
            },
        ]
    }
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
        px(30.)
    }
    pub fn control_lg() -> Pixels {
        px(36.)
    }
    /// Sidebar navigation row.
    pub fn nav_row() -> Pixels {
        px(30.)
    }
    /// Minimum height of a row inside a group.
    pub fn row() -> Pixels {
        px(44.)
    }
    pub fn radius_inner() -> Pixels {
        px(6.)
    }
    pub fn radius_element() -> Pixels {
        px(8.)
    }
    /// Inset grouped lists.
    pub fn radius_group() -> Pixels {
        px(10.)
    }
    /// Dialogs and the one-per-page card.
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
    pub const SIDEBAR: f32 = 240.;
    pub const COLUMN: f32 = 760.;
    pub const LIST_PANE: f32 = 340.;
    pub const READING: f32 = 640.;
}

/// Type scale (base 13, macOS-like) as `(size, line height)`. Ranks come
/// from weight and colour more than from size.
pub mod text {
    use super::*;
    pub fn title() -> (Pixels, Pixels) {
        (px(22.), px(28.))
    }
    pub fn heading() -> (Pixels, Pixels) {
        (px(15.), px(20.))
    }
    pub fn body() -> (Pixels, Pixels) {
        (px(13.), px(18.))
    }
    pub fn reading() -> (Pixels, Pixels) {
        (px(15.), px(22.))
    }
    pub fn supporting() -> (Pixels, Pixels) {
        (px(12.), px(16.))
    }
    pub fn mono() -> (Pixels, Pixels) {
        (px(12.), px(18.))
    }
    pub fn stat() -> (Pixels, Pixels) {
        (px(22.), px(28.))
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

    /// Every text/background and indicator/background pair the redesign
    /// introduces: text >= 4.5:1, non-text indicators >= 3:1.
    #[test]
    fn new_token_pairs_meet_contrast_in_both_palettes() {
        for p in [Palette::LIGHT, Palette::DARK] {
            let text = |fg: u32, bg: u32, what: &str| {
                let r = contrast(fg, bg);
                assert!(r >= 4.5, "{what}: {fg:06x} on {bg:06x} is {r:.2}");
            };
            let mark = |fg: u32, bg: u32, what: &str| {
                let r = contrast(fg, bg);
                assert!(r >= 3.0, "{what}: {fg:06x} on {bg:06x} is {r:.2}");
            };
            // Popovers, controls, raised segments and the sidebar selection.
            for bg in [p.bg_popover, p.bg_control] {
                text(p.text_primary, bg, "primary on popover/control");
                text(p.text_secondary, bg, "secondary on popover/control");
            }
            text(p.text_accent, p.bg_popover, "toast action on popover");
            text(p.text_accent, p.bg_card, "accent label in a group");
            text(p.text_primary, p.bg_raised, "selected segment");
            // A disabled selected segment: secondary text on the control
            // fill (ui::segment), readable and stronger than the disabled
            // unselected labels on the track so the selection still reads.
            text(p.text_secondary, p.bg_control, "disabled selected segment");
            assert!(
                contrast(p.text_secondary, p.bg_control) > contrast(p.text_disabled, p.bg_muted),
                "disabled selected segment must out-read disabled unselected ones"
            );
            text(p.text_primary, p.bg_muted, "keycap / selected tab");
            text(p.text_primary, p.bg_sidebar_selected, "selected nav label");
            // A selected list row (Agents): primary text over the
            // `selected` overlay, composited on the page backgrounds a
            // list pane sits on.
            let over = |base: u32, top: u32, alpha: f32| -> u32 {
                let ch = |c: u32, s: u32| {
                    let (b, t) = (((c >> s) & 0xFF) as f32, ((top >> s) & 0xFF) as f32);
                    ((b + (t - b) * alpha).round() as u32) << s
                };
                ch(base, 16) | ch(base, 8) | ch(base, 0)
            };
            for bg in [p.bg_body, p.bg_surface, p.bg_card] {
                let row = over(bg, p.overlay_base, p.selected_alpha);
                text(p.text_primary, row, "selected list row");
            }
            mark(p.icon_accent, p.bg_sidebar_selected, "selected nav icon");
            text(p.text_accent, p.accent_muted, "accent token / badge");
            // Banner messages are primary text on the tinted fill.
            for (_, bg) in [p.success, p.warning, p.error, p.info, p.neutral] {
                text(p.text_primary, bg, "banner message");
            }
            // Neutral tokens and unselected segment labels on the track.
            text(p.neutral.0, p.neutral.1, "neutral token");
            text(p.neutral.0, p.bg_muted, "unselected segment");
            // The primary button's hover keeps a legible label.
            text(p.text_on_accent, p.accent_hover, "primary hover");
            // ... and the hover is a perceptible step from the rest fill.
            let step = contrast(p.accent, p.accent_hover);
            assert!(step >= 1.15, "primary hover step is only {step:.3}");
            // Control boundaries (inputs, off switch track) and the knob.
            for bg in [p.bg_control, p.bg_popover] {
                mark(p.border_control, bg, "control boundary");
            }
            mark(p.accent, p.bg_card, "on switch / selection in a group");
            // Progress fill on its muted track (ui::progress, both variants).
            mark(p.icon_accent, p.bg_muted, "progress fill on track");
            // The selected-option check in the select popover.
            mark(p.icon_accent, p.bg_popover, "select check on popover");
            // The neutral focus edge, on every surface a control sits on or
            // is filled with (ui::ring / ui::ring_over).
            for bg in [
                p.bg_body,
                p.bg_surface,
                p.bg_card,
                p.bg_popover,
                p.bg_muted,
                p.bg_control,
                p.bg_raised,
                p.bg_sidebar_selected,
            ] {
                mark(p.focus, bg, "focus edge");
            }
            // Status dots, the recording dot and secondary icons.
            for bg in [p.bg_body, p.bg_surface, p.bg_card] {
                for (dot, what) in [
                    (p.dot_success, "success dot"),
                    (p.dot_warning, "warning dot"),
                    (p.dot_info, "info dot"),
                    (p.recording, "recording / error dot"),
                    (p.icon_secondary, "secondary icon"),
                    (p.icon_accent, "accent icon"),
                ] {
                    mark(dot, bg, what);
                }
            }
        }
    }

    /// The knob reads against the on track; the off track has a boundary.
    #[test]
    fn switch_states_are_distinguishable() {
        for p in [Palette::LIGHT, Palette::DARK] {
            assert!(contrast(p.knob, p.accent) >= 3.0, "knob on accent track");
            assert!(contrast(p.border_control, p.bg_card) >= 3.0);
        }
    }

    #[test]
    fn explicit_modes_ignore_the_os_and_system_follows_it() {
        use WindowAppearance::*;
        for system in [Light, VibrantLight, Dark, VibrantDark] {
            assert!(!resolve_dark(AppearanceMode::Light, system), "{system:?}");
            assert!(resolve_dark(AppearanceMode::Dark, system), "{system:?}");
        }
        assert!(!resolve_dark(AppearanceMode::System, Light));
        assert!(!resolve_dark(AppearanceMode::System, VibrantLight));
        assert!(resolve_dark(AppearanceMode::System, Dark));
        assert!(resolve_dark(AppearanceMode::System, VibrantDark));
    }

    #[test]
    fn re_theming_keeps_the_font_and_records_the_mode() {
        let mut start = Theme::for_mode(AppearanceMode::System, WindowAppearance::Light);
        start.font_family = "Test Sans".into();
        assert!(!start.dark);

        let dark = start.with_mode(AppearanceMode::Dark, WindowAppearance::Light);
        assert!(dark.dark);
        assert_eq!(dark.mode, AppearanceMode::Dark);
        assert_eq!(dark.font_family, start.font_family);
        assert_eq!(dark.bg_surface, Theme::dark().bg_surface);

        let back = dark.with_mode(AppearanceMode::System, WindowAppearance::Light);
        assert!(!back.dark, "System re-resolves from the OS at once");
        assert_eq!(back.mode, AppearanceMode::System);
        assert_eq!(back.font_family, start.font_family);
    }

    #[test]
    fn os_changes_re_theme_only_in_system_mode() {
        let mut light = Theme::for_mode(AppearanceMode::Light, WindowAppearance::Light);
        light.font_family = "Test Sans".into();
        assert!(light.after_system_change(WindowAppearance::Dark).is_none());
        let dark = light.with_mode(AppearanceMode::Dark, WindowAppearance::Dark);
        assert!(dark.after_system_change(WindowAppearance::Light).is_none());

        let system = light.with_mode(AppearanceMode::System, WindowAppearance::Light);
        assert!(system
            .after_system_change(WindowAppearance::VibrantLight)
            .is_none());
        let next = system
            .after_system_change(WindowAppearance::VibrantDark)
            .expect("System follows the OS");
        assert!(next.dark);
        assert_eq!(next.mode, AppearanceMode::System);
        assert_eq!(next.font_family, light.font_family);
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
