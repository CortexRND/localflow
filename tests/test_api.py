from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest
from fastapi.testclient import TestClient

from localflow import server
from localflow.config import Config
from localflow.history import HistoryStore


@pytest.fixture(autouse=True)
def disable_meeting_watcher(monkeypatch, tmp_path):
    monkeypatch.setattr(
        server, "_config", Config(meetings_enabled=False, meeting_watch=False)
    )
    monkeypatch.setattr(server, "_history", HistoryStore(tmp_path / "history.sqlite"))


class FakePlatform:
    name = "linux"

    def autostart_status(self):
        return "not installed"


def test_settings_and_api_token_auth(monkeypatch):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "current", lambda: FakePlatform())

    for client_ip in ("127.0.0.1", "10.0.0.5"):
        with TestClient(
            server.app, base_url="http://127.0.0.1:8756", client=(client_ip, 1234)
        ) as client:
            assert client.get("/settings").status_code == 401
            assert client.get("/api/status").status_code == 401
            settings = client.get("/settings?token=test-token")
            assert settings.status_code == 200
            assert "test-token" in settings.text
            status = client.get("/api/status", headers={"X-Localflow-Token": "test-token"})
            assert status.status_code == 200
            assert status.json()["stt"]["loaded"] is False
            assert status.json()["platform"] == "linux"


def test_api_auth_requires_tokens_for_loopback_writes(monkeypatch):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "save_config", lambda config: None)
    monkeypatch.setattr(server, "build_llm", lambda config, **kwargs: object())
    monkeypatch.setattr(server.api_logic, "validate_runtime", lambda config: object())

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        assert client.put("/api/config", json={"stt": {"device": "cpu"}}).status_code == 401
        response = client.put(
            "/api/config",
            json={"stt": {"device": "cpu"}},
            headers={"X-Localflow-Token": "test-token"},
        )
        assert response.status_code == 200
        assert client.put(
            "/api/config",
            json={"stt": {"device": "cpu"}},
            headers={"X-Localflow-Token": "wrong"},
        ).status_code == 401


@pytest.mark.parametrize("method,path", [
    ("GET", "/settings"),
    ("HEAD", "/settings"),
    ("GET", "/api/status"),
    ("GET", "/api/config"),
    ("GET", "/prompts"),
    ("POST", "/prompts/x/dispatch"),
    ("POST", "/transcribe"),
    ("GET", "/meeting/status"),
    ("GET", "/openapi.json"),
])
@pytest.mark.parametrize("origin", [None, "null", "http://127.0.0.1:8756"])
def test_loopback_routes_require_token_whatever_the_origin(monkeypatch, method, path, origin):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    headers = {"Origin": origin} if origin is not None else {}

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.request(method, path, headers=headers)

    assert response.status_code == 401


def test_static_shell_and_health_stay_public(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        page = client.get("/")
        assert page.status_code == 200
        assert "test-token" not in page.text
        assert client.get("/healthz").status_code == 200


def test_config_api_applies_partial_updates(monkeypatch, tmp_path):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "save_config", lambda config: tmp_path / "config.toml")
    monkeypatch.setattr(server, "build_llm", lambda config, **kwargs: object())
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.put(
            "/api/config",
            json={"stt": {"device": "cpu"}},
            headers={"X-Localflow-Token": "test-token"},
        )

    assert response.status_code == 200
    assert response.json()["stt"]["device"] == "cpu"
    assert server._config.stt_device == "cpu"


