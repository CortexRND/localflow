"""Static and pure-logic checks for the AppKit Quick checkup page (Linux-safe)."""

import ast
import re
from pathlib import Path

import pytest

from localflow import ui_tokens
from localflow.setup_flow import Check

ROOT = Path(__file__).parent.parent
SRC = ROOT / "localflow" / "setup_mac.py"
SNAPSHOTS = ROOT / "scripts" / "mac_ui_snapshots.py"


def _methods():
    tree = ast.parse(SRC.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "SetupWindowController"
    )
    return [node for node in cls.body if isinstance(node, ast.FunctionDef)]


def _is_python_method(function):
    return any(
        isinstance(dec, ast.Attribute) and dec.attr == "python_method"
        for dec in function.decorator_list
    )


def _method_source(name):
    method = next(m for m in _methods() if m.name == name)
    return method, ast.get_source_segment(SRC.read_text(encoding="utf-8"), method)


def _extract(path, wanted, namespace):
    """Execute selected module-level defs/assignments without importing AppKit."""
    tree = ast.parse(path.read_text(encoding="utf-8"))
    nodes = [
        node for node in tree.body
        if (isinstance(node, ast.FunctionDef) and node.name in wanted)
        or (isinstance(node, ast.Assign) and node.targets[0].id in wanted)
    ]
    found = {getattr(n, "name", None) or n.targets[0].id for n in nodes}
    assert found == wanted
    exec(compile(ast.Module(nodes, []), str(path), "exec"), namespace)  # noqa: S102
    return namespace


H = _extract(
    SRC,
    {"_CHECK_ACTION_LABELS", "check_action_label", "check_status", "ordered_check_indexes"},
    {"CHECK_STATUS": ui_tokens.CHECK_STATUS},
)
REAL_IDS = {
    "microphone": "Allow microphone",
    "accessibility": "Allow accessibility",
    "input-monitoring": "Allow input monitoring",
    "ffmpeg": "Install ffmpeg",
    "ollama-installed": "Get Ollama",
    "ollama-running": "Start Ollama",
    "microphone-device": "Check microphone",
}


def _check(key, ok, fix_label="Open settings"):
    return Check(key, key.title(), ok, "detail", fix_label, "https://example.com")


def test_ready_checks_never_have_an_action_even_with_a_fix_label():
    for key in REAL_IDS:
        assert H["check_action_label"](_check(key, True)) is None


@pytest.mark.parametrize("ok", [False, None])
def test_unready_checks_use_the_semantic_label_for_their_id(ok):
    for key, label in REAL_IDS.items():
        assert H["check_action_label"](_check(key, ok)) == label


def test_action_label_falls_back_and_requires_a_fix():
    assert H["check_action_label"](_check("llm-provider", None)) == "Open settings"
    assert H["check_action_label"](_check("ffmpeg", False, fix_label="")) is None


def test_action_labels_cover_the_real_check_ids_from_setup_flow():
    source = (ROOT / "localflow" / "setup_flow.py").read_text(encoding="utf-8")
    keys = set(re.findall(r'Check\(\s*"([a-z-]+)"', source))
    assert set(REAL_IDS) <= keys
    assert set(H["_CHECK_ACTION_LABELS"]) == set(REAL_IDS)


def test_status_pill_text_and_tint_follow_the_result():
    assert H["check_status"](True) == ("Ready", "success", "success")
    assert H["check_status"](False) == ("Needs you", "red", "red")
    assert H["check_status"](None) == ("Unknown", "sun", "ink")
    assert H["check_status"](True, pending=True) == ("Checking…", "sun", "ink")


def test_actionable_checks_sort_first_with_original_indexes_kept():
    checks = [
        _check("ffmpeg", True), _check("ollama-installed", True),
        _check("ollama-running", True), _check("microphone-device", True),
        _check("microphone", None), _check("accessibility", False),
        _check("input-monitoring", False),
    ]
    order = H["ordered_check_indexes"]
    assert order(checks) == [4, 5, 6, 0, 1, 2, 3]
    mixed = [_check("a", True), _check("b", False), _check("c", True), _check("d", None)]
    assert order(mixed) == [1, 3, 0, 2]
    assert order([_check("a", True), _check("b", True)]) == [0, 1]
    assert order([]) == []


def test_check_cards_tag_actions_with_the_original_index_not_the_sorted_one():
    _, render = _method_source("_render_checks")
    _, card = _method_source("_add_check_card")
    assert "order = ordered_check_indexes(self.checks)" in render
    assert (
        "self._add_check_card(document, self.checks[index], index, position * step)"
        in render
    )
    assert "action.setTag_(index)" in card
    assert "check_action_label(check)" in card
    assert "if action_label:" in card


def test_check_ui_uses_only_tokens_and_helpers_for_layout_and_style():
    for name in ("_render_checks", "_add_check_card", "_add_ollama_row"):
        method, text = _method_source(name)
        numbers = {
            node.value for node in ast.walk(method)
            if isinstance(node, ast.Constant)
            and isinstance(node.value, (int, float))
            and not isinstance(node.value, bool)
        }
        assert numbers <= {0, 1, 2}, f"{name} has ad-hoc numbers: {numbers}"
        assert not re.search(r"#[0-9A-Fa-f]{6}", text), f"{name} has a hex color"


