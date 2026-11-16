from dataclasses import replace
from types import SimpleNamespace

import pytest
from click.testing import CliRunner
from test_pushtotalk import _wait_until, make_controller

from localflow import api, config, live_config, settings_agent
from localflow.app import LiveSettings, keyboard_bindings
from localflow.cli import cli
from localflow.config import Config, ConfigError


@pytest.fixture
def config_file(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setattr(config, "legacy_config_path", lambda: tmp_path / "legacy.toml")
    return path


def test_config_default_and_roundtrip(config_file):
    assert config.load_config().dictation_enabled is True
    assert config.load_config().dictation_toggle_hotkey == ""
    config.save_config(
        Config(dictation_enabled=False, dictation_toggle_hotkey="<ctrl>+d")
    )
    loaded = config.load_config()
    assert loaded.dictation_enabled is False
    assert loaded.dictation_toggle_hotkey == "<ctrl>+d"
    with pytest.raises(ConfigError):
        config.apply_config_update(loaded, {"dictation": {"enabled": "false"}})


def test_cli_and_live_reload(config_file):
    config.save_config(Config())
    controller, recorder, _, work, _ = make_controller(config=Config())
    live = LiveSettings(Config(), controller)
    watcher = live_config.ConfigWatcher(live.apply)
    try:
        runner = CliRunner()
        assert runner.invoke(cli, ["dictation", "status"]).output == "Dictation: on\n"
        result = runner.invoke(cli, ["dictation", "off"])
        assert result.exit_code == 0, result.output
        assert result.output == "Dictation: off\n"
        assert watcher.check()
        assert _wait_until(lambda: not controller.config.dictation_enabled)
        controller.note_down()
        controller.note_up()
        controller.note_start()
        controller.note_stop()
        controller.note_toggle()
        controller.note_command_down()
        controller.note_command_up()
        controller.shutdown()
        assert recorder.start_calls == 0
        assert work.empty()
        assert runner.invoke(cli, ["dictation", "status"]).output == "Dictation: off\n"
        assert runner.invoke(cli, ["dictation", "on"]).exit_code == 0
        assert config.load_config().dictation_enabled
        assert runner.invoke(cli, ["dictation", "invalid"]).exit_code != 0
    finally:
        controller.shutdown()


def test_toggle_binding_works_while_disabled(config_file):
    initial = Config(dictation_enabled=False, dictation_toggle_hotkey="<ctrl>+<alt>+d")
    config.save_config(initial)
    controller, recorder, _, _, _ = make_controller(config=initial)
    try:
        bindings = keyboard_bindings(initial, controller)
        shortcut, callback = next(
            pair
            for pair in bindings["combos"]
            if pair[0] == initial.dictation_toggle_hotkey
        )
        assert shortcut == initial.dictation_toggle_hotkey
        callback()
        assert _wait_until(lambda: controller.config.dictation_enabled)
        assert config.load_config().dictation_enabled
        callback()
        assert _wait_until(lambda: not controller.config.dictation_enabled)
        assert not config.load_config().dictation_enabled
        assert recorder.start_calls == 0
    finally:
        controller.shutdown()


def test_toggle_save_error_keeps_state(config_file, monkeypatch):
    from localflow import pushtotalk

    def fail(_config):
        raise OSError("disk unavailable")

    config.save_config(Config())
    monkeypatch.setattr(pushtotalk, "save_config", fail)
    controller, recorder, _, _, _ = make_controller(config=Config())
    controller.note_dictation_toggle()
    controller.shutdown()
    assert controller.config.dictation_enabled
    assert config.load_config().dictation_enabled
    assert recorder.start_calls == 0


def test_disabling_discards_active_recording(config_file, monkeypatch):
    from localflow import pushtotalk

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    initial = Config(sounds_enabled=False)
    controller, recorder, _, work, _ = make_controller(config=initial)
    try:
        controller.note_down()
        assert _wait_until(lambda: recorder.recording)
        controller.update_config(replace(initial, dictation_enabled=False))
        assert _wait_until(lambda: not recorder.recording)
        controller.note_up()
        controller.shutdown()
        assert recorder.start_calls == 1
        assert recorder.stop_calls == 1
        assert work.empty()
        assert controller.is_idle()
    finally:
        controller.shutdown()


def test_settings_agent_validation_and_status():
    validator = settings_agent.Validator(platform="linux")
    initial = Config()
    assert validator.value("dictation_enabled", "off", initial) is False
    assert validator.value("dictation_toggle_hotkey", "off", initial) == ""
    assert (
        validator.value("dictation_toggle_hotkey", "ctrl+alt+d", initial)
        == "<ctrl>+<alt>+d"
    )
    with pytest.raises(settings_agent.SettingError):
        validator.value("dictation_toggle_hotkey", "not a shortcut", initial)
    conflicting = replace(initial, dictation_toggle_hotkey=initial.polish_hotkey)
    assert validator.conflicts(conflicting, ["dictation_toggle_hotkey"])
    platform = SimpleNamespace(name="linux", autostart_status=lambda: "off")
    assert (
        api.status(replace(initial, dictation_enabled=False), platform, False)[
            "dictation_enabled"
        ]
        is False
    )
