"""Real HTTP contracts for Python uvicorn and the optional `lf-rs serve`.

Uses the Stage 1 build/sandbox fixtures; skips without cargo + a prebuilt binary,
or fails with LF_RUST_PARITY_REQUIRED=1. No models, microphone or real keychain.
"""
from __future__ import annotations

import contextlib
import json
import socket
import subprocess
import sys
import time
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
    token_path = Path(subprocess.check_output([sys.executable, "-c", "from localflow.apitoken import token_path; print(token_path())"], env=env, cwd=ROOT, text=True).strip())
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
    assert all(not p["available"] for p in rs)
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
    assert rust.request("/api/recall", "POST", {"question": "test"})[:2] == (503, {"detail": "recall backend unavailable"})
    assert rust.request("/meeting/start", "POST", {})[:2] == (503, {"detail": "meeting backend unavailable"})


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
