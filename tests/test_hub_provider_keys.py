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
    source = SOURCE.read_text()
    models = source[
        source.index("    def _page_models") : source.index("    def _page_hot_phrases")
    ]
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
