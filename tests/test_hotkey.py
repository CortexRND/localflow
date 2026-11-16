from dataclasses import dataclass
from types import SimpleNamespace
from typing import ClassVar

import pytest

from localflow import hotkey as hotkey_module


@dataclass(frozen=True)
class _KeyCode:
    char: str

    @classmethod
    def from_char(cls, char):
        return cls(char)


class _HotKey:
    def __init__(self, keys, callback):
        self._keys = set(keys)
        self._pressed = set()
        self._callback = callback
        self._active = False

    @staticmethod
    def parse(shortcut):
        keys = []
        for part in shortcut.split("+"):
            if part.startswith("<") and part.endswith(">"):
                key = getattr(_Key, part[1:-1], None)
                if key is None:
                    raise ValueError(shortcut)
                keys.append(key)
            elif len(part) == 1:
                keys.append(_KeyCode.from_char(part.lower()))
            else:
                raise ValueError(shortcut)
        if len(keys) != len(set(keys)):
            raise ValueError(shortcut)
        return keys

    def press(self, key):
        if key in self._keys:
            self._pressed.add(key)
        if self._pressed == self._keys and not self._active:
            self._active = True
            self._callback()

    def release(self, key):
        self._pressed.discard(key)
        if self._pressed != self._keys:
            self._active = False


class _Key:
    alt = object()
    alt_l = object()
    alt_r = object()
    cmd = object()
    cmd_l = object()
    cmd_r = object()
    ctrl = object()
    ctrl_l = object()
    ctrl_r = object()
    esc = object()
    f19 = object()


keyboard = SimpleNamespace(
    Key=_Key,
    KeyCode=_KeyCode,
    HotKey=_HotKey,
)
hotkey_module.keyboard = keyboard
KeyboardHub = hotkey_module.KeyboardHub


def _hub(**overrides):
    arguments = {
        "hold_key": None,
        "toggle_combo": None,
        "combos": [],
        "on_down": lambda: None,
        "on_up": lambda: None,
        "on_toggle": lambda: None,
        "on_cancel": lambda: None,
        "command_key": None,
        "on_command_down": lambda: None,
        "on_command_up": lambda: None,
    }
    arguments.update(overrides)
    return KeyboardHub(**arguments)


def test_keyboard_hub_routes_multiple_combos_and_escape():
    called = []
    hub = _hub(
        combos=[
            ("<ctrl>+<alt>+1", lambda: called.append("polish")),
            ("<ctrl>+<alt>+2", lambda: called.append("prompt")),
        ],
        on_cancel=lambda: called.append("cancel"),
    )
    keys = (keyboard.Key.ctrl, keyboard.Key.alt, keyboard.KeyCode.from_char("1"))
    for key in keys:
        hub._press(key)
    for key in reversed(keys):
        hub._release(key)

    keys = (keyboard.Key.ctrl, keyboard.Key.alt, keyboard.KeyCode.from_char("2"))
    for key in keys:
        hub._press(key)
    for key in reversed(keys):
        hub._release(key)
    hub._press(keyboard.Key.esc)

    assert called == ["polish", "prompt", "cancel"]


def test_keyboard_hub_routes_hold_and_command_key_events():
    called = []
    hub = _hub(
        hold_key="f19",
        command_key="alt_r",
        on_down=lambda: called.append("dictation-down"),
        on_up=lambda: called.append("dictation-up"),
        on_command_down=lambda: called.append("command-down"),
        on_command_up=lambda: called.append("command-up"),
    )

    hub._press(keyboard.Key.f19)
    hub._release(keyboard.Key.f19)
    hub._press(keyboard.Key.alt_r)
    hub._command_timer.cancel()
    hub._command_down_fire()
    hub._release(keyboard.Key.alt_r)

    assert called == [
        "dictation-down",
        "dictation-up",
        "command-down",
        "command-up",
    ]


def test_keyboard_hub_disables_later_duplicate_shortcuts(capsys):
    called = []
    hub = _hub(
        hold_key="f19",
        combos=[("f19", lambda: called.append("combo"))],
        command_key="f19",
        on_down=lambda: called.append("hold"),
        on_command_down=lambda: called.append("command"),
    )

    hub._press(keyboard.Key.f19)
    output = capsys.readouterr().out

    assert called == ["hold"]
    assert output.count("disabling") == 2


def test_command_and_hold_modifier_sides_are_distinct():
    called = []
    hub = _hub(
        hold_key="alt_l",
        command_key="alt_r",
        on_down=lambda: called.append("hold-down"),
        on_up=lambda: called.append("hold-up"),
    )

    hub._press(keyboard.Key.alt_l)
    hub._release(keyboard.Key.alt_l)
    hub._press(keyboard.Key.alt_r)
    hub._command_timer.cancel()
    hub._command_timer = None
    hub._release(keyboard.Key.alt_r)

    assert called == ["hold-down", "hold-up"]


def test_command_modifier_in_transform_chord_does_not_start_command():
    called = []
    hub = _hub(
        command_key="alt_r",
        combos=[("<ctrl>+<alt>+1", lambda: called.append("polish"))],
        on_command_down=lambda: called.append("command"),
    )

    class CanonicalListener:
        @staticmethod
        def canonical(key):
            return keyboard.Key.alt if key == keyboard.Key.alt_r else key

    hub._listener = CanonicalListener()
    hub._command_combo_key = keyboard.Key.alt
    for key in (
        keyboard.Key.ctrl,
        keyboard.Key.alt_r,
        keyboard.KeyCode.from_char("1"),
    ):
        hub._press(key)

    assert hub._command_timer is None
    assert called == ["polish"]


