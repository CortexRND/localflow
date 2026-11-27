"""No planted secret reaches a response, a store, an event log, config.toml or a log line."""

import logging
from pathlib import Path

import pytest
from fixtures.agents.support import (
    agents_config,
    api_client,
    isolate,
    make_manager,
    wait_for,
)

from localflow import server
from localflow.agents import api as agents_api
from localflow.agents.redact import Redactor, scrub_env

DEVIN = "sim-secret-devin-4f9a8b7c6d5e"
CLAUDE = "sk-ant-api03-simsecretclaude0000000"
HTTP_TOKEN = "sim-secret-http-1a2b3c4d5e6f"


def test_patterns_and_exact_values():
    redactor = Redactor(names=lambda: ["agent_devin_api_key", "short"],
                        get_secret=lambda name: DEVIN if name == "agent_devin_api_key" else "abc")
    text = (f"key={DEVIN} anthropic=sk-ant-api03-zzzzzzzzzzzz Authorization: Bearer abc.def.ghi "
            "x-api-key: topsecretvalue apk_user_0123456789abcdef keep abc")
    out = redactor(text)
    assert DEVIN not in out and "[redacted:agent_devin_api_key]" in out
    assert "sk-ant-api03-zzzz" not in out and "abc.def.ghi" not in out
    assert "topsecretvalue" not in out and "apk_user_0123456789abcdef" not in out
    assert out.endswith("keep abc")  # values shorter than 8 characters are not replaced
    redactor.add("agent_claude_api_key", "brand-new-secret-value")
    assert "brand-new" not in redactor("x brand-new-secret-value y")
    assert redactor.redact_obj({"a": [DEVIN]}) == {"a": ["[redacted:agent_devin_api_key]"]}


def test_scrub_env_drops_localflow_secrets():
    env = scrub_env({"PATH": "/bin", "LOCALFLOW_AGENT_DEVIN_API_KEY": "x", "localflow_x": "y",
                     "FIREWORKS_API_KEY": "z", "DEVIN_API_KEY": "w", "HOME": "/h"})
    assert env == {"PATH": "/bin", "HOME": "/h"}


@pytest.fixture
def env(tmp_path, monkeypatch):
    keyring = isolate(tmp_path, monkeypatch)
    monkeypatch.setenv("LOCALFLOW_AGENT_DEVIN_API_KEY", DEVIN)
    config = agents_config(tmp_path)
    manager = make_manager(tmp_path, lambda: server._config, supervise=True)
    client = api_client(monkeypatch, config, manager)
    yield client, manager, keyring, tmp_path
    manager.shutdown()
    agents_api.set_manager(None)


def test_full_run_leaks_nothing(env, caplog, monkeypatch):
    client, _manager, keyring, tmp_path = env
    caplog.set_level(logging.DEBUG)
    bodies: list[str] = []

    def call(method, path, **kwargs):
        response = client.request(method, path, **kwargs)
        bodies.append(response.text)
        return response

    assert call("PUT", "/api/agents/providers/claude/secret",
                json={"value": CLAUDE}).status_code == 200
    profile = call("POST", "/api/agents/http-profiles", json={
        "label": "Agent", "base_url": "https://agents.example.com",
        "headers": {"Authorization": "Bearer {secret}"},
        "start": {"method": "POST", "path": "/runs", "body": {"in": "{prompt}"}}}).json()
    assert call("PUT", "/api/agents/providers/http/secret",
                json={"profile": profile["id"], "value": HTTP_TOKEN}).status_code == 200
    literal = call("POST", "/api/agents/http-profiles", json={
        "label": "Bad", "base_url": "https://agents.example.com",
        "headers": {"X-Api-Key": HTTP_TOKEN}, "start": {"method": "POST", "path": "/r"}})
    assert literal.status_code == 400
    launched = call("POST", "/api/agents/sessions", json={
        "prompt": f"use {DEVIN} and {CLAUDE} and {HTTP_TOKEN}", "provider": "mock",
        "title": f"title {DEVIN}", "options": {"script": "secret_echo"}}).json()
    session_id = launched["session"]["id"]
    wait_for(lambda: call("GET", f"/api/agents/sessions/{session_id}").json()["session"][
        "state"] == "completed")
    call("GET", f"/api/agents/sessions/{session_id}/events")
    call("GET", "/api/agents/sessions")
    call("GET", "/api/agents/changes")
    call("GET", "/api/agents/providers")
    call("GET", "/api/agents/http-profiles")
    bad = call("POST", "/api/agents/sessions", json={"prompt": "x", "provider": DEVIN})
    assert bad.status_code == 400
    call("POST", "/api/agents/hot-commands", json={"trigger": f"say {DEVIN}", "provider": "x"})
    detail = call("GET", f"/api/agents/sessions/{session_id}").json()["session"]
    assert "[redacted:" in detail["prompt"] and "[redacted:" in detail["result_text"]

    from localflow.config import save_config

    save_config(server._config)
    planted = [DEVIN, CLAUDE, HTTP_TOKEN]
    for body in bodies:
        for secret in planted:
            assert secret not in body, body[:300]
    files = [p for p in Path(tmp_path).rglob("*") if p.is_file()]
    assert any(p.name == "sessions.json" for p in files)
    assert any(p.suffix == ".jsonl" for p in files)
    for path in files:
        data = path.read_bytes()
        for secret in planted:
            assert secret.encode() not in data, path
    for secret in planted:
        assert secret not in caplog.text
    # The keys live only in the keyring.
    assert keyring.values[("localflow", "agent_claude_api_key")] == CLAUDE


def test_error_details_are_redacted(env):
    client, _manager, _keyring, _tmp = env
    response = client.post("/api/agents/sessions",
                           json={"prompt": "x", "provider": "mock", "rule_id": DEVIN})
    assert DEVIN not in response.text
