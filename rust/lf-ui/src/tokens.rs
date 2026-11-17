//! Shared presentation tokens for the native windows (port of
//! `localflow/ui_tokens.py`). Constants, strings and math only.
//!
//! Python's `round()` rounds half to even; every `round` here uses
//! [`f64::round_ties_even`] so results agree on exact halves.

use std::fmt;

/// Font weight used by the type scale and by injectable text measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular,
    Semibold,
    Bold,
}

impl Weight {
    /// The Python token name (`"regular"`, `"semibold"`, `"bold"`).
    pub fn name(self) -> &'static str {
        match self {
            Weight::Regular => "regular",
            Weight::Semibold => "semibold",
            Weight::Bold => "bold",
        }
    }
}

/// A semantic colour palette: token name -> `#RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub paper: &'static str,
    pub surface: &'static str,
    pub ink: &'static str,
    pub sidebar: &'static str,
    pub sidebar_text: &'static str,
    pub shadow: &'static str,
    pub muted: &'static str,
    pub line: &'static str,
    pub cobalt: &'static str,
    pub sun: &'static str,
    pub red: &'static str,
    pub success: &'static str,
}

/// Token names in the Python dict's insertion order.
pub const TOKEN_NAMES: [&str; 12] = [
    "paper",
    "surface",
    "ink",
    "sidebar",
    "sidebar_text",
    "shadow",
    "muted",
    "line",
    "cobalt",
    "sun",
    "red",
    "success",
];

impl Palette {
    /// The colour of a token, `None` for an unknown token name.
    pub fn get(&self, token: &str) -> Option<&'static str> {
        Some(match token {
            "paper" => self.paper,
            "surface" => self.surface,
            "ink" => self.ink,
            "sidebar" => self.sidebar,
            "sidebar_text" => self.sidebar_text,
            "shadow" => self.shadow,
            "muted" => self.muted,
            "line" => self.line,
            "cobalt" => self.cobalt,
            "sun" => self.sun,
            "red" => self.red,
            "success" => self.success,
            _ => return None,
        })
    }

    /// `(token, colour)` pairs in the Python dict's order.
    pub fn entries(&self) -> Vec<(&'static str, &'static str)> {
        TOKEN_NAMES
            .iter()
            .map(|name| (*name, self.get(name).unwrap_or("")))
            .collect()
    }
}

pub const LIGHT: Palette = Palette {
    paper: "#FBF6EC",
    surface: "#FFFFFF",
    ink: "#141414",
    sidebar: "#141414",
    sidebar_text: "#FBF6EC",
    shadow: "#141414",
    muted: "#5C5C5C",
    line: "#E6DFD0",
    cobalt: "#2346D8",
    sun: "#FFD23F",
    red: "#E5484D",
    success: "#1F9D55",
};

pub const DARK: Palette = Palette {
    paper: "#15140F",
    surface: "#1E1C17",
    ink: "#F5F1E8",
    sidebar: "#0B0A08",
    sidebar_text: "#FBF6EC",
    shadow: "#000000",
    muted: "#A8A296",
    line: "#34312A",
    cobalt: "#5B7BFF",
    sun: "#FFD23F",
    red: "#FF6369",
    success: "#3DD68C",
};

pub const SUN_TEXT: &str = "#141414";
pub const TOKENS: Palette = LIGHT;
/// `PALETTES` in Python: theme name -> palette.
pub const PALETTES: [(&str, Palette); 2] = [("light", LIGHT), ("dark", DARK)];

/// A type-scale entry: `(size, weight)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeStyle {
    pub size: f64,
    pub weight: Weight,
}

pub const TYPE_DISPLAY: TypeStyle = TypeStyle {
    size: 28.0,
    weight: Weight::Bold,
};
pub const TYPE_TITLE: TypeStyle = TypeStyle {
    size: 20.0,
    weight: Weight::Semibold,
};
pub const TYPE_HEADING: TypeStyle = TypeStyle {
    size: 15.0,
    weight: Weight::Semibold,
};
pub const TYPE_BODY: TypeStyle = TypeStyle {
    size: 13.0,
    weight: Weight::Regular,
};
pub const TYPE_CAPTION: TypeStyle = TypeStyle {
    size: 11.0,
    weight: Weight::Semibold,
};