def test_settings_rejects_dns_rebinding_host(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.get(
            "/settings",
            headers={
                "Host": "evil.example:8756",
                "Origin": "http://evil.example:8756",
            },
        )

    assert response.status_code == 403
    assert response.json() == {"detail": "invalid host"}


def test_bad_host_without_token_is_rejected_as_host(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.get("/meeting/status", headers={"Host": "evil.example:8756"})

    assert response.status_code == 403


def test_settings_allows_token_with_localhost_origin(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.get(
            "/settings",
            headers={
                "Host": "LOCALHOST:8756",
                "Origin": "http://localhost:8756",
                "X-Localflow-Token": "test-token",
            },
        )

    assert response.status_code == 200


def test_transcribe_rejects_oversized_upload(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "MAX_UPLOAD_BYTES", 1024)
    decode = Mock()
    monkeypatch.setattr(server, "_decode_audio", decode)

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234),
        headers={"X-Localflow-Token": "test-token"},
    ) as client:
        response = client.post(
            "/transcribe", files={"file": ("a.webm", b"x" * 4096, "audio/webm")}
        )

    assert response.status_code == 413
    decode.assert_not_called()


def test_transcribe_caps_read_even_without_content_length(monkeypatch):
    import asyncio

    monkeypatch.setattr(server, "MAX_UPLOAD_BYTES", 8)
    decode = Mock()
    monkeypatch.setattr(server, "_decode_audio", decode)
    upload = Mock()
    sizes = []

    async def read(size=-1):
        sizes.append(size)
        return b"x" * 9

    upload.read = read
    with pytest.raises(server.HTTPException) as exc:
        asyncio.run(server.transcribe(upload))

    assert exc.value.status_code == 413
    assert sizes == [9]
    decode.assert_not_called()


def test_ffmpeg_is_sandboxed_and_bounded(monkeypatch):
    calls = {}

    def fake_run(args, **kwargs):
        calls["args"] = args
        calls["kwargs"] = kwargs
        return SimpleNamespace(returncode=0, stdout=b"", stderr=b"")

    monkeypatch.setattr(server.subprocess, "run", fake_run)
    server._run_ffmpeg(b"data", max_seconds=90)

    args = calls["args"]
    assert args[args.index("-protocol_whitelist") + 1] == "pipe"
    assert "concat" not in args[args.index("-format_whitelist") + 1]
    assert args.index("-protocol_whitelist") < args.index("-i")
    assert args[args.index("-t") + 1] == "90"
    assert calls["kwargs"]["timeout"] == server.FFMPEG_TIMEOUT_SECONDS


def test_ffmpeg_timeout_is_a_client_error(monkeypatch):
    import subprocess

    def slow(*args, **kwargs):
        raise subprocess.TimeoutExpired("ffmpeg", 1)

    monkeypatch.setattr(server, "_run_ffmpeg", slow)
    with pytest.raises(server.HTTPException) as exc:
        server._decode_audio(b"data")
    assert exc.value.status_code == 400


def test_ffmpeg_refuses_playlists_that_open_other_files(tmp_path):
    import shutil

    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg not installed")
    secret = tmp_path / "secret.wav"
    secret.write_bytes(b"RIFF")
    playlist = f"ffconcat version 1.0\nfile '{secret}'\n".encode()
    proc = server._run_ffmpeg(playlist, max_seconds=5)
    assert proc.returncode != 0
    hls = f"#EXTM3U\n#EXTINF:1,\nfile://{secret}\n".encode()
    assert server._run_ffmpeg(hls, max_seconds=5).returncode != 0


def test_non_loopback_settings_requires_token_but_token_auth_still_works(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("10.0.0.5", 1234)
    ) as client:
        assert client.get("/settings").status_code == 401
        response = client.get(
            "/settings",
            headers={
                "Host": "localflow.tailnet.example:8756",
                "X-Localflow-Token": "test-token",
            },
        )

    assert response.status_code == 200


def test_loopback_post_config_requires_token(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.post("/api/config", json={"stt": {"device": "cpu"}})

    assert response.status_code == 401


def test_server_silence_gate_skips_transcription(monkeypatch):
    monkeypatch.setattr(server, "_config", Config())
    transcriber = Mock()
    monkeypatch.setattr(server, "_get_transcriber", lambda: transcriber)

    assert server._transcribe_sync(np.zeros(16000, dtype=np.float32), True) == ""
    transcriber.transcribe.assert_not_called()
    row = server._history.recent()[0]
    assert row["status"] == "silent"
    assert row["app_name"] == "phone"


def test_server_uses_other_style_category(monkeypatch):
    monkeypatch.setattr(
        server,
        "_config",
        Config(
            cleanup_enabled=False,
            styles_enabled=True,
            style_other="very_casual",
            spoken_symbols=False,
        ),
    )
    transcriber = Mock(transcribe=Mock(return_value="Hello world."))
    monkeypatch.setattr(server, "_get_transcriber", lambda: transcriber)
    monkeypatch.setattr(
        server,
        "_dictionary",
        SimpleNamespace(words=lambda: [], replacements=lambda: []),
    )
    monkeypatch.setattr(
        server, "_hot_phrases", SimpleNamespace(enabled=lambda: [])
    )

    assert server._transcribe_sync(
        np.full(16000, 0.1, dtype=np.float32), False
    ) == "hello world"


def test_server_cleans_short_transcript_when_requested(monkeypatch):
    monkeypatch.setattr(
        server, "_config", Config(cleanup_enabled=True, cleanup_level="light")
    )
    monkeypatch.setattr(
        server, "_dictionary", SimpleNamespace(words=lambda: [], replacements=lambda: [])
    )
    monkeypatch.setattr(server, "_hot_phrases", SimpleNamespace(enabled=lambda: []))
    monkeypatch.setattr(
        server,
        "_get_transcriber",
        lambda: Mock(transcribe=Mock(return_value="hello world")),
    )
    cleaner = Mock(clean=Mock(return_value="Cleaned."))
    monkeypatch.setattr(server, "_get_cleaner", lambda: cleaner)

    assert server._transcribe_sync(
        np.full(16000, 0.1, dtype=np.float32), True
    ) == "Cleaned."
    cleaner.clean.assert_called_once()
    row = server._history.recent()[0]
    assert row["kind"] == "dictate"
    assert row["app_name"] == "phone"
    assert row["raw_text"] == "hello world"
    assert row["final_text"] == "Cleaned."


def _capture_openai_requests(monkeypatch):
    from localflow.providers import factory

    sent = []

    class Resp:
        status_code = 200

        def raise_for_status(self):
            pass

        def json(self):
            return {"data": [{"id": "m"}]}

    def fake_get(url, headers=None, **kwargs):
        sent.append((url, dict(headers or {})))
        return Resp()

    monkeypatch.setattr(factory, "get_secret", lambda name: "stored-secret")
    monkeypatch.setattr("requests.get", fake_get)
    return sent


def test_models_never_sends_stored_key_to_unsaved_base_url(monkeypatch):
    from localflow import api

    sent = _capture_openai_requests(monkeypatch)
    config = Config(llm_provider="openai-compatible", llm_base_url="https://saved.example/v1")

    api.models(config, "llm", "openai-compatible", base_url="https://attacker.example/v1")
    api.models(config, "llm", "openai-compatible", base_url="https://saved.example/v1/")
    api.models(config, "llm", "openai-compatible")

    assert sent[0][0].startswith("https://attacker.example/")
    assert "Authorization" not in sent[0][1]
    assert sent[1][1]["Authorization"] == "Bearer stored-secret"
    assert sent[2][1]["Authorization"] == "Bearer stored-secret"


def test_test_llm_never_sends_stored_key_to_unsaved_base_url(monkeypatch):
    from localflow import api
    from localflow.providers import factory

    seen = []
    real_build = factory.build_llm

    def spy(config, **kwargs):
        llm = real_build(config, **kwargs)
        seen.append((config.llm_base_url, llm.api_key))
        llm.healthcheck = lambda: SimpleNamespace(ok=False, detail="401")
        return llm

    monkeypatch.setattr(factory, "get_secret", lambda name: "stored-secret")
    monkeypatch.setattr(api, "build_llm", spy)
    config = Config(llm_provider="openai-compatible", llm_base_url="https://saved.example/v1")

    result = api.test_llm(config, base_url="https://attacker.example/v1")
    api.test_llm(config)

    assert seen == [
        ("https://attacker.example/v1", None),
        ("https://saved.example/v1", "stored-secret"),
    ]
    assert "save it first" in result["detail"]


def _launch_client():
    return TestClient(server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234))


def _issue(client):
    return client.post("/api/launch-code", headers={"X-Localflow-Token": "test-token"}).json()[
        "code"
    ]


def test_launch_code_opens_settings_once_via_post(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})

    with _launch_client() as client:
        monkeypatch.setattr(server, "load_or_create", lambda: "rotated-token")
        assert client.post("/api/launch-code").status_code == 401
        code = _issue(client)
        assert "test-token" not in code

        page = client.post("/settings", data={"launch": code})
        assert page.status_code == 200
        assert "test-token" in page.text
        assert page.headers["cache-control"] == "no-store"

        assert client.post("/settings", data={"launch": "made-up"}).status_code == 401
        assert client.post("/settings").status_code == 401
        assert server._api_token == "test-token"


