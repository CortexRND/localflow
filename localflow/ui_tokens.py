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
