import ast
from pathlib import Path

ROOT = Path(__file__).parent.parent
SOURCE = ROOT / "localflow" / "hub_mac.py"


def _controller_methods():
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    return [node for node in cls.body if isinstance(node, ast.FunctionDef)]


def _python_method(function):
    return any(
        isinstance(decorator, ast.Attribute)
        and isinstance(decorator.value, ast.Name)
        and decorator.value.id == "objc"
        and decorator.attr == "python_method"
        for decorator in function.decorator_list
    )


def test_appkit_hub_has_valid_selector_names():
    invalid = []
    for method in _controller_methods():
        if _python_method(method):
            continue
        args = len(method.args.args) + len(method.args.kwonlyargs) - 1
        if method.name.startswith("_") or method.name.count("_") != args:
            invalid.append(method.name)
    assert not invalid


def test_hub_snapshot_workflow_covers_each_page_and_error_state():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text()
    workflow = (ROOT / ".github" / "workflows" / "macos-ui.yml").read_text()
    assert "from localflow.hub_mac import build_window" in snapshots
    assert '"Home", "History", "Dictionary", "Style", "Models"' in snapshots
    assert '"Unable to load local data."' in snapshots
    assert "localflow/hub*" in workflow


def test_label_calls_do_not_repeat_positional_parameters():
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    helper = next(
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_label"
    )
    parameter_names = [argument.arg for argument in helper.args.args]
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        if not isinstance(node.func, ast.Name) or node.func.id != "_label":
            continue
        positional = set(parameter_names[:len(node.args)])
        keywords = {keyword.arg for keyword in node.keywords}
        assert not positional & keywords


def test_appkit_history_uses_a_cleared_paged_results_container():
    source = SOURCE.read_text(encoding="utf-8")
    assert "self.history_results.subviews()" in source
    assert "view.removeFromSuperview()" in source
    assert "offset=self.history_offset" in source
    assert '"olderHistory:"' in source
    assert '"newerHistory:"' in source


def test_hub_settings_use_segmented_controls_and_reload_models_config():
    source = SOURCE.read_text(encoding="utf-8")
    assert "NSPopUpButton" not in source
    assert '"strict"' not in source
    assert '("formal", "casual", "very_casual", "excited")' in source
    assert "self.config = load_config()" in source
    assert "self.dictionary.update_word(" in source
    assert "self.dictionary.update_replacement(" in source
    assert '"selectCleanupLevel_"' in source
    assert '"selectStyle_"' in source


def test_hub_buttons_use_setup_pop_art_tokens():
    source = SOURCE.read_text(encoding="utf-8")
    style = (ROOT / "localflow" / "mac_style.py").read_text(encoding="utf-8")
    assert "from localflow.mac_style import color as _color" in source
    assert "from localflow.setup_mac import TOKENS" in style
    assert "def style_flat_button(button, selected=False)" in style
    assert 'selected=page == self.page' in source
    assert "selected=PAGES[button.tag()] == page" in source
    assert "button.layer().setBorderWidth_(2)" in style
