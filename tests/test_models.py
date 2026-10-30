import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
from types import SimpleNamespace

import pytest

from localflow import models
from localflow.config import Config
from localflow.stt import _MLX_REPOS


def test_catalog_repos_are_consistent_and_ids_unique():
    ids = [entry.id for entry in models.STT_MODELS]
    assert len(ids) == len(set(ids))
    assert {entry.repo for entry in models.STT_MODELS if entry.id.startswith("fw-")} == {
        models._MODELS[size] for size, *_ in models._FW_CATALOG
    }
    assert {entry.repo for entry in models.STT_MODELS if entry.id.startswith("mlx-")} == set(
        _MLX_REPOS.values()
    )
    assert models._STT_BY_ID["mlx-large-v3-turbo"].repo == (
        "mlx-community/whisper-large-v3-turbo"
    )
    assert models._STT_BY_ID["parakeet-v2"].english_only
    assert models._STT_BY_ID["fw-distil-large-v3"].english_only
    assert len({entry.id for entry in models.LLM_MODELS}) == len(models.LLM_MODELS)


@pytest.mark.parametrize(
    ("hw", "expected"),
    [
        (models.HardwareInfo("Linux", "x86_64", 4, False, 10), ("fw-tiny", None)),
        (models.HardwareInfo("Linux", "x86_64", 8, False, 10), ("fw-base", "llama3.2:1b")),
        (models.HardwareInfo("Darwin", "arm64", 8, True, 10), ("mlx-base", "llama3.2:1b")),
        (models.HardwareInfo("Darwin", "arm64", 16, True, 10), ("mlx-small", "llama3.2:3b")),
    ],
)
def test_recommendations(hw, expected):
    assert models.recommend(hw) == expected


def test_available_stt_models_filters_apple_silicon_only():
    linux = models.available_stt_models(models.HardwareInfo("Linux", "x86_64", 8, False, 10))
    mac = models.available_stt_models(models.HardwareInfo("Darwin", "arm64", 8, True, 10))

    assert all(not entry.apple_silicon_only for entry in linux)
    assert len(mac) == len(models.STT_MODELS)


