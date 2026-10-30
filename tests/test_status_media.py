import json
import os
import stat

from localflow import media
from localflow.status import live_state, title_prefix, write_last_text, write_state


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


def test_title_prefix_mapping_is_pure():
    assert title_prefix("recording") == "● "
    assert title_prefix("locked") == "🔒 "
    assert title_prefix("command") == "⌘ "
    assert title_prefix("processing") == "… "
    assert title_prefix("idle") == ""


def test_last_text_file_permissions_are_private(tmp_path):
    path = tmp_path / "last.txt"

    write_last_text("hello", path)

    assert path.read_text() == "hello"
    assert stat.S_IMODE(path.stat().st_mode) == 0o600


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
