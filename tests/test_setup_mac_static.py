"""Check AppKit setup selectors without importing PyObjC on Linux."""

import ast
from pathlib import Path

SRC = Path(__file__).parent.parent / "localflow" / "setup_mac.py"
SNAPSHOTS = Path(__file__).parent.parent / "scripts" / "mac_ui_snapshots.py"


def _methods():
    tree = ast.parse(SRC.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "SetupWindowController"
    )
    return [node for node in cls.body if isinstance(node, ast.FunctionDef)]


def _is_python_method(function):
    return any(
        isinstance(dec, ast.Attribute)
        and isinstance(dec.value, ast.Name)
        and dec.value.id == "objc"
        and dec.attr == "python_method"
        for dec in function.decorator_list
    )


def test_non_python_methods_are_valid_selectors():
    invalid = []
    for function in _methods():
        if _is_python_method(function):
            continue
        args = len(function.args.args) + len(function.args.kwonlyargs) - 1
        if function.name.startswith("_"):
            invalid.append(f"{function.name}: private method exported as a selector")
        elif function.name.count("_") != args:
            invalid.append(
                f"{function.name}: {function.name.count('_')} underscores but {args} args"
            )
    assert not invalid, "malformed selectors:\n" + "\n".join(invalid)


def test_setup_helpers_are_not_exported_as_selectors():
    names = {function.name for function in _methods() if _is_python_method(function)}
    assert {
        "_build_window", "_render", "_start_worker", "_handle_events",
        "_start_checks", "_start_download", "_start_pull", "_start_trial",
        "_advance", "_select_stt", "_select_llm", "_fix_check",
    } <= names


def test_mac_snapshot_script_captures_each_setup_step_and_provider_states():
    source = SNAPSHOTS.read_text(encoding="utf-8")
    assert "from localflow.setup_mac import SetupWindowController" in source
    assert "for step in range(7):" in source
    assert 'setup-non-ollama-provider' in source
    assert 'setup-download-progress' in source
    assert 'setup-download-error' in source


def test_setup_mac_requires_ollama_for_selected_cleanup_and_reads_login_choice():
    source = SRC.read_text(encoding="utf-8")

    assert "Ollama isn't running — start it, or choose No cleanup." in source
    assert "start_on_login=bool(self.start_on_login)" in source
    assert "if self.step == 2 and self.stt_id == model_id:" in source
    assert "if self.step == 3 and self.llm_id == model_id:" in source
    assert 'if "stt-download" in self.busy:' in source
    assert 'if "llm-pull" in self.busy:' in source
    assert 'model_id=model.id' in source
    assert 'self._start_worker("llm-pull", pull, model_id=model_id)' in source
    assert 'self.back_button.setEnabled_(not model_busy)' in source
    assert 'if self.busy & {"stt-download", "llm-pull"}:' in source
