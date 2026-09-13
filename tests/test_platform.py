import base64
import os
import re
import sys
from pathlib import Path
from types import ModuleType

import pytest

from localflow.platform import current, reset
from localflow.platform.darwin import DarwinPlatform
from localflow.platform.linux import LinuxPlatform
from localflow.platform.win32 import Win32Platform


@pytest.fixture(autouse=True)
def clear_platform(monkeypatch):
    monkeypatch.delenv("LOCALFLOW_PLATFORM", raising=False)
    reset()
    yield
    reset()


def test_platform_override_selects_each_backend(monkeypatch):
    for name, expected in (
        ("darwin", DarwinPlatform),
        ("win32", Win32Platform),
        ("linux", LinuxPlatform),
    ):
        monkeypatch.setenv("LOCALFLOW_PLATFORM", name)
        reset()
        assert isinstance(current(), expected)


def test_platform_default_is_linux(monkeypatch):
    monkeypatch.setattr("localflow.platform.sys.platform", "linux")

    assert isinstance(current(), LinuxPlatform)


def test_platform_unknown_override_raises(monkeypatch):
    monkeypatch.setenv("LOCALFLOW_PLATFORM", "plan9")

    with pytest.raises(ValueError, match="unknown platform"):
        current()


def test_linux_paste_auto_prefers_wtype_on_wayland(monkeypatch):
    calls = []
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setattr(
        "localflow.platform.linux.shutil.which",
        lambda name: "/usr/bin/wtype" if name == "wtype" else None,
    )
    monkeypatch.setattr(
        "localflow.platform.linux.subprocess.run",
        lambda argv, **kwargs: calls.append(argv),
    )

    LinuxPlatform().paste()

    assert calls == [["wtype", "-M", "ctrl", "v", "-m", "ctrl"]]


def test_linux_paste_auto_uses_xdotool(monkeypatch):
    calls = []
    monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
    monkeypatch.setattr(
        "localflow.platform.linux.shutil.which",
        lambda name: "/usr/bin/xdotool" if name == "xdotool" else None,
    )
    monkeypatch.setattr(
        "localflow.platform.linux.subprocess.run",
        lambda argv, **kwargs: calls.append(argv),
    )

    LinuxPlatform().paste()

    assert calls == [["xdotool", "key", "--clearmodifiers", "ctrl+v"]]


def test_linux_paste_pynput_fallback(monkeypatch):
    events = []

    class Key:
        ctrl = "ctrl"

    class Controller:
        def pressed(self, key):
            events.append(("pressed", key))
            return self

        def __enter__(self):
            return self

        def __exit__(self, *args):
            events.append(("released",))

        def press(self, key):
            events.append(("press", key))

        def release(self, key):
            events.append(("release", key))

    keyboard = ModuleType("pynput.keyboard")
    keyboard.Controller = Controller
    keyboard.Key = Key
    pynput = ModuleType("pynput")
    pynput.keyboard = keyboard
    monkeypatch.setitem(sys.modules, "pynput", pynput)
    monkeypatch.setitem(sys.modules, "pynput.keyboard", keyboard)
    monkeypatch.setattr("localflow.platform.linux.shutil.which", lambda name: None)

    LinuxPlatform().paste("pynput")

    assert events == [
        ("pressed", "ctrl"),
        ("press", "v"),
        ("release", "v"),
        ("released",),
    ]


def test_linux_paste_unknown_method():
    with pytest.raises(ValueError, match="unknown paste method"):
        LinuxPlatform().paste("bad")


def test_linux_autostart_round_trip(monkeypatch, tmp_path):
    monkeypatch.setattr(
        "localflow.platform.linux.platformdirs.user_config_dir",
        lambda: str(tmp_path / "config"),
    )
    platform = LinuxPlatform()

    path = platform.autostart_install("/opt/local flow/$app")

    assert path == tmp_path / "config" / "autostart" / "localflow.desktop"
    assert 'Exec="/opt/local flow/\\$app"' in path.read_text()
    assert platform.autostart_status() == f"installed ({path})"
    platform.autostart_uninstall()
    assert platform.autostart_status() == "not installed"


def test_linux_try_lock_second_fd_fails(tmp_path):
    path = tmp_path / "lock"
    first = os.open(path, os.O_CREAT | os.O_RDWR)
    second = os.open(path, os.O_CREAT | os.O_RDWR)
    try:
        platform = LinuxPlatform()
        assert platform.try_lock(first) is True
        assert platform.try_lock(second) is False
        platform.unlock(first)
    finally:
        os.close(first)
        os.close(second)


def test_linux_sound_does_not_use_aplay(monkeypatch):
    players = []
    calls = []
    monkeypatch.setattr("localflow.platform.linux.Path.exists", lambda path: True)

    def which(name):
        players.append(name)
        return "/usr/bin/aplay" if name == "aplay" else None

    monkeypatch.setattr("localflow.platform.linux.shutil.which", which)
    monkeypatch.setattr(
        "localflow.platform.linux.subprocess.Popen",
        lambda argv, **kwargs: calls.append(argv),
    )

    LinuxPlatform().play_sound("start")

    assert players == ["paplay", "pw-play"]
    assert calls == []


