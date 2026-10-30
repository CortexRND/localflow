import json
import os
import stat
import sys
from types import SimpleNamespace

import pytest

from localflow import media, status
from localflow.status import (
    live_state,
    read_last_text,
    title_prefix,
    write_last_text,
    write_state,
)


def test_status_state_is_atomic_payload_with_pid(tmp_path):
    path = tmp_path / "status.json"

    write_state("recording", path)

    data = json.loads(path.read_text())
    assert data["state"] == "recording"
    assert data["pid"] == os.getpid()
    assert isinstance(data["since"], float)
    assert live_state(path) == "recording"


def test_stale_status_pid_maps_to_idle(tmp_path):
    path = tmp_path / "status.json"
    path.write_text(json.dumps({"state": "locked", "since": 1, "pid": 999999999}))

    assert live_state(path) == "idle"


def test_posix_pid_check_uses_ps_not_signal_zero(monkeypatch):
    calls = []

    def run(args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(stdout="42\n")

    monkeypatch.setattr(status, "_IS_WINDOWS", False)
    monkeypatch.setattr(status.subprocess, "run", run)

    assert status._pid_is_alive(42)
    assert calls[0][0] == ["ps", "-p", "42", "-o", "pid="]


def test_windows_live_state_uses_safe_process_check(monkeypatch, tmp_path):
    path = tmp_path / "status.json"
    path.write_text(json.dumps({"state": "locked", "since": 1, "pid": 42}))
    checked = []

    monkeypatch.setattr(status, "_IS_WINDOWS", True)
    monkeypatch.setattr(
        status,
        "_windows_pid_is_alive",
        lambda pid: checked.append(pid) or False,
    )

    assert live_state(path) == "idle"
    assert checked == [42]


def test_windows_pid_check_opens_and_closes_process(monkeypatch):
    calls = []

    def open_process(access, inherit, pid):
        calls.append(("open", access, inherit, pid))
        return 123

    def close_handle(handle):
        calls.append(("close", handle))
        return True

    kernel32 = SimpleNamespace(
        OpenProcess=open_process,
        CloseHandle=close_handle,
    )
    monkeypatch.setattr(
        status.ctypes,
        "WinDLL",
        lambda *_args, **_kwargs: kernel32,
        raising=False,
    )

    assert status._windows_pid_is_alive(42)
    assert calls == [
        ("open", status._PROCESS_QUERY_LIMITED_INFORMATION, False, 42),
        ("close", 123),
    ]


def test_title_prefix_mapping_is_pure():
    assert title_prefix("recording") == "● "
    assert title_prefix("locked") == "🔒 "
    assert title_prefix("command") == "⌘ "
    assert title_prefix("processing") == "… "
    assert title_prefix("idle") == ""


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="Windows does not expose POSIX file mode bits",
)
def test_last_text_file_permissions_are_private(tmp_path):
    path = tmp_path / "last.txt"

    write_last_text("hello", path)

    assert path.read_text() == "hello"
    assert stat.S_IMODE(path.stat().st_mode) == 0o600


def test_read_last_text_returns_saved_text_and_empty_when_missing(tmp_path):
    path = tmp_path / "last.txt"

    assert read_last_text(path) == ""
    write_last_text("saved text", path)
    assert read_last_text(path) == "saved text"


def test_media_pause_and_resume_are_noops_off_macos(monkeypatch):
    monkeypatch.setattr(media.platform, "system", lambda: "Linux")
    monkeypatch.setattr(
        media.subprocess, "run",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(
            AssertionError("osascript must not run")
        ),
    )

    assert media.pause_playing() == set()
    media.resume({"Music"})


def test_media_only_resumes_players_it_paused(monkeypatch):
    monkeypatch.setattr(media.platform, "system", lambda: "Darwin")
    calls = []

    def run(args, **_kwargs):
        calls.append(args)
        return type("Result", (), {"stdout": "paused" if "Music" in args[-1] else "unchanged"})()

    monkeypatch.setattr(media.subprocess, "run", run)

    paused = media.pause_playing()
    media.resume(paused)

    assert paused == {"Music"}
    assert any("tell application \"Music\" to play" in str(call) for call in calls)
    assert not any("tell application \"Spotify\" to play" in str(call) for call in calls)
