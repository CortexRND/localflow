"""Real HTTP contracts for Python uvicorn and the optional `lf-rs serve`.

Uses the Stage 1 build/sandbox fixtures; skips without cargo + a prebuilt binary,
or fails with LF_RUST_PARITY_REQUIRED=1. No models, microphone or real keychain.
"""
from __future__ import annotations

import contextlib
import io
import json
import socket
import struct
import subprocess
import sys
import time
import tomllib
import wave
from dataclasses import dataclass
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import pytest
import test_rust_parity as parity

LF_RS, ROOT = parity.LF_RS, parity.ROOT
rust_build, sandbox = parity.rust_build, parity.sandbox


def _free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _token_path(env):
    return Path(subprocess.check_output(
        [sys.executable, "-c", "from localflow.apitoken import token_path; print(token_path())"],
        env=env, cwd=ROOT, text=True,
    ).strip())


@dataclass
class Endpoint:
    port: int
    process: subprocess.Popen
    token_path: Path

    def request(self, path, method="GET", body=None, *, auth=True, headers=None):
        headers = dict(headers or {})
        if auth:
            headers.setdefault("X-Localflow-Token", self.token_path.read_text().strip())
        if isinstance(body, (dict, list, int, bool)):
            body = json.dumps(body).encode()
            headers.setdefault("Content-Type", "application/json")
        if isinstance(body, str):
            body = body.encode()
        request = Request(f"http://127.0.0.1:{self.port}{path}", body, headers, method=method)
        try:
            response = urlopen(request, timeout=5)
        except HTTPError as exc:
            response = exc
        with response:
            raw = response.read().decode()
            content = json.loads(raw) if "application/json" in response.headers.get("Content-Type", "") else raw
            return response.status, content, response.headers


@contextlib.contextmanager
def _start(command, env, token_path, log_path, port):
    with log_path.open("wb") as log:
        process = subprocess.Popen(command, env=env, cwd=ROOT, stdout=log, stderr=log)
        endpoint = Endpoint(port, process, token_path)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    pytest.fail(f"server exited {process.returncode}: {log_path.read_text(errors='replace')[-4000:]}")
                try:
                    if endpoint.request("/healthz", auth=False)[0] == 200:
                        break
                except (OSError, URLError):
                    time.sleep(0.05)
            else:
                pytest.fail("server did not become healthy within 30s")
            yield endpoint
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


@pytest.fixture
def servers(rust_build, sandbox):
    env = sandbox["env"].copy()
    env["PYNPUT_BACKEND"] = "dummy"
    config = sandbox["config"]
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text('config_version = 2\n[features]\nmeetings = false\n[meetings]\nwatch = false\n', encoding="utf-8")
    # Both processes use the same fixture config/token locations. Python creates
    # its per-boot token first; Rust must READ it unchanged on startup.
    py_port = _free_port()
    token_path = _token_path(env)
    command = [sys.executable, "-m", "uvicorn", "localflow.server:app", "--host", "127.0.0.1", "--port", str(py_port), "--log-level", "error", "--no-access-log"]
    with _start(command, env, token_path, sandbox["root"] / "python-server.log", py_port) as python:
        before = token_path.read_bytes()
        rs_port = _free_port()
        with _start([str(LF_RS), "serve", "--host", "127.0.0.1", "--port", str(rs_port)], env, token_path, sandbox["root"] / "rust-server.log", rs_port) as rust:
            assert token_path.read_bytes() == before
            yield python, rust


def compare(servers, path, method="GET", body=None, **kwargs):
    responses = [server.request(path, method, body, **kwargs) for server in servers]
    assert responses[0][:2] == responses[1][:2]
    return responses


