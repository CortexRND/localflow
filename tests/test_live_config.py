"""Live settings: reloaded config reaches running callbacks and bindings.

Proves behaviour, not just that load_config ran: a sound toggle changes what
the running PushToTalkController plays, and a shortcut change rebinds the
running KeyboardHub in place, deferred while a recording or key is active.
"""

import dataclasses
import queue
import threading
import time
from dataclasses import dataclass
from types import SimpleNamespace

import numpy as np
import pytest

from localflow import app as app_module
from localflow import hotkey as hotkey_module
from localflow import live_config, meetings
from localflow.config import Config, _config_from_v2, _parse, save_config
from localflow.pushtotalk import PushToTalkController


def _wait_until(predicate, timeout=2.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.005)
    return predicate()


# ------------------------------------------------------- fake pynput keys ---


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
                keys.append(getattr(_Key, part[1:-1]))
            else:
                keys.append(_KeyCode.from_char(part))
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
    ctrl = object()
    ctrl_r = object()
    esc = object()
    f19 = object()
    shift = object()


@pytest.fixture
def keys(monkeypatch):
    fake = SimpleNamespace(Key=_Key, KeyCode=_KeyCode, HotKey=_HotKey)
    monkeypatch.setattr(hotkey_module, "keyboard", fake)
    return fake


def _chord(hub, *keys):
    for key in keys:
        hub._press(key)
    for key in reversed(keys):
        hub._release(key)


class Controller:
    """Records what KeyboardHub calls, like PushToTalkController's note_*."""

    def __init__(self):
        self.events = []
        self.idle = True
        self.configs = []

    def __getattr__(self, name):
        if name.startswith("note_"):
            return lambda *args: self.events.append((name, *args))
        raise AttributeError(name)

    def update_config(self, config):
        self.configs.append(config)

    def is_idle(self):
        return self.idle


def _runtime(config, clock=time.monotonic):
    controller = Controller()
    hub = hotkey_module.KeyboardHub(
        **app_module.keyboard_bindings(config, controller),
        on_down=controller.note_down,
        on_up=controller.note_up,
        on_toggle=controller.note_toggle,
        on_cancel=controller.note_cancel,
        on_command_down=controller.note_command_down,
        on_command_up=controller.note_command_up,
        on_command_cancel=controller.note_command_cancel,
        can_reconfigure=controller.is_idle,
        clock=clock,
    )
    live = app_module.LiveSettings(config, controller, hub)
    return live, controller, hub


# --------------------------------------------------------- merge & reload ---


def test_merge_live_takes_only_allowlisted_fields():
    running = Config(sounds_enabled=True, server_port=8756, llm_model="a")
    fresh = Config(sounds_enabled=False, server_port=9999, llm_model="b",
                   paste_method="clipboard")
    merged, changed = live_config.merge_live(running, fresh)
    assert changed == {"sounds_enabled", "paste_method"}
    assert (merged.sounds_enabled, merged.paste_method) == (False, "clipboard")
    assert (merged.server_port, merged.llm_model) == (8756, "a")  # restart-only
    same, nothing = live_config.merge_live(merged, merged)
    assert same is merged and nothing == frozenset()


def test_watcher_notifies_once_per_change_and_survives_bad_files(tmp_path):
    path = tmp_path / "config.toml"
    save_config(Config(), path)
    seen = []
    watcher = live_config.ConfigWatcher(
        seen.append, path=path, loader=lambda: _config_from_v2(_parse(path)),
    )
    assert watcher.check() is False  # nothing changed since start
    save_config(Config(sounds_enabled=False), path)
    assert watcher.check() is True and seen[-1].sounds_enabled is False
    assert watcher.check() is False
    path.write_text("not = [valid toml")
    assert watcher.check() is False and len(seen) == 1  # half-written: ignored
    save_config(Config(sounds_enabled=True), path)
    assert watcher.check() is True and seen[-1].sounds_enabled is True


