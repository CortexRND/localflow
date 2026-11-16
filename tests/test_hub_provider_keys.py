import ast
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import provider_keys as keys

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "localflow/hub_mac.py"


def action_method():
    tree = ast.parse(SOURCE.read_text())
    controller = next(
        node
        for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    method = next(
        node
        for node in controller.body
        if isinstance(node, ast.FunctionDef) and node.name == "_models_action"
    )
    method.decorator_list = []
    namespace = {}
    exec(  # noqa: S102
        compile(ast.Module(body=[method], type_ignores=[]), str(SOURCE), "exec"),
        namespace,
    )
    return namespace["_models_action"]


def controller_methods():
    tree = ast.parse(SOURCE.read_text())
    controller = next(
        node
        for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    return {
        node.name: node for node in controller.body if isinstance(node, ast.FunctionDef)
    }


def provider_ui_source():
    """Every Models & keys method, wherever it sits relative to other pages."""
    source = SOURCE.read_text()
    return "\n".join(
        ast.get_source_segment(source, node)
        for name, node in controller_methods().items()
        if name == "_page_models" or name.startswith(("_models_", "provider"))
    )


def test_models_state_is_captured_before_teardown_and_secrets_never_are():
    methods = controller_methods()
    source = SOURCE.read_text()
    show_page = ast.get_source_segment(source, methods["show_page"])
    assert show_page.index("self._models_capture()") < show_page.index(
        "removeFromSuperview"
    )
    capture = ast.get_source_segment(source, methods["_models_capture"])
    assert "_secure" in capture and "secure" not in capture.replace("_secure", "")
    # show_page drops the old control refs after capture, before teardown.
    capture_at = show_page.index("self._models_capture()")
    drop_at = show_page.index("self.provider_controls = {}")
    assert capture_at < drop_at < show_page.index("removeFromSuperview")
    assert "self.provider_checks = []" in show_page
    # ...but capture itself keeps them: _models_action reads the secure field
    # after capturing.
    assert "provider_controls = {}" not in capture
    worker = ast.get_source_segment(source, methods["_models_worker"])
    assert "if self._closed:" in worker
    # Provider helpers stay discoverable even though they follow the Ask and
    # Hot Phrases pages in the file.
    for name in ("_page_models", "_models_action", "providerActivate_"):
        assert name in methods


def extracted(name):
    method = controller_methods()[name]
    method.decorator_list = []
    namespace = {}
    exec(  # noqa: S102
        compile(ast.Module(body=[method], type_ignores=[]), str(SOURCE), "exec"),
        namespace,
    )
    return namespace[name]


def test_real_capture_keeps_controls_for_save_and_never_reads_secure_field():
    secure, combo, url = Mock(), Mock(), Mock()
    combo.stringValue.return_value = " model-x "
    url.stringValue.return_value = " https://x.example/v1 "
    controller = SimpleNamespace(
        provider_state={"custom": {"url": "", "model": ""}},
        provider_controls={"custom": (url, combo, secure)},
        provider_checks=[],
    )
    extracted("_models_capture")(controller)
    assert controller.provider_state["custom"] == {
        "url": "https://x.example/v1",
        "model": "model-x",
    }
    secure.stringValue.assert_not_called()  # never persisted into state
    assert controller.provider_controls["custom"][2] is secure  # Save can still read it


@pytest.mark.parametrize("fails", [False, True])
def test_save_always_clears_secure_field_and_sanitizes_errors(monkeypatch, fails):
    secure = Mock()
    secure.stringValue.return_value = "private-key-1234"
    controller = SimpleNamespace(
        _models_capture=Mock(),
        provider_state={
            "fireworks": {
                "url": keys.get_preset("fireworks").base_url,
                "model": "model",
            }
        },
        provider_controls={"fireworks": (None, None, secure)},
        show_page=Mock(),
    )
    save = Mock(side_effect=RuntimeError("private-key") if fails else None)
    monkeypatch.setattr(keys, "save_key", save)
    action_method()(controller, SimpleNamespace(tag=lambda: 1), "save")
    save.assert_called_once_with("fireworks", "private-key-1234")
    secure.setStringValue_.assert_called_once_with("")
    message, ok = controller.provider_state["fireworks"]["message"]
    assert ok is not fails
    assert "private-key" not in message
    controller.show_page.assert_called_once_with("Models")


def test_provider_ui_has_secure_fields_masked_results_and_async_network():
    models = provider_ui_source()
    assert "_phrases" not in models  # only provider methods, not Hot Phrases helpers
    for required in (
        'setStringValue_("Models & keys")',
        "provider_keys.PRESETS",
        "AppKit.NSSecureTextField",
        "candidate.masked_value",
        "NSComboBox",
        "setShowsHiddenFiles_(True)",
        '"No keys found"',
        "provider_keys.save_imports",
        "check.state()",
        "provider_keys.activate",
        "provider_keys.fetch_models",
        "provider_keys.test_llm",
        "threading.Thread",
        "NSOperationQueue.mainQueue().addOperationWithBlock_",
        'color="success" if ok else "red"',
    ):
        assert required in models
    assert "candidate._value" not in models
    assert "print(" not in models


def test_provider_snapshots_cover_states_sizes_and_mock_external_access():
    source = (ROOT / "scripts/mac_ui_snapshots.py").read_text()
    for state in (
        "no-keys",
        "saved-active",
        "import-results",
        "test-success",
        "test-failure",
    ):
        assert f'"{state}"' in source
    assert 'f"hub-models-keys-{state}-{label}"' in source
    assert "for label, width, height in HUB_SIZES:" in source
    for name in ("get_secret", "set_secret", "delete_secret"):
        assert f'patch("localflow.secrets.{name}"' in source
    for name in ("detect_imports", "fetch_models", "test_llm", "save_config"):
        assert f'patch("localflow.provider_keys.{name}"' in source
