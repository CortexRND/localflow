"""GET /api/diarization/status and POST /api/diarization/download."""

import socket
import threading
import time

import pytest
from fastapi.testclient import TestClient

from localflow import server
from localflow.config import Config
from localflow.diarization import models as dm
from localflow.models import DownloadCancelled, DownloadError

TOKEN = "test-token"
BASE = "http://127.0.0.1:8756"


class FakeSession:
    def __init__(self, transcriber, lock, sample_rate=16000, chunk_seconds=30,
                 system_audio=None):
        self.system_audio_error = ""
        self.system_audio_status = "off"
        self.active = False
        self.segments = []

    def start(self):
        self.active = True

    def elapsed_seconds(self):
        return 1.0

    def stop(self):
        self.active = False
        return self.segments


class RecordingDownload:
    """Stands in for download_diarization_models; records every call."""

    def __init__(self, outcome=None):
        self.calls = []
        self.release = threading.Event()
        self.outcome = outcome

    def __call__(self, on_progress, cancel=None, **kwargs):
        self.calls.append(cancel)
        on_progress(5, 10)
        if self.outcome is None:
            self.release.wait(5)
            return {}
        raise self.outcome


@pytest.fixture
def config(tmp_path):
    return Config(
        meetings_enabled=False, meeting_watch=False, sounds_enabled=False,
        work_prompts=False, vault_path=str(tmp_path / "vault"),
        speaker_diarization=True, diarize_mic=True,
    )


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch, config):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
    # Meeting starts re-read the speaker flags; never from the developer's own file.
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))

    def no_network(url):
        raise AssertionError(f"network: {url}")

    monkeypatch.setattr(dm, "_open_stream", no_network)
    monkeypatch.setattr(server, "_config", config)
    monkeypatch.setattr(server, "_api_token", TOKEN)
    monkeypatch.setattr(server, "load_or_create", lambda: TOKEN)
    monkeypatch.setattr(server, "_diar_download", {
        "state": "idle", "done_bytes": 0, "total_bytes": 0, "error": None,
    })
    monkeypatch.setattr(server, "_diar_cancel", threading.Event())
    monkeypatch.setattr(server, "_diar_thread", None)


@pytest.fixture
def client():
    with TestClient(
        server.app, base_url=BASE, client=("127.0.0.1", 1234),
        headers={"X-Localflow-Token": TOKEN},
    ) as c:
        yield c