def test_public_and_auth_contract(servers):
    compare(servers, "/healthz", auth=False)
    compare(servers, "/favicon.ico", auth=False)
    for path in ["/settings", "/api/config", "/api/status", "/api/providers/stt", "/api/providers/llm", "/api/models", "/api/secrets", "/api/autostart", "/meeting/templates", "/meeting/status", "/prompts", "/docs", "/openapi.json", "/unknown", "/%61pi/config"]:
        compare(servers, path, auth=False)
        compare(servers, path, auth=False, headers={"Host": "remote.example"})
        for host in ["127.0.0.1:8756/?", "127.0.0.1:8756/#"]:
            compare(servers, path, auth=False, headers={"Host": host})
    for path in ["/transcribe", "/meeting/start", "/meeting/notes", "/meeting/chat", "/meeting/stop", "/meeting/dismiss", "/api/recall", "/prompts/id/approve", "/prompts/id/reject", "/api/launch-code", "/api/test/llm"]:
        compare(servers, path, "POST", {}, auth=False)
    for path in ["/api/config", "/api/secrets/llm_api_key"]:
        compare(servers, path, "PUT", {}, auth=False)
    compare(servers, "/api/config", headers={"Host": "remote.example"})
    token = servers[0].token_path.read_text().strip()
    compare(servers, "/api/config?" + urlencode({"token": token}), auth=False)
    compare(servers, "/api/config?" + urlencode({"token": token}), auth=False, headers={"X-Localflow-Token": "wrong"})


def test_embedded_ui_and_request_token(servers):
    for path in ["/", "/settings"]:
        responses = compare(servers, path, auth=path != "/")
        for _, _, headers in responses:
            assert headers["Cache-Control"] == "no-store"
    compare(servers, "/settings", headers={"X-Localflow-Token": "wrong"})


def test_configuration_and_provider_contract(servers):
    compare(servers, "/api/config")
    compare(servers, "/api/providers/llm")
    # STT availability reflects the linked runtime, not Python wheel installs.
    py = servers[0].request("/api/providers/stt")[1]
    rs = servers[1].request("/api/providers/stt")[1]
    assert [{k: v for k, v in x.items() if k != "available"} for x in py] == [{k: v for k, v in x.items() if k != "available"} for x in rs]
    assert all(p["available"] for p in rs)
    py_status = servers[0].request("/api/status")[1]
    rs_status = servers[1].request("/api/status")[1]
    assert rs_status.pop("autostart") == "unavailable"
    py_status.pop("autostart")  # platform integration is an explicit backend gap
    assert py_status == rs_status
    compare(servers, "/api/models")
    compare(servers, "/api/models?kind=stt")
    for provider in ["auto", "faster-whisper", "mlx", "mlx-whisper", "parakeet", "bogus"]:
        compare(servers, "/api/models?" + urlencode({"kind": "stt", "provider": provider}))
    compare(servers, "/api/models?kind=bad&provider=bogus")
    compare(servers, "/api/models?kind=llm&provider=bogus")
    compare(servers, "/api/test/llm", "POST", {"provider": "bogus"})
    for update in [
        {"dictation": {"model_size": "tiny"}},
        {"llm": {"model": "another-model"}, "server": {"port": 9001}},
        {"features": {"meetings": True}, "hotkey": {"key": "ctrl+space"}},
        {"dictation": {"sample_rate": 22050, "pause_media": False}},
        {"dictation": {"enabled": False}},
        {"dictation": {"enabled": True}},
        {"nonsense": {}}, {"dictation": {"nonsense": True}},
        {"server": {"port": -1}}, {"stt": {}},
    ]:
        compare(servers, "/api/config", "PUT", update)
        compare(servers, "/api/config")
        assert servers[0].request("/api/status")[1]["dictation_enabled"] == servers[1].request("/api/status")[1]["dictation_enabled"]
    compare(servers, "/api/config", "PUT")
    compare(servers, "/api/config", "PUT", [])
    compare(servers, "/api/autostart", "POST", {"enabled": "true"})