def test_watcher_thread_sleeps_between_checks(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    save_config(Config(), path)
    ticks, waits = [], []
    watcher = live_config.ConfigWatcher(
        lambda _c: None, path=path, loader=Config, interval=0.01,
        on_tick=lambda: ticks.append(None),
    )
    wait = watcher._stop.wait

    def record_wait(interval):
        waits.append(interval)
        return wait(interval)

    monkeypatch.setattr(watcher._stop, "wait", record_wait)
    watcher.start()
    try:
        assert _wait_until(lambda: len(ticks) >= 3)
    finally:
        watcher.stop()
        watcher._thread.join(timeout=2)
    assert not watcher._thread.is_alive()
    assert len(waits) >= len(ticks) >= 3
    assert waits == [watcher.interval] * len(waits)


def test_lazy_config_refreshes_live_fields_on_read(tmp_path):
    path = tmp_path / "config.toml"
    save_config(Config(), path)
    lazy = live_config.LazyConfig(
        Config(), path=lambda: path, loader=lambda: _config_from_v2(_parse(path)),
    )
    assert lazy.get().paste_method == "auto"
    save_config(Config(paste_method="clipboard", server_port=1), path)
    assert lazy.get().paste_method == "clipboard"
    assert lazy.get().server_port == 8756


# ----------------------------------------------------- running controller ---


class FakeRecorder:
    def __init__(self):
        self.recording = False

    def start(self):
        self.recording = True

    def stop(self):
        self.recording = False
        return np.zeros(16000, dtype=np.float32)


def test_sound_toggle_changes_what_the_running_controller_plays():
    played = []
    recorder = FakeRecorder()
    controller = PushToTalkController(
        recorder, Config(sounds_enabled=True), queue.Queue(),
        play_sound=played.append, min_clip_seconds=0.01,
    )
    live = app_module.LiveSettings(controller.config, controller)
    try:
        def cycle():
            controller.note_start()
            assert _wait_until(lambda: recorder.recording)
            time.sleep(0.02)
            controller.note_stop()
            assert _wait_until(lambda: not recorder.recording)
            assert _wait_until(controller.is_idle)

        cycle()
        assert played == ["start", "stop"]

        assert live.apply(Config(sounds_enabled=False)) == {"sounds_enabled"}
        assert _wait_until(lambda: controller.config.sounds_enabled is False)
        cycle()
        assert played == ["start", "stop"]  # silent now, no restart

        live.apply(Config(sounds_enabled=True))
        assert _wait_until(lambda: controller.config.sounds_enabled is True)
        cycle()
        assert played == ["start", "stop", "start", "stop"]
    finally:
        controller.shutdown()


def test_idle_controller_blocks_instead_of_polling():
    controller = PushToTalkController(FakeRecorder(), Config(), queue.Queue(),
                                      play_sound=lambda _cue: None)
    try:
        assert controller._timer_timeout() is None
        assert controller.is_idle()
    finally:
        controller.shutdown()


def test_config_swap_waits_for_events_already_queued():
    recorder = FakeRecorder()
    controller = PushToTalkController(recorder, Config(), queue.Queue(),
                                      play_sound=lambda _cue: None, min_clip_seconds=0.01)
    try:
        controller.note_start()
        controller.update_config(Config(paste_method="clipboard"))
        assert _wait_until(lambda: controller.config.paste_method == "clipboard")
        assert recorder.recording  # the earlier start was handled first
        assert not controller.is_idle()
    finally:
        controller.shutdown()


# ------------------------------------------------------------- live hotkeys ---


def test_combo_change_rebinds_the_running_hub(keys):
    live, controller, hub = _runtime(Config())
    _chord(hub, keys.Key.ctrl, keys.Key.alt, keys.KeyCode.from_char("1"))
    assert controller.events == [("note_transform", "polish")]

    changed = live.apply(Config(polish_hotkey="<ctrl>+<alt>+3", paste_last_hotkey=""))
    assert changed == {"polish_hotkey", "paste_last_hotkey"}
    assert not hub.reconfigure_pending
    controller.events.clear()
    _chord(hub, keys.Key.ctrl, keys.Key.alt, keys.KeyCode.from_char("1"))
    assert controller.events == []  # old shortcut gone
    _chord(hub, keys.Key.ctrl, keys.Key.alt, keys.KeyCode.from_char("3"))
    assert controller.events == [("note_transform", "polish")]
    assert controller.configs[-1].polish_hotkey == "<ctrl>+<alt>+3"


def test_dictation_key_change_takes_effect(keys):
    live, controller, hub = _runtime(Config(hotkey="alt_l"))
    live.apply(Config(hotkey="f19"))
    hub._press(keys.Key.alt_l)
    hub._release(keys.Key.alt_l)
    assert controller.events == []
    hub._press(keys.Key.f19)
    hub._release(keys.Key.f19)
    assert controller.events == [("note_down",), ("note_up",)]


def test_rebind_waits_while_recording(keys):
    live, controller, hub = _runtime(Config())
    controller.idle = False  # e.g. a locked (hands-free) recording
    live.apply(Config(hotkey="f19"))
    assert hub.reconfigure_pending
    hub._press(keys.Key.alt_l)  # the old key still ends what it started
    hub._release(keys.Key.alt_l)
    assert controller.events == [("note_down",), ("note_up",)]
    assert hub.reconfigure_pending

    controller.idle = True
    live.retry()  # the config watcher's tick
    assert not hub.reconfigure_pending
    controller.events.clear()
    hub._press(keys.Key.f19)
    hub._release(keys.Key.f19)
    assert controller.events == [("note_down",), ("note_up",)]


def test_rebind_never_orphans_a_held_key_release(keys):
    live, controller, hub = _runtime(Config(hotkey="alt_l"))
    hub._press(keys.Key.alt_l)          # recording starts with the old key
    live.apply(Config(hotkey="f19"))    # change lands mid-hold
    assert hub.reconfigure_pending
    hub._release(keys.Key.alt_l)        # release still reaches the controller...
    assert controller.events == [("note_down",), ("note_up",)]
    assert not hub.reconfigure_pending  # ...and the swap happens right after


def test_rebind_does_not_wait_forever_on_a_missed_release(keys):
    now = [100.0]
    live, _controller, hub = _runtime(Config(), clock=lambda: now[0])
    hub._press(keys.Key.shift)  # release never reported by pynput
    live.apply(Config(polish_hotkey="<ctrl>+<alt>+3"))
    assert hub.reconfigure_pending
    now[0] += hotkey_module.KeyboardHub.STALE_PRESS_SECONDS + 1
    assert hub.apply_pending()


def test_non_hotkey_changes_do_not_touch_bindings(keys, monkeypatch):
    live, controller, hub = _runtime(Config())
    calls = []
    monkeypatch.setattr(hub, "reconfigure", lambda **kw: calls.append(kw) or True)
    live.apply(Config(sounds_enabled=False, cleanup_level="medium"))
    assert calls == [] and live.config.cleanup_level == "medium"
    assert controller.configs[-1].sounds_enabled is False


# ------------------------------------------------------------------ server ---


def test_server_adopts_live_settings_and_toggles_meeting_watch(monkeypatch):
    from localflow import server

    class Watcher:
        def __init__(self):
            self.calls = []

        def start(self):
            self.calls.append("start")

        def stop(self):
            self.calls.append("stop")

    watcher = Watcher()
    monkeypatch.setattr(server, "_watcher", watcher)
    monkeypatch.setattr(
        server, "_config",
        Config(meetings_enabled=True, meeting_watch=True, server_port=8756),
    )
    server._apply_live_config(Config(
        meetings_enabled=True, meeting_watch=False, sounds_enabled=False,
        meeting_auto_start=False, server_port=1,
    ))
    assert watcher.calls == ["stop"]
    assert server._config.sounds_enabled is False
    assert server._config.meeting_auto_start is False
    assert server._config.server_port == 8756  # restart-only

    server._apply_live_config(dataclasses.replace(server._config, meeting_watch=True))
    assert watcher.calls == ["stop", "start"]


def test_server_reload_tracks_file_meetings_value_for_later_saves(monkeypatch, tmp_path):
    import tomllib

    from localflow import config as config_module
    from localflow import server

    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setattr(config_module, "legacy_config_path", lambda: tmp_path / "legacy.toml")
    monkeypatch.setattr(config_module.sys, "platform", "linux")
    path.write_text("[features]\nmeetings = true\n")
    monkeypatch.setattr(server, "_config", config_module.load_config())

    # Another process (`lf config set features.meetings false`) edits the file.
    path.write_text("[features]\nmeetings = false\n")
    server._apply_live_config(config_module.load_config())
    assert server._config.meetings_enabled is False

    server.api_config_update({"cleanup": {"style_work": "casual"}})
    written = tomllib.loads(path.read_text())
    assert written["cleanup"]["style_work"] == "casual"
    assert written["features"]["meetings"] is False


def test_meeting_watcher_stops_and_restarts_with_one_thread(monkeypatch):
    class Probe:
        def mic_in_use(self):
            return False

    monkeypatch.setattr(meetings, "MicUsageProbe", Probe)
    watcher = meetings.MeetingWatcher(poll_seconds=0.01)
    watcher.start()
    first = watcher._thread
    watcher.start()  # idempotent
    assert watcher._thread is first
    watcher.stop()
    assert _wait_until(lambda: watcher._thread is None)
    assert _wait_until(lambda: not first.is_alive())
    watcher.start()
    second = watcher._thread
    assert second is not None and second is not first
    alive = [t for t in threading.enumerate() if t in (first, second) and t.is_alive()]
    assert alive == [second]
    watcher.stop()
    assert _wait_until(lambda: not second.is_alive())