def wait_for(client, predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        body = client.get("/api/diarization/status").json()
        if predicate(body["download"]):
            return body
        time.sleep(0.01)
    raise AssertionError(f"timed out; last status {body['download']}")


def test_status_is_offline_and_reports_models_runtime_flags(client, monkeypatch, tmp_path):
    def refuse(*args, **kwargs):
        raise AssertionError("socket opened")

    monkeypatch.setattr(socket, "create_connection", refuse)
    response = client.get("/api/diarization/status")
    assert response.status_code == 200
    body = response.json()
    assert body["models_ready"] is False
    assert "lf speakers download" in body["reason"]
    assert isinstance(body["extra_installed"], bool)
    assert [model["id"] for model in body["models"]] == [m.id for m in dm.DIAR_MODELS]
    assert all(model["ready"] is False for model in body["models"])
    assert body["speaker_diarization"] is True
    assert body["diarize_mic"] is True
    assert body["speaker_recognition"] is False
    assert body["download"] == {
        "state": "idle", "done_bytes": 0, "total_bytes": 0, "error": None,
    }
    assert not (tmp_path / "hub").exists()


@pytest.mark.parametrize("method, path", [
    ("GET", "/api/diarization/status"),
    ("POST", "/api/diarization/download"),
])
def test_routes_require_the_token(monkeypatch, method, path):
    fake = RecordingDownload()
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    with TestClient(server.app, base_url=BASE, client=("127.0.0.1", 1234)) as loopback:
        assert loopback.request(method, path).status_code == 401
    with TestClient(
        server.app, base_url="http://remote.example", client=("10.0.0.5", 1234)
    ) as remote:
        assert remote.request(method, path).status_code == 403
    assert fake.calls == []
    assert server._diar_download["state"] == "idle"


def test_download_progress_is_visible_and_post_is_idempotent(client, monkeypatch):
    fake = RecordingDownload()
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    try:
        first = client.post("/api/diarization/download")
        assert first.status_code == 202
        assert first.json()["started"] is True
        assert first.json()["download"]["state"] == "downloading"
        body = wait_for(client, lambda d: d["done_bytes"] == 5)
        assert body["download"] == {
            "state": "downloading", "done_bytes": 5, "total_bytes": 10, "error": None,
        }
        second = client.post("/api/diarization/download")
        assert second.status_code == 202
        assert second.json()["started"] is False
        assert second.json()["download"]["state"] == "downloading"
    finally:
        fake.release.set()
    wait_for(client, lambda d: d["state"] == "idle")
    assert len(fake.calls) == 1
    assert fake.calls[0] is server._diar_cancel


def test_failed_download_reports_an_error_and_can_be_retried(client, monkeypatch):
    fake = RecordingDownload(DownloadError("Could not download model.onnx: 404 Not Found"))
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    assert client.post("/api/diarization/download").json()["started"] is True
    body = wait_for(client, lambda d: d["state"] != "downloading")
    assert body["download"]["state"] == "failed"
    assert body["download"]["error"]
    assert client.post("/api/diarization/download").json()["started"] is True
    wait_for(client, lambda d: d["state"] == "failed")
    assert len(fake.calls) == 2


def test_unexpected_errors_never_leave_the_state_downloading(client, monkeypatch):
    fake = RecordingDownload(RuntimeError("disk on fire"))
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    client.post("/api/diarization/download")
    body = wait_for(client, lambda d: d["state"] != "downloading")
    assert body["download"]["state"] == "failed"
    assert body["download"]["error"] == "disk on fire"


def test_thread_start_failure_is_reported_and_not_left_downloading(client, monkeypatch):
    fake = RecordingDownload(DownloadError("Could not download model.onnx: HTTP 404"))
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    real_thread = threading.Thread

    class NoDownloadThread(real_thread):
        def start(self):
            if self.name == "diarization-download":
                raise RuntimeError("can't start new thread")
            super().start()

    monkeypatch.setattr(threading, "Thread", NoDownloadThread)
    response = client.post("/api/diarization/download")
    assert response.status_code == 503
    assert response.json()["detail"] == "Could not start the speaker model download. Try again."
    download = client.get("/api/diarization/status").json()["download"]
    assert download["state"] == "failed"
    assert download["error"] == "Could not start the speaker model download. Try again."
    assert fake.calls == []
    monkeypatch.setattr(threading, "Thread", real_thread)
    assert client.post("/api/diarization/download").json()["started"] is True
    wait_for(client, lambda d: d["state"] == "failed")
    assert len(fake.calls) == 1


def test_cancelled_download_is_reported_without_an_error(client, monkeypatch):
    fake = RecordingDownload(DownloadCancelled("Download cancelled"))
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    client.post("/api/diarization/download")
    body = wait_for(client, lambda d: d["state"] != "downloading")
    assert body["download"]["state"] == "cancelled"
    assert body["download"]["error"] is None


def test_shutdown_cancels_a_running_download():
    with TestClient(server.app, base_url=BASE, headers={"X-Localflow-Token": TOKEN}):
        assert not server._diar_cancel.is_set()
    assert server._diar_cancel.is_set()


def test_status_snapshots_the_download_before_hashing(client, monkeypatch):
    server._diar_download.update(state="downloading", done_bytes=10, total_bytes=10)
    real_status = dm.status

    def finishes_while_hashing(*args, **kwargs):
        # The worker publishes the last file and goes idle mid-check.
        with server._diar_lock:
            server._diar_download.update(state="idle")
        return {**real_status(*args, **kwargs), "models_ready": True, "reason": ""}

    monkeypatch.setattr(dm, "status", finishes_while_hashing)
    body = client.get("/api/diarization/status").json()
    assert body["models_ready"] is True
    assert body["download"]["state"] == "downloading"  # stale but harmless: poll again
    assert client.get("/api/diarization/status").json()["download"]["state"] == "idle"


def test_shutdown_waits_for_the_worker_without_holding_the_lock(monkeypatch):
    seen = threading.Event()

    def stops_on_cancel(on_progress, cancel=None, **kwargs):
        on_progress(1, 10)
        seen.set()
        assert cancel.wait(5)
        on_progress(7, 10)  # needs _diar_lock while shutdown is joining
        raise DownloadCancelled("Download cancelled")

    monkeypatch.setattr(dm, "download_diarization_models", stops_on_cancel)
    monkeypatch.setattr(server, "_DIAR_SHUTDOWN_JOIN_SECONDS", 5.0)
    with TestClient(
        server.app, base_url=BASE, client=("127.0.0.1", 1234),
        headers={"X-Localflow-Token": TOKEN},
    ) as c:
        assert c.post("/api/diarization/download").json()["started"] is True
        assert seen.wait(5)
        worker = server._diar_thread
        assert worker is not None and worker.is_alive()
        started = time.monotonic()
    assert time.monotonic() - started < 4  # joined as soon as the worker finished
    assert not worker.is_alive()
    assert server._diar_download == {
        "state": "cancelled", "done_bytes": 7, "total_bytes": 10, "error": None,
    }


def test_shutdown_wait_is_bounded_when_the_worker_is_stuck(monkeypatch):
    release = threading.Event()
    seen = threading.Event()

    def stuck(on_progress, cancel=None, **kwargs):
        seen.set()
        release.wait(10)  # e.g. a read stalled on the network, ignoring cancel
        raise DownloadCancelled("Download cancelled")

    monkeypatch.setattr(dm, "download_diarization_models", stuck)
    monkeypatch.setattr(server, "_DIAR_SHUTDOWN_JOIN_SECONDS", 0.2)
    try:
        with TestClient(
            server.app, base_url=BASE, client=("127.0.0.1", 1234),
            headers={"X-Localflow-Token": TOKEN},
        ) as c:
            c.post("/api/diarization/download")
            assert seen.wait(5)
            started = time.monotonic()
        assert time.monotonic() - started < 3
        assert server._diar_cancel.is_set()
        assert server._diar_thread.is_alive()
        assert server._diar_download["state"] == "downloading"
    finally:
        release.set()
        server._diar_thread.join(5)
    assert server._diar_download["state"] == "cancelled"


def test_nothing_else_downloads_speaker_models(client, monkeypatch):
    fake = RecordingDownload()
    monkeypatch.setattr(dm, "download_diarization_models", fake)
    monkeypatch.setattr(server, "MeetingSession", FakeSession)
    monkeypatch.setattr(server, "_get_transcriber", lambda: object())
    monkeypatch.setattr(server, "_open_system_audio", lambda: (None, ""))
    monkeypatch.setattr(server._watcher, "pause", lambda: None)
    monkeypatch.setattr(server, "_session", None)
    monkeypatch.setattr(server, "_session_meta", {})
    monkeypatch.setattr(server, "_session_state", "idle")
    monkeypatch.setattr(server, "_last_saved", None)
    assert client.get("/meeting/status").status_code == 200
    assert client.post("/meeting/start", json={"title": "Standup"}).status_code == 200
    assert client.get("/meeting/status").status_code == 200
    assert client.get("/api/diarization/status").status_code == 200
    assert fake.calls == []
    assert server._diar_download["state"] == "idle"