def test_rust_put_keeps_config_edits_made_after_startup(servers, sandbox):
    rust, config = servers[1], sandbox["config"]
    config.write_text(
        'config_version = 2\n[features]\nmeetings = false\n[meetings]\nwatch = false\n[llm]\nmodel = "edited-outside"\n',
        encoding="utf-8",
    )
    status, put, _ = rust.request("/api/config", "PUT", {"dictation": {"sample_rate": 22050}})
    assert status == 200, put
    saved = tomllib.loads(config.read_text(encoding="utf-8"))
    for values in [put, rust.request("/api/config")[1], saved]:
        assert values["llm"]["model"] == "edited-outside"
        assert values["dictation"]["sample_rate"] == 22050
        assert values["features"]["meetings"] is False


def test_rust_put_rejects_unreadable_config_without_changes(servers, sandbox):
    rust, config = servers[1], sandbox["config"]
    remembered = rust.request("/api/config")[1]
    broken = b"config_version = 2\n[llm\nmodel = \n"
    config.write_bytes(broken)
    status, body, _ = rust.request("/api/config", "PUT", {"dictation": {"sample_rate": 22050}})
    assert (status, body) == (503, {"detail": "could not load config"})
    assert config.read_bytes() == broken
    assert rust.request("/api/config")[:2] == (200, remembered)


def test_secrets_reject_unknown_and_never_expose_values(servers):
    compare(servers, "/api/secrets")
    compare(servers, "/api/secrets/bogus", "PUT", {"value": "fixture-only"})
    compare(servers, "/api/secrets/llm_api_key", "PUT", {"value": 123})
    for server in servers:
        status = server.request("/api/secrets")[1]
        assert all(value is False for value in status.values())


def test_launch_codes_expire_or_replay_without_exposing_token(servers):
    token = servers[0].token_path.read_text().strip()
    for server in servers:
        status, issued, _ = server.request("/api/launch-code", "POST", headers={"X-Localflow-Token": token})
        assert status == 200 and issued["expires_in"] == 60 and len(issued["code"]) == 43
        form = urlencode({"launch": issued["code"]})
        headers = {"Content-Type": "application/x-www-form-urlencoded"}
        assert server.request("/api/config", "POST", form, auth=False, headers=headers)[0] == 401
        assert server.request("/settings", "POST", {"launch": issued["code"]}, auth=False)[0] == 401
        status, page, response_headers = server.request("/settings", "POST", form, auth=False, headers=headers)
        assert status == 200 and token in page and response_headers["Cache-Control"] == "no-store"
        # Explicit replay rotates; startup itself did not. Each server owns an
        # independent in-memory token. Do not compare them after this point.
        status, body, _ = server.request("/settings", "POST", form, auth=False, headers=headers)
        assert (status, body) == (401, {"detail": "launch link expired or already used"})
        assert server.token_path.read_text().strip() != token
        assert server.request("/api/config", auth=False, headers={"X-Localflow-Token": token})[0] == 401


def test_upload_limit_and_error_routes(servers):
    compare(servers, "/transcribe", "POST", auth=True, headers={"Content-Length": str(25 * 1024 * 1024 + 1)})
    compare(servers, "/api/config", "POST", {})
    compare(servers, "/not-a-route")
    rust = servers[1]
    assert rust.request("/api/recall", "POST", {"question": ""})[:2] == (400, {"detail": "question is empty"})
    assert rust.request("/meeting/start", "POST", {"template": "missing"})[:2] == (400, {"detail": "unknown template 'missing'"})


def test_dictation_decodes_silent_wav_without_loading_a_model(servers):
    rust = servers[1]
    wav = io.BytesIO()
    with wave.open(wav, "wb") as audio:
        audio.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
        audio.writeframes(bytes(16000 * 2))
    upload = (
        b'--lf-test\r\nContent-Disposition: form-data; name="file"; filename="silence.wav"\r\n'
        b"Content-Type: audio/wav\r\n\r\n" + wav.getvalue() + b"\r\n--lf-test--\r\n"
    )
    status, body, _ = rust.request("/transcribe", "POST", upload, headers={
        "Content-Type": "multipart/form-data; boundary=lf-test",
    })
    assert status == 200
    assert body["text"] == ""
    assert isinstance(body["ms"], int)
    assert rust.request("/api/status")[1]["stt"]["loaded"] is False