def test_polish_and_prompt_duplicate_shortcut_disables_prompt(capsys):
    called = []
    shortcut = "<ctrl>+<alt>+1"
    hub = _hub(
        combos=[
            (shortcut, lambda: called.append("polish")),
            (shortcut, lambda: called.append("prompt")),
        ]
    )
    keys = (keyboard.Key.ctrl, keyboard.Key.alt, keyboard.KeyCode.from_char("1"))
    for key in keys:
        hub._press(key)

    assert called == ["polish"]
    assert "disabling shortcut" in capsys.readouterr().out


def test_active_command_is_cancelled_before_command_key_chord_callback():
    called = []
    hub = _hub(
        command_key="alt_r",
        combos=[("<ctrl>+<alt>+1", lambda: called.append("polish"))],
        on_command_down=lambda: called.append("command"),
        on_command_up=lambda: called.append("command-up"),
        on_command_cancel=lambda: called.append("command-cancel"),
    )

    class CanonicalListener:
        @staticmethod
        def canonical(key):
            return keyboard.Key.alt if key == keyboard.Key.alt_r else key

    hub._listener = CanonicalListener()
    hub._command_combo_key = keyboard.Key.alt
    hub._press(keyboard.Key.alt_r)
    hub._command_timer.cancel()
    hub._command_down_fire()
    for key in (keyboard.Key.ctrl, keyboard.KeyCode.from_char("1")):
        hub._press(key)
    hub._release(keyboard.Key.alt_r)

    assert called == ["command", "command-cancel", "polish"]


def _stub_input_monitoring(monkeypatch, granted):
    requests = []
    monkeypatch.setattr(hotkey_module.sys, "platform", "darwin")
    monkeypatch.setattr(hotkey_module, "_input_monitoring_granted", lambda: granted)
    monkeypatch.setattr(
        hotkey_module, "_request_input_monitoring", lambda: requests.append(True)
    )
    return requests


def test_denied_input_monitoring_requests_access_once_and_warns(monkeypatch, caplog, capsys):
    requests = _stub_input_monitoring(monkeypatch, granted=False)

    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        assert _hub()._preflight_input_monitoring() is False

    assert requests == [True]
    assert "Input Monitoring not granted" in caplog.text
    stderr = capsys.readouterr().err
    assert "Input Monitoring" in stderr
    assert "LocalFlow" in stderr
    assert "Restart" in stderr


def test_granted_input_monitoring_is_silent(monkeypatch, caplog, capsys):
    requests = _stub_input_monitoring(monkeypatch, granted=True)

    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        assert _hub()._preflight_input_monitoring() is True

    assert requests == []
    assert not [r for r in caplog.records if r.levelname == "WARNING"]
    assert capsys.readouterr().err == ""


def test_input_monitoring_preflight_skipped_off_macos(monkeypatch, caplog):
    requests = _stub_input_monitoring(monkeypatch, granted=False)
    monkeypatch.setattr(hotkey_module.sys, "platform", "linux")

    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        assert _hub()._preflight_input_monitoring() is True

    assert requests == []
    assert not [r for r in caplog.records if r.levelname == "WARNING"]


def test_undetermined_input_monitoring_is_not_treated_as_granted(monkeypatch, caplog):
    requests = _stub_input_monitoring(monkeypatch, granted=None)

    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        assert _hub()._preflight_input_monitoring() is False

    assert requests == []
    assert not [r for r in caplog.records if r.levelname == "WARNING"]


class _InstantListener:
    """Listener stub whose join() returns at once, so run_forever completes."""

    def __init__(self, on_press, on_release):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def join(self):
        pass

    def canonical(self, key):
        return key


class _RecordingTimer:
    created: ClassVar[list] = []

    def __init__(self, interval, function):
        self.interval = interval
        self.function = function
        self.started = False
        self.cancelled = False
        _RecordingTimer.created.append(self)

    def start(self):
        self.started = True

    def cancel(self):
        self.cancelled = True


@pytest.mark.parametrize(
    ("granted", "armed"), [(True, False), (False, True), (None, True)]
)
def test_dead_tap_watchdog_armed_only_without_confirmed_grant(monkeypatch, granted, armed):
    _stub_input_monitoring(monkeypatch, granted=granted)
    hub = _hub()
    _RecordingTimer.created = []
    monkeypatch.setattr(keyboard, "Listener", _InstantListener, raising=False)
    monkeypatch.setattr(hotkey_module, "threading", SimpleNamespace(Timer=_RecordingTimer))

    hub.run_forever()

    if armed:
        [timer] = _RecordingTimer.created
        assert timer.interval == hotkey_module._DEAD_TAP_SECONDS
        assert timer.function == hub._check_tap_alive
        assert timer.started and timer.cancelled
    else:
        assert _RecordingTimer.created == []


def test_dead_tap_watchdog_warns_without_events(caplog, capsys):
    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        _hub()._check_tap_alive()

    assert "no keyboard events received" in caplog.text
    assert "Input Monitoring" in capsys.readouterr().err


def test_dead_tap_watchdog_quiet_after_real_event(caplog, capsys):
    hub = _hub()
    hub._press(_KeyCode("a"))
    hub._release(_KeyCode("a"))

    with caplog.at_level("WARNING", logger="localflow.hotkey"):
        hub._check_tap_alive()

    assert not [r for r in caplog.records if r.levelname == "WARNING"]
    assert capsys.readouterr().err == ""