def test_check_cards_are_ink_outlined_shadowed_and_measured():
    _, card = _method_source("_add_check_card")
    assert 'border="ink", shadow=True, width=CARD_BORDER_WIDTH' in card
    assert "make_pill(" in card and "fit_button(action)" in card
    assert "controls_left - CONTROL_GAP - action_width" in card
    assert "NSLineBreakByTruncatingTail" in card
    assert "NSViewMinXMargin" in card and "NSViewWidthSizable" in card
    assert ui_tokens.CARD_BORDER_WIDTH == 2
    assert ui_tokens.CHECK_CARD_HEIGHT == (
        2 * ui_tokens.CARD_PADDING + ui_tokens.LINE_HEIGHT["heading"]
        + ui_tokens.LINE_HEIGHT["body"]
    )


def test_checkup_copy_all_ready_callout_and_recheck_placement():
    source = SRC.read_text(encoding="utf-8")
    _, render = _method_source("_render_checks")
    assert '"Quick checkup."' in source
    assert "Let's make sure LocalFlow can hear you and type for you." in source
    assert "All set — nothing to fix." in render
    assert "all(check.ok is True for check in self.checks)" in render
    assert 'fill="sun"' in render and "SUN_TEXT" in render
    # Recheck sits on the intro row (right-anchored), above the list.
    assert "NSMakePoint(page_width - recheck_width, 0)" in render
    assert "NSViewMinXMargin" in render
    assert render.index("recheck") < render.index("_make_scroll")


def test_check_list_and_model_lists_fill_down_to_the_footer():
    _, scroll = _method_source("_make_scroll")
    assert "self.page.bounds().size" in scroll
    assert "page_size.height - y - reserve" in scroll
    assert "NSViewWidthSizable | AppKit.NSViewHeightSizable" in scroll
    _, render = _method_source("_render")
    assert "frame_height=" not in render  # no fixed short frames in the active renderer
    assert "reserve=ACTION_HEIGHT + CARD_GAP" in render
    assert "self._make_scroll(len(self.models) * 84 + SHADOW_OFFSET)" in render


def test_setup_window_is_resizable_with_a_min_size_and_anchored_footer():
    source = SRC.read_text(encoding="utf-8")
    assert "NSWindowStyleMaskResizable" in source
    assert "setContentMinSize_(NSMakeSize(WINDOW_MIN_WIDTH, WINDOW_MIN_HEIGHT))" in source
    assert "NSViewMinYMargin" in source and "NSViewMinXMargin" in source
    assert ui_tokens.WINDOW_MIN_WIDTH >= 800 and ui_tokens.WINDOW_MIN_HEIGHT >= 700
    _, refresh = _method_source("_refresh_header_colors")
    assert "self.divider" in refresh  # theme refresh covers the footer divider


def test_buttons_get_visible_focus_rings():
    style = (ROOT / "localflow" / "mac_style.py").read_text(encoding="utf-8")
    assert "setFocusRingType_(AppKit.NSFocusRingTypeExterior)" in style
    _, card = _method_source("_add_card_button")
    assert "setFocusRingType_(AppKit.NSFocusRingTypeExterior)" in card


def test_new_check_helpers_are_python_methods_not_selectors():
    names = {f.name for f in _methods() if _is_python_method(f)}
    assert {"_render_checks", "_add_check_card", "_add_ollama_row"} <= names


def test_snapshot_fixtures_are_the_real_seven_checks_and_keep_fix_labels():
    seven = _extract(
        SNAPSHOTS, {"_CHECK_SPECS", "_NEEDS_ACTION", "_seven_checks"}, {"Check": Check}
    )["_seven_checks"]
    needs = seven()
    assert [c.key for c in needs] == [
        "ffmpeg", "ollama-installed", "ollama-running", "microphone-device",
        "microphone", "accessibility", "input-monitoring",
    ]
    assert [c.ok for c in needs] == [True, True, True, True, None, False, False]
    ready = seven(all_ready=True)
    assert all(c.ok is True for c in ready)
    assert all(c.fix_label for c in ready)  # regression bait for buttons on ready rows
    assert not any(H["check_action_label"](c) for c in ready)
    assert [H["check_action_label"](needs[i]) for i in (4, 5, 6)] == [
        "Allow microphone", "Allow accessibility", "Allow input monitoring",
    ]


def test_snapshot_script_covers_checkup_states_wide_width_and_restores_size():
    source = SNAPSHOTS.read_text(encoding="utf-8")
    for name in (
        "setup-checks-needs-action", "setup-checks-all-ready",
        "setup-checks-needs-action-wide",
    ):
        assert name in source
    assert "WIDE_WIDTH = 1100" in source
    assert "window.setContentSize_(NSMakeSize(WIDE_WIDTH, size.height))" in source
    assert "window.setContentSize_(size)" in source  # restored after the wide shot
    assert "snapshot mode starts no workers" in source
    assert "setup.snapshot_mode = True" in source
    assert "_setup_check_snapshots(setup)" in source
    assert "for step in range(7):" in source  # existing states preserved


@pytest.mark.parametrize(
    "path",
    [SRC, SNAPSHOTS, ROOT / "localflow" / "mac_style.py", ROOT / "localflow" / "ui_tokens.py"],
)
def test_macos_only_modules_compile_without_importing_appkit(path):
    compile(path.read_text(encoding="utf-8"), str(path), "exec")
