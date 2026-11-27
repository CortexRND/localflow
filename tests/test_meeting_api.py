import json
import logging
import os
import re
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path
from typing import ClassVar

import numpy as np
import pytest
from fastapi.testclient import TestClient

from localflow import meetings, server
from localflow.config import Config, save_config
from localflow.diarization import collector as collector_module
from localflow.diarization import factory as factory_module
from localflow.diarization.collector import DiarizationCollector, DiarizationResult
from localflow.diarization.mock import MockDiarizer, MockEmbedder, tone_index
from localflow.diarization.store import MeetingSpeakerStore
from localflow.diarization.types import TimedText
from localflow.meetings import (
    ENHANCE_INSTRUCTION,
    MeetingSummarizer,
    ObsidianWriter,
    Segment,
)


class FakeLLM:
    id = "fake"
    offline = True

    def __init__(self, reply="answer"):
        self.reply = reply
        self.prompts: list[str] = []

    def complete(self, system, user, *, max_tokens=None):
        self.prompts.append(user)
        if isinstance(self.reply, Exception):
            raise self.reply
        return self.reply


class FakeSession:
    instances: ClassVar[list["FakeSession"]] = []

    def __init__(self, transcriber, lock, sample_rate=16000, chunk_seconds=30,
                 system_audio=None):
        self.system_audio = system_audio
        self.system_audio_error = ""
        self.active = False
        self.started_at = datetime(2026, 11, 14, 10, 0)
        self.segments = [
            Segment(0, "we will launch on March 3", "me"),
            Segment(12, "sounds good, Dana owns the blog post", "others"),
        ]
        FakeSession.instances.append(self)

    @property
    def system_audio_status(self):
        if self.system_audio is not None:
            return "on"
        return "error" if self.system_audio_error else "off"

    def start(self):
        self.active = True

    def elapsed_seconds(self):
        return 90.0

    def stop(self):
        self.active = False
        return self.segments


@pytest.fixture
def client(monkeypatch, tmp_path, tmp_path_factory):
    templates = tmp_path / "templates"
    templates.mkdir()
    (templates / "retro.md").write_text("---\nname: Retro\n---\n## Went Well\nBullets.\n")
    config = Config(
        meetings_enabled=False, meeting_watch=False, sounds_enabled=False,
        work_prompts=False, vault_path=str(tmp_path / "vault"),
        meeting_templates_dir=str(templates),
    )
    llm = FakeLLM("## Summary\nnotes")
    # Meeting starts re-read the speaker flags; never from the developer's own file.
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setattr(server, "_config", config)
    monkeypatch.setattr(server, "_summarizer", MeetingSummarizer(llm))
    monkeypatch.setattr(server, "_writer", ObsidianWriter(
        config.vault_path, config.notes_folder, config.logs_folder))
    monkeypatch.setattr(server, "MeetingSession", FakeSession)
    monkeypatch.setattr(server, "_get_transcriber", lambda: object())
    monkeypatch.setattr(server, "_session", None)
    monkeypatch.setattr(server, "_session_meta", {})
    monkeypatch.setattr(server, "_session_state", "idle")
    monkeypatch.setattr(server, "_last_saved", None)
    FakeSession.instances = []
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    sidecars = tmp_path_factory.mktemp("sidecars") / "meetings"
    monkeypatch.setattr(server, "_meeting_store", lambda: MeetingSpeakerStore(sidecars))
    staging_root = tmp_path_factory.mktemp("staging") / "speakers"  # never the real home
    monkeypatch.setattr(server, "_enrollment_staging_root", lambda: staging_root)
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234),
        headers={"X-Localflow-Token": "test-token"},
    ) as c:
        c.llm = llm
        c.tmp = tmp_path
        c.sidecars = sidecars
        yield c


def test_templates_endpoint_lists_builtins_and_user_templates(client):
    data = client.get("/meeting/templates").json()
    ids = [t["id"] for t in data["templates"]]
    assert {"general", "standup", "one-on-one", "retro"} <= set(ids)
    assert data["default"] == "general"
    one_on_one = next(t for t in data["templates"] if t["id"] == "one-on-one")
    assert one_on_one["name"] == "1:1" and "Commitments" in one_on_one["sections"]


def test_start_rejects_unknown_template_without_claiming_the_slot(client):
    resp = client.post("/meeting/start", json={"template": "nope"})
    assert resp.status_code == 400
    assert server._session_state == "idle"


def test_notes_require_a_running_session(client):
    assert client.post("/meeting/notes", json={"notes": "x"}).status_code == 409


def test_full_meeting_with_notes_template_and_speakers(client):
    started = client.post("/meeting/start", json={
        "title": "Launch sync", "template": "standup", "user_notes": "- launch",
    })
    assert started.status_code == 200
    assert started.json()["template"] == "standup"
    assert started.json()["system_audio"] == "off"

    saved = client.post("/meeting/notes", json={"notes": "## Launch\n- **date?**\n- blog"})
    assert saved.json() == {"ok": True, "chars": 28}
    status = client.get("/meeting/status").json()
    assert status["session"]["user_notes"] == "## Launch\n- **date?**\n- blog"
    assert status["session"]["template"] == "standup"
    assert [s["source"] for s in status["session"]["segments"]] == ["me", "others"]

    result = client.post("/meeting/stop")
    assert result.status_code == 200
    body = result.json()
    assert body["template"] == "standup" and body["enhanced"] is True
    prompt = client.llm.prompts[0]
    assert ENHANCE_INSTRUCTION in prompt
    assert "## Launch\n- **date?**\n- blog" in prompt
    assert "## Yesterday" in prompt
    assert "[00:12] Others: sounds good" in prompt
    log_text = Path(body["log_path"]).read_text()
    assert "**Others:**" in log_text and "## My raw notes" in log_text
    assert client.get("/meeting/status").json()["last_saved"]["notes_path"] == body["notes_path"]


def test_stop_without_notes_uses_plain_template_prompt(client):
    client.post("/meeting/start", json={"template": "retro"})
    body = client.post("/meeting/stop").json()
    assert body["enhanced"] is False
    assert "## Went Well" in client.llm.prompts[0]
    assert ENHANCE_INSTRUCTION not in client.llm.prompts[0]


def test_start_defaults_to_configured_template(client, monkeypatch):
    monkeypatch.setattr(server._config, "meeting_default_template", "one-on-one")
    assert client.post("/meeting/start", json={}).json()["template"] == "one-on-one"
    client.post("/meeting/stop")
    assert "## Commitments" in client.llm.prompts[0]


def test_system_audio_failure_is_reported_not_fatal(client, monkeypatch):
    monkeypatch.setattr(server._config, "capture_system_audio", True)

    def boom(rate, device):
        raise RuntimeError("Screen Recording permission denied")

    monkeypatch.setattr(server, "open_system_audio", boom)
    resp = client.post("/meeting/start", json={})
    assert resp.status_code == 200
    assert resp.json()["system_audio"] == "error"
    assert "permission" in client.get("/meeting/status").json()["session"]["system_audio_error"]


def test_system_audio_source_is_passed_to_the_session(client, monkeypatch):
    monkeypatch.setattr(server._config, "capture_system_audio", True)
    monkeypatch.setattr(server._config, "system_audio_device", "BlackHole")
    seen = {}
    source = object()

    def fake_open(rate, device):
        seen["device"] = device
        return source

    monkeypatch.setattr(server, "open_system_audio", fake_open)
    assert client.post("/meeting/start", json={}).json()["system_audio"] == "on"
    assert seen["device"] == "BlackHole"
    assert FakeSession.instances[-1].system_audio is source


def test_chat_404_before_any_saved_meeting(client):
    resp = client.post("/meeting/chat", json={"question": "what happened?"})
    assert resp.status_code == 404


def test_chat_validates_question(client):
    assert client.post("/meeting/chat", json={"question": "  "}).status_code == 400
    assert client.post("/meeting/chat", json={"question": "x" * 2001}).status_code == 413


def test_chat_answers_from_last_saved_transcript(client):
    client.post("/meeting/start", json={"title": "Launch"})
    log_path = client.post("/meeting/stop").json()["log_path"]
    client.llm.reply = "March 3 [00:00]."
    resp = client.post("/meeting/chat", json={"question": "When do we launch?"})
    assert resp.status_code == 200
    assert resp.json() == {"answer": "March 3 [00:00].", "log_path": log_path}
    prompt = client.llm.prompts[-1]
    assert "Question: When do we launch?" in prompt
    assert "we will launch on March 3" in prompt
    assert "Notes: [[" not in prompt and "duration_minutes" not in prompt


def test_chat_falls_back_to_newest_log_on_disk_after_restart(client, monkeypatch):
    client.post("/meeting/start", json={})
    log_path = client.post("/meeting/stop").json()["log_path"]
    monkeypatch.setattr(server, "_last_saved", None)
    assert client.post("/meeting/chat", json={"question": "q"}).json()["log_path"] == log_path


def test_chat_surfaces_llm_errors(client):
    client.post("/meeting/start", json={})
    client.post("/meeting/stop")
    client.llm.reply = RuntimeError("ollama not running")
    resp = client.post("/meeting/chat", json={"question": "q"})
    assert resp.status_code == 502
    assert "ollama not running" in resp.json()["detail"]