def test_meetings_reads_local_templates_and_handles_session_requests(servers, sandbox):
    rust = servers[1]
    templates = sandbox["home"] / ".localflow" / "meeting_templates"
    templates.mkdir(parents=True, exist_ok=True)
    (templates / "sandbox.md").write_text(
        "---\nname: Sandbox meeting\n---\n## Decisions\n", encoding="utf-8",
    )
    status, body, _ = rust.request("/meeting/templates")
    assert status == 200
    template = next(item for item in body["templates"] if item["id"] == "sandbox")
    assert template["name"] == "Sandbox meeting"
    assert template["sections"] == ["Decisions"]

    assert rust.request("/api/config", "PUT", {
        "features": {"meetings": True},
        "dictation": {"input_device": "localflow-test-missing-microphone"},
    })[0] == 200
    status, body, _ = rust.request("/meeting/start", "POST", {"template": "sandbox"})
    assert status == 500
    assert body["detail"].startswith("mic open failed: ")
    assert "transcriber unavailable" not in body["detail"]
    status, body, _ = rust.request("/meeting/status")
    assert status == 200
    assert body["session"] is None
    assert rust.request("/meeting/stop", "POST")[:2] == (409, {"detail": "no meeting session running"})


def _seed_empty_whisper_model(hub):
    """Cache a header-only `ggml-tiny.bin`: whisper.cpp loads a model file with
    no tensors as an empty test model, so nothing is downloaded."""
    repo, revision = hub / "models--ggerganov--whisper.cpp", "0" * 40
    snapshot = repo / "snapshots" / revision
    snapshot.mkdir(parents=True)
    (repo / "refs").mkdir()
    (repo / "refs" / "main").write_text(revision, encoding="ascii")
    n_mel, n_fft = 80, 201
    # magic; n_vocab, audio ctx/state/head/layer, text ctx/state/head/layer, n_mels, ftype (f16)
    header = struct.pack("<I11i", 0x67676D6C, 51864, 1500, 64, 1, 4, 448, 64, 1, 4, n_mel, 1)
    # mel filters, then an empty vocabulary (whisper.cpp fills in the tokens)
    filters = struct.pack("<2i", n_mel, n_fft) + bytes(4 * n_mel * n_fft)
    (snapshot / "ggml-tiny.bin").write_bytes(header + filters + struct.pack("<i", 0))


@contextlib.contextmanager
def _virtual_mic_server(sandbox, *, model):
    """`lf-rs serve` with an ALSA /dev/zero mic, offline, with or without a cached speech model."""
    alsa = sandbox["root"] / "asound.conf"
    alsa.write_text(
        'pcm.!default { type file slave.pcm { type null } file "/dev/null" infile "/dev/zero" format "raw" }\n',
        encoding="utf-8",
    )
    hub = sandbox["root"] / "hf-hub"
    hub.mkdir()
    if model:
        _seed_empty_whisper_model(hub)
    env = {
        **sandbox["env"], "ALSA_CONFIG_PATH": str(alsa), "HF_HUB_CACHE": str(hub),
        # A closed loopback port: any download attempt fails at once.
        "HF_ENDPOINT": f"http://127.0.0.1:{_free_port()}",
    }
    config = sandbox["config"]
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        'config_version = 2\n[features]\nmeetings = true\n[stt]\nmodel = "tiny"\n'
        '[meetings]\nwatch = false\nwork_prompts = false\nvault_path = "~/vault"\n',
        encoding="utf-8",
    )
    token_path = _token_path(env)
    token_path.parent.mkdir(parents=True, exist_ok=True)
    token_path.write_text("localflow-http-test-token\n", encoding="utf-8")
    port = _free_port()
    command = [str(LF_RS), "serve", "--host", "127.0.0.1", "--port", str(port)]
    with _start(command, env, token_path, sandbox["root"] / "rust-server.log", port) as rust:
        yield rust


