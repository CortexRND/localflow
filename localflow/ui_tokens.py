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


def resolve_tk_font(available_families) -> str:
    families = {family.casefold(): family for family in available_families}
    for preferred in ("SF Pro Text", "Helvetica Neue", "DejaVu Sans"):
        match = families.get(preferred.casefold())
        if match:
            return match
    return "DejaVu Sans"