def test_meeting_routes_require_token_off_loopback(client, monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    with TestClient(
        server.app, base_url="http://10.0.0.5:8756", client=("10.0.0.5", 1234)
    ) as remote:
        assert remote.post("/meeting/chat", json={"question": "q"}).status_code == 403
        assert remote.post("/meeting/notes", json={"notes": "x"}).status_code == 403
        assert remote.get(
            "/meeting/status", headers={"X-Localflow-Token": "test-token"}
        ).status_code == 200


@pytest.mark.parametrize("method,path", [
    ("GET", "/meeting/status"),
    ("GET", "/meeting/templates"),
    ("POST", "/meeting/start"),
    ("POST", "/meeting/stop"),
    ("POST", "/meeting/dismiss"),
])
@pytest.mark.parametrize("origin", [None, "null", "http://127.0.0.1:8756"])
def test_loopback_meeting_routes_require_token(client, method, path, origin):
    headers = {"X-Localflow-Token": ""}
    if origin is not None:
        headers["Origin"] = origin
    resp = client.request(method, path, headers=headers, json={})
    assert resp.status_code == 401
    assert FakeSession.instances == []


def test_meeting_title_is_bounded(client):
    assert client.post("/meeting/start", json={"title": "x" * 201}).status_code == 422


def test_status_reports_chat_available_from_disk_after_restart(client):
    assert client.get("/meeting/status").json()["chat_available"] is False
    cfg = server._config
    ObsidianWriter(cfg.vault_path, cfg.notes_folder, cfg.logs_folder).write(
        title="Old", category="Zoom", started_at=datetime(2026, 11, 13, 9, 0),
        duration_seconds=60, notes_md="ok", segments=[Segment(0, "hi")],
    )
    assert client.get("/meeting/status").json()["chat_available"] is True


# ------------------------------------------------- diarization (slice B5) ---

def _flag_off_meeting(client):
    """Start, read status, stop a FakeSession meeting; (start, status, stop) JSON."""
    start = client.post("/meeting/start", json={"title": "Launch sync"})
    assert start.status_code == 200
    status = client.get("/meeting/status").json()
    stop = client.post("/meeting/stop")
    assert stop.status_code == 200
    return start.json(), status, stop.json()


# Captured on the tree before slice B5.
_FLAG_OFF_FRONT = (
    b'---\ntitle: "Launch sync"\ndate: 2026-11-14T10:00\nplatform: Other\n'
    b"duration_minutes: 1\ntemplate: general\ntags: [meeting, localflow]\n---\n\n"
)
_FLAG_OFF_LOG = _FLAG_OFF_FRONT + (
    b"# Launch sync \xe2\x80\x94 transcript\n\n"
    b"**00:00** **Me:** we will launch on March 3\n\n"
    b"**00:12** **Others:** sounds good, Dana owns the blog post\n\n"
    b"Notes: [[2026-11-14 1000 Launch sync]]\n"
)
_FLAG_OFF_NOTES = _FLAG_OFF_FRONT + (
    b"# Launch sync\n\n## Summary\nnotes\n\n"
    b"Raw transcript: [[2026-11-14 1000 Launch sync (transcript)]]\n"
)


def test_diarization_off_markdown_is_byte_identical(client):
    start, status, stop = _flag_off_meeting(client)
    # Text with universal newlines: write_text emits CRLF on Windows.
    notes = Path(stop["notes_path"]).read_text(encoding="utf-8")
    log = Path(stop["log_path"]).read_text(encoding="utf-8")
    assert log == _FLAG_OFF_LOG.decode("utf-8")
    assert notes == _FLAG_OFF_NOTES.decode("utf-8")
    for text in (notes, log):
        assert "meeting_id" not in text and "speakers:" not in text
    assert all(set(s) == {"stamp", "text", "source"} for s in status["session"]["segments"])
    assert (start["diarization"], start["diarization_error"]) == ("off", "")
    assert status["speaker_diarization"] is False
    assert status["session"]["diarization"] == "off"
    assert status["session"]["diarization_error"] == ""
    assert (stop["diarization"], stop["speakers"], stop["meeting_id"]) == ("off", [], None)


def _diar_threads():
    return [t for t in threading.enumerate()
            if t.name.startswith("localflow-diarization") and t.is_alive()]


def test_diarization_off_does_no_diarization_work(client, monkeypatch):
    monkeypatch.setitem(sys.modules, "localflow.diarization.factory", None)
    monkeypatch.setitem(sys.modules, "localflow.diarization.collector", None)
    start, _, stop = _flag_off_meeting(client)
    assert (start["diarization"], start["diarization_error"]) == ("off", "")
    assert stop["diarization"] == "off"
    assert not hasattr(FakeSession.instances[-1], "diarization")
    assert not hasattr(FakeSession.instances[-1], "diarization_error")
    assert _diar_threads() == []


RATE = 16000


def tone(i, seconds):
    return np.full(int(RATE * seconds), 0.1 + i * 0.001, dtype=np.float32)


class ToneTimedTranscriber:
    """One TimedText per tone run, text 'line <tone index>'."""

    def transcribe_timed(self, audio):
        return [
            TimedText(t.start, t.end,
                      f"line {tone_index(audio[int(t.start * RATE):int(t.end * RATE)])}")
            for t in MockDiarizer().diarize(audio, RATE)
        ]

    def transcribe(self, audio):
        return " ".join(t.text for t in self.transcribe_timed(audio))


class FakeStream:
    def __init__(self, **kwargs):
        self.callback = kwargs["callback"]

    def start(self):
        pass

    def stop(self):
        pass

    def close(self):
        pass


class FixedDatetime(datetime):
    @classmethod
    def now(cls, tz=None):
        return cls(2026, 11, 14, 10, 0)


@pytest.fixture
def real_session(client, monkeypatch):
    """`client` with the real MeetingSession, a fake mic and the tone transcriber."""
    monkeypatch.setattr(server, "MeetingSession", meetings.MeetingSession)
    monkeypatch.setattr(meetings.sd, "InputStream", FakeStream)
    monkeypatch.setattr(meetings, "datetime", FixedDatetime)
    monkeypatch.setattr(server, "_get_transcriber", lambda: ToneTimedTranscriber())
    return client


@pytest.fixture
def diar(real_session, monkeypatch):
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(server._config, "diarization_backend", "mock")
    yield real_session
    end = time.monotonic() + 5
    while _diar_threads() and time.monotonic() < end:
        time.sleep(0.01)
    assert _diar_threads() == []


def _push(others=None, mic=None):
    session = server._session
    if mic is not None:
        session._mic.append(mic)
    if others is not None:
        session._others.append(others)
    session._drain_once()


def _front(path):
    text = Path(path).read_text(encoding="utf-8")
    lines = text[4:text.index("\n---\n", 4)].split("\n")
    pairs = (line.partition(": ")[::2] for line in lines)
    return {k: json.loads(v) if k in ("speakers", "meeting_id") else v for k, v in pairs}


def _three_speaker_api_meeting(client):
    start = client.post("/meeting/start", json={"title": "Launch sync"})
    assert start.status_code == 200
    _push(np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4), tone(0, 2)]), mic=tone(5, 2))
    status = client.get("/meeting/status").json()
    _push(np.concatenate([tone(2, 3), tone(1, 3)]))
    stop = client.post("/meeting/stop")
    assert stop.status_code == 200
    return start.json(), status, stop.json()


def test_three_speaker_mock_meeting(diar):
    start, status, stop = _three_speaker_api_meeting(diar)
    assert (start["diarization"], start["diarization_error"]) == ("collecting", "")
    assert status["speaker_diarization"] is True
    live = status["session"]
    assert (live["diarization"], live["diarization_error"]) == ("collecting", "")
    assert [s["source"] for s in live["segments"]] == ["me", "others"]
    assert all(set(s) == {"stamp", "text", "source"} for s in live["segments"])

    assert stop["diarization"] == "ok"
    assert [s["id"] for s in stop["speakers"]] == ["S1", "S2", "S3"]
    assert [s["label"] for s in stop["speakers"]] == ["Speaker 1", "Speaker 2", "Speaker 3"]
    assert all(set(s) == {"id", "label", "name", "source", "seconds"} for s in stop["speakers"])
    assert [(s["source"], s["name"], s["seconds"]) for s in stop["speakers"]] == [
        ("others", None, 6.0), ("others", None, 7.0), ("others", None, 7.0),
    ]
    log_text = Path(stop["log_path"]).read_text(encoding="utf-8")
    for n in (1, 2, 3):
        assert f"**Speaker {n}:**" in log_text
    assert "**Others:**" not in log_text
    assert re.fullmatch(r"[0-9a-f]{32}", stop["meeting_id"])
    assert _front(stop["log_path"])["meeting_id"] == stop["meeting_id"]
    assert _front(stop["notes_path"])["meeting_id"] == stop["meeting_id"]
    assert stop["segments"] == 7 == len(re.findall(r"(?m)^\*\*\d\d:\d\d\*\* ", log_text))
    assert diar.get("/meeting/status").json()["last_saved"] == stop


def test_single_remote_speaker_matches_flag_off(real_session, monkeypatch):
    def meeting(vault):
        cfg = server._config
        monkeypatch.setattr(server, "_writer", ObsidianWriter(
            str(vault), cfg.notes_folder, cfg.logs_folder))
        assert real_session.post("/meeting/start", json={"title": "Solo"}).status_code == 200
        _push(tone(0, 4), mic=tone(5, 2))
        stop = real_session.post("/meeting/stop").json()
        return stop, Path(stop["notes_path"]).read_bytes(), Path(stop["log_path"]).read_bytes()

    off, off_notes, off_log = meeting(real_session.tmp / "vault-off")
    assert off["diarization"] == "off"
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(server._config, "diarization_backend", "mock")
    on, on_notes, on_log = meeting(real_session.tmp / "vault-on")
    assert (on["diarization"], on["speakers"], on["meeting_id"]) == ("ok", [], None)
    assert (on_notes, on_log) == (off_notes, off_log)
    assert b"**Others:** line 0" in on_log


def test_sherpa_backend_is_reported_unavailable(client, monkeypatch):
    monkeypatch.setitem(sys.modules, "sherpa_onnx", None)
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    start = client.post("/meeting/start", json={"title": "Launch sync"})
    assert start.status_code == 200
    body = start.json()
    assert body["diarization"] == "unavailable"
    assert "localflow[diarization]" in body["diarization_error"]
    live = client.get("/meeting/status").json()["session"]
    assert (live["diarization"], live["diarization_error"]) == (
        "unavailable", body["diarization_error"])
    stop = client.post("/meeting/stop")
    assert stop.status_code == 200 and stop.json()["diarization"] == "off"
    assert Path(stop.json()["log_path"]).is_file()


def test_factory_raising_is_reported_unavailable(client, monkeypatch):
    def boom(config):
        raise RuntimeError("factory broke")

    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(factory_module, "build_diarization", boom)
    start = client.post("/meeting/start", json={})
    assert start.status_code == 200
    assert (start.json()["diarization"], start.json()["diarization_error"]) == (
        "unavailable", "factory broke")
    assert client.post("/meeting/stop").json()["diarization"] == "off"


class BlockingDiarizer(MockDiarizer):
    def __init__(self):
        super().__init__()
        self.entered = threading.Event()
        self.release = threading.Event()

    def diarize(self, audio, sample_rate):
        self.entered.set()
        self.release.wait(10)
        return super().diarize(audio, sample_rate)


def _use_collector(monkeypatch, collector):
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(factory_module, "build_diarization", lambda config: (collector, ""))


def test_finalize_timeout_keeps_channel_labels(real_session, monkeypatch):
    diarizer = BlockingDiarizer()
    collector = DiarizationCollector(diarizer, MockEmbedder())
    _use_collector(monkeypatch, collector)
    monkeypatch.setattr(collector_module, "DIAR_FINALIZE_TIMEOUT_S", 0.2)
    try:
        real_session.post("/meeting/start", json={"title": "Launch sync"})
        _push(np.concatenate([tone(0, 2), tone(1, 2)]))
        assert diarizer.entered.wait(5)
        t0 = time.monotonic()
        stop = real_session.post("/meeting/stop")
        assert time.monotonic() - t0 < 5
    finally:
        diarizer.release.set()
    body = stop.json()
    assert stop.status_code == 200
    assert (body["diarization"], body["speakers"], body["meeting_id"]) == ("timeout", [], None)
    assert "**00:00** **Others:** line 0 line 1" in Path(body["log_path"]).read_text()


class ScriptedCollector:
    """Stands in for DiarizationCollector: accepts feeds, finalize is scripted."""

    error = ""

    def __init__(self, finalize):
        self._finalize = finalize

    def feed(self, source, offset, audio, timed):
        return True

    def finalize(self, segments, *, timeout=None):
        return self._finalize(segments)


def test_failed_capture_shutdown_closes_the_collector(real_session, monkeypatch):
    class ClosingCollector(ScriptedCollector):
        closed = 0

        def close(self):
            self.closed += 1

    def no_finalize(segments):
        raise AssertionError("finalize must not run when stop failed")

    collector = ClosingCollector(no_finalize)
    _use_collector(monkeypatch, collector)
    real_session.post("/meeting/start", json={"title": "Launch sync"})

    def broken_stop(self):
        raise RuntimeError("audio device vanished")

    monkeypatch.setattr(meetings.MeetingSession, "stop", broken_stop)
    with pytest.raises(RuntimeError, match="audio device vanished"):
        real_session.post("/meeting/stop")
    assert collector.closed == 1
    assert server._session_state == "idle"


def test_finalize_raising_is_failed(real_session, monkeypatch):
    def boom(segments):
        raise RuntimeError("finalize broke")

    _use_collector(monkeypatch, ScriptedCollector(boom))
    real_session.post("/meeting/start", json={"title": "Launch sync"})
    _push(np.concatenate([tone(0, 2), tone(1, 2)]))
    stop = real_session.post("/meeting/stop")
    assert stop.status_code == 200
    body = stop.json()
    assert (body["diarization"], body["speakers"], body["meeting_id"]) == ("failed", [], None)
    assert "**00:00** **Others:** line 0 line 1" in Path(body["log_path"]).read_text()


@pytest.mark.parametrize("status, expected", [("channel", "channel"), ("weird", "failed")])
def test_non_ok_finalize_keeps_original_segments(real_session, monkeypatch, status, expected):
    def relabel(segments):
        # A non-ok result's segments are ignored: the originals are saved.
        bogus = [Segment(s.offset_seconds, "bogus", s.source, None, "S1", "Speaker 1")
                 for s in segments]
        return DiarizationResult(status, bogus, [])

    _use_collector(monkeypatch, ScriptedCollector(relabel))
    real_session.post("/meeting/start", json={"title": "Launch sync"})
    _push(np.concatenate([tone(0, 2), tone(1, 2)]))
    body = real_session.post("/meeting/stop").json()
    assert (body["diarization"], body["speakers"], body["meeting_id"]) == (expected, [], None)
    log_text = Path(body["log_path"]).read_text()
    assert "**00:00** **Others:** line 0 line 1" in log_text and "bogus" not in log_text


def test_diarized_prompts_carry_labels_only(diar):
    _, _, stop = _three_speaker_api_meeting(diar)
    prompts = diar.llm.prompts
    assert prompts
    for prompt in prompts:
        assert not re.search(r"\bS[0-9]+\b", prompt)
        assert "array(" not in prompt and "dtype" not in prompt
        assert stop["meeting_id"] not in prompt
    assert any("Speaker 2: line 1" in p for p in prompts)
    assert any(meetings._DIARIZED_SPEAKER_NOTE in p for p in prompts)
    files = [p for p in diar.tmp.rglob("*") if p.is_file()]
    allowed = (diar.tmp / "vault", diar.tmp / "templates")
    assert files and all(any(p.is_relative_to(a) for a in allowed) for p in files)