def test_launch_code_is_not_redeemed_from_a_get(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})

    with _launch_client() as client:
        code = server._issue_launch_code()
        assert client.get(f"/settings?launch={code}").status_code == 401
        index = client.get(f"/?launch={code}")
        assert index.status_code == 200
        assert "test-token" not in index.text
        assert code in server._launch_codes


def test_reused_launch_code_rotates_the_token(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})

    with _launch_client() as client:
        monkeypatch.setattr(server, "load_or_create", lambda: "rotated-token")
        code = _issue(client)
        first = client.post("/settings", data={"launch": code})
        assert first.status_code == 200
        assert "test-token" in first.text
        pending = server._issue_launch_code()

        second = client.post("/settings", data={"launch": code})
        assert second.status_code == 401
        assert "test-token" not in second.text
        assert "rotated-token" not in second.text

        stolen = client.get("/api/config", headers={"X-Localflow-Token": "test-token"})
        assert stolen.status_code == 401
        fresh = client.get("/api/config", headers={"X-Localflow-Token": "rotated-token"})
        assert fresh.status_code == 200
        assert client.post("/settings", data={"launch": pending}).status_code == 401


def test_launch_code_expires(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})
    now = [1000.0]
    monkeypatch.setattr(server.time, "monotonic", lambda: now[0])

    with _launch_client() as client:
        code = _issue(client)
        now[0] += server.LAUNCH_CODE_TTL_SECONDS + 1
        assert client.post("/settings", data={"launch": code}).status_code == 401


