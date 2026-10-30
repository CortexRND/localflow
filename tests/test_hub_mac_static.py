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