@contextlib.contextmanager
def _virtual_mic_meeting(sandbox, title, notes):
    """A started meeting holding notes on `_virtual_mic_server` with the empty model."""
    with _virtual_mic_server(sandbox, model=True) as rust:
        status, body, _ = rust.request("/meeting/start", "POST", {"title": title})
        assert status == 200, body
        assert body["ok"] is True
        assert rust.request("/meeting/status")[1]["session"]["title"] == title

        assert rust.request("/meeting/notes", "POST", {"notes": notes})[0] == 200
        yield rust


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Uses an ALSA virtual microphone")
def test_meeting_lifecycle_with_virtual_microphone(rust_build, sandbox):
    notes = "Discussed next steps."
    with _virtual_mic_meeting(sandbox, "Sandbox meeting", notes) as rust:
        status, body, _ = rust.request("/meeting/stop", "POST")
        assert status == 200, body
        assert body["segments"] == 0
        notes_path = Path(body["notes_path"])
        assert notes_path.is_relative_to(sandbox["home"] / "vault")
        assert notes in notes_path.read_text(encoding="utf-8")
        assert rust.request("/meeting/status")[1]["session"] is None


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Uses an ALSA virtual microphone")
def test_sigterm_during_meeting_persists_notes(rust_build, sandbox):
    title, notes = "Interrupted meeting", "Agreed to ship on Friday."
    vault = sandbox["home"] / "vault"

    def saved():
        # The writer pairs each meeting note with a "<name> (transcript).md" log.
        return sorted(vault.rglob("*.md"))

    with _virtual_mic_meeting(sandbox, title, notes) as rust:
        # Still recording and nothing written yet: only shutdown can persist it.
        assert rust.request("/meeting/status")[1]["session"]["title"] == title
        assert not saved()
        rust.process.terminate()  # SIGTERM; /meeting/stop is never requested
        assert rust.process.wait(timeout=30) == 0
    files = saved()
    (notes_path,) = [p for p in files if not p.stem.endswith(" (transcript)")]
    (log_path,) = [p for p in files if p.stem.endswith(" (transcript)")]
    assert log_path.stem == f"{notes_path.stem} (transcript)"
    text = notes_path.read_text(encoding="utf-8")
    assert title in text and notes in text


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Uses an ALSA virtual microphone")
def test_meeting_start_fails_when_the_speech_model_cannot_load(rust_build, sandbox):
    with _virtual_mic_server(sandbox, model=False) as rust:
        status, body, _ = rust.request("/meeting/start", "POST", {"title": "No model"})
        assert status == 500, body
        assert body["detail"].startswith("mic open failed: could not download ")
        assert rust.request("/meeting/status")[1]["session"] is None
        assert rust.request("/meeting/stop", "POST")[:2] == (409, {"detail": "no meeting session running"})
    assert not list((sandbox["home"] / "vault").rglob("*"))


SPEAKER_ROUTES = [
    ("GET", "/api/speakers"), ("POST", "/api/speakers"), ("DELETE", "/api/speakers"),
    ("GET", "/api/speakers/export"), ("PATCH", "/api/speakers/Ana"), ("DELETE", "/api/speakers/Ana"),
]


def test_speaker_routes_auth_gates_and_unknown_meetings_match_python(servers):
    """Recognition is off in the shared fixture: every voiceprint route is 403 on
    both servers (before any body is read), every new route needs the token."""
    meeting = "/api/meetings/" + "a" * 32 + "/speakers"
    for method, path in SPEAKER_ROUTES:
        compare(servers, path, method, {}, auth=False)
        compare(servers, path, method, {})
    compare(servers, "/api/speakers", "POST", "x" * 200_000, headers={"Content-Type": "application/json"})
    for method, path, body in [("GET", meeting, None), ("PUT", meeting + "/S1", {"name": "Dana"}),
                               ("POST", meeting + "/merge", {"from": "S1", "into": "S2"}),
                               ("GET", "/api/meetings/not-an-id/speakers", None)]:
        compare(servers, path, method, body, auth=False)
        compare(servers, path, method, body)
    statuses = [server.request("/api/diarization/status") for server in servers]
    assert [s[0] for s in statuses] == [200, 200]
    py, rs = (s[1] for s in statuses)
    assert list(py) == list(rs)
    assert (py["models_ready"], py["reason"]) == (rs["models_ready"], rs["reason"])
    assert [{k: m[k] for k in m if k != "reason"} for m in py["models"]] == \
        [{k: m[k] for k in m if k != "reason"} for m in rs["models"]]