def test_windows_autostart_and_lock(monkeypatch):
    values = {}

    class Key:
        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

    winreg = ModuleType("winreg")
    winreg.HKEY_CURRENT_USER = object()
    winreg.KEY_READ = 1
    winreg.KEY_SET_VALUE = 2
    winreg.REG_SZ = 1
    winreg.OpenKey = lambda *args: Key()
    winreg.SetValueEx = lambda key, name, reserved, kind, value: values.__setitem__(
        name, value
    )
    def query_value(key, name):
        if name not in values:
            raise FileNotFoundError(name)
        return values[name], winreg.REG_SZ

    winreg.QueryValueEx = query_value
    winreg.DeleteValue = lambda key, name: values.pop(name)
    monkeypatch.setitem(sys.modules, "winreg", winreg)

    locks = []
    msvcrt = ModuleType("msvcrt")
    msvcrt.LK_NBLCK = 1
    msvcrt.LK_UNLCK = 2

    def locking(fd, mode, length):
        if mode == msvcrt.LK_NBLCK and locks:
            raise OSError("locked")
        if mode == msvcrt.LK_NBLCK:
            locks.append(fd)

    msvcrt.locking = locking
    monkeypatch.setitem(sys.modules, "msvcrt", msvcrt)

    platform = Win32Platform()
    assert platform.autostart_install("C:\\localflow.exe") == Path("C:\\localflow.exe")
    assert platform.autostart_status() == 'installed ("C:\\localflow.exe")'
    platform.autostart_uninstall()
    assert platform.autostart_status() == "not installed"
    assert platform.try_lock(3) is True
    assert platform.try_lock(4) is False


def test_windows_notify_escapes_xml(monkeypatch):
    calls = []
    monkeypatch.setattr(
        "localflow.platform.win32.subprocess.Popen",
        lambda argv, **kwargs: calls.append(argv),
    )

    Win32Platform().notify("O'Reilly $(", "<message>")

    assert calls
    script = calls[0][-1]
    assert "O'Reilly $(" not in script
    encoded = re.search(r"FromBase64String\('([^']+)'\)", script).group(1)
    xml = base64.b64decode(encoded).decode("utf-8")
    assert "<text id=\"1\">O&apos;Reilly $(</text>" in xml
    assert "<text id=\"2\">&lt;message&gt;</text>" in xml


def test_platform_paste_methods_are_validated():
    with pytest.raises(ValueError, match="unknown paste method"):
        DarwinPlatform().paste("pynput")
    with pytest.raises(ValueError, match="unknown paste method"):
        Win32Platform().paste("osascript")


def test_darwin_autostart_delegates(monkeypatch, tmp_path):
    monkeypatch.setattr(
        "localflow.launchagent.install",
        lambda program=None: tmp_path / (program or "default"),
    )
    monkeypatch.setattr("localflow.launchagent.uninstall", lambda: None)
    monkeypatch.setattr("localflow.launchagent.status", lambda: "running")
    platform = DarwinPlatform()

    assert platform.autostart_install("localflow") == tmp_path / "localflow"
    platform.autostart_uninstall()
    assert platform.autostart_status() == "running"


def test_darwin_notify_uses_display_notification(monkeypatch):
    calls = []
    monkeypatch.setattr(
        "localflow.platform.darwin.subprocess.run",
        lambda argv, **kwargs: calls.append(argv),
    )

    DarwinPlatform().notify("Title", 'Message "quoted"')

    assert calls[0][:2] == ["osascript", "-e"]
    assert "display notification" in calls[0][2]
    assert '\\"quoted\\"' in calls[0][2]


def test_inject_facade_passes_paste_method(monkeypatch):
    from localflow import inject

    calls = []
    clipboard = []

    class FakePlatform:
        def paste(self, method):
            calls.append(method)

    monkeypatch.setattr(inject, "current", lambda: FakePlatform())
    monkeypatch.setattr(inject.pyperclip, "paste", lambda: "old")
    monkeypatch.setattr(inject.pyperclip, "copy", clipboard.append)
    monkeypatch.setattr(inject.time, "sleep", lambda seconds: None)

    inject.paste_text("text", "xdotool")

    assert calls == ["xdotool"]
    assert clipboard == ["text", "old"]


def test_inject_clipboard_method_keeps_text_without_keystroke(monkeypatch):
    from localflow import inject

    clipboard = []

    class FakePlatform:
        def paste(self, method):
            raise AssertionError("clipboard mode must not send a keystroke")

    monkeypatch.setattr(inject, "current", lambda: FakePlatform())
    monkeypatch.setattr(inject.pyperclip, "copy", clipboard.append)

    inject.paste_text("transcript", "clipboard")

    assert clipboard == ["transcript"]


def test_cli_rejects_unknown_paste_method():
    from click.testing import CliRunner

    import localflow.cli as cli_module

    result = CliRunner().invoke(
        cli_module.cli,
        ["config", "set", "paste.method", "bad"],
    )

    assert result.exit_code != 0
    assert "invalid paste method" in result.output