# ---------------------------------- speaker sidecar + rename/merge (slice B8) ---

def _saved(diar):
    return _three_speaker_api_meeting(diar)[2]


def _sidecar_path(client, meeting_id):
    return client.sidecars / f"{meeting_id}.json"


def _sidecar(client, meeting_id):
    return json.loads(_sidecar_path(client, meeting_id).read_text(encoding="utf-8"))


def _vault_bytes(client):
    return {p: p.read_bytes() for p in (client.tmp / "vault").rglob("*") if p.is_file()}


def _url(stop, tail=""):
    return f"/api/meetings/{stop['meeting_id']}/speakers{tail}"


def _lines(path):
    return Path(path).read_text(encoding="utf-8").split("\n")


_ORIGINAL_SPEAKERS = ["Me", "Speaker 1", "Speaker 2", "Speaker 3"]


def test_diarized_stop_writes_the_sidecar(diar):
    stop = _saved(diar)
    path = _sidecar_path(diar, stop["meeting_id"])
    assert path.is_file()
    record = _sidecar(diar, stop["meeting_id"])
    assert record["schema_version"] == 1
    assert (record["meeting_id"], record["log_path"], record["notes_path"]) == (
        stop["meeting_id"], stop["log_path"], stop["notes_path"])
    assert record["started_at"] == "2026-11-14T10:00:00"
    assert (record["embedder"], record["status"]) == ("mock", "ok")
    assert [sp["id"] for sp in record["speakers"]] == ["S1", "S2", "S3"]
    assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
               for sp in record["speakers"])
    assert [{k: sp[k] for k in ("id", "label", "name", "source", "seconds")}
            for sp in record["speakers"]] == stop["speakers"]
    assert len(record["segments"]) == stop["segments"]
    assert all(set(s) == {"start", "end", "source", "speaker_id"} for s in record["segments"])
    assert "line " not in path.read_text(encoding="utf-8")
    if sys.platform != "win32":
        assert path.stat().st_mode & 0o777 == 0o600
    assert diar.get("/meeting/status").json()["last_saved"] == stop


def test_get_speakers_returns_the_public_view(diar):
    stop = _saved(diar)
    resp = diar.get(_url(stop))
    assert resp.status_code == 200
    assert resp.json() == {"meeting_id": stop["meeting_id"], "speakers": stop["speakers"]}


def test_flag_off_stop_writes_no_sidecar(client, monkeypatch):
    def no_store():
        raise AssertionError("flag off must not touch the speaker store")

    monkeypatch.setattr(server, "_meeting_store", no_store)
    _, _, stop = _flag_off_meeting(client)
    assert not client.sidecars.exists()
    assert set(stop) == {"title", "title_generated", "notes_path", "log_path", "segments",
                         "template", "enhanced", "meeting_id", "diarization", "speakers"}


def test_single_remote_speaker_writes_no_sidecar(real_session, monkeypatch):
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(server._config, "diarization_backend", "mock")
    assert real_session.post("/meeting/start", json={"title": "Solo"}).status_code == 200
    _push(tone(0, 4), mic=tone(5, 2))
    stop = real_session.post("/meeting/stop").json()
    assert stop["meeting_id"] is None
    assert not real_session.sidecars.exists()


def test_sidecar_failure_never_fails_the_save(diar, monkeypatch):
    class BrokenStore:
        def save(self, record):
            raise OSError("disk full")

    original = server._meeting_store
    monkeypatch.setattr(server, "_meeting_store", lambda: BrokenStore())
    stop = _saved(diar)
    assert stop["meeting_id"] and len(stop["speakers"]) == 3
    assert Path(stop["notes_path"]).is_file() and Path(stop["log_path"]).is_file()
    monkeypatch.setattr(server, "_meeting_store", original)
    assert diar.get(_url(stop)).status_code == 404


@pytest.mark.parametrize("method,tail", [
    ("GET", ""), ("PUT", "/S1"), ("POST", "/merge"),
])
def test_speaker_routes_require_token(client, method, tail):
    resp = client.request(method, f"/api/meetings/{'0' * 32}/speakers{tail}",
                          headers={"X-Localflow-Token": ""}, json={})
    assert resp.status_code == 401


@pytest.mark.parametrize("meeting_id", ["0" * 32, "abc", "A" * 32, "%2e%2e"])
def test_speakers_404_for_meetings_without_sidecar(client, meeting_id):
    base = f"/api/meetings/{meeting_id}/speakers"
    assert client.get(base).status_code == 404
    assert client.put(f"{base}/S1", json={"name": "X"}).status_code == 404
    assert client.post(f"{base}/merge", json={"from": "S1", "into": "S2"}).status_code == 404
    assert not client.sidecars.exists()


def test_unknown_speaker_is_404(diar):
    stop = _saved(diar)
    vault = _vault_bytes(diar)
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()
    assert diar.put(_url(stop, "/S9"), json={"name": "X"}).status_code == 404
    assert diar.post(_url(stop, "/merge"), json={"from": "S1", "into": "S9"}).status_code == 404
    assert _vault_bytes(diar) == vault
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_rename_updates_transcript_frontmatter_and_sidecar_only(diar):
    stop = _saved(diar)
    log_path = Path(stop["log_path"])
    vault = _vault_bytes(diar)
    before_lines = _lines(log_path)
    before_record = _sidecar(diar, stop["meeting_id"])
    counts = {label: sum(f"**{label}:**" in line for line in before_lines)
              for label in ("Me", "Speaker 1", "Speaker 3")}

    resp = diar.put(_url(stop, "/S2"), json={"name": "  Dana   Lee!! "})
    assert resp.status_code == 200
    speakers = resp.json()["speakers"]
    assert speakers[1] == {**stop["speakers"][1], "label": "Dana Lee", "name": "Dana Lee"}
    assert [speakers[0], speakers[2]] == [stop["speakers"][0], stop["speakers"][2]]

    after_lines = _lines(log_path)
    assert "**00:04** **Dana Lee:** line 1" in after_lines
    assert "**00:17** **Dana Lee:** line 1" in after_lines
    assert not any("**Speaker 2:**" in line for line in after_lines)
    for label, n in counts.items():
        assert sum(f"**{label}:**" in line for line in after_lines) == n
    assert _front(log_path)["speakers"] == ["Me", "Speaker 1", "Dana Lee", "Speaker 3"]
    changed = [b for a, b in zip(before_lines, after_lines, strict=True) if a != b]
    assert changed == [
        'speakers: ["Me", "Speaker 1", "Dana Lee", "Speaker 3"]',
        "**00:04** **Dana Lee:** line 1",
        "**00:17** **Dana Lee:** line 1",
    ]

    after_vault = _vault_bytes(diar)
    assert set(after_vault) == set(vault)
    assert {p: b for p, b in after_vault.items() if p != log_path} == {
        p: b for p, b in vault.items() if p != log_path}

    record = _sidecar(diar, stop["meeting_id"])
    s2 = record["speakers"][1]
    assert (s2["id"], s2["label"], s2["name"]) == ("S2", "Dana Lee", "Dana Lee")
    assert record["segments"] == before_record["segments"]
    assert diar.get("/meeting/status").json()["last_saved"]["speakers"] == speakers
    assert diar.get(_url(stop)).json()["speakers"] == speakers


def test_rename_to_current_label_is_a_noop(diar):
    stop = _saved(diar)
    log_path = Path(stop["log_path"])
    mtime = log_path.stat().st_mtime_ns
    vault = _vault_bytes(diar)
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()
    resp = diar.put(_url(stop, "/S1"), json={"name": "Speaker 1"})
    assert resp.status_code == 200
    assert resp.json()["speakers"][0]["name"] is None
    assert log_path.stat().st_mtime_ns == mtime
    assert _vault_bytes(diar) == vault
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_rename_twice_follows_the_new_label(diar):
    stop = _saved(diar)
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == 200
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana Lee"}).status_code == 200
    text = Path(stop["log_path"]).read_text(encoding="utf-8")
    assert text.count("**Dana Lee:** line 1") == 2
    assert "**Dana:**" not in text and "**Speaker 2:**" not in text


def test_two_speakers_sharing_a_name_stay_distinct(diar):
    stop = _saved(diar)
    for sid in ("S2", "S3"):
        assert diar.put(_url(stop, f"/{sid}"), json={"name": "Dana"}).status_code == 200
    assert diar.put(_url(stop, "/S3"), json={"name": "Eve"}).status_code == 200
    lines = _lines(stop["log_path"])
    for stamp in ("00:04", "00:17"):
        assert f"**{stamp}** **Dana:** line 1" in lines
    for stamp in ("00:08", "00:14"):
        assert f"**{stamp}** **Eve:** line 2" in lines
    assert _front(stop["log_path"])["speakers"] == ["Me", "Speaker 1", "Dana", "Eve"]


@pytest.mark.parametrize("body", [{}, {"name": ""}, {"name": "***"}, {"name": 7},
                                  {"name": None}])
def test_bad_speaker_name_is_400(diar, body):
    stop = _saved(diar)
    vault = _vault_bytes(diar)
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()
    assert diar.put(_url(stop, "/S1"), json=body).status_code == 400
    assert _vault_bytes(diar) == vault
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_merge_speakers(diar):
    stop = _saved(diar)
    log_path = Path(stop["log_path"])
    notes = Path(stop["notes_path"]).read_bytes()
    before = log_path.read_text(encoding="utf-8")
    resp = diar.post(_url(stop, "/merge"), json={"from": "S3", "into": "S1"})
    assert resp.status_code == 200
    speakers = resp.json()["speakers"]
    assert [s["id"] for s in speakers] == ["S1", "S2"]
    assert speakers[0]["seconds"] == 13.0
    after = log_path.read_text(encoding="utf-8")
    assert "**Speaker 3:**" not in after
    assert after.count("**Speaker 1:**") == (
        before.count("**Speaker 1:**") + before.count("**Speaker 3:**"))
    assert _front(log_path)["speakers"] == ["Me", "Speaker 1", "Speaker 2"]
    record = _sidecar(diar, stop["meeting_id"])
    assert "S3" not in [sp["id"] for sp in record["speakers"]]
    assert all(seg["speaker_id"] != "S3" for seg in record["segments"])
    assert Path(stop["notes_path"]).read_bytes() == notes
    assert diar.get("/meeting/status").json()["last_saved"]["speakers"] == speakers


@pytest.mark.parametrize("body", [{}, {"from": "S1"}, {"from": "S1", "into": "S1"},
                                  {"from": 1, "into": "S2"}])
def test_merge_validates_ids(diar, body):
    stop = _saved(diar)
    vault = _vault_bytes(diar)
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()
    assert diar.post(_url(stop, "/merge"), json=body).status_code == 400
    assert _vault_bytes(diar) == vault
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def _point_sidecar_at(client, meeting_id, path):
    store = MeetingSpeakerStore(client.sidecars)
    record = store.load(meeting_id)
    record["log_path"] = str(path)
    store.save(record)
    return _sidecar_path(client, meeting_id).read_bytes()