@pytest.fixture
def speaker_server(rust_build, sandbox):
    """`lf-rs serve` alone, with both speaker flags on and a temp vault."""
    env = {**sandbox["env"], "PYNPUT_BACKEND": "dummy"}
    vault = sandbox["root"] / "vault"
    vault.mkdir()
    config = sandbox["config"]
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text(
        'config_version = 2\n[features]\nmeetings = false\n'
        f'[meetings]\nwatch = false\nvault_path = {json.dumps(str(vault))}\n'
        'speaker_diarization = true\nspeaker_recognition = true\ndiarization_backend = "mock"\n',
        encoding="utf-8",
    )
    token_path = _token_path(env)
    token_path.parent.mkdir(parents=True, exist_ok=True)
    token_path.write_text("localflow-http-test-token\n", encoding="utf-8")
    port = _free_port()
    command = [str(LF_RS), "serve", "--host", "127.0.0.1", "--port", str(port)]
    with _start(command, env, token_path, sandbox["root"] / "rust-server.log", port) as rust:
        yield rust, vault


def _python_meeting(sandbox, vault):
    """A diarized meeting saved by the Python writer + store, with voice data."""
    from datetime import datetime

    from localflow import meetings
    from localflow.diarization.store import MeetingSpeakerStore, build_record

    segments = [
        meetings.Segment(0.0, "we ship Friday", "me"),
        meetings.Segment(3.0, "pricing is done", "others", 5.0, "S1", "Speaker 1"),
        meetings.Segment(6.0, "docs too", "others", 8.0, "S2", "Speaker 2"),
        meetings.Segment(9.0, "and the blog", "others", 11.0, "S3", "Speaker 3"),
        meetings.Segment(70.0, "agreed", "others", 72.0, "S1", "Speaker 1"),
    ]
    saved = meetings.ObsidianWriter(str(vault), "Notes", "Transcripts").write(
        title="Launch", category="Zoom", started_at=datetime(2026, 11, 25, 10, 30),
        duration_seconds=125.0, notes_md="## Summary\nShip", segments=segments,
        user_notes="- **00:03** **Speaker 1:** not a transcript line", template="general")
    assert saved.meeting_id
    centroid = [0.0, 0.6, 0.8, 0.0]
    speakers = [
        {"id": "S1", "source": "others", "label": "Speaker 1", "seconds": 4.0},
        {"id": "S2", "source": "others", "label": "Speaker 2", "seconds": 2.0, "centroid": centroid},
        {"id": "S3", "source": "others", "label": "Speaker 3", "seconds": 2.0, "centroid": [1.0, 0.0, 0.0, 0.0]},
    ]
    store = MeetingSpeakerStore(root=sandbox["home"] / ".localflow" / "meetings")
    store.save(build_record(meeting_id=saved.meeting_id, log_path=str(saved.log_path),
                            notes_path=str(saved.notes_path), started_at="2026-11-25T10:30:00",
                            embedder="mock", status="ok", speakers=speakers, segments=segments))
    return saved, store, centroid


