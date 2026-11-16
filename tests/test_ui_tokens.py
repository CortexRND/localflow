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
    assert DARK == {
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


def test_page_titles_share_one_display_style_that_clears_the_header_offset():
    import ast
    from pathlib import Path

    from localflow import ui_tokens

    root = Path(__file__).resolve().parents[1] / "localflow"
    assert ui_tokens.TYPE["display"] == (28, "bold")  # the setup Quick checkup title
    assert ui_tokens.PAGE_TITLE_HEIGHT == ui_tokens.LINE_HEIGHT["display"] == 38
    assert ui_tokens.PAGE_TITLE_HEIGHT <= ui_tokens.HUB_HEADER_OFFSET
    style = (root / "mac_style.py").read_text(encoding="utf-8")
    title = next(
        node for node in ast.parse(style).body
        if isinstance(node, ast.FunctionDef) and node.name == "page_title"
    )
    body = ast.get_source_segment(style, title)
    assert 'TYPE["display"]' in body and "PAGE_TITLE_HEIGHT" in body
    # Hub: every page title goes through the shared helper (no per-page 20pt label).
    hub = (root / "hub_mac.py").read_text(encoding="utf-8")
    calls = [
        node for node in ast.walk(ast.parse(hub))
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
        and node.func.id == "page_title"
    ]
    assert calls
    for call in calls:
        # The title text is the page's display label (e.g. "Models & keys").
        text = call.args[0]
        assert isinstance(text, ast.Call) and ast.unparse(text) == "sidebar_label(page)"
    assert '_label(page, 0' not in hub
    assert '_label(sidebar_label(page)' not in hub
    # Setup keeps the same size and line height through the tokens.
    setup = (root / "setup_mac.py").read_text(encoding="utf-8")
    assert 'TYPE["display"][0], True, height=PAGE_TITLE_HEIGHT' in setup