/// `TYPE[name]` from Python; `None` for an unknown style.
pub fn type_style(name: &str) -> Option<TypeStyle> {
    Some(match name {
        "display" => TYPE_DISPLAY,
        "title" => TYPE_TITLE,
        "heading" => TYPE_HEADING,
        "body" => TYPE_BODY,
        "caption" => TYPE_CAPTION,
        _ => return None,
    })
}

pub const SPACING: [i32; 7] = [4, 8, 12, 16, 24, 32, 48];
pub const RADIUS_CARD: i32 = 12;
pub const RADIUS_BUTTON: i32 = 10;
pub const RADIUS_PILL: i32 = 999;
pub const PAGE_MARGIN: i32 = 32;
pub const CARD_PADDING: i32 = 16;
pub const CARD_GAP: i32 = 12;
pub const SHADOW_OFFSET: i32 = 3;

// Setup checkup cards (AppKit).
pub const CARD_BORDER_WIDTH: i32 = 2;
pub const LINE_HEIGHT_DISPLAY: i32 = 38;
pub const LINE_HEIGHT_HEADING: i32 = 22;
pub const LINE_HEIGHT_BODY: i32 = 18;
pub const LINE_HEIGHT_CAPTION: i32 = 16;

/// `LINE_HEIGHT[name]` from Python.
pub fn line_height(name: &str) -> Option<i32> {
    Some(match name {
        "display" => LINE_HEIGHT_DISPLAY,
        "heading" => LINE_HEIGHT_HEADING,
        "body" => LINE_HEIGHT_BODY,
        "caption" => LINE_HEIGHT_CAPTION,
        _ => return None,
    })
}

/// Page titles share the display style; the label is exactly one display line
/// tall so the Hub header offset clears it.
pub const PAGE_TITLE_HEIGHT: i32 = LINE_HEIGHT_DISPLAY;
pub const CHECK_CARD_HEIGHT: i32 = 2 * CARD_PADDING + LINE_HEIGHT_HEADING + LINE_HEIGHT_BODY;
pub const PILL_HEIGHT: i32 = 24;
pub const PILL_PADDING: i32 = SPACING[2];
pub const PILL_BORDER_WIDTH: i32 = 1;
pub const PILL_TINT: f64 = 0.18;
pub const ACTION_HEIGHT: i32 = 28;
pub const ACTION_PADDING: i32 = SPACING[2];
pub const CONTROL_GAP: i32 = SPACING[1];
pub const CALLOUT_HEIGHT: i32 = 2 * SPACING[2] + LINE_HEIGHT_BODY;
/// Smallest content size the setup window supports without overlapping controls.
pub const WINDOW_MIN_WIDTH: i32 = 800;
pub const WINDOW_MIN_HEIGHT: i32 = 700;
// Hub shell (AppKit).
pub const HUB_MIN_WIDTH: i32 = 860;
pub const HUB_MIN_HEIGHT: i32 = 600;
pub const HUB_DEFAULT_WIDTH: i32 = 1040;
pub const HUB_DEFAULT_HEIGHT: i32 = 700;
pub const HUB_SIDEBAR_WIDTH: i32 = 200;
pub const HUB_CONTENT_X: i32 = HUB_SIDEBAR_WIDTH + PAGE_MARGIN;
pub const HUB_CONTENT_TOP: i32 = 24;
pub const HUB_CONTENT_GUTTER: i32 = 16;
pub const HUB_PAGE_BOTTOM: i32 = 24;
/// `PAGE_TITLE_HEIGHT` plus a small gap.
pub const HUB_HEADER_OFFSET: i32 = 48;
// Ask (AppKit).
pub const ASK_TOOLBAR_HEIGHT: i32 = 28;
pub const ASK_SECTION_GAP: i32 = 12;
pub const ASK_COMPOSER_HEIGHT: i32 = 32;
pub const ASK_CONVERSATION_MIN_HEIGHT: i32 = 120;
pub const ASK_CONTROL_FONT: i32 = 12;
pub const ASK_BUBBLE_PADDING: i32 = 12;
pub const ASK_BUBBLE_MAX_FRACTION: f64 = 0.8;
pub const ASK_TURN_GAP: i32 = 12;
pub const ASK_CHIP_HEIGHT: i32 = 26;
pub const ASK_CHIP_PADDING: i32 = 12;
pub const ASK_CHIP_GAP: i32 = 8;
pub const ASK_ANSWER_LINE: i32 = 18;
// Models & keys (AppKit).
pub const MODELS_ROW_HEIGHT: i32 = 48;
pub const MODELS_FIELD_HEIGHT: i32 = 28;
pub const MODELS_LABEL_HEIGHT: i32 = 16;
pub const MODELS_ROW_GAP: i32 = 8;