def test_transcript_outside_vault_is_refused(diar):
    stop = _saved(diar)
    log_path = Path(stop["log_path"])
    original = log_path.read_bytes()
    for target in (diar.tmp / "outside" / "evil.md", diar.tmp / "vault" / "copy.txt"):
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(original)
        sidecar = _point_sidecar_at(diar, stop["meeting_id"], target)
        assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == 409
        assert target.read_bytes() == original
        assert log_path.read_bytes() == original
        assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_wrong_meeting_transcript_is_refused(diar):
    stop = _saved(diar)
    cfg = server._config
    other = ObsidianWriter(cfg.vault_path, cfg.notes_folder, cfg.logs_folder).write(
        title="Other", category="Zoom", started_at=datetime(2026, 11, 13, 9, 0),
        duration_seconds=60, notes_md="ok",
        segments=[Segment(4, "x", "others", None, None, "Speaker 2")],
    )
    original = other.log_path.read_bytes()
    sidecar = _point_sidecar_at(diar, stop["meeting_id"], other.log_path)
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == 409
    assert other.log_path.read_bytes() == original
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_notes_file_as_log_path_is_refused(diar):
    stop = _saved(diar)
    notes = Path(stop["notes_path"])
    original = notes.read_bytes()
    sidecar = _point_sidecar_at(diar, stop["meeting_id"], notes)
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == 409
    assert diar.post(_url(stop, "/merge"), json={"from": "S3", "into": "S1"}).status_code == 409
    assert notes.read_bytes() == original
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def _same_stamp_meeting(client):
    """Two speakers labelled "Speaker 1" with lines at the same stamp (00:05)."""
    from localflow.diarization.store import build_record

    cfg = server._config
    segments = [
        Segment(0, "hello", "me"),
        Segment(5, "from ann", "others", 5.4, "S1", "Speaker 1"),
        Segment(5.4, "from bob", "others", 7, "S2", "Speaker 1"),
        Segment(9, "ann again", "others", 11, "S1", "Speaker 1"),
    ]
    saved = ObsidianWriter(cfg.vault_path, cfg.notes_folder, cfg.logs_folder).write(
        title="Same", category="Zoom", started_at=datetime(2026, 11, 14, 9, 0),
        duration_seconds=60, notes_md="ok", segments=segments,
    )
    MeetingSpeakerStore(client.sidecars).save(build_record(
        meeting_id=saved.meeting_id, log_path=str(saved.log_path),
        notes_path=str(saved.notes_path), started_at="2026-11-14T09:00:00",
        embedder="mock", status="ok", segments=segments, speakers=[
            {"id": "S1", "label": "Speaker 1", "name": None, "source": "others",
             "seconds": 2.4},
            {"id": "S2", "label": "Speaker 1", "name": None, "source": "others",
             "seconds": 1.6},
        ]))
    return {"meeting_id": saved.meeting_id}, saved


def test_same_label_same_stamp_speakers_are_renamed_separately(client):
    stop, saved = _same_stamp_meeting(client)
    assert client.put(_url(stop, "/S2"), json={"name": "Bob"}).status_code == 200
    lines = _lines(saved.log_path)
    assert "**00:05** **Speaker 1:** from ann" in lines
    assert "**00:05** **Bob:** from bob" in lines
    assert client.put(_url(stop, "/S1"), json={"name": "Ann"}).status_code == 200
    lines = _lines(saved.log_path)
    assert "**00:05** **Ann:** from ann" in lines
    assert "**00:05** **Bob:** from bob" in lines
    assert "**00:09** **Ann:** ann again" in lines
    assert _front(saved.log_path)["speakers"] == ["Me", "Ann", "Bob"]
    labels = {sp["id"]: sp["label"] for sp in _sidecar(client, stop["meeting_id"])["speakers"]}
    assert labels == {"S1": "Ann", "S2": "Bob"}


def test_transcript_with_missing_lines_is_409(client):
    stop, saved = _same_stamp_meeting(client)
    text = saved.log_path.read_text(encoding="utf-8")
    saved.log_path.write_text(text.replace("**00:05** **Speaker 1:** from bob\n\n", ""),
                              encoding="utf-8")
    original = saved.log_path.read_bytes()
    sidecar = _sidecar_path(client, stop["meeting_id"]).read_bytes()
    assert client.put(_url(stop, "/S2"), json={"name": "Bob"}).status_code == 409
    assert saved.log_path.read_bytes() == original
    assert _sidecar_path(client, stop["meeting_id"]).read_bytes() == sidecar


def test_missing_transcript_is_404_and_keeps_the_sidecar(diar):
    stop = _saved(diar)
    Path(stop["log_path"]).unlink()
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == 404
    assert diar.post(_url(stop, "/merge"), json={"from": "S3", "into": "S1"}).status_code == 404
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


@pytest.mark.parametrize("exc,status", [
    (meetings.TranscriptChanged("changed"), 409),
    (PermissionError("denied"), 500),
])
def test_changed_transcript_is_409_and_keeps_the_sidecar(diar, monkeypatch, exc, status):
    stop = _saved(diar)
    sidecar = _sidecar_path(diar, stop["meeting_id"]).read_bytes()

    def boom(*args, **kwargs):
        raise exc

    monkeypatch.setattr(server, "relabel_transcript", boom)
    assert diar.put(_url(stop, "/S2"), json={"name": "Dana"}).status_code == status
    assert _sidecar_path(diar, stop["meeting_id"]).read_bytes() == sidecar


def test_speaker_edits_never_reach_prompts_or_logs(diar, caplog):
    stop = _saved(diar)
    prompts = len(diar.llm.prompts)
    with caplog.at_level(logging.DEBUG):
        assert diar.put(_url(stop, "/S2"), json={"name": "Zanzibar Q"}).status_code == 200
        assert diar.post(_url(stop, "/merge"),
                         json={"from": "S3", "into": "S2"}).status_code == 200
    assert caplog.records
    assert not any("Zanzibar" in r.getMessage() for r in caplog.records)
    assert len(diar.llm.prompts) == prompts


# -------------------------------------- voiceprints + recognition (slice B9) ---

from localflow.diarization import registry as registry_module
from localflow.diarization.registry import FileSpeakerRegistry

_HEX32 = re.compile(r"\b[0-9a-f]{32}\b")
_REAL_ENROLL_DECODE = server._decode_enrollment_audio
_SPEAKER_ROUTES = [
    ("GET", "/api/speakers"), ("POST", "/api/speakers"), ("GET", "/api/speakers/export"),
    ("PATCH", "/api/speakers/Dana"), ("DELETE", "/api/speakers/Dana"),
    ("DELETE", "/api/speakers"),
]


@pytest.fixture
def voices(client, monkeypatch, tmp_path_factory):
    """Registry seam pointing at a temp dir; fails the test if used while the gate is off."""
    root = tmp_path_factory.mktemp("voices") / "speakers"
    used = []

    def registry():
        assert server._recognition_on(), "voiceprint registry used with recognition off"
        used.append(1)
        return FileSpeakerRegistry(root, meetings=server._meeting_store())

    monkeypatch.setattr(server, "_speaker_registry", registry)
    client.voices = root
    client.registry_used = used
    return client


@pytest.fixture
def rec(diar, voices, monkeypatch):
    monkeypatch.setattr(server._config, "speaker_recognition", True)
    return diar


def _enroll(client, stop, sid="S1", name="Dana Lee"):
    return client.post("/api/speakers", json={
        "name": name, "meeting_id": stop["meeting_id"], "speaker_id": sid})


def test_enrolled_speaker_is_named_in_the_next_meeting(rec):
    first = _saved(rec)
    record = _sidecar(rec, first["meeting_id"])
    assert record["embedder"] == "mock"
    assert all(isinstance(sp["centroid"], list) and sp["voiceprint_id"] is None
               for sp in record["speakers"])
    resp = _enroll(rec, first)
    assert resp.status_code == 201
    vp = resp.json()
    assert set(vp) == {"id", "name", "created", "samples", "model_id"}
    assert (vp["name"], vp["samples"], vp["model_id"]) == ("Dana Lee", 1, "mock")
    assert _sidecar(rec, first["meeting_id"])["speakers"][0]["voiceprint_id"] == vp["id"]
    assert rec.get("/api/speakers").json() == {"speakers": [vp], "embedder": "mock"}

    rec.llm.prompts.clear()
    _, status, stop = _three_speaker_api_meeting(rec)
    assert [(s["id"], s["label"], s["name"]) for s in stop["speakers"]] == [
        ("S1", "Dana Lee", "Dana Lee"), ("S2", "Speaker 2", None), ("S3", "Speaker 3", None)]
    assert all(set(s) == {"id", "label", "name", "source", "seconds"} for s in stop["speakers"])
    log_text = Path(stop["log_path"]).read_text(encoding="utf-8")
    assert "**00:00** **Dana Lee:** line 0" in log_text and "**Speaker 2:**" in log_text
    assert _front(stop["log_path"])["speakers"] == ["Me", "Dana Lee", "Speaker 2", "Speaker 3"]
    sidecar = _sidecar(rec, stop["meeting_id"])
    assert [(sp["label"], sp["voiceprint_id"]) for sp in sidecar["speakers"]] == [
        ("Dana Lee", vp["id"]), ("Speaker 2", None), ("Speaker 3", None)]

    # Prompts carry the label only: no ids, vectors, files or paths.
    assert rec.llm.prompts and any("Dana Lee: line 0" in p for p in rec.llm.prompts)
    for prompt in rec.llm.prompts:
        assert vp["id"] not in prompt and stop["meeting_id"] not in prompt
        assert not _HEX32.search(prompt) and ".npy" not in prompt
        assert "centroid" not in prompt and "[1.0" not in prompt and "0.0, " not in prompt
        assert str(rec.voices) not in prompt and str(rec.sidecars) not in prompt
    # Ordinary API responses carry no voice data.
    for blob in (stop, status, rec.get("/meeting/status").json(), rec.get(_url(stop)).json()):
        text = json.dumps(blob)
        assert "centroid" not in text and "voiceprint" not in text and vp["id"] not in text


def test_recognition_logs_carry_no_names_ids_or_vectors(rec, caplog):
    first = _saved(rec)
    with caplog.at_level(logging.DEBUG):
        vp = _enroll(rec, first, name="Zanzibar Q").json()
        _saved(rec)
        rec.delete("/api/speakers")
    text = "\n".join(r.getMessage() for r in caplog.records)
    assert caplog.records and "recognised" in text
    assert "Zanzibar" not in text and vp["id"] not in text and "[" not in text


def test_recognition_off_stores_nothing_and_keeps_the_b8_sidecar(diar, voices, monkeypatch):
    # Diarization on, recognition off; then recognition on but diarization off.
    stop = _saved(diar)
    record = _sidecar(diar, stop["meeting_id"])
    assert all(set(sp) == {"id", "source", "label", "name", "voiceprint_id", "seconds",
                           "centroid"} for sp in record["speakers"])
    assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
               for sp in record["speakers"])
    assert [s["label"] for s in stop["speakers"]] == ["Speaker 1", "Speaker 2", "Speaker 3"]
    monkeypatch.setattr(server._config, "speaker_recognition", True)
    monkeypatch.setattr(server._config, "speaker_diarization", False)
    _, _, off = _flag_off_meeting(diar)
    assert off["diarization"] == "off" and off["speakers"] == []
    assert not diar.voices.exists() and diar.registry_used == []


def test_recognition_off_never_imports_the_registry(tmp_path):
    code = (
        "import sys\n"
        "from fastapi.testclient import TestClient\n"
        "from localflow import server\n"
        "from localflow.config import Config\n"
        "server._config = Config(meetings_enabled=False, meeting_watch=False,"
        " speaker_diarization=True, diarization_backend='mock', speaker_recognition=False)\n"
        "c, reason = server._open_diarization()\n"
        "assert c is not None and c.registry is None, reason\n"
        "segs, status, speakers, private, epoch = server._finalize_diarization("
        "type('S', (), {'diarization': c})(), [])\n"
        "assert (private, epoch) == ({}, None)\n"
        "server.load_or_create = lambda: 't'\n"
        "with TestClient(server.app, base_url='http://127.0.0.1:1',"
        " client=('127.0.0.1', 1)) as tc:\n"
        "    server._api_token = 't'\n"
        "    for m, p in " + repr(_SPEAKER_ROUTES) + ":\n"
        "        r = tc.request(m, p, headers={'X-Localflow-Token': 't'})\n"
        "        assert r.status_code == 403, (m, p, r.status_code, r.text)\n"
        "assert 'localflow.diarization.registry' not in sys.modules\n"
    )
    proc = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True,
                          env={**os.environ, "HOME": str(tmp_path)}, check=False)
    assert proc.returncode == 0, proc.stdout + proc.stderr
    assert not (tmp_path / ".localflow" / "speakers").exists()


@pytest.mark.parametrize("diarization,recognition", [(True, False), (False, True),
                                                     (False, False)])
@pytest.mark.parametrize("method,path", _SPEAKER_ROUTES)
def test_every_speaker_route_is_403_while_recognition_is_off(
        voices, monkeypatch, diarization, recognition, method, path):
    monkeypatch.setattr(server._config, "speaker_diarization", diarization)
    monkeypatch.setattr(server._config, "speaker_recognition", recognition)

    def never(*args, **kwargs):
        raise AssertionError("body read or audio decoded with recognition off")

    monkeypatch.setattr(server, "_decode_enrollment_audio", never)
    monkeypatch.setattr(server, "_bounded_body", never)
    kwargs = {"files": {"file": ("a.wav", b"RIFF" * 10)}, "data": {"name": "Dana"}} \
        if method == "POST" else {}
    resp = voices.request(method, path, **kwargs)
    assert resp.status_code == 403
    assert resp.json() == {"detail": "speaker recognition is off"}
    assert voices.registry_used == [] and not voices.voices.exists()


