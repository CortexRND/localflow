from fastapi.testclient import TestClient

from localflow import server
from localflow.config import Config


class FakePlatform:
    name = "linux"

    def autostart_status(self):
        return "not installed"


def test_settings_and_api_token_auth(monkeypatch):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "current", lambda: FakePlatform())

    with TestClient(server.app, client=("127.0.0.1", 1234)) as client:
        settings = client.get("/settings")
        assert settings.status_code == 200
        assert "test-token" in settings.text
        status = client.get("/api/status")
        assert status.status_code == 200
        assert status.json()["stt"]["loaded"] is False
        assert status.json()["platform"] == "linux"

    with TestClient(server.app, client=("10.0.0.5", 1234)) as client:
        assert client.get("/api/status").status_code == 401
        authorized = client.get(
            "/api/status", headers={"X-Localflow-Token": "test-token"}
        )
        assert authorized.status_code == 200
        assert client.get("/settings").status_code == 401
        settings = client.get("/settings?token=test-token")
        assert settings.status_code == 200
        assert "test-token" in settings.text


def test_api_auth_requires_tokens_for_loopback_writes_and_checks_origin(monkeypatch):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    monkeypatch.setattr(server, "save_config", lambda config: None)
    monkeypatch.setattr(server, "build_llm", lambda config: object())
    monkeypatch.setattr(server.api_logic, "validate_runtime", lambda config: object())

    with TestClient(server.app, client=("127.0.0.1", 1234)) as client:
        assert client.put("/api/config", json={"stt": {"device": "cpu"}}).status_code == 401
        response = client.put(
            "/api/config",
            json={"stt": {"device": "cpu"}},
            headers={"X-Localflow-Token": "test-token"},
        )
        assert response.status_code == 200
        assert (
            client.get(
                "/api/status",
                headers={"Origin": "http://evil.example"},
            ).status_code
            == 403
        )


def test_config_api_applies_partial_updates(monkeypatch, tmp_path):
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False))
    monkeypatch.setattr(server, "save_config", lambda config: tmp_path / "config.toml")
    monkeypatch.setattr(server, "build_llm", lambda config: object())
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")

    with TestClient(server.app, client=("127.0.0.1", 1234)) as client:
        response = client.put(
            "/api/config",
            json={"stt": {"device": "cpu"}},
            headers={"X-Localflow-Token": "test-token"},
        )

    assert response.status_code == 200
    assert response.json()["stt"]["device"] == "cpu"
    assert server._config.stt_device == "cpu"
