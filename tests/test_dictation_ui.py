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
    tree = ast.parse((ROOT / "localflow" / filename).read_text(encoding="utf-8"))
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
        "refresh_speaker_labels",
        "toggle_speaker_labels",
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
    read_text = Path.read_text

    def windows_read_text(path, *args, **kwargs):
        if path.name == "menubar.py":
            kwargs.setdefault("encoding", "cp1252")
        return read_text(path, *args, **kwargs)

    monkeypatch.setattr(Path, "read_text", windows_read_text)
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


class _NativeItem:
    """Stands in for rumps.MenuItem; records every write to `.state`."""

    def __init__(self):
        self._state = 0
        self.writes = []

    @property
    def state(self):
        return self._state

    @state.setter
    def state(self, value):
        self.writes.append(value)
        self._state = value


def _speaker_menu(tmp_path, monkeypatch):
    from localflow.live_config import file_signature

    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    loads = Mock(side_effect=config.load_config)
    notifications = Mock()
    ns = _methods(
        "menubar.py",
        "LocalflowMenuBar",
        {
            "load_config": loads,
            "save_config": config.save_config,
            "replace": replace,
            "config_path": config.config_path,
            "file_signature": file_signature,
            "ConfigError": config.ConfigError,
            "rumps": SimpleNamespace(notification=notifications),
        },
    )
    # The attributes __init__ sets before its first (forced) refresh.
    app = SimpleNamespace(
        speaker_labels_item=_NativeItem(), _speaker_signature=None, _speaker_labels=None
    )
    app.refresh_speaker_labels = lambda force=False: ns["refresh_speaker_labels"](
        app, force
    )
    return ns, app, loads, notifications


def test_menubar_speaker_labels_item_is_a_native_checkmark_after_dismiss():
    source = (ROOT / "localflow" / "menubar.py").read_text(encoding="utf-8")
    assert (
        'rumps.MenuItem(\n            "Speaker labels", '
        "callback=self.toggle_speaker_labels\n        )"
    ) in source
    assert (
        "self.dismiss_item,\n            self.speaker_labels_item,\n            None,"
    ) in source
    init = source.split("def __init__(", 1)[1].split("def ", 1)[0]
    assert init.index("self.refresh_speaker_labels(force=True)") < init.index(
        "self.timer.start()"
    )
    tick = source.split("def tick(", 1)[1].split("def ", 1)[0]
    assert "self.refresh_speaker_labels()" in tick
    refresh = source.split("def refresh_speaker_labels(", 1)[1].split("def ", 1)[0]
    assert "_live_config" not in refresh and "requests." not in refresh


@pytest.mark.parametrize("enabled", [True, False])
def test_menubar_speaker_labels_initial_state(tmp_path, monkeypatch, enabled):
    _ns, app, loads, _notifications = _speaker_menu(tmp_path, monkeypatch)
    config.save_config(config.Config(speaker_diarization=enabled))
    app.refresh_speaker_labels(force=True)
    assert app.speaker_labels_item.state == int(enabled)
    assert app.speaker_labels_item.writes == [int(enabled)]
    for _ in range(5):  # idle ticks: a stat only, no reload, no AppKit write
        app.refresh_speaker_labels()
    assert loads.call_count == 1
    assert app.speaker_labels_item.writes == [int(enabled)]