@pytest.mark.parametrize("method,path", _SPEAKER_ROUTES)
def test_speaker_routes_require_the_token(rec, method, path):
    resp = rec.request(method, path, headers={"X-Localflow-Token": ""})
    assert resp.status_code == 401
    assert rec.registry_used == []


def test_turning_recognition_off_keeps_files_and_stops_reads(rec, monkeypatch):
    first = _saved(rec)
    assert _enroll(rec, first).status_code == 201
    files = {p: p.read_bytes() for p in rec.voices.iterdir()}
    first_sidecar = _sidecar_path(rec, first["meeting_id"]).read_bytes()
    monkeypatch.setattr(server._config, "speaker_recognition", False)
    used = len(rec.registry_used)
    stop = _saved(rec)
    assert [s["label"] for s in stop["speakers"]] == ["Speaker 1", "Speaker 2", "Speaker 3"]
    assert all(sp["centroid"] is None for sp in _sidecar(rec, stop["meeting_id"])["speakers"])
    assert rec.get("/api/speakers").status_code == 403
    assert rec.delete("/api/speakers").status_code == 403
    assert {p: p.read_bytes() for p in rec.voices.iterdir()} == files
    assert _sidecar_path(rec, first["meeting_id"]).read_bytes() == first_sidecar
    assert len(rec.registry_used) == used
    monkeypatch.setattr(server._config, "speaker_recognition", True)
    assert rec.delete("/api/speakers/Dana Lee").json() == {"forgotten": 1}


def test_recognition_turned_off_mid_meeting_reads_no_voiceprints(rec, monkeypatch):
    assert rec.post("/meeting/start", json={"title": "x"}).status_code == 200
    collector = server._session.diarization
    assert collector.registry is not None
    _push(np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    monkeypatch.setattr(server._config, "speaker_recognition", False)
    monkeypatch.setattr(registry_module.FileSpeakerRegistry, "match",
                        lambda *a: pytest.fail("matched with recognition off"))
    stop = rec.post("/meeting/stop").json()
    assert collector.registry is None
    assert all(sp["centroid"] is None for sp in _sidecar(rec, stop["meeting_id"])["speakers"])


def _two_meetings_one_enrolled(rec):
    first = _saved(rec)
    vp = _enroll(rec, first).json()
    second = _saved(rec)
    for stop in (first, second):
        assert all(sp["centroid"] for sp in _sidecar(rec, stop["meeting_id"])["speakers"])
    return vp, first, second


@pytest.mark.parametrize("ref", ["id", "Dana Lee", "dana lee"])
def test_forget_one_erases_every_stored_centroid(rec, ref):
    vp, first, second = _two_meetings_one_enrolled(rec)
    vault = _vault_bytes(rec)
    resp = rec.delete(f"/api/speakers/{vp['id'] if ref == 'id' else ref}")
    assert resp.status_code == 200 and resp.json() == {"forgotten": 1}
    for stop in (first, second):
        record = _sidecar(rec, stop["meeting_id"])
        assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
                   for sp in record["speakers"])
    assert _sidecar(rec, second["meeting_id"])["speakers"][0]["label"] == "Dana Lee"
    assert _vault_bytes(rec) == vault  # transcripts and notes are left alone
    assert not (rec.voices / f"{vp['id']}.npy").exists()
    assert rec.get("/api/speakers").json() == {"speakers": [], "embedder": "mock"}
    assert rec.delete(f"/api/speakers/{vp['id']}").status_code == 404


def test_forget_all_erases_every_stored_centroid(rec):
    _, first, second = _two_meetings_one_enrolled(rec)
    assert _enroll(rec, first, sid="S2", name="Lee").status_code == 201
    assert rec.delete("/api/speakers").json() == {"forgotten": 2}
    assert list(rec.voices.iterdir()) == []
    for stop in (first, second):
        assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
                   for sp in _sidecar(rec, stop["meeting_id"])["speakers"])


def test_forget_reports_files_it_could_not_erase(rec):
    _two_meetings_one_enrolled(rec)
    (rec.sidecars / ("9" * 32 + ".json")).write_text("{broken")
    resp = rec.delete("/api/speakers")
    assert resp.status_code == 500
    assert resp.json()["detail"] == (
        "forgot 1 speaker(s), but voice data in 1 meeting file(s) could not be erased")


def test_forget_between_finalize_and_sidecar_write_wins(rec, monkeypatch):
    first = _saved(rec)
    _enroll(rec, first)
    real_write = server._writer.write

    def write_then_forget(**kwargs):
        saved = real_write(**kwargs)
        server._speaker_registry().forget()  # lands while summary/title/write run
        return saved

    monkeypatch.setattr(server._writer, "write", write_then_forget)
    stop = _saved(rec)
    record = _sidecar(rec, stop["meeting_id"])
    assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
               for sp in record["speakers"])
    assert stop["speakers"][0]["label"] == "Dana Lee"  # already in the transcript; stays


def test_recognition_off_between_finalize_and_sidecar_write_stores_nothing(rec, monkeypatch):
    real_write = server._writer.write

    def write_then_disable(**kwargs):
        monkeypatch.setattr(server._config, "speaker_recognition", False)
        return real_write(**kwargs)

    monkeypatch.setattr(server._writer, "write", write_then_disable)
    stop = _saved(rec)
    assert all(sp["centroid"] is None for sp in _sidecar(rec, stop["meeting_id"])["speakers"])


@pytest.mark.parametrize("body,status", [
    ({"name": "!!!", "meeting_id": "0" * 32, "speaker_id": "S1"}, 400),
    ({"name": 5, "meeting_id": "0" * 32, "speaker_id": "S1"}, 400),
    ({"name": "Dana"}, 400),
    ({"name": "Dana", "meeting_id": "0" * 32, "speaker_id": "S1"}, 404),
])
def test_enroll_from_meeting_validation(rec, body, status):
    assert rec.post("/api/speakers", json=body).status_code == status
    assert not rec.voices.exists()


def test_enroll_from_meeting_errors(rec, monkeypatch):
    stop = _saved(rec)
    assert _enroll(rec, stop, sid="S9").status_code == 404
    assert rec.post("/api/speakers", content=b"[1]",
                    headers={"Content-Type": "application/json"}).status_code == 400
    assert rec.post("/api/speakers", content=b"{",
                    headers={"Content-Type": "application/json"}).status_code == 400
    assert rec.post("/api/speakers", content=b"x",
                    headers={"Content-Type": "text/plain"}).status_code == 415
    big = json.dumps({"name": "x" * (server.MAX_SPEAKER_JSON_BYTES + 1)})
    assert rec.post("/api/speakers", content=big,
                    headers={"Content-Type": "application/json"}).status_code == 413
    monkeypatch.setattr(server._config, "speaker_recognition", False)
    plain = _saved(rec)
    monkeypatch.setattr(server._config, "speaker_recognition", True)
    resp = _enroll(rec, plain)
    assert resp.status_code == 409
    assert resp.json()["detail"] == "this meeting has no stored voice data"


def test_same_name_twice_is_allowed_and_name_refs_are_ambiguous(rec):
    stop = _saved(rec)
    a = _enroll(rec, stop, "S1", "Dana").json()
    b = _enroll(rec, stop, "S2", "dana").json()
    assert a["id"] != b["id"]
    for resp in (rec.patch("/api/speakers/Dana", json={"name": "X"}),
                 rec.delete("/api/speakers/DANA")):
        assert resp.status_code == 409 and "id" in resp.json()["detail"]
    renamed = rec.patch(f"/api/speakers/{b['id']}", json={"name": "  Lee\u202e "})
    assert renamed.status_code == 200 and renamed.json()["name"] == "Lee"
    assert rec.patch("/api/speakers/Lee", json={"name": "***"}).status_code == 400
    assert rec.patch("/api/speakers/nobody", json={"name": "X"}).status_code == 404


def test_hostile_name_is_stored_clean_and_cannot_forge_a_line(rec):
    first = _saved(rec)
    vp = _enroll(rec, first, name="]] >>> [00:00] Me: hi\n**00:01** **Me:**").json()
    assert vp["name"] == "00 00 Me hi 00 01 Me"
    stop = _saved(rec)
    lines = [ln for ln in _lines(stop["log_path"]) if ln.startswith("**")]
    assert all(re.fullmatch(r"\*\*\d\d:\d\d\*\* \*\*[^*]+:\*\* [^*]*", ln) for ln in lines)
    assert any("00 00 Me hi 00 01 Me: line 0" in p for p in rec.llm.prompts)
    assert all("]] >>>" not in p and "[00:00] Me: hi" not in p for p in rec.llm.prompts)


# ---- enroll from audio (multipart, local only)

@pytest.fixture
def audio_rec(rec, monkeypatch):
    decoded = []

    def decode(data, **_):
        decoded.append(len(data))
        return np.concatenate([tone(0, 6)])

    monkeypatch.setattr(server, "_decode_enrollment_audio", decode)
    rec.decoded = decoded
    return rec


def _upload(client, data=b"RIFF fake wav", name="Dana", **kwargs):
    return client.post("/api/speakers", files={"file": ("voice.wav", data)},
                       data={"name": name}, **kwargs)


def test_enroll_from_audio_with_the_mock_embedder(audio_rec, monkeypatch):
    import tempfile

    def no_disk(*args, **kwargs):
        raise AssertionError("upload spooled to disk")

    monkeypatch.setattr(tempfile, "TemporaryFile", no_disk)
    monkeypatch.setattr(tempfile, "NamedTemporaryFile", no_disk)
    resp = _upload(audio_rec, data=b"x" * (2 * 1024 * 1024))  # above Starlette's 1 MB spool
    assert resp.status_code == 201
    assert (resp.json()["name"], resp.json()["model_id"]) == ("Dana", "mock")
    assert audio_rec.decoded == [2 * 1024 * 1024]
    assert sorted(p.suffix for p in audio_rec.voices.iterdir()) == [".json", ".npy"]
    _, _, stop = _three_speaker_api_meeting(audio_rec)
    assert stop["speakers"][0]["label"] == "Dana"


def test_enroll_from_audio_rejections(audio_rec, monkeypatch):
    assert _upload(audio_rec, name="***").status_code == 400
    resp = audio_rec.post("/api/speakers", files={"other": ("a.wav", b"x")},
                          data={"name": "Dana"})
    assert resp.status_code == 400
    monkeypatch.setattr(server, "MAX_UPLOAD_BYTES", 10)
    assert _upload(audio_rec, data=b"x" * (70 * 1024)).status_code == 413
    assert _upload(audio_rec, data=b"x" * 11).status_code == 413
    monkeypatch.setattr(server, "MAX_UPLOAD_BYTES", 25 * 1024 * 1024)
    monkeypatch.setattr(server, "_decode_enrollment_audio",
                        lambda data, **_: np.zeros(16000 * 6, np.float32))
    assert _upload(audio_rec).json() == {"detail": "no speech detected"}
    monkeypatch.setattr(server, "_decode_enrollment_audio",
                        lambda data, **_: np.zeros(0, np.float32))
    assert _upload(audio_rec).status_code == 400
    assert not audio_rec.voices.exists()


def test_enroll_from_audio_decode_errors_are_generic(audio_rec, monkeypatch):
    monkeypatch.setattr(server, "_decode_enrollment_audio", _REAL_ENROLL_DECODE)

    def bad(data, max_seconds=1200, *, source=None):
        return subprocess.CompletedProcess([], 1, b"", b"/private/path/clip.wav: Invalid")

    monkeypatch.setattr(server, "_run_ffmpeg", bad)
    assert _upload(audio_rec).json() == {"detail": "could not decode the audio"}


def test_enroll_from_audio_embedder_unavailable_is_503(audio_rec, monkeypatch):
    from localflow.diarization import factory
    from localflow.diarization.types import DiarizationUnavailable

    def unavailable(config):
        raise DiarizationUnavailable("Speaker models are not downloaded.")

    monkeypatch.setattr(factory, "build_embedder", unavailable)
    resp = _upload(audio_rec)
    assert resp.status_code == 503 and "not downloaded" in resp.json()["detail"]
    assert not audio_rec.voices.exists()


def test_enroll_from_audio_with_a_silent_embedding_is_400(audio_rec, monkeypatch):
    from localflow.diarization.mock import MockEmbedder as Mock

    monkeypatch.setattr(Mock, "embed", lambda self, audio, rate: np.zeros(64, np.float32))
    assert _upload(audio_rec).status_code == 400
    assert not audio_rec.voices.exists()