def test_speaker_data_round_trips_between_python_and_rust(speaker_server, sandbox, tmp_path):
    np = pytest.importorskip("numpy")
    import zipfile

    from localflow.diarization import registry as reg
    from localflow.diarization.store import _problem, public_speakers
    from localflow.meetings import read_transcript_log

    rust, vault = speaker_server
    saved, store, centroid = _python_meeting(sandbox, vault)
    mid = saved.meeting_id
    speakers_root = sandbox["home"] / ".localflow" / "speakers"
    registry = reg.FileSpeakerRegistry(root=speakers_root, meetings=store)
    ana = registry.enroll("Ana", [np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float32)], "mock")
    notes_before = Path(saved.notes_path).read_bytes()

    # Python wrote, Rust reads and edits.
    status, body, _ = rust.request(f"/api/meetings/{mid}/speakers")
    assert (status, body) == (200, {"meeting_id": mid, "speakers": public_speakers(store.load(mid))})
    status, body, _ = rust.request(f"/api/meetings/{mid}/speakers/S1", "PUT", {"name": "Dana"})
    assert status == 200, body
    status, body, _ = rust.request(f"/api/meetings/{mid}/speakers/merge", "POST", {"from": "S3", "into": "S2"})
    assert status == 200, body
    record = store.load(mid)  # Rust's rewrite validates in Python
    assert record is not None and _problem(record, mid) is None
    assert [(s["id"], s["label"], s["seconds"]) for s in record["speakers"]] == \
        [("S1", "Dana", 4.0), ("S2", "Speaker 2", 4.0)]
    assert record["speakers"][1]["centroid"] == centroid
    transcript = read_transcript_log(Path(saved.log_path))
    text = Path(saved.log_path).read_text(encoding="utf-8")
    assert "**00:03** **Dana:** pricing is done" in transcript
    assert "**01:10** **Dana:** agreed" in transcript
    assert "**00:09** **Speaker 2:** and the blog" in transcript
    assert 'speakers: ["Me", "Dana", "Speaker 2"]' in text
    assert "- **00:03** **Speaker 1:** not a transcript line" in text, "raw notes are not relabelled"
    assert Path(saved.notes_path).read_bytes() == notes_before

    # Voiceprints: Python-enrolled read by Rust; Rust-enrolled read by Python.
    status, body, _ = rust.request("/api/speakers")
    assert (status, body) == (200, {"speakers": [reg.public(ana)], "embedder": "mock"})
    status, bo, _ = rust.request("/api/speakers", "POST", {"name": "Bo", "meeting_id": mid, "speaker_id": "S2"})
    assert status == 201, bo
    assert [vp.name for vp in registry.list()] == ["Ana", "Bo"]
    vector = np.load(speakers_root / f"{bo['id']}.npy", allow_pickle=False)
    assert vector.dtype == np.float32 and vector.shape == (4,)
    assert abs(float(np.linalg.norm(vector)) - 1.0) < 1e-6
    assert np.allclose(vector, np.asarray(centroid, dtype=np.float32), atol=1e-6)
    expected = (speakers_root / f"{bo['id']}.npy").read_bytes()
    import io
    buf = io.BytesIO()
    np.save(buf, vector, allow_pickle=False)
    assert expected == buf.getvalue(), "the .npy bytes are exactly NumPy's"
    assert registry.match(np.asarray(centroid, dtype=np.float32), "mock")[0].id == bo["id"]
    assert store.load(mid)["speakers"][1]["voiceprint_id"] == bo["id"]
    status, renamed, _ = rust.request(f"/api/speakers/{bo['id']}", "PATCH", {"name": "Bo B"})
    assert (status, renamed["name"]) == (200, "Bo B")
    assert registry.find(bo["id"]).name == "Bo B"

    # Export: only on this computer; Python reads Rust's ZIP.
    for headers in [{"Host": "remote.example"}, {"X-Forwarded-For": "10.0.0.9"}, {"Via": "1.1 proxy"}]:
        status, body, _ = rust.request("/api/speakers/export", headers=headers)
        assert (status, body) == (403, {"detail": "export is only available on this computer"})
    status, data, headers = _raw_get(rust, "/api/speakers/export")
    assert status == 200
    assert headers["Content-Type"] == "application/zip"
    assert headers["Cache-Control"] == "no-store"
    assert headers["X-Content-Type-Options"] == "nosniff"
    assert headers["Content-Disposition"] == 'attachment; filename="localflow-speakers.zip"'
    with zipfile.ZipFile(io.BytesIO(data)) as zf:
        assert zf.namelist()[0] == "registry.json"
        assert sorted(zf.namelist()[1:]) == sorted(f"{i}.npy" for i in (ana.id, bo["id"]))
    out, count = reg.write_bundle(data, tmp_path / "py-export")
    assert count == 2

    # Forget across runtimes erases every copy of voice data, staged audio included.
    staged = speakers_root / f".enroll-{'e' * 32}.audio"
    staged.write_bytes(b"left by a killed server")
    status, body, _ = rust.request("/api/speakers", "DELETE")
    assert (status, body) == (200, {"forgotten": 2})
    assert registry.list() == [] and not list(speakers_root.glob("*.npy")) and not staged.exists()
    for sp in store.load(mid)["speakers"]:
        assert sp["centroid"] is None and sp["voiceprint_id"] is None
    assert "Dana" in Path(saved.log_path).read_text(encoding="utf-8"), "names in transcripts stay"


