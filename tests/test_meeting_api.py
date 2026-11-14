from datetime import datetime
from pathlib import Path
from typing import ClassVar

import pytest
from fastapi.testclient import TestClient

from localflow import server
from localflow.config import Config
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
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
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


def test_status_reports_chat_available_from_disk_after_restart(client):
    assert client.get("/meeting/status").json()["chat_available"] is False
    cfg = server._config
    ObsidianWriter(cfg.vault_path, cfg.notes_folder, cfg.logs_folder).write(
        title="Old", category="Zoom", started_at=datetime(2026, 11, 13, 9, 0),
        duration_seconds=60, notes_md="ok", segments=[Segment(0, "hi")],
    )
    assert client.get("/meeting/status").json()["chat_available"] is True