def test_forget_during_audio_inference_does_not_recreate_a_voiceprint(audio_rec, monkeypatch):
    stop = _saved(audio_rec)
    assert _enroll(audio_rec, stop).status_code == 201

    def decode_while_forgetting(data, **_):
        assert audio_rec.delete("/api/speakers").json() == {"forgotten": 1}
        return tone(0, 6)

    monkeypatch.setattr(server, "_decode_enrollment_audio", decode_while_forgetting)
    resp = _upload(audio_rec)
    assert resp.status_code == 409 and "forgotten" in resp.json()["detail"]
    assert list(audio_rec.voices.iterdir()) == []


def test_recognition_off_during_audio_inference_stores_nothing(audio_rec, monkeypatch):
    def decode_then_disable(data, **_):
        monkeypatch.setattr(server._config, "speaker_recognition", False)
        return tone(0, 6)

    monkeypatch.setattr(server, "_decode_enrollment_audio", decode_then_disable)
    assert _upload(audio_rec).status_code == 403
    assert not audio_rec.voices.exists()


# ---- local-only: real peer, Host and proxy headers

def _remote_variants():
    return [
        ({"base_url": "http://testserver"}, {}),  # TestClient defaults: peer "testclient"
        ({"base_url": "http://127.0.0.1:8756", "client": ("10.0.0.5", 1)}, {}),
        ({"base_url": "http://laptop.tail1234.ts.net", "client": ("127.0.0.1", 1)}, {}),
        ({"base_url": "http://127.0.0.1:8756", "client": ("127.0.0.1", 1)},
         {"X-Forwarded-For": "100.64.0.7"}),
        ({"base_url": "http://127.0.0.1:8756", "client": ("127.0.0.1", 1)},
         {"Forwarded": "for=100.64.0.7"}),
        ({"base_url": "http://127.0.0.1:8756", "client": ("127.0.0.1", 1)},
         {"Tailscale-User-Login": "someone@example.com"}),
    ]


@pytest.mark.parametrize("variant", range(6))
def test_export_and_audio_upload_are_refused_off_this_computer(audio_rec, variant):
    stop = _saved(audio_rec)
    assert _enroll(audio_rec, stop).status_code == 201
    kwargs, extra = _remote_variants()[variant]
    headers = {"X-Localflow-Token": "test-token", **extra}
    with TestClient(server.app, headers=headers, **kwargs) as remote:
        export = remote.get("/api/speakers/export")
        assert export.status_code == 403
        assert export.json() == {"detail": "export is only available on this computer"}
        assert export.headers["content-type"] == "application/json"
        assert _upload(remote).status_code == 403
        assert remote.get("/api/speakers").status_code == 200  # metadata needs the token only
    assert audio_rec.decoded == []


@pytest.mark.parametrize("peer", ["127.0.0.1", "::1", "::ffff:127.0.0.1"])
def test_export_on_loopback_is_an_uncached_zip(audio_rec, peer):
    import io
    import zipfile

    stop = _saved(audio_rec)
    vp = _enroll(audio_rec, stop).json()
    with TestClient(server.app, base_url="http://localhost:8756", client=(peer, 1),
                    headers={"X-Localflow-Token": "test-token"}) as local:
        resp = local.get("/api/speakers/export")
    assert resp.status_code == 200
    assert resp.headers["cache-control"] == "no-store"
    assert resp.headers["content-type"] == "application/zip"
    with zipfile.ZipFile(io.BytesIO(resp.content)) as zf:
        assert sorted(zf.namelist()) == sorted(["registry.json", f"{vp['id']}.npy"])
        assert json.loads(zf.read("registry.json")) == {"schema_version": 1,
                                                         "voiceprints": [vp]}


# ---- speaker flags written to the config file (server._apply_live_config)

def _file_says(**flags):
    """What the config watcher hands the server after another process saved the file."""
    import dataclasses

    server._apply_live_config(dataclasses.replace(server._config, **flags))


@pytest.mark.parametrize("flag", ["speaker_recognition", "speaker_diarization"])
def test_config_file_turning_a_speaker_flag_off_closes_the_gate_now(rec, flag):
    first = _saved(rec)
    assert _enroll(rec, first).status_code == 201
    files = {p: p.read_bytes() for p in rec.voices.iterdir()}
    first_sidecar = _sidecar_path(rec, first["meeting_id"]).read_bytes()
    used = len(rec.registry_used)

    _file_says(**{flag: False})
    assert getattr(server._config, flag) is False
    for method, path in _SPEAKER_ROUTES:
        assert rec.request(method, path).status_code == 403, (method, path)
    if flag == "speaker_recognition":
        stop = _saved(rec)  # diarization still on: labels, but no voice data stored
        assert [s["label"] for s in stop["speakers"]] == ["Speaker 1", "Speaker 2", "Speaker 3"]
        assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
                   for sp in _sidecar(rec, stop["meeting_id"])["speakers"])
    assert len(rec.registry_used) == used
    assert {p: p.read_bytes() for p in rec.voices.iterdir()} == files  # nothing deleted
    assert _sidecar_path(rec, first["meeting_id"]).read_bytes() == first_sidecar

    _file_says(**{flag: True})
    assert rec.get("/api/speakers").json()["speakers"][0]["name"] == "Dana Lee"


def test_config_file_turning_recognition_on_opens_the_gate(diar, voices):
    assert diar.get("/api/speakers").status_code == 403
    _file_says(speaker_recognition=True)
    assert diar.get("/api/speakers").json() == {"speakers": [], "embedder": "mock"}
    stop = _saved(diar)
    assert all(sp["centroid"] for sp in _sidecar(diar, stop["meeting_id"])["speakers"])