def test_menubar_speaker_labels_default_off_without_a_config_file(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("HOME", str(tmp_path))  # no legacy ~/.localflow.toml
    _ns, app, loads, _notifications = _speaker_menu(tmp_path, monkeypatch)
    app.refresh_speaker_labels(force=True)
    assert app.speaker_labels_item.state == 0
    app.refresh_speaker_labels()
    assert loads.call_count == 1


def test_menubar_speaker_labels_toggle_persists_and_follows_external_edits(
    tmp_path, monkeypatch
):
    ns, app, _loads, notifications = _speaker_menu(tmp_path, monkeypatch)
    config.save_config(config.Config(llm_model="keep-me", dictation_enabled=False))
    app.refresh_speaker_labels(force=True)
    assert app.speaker_labels_item.state == 0

    ns["toggle_speaker_labels"](app, None)
    saved = config.load_config()
    assert saved.speaker_diarization is True
    assert saved.llm_model == "keep-me" and saved.dictation_enabled is False
    assert app.speaker_labels_item.state == 1

    ns["toggle_speaker_labels"](app, None)
    assert config.load_config().speaker_diarization is False
    assert app.speaker_labels_item.state == 0

    # Edited elsewhere (Hub, `lf config set`, a text editor): next tick shows it.
    config.save_config(replace(config.load_config(), speaker_diarization=True))
    app.refresh_speaker_labels()
    assert app.speaker_labels_item.state == 1
    writes = len(app.speaker_labels_item.writes)

    # An unrelated key changes: reloaded once, but the checkmark is not re-set.
    config.save_config(replace(config.load_config(), llm_model="other"))
    app.refresh_speaker_labels()
    assert app.speaker_labels_item.state == 1
    assert len(app.speaker_labels_item.writes) == writes
    notifications.assert_not_called()


def test_menubar_speaker_labels_keeps_state_for_a_malformed_file(
    tmp_path, monkeypatch
):
    _ns, app, loads, notifications = _speaker_menu(tmp_path, monkeypatch)
    config.save_config(config.Config(speaker_diarization=True))
    app.refresh_speaker_labels(force=True)
    path = tmp_path / "config.toml"
    path.write_text("[meetings\nspeaker_diarization = ", encoding="utf-8")
    for _ in range(3):
        app.refresh_speaker_labels()
    assert app.speaker_labels_item.state == 1
    assert loads.call_count == 2  # one failed read, not one per tick
    notifications.assert_not_called()

    config.save_config(config.Config(speaker_diarization=False))
    app.refresh_speaker_labels()
    assert app.speaker_labels_item.state == 0


def test_menubar_speaker_labels_save_failure_notifies_and_keeps_state(
    tmp_path, monkeypatch
):
    ns, app, _loads, notifications = _speaker_menu(tmp_path, monkeypatch)
    config.save_config(config.Config(speaker_diarization=True, llm_model="keep-me"))
    app.refresh_speaker_labels(force=True)

    def fail(_config):
        raise OSError("read-only")

    ns["save_config"] = fail
    ns["toggle_speaker_labels"](app, None)
    notifications.assert_called_once()
    assert notifications.call_args.args[:2] == (
        "localflow",
        "Couldn't change speaker labels",
    )
    assert "read-only" in notifications.call_args.args[2]
    saved = config.load_config()
    assert saved.speaker_diarization is True and saved.llm_model == "keep-me"
    assert app.speaker_labels_item.state == 1
    assert app.speaker_labels_item.writes == [1]


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


def test_snapshot_manual_controls_match_current_allowlist():
    from localflow.settings_agent import ALLOWLIST

    tree = ast.parse((ROOT / "scripts/mac_ui_snapshots.py").read_text(encoding="utf-8"))
    function = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_hub_settings_snapshots"
    )
    assertion = next(
        node
        for node in function.body
        if isinstance(node, ast.Assert)
        and any(
            isinstance(child, ast.Attribute) and child.attr == "manual_rows"
            for child in ast.walk(node)
        )
    )
    code = compile(
        ast.Module(body=[assertion], type_ignores=[]), "snapshot_assertion", "exec"
    )
    page = SimpleNamespace(manual_rows=[SimpleNamespace(key=key) for key in ALLOWLIST])
    namespace = {"page": page, "ALLOWLIST": ALLOWLIST}
    exec(code, namespace)  # noqa: S102 - exercise the snapshot guard without AppKit
    page.manual_rows.pop()
    with pytest.raises(AssertionError, match="every allowlisted setting"):
        exec(code, namespace)  # noqa: S102
