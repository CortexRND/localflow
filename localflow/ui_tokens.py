"""Shared presentation tokens for the native Tk and AppKit windows."""

LIGHT = {
    "paper": "#FBF6EC",
    "surface": "#FFFFFF",
    "ink": "#141414",
    "sidebar": "#141414",
    "sidebar_text": "#FBF6EC",
    "shadow": "#141414",
    "muted": "#5C5C5C",
    "line": "#E6DFD0",
    "cobalt": "#2346D8",
    "sun": "#FFD23F",
    "red": "#E5484D",
    "success": "#1F9D55",
}

DARK = {
    "paper": "#15140F",
    "surface": "#1E1C17",
    "ink": "#F5F1E8",
    "sidebar": "#0B0A08",
    "sidebar_text": "#FBF6EC",
    "shadow": "#000000",
    "muted": "#A8A296",
    "line": "#34312A",
    "cobalt": "#5B7BFF",
    "sun": "#FFD23F",
    "red": "#FF6369",
    "success": "#3DD68C",
}

SUN_TEXT = "#141414"
TOKENS = LIGHT
PALETTES = {"light": LIGHT, "dark": DARK}

TYPE = {
    "display": (28, "bold"),
    "title": (20, "semibold"),
    "heading": (15, "semibold"),
    "body": (13, "regular"),
    "caption": (11, "semibold"),
}
SPACING = (4, 8, 12, 16, 24, 32, 48)
RADIUS = {"card": 12, "button": 10, "pill": 999}
PAGE_MARGIN = 32
CARD_PADDING = 16
CARD_GAP = 12
SHADOW_OFFSET = 3

# Setup checkup cards (AppKit).
CARD_BORDER_WIDTH = 2
LINE_HEIGHT = {"display": 38, "heading": 22, "body": 18, "caption": 16}
# Page titles (setup steps and every Hub page) share the display style; the
# label is exactly one display line tall so the Hub header offset clears it.
PAGE_TITLE_HEIGHT = LINE_HEIGHT["display"]
CHECK_CARD_HEIGHT = 2 * CARD_PADDING + LINE_HEIGHT["heading"] + LINE_HEIGHT["body"]
PILL_HEIGHT = 24
PILL_PADDING = SPACING[2]
PILL_BORDER_WIDTH = 1
PILL_TINT = 0.18
ACTION_HEIGHT = 28
ACTION_PADDING = SPACING[2]
CONTROL_GAP = SPACING[1]
CALLOUT_HEIGHT = 2 * SPACING[2] + LINE_HEIGHT["body"]
# Smallest content size the setup window supports without overlapping controls.
WINDOW_MIN_WIDTH = 800
WINDOW_MIN_HEIGHT = 700
# Hub shell (AppKit). Smallest content size at which every page fits its width;
# taller pages scroll.
HUB_MIN_WIDTH = 860
HUB_MIN_HEIGHT = 600
HUB_DEFAULT_WIDTH = 1040
HUB_DEFAULT_HEIGHT = 700
HUB_SIDEBAR_WIDTH = 200
HUB_CONTENT_X = HUB_SIDEBAR_WIDTH + PAGE_MARGIN
HUB_CONTENT_TOP = 24
HUB_CONTENT_GUTTER = 16
HUB_PAGE_BOTTOM = 24
HUB_HEADER_OFFSET = 48  # PAGE_TITLE_HEIGHT plus a small gap
# Ask (AppKit): one compact controls row, a scrolling conversation, then the
# composer and the privacy note pinned to the bottom of the Hub viewport.
ASK_TOOLBAR_HEIGHT = 28
ASK_SECTION_GAP = 12
ASK_COMPOSER_HEIGHT = 32
ASK_CONVERSATION_MIN_HEIGHT = 120
ASK_CONTROL_FONT = 12
ASK_BUBBLE_PADDING = 12
ASK_BUBBLE_MAX_FRACTION = 0.8
ASK_TURN_GAP = 12
ASK_CHIP_HEIGHT = 26
ASK_CHIP_PADDING = 12
ASK_CHIP_GAP = 8
ASK_ANSWER_LINE = 18
# Models & keys (AppKit): compact provider list rows and the inline editor.
MODELS_ROW_HEIGHT = 48
MODELS_FIELD_HEIGHT = 28
MODELS_LABEL_HEIGHT = 16
MODELS_ROW_GAP = 8
# Status pill: (label, tint token, text token). Text on the sun tint is ink so
# it stays readable in both appearances.
CHECK_STATUS = {
    "ready": ("Ready", "success", "success"),
    "needs": ("Needs you", "red", "red"),
    "unknown": ("Unknown", "sun", "ink"),
    "checking": ("Checking…", "sun", "ink"),
}


def tokens_for(theme: str = "light") -> dict[str, str]:
    """Return the semantic palette for a native window appearance."""
    return DARK if theme.lower() == "dark" else LIGHT


def surface_tint(token: str, palette: dict[str, str], amount: float = 0.12) -> str:
    base = palette["surface"].lstrip("#")
    accent = palette[token].lstrip("#")
    channels = (
        round(
            int(base[index:index + 2], 16) * (1 - amount)
            + int(accent[index:index + 2], 16) * amount
        )
        for index in (0, 2, 4)
    )
    return "#" + "".join(f"{channel:02X}" for channel in channels)


# Native text inputs (AppKit): one outer height for fields and pickers so a
# title field and a template picker side by side line up exactly.
INPUT_HEIGHT = 32
INPUT_BORDER_WIDTH = 1.5
INPUT_PADDING_X = 8


def input_inner_frame(width: float, height: float, control_height: float):
    """(x, y, w, h) of an unbezeled control inside its shared input box.

    Every control is centred vertically at its own cell height, so a text
    field and a picker in boxes of the same height share the same text
    baseline band and top edge, whatever their intrinsic heights are.
    """
    inner_h = min(control_height, height - 2 * INPUT_BORDER_WIDTH)
    return (
        INPUT_PADDING_X, round((height - inner_h) / 2),
        max(width - 2 * INPUT_PADDING_X, 0), inner_h,
    )


def halftone_dots(
    width: float, height: float, *, step: int = 14, max_radius: int = 4,
    pad: int = 8, anchor: str = "bottom-left",
) -> list[tuple[float, float, float]]:
    """(x, y, diameter) of the sun halftone, every dot inside width x height.

    Dots fade with their distance from `anchor` (in the view's own, unflipped
    coordinates) and stop at the diagonal, as the original 12 x 12 pattern did.
    Only whole dots that fit are returned, so drawing never leaves the bounds.
    """
    span = 2 * max_radius
    cols = int((width - 2 * pad - span) // step) + 1 if width >= 2 * pad + span else 0
    rows = int((height - 2 * pad - span) // step) + 1 if height >= 2 * pad + span else 0
    limit = max(cols, rows)
    dots = []
    for row in range(rows):
        for col in range(cols):
            if row + col >= limit:
                continue
            radius = max(1, max_radius - (row + col) // 5)
            x = pad + col * step
            y = pad + row * step
            if anchor.endswith("right"):
                x = width - pad - span - col * step
            if anchor.startswith("top"):
                y = height - pad - span - row * step
            # Smaller dots sit centred in their cell.
            offset = max_radius - radius
            dots.append((x + offset, y + offset, 2 * radius))
    return dots


def resolve_tk_font(available_families) -> str:
    families = {family.casefold(): family for family in available_families}
    for preferred in ("SF Pro Text", "Helvetica Neue", "DejaVu Sans"):
        match = families.get(preferred.casefold())
        if match:
            return match
    return "DejaVu Sans"