/// Status pill: `(label, tint token, text token)`. Text on the sun tint is ink
/// so it stays readable in both appearances.
pub const CHECK_STATUS: [(&str, (&str, &str, &str)); 4] = [
    ("ready", ("Ready", "success", "success")),
    ("needs", ("Needs you", "red", "red")),
    ("unknown", ("Unknown", "sun", "ink")),
    ("checking", ("Checking…", "sun", "ink")),
];

/// `CHECK_STATUS[key]` from Python.
pub fn check_status(key: &str) -> Option<(&'static str, &'static str, &'static str)> {
    CHECK_STATUS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
}

/// Malformed `#RRGGBB` colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorError(pub String);

impl fmt::Display for ColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid hex colour: {:?}", self.0)
    }
}

impl std::error::Error for ColorError {}

/// The semantic palette for a native window appearance (`"dark"` in any case,
/// everything else is light).
pub fn tokens_for(theme: &str) -> Palette {
    if theme.to_lowercase() == "dark" {
        DARK
    } else {
        LIGHT
    }
}

/// `(r, g, b)` channels of `#RRGGBB` (the `#` is optional, like `lstrip("#")`).
fn channels(hex: &str) -> Result<[u8; 3], ColorError> {
    let value = hex.trim_start_matches('#');
    let bad = || ColorError(hex.to_string());
    if value.len() < 6 || !value.is_char_boundary(6) {
        return Err(bad());
    }
    let mut out = [0u8; 3];
    for (slot, index) in out.iter_mut().zip([0usize, 2, 4]) {
        let part = &value[index..index + 2];
        if !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        *slot = u8::from_str_radix(part, 16).map_err(|_| bad())?;
    }
    Ok(out)
}

/// `surface` blended towards `token` by `amount` (default 0.12 in Python).
/// `None` when `token` is not in the palette.
pub fn surface_tint(token: &str, palette: &Palette, amount: f64) -> Option<String> {
    let base = channels(palette.surface).ok()?;
    let accent = channels(palette.get(token)?).ok()?;
    let mut out = String::from("#");
    for (b, a) in base.iter().zip(accent.iter()) {
        let value = (f64::from(*b) * (1.0 - amount) + f64::from(*a) * amount).round_ties_even();
        out.push_str(&format!("{:02X}", value as i64));
    }
    Some(out)
}

/// [`surface_tint`] with the default amount `0.12`.
pub fn surface_tint_default(token: &str, palette: &Palette) -> Option<String> {
    surface_tint(token, palette, 0.12)
}

// Native text inputs (AppKit): one outer height for fields and pickers.
pub const INPUT_HEIGHT: i32 = 32;
pub const INPUT_BORDER_WIDTH: f64 = 1.5;
pub const INPUT_PADDING_X: i32 = 8;

/// `(x, y, w, h)` rectangle.
pub type Rect = (f64, f64, f64, f64);

/// `(x, y, w, h)` of an unbezeled control inside its shared input box.
///
/// Every control is centred vertically at its own cell height, so a text
/// field and a picker in boxes of the same height share the same text
/// baseline band and top edge, whatever their intrinsic heights are.
pub fn input_inner_frame(width: f64, height: f64, control_height: f64) -> Rect {
    let inner_h = control_height.min(height - 2.0 * INPUT_BORDER_WIDTH);
    let pad = f64::from(INPUT_PADDING_X);
    (
        pad,
        ((height - inner_h) / 2.0).round_ties_even(),
        (width - 2.0 * pad).max(0.0),
        inner_h,
    )
}

/// Anchor corner of the halftone (`"bottom-left"` by default in Python).
pub const HALFTONE_DEFAULT_ANCHOR: &str = "bottom-left";