def test_detect_hardware_uses_platform_memory_and_disk(monkeypatch, tmp_path):
    monkeypatch.setattr(models.platform, "system", lambda: "Linux")
    monkeypatch.setattr(models.platform, "machine", lambda: "x86_64")
    monkeypatch.setattr(models.os, "sysconf", lambda key: 4096 if key == "SC_PAGE_SIZE" else 1024**2)
    monkeypatch.setattr(models.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(models.shutil, "disk_usage", lambda _path: SimpleNamespace(free=2**30))

    info = models.detect_hardware()

    assert info.ram_gb == 4.0
    assert info.free_disk_gb == 1.0
    assert not info.apple_silicon


def test_detect_hardware_uses_windows_global_memory_status(monkeypatch, tmp_path):
    class Kernel32:
        @staticmethod
        def GlobalMemoryStatusEx(status):
            status._obj.ullTotalPhys = 8 * 1024**3
            return 1

    monkeypatch.setattr(models.platform, "system", lambda: "Windows")
    monkeypatch.setattr(models.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(
        models.ctypes, "windll", SimpleNamespace(kernel32=Kernel32()), raising=False
    )
    monkeypatch.setattr(models.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(models.shutil, "disk_usage", lambda _path: SimpleNamespace(free=2**30))

    info = models.detect_hardware()

    assert info.ram_gb == 8.0
    assert info.machine == "AMD64"


def test_detect_hardware_falls_back_when_ram_probe_fails(monkeypatch, tmp_path):
    monkeypatch.setattr(models.platform, "system", lambda: "Linux")
    monkeypatch.setattr(models.platform, "machine", lambda: "x86_64")
    monkeypatch.setattr(models.os, "sysconf", lambda _key: (_ for _ in ()).throw(OSError("no probe")))
    monkeypatch.setattr(models.Path, "home", lambda: tmp_path)
    monkeypatch.setattr(models.shutil, "disk_usage", lambda _path: SimpleNamespace(free=2**30))

    info = models.detect_hardware()

    assert info.ram_gb == 0.0


def test_current_stt_id_resolves_config_backend(monkeypatch):
    assert models.current_stt_id(Config(stt_backend="parakeet")) == "parakeet-v2"
    assert models.current_stt_id(Config(stt_backend="faster-whisper", model_size="tiny")) == "fw-tiny"
    assert models.current_stt_id(Config(stt_backend="mlx", model_size="small")) == "mlx-small"
    monkeypatch.setattr(models.platform, "system", lambda: "Linux")
    assert models.current_stt_id(Config(stt_backend="auto", model_size="small")) == "fw-small"


def test_stt_config_uses_nested_provider_ids():
    assert models.stt_config(models._STT_BY_ID["mlx-small"]) == {
        "stt": {"provider": "mlx-whisper", "model": "small"}
    }


def test_download_stt_reports_monotone_progress_and_total(tmp_path, monkeypatch):
    files = [
        SimpleNamespace(rfilename="weights.bin", size=12),
        SimpleNamespace(rfilename="config.json", size=8),
    ]
    monkeypatch.setattr(models.HfApi, "model_info", lambda *_a, **_k: SimpleNamespace(siblings=files))
    downloaded = []

    def hf_download(repo_id, filename):
        target = tmp_path / filename
        target.write_bytes(b"x")
        downloaded.append((repo_id, filename))
        return str(target)

    monkeypatch.setattr(models, "hf_hub_download", hf_download)
    progress = []
    model = models._STT_BY_ID["fw-tiny"]

    result = models.download_stt(model, lambda done, total: progress.append((done, total)))

    assert result == tmp_path / "config.json"
    assert downloaded == [(model.repo, "weights.bin"), (model.repo, "config.json")]
    assert [done for done, _total in progress] == sorted(done for done, _total in progress)
    assert all(total == 20 for _done, total in progress)
    assert progress[-1][0] == 20


def test_download_stt_cancels_between_files(tmp_path, monkeypatch):
    files = [
        SimpleNamespace(rfilename="first.bin", size=12),
        SimpleNamespace(rfilename="second.bin", size=8),
    ]
    monkeypatch.setattr(models.HfApi, "model_info", lambda *_a, **_k: SimpleNamespace(siblings=files))
    cancel = threading.Event()
    downloaded = []

    def hf_download(repo_id, filename):
        downloaded.append(filename)
        target = tmp_path / filename
        target.write_bytes(b"x")
        cancel.set()
        return str(target)

    monkeypatch.setattr(models, "hf_hub_download", hf_download)
    with pytest.raises(models.DownloadCancelled):
        models.download_stt(
            models._STT_BY_ID["fw-tiny"], lambda *_args: None, cancel=cancel
        )
    assert downloaded == ["first.bin"]


@pytest.fixture
def ollama_server():
    class Handler(BaseHTTPRequestHandler):
        events = []
        request_error = False

        def log_message(self, *_args):
            pass

        def do_GET(self):
            body = json.dumps({"models": [{"name": "llama3.2:1b"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            self.rfile.read(length)
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson")
            self.end_headers()
            for event in self.events:
                self.wfile.write(json.dumps(event).encode() + b"\n")
                self.wfile.flush()

    Handler.events = []
    Handler.request_error = False
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", Handler
    finally:
        server.shutdown()
        thread.join(timeout=2)
        server.server_close()


def test_ollama_client_tags_and_pull_progress_aggregate_by_digest(ollama_server):
    url, handler = ollama_server
    handler.events = [
        {"status": "pulling", "digest": "a", "completed": 4, "total": 10},
        {"status": "pulling", "digest": "a", "completed": 10, "total": 10},
        {"status": "pulling", "digest": "b", "completed": 3, "total": 5},
    ]
    client = models.OllamaClient(url)
    assert client.is_running()
    assert client.installed() == ["llama3.2:1b"]
    progress = []

    client.pull("llama3.2:1b", lambda done, total, status: progress.append((done, total, status)))

    assert progress == [(4, 10, "pulling"), (10, 10, "pulling"), (13, 15, "pulling")]


def test_ollama_pull_error_line_becomes_plain_download_error(ollama_server):
    url, handler = ollama_server
    handler.events = [{"error": "manifest not found"}]

    with pytest.raises(models.DownloadError, match="manifest not found"):
        models.OllamaClient(url).pull("missing:tag", lambda *_args: None)


def test_ollama_pull_cancellation(ollama_server):
    url, handler = ollama_server
    handler.events = [
        {"status": "pulling", "digest": "a", "completed": 1, "total": 2},
        {"status": "done"},
    ]
    cancel = threading.Event()

    def progress(*_args):
        cancel.set()

    with pytest.raises(models.DownloadCancelled):
        models.OllamaClient(url).pull("llama3.2:1b", progress, cancel)