def test_config_file_off_during_a_meeting_stores_no_voice_data(rec):
    assert rec.post("/meeting/start", json={"title": "x"}).status_code == 200
    collector = server._session.diarization
    assert collector.registry is not None
    _push(np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    _file_says(speaker_recognition=False)
    stop = rec.post("/meeting/stop").json()
    assert collector.registry is None and stop["diarization"] == "ok"
    assert all(sp["centroid"] is None for sp in _sidecar(rec, stop["meeting_id"])["speakers"])


def test_live_speaker_flags_keep_other_settings_behaviour(client, caplog):
    import dataclasses

    before = server._config
    with caplog.at_level(logging.INFO, "localflow.live_config"):
        server._apply_live_config(dataclasses.replace(
            before, speaker_recognition=True, diarize_mic=True, server_port=1,
            diarization_max_speakers=before.diarization_max_speakers + 3))
    assert server._config.speaker_recognition is True
    # A Settings switch (ALLOWLIST), so it is live like capture_system_audio.
    assert server._config.diarize_mic is True
    assert server._config.server_port == before.server_port
    assert server._config.diarization_max_speakers == before.diarization_max_speakers
    restart = [r.getMessage() for r in caplog.records if "restart" in r.getMessage()]
    assert restart and "speaker_recognition" not in restart[0]
    assert "diarize_mic" not in restart[0]
    assert "diarization_max_speakers" in restart[0] and "server_port" in restart[0]


def _saved_on_disk(**flags):
    """Another process saved the file; the watcher has not polled, so _config is stale."""
    import dataclasses

    save_config(dataclasses.replace(server._config, **flags))


def _start_and_stop(client):
    start = client.post("/meeting/start", json={"title": "x"})
    assert start.status_code == 200
    collector = getattr(server._session, "diarization", None)
    stop = client.post("/meeting/stop").json()
    return start.json(), collector, stop


def test_diarization_enabled_on_disk_just_before_start_is_used(diar, monkeypatch):
    monkeypatch.setattr(server._config, "speaker_diarization", False)
    port = server._config.server_port
    _saved_on_disk(speaker_diarization=True, diarize_mic=True, server_port=1)
    assert server._config.speaker_diarization is False  # no watcher tick
    start, collector, stop = _start_and_stop(diar)
    assert start["diarization"] == "collecting" and collector is not None
    assert stop["diarization"] == "ok"
    assert collector.registry is None  # recognition stayed off
    # Only the speaker flags are picked up; restart-only settings stay as started.
    assert server._config.speaker_diarization is True
    assert server._config.diarize_mic is False and server._config.server_port == port


def test_diarization_disabled_on_disk_just_before_start_is_used(diar, monkeypatch):
    monkeypatch.setitem(sys.modules, "localflow.diarization.factory", None)
    _saved_on_disk(speaker_diarization=False)
    start, collector, stop = _start_and_stop(diar)
    assert (start["diarization"], collector, stop["diarization"]) == ("off", None, "off")


def test_recognition_opt_out_on_disk_just_before_start_reads_no_voiceprints(rec):
    first = _saved(rec)
    assert _enroll(rec, first).status_code == 201
    used = len(rec.registry_used)
    _saved_on_disk(speaker_recognition=False)
    assert server._config.speaker_recognition is True  # no watcher tick
    _, _, stop = _three_speaker_api_meeting(rec)  # Dana Lee would be named if stale
    assert [s["label"] for s in stop["speakers"]] == ["Speaker 1", "Speaker 2", "Speaker 3"]
    assert all(sp["centroid"] is None for sp in _sidecar(rec, stop["meeting_id"])["speakers"])
    assert len(rec.registry_used) == used
    assert rec.get("/api/speakers").status_code == 403


@pytest.mark.parametrize("stale_callback", [False, True])
def test_recognition_opt_in_on_disk_just_before_start_is_used(diar, voices, monkeypatch,
                                                              stale_callback):
    _saved_on_disk(speaker_recognition=True)
    if stale_callback:
        # The watcher read the file before the save; its callback lands after the refresh.
        refresh = server._refresh_speaker_flags

        def refresh_then_stale_callback():
            fresh = refresh()
            _file_says(speaker_recognition=False)
            return fresh

        monkeypatch.setattr(server, "_refresh_speaker_flags", refresh_then_stale_callback)
        # The voices stub asserts the live gate the stale callback just closed; this one
        # only constructs (no I/O). Voiceprint reads still pass the live gate later.
        monkeypatch.setattr(server, "_speaker_registry", lambda: FileSpeakerRegistry(
            diar.voices, meetings=server._meeting_store()))
    assert diar.post("/meeting/start", json={"title": "x"}).status_code == 200
    collector = server._session.diarization
    assert collector.registry is not None
    _push(np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    if stale_callback:
        assert server._recognition_on() is False
        _file_says(speaker_recognition=True)  # the watcher's next poll corrects it
    else:
        assert diar.registry_used
    stop = diar.post("/meeting/stop").json()
    assert stop["diarization"] == "ok" and stop["speakers"]
    assert all(sp["centroid"] for sp in _sidecar(diar, stop["meeting_id"])["speakers"])


@pytest.mark.parametrize("text", [
    "[speakers\nnot toml",
    ('config_version = "2"\n[meetings]\nspeaker_diarization = false\n'
     "speaker_recognition = false\n"),
])
def test_invalid_config_file_keeps_the_running_speaker_flags(rec, caplog, text):
    Path(os.environ["LOCALFLOW_CONFIG"]).write_text(text, encoding="utf-8")
    with caplog.at_level(logging.WARNING, "localflow.server"):
        start, collector, _ = _start_and_stop(rec)
    assert start["diarization"] == "collecting" and collector.registry is not None
    assert server._config.speaker_diarization is True
    assert server._config.speaker_recognition is True
    assert any("config reload skipped" in r.getMessage() for r in caplog.records)


def test_missing_config_with_a_legacy_file_starts_without_migrating(rec, monkeypatch, tmp_path):
    from localflow import config as config_module

    legacy = tmp_path / ".localflow.toml"
    legacy.write_text('model_size = "tiny"\n', encoding="utf-8")  # no credentials
    monkeypatch.setattr(config_module, "legacy_config_path", lambda: legacy)
    active = Path(os.environ["LOCALFLOW_CONFIG"])
    real_exists = Path.exists
    assert not real_exists(active)
    probes = []

    def exists(self, *args, **kwargs):
        # The active file is there for the first probe, then gone (deleted meanwhile).
        if self == active and not probes:
            probes.append(1)
            return True
        return real_exists(self, *args, **kwargs)

    monkeypatch.setattr(Path, "exists", exists)
    start, collector, _ = _start_and_stop(rec)  # asserts the start returned 200
    assert start["diarization"] == "collecting" and collector.registry is not None
    assert server._config.speaker_diarization and server._config.speaker_recognition
    assert not real_exists(active)  # never migrated into a new v2 file
    assert legacy.read_text(encoding="utf-8") == 'model_size = "tiny"\n'


def _closing_during_body_read(monkeypatch):
    real = server._bounded_body

    async def read_then_close(request, limit):
        body = await real(request, limit)
        _file_says(speaker_recognition=False)  # the file changes while the body arrives
        return body

    monkeypatch.setattr(server, "_bounded_body", read_then_close)


def test_json_enroll_rechecks_the_gate_after_reading_the_body(rec, monkeypatch):
    first = _saved(rec)
    _closing_during_body_read(monkeypatch)
    resp = _enroll(rec, first)
    assert resp.status_code == 403
    assert not rec.voices.exists()
    assert _sidecar(rec, first["meeting_id"])["speakers"][0]["voiceprint_id"] is None


def test_rename_rechecks_the_gate_after_reading_the_body(rec, monkeypatch):
    vp = _enroll(rec, _saved(rec)).json()
    registry_file = (rec.voices / "registry.json").read_bytes()
    _closing_during_body_read(monkeypatch)
    assert rec.patch(f"/api/speakers/{vp['id']}", json={"name": "Lee"}).status_code == 403
    assert (rec.voices / "registry.json").read_bytes() == registry_file


def test_rename_route_returns_the_renamed_voiceprint(rec):
    vp = _enroll(rec, _saved(rec)).json()
    resp = rec.patch("/api/speakers/Dana Lee", json={"name": " Lee\n"})
    assert resp.status_code == 200
    assert resp.json() == {**vp, "name": "Lee"}
    assert rec.get("/api/speakers").json() == {"speakers": [{**vp, "name": "Lee"}],
                                               "embedder": "mock"}


# ---- access log: no enrolled names or ids in request lines

def _access_record(path, method="DELETE"):
    return logging.LogRecord(
        "uvicorn.access", logging.INFO, __file__, 1, '%s - "%s %s HTTP/%s" %d',
        ("127.0.0.1:50000", method, path, "1.1", 200), None)


@pytest.mark.parametrize("path", [
    "/api/speakers/Dana%20Lee", "/api/speakers/" + "a" * 32, "/api/speakers/Dana?x=1",
    "/api/speakers?token=secret-Dana", "/api/speakers/", "/api/speakers/export",
    "/api/%73peakers/Dana", "/prefix/api/speakers/Dana", "/API/Speakers/Dana",
])
def test_speaker_paths_are_redacted_in_access_logs(path):
    record = _access_record(path)
    assert server._SpeakerPathFilter().filter(record) is True
    line = record.getMessage()
    assert "Dana" not in line and "a" * 32 not in line and "secret" not in line
    assert line == '127.0.0.1:50000 - "DELETE /api/speakers/<redacted> HTTP/1.1" 200'


@pytest.mark.parametrize("path", [
    "/api/speakers", "/meeting/status", "/api/meetings/" + "b" * 32 + "/speakers",
    "/api/speakersx/Dana", "/transcribe?clean=1",
])
def test_other_access_log_lines_are_unchanged(path):
    record = _access_record(path, "GET")
    expected = record.getMessage()
    assert server._SpeakerPathFilter().filter(record) is True
    assert record.getMessage() == expected


def test_server_main_runs_uvicorn_with_the_redacting_access_log(monkeypatch, capsys):
    import logging.config

    import uvicorn

    captured = {}
    monkeypatch.setattr(uvicorn, "run", lambda app, **kw: captured.update(kw))
    monkeypatch.setattr(server, "prepare_process_logs", lambda: None)
    monkeypatch.setattr(server, "ConfigWatcher",
                        lambda *a, **k: type("W", (), {"start": lambda self: None})())
    server.main()
    log_config = captured["log_config"]
    assert (captured["host"], captured["port"]) == (server._config.server_host,
                                                    server._config.server_port)
    loggers = [logging.getLogger(n) for n in ("uvicorn", "uvicorn.error", "uvicorn.access")]
    saved = [(lg.handlers[:], lg.filters[:], lg.level, lg.propagate) for lg in loggers]
    access = loggers[2]
    try:
        logging.config.dictConfig(log_config)
        access.info('%s - "%s %s HTTP/%s" %d', "127.0.0.1:1", "PATCH",
                    "/api/speakers/Zanzibar%20Q", "1.1", 200)
        access.info('%s - "%s %s HTTP/%s" %d', "127.0.0.1:1", "GET", "/meeting/status",
                    "1.1", 200)
        for handler in access.handlers:
            handler.flush()
    finally:
        for lg, (handlers, filters, level, propagate) in zip(loggers, saved):
            lg.handlers[:], lg.filters[:] = handlers, filters
            lg.setLevel(level)
            lg.propagate = propagate
    out = capsys.readouterr().out
    assert "PATCH /api/speakers/<redacted> HTTP/1.1" in out and "Zanzibar" not in out
    assert "GET /meeting/status HTTP/1.1" in out


# ---- opt-out while finalize drains queued chunks (BUG_0003)

def test_opt_out_while_finalize_drains_queued_chunks_names_nobody(rec, monkeypatch):
    first = _saved(rec)
    assert _enroll(rec, first).status_code == 201  # tone 0 would be "Dana Lee"
    entered, release = threading.Event(), threading.Event()
    block = {"on": False}
    real_diarize = MockDiarizer.diarize

    def diarize(self, audio, rate):
        # Only the collector worker blocks (the tone transcriber also uses the mock).
        if block["on"] and threading.current_thread().name == "localflow-diarization":
            entered.set()
            assert release.wait(10)
        return real_diarize(self, audio, rate)

    matched = []
    real_match = FileSpeakerRegistry.match
    monkeypatch.setattr(MockDiarizer, "diarize", diarize)
    monkeypatch.setattr(FileSpeakerRegistry, "match",
                        lambda self, v, m: matched.append(m) or real_match(self, v, m))
    assert rec.post("/meeting/start", json={"title": "Launch sync"}).status_code == 200
    _push(np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4), tone(0, 2)]))
    block["on"] = True
    _push(np.concatenate([tone(2, 3), tone(1, 3)]))  # queued; the worker blocks on it
    assert entered.wait(5)
    box = {}
    stopper = threading.Thread(target=lambda: box.update(stop=rec.post("/meeting/stop")))
    stopper.start()
    end = time.monotonic() + 5  # finalize has started and is waiting for the drain
    while not any(t.name == "localflow-diarization-finalize" for t in threading.enumerate()):
        assert time.monotonic() < end
        time.sleep(0.005)
    _file_says(speaker_recognition=False)  # the opt-out lands mid-drain
    release.set()
    stopper.join(30)
    stop = box["stop"].json()
    assert stop["diarization"] == "ok"
    assert [(s["label"], s["name"]) for s in stop["speakers"]] == [
        ("Speaker 1", None), ("Speaker 2", None), ("Speaker 3", None)]
    assert "Dana Lee" not in Path(stop["log_path"]).read_text(encoding="utf-8")
    assert all("Dana Lee" not in p for p in rec.llm.prompts[-3:])
    assert matched == []
    assert all(sp["centroid"] is None and sp["voiceprint_id"] is None
               for sp in _sidecar(rec, stop["meeting_id"])["speakers"])


# ---- enrollment decoding: seekable containers, own duration bound (BUG_0001, BUG_0004)

def _late_moov_m4a(tmp_path, seconds=12):
    import shutil

    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        pytest.skip("ffmpeg not installed")
    out = tmp_path / "gen" / "late.m4a"
    out.parent.mkdir()
    subprocess.run([ffmpeg, "-nostdin", "-y", "-loglevel", "error", "-f", "lavfi", "-i",
                    f"sine=frequency=300:duration={seconds}", "-c:a", "aac", str(out)],
                   check=True)
    data = out.read_bytes()
    out.unlink()
    assert data[4:8] == b"ftyp" and server._is_iso_bmff(data)  # standard M4A, ftyp first
    assert data.find(b"moov") > data.find(b"mdat") > 0  # index after the audio
    return data


@pytest.fixture
def staging_registry(tmp_path, monkeypatch):
    """A temp voiceprint folder for direct decoder calls that may stage a seekable copy."""
    root = tmp_path / "speakers"
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    monkeypatch.setattr(server._config, "speaker_recognition", True)
    monkeypatch.setattr(server, "_speaker_registry", lambda: FileSpeakerRegistry(root))
    return root


def test_real_late_moov_m4a_decodes_for_enrollment(tmp_path, monkeypatch, staging_registry):
    import tempfile

    data = _late_moov_m4a(tmp_path)  # whichever path this FFmpeg needs, the clip decodes
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    monkeypatch.setattr(tempfile, "tempdir", str(scratch))
    audio = _REAL_ENROLL_DECODE(data)
    assert audio.dtype == np.float32 and 11.9 * RATE <= audio.size <= 12.1 * RATE
    assert list(scratch.iterdir()) == []  # nothing in the system temp folder
    assert not staging_registry.exists() or list(staging_registry.iterdir()) == []


def test_real_late_moov_m4a_enrolls_end_to_end(rec, tmp_path):
    data = _late_moov_m4a(tmp_path)
    resp = _upload(rec, data=data, name="Dana")
    assert resp.status_code == 201, resp.text
    assert resp.json()["name"] == "Dana"


def _fake_ffmpeg(monkeypatch, outputs):
    """Fake _run_ffmpeg: pops one stdout per call; records (max_seconds, source, file state)."""
    calls = []

    def run(data, max_seconds=1200, *, source=None):
        state = None
        if source is not None:
            st = os.stat(source)
            state = (Path(source).read_bytes(), st.st_mode & 0o777,
                     os.stat(os.path.dirname(source)).st_mode & 0o777)
        calls.append((max_seconds, source, state))
        return subprocess.CompletedProcess([], 0, outputs.pop(0), b"")

    monkeypatch.setattr(server, "_run_ffmpeg", run)
    return calls


def _pcm(seconds):
    return np.full(int(seconds * RATE), 0.2, dtype=np.float32).tobytes()


_FTYP = b"\x00\x00\x00\x18ftypM4A \x00\x00\x02\x00isomM4A "  # conventional M4A first box
_BMFF_CLIP = _FTYP + b"\x00\x00\x00\x08mdat"


def test_enrollment_retries_with_a_private_seekable_copy_only_when_needed(monkeypatch,
                                                                         staging_registry):
    calls = _fake_ffmpeg(monkeypatch, [b"", _pcm(6)])
    audio = _REAL_ENROLL_DECODE(_BMFF_CLIP)
    assert audio.size == 6 * RATE
    assert [(m, s is None) for m, s, _ in calls] == [(301, True), (301, False)]
    source, (content, mode, dir_mode) = calls[1][1], calls[1][2]
    assert content == _BMFF_CLIP
    if os.name != "nt":
        assert (mode, dir_mode) == (0o600, 0o700)
    assert Path(source).parent == staging_registry  # app-owned voiceprint folder
    assert re.fullmatch(r"\.enroll-[0-9a-f]{32}\.audio", Path(source).name)
    assert not os.path.exists(source) and list(staging_registry.iterdir()) == []
    calls = _fake_ffmpeg(monkeypatch, [_pcm(6)])
    _REAL_ENROLL_DECODE(_BMFF_CLIP)
    assert [s for _, s, _ in calls] == [None]  # pipe worked: no temporary file at all


@pytest.mark.parametrize("data", [
    b"clip-bytes", b"RIFF\x24\x00\x00\x00WAVEfmt ", b"OggS\x00\x02", b"\x1aE\xdf\xa3",
    b"\x00\x00\x00\x08moov", b"\x00\x00\x00\x0cftypM4A ", b"\x00\x00\x00\x04free" + _FTYP,
    b"",
])
def test_non_iso_bmff_input_is_never_retried_from_disk(monkeypatch, data):
    calls = _fake_ffmpeg(monkeypatch, [b""])
    with pytest.raises(server.HTTPException) as info:
        _REAL_ENROLL_DECODE(data)
    assert info.value.detail == "could not decode the audio"
    assert [s for _, s, _ in calls] == [None]  # only the pipe ran


def test_iso_bmff_detection_allows_leading_padding_boxes():
    assert server._is_iso_bmff(_BMFF_CLIP)
    assert server._is_iso_bmff(b"\x00\x00\x00\x08free" + _FTYP)
    assert server._is_iso_bmff(b"\x00\x00\x00\x08skip\x00\x00\x00\x08wide" + _FTYP)
    assert not server._is_iso_bmff(b"\x00\x00\x00\x08free" * 4 + _FTYP)  # bounded walk