/// `(x, y, diameter)` of the sun halftone, every dot inside width x height.
///
/// Dots fade with their distance from `anchor` (in the view's own, unflipped
/// coordinates) and stop at the diagonal. Only whole dots that fit are
/// returned, so drawing never leaves the bounds. Python defaults:
/// `step=14, max_radius=4, pad=8, anchor="bottom-left"`.
pub fn halftone_dots(
    width: f64,
    height: f64,
    step: i32,
    max_radius: i32,
    pad: i32,
    anchor: &str,
) -> Vec<(f64, f64, f64)> {
    let span = 2 * max_radius;
    let (stepf, padf, spanf) = (f64::from(step), f64::from(pad), f64::from(span));
    let count = |extent: f64| -> i32 {
        if extent >= 2.0 * padf + spanf {
            ((extent - 2.0 * padf - spanf) / stepf).floor() as i32 + 1
        } else {
            0
        }
    };
    let cols = count(width);
    let rows = count(height);
    let limit = cols.max(rows);
    let mut dots = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if row + col >= limit {
                continue;
            }
            let radius = 1.max(max_radius - (row + col) / 5);
            let mut x = padf + f64::from(col * step);
            let mut y = padf + f64::from(row * step);
            if anchor.ends_with("right") {
                x = width - padf - spanf - f64::from(col * step);
            }
            if anchor.starts_with("top") {
                y = height - padf - spanf - f64::from(row * step);
            }
            // Smaller dots sit centred in their cell.
            let offset = f64::from(max_radius - radius);
            dots.push((x + offset, y + offset, f64::from(2 * radius)));
        }
    }
    dots
}

/// [`halftone_dots`] with the Python defaults.
pub fn halftone_dots_default(width: f64, height: f64) -> Vec<(f64, f64, f64)> {
    halftone_dots(width, height, 14, 4, 8, HALFTONE_DEFAULT_ANCHOR)
}

// Settings switch (mac_style.CobaltSwitch).
pub const SWITCH_TRACK_W: i32 = 40;
pub const SWITCH_TRACK_H: i32 = 22;
pub const SWITCH_KNOB_INSET: i32 = 3;
pub const SWITCH_STATE_W: i32 = 28;
pub const SWITCH_STATE_GAP: i32 = 8;
pub const SWITCH_TEXT_H: i32 = 16;
pub const SWITCH_BORDER_WIDTH: i32 = 2;
pub const SWITCH_WIDTH: i32 = SWITCH_STATE_W + SWITCH_STATE_GAP + SWITCH_TRACK_W;
pub const SWITCH_DISABLED_ALPHA: f64 = 0.45;

/// Frames of a switch drawn in a box (`switch_parts` in Python).
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchParts {
    pub track: Rect,
    pub knob: Rect,
    pub radius: f64,
    pub text: Rect,
    pub label: &'static str,
}

/// Frames of a switch drawn in a `width` x `height` box.
///
/// The track is right-aligned and vertically centred; the knob sits inside
/// it, right when on and left when off; the state word fills the room left of
/// the track.
pub fn switch_parts(width: f64, height: f64, on: bool) -> SwitchParts {
    let inset = f64::from(SWITCH_KNOB_INSET);
    let track_w = f64::from(SWITCH_TRACK_W).min(width);
    let track_h = f64::from(SWITCH_TRACK_H).min(height);
    let track_x = width - track_w;
    let track_y = (height - track_h) / 2.0;
    let knob = (track_h - 2.0 * inset).max(0.0);
    let knob_x = if on {
        track_x + track_w - inset - knob
    } else {
        track_x + inset
    };
    let text_h = f64::from(SWITCH_TEXT_H).min(height);
    SwitchParts {
        track: (track_x, track_y, track_w, track_h),
        knob: (knob_x, track_y + inset, knob, knob),
        radius: track_h / 2.0,
        text: (
            0.0,
            (height - text_h) / 2.0,
            (track_x - f64::from(SWITCH_STATE_GAP)).max(0.0),
            text_h,
        ),
        label: if on { "On" } else { "Off" },
    }
}

/// WCAG relative luminance of `#RRGGBB`.
pub fn relative_luminance(hex_color: &str) -> Result<f64, ColorError> {
    let [r, g, b] = channels(hex_color)?;
    let lin = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Ok(0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b))
}

/// WCAG contrast ratio between two colours.
pub fn contrast_ratio(first: &str, second: &str) -> Result<f64, ColorError> {
    let (a, b) = (relative_luminance(first)?, relative_luminance(second)?);
    let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
    Ok((lighter + 0.05) / (darker + 0.05))
}

/// Hex colours of a switch from the palette's own tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchColors {
    pub track: &'static str,
    pub knob: &'static str,
    pub outline: &'static str,
    pub text: &'static str,
    pub alpha: f64,
}

