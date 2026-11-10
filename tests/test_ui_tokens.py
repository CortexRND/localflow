from localflow.ui_tokens import (
    DARK,
    LIGHT,
    PALETTES,
    SUN_TEXT,
    TOKENS,
    resolve_tk_font,
    surface_tint,
    tokens_for,
)


def test_native_palettes_match_the_design_spec():
    assert LIGHT == {
        "paper": "#FBF6EC",
        "surface": "#FFFFFF",
        "ink": "#141414",
        "muted": "#5C5C5C",
        "line": "#E6DFD0",
        "cobalt": "#2346D8",
        "sun": "#FFD23F",
        "red": "#E5484D",
        "success": "#1F9D55",
    }
    assert DARK == {
        "paper": "#15140F",
        "surface": "#1E1C17",
        "ink": "#F5F1E8",
        "muted": "#A8A296",
        "line": "#34312A",
        "cobalt": "#5B7BFF",
        "sun": "#FFD23F",
        "red": "#FF6369",
        "success": "#3DD68C",
    }
    assert TOKENS is LIGHT
    assert PALETTES == {"light": LIGHT, "dark": DARK}


def test_theme_and_tk_font_resolution():
    assert tokens_for("dark") is DARK
    assert tokens_for("LIGHT") is LIGHT
    assert SUN_TEXT == "#141414"
    assert surface_tint("success", LIGHT) == "#E4F3EB"
    assert surface_tint("success", DARK) == "#223225"
    assert resolve_tk_font(["Helvetica", "DejaVu Sans", "SF Pro Text"]) == "SF Pro Text"
    assert resolve_tk_font(["Helvetica Neue", "DejaVu Sans"]) == "Helvetica Neue"
    assert resolve_tk_font(["Helvetica"]) == "DejaVu Sans"