def test_launch_code_only_works_for_the_ui_pages(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})

    with _launch_client() as client:
        code = server._issue_launch_code()
        assert client.post("/api/config", data={"launch": code}).status_code == 401
        assert client.get(f"/api/config?launch={code}").status_code == 401
        assert client.post("/settings", json={"launch": code}).status_code == 401
        assert client.post(
            "/settings", content="launch=" + code + "&pad=" + "x" * 2048,
            headers={"Content-Type": "application/x-www-form-urlencoded"},
        ).status_code == 401


def test_launch_code_injects_token_into_index_once(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "_launch_codes", {})
    monkeypatch.setattr(server, "_used_launch_codes", {})

    with _launch_client() as client:
        code = server._issue_launch_code()
        page = client.post("/", data={"launch": code})
        assert page.status_code == 200
        assert 'content="test-token"' in page.text
        assert page.headers["cache-control"] == "no-store"
        plain = client.get("/")
        assert plain.status_code == 200
        assert "test-token" not in plain.text


def test_outstanding_launch_codes_are_capped(monkeypatch):
    monkeypatch.setattr(server, "_launch_codes", {})
    codes = [server._issue_launch_code() for _ in range(server._MAX_LAUNCH_CODES + 5)]
    assert len(server._launch_codes) == server._MAX_LAUNCH_CODES
    assert server._redeem_launch_code(codes[0]) == "invalid"
    assert server._redeem_launch_code(codes[-1]) == "ok"


@pytest.mark.parametrize("path", ["/settings", "/api/config"])
@pytest.mark.parametrize("host", ["127.0.0.1:8756/?", "127.0.0.1:8756/#"])
def test_host_with_path_chars_cannot_make_route_look_public(monkeypatch, path, host):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        response = client.get(path, headers={"Host": host})

    assert response.status_code == 401
    assert "test-token" not in response.text