/// Hex colours of a switch from the palette's own tokens.
///
/// On: a cobalt track with the paper/ink knob that contrasts most with it
/// (paper wins a tie, like Python's `max`). Off: a paper track with an ink
/// knob. Both keep the house ink outline, and the state word is cobalt (on) or
/// muted (off), so On/Off never relies on colour alone. Disabled only lowers
/// the alpha.
pub fn switch_colors(
    palette: &Palette,
    on: bool,
    enabled: bool,
) -> Result<SwitchColors, ColorError> {
    let (track, knob, text) = if on {
        let track = palette.cobalt;
        let paper = contrast_ratio(palette.paper, track)?;
        let ink = contrast_ratio(palette.ink, track)?;
        let knob = if ink > paper {
            palette.ink
        } else {
            palette.paper
        };
        (track, knob, palette.cobalt)
    } else {
        (palette.paper, palette.ink, palette.muted)
    };
    Ok(SwitchColors {
        track,
        knob,
        outline: palette.ink,
        text,
        alpha: if enabled { 1.0 } else { SWITCH_DISABLED_ALPHA },
    })
}

/// The first available preferred Tk font family (case-insensitive), else
/// `"DejaVu Sans"`. Later duplicates of a casefolded name win, as in Python's
/// dict comprehension.
pub fn resolve_tk_font<S: AsRef<str>>(available_families: &[S]) -> String {
    let mut families: Vec<(String, &str)> = Vec::new();
    for family in available_families {
        let key = caseless::default_case_fold_str(family.as_ref());
        match families.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = family.as_ref(),
            None => families.push((key, family.as_ref())),
        }
    }
    for preferred in ["SF Pro Text", "Helvetica Neue", "DejaVu Sans"] {
        let key = caseless::default_case_fold_str(preferred);
        if let Some((_, found)) = families.iter().find(|(k, _)| *k == key) {
            return (*found).to_string();
        }
    }
    "DejaVu Sans".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_palettes_match_the_design_spec() {
        let light: Vec<_> = LIGHT.entries();
        assert_eq!(
            light,
            vec![
                ("paper", "#FBF6EC"),
                ("surface", "#FFFFFF"),
                ("ink", "#141414"),
                ("sidebar", "#141414"),
                ("sidebar_text", "#FBF6EC"),
                ("shadow", "#141414"),
                ("muted", "#5C5C5C"),
                ("line", "#E6DFD0"),
                ("cobalt", "#2346D8"),
                ("sun", "#FFD23F"),
                ("red", "#E5484D"),
                ("success", "#1F9D55"),
            ]
        );
        assert_eq!(
            DARK.entries(),
            vec![
                ("paper", "#15140F"),
                ("surface", "#1E1C17"),
                ("ink", "#F5F1E8"),
                ("sidebar", "#0B0A08"),
                ("sidebar_text", "#FBF6EC"),
                ("shadow", "#000000"),
                ("muted", "#A8A296"),
                ("line", "#34312A"),
                ("cobalt", "#5B7BFF"),
                ("sun", "#FFD23F"),
                ("red", "#FF6369"),
                ("success", "#3DD68C"),
            ]
        );
        assert_eq!(TOKENS, LIGHT);
        assert_eq!(PALETTES[0], ("light", LIGHT));
        assert_eq!(PALETTES[1], ("dark", DARK));
        assert_eq!(LIGHT.get("nope"), None);
    }

    #[test]
    fn theme_and_tk_font_resolution() {
        assert_eq!(tokens_for("dark"), DARK);
        assert_eq!(tokens_for("LIGHT"), LIGHT);
        assert_eq!(tokens_for("anything"), LIGHT);
        assert_eq!(SUN_TEXT, "#141414");
        assert_eq!(
            surface_tint_default("success", &LIGHT).as_deref(),
            Some("#E4F3EB")
        );
        assert_eq!(
            surface_tint_default("success", &DARK).as_deref(),
            Some("#223225")
        );
        assert_eq!(surface_tint_default("missing", &DARK), None);
        assert_eq!(
            resolve_tk_font(&["Helvetica", "DejaVu Sans", "SF Pro Text"]),
            "SF Pro Text"
        );
        assert_eq!(
            resolve_tk_font(&["Helvetica Neue", "DejaVu Sans"]),
            "Helvetica Neue"
        );
        assert_eq!(resolve_tk_font(&["Helvetica"]), "DejaVu Sans");
        assert_eq!(resolve_tk_font::<&str>(&[]), "DejaVu Sans");
        assert_eq!(resolve_tk_font(&["sf pro text"]), "sf pro text");
    }

    #[test]
    fn page_titles_share_one_display_style_that_clears_the_header_offset() {
        assert_eq!(
            TYPE_DISPLAY,
            TypeStyle {
                size: 28.0,
                weight: Weight::Bold
            }
        );
        assert_eq!(PAGE_TITLE_HEIGHT, LINE_HEIGHT_DISPLAY);
        assert_eq!(PAGE_TITLE_HEIGHT, 38);
        const { assert!(PAGE_TITLE_HEIGHT <= HUB_HEADER_OFFSET) };
        assert_eq!(CHECK_CARD_HEIGHT, 2 * 16 + 22 + 18);
        assert_eq!(CALLOUT_HEIGHT, 2 * 12 + 18);
        assert_eq!(HUB_CONTENT_X, 232);
        assert_eq!(SWITCH_WIDTH, 76);
        assert_eq!(check_status("checking"), Some(("Checking…", "sun", "ink")));
        assert_eq!(type_style("caption").unwrap().weight.name(), "semibold");
        assert_eq!(line_height("body"), Some(18));
    }

    #[test]
    fn input_inner_frame_centres_controls() {
        assert_eq!(
            input_inner_frame(200.0, 32.0, 22.0),
            (8.0, 5.0, 184.0, 22.0)
        );
        // Clamped to the box minus borders; narrow widths never go negative.
        assert_eq!(input_inner_frame(10.0, 20.0, 40.0), (8.0, 2.0, 0.0, 17.0));
        // Python round(): 2.5 -> 2 (half to even).
        assert_eq!(input_inner_frame(100.0, 27.0, 22.0).1, 2.0);
    }

    #[test]
    fn halftone_dots_stay_inside_the_bounds() {
        assert!(halftone_dots_default(10.0, 10.0).is_empty());
        for anchor in ["bottom-left", "top-left", "bottom-right", "top-right"] {
            let dots = halftone_dots(120.0, 90.0, 14, 4, 8, anchor);
            assert!(!dots.is_empty());
            for (x, y, d) in dots {
                assert!(
                    x >= 0.0 && y >= 0.0 && x + d <= 120.0 && y + d <= 90.0,
                    "{anchor}"
                );
            }
        }
        let dots = halftone_dots_default(60.0, 60.0);
        // cols = rows = 3 -> limit 3: the diagonal (row + col >= 3) is dropped.
        assert_eq!(dots.len(), 6);
        assert_eq!(dots[0], (8.0, 8.0, 8.0));
    }

    #[test]
    fn switch_geometry() {
        let on = switch_parts(100.0, 30.0, true);
        assert_eq!(on.track, (60.0, 4.0, 40.0, 22.0));
        assert_eq!(on.knob, (60.0 + 40.0 - 3.0 - 16.0, 7.0, 16.0, 16.0));
        assert_eq!(on.label, "On");
        assert_eq!(on.text, (0.0, 7.0, 52.0, 16.0));
        let off = switch_parts(100.0, 30.0, false);
        assert_eq!(off.knob.0, 63.0);
        assert_eq!(off.label, "Off");
        let tiny = switch_parts(10.0, 10.0, true);
        assert_eq!(tiny.track, (0.0, 0.0, 10.0, 10.0));
        assert_eq!(tiny.knob.2, 4.0);
        assert_eq!(tiny.text.2, 0.0);
    }

    #[test]
    fn switch_colours_use_palette_tokens() {
        let on = switch_colors(&LIGHT, true, true).unwrap();
        assert_eq!(
            (on.track, on.text, on.alpha),
            (LIGHT.cobalt, LIGHT.cobalt, 1.0)
        );
        assert_eq!(on.knob, LIGHT.paper);
        assert_eq!(on.outline, LIGHT.ink);
        let off = switch_colors(&DARK, false, false).unwrap();
        assert_eq!(
            (off.track, off.knob, off.text),
            (DARK.paper, DARK.ink, DARK.muted)
        );
        assert_eq!(off.alpha, SWITCH_DISABLED_ALPHA);
    }

    #[test]
    fn contrast_math_and_errors() {
        assert!((contrast_ratio("#000000", "#FFFFFF").unwrap() - 21.0).abs() < 1e-9);
        assert!((relative_luminance("FFFFFF").unwrap() - 1.0).abs() < 1e-9);
        assert!(relative_luminance("#12").is_err());
        assert!(relative_luminance("#GG0000").is_err());
    }
}