def _raw_get(endpoint, path):
    request = Request(f"http://127.0.0.1:{endpoint.port}{path}",
                      headers={"X-Localflow-Token": endpoint.token_path.read_text().strip()})
    with urlopen(request, timeout=5) as response:
        return response.status, response.read(), response.headers


def test_rust_cli_writes_a_python_export_and_refuses_an_invalid_one(rust_build, sandbox, tmp_path):
    """`lf-rs speakers export` against a local stub serving Python's export ZIP."""
    np = pytest.importorskip("numpy")
    import http.server
    import threading

    from localflow.diarization import registry as reg
    from localflow.diarization.store import MeetingSpeakerStore

    source = reg.FileSpeakerRegistry(root=tmp_path / "source",
                                     meetings=MeetingSpeakerStore(root=tmp_path / "m"))
    ana = source.enroll("Ana", [np.array([0.0, 3.0, 4.0], dtype=np.float32)], "mock")
    bodies = [source.export_zip(), b"PK\x03\x04 not really"]

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            body = bodies.pop(0)
            self.send_response(200)
            self.send_header("Content-Type", "application/zip")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        config = sandbox["config"]
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(f"config_version = 2\n[server]\nhost = \"localhost\"\nport = {httpd.server_port}\n",
                          encoding="utf-8")
        target = tmp_path / "rust-export"
        out = subprocess.run([str(LF_RS), "speakers", "export", str(target)], env=sandbox["env"],
                             capture_output=True, text=True, timeout=60)
        assert out.returncode == 0, out.stderr
        assert out.stdout == (f"exported 1 speaker(s) to {target}\n"
                              "These files are voiceprints. Keep them private.\n")
        copy = reg.FileSpeakerRegistry(root=target, meetings=MeetingSpeakerStore(root=tmp_path / "m2"))
        assert [reg.public(vp) for vp in copy.list()] == [reg.public(ana)]
        assert copy.match(np.array([0.0, 0.6, 0.8], dtype=np.float32), "mock")[0].id == ana.id
        bad = subprocess.run([str(LF_RS), "speakers", "export", str(tmp_path / "bad")], env=sandbox["env"],
                             capture_output=True, text=True, timeout=60)
        assert (bad.returncode, bad.stderr) == (1, "error: not a speaker export\n")
        assert not (tmp_path / "bad").exists()
    finally:
        httpd.shutdown()


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Linux /proc RSS measurement")
def test_idle_rss(servers):
    """Reports comparable unloaded HTTP hosts, not the full desktop/dictation app."""
    readings = []
    for server in servers:
        text = Path(f"/proc/{server.process.pid}/status").read_text()
        rss_kib = int(next(line.split()[1] for line in text.splitlines() if line.startswith("VmRSS:")))
        readings.append(rss_kib)
    print(f"idle HTTP host RSS: Python={readings[0]} KiB; Rust={readings[1]} KiB")
    assert all(rss > 0 for rss in readings)