def test_seekable_command_forces_mov_without_external_references(monkeypatch):
    seen = []

    def run(cmd, **kwargs):
        seen.append((cmd, kwargs.get("input")))
        return subprocess.CompletedProcess(cmd, 0, b"", b"")

    monkeypatch.setattr(server.subprocess, "run", run)
    server._run_ffmpeg(b"pipe-bytes", 301)
    server._run_ffmpeg(b"", 301, source="/tmp/x/clip")
    (pipe_cmd, pipe_input), (file_cmd, _) = seen
    assert pipe_cmd[:8] == ["ffmpeg", "-nostdin", "-protocol_whitelist", "pipe",
                            "-format_whitelist", server._FFMPEG_INPUT_FORMATS,
                            "-i", "pipe:0"] and pipe_input == b"pipe-bytes"
    assert "-enable_drefs" not in pipe_cmd and "mov" not in pipe_cmd
    assert file_cmd[:12] == ["ffmpeg", "-nostdin", "-protocol_whitelist", "file",
                             "-format_whitelist", server._FFMPEG_INPUT_FORMATS,
                             "-f", "mov", "-enable_drefs", "0", "-i", "file:/tmp/x/clip"]
    assert pipe_cmd[8:] == file_cmd[12:]  # same bounded output either way


@pytest.mark.parametrize("dictation_limit", [0, 1, 20])
def test_dictation_limit_never_truncates_enrollment(monkeypatch, dictation_limit):
    monkeypatch.setattr(server._config, "max_recording_seconds", dictation_limit)
    calls = _fake_ffmpeg(monkeypatch, [_pcm(45)])
    assert _REAL_ENROLL_DECODE(b"x").size == 45 * RATE
    assert calls[0][0] == server.ENROLL_MAX_SECONDS + 1 == 301


def test_enrollment_over_the_bound_is_refused_not_cut(audio_rec, monkeypatch):
    monkeypatch.setattr(server, "_decode_enrollment_audio", _REAL_ENROLL_DECODE)
    _fake_ffmpeg(monkeypatch, [_pcm(300)])
    assert _REAL_ENROLL_DECODE(b"x").size == 300 * RATE  # exactly the bound is fine
    _fake_ffmpeg(monkeypatch, [_pcm(300) + np.zeros(1, np.float32).tobytes()])
    resp = _upload(audio_rec)
    assert resp.status_code == 400
    assert resp.json() == {"detail": "recording is longer than 300 seconds; use a shorter clip"}
    assert not audio_rec.voices.exists()


def test_transcribe_keeps_its_pipe_and_dictation_limit(client, monkeypatch):
    monkeypatch.setattr(server._config, "max_recording_seconds", 0)
    calls = _fake_ffmpeg(monkeypatch, [b""])
    with pytest.raises(server.HTTPException):
        server._decode_audio(b"x")
    assert calls == [(0, None, None)]  # no seekable retry; same max_seconds as before


# ---- the server's own embedder id in the list envelope (ANALYSIS_0002)

@pytest.mark.parametrize("backend,expected", [
    ("mock", "mock"), ("sherpa-onnx", "3dspeaker-campplus-zh-en-16k-advanced"),
])
def test_speaker_list_reports_the_servers_embedder_without_loading_it(
        rec, monkeypatch, backend, expected):
    from localflow.diarization import factory, sherpa

    monkeypatch.setattr(factory, "build_embedder", lambda c: pytest.fail("model loaded"))
    monkeypatch.setattr(sherpa.SherpaEmbedder, "load", lambda self: pytest.fail("loaded"))
    monkeypatch.setattr(server._config, "diarization_backend", backend)
    out = rec.get("/api/speakers").json()
    assert out == {"speakers": [], "embedder": expected}


# ---- any loopback IP literal as Host for local-only routes (ANALYSIS_0003)

def test_export_accepts_a_server_bound_to_another_loopback_address(audio_rec):
    vp = _enroll(audio_rec, _saved(audio_rec)).json()
    headers = {"X-Localflow-Token": "test-token"}
    with TestClient(server.app, base_url="http://127.0.0.2:8756", client=("127.0.0.2", 1),
                    headers=headers) as local:
        assert local.get("/api/speakers/export").status_code == 200
        assert local.get("/api/speakers/export",
                         headers={"X-Forwarded-For": "100.64.0.7"}).status_code == 403
    with TestClient(server.app, base_url="http://127.0.0.2:8756", client=("10.0.0.5", 1),
                    headers=headers) as remote:
        assert remote.get("/api/speakers/export").status_code == 403
    assert vp["id"]
    # The shared token-middleware Host check is unchanged.
    assert server._host_is_loopback("127.0.0.2:8756") is False
    assert server._host_header_is_loopback("127.0.0.2:8756") is True
    assert server._host_header_is_loopback("[::ffff:127.0.0.1]:1") is True
    assert server._host_header_is_loopback("10.0.0.5") is False
    assert server._host_header_is_loopback("127.0.0.1.evil.example") is False


# ---- staged enrollment audio left by a killed server (BUG b35d2d60_0001)

_ORPHAN = ".enroll-" + "c" * 32 + ".audio"


def _seed_staging(root):
    """An abandoned staged copy plus neighbours that must all survive a sweep."""
    root.mkdir(parents=True, exist_ok=True)
    (root / _ORPHAN).write_bytes(b"raw voice audio")
    keep = {"registry.json": b"{}", "e" * 32 + ".npy": b"npy", "notes.txt": b"mine",
            # Uppercase near-miss with a digit no other entry uses: a case-only variant of
            # the orphan would be the same file on NTFS / default APFS.
            ".enroll-short.audio": b"x", ".enroll-" + "A" * 32 + ".audio": b"x",
            ".enroll-" + "c" * 32 + ".audio.tmp": b"x"}
    for name, data in keep.items():
        (root / name).write_bytes(data)
    (root / (".enroll-" + "d" * 32 + ".audio")).mkdir()  # a directory with the name
    target = root.parent / "outside.bin"
    target.write_bytes(b"not ours")
    try:
        (root / (".enroll-" + "f" * 32 + ".audio")).symlink_to(target)
    except (OSError, NotImplementedError):
        target = None
    return keep, target


def _restart(client):
    with TestClient(server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1),
                    headers={"X-Localflow-Token": "test-token"}) as again:
        assert again.get("/healthz").status_code == 200


def test_startup_deletes_abandoned_enrollment_audio_even_with_recognition_off(client):
    assert not (server._config.speaker_diarization or server._config.speaker_recognition)
    root = server._enrollment_staging_root()
    keep, target = _seed_staging(root)
    _restart(client)
    assert not (root / _ORPHAN).exists()
    assert {n: (root / n).read_bytes() for n in keep} == keep
    assert (root / (".enroll-" + "d" * 32 + ".audio")).is_dir()
    if target is not None:  # the link is not followed: its target and the link stay
        assert target.read_bytes() == b"not ours"
        assert (root / (".enroll-" + "f" * 32 + ".audio")).is_symlink()


def test_clean_startup_creates_no_staging_state(client):
    root = server._enrollment_staging_root()
    _restart(client)
    assert not root.exists()


def test_startup_sweep_imports_no_registry_and_logs_no_path(tmp_path):
    home = tmp_path / "home"
    root = home / ".localflow" / "speakers"
    root.mkdir(parents=True)
    (root / _ORPHAN).write_bytes(b"raw voice audio")
    code = (
        "import logging, sys\n"
        "records = []\n"
        "class H(logging.Handler):\n"
        "    def emit(self, r): records.append(r.getMessage())\n"
        "logging.getLogger().addHandler(H()); logging.getLogger().setLevel(logging.DEBUG)\n"
        "from localflow import server\n"
        "server.load_or_create = lambda: 't'\n"
        "server._start_watcher()\n"
        "assert 'localflow.diarization.registry' not in sys.modules\n"
        "assert not any('.enroll-' in m for m in records), records\n"
    )
    proc = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True,
                          env={**os.environ, "HOME": str(home), "USERPROFILE": str(home)},
                          check=False)
    assert proc.returncode == 0, proc.stdout + proc.stderr
    assert list(root.iterdir()) == []


def test_startup_sweep_failure_never_fails_startup(client, monkeypatch, caplog):
    from localflow.diarization import staging

    def boom(root):
        raise RuntimeError("/secret/path")

    monkeypatch.setattr(staging, "sweep", boom)
    with caplog.at_level(logging.WARNING):
        _restart(client)
    assert any("abandoned enrollment audio" in r.getMessage() for r in caplog.records)
    assert not any("secret" in r.getMessage() for r in caplog.records)


def test_forget_all_deletes_abandoned_enrollment_audio(rec):
    assert _enroll(rec, _saved(rec)).status_code == 201
    (rec.voices / _ORPHAN).write_bytes(b"raw voice audio")
    (rec.voices / "notes.txt").write_bytes(b"mine")
    assert rec.delete("/api/speakers").json() == {"forgotten": 1}
    assert sorted(p.name for p in rec.voices.iterdir()) == ["notes.txt"]


def test_undeletable_abandoned_audio_makes_forget_all_report_incomplete(rec, monkeypatch):
    rec.voices.mkdir(parents=True, exist_ok=True)
    orphan = rec.voices / _ORPHAN
    orphan.write_bytes(b"raw voice audio")
    real_unlink = Path.unlink

    def refuse(self, *args, **kwargs):
        if self == orphan:
            raise PermissionError("in use")
        return real_unlink(self, *args, **kwargs)

    monkeypatch.setattr(Path, "unlink", refuse)
    resp = rec.delete("/api/speakers")
    assert resp.status_code == 500
    assert resp.json()["detail"] == (
        "forgot 0 speaker(s), but 1 voice-data file(s) could not be erased")
    assert _ORPHAN not in resp.text


def test_forget_all_during_seekable_decode_leaves_no_staged_copy(audio_rec, monkeypatch):
    monkeypatch.setattr(server, "_decode_enrollment_audio", _REAL_ENROLL_DECODE)
    seen = {}

    def run(data, max_seconds=1200, *, source=None):
        if source is None:
            return subprocess.CompletedProcess([], 0, b"", b"")  # pipe can't seek
        seen["staged"] = Path(source).exists()
        assert audio_rec.delete("/api/speakers").json() == {"forgotten": 0}
        seen["after_forget"] = Path(source).exists()
        return subprocess.CompletedProcess([], 0, tone(0, 6).tobytes(), b"")

    monkeypatch.setattr(server, "_run_ffmpeg", run)
    resp = _upload(audio_rec, data=_BMFF_CLIP)
    assert resp.status_code == 409 and "forgotten" in resp.json()["detail"]
    assert seen == {"staged": True, "after_forget": False}
    assert list(audio_rec.voices.iterdir()) == []


def test_opt_out_during_the_pipe_attempt_stages_nothing(audio_rec, monkeypatch):
    monkeypatch.setattr(server, "_decode_enrollment_audio", _REAL_ENROLL_DECODE)
    calls = []

    def run(data, max_seconds=1200, *, source=None):
        calls.append(source)
        if source is None:  # the opt-out lands while the pipe attempt runs
            _file_says(speaker_recognition=False)
        return subprocess.CompletedProcess([], 0, b"", b"")

    monkeypatch.setattr(server, "_run_ffmpeg", run)
    resp = _upload(audio_rec, data=_BMFF_CLIP)
    assert resp.status_code == 403
    assert calls == [None]  # no seekable attempt
    assert not audio_rec.voices.exists()  # nothing staged


def test_forget_before_staging_refuses_to_stage(audio_rec, monkeypatch):
    monkeypatch.setattr(server, "_decode_enrollment_audio", _REAL_ENROLL_DECODE)
    calls = []

    def run(data, max_seconds=1200, *, source=None):
        calls.append(source)
        if source is None:  # the forget lands after the pipe attempt, before staging
            assert audio_rec.delete("/api/speakers").json() == {"forgotten": 0}
        return subprocess.CompletedProcess([], 0, b"", b"")

    monkeypatch.setattr(server, "_run_ffmpeg", run)
    resp = _upload(audio_rec, data=_BMFF_CLIP)
    assert resp.status_code == 409 and "forgotten" in resp.json()["detail"]
    assert calls == [None]  # no staged decode was attempted
    assert not audio_rec.voices.exists() or list(audio_rec.voices.iterdir()) == []
