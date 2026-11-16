import ast
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import config, hub_model
from localflow.live_config import LazyConfig

ROOT = Path(__file__).parent.parent


def _methods(filename, classname, namespace):
    tree = ast.parse((ROOT / "localflow" / filename).read_text())
    cls = next(
        node
        for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == classname
    )
    names = {
        "_show",
        "refresh_dictation_status",
        "toggle_dictation",
        "toggleDictation_",
    }
    methods = [
        node
        for node in cls.body
        if isinstance(node, ast.FunctionDef) and node.name in names
    ]
    for node in methods:
        node.decorator_list = []
    exec(  # noqa: S102 - native handlers are exercised without importing AppKit
        compile(ast.Module(body=methods, type_ignores=[]), filename, "exec"), namespace
    )
    return namespace


@pytest.mark.parametrize("enabled", [True, False])
def test_home_dictation_labels(enabled):
    state = hub_model.dictation_state(config.Config(dictation_enabled=enabled))
    assert state["enabled"] is enabled
    assert state["label"] == f"Dictation: {'On' if enabled else 'Off'}"
    assert state["action"] == f"Turn Dictation {'Off' if enabled else 'On'}"
    assert state["detail"] == (
        "Dictation is on — push-to-talk is ready"
        if enabled
        else "Dictation is off — push-to-talk is paused"
    )


def test_shortcut_listing_includes_dictation_toggle():
    row = hub_model.shortcut_rows(
        config.Config(dictation_toggle_hotkey="<ctrl>+<alt>+d")
    )[-1]
    assert row["name"] == "Toggle Dictation"
    assert row["hotkey"] == "Control + Option + D"
    assert hub_model.shortcut_rows(config.Config())[-1]["hotkey"] == "Off"


def test_menubar_toggle_persists_and_external_changes_refresh(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    config.save_config(config.Config(llm_model="keep-me"))
    notifications = Mock()
    ns = _methods(
        "menubar.py",
        "LocalflowMenuBar",
        {
            "load_config": config.load_config,
            "save_config": config.save_config,
            "replace": replace,
            "dictation_state": hub_model.dictation_state,
            "_live_config": LazyConfig(config.load_config()),
            "title_prefix": lambda _state: "",
            "rumps": SimpleNamespace(notification=notifications),
        },
    )
    app = SimpleNamespace(
        _shown={},
        _live_state=SimpleNamespace(state=lambda: "idle"),
        _base_icon="mic",
        dictation_item=SimpleNamespace(title=""),
    )
    app._show = lambda *args: ns["_show"](app, *args)
    app.refresh_dictation_status = lambda: ns["refresh_dictation_status"](app)
    app.refresh_dictation_status()
    assert app.dictation_item.title == "Dictation: On"
    ns["toggle_dictation"](app, None)
    assert not config.load_config().dictation_enabled
    assert config.load_config().llm_model == "keep-me"
    assert app.dictation_item.title == "Dictation: Off"
    assert app.title == "mic (off)"
    config.save_config(replace(config.load_config(), dictation_enabled=True))
    app.refresh_dictation_status()
    assert app.dictation_item.title == "Dictation: On"
    assert app.title == "mic"

    def fail(_config):
        raise OSError("read-only")

    ns["save_config"] = fail
    ns["toggle_dictation"](app, None)
    assert config.load_config().dictation_enabled
    notifications.assert_called_once()


def test_native_home_toggle_saves_switch_value(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    config.save_config(config.Config())
    ns = _methods(
        "hub_mac.py",
        "HubWindowController",
        {
            "load_config": config.load_config,
            "save_config": config.save_config,
            "replace": replace,
        },
    )
    hub = SimpleNamespace(config=config.Config(), page_errors={}, show_page=Mock())
    ns["toggleDictation_"](hub, SimpleNamespace(state=lambda: 0))
    assert not config.load_config().dictation_enabled
    hub.show_page.assert_called_with("Home")

    def fail(_config):
        raise OSError("read-only")

    ns["save_config"] = fail
    ns["toggleDictation_"](hub, SimpleNamespace(state=lambda: 1))
    assert not config.load_config().dictation_enabled
    assert "read-only" in hub.page_errors["Home"]
