import json
import re
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
from localflow.config import Config
from localflow.diarization import collector as collector_module
from localflow.diarization import factory as factory_module
from localflow.diarization.collector import DiarizationCollector, DiarizationResult
from localflow.diarization.mock import MockDiarizer, MockEmbedder, tone_index
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
def client(monkeypatch, tmp_path):
    templates = tmp_path / "templates"
    templates.mkdir()
    (templates / "retro.md").write_text("---\nname: Retro\n---\n## Went Well\nBullets.\n")
    config = Config(
        meetings_enabled=False, meeting_watch=False, sounds_enabled=False,
        work_prompts=False, vault_path=str(tmp_path / "vault"),
        meeting_templates_dir=str(templates),
    )
    llm = FakeLLM("## Summary\nnotes")
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
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234),
        headers={"X-Localflow-Token": "test-token"},
    ) as c:
        c.llm = llm
        c.tmp = tmp_path
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
    monkeypatch.setattr(server._config, "speaker_diarization", True)
    start = client.post("/meeting/start", json={"title": "Launch sync"})
    assert start.status_code == 200
    body = start.json()
    assert body["diarization"] == "unavailable" and "sherpa-onnx" in body["diarization_error"]
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
