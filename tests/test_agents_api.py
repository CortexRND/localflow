import asyncio
import dataclasses
import threading
import time
from types import SimpleNamespace

import pytest
from fastapi.testclient import TestClient
from fixtures.agents.support import (
    TOKEN,
    agents_config,
    api_client,
    isolate,
    make_manager,
    validate,
    validate_response,
    wait_for,
)

from localflow import server
from localflow.agents import api as agents_api
from localflow.agents.manager import Sink

S = "/api/agents/sessions"


@pytest.fixture
def env(tmp_path, monkeypatch):
    keyring = isolate(tmp_path, monkeypatch)
    config = agents_config(tmp_path)
    manager = make_manager(tmp_path, lambda: server._config, supervise=True)
    client = api_client(monkeypatch, config, manager)

    def set_config(**changes):
        monkeypatch.setattr(server, "_config", dataclasses.replace(server._config, **changes))

    yield SimpleNamespace(client=client, manager=manager, keyring=keyring, set_config=set_config,
                          tmp_path=tmp_path)
    manager.shutdown()
    agents_api.set_manager(None)


def _launch(client, **body):
    body = {"prompt": "run the mock script", "provider": "mock", **body}
    response = client.post(S, json=body)
    return response, (response.json() if response.status_code < 300 else None)


def _wait_terminal(client, session_id, timeout=15):
    def done():
        body = client.get(f"{S}/{session_id}").json()["session"]
        return body if body["state"] in ("completed", "failed", "cancelled", "timed_out") else None

    return wait_for(done, timeout=timeout)


def test_status_and_providers(env):
    status = validate_response(env.client.get("/api/agents/status"), "GET", "/api/agents/status")
    assert status["api_version"] == 1 and status["enabled"] and status["sandbox"]
    assert status["repos"][0]["alias"] == "proj" and status["repos"][0]["exists"]
    providers = validate_response(env.client.get("/api/agents/providers"), "GET",
                                  "/api/agents/providers")["providers"]
    ids = [p["id"] for p in providers]
    assert ids == sorted(ids) and {"claude", "devin", "http", "mock"} <= set(ids)
    devin = next(p for p in providers if p["id"] == "devin")
    assert devin["readiness"] == {"ready": False, "code": "missing_secret",
                                  "detail": "Add a Devin API key.", "checked": "shallow"}
    assert devin["secrets"] == [{"name": "agent_devin_api_key", "set": False, "required": True}]
    http = next(p for p in providers if p["id"] == "http")
    assert http["profile"] == "" and http["readiness"]["code"] == "missing_profile"
    check = env.client.post("/api/agents/providers/mock/check", json={"profile": ""})
    assert validate_response(check, "POST", "/api/agents/providers/{id}/check")[
        "readiness"]["checked"] == "deep"
    missing = env.client.post("/api/agents/providers/nope/check", json={})
    assert validate_response(missing, "POST", "/api/agents/providers/{id}/check")[
        "code"] == "not_found"


def test_launch_events_and_replay(env):
    response, body = _launch(env.client, id="00000000abcd", title="My first run")
    validate_response(response, "POST", S)
    assert response.status_code == 202 and body["session"]["state"] == "queued"
    session_id = body["session"]["id"]
    done = _wait_terminal(env.client, session_id)
    assert done["state"] == "completed" and done["title"] == "My first run"
    validate(done, "Session")
    seen, after = [], 0
    while True:
        page = env.client.get(f"{S}/{session_id}/events", params={"after": after, "limit": 2})
        page = validate_response(page, "GET", "/api/agents/sessions/{id}/events")
        if page["events"]:
            assert page["events"][0]["seq"] == after + 1
        seen += [e["seq"] for e in page["events"]]
        after = page["next_after"]
        if page["terminal"] and not page["has_more"]:
            break
    assert seen == list(range(1, done["last_seq"] + 1))
    replay, again = _launch(env.client, id="00000000abcd", title="My first run")
    assert replay.status_code == 200 and again["session"]["id"] == session_id
    validate_response(replay, "POST", S)
    conflict, _ = _launch(env.client, id="00000000abcd", prompt="different")
    assert validate_response(conflict, "POST", S)["code"] == "id_conflict"
    assert conflict.status_code == 409
    one = env.client.get(f"{S}/{session_id}")
    validate_response(one, "GET", "/api/agents/sessions/{id}")
    gone = env.client.get(f"{S}/ffffffffffff")
    assert gone.status_code == 404
    assert validate_response(gone, "GET", "/api/agents/sessions/{id}")["code"] == "not_found"


@pytest.mark.parametrize("body,status,code", [
    ({"prompt": "x", "provider": "mock", "nope": 1}, 400, "validation"),
    ({"prompt": "x" * 20001, "provider": "mock"}, 413, "prompt_too_long"),
    ({"prompt": "x", "provider": "unknown"}, 400, "validation"),
    ({"prompt": "x", "provider": "mock", "repo": "proj", "cwd": "/tmp"}, 400, "validation"),
    ({"prompt": 5}, 400, "validation"),
])
def test_launch_validation_errors(env, body, status, code):
    response = env.client.post(S, json=body)
    assert response.status_code == status
    assert validate_response(response, "POST", S)["code"] == code


def test_invalid_json_is_400_not_422(env):
    response = env.client.post(S, content=b"{oops", headers={"Content-Type": "application/json"})
    assert response.status_code == 400
    assert validate_response(response, "POST", S)["code"] == "validation"
    response = env.client.post(S, json=["a"])
    assert response.status_code == 400


def test_provider_not_ready_and_unsupported(env):
    env.set_config(agent_sandbox=False)
    response, _ = _launch(env.client, provider="devin")
    body = validate_response(response, "POST", S)
    assert response.status_code == 409 and body["code"] == "provider_not_ready"
    assert body["readiness"]["code"] == "missing_secret"
    response, _ = _launch(env.client, provider="devin", resume_of="a" * 12)
    assert validate_response(response, "POST", S)["code"] == "unsupported"


def test_queue_full_is_429_with_retry_after(env):
    env.set_config(agent_max_concurrent=1, agent_max_queued=1)
    _launch(env.client, options={"script": "hang"})
    _launch(env.client, options={"script": "hang"})
    response, _ = _launch(env.client, options={"script": "hang"})
    assert response.status_code == 429 and response.headers["Retry-After"] == "5"
    assert validate_response(response, "POST", S)["code"] == "queue_full"


def test_cancel_message_delete(env):
    _, body = _launch(env.client, options={"script": "needs_input"})
    session_id = body["session"]["id"]
    wait_for(lambda: env.client.get(f"{S}/{session_id}").json()["session"]["state"]
             == "waiting_input")
    sent = env.client.post(f"{S}/{session_id}/messages", json={"text": "go ahead"})
    assert sent.status_code == 202
    sent = validate_response(sent, "POST", "/api/agents/sessions/{id}/messages")
    assert sent["event"]["type"] == "message" and sent["event"]["text"] == "go ahead"
    done = _wait_terminal(env.client, session_id)
    assert done["result_text"] == "received: go ahead"
    late = env.client.post(f"{S}/{session_id}/messages", json={"text": "again"})
    body = validate_response(late, "POST", "/api/agents/sessions/{id}/messages")
    assert late.status_code == 409 and body["code"] == "session_conflict"
    assert body["state"] == "completed" and body["expected"] == ["running", "waiting_input"]
    cancel_done = env.client.post(f"{S}/{session_id}/cancel")
    assert cancel_done.status_code == 409
    validate_response(cancel_done, "POST", "/api/agents/sessions/{id}/cancel")
    _, hang = _launch(env.client, options={"script": "hang"})
    hang_id = hang["session"]["id"]
    active_delete = env.client.delete(f"{S}/{hang_id}")
    assert active_delete.status_code == 409
    validate_response(active_delete, "DELETE", "/api/agents/sessions/{id}")
    cancelled = env.client.post(f"{S}/{hang_id}/cancel")
    body = validate_response(cancelled, "POST", "/api/agents/sessions/{id}/cancel")
    assert body["session"]["state"] == "cancelled"
    again = env.client.post(f"{S}/{hang_id}/cancel")
    assert again.status_code == 200 and again.json()["session"]["state"] == "cancelled"
    deleted = env.client.delete(f"{S}/{hang_id}")
    assert validate_response(deleted, "DELETE", "/api/agents/sessions/{id}") == {"deleted": True}
    assert env.client.get(f"{S}/{hang_id}").status_code == 404


def test_feature_off(env):
    _, body = _launch(env.client, options={"script": "hang"})
    session_id = body["session"]["id"]
    env.set_config(agents_enabled=False)
    status = env.client.get("/api/agents/status").json()
    assert status["enabled"] is False
    for response, route in [
        (env.client.post(S, json={"prompt": "x", "provider": "mock"}), S),
        (env.client.post(f"{S}/{session_id}/messages", json={"text": "hi"}),
         "/api/agents/sessions/{id}/messages"),
        (env.client.post("/api/agents/hot-commands/test", json={"text": "x"}),
         "/api/agents/hot-commands/test"),
    ]:
        assert response.status_code == 409, response.text
        assert validate_response(response, "POST", route)["code"] == "agents_disabled"
    assert env.client.get(f"{S}/{session_id}").status_code == 200
    assert env.client.post(f"{S}/{session_id}/cancel").json()["session"]["state"] == "cancelled"


def test_list_pagination_and_filters(env):
    env.set_config(agent_max_queued=64, agent_max_concurrent=8)
    ids = []
    for i in range(7):
        _, body = _launch(env.client, title=f"run number {i}", options={"script": "fail"}
                          if i % 2 else {"script": "ok"})
        ids.append(body["session"]["id"])
    for session_id in ids:
        _wait_terminal(env.client, session_id)
    walked, before = [], None
    while True:
        params = {"limit": 3, **({"before": before} if before else {})}
        page = validate_response(env.client.get(S, params=params), "GET", S)
        walked += [s["id"] for s in page["sessions"]]
        before = page["next_cursor"]
        if before is None:
            break
    assert sorted(walked) == sorted(ids) and len(walked) == len(set(walked)) == 7
    failed = env.client.get(S, params={"state": "failed"}).json()["sessions"]
    assert len(failed) == 3 and all(s["state"] == "failed" for s in failed)
    terminal = env.client.get(S, params={"state": "terminal"}).json()["sessions"]
    assert len(terminal) == 7
    assert env.client.get(S, params={"state": "active"}).json()["sessions"] == []
    assert [s["title"] for s in env.client.get(S, params={"q": "NUMBER 3"}).json()[
        "sessions"]] == ["run number 3"]
    assert env.client.get(S, params={"provider": "claude"}).json()["sessions"] == []
    for params in ({"state": "bogus"}, {"limit": 0}, {"limit": 999}, {"before": "%%%"},
                   {"origin": "x"}, {"color": "red"}):
        bad = env.client.get(S, params=params)
        assert bad.status_code == 400, params
        validate_response(bad, "GET", S)


def test_events_long_poll(env):
    _, body = _launch(env.client, options={"script": "hang"})
    session_id = body["session"]["id"]
    wait_for(lambda: env.client.get(f"{S}/{session_id}").json()["session"]["state"] == "running")
    last = env.client.get(f"{S}/{session_id}").json()["session"]["last_seq"]
    started = time.monotonic()
    idle = env.client.get(f"{S}/{session_id}/events", params={"after": last, "wait": 1})
    elapsed = time.monotonic() - started
    idle = validate_response(idle, "GET", "/api/agents/sessions/{id}/events")
    assert idle["events"] == [] and idle["waited"] is True and 0.9 <= elapsed < 2.0

    def emit():
        time.sleep(0.3)
        Sink(env.manager, session_id).output("agent", "ping")

    threading.Thread(target=emit).start()
    started = time.monotonic()
    page = env.client.get(f"{S}/{session_id}/events", params={"after": last, "wait": 10})
    assert time.monotonic() - started < 1.5
    assert [e["text"] for e in page.json()["events"]] == ["ping"]
    for params in ({"wait": 26}, {"after": -1}, {"limit": 501}, {"wait": "soon"}):
        bad = env.client.get(f"{S}/{session_id}/events", params=params)
        assert bad.status_code == 400
        validate_response(bad, "GET", "/api/agents/sessions/{id}/events")


def test_long_poll_waiter_cap():
    async def run():
        loop = asyncio.get_running_loop()
        fillers = {(loop, asyncio.Event()) for _ in range(agents_api.MAX_WAITERS)}
        agents_api._waiters.update(fillers)
        try:
            return await agents_api._hold(lambda: False, 5)
        finally:
            agents_api._waiters.difference_update(fillers)

    assert asyncio.run(run()) is False


def test_changes_feed(env):
    start = validate_response(env.client.get("/api/agents/changes"), "GET", "/api/agents/changes")
    since = start["revision"]

    def launch_later():
        time.sleep(0.3)
        _launch(env.client, options={"script": "fail"})

    threading.Thread(target=launch_later).start()
    began = time.monotonic()
    changed = env.client.get("/api/agents/changes", params={"since": since, "wait": 10})
    assert time.monotonic() - began < 2.0
    changed = validate_response(changed, "GET", "/api/agents/changes")
    assert changed["waited"] is True and changed["sessions"]
    session_id = changed["sessions"][0]["id"]
    _wait_terminal(env.client, session_id)
    mark = env.client.get("/api/agents/changes", params={"since": 0}).json()["revision"]
    env.client.delete(f"{S}/{session_id}")
    after = validate_response(env.client.get("/api/agents/changes", params={"since": mark}),
                              "GET", "/api/agents/changes")
    assert after["deleted"] == [session_id] and after["reset"] is False
    future = env.client.get("/api/agents/changes", params={"since": 10**9}).json()
    assert future["reset"] is True


def test_hot_command_routes(env, monkeypatch):
    monkeypatch.setattr("localflow.agents.rules.hot_phrase_triggers", lambda: ["review list"])
    route = "/api/agents/hot-commands"
    empty = validate_response(env.client.get(route), "GET", route)
    assert empty == {"rules": []}
    created = env.client.post(route, json={"trigger": "launch claude", "provider": "claude"})
    assert created.status_code == 201
    rule = validate_response(created, "POST", route)
    duplicate = env.client.post(route, json={"trigger": "Launch Claude!", "provider": "devin"})
    assert duplicate.status_code == 409
    assert validate_response(duplicate, "POST", route)["code"] == "rule_conflict"
    phrase = env.client.post(route, json={"trigger": "review list", "provider": "devin"})
    assert phrase.status_code == 409
    bad = env.client.post(route, json={"trigger": "one", "provider": "claude"})
    assert bad.status_code == 400 and validate_response(bad, "POST", route)["code"] == "validation"
    test_route = f"{route}/test"
    off = validate_response(env.client.post(test_route, json={"text": "launch claude to x y z"}),
                            "POST", test_route)
    assert off["matched"] is False and off["reason"] == "feature_off"
    env.set_config(agent_hot_commands=True)
    hit = validate_response(env.client.post(test_route, json={
        "text": "Launch Claude in proj to fix the tests"}), "POST", test_route)
    assert hit == {"matched": True, "rule_id": rule["id"], "provider": "claude", "profile": "",
                   "repo": "proj", "prompt": "fix the tests", "reason": ""}
    assert env.client.get(S).json()["sessions"] == []  # testing never launches
    updated = env.client.put(f"{route}/{rule['id']}", json={"enabled": False})
    assert validate_response(updated, "PUT", "/api/agents/hot-commands/{id}")["enabled"] is False
    missing = env.client.put(f"{route}/deadbeef", json={"enabled": True})
    assert missing.status_code == 404
    deleted = env.client.delete(f"{route}/{rule['id']}")
    assert validate_response(deleted, "DELETE", "/api/agents/hot-commands/{id}") == {
        "deleted": True}
    assert env.client.delete(f"{route}/{rule['id']}").status_code == 404


def test_http_profile_routes(env):
    route = "/api/agents/http-profiles"
    body = {"label": "Agent", "base_url": "https://agents.example.com",
            "headers": {"Authorization": "Bearer {secret}"},
            "start": {"method": "POST", "path": "/runs", "body": {"in": "{prompt}"}},
            "status": {"method": "GET", "path": "/runs/{external_id}", "state_path": "s",
                       "state_map": {"done": "completed"}}}
    created = env.client.post(route, json=body)
    assert created.status_code == 201
    profile = validate_response(created, "POST", route)
    assert profile["secret_set"] is False
    secret_route = "/api/agents/providers/{id}/secret"
    set_secret = env.client.put("/api/agents/providers/http/secret",
                                json={"profile": profile["id"], "value": "tok-123456789"})
    provider = validate_response(set_secret, "PUT", secret_route)
    assert provider["secrets"][0]["set"] is True and "tok-123456789" not in set_secret.text
    listed = validate_response(env.client.get(route), "GET", route)
    assert listed["profiles"][0]["secret_set"] is True
    same_origin = env.client.put(f"{route}/{profile['id']}", json={"label": "Renamed"})
    assert validate_response(same_origin, "PUT", "/api/agents/http-profiles/{id}")[
        "secret_set"] is True
    moved = env.client.put(f"{route}/{profile['id']}",
                           json={"base_url": "https://evil.example.net"})
    assert validate_response(moved, "PUT", "/api/agents/http-profiles/{id}")[
        "secret_set"] is False
    literal = env.client.post(route, json={**body, "headers": {"Authorization": "Bearer abc"}})
    assert literal.status_code == 400
    validate_response(literal, "POST", route)
    deleted = env.client.delete(f"{route}/{profile['id']}")
    assert validate_response(deleted, "DELETE", "/api/agents/http-profiles/{id}") == {
        "deleted": True}
    assert env.client.delete(f"{route}/{profile['id']}").status_code == 404


def test_secret_route(env):
    route = "/api/agents/providers/{id}/secret"
    response = env.client.put("/api/agents/providers/devin/secret",
                              json={"value": "apk_user_abcdefgh12345678"})
    provider = validate_response(response, "PUT", route)
    assert provider["readiness"]["ready"] and provider["secrets"][0]["set"]
    assert "apk_user_abcdefgh12345678" not in response.text
    assert env.client.get("/api/secrets").json()["agent_devin_api_key"] is True
    wrong = env.client.put("/api/agents/providers/devin/secret",
                           json={"name": "llm_api_key", "value": "x"})
    assert wrong.status_code == 400 and validate_response(wrong, "PUT", route)["code"] == \
        "validation"
    cleared = env.client.put("/api/agents/providers/devin/secret", json={"value": ""})
    assert validate_response(cleared, "PUT", route)["secrets"][0]["set"] is False

    def broken(*_args):
        raise RuntimeError("no keyring")

    env.keyring.set_password = broken
    failed = env.client.put("/api/agents/providers/devin/secret", json={"value": "abcdefghijk"})
    assert failed.status_code == 503
    assert validate_response(failed, "PUT", route)["code"] == "secrets_unavailable"


def test_auth_host_and_local_only(env):
    bare = TestClient(server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1))
    assert bare.get("/api/agents/status").status_code == 401
    assert bare.post(S, json={"prompt": "x"}).status_code == 401
    foreign = TestClient(server.app, base_url="http://evil.example.com",
                         client=("127.0.0.1", 1))
    assert foreign.get("/api/agents/status").status_code == 403
    remote = TestClient(server.app, base_url="http://127.0.0.1:8756", client=("10.0.0.5", 1),
                        headers={"X-Localflow-Token": TOKEN})
    assert remote.get("/api/agents/status").status_code == 200
    for method, path in [("POST", S), ("POST", "/api/agents/hot-commands"),
                         ("POST", "/api/agents/http-profiles"),
                         ("PUT", "/api/agents/providers/devin/secret"),
                         ("POST", f"{S}/aaaaaaaaaaaa/cancel")]:
        response = remote.request(method, path, json={"prompt": "x"})
        assert response.status_code == 403, path
        assert response.json()["code"] == "local_only"
        validate(response.json(), "Error")
    proxied = env.client.post(S, json={"prompt": "x", "provider": "mock"},
                              headers={"X-Forwarded-For": "1.2.3.4"})
    assert proxied.status_code == 403
    assert env.manager.store.list() == []  # nothing changed
    env.set_config(agent_allow_remote=True)
    assert remote.post(S, json={"prompt": "x", "provider": "mock"}).status_code == 202


def test_config_local_only_keys_and_restart(env, monkeypatch):
    monkeypatch.setattr(server, "build_llm", lambda *a, **k: object())
    monkeypatch.setattr(server.api_logic, "validate_runtime", lambda config: object())
    monkeypatch.setattr(server, "MeetingSummarizer", lambda *a, **k: object())
    remote = TestClient(server.app, base_url="http://127.0.0.1:8756", client=("10.0.0.5", 1),
                        headers={"X-Localflow-Token": TOKEN})
    for key, value in [("repos", ["evil=/etc"]), ("claude_bin", "/tmp/evil"),
                       ("allow_remote", True)]:
        response = remote.put("/api/config", json={"agents": {key: value}})
        assert response.status_code == 403, key
    allowed = remote.put("/api/config", json={"agents": {"max_concurrent": 3}})
    assert allowed.status_code == 200 and allowed.json()["restart_required"] == []
    local = env.client.put("/api/config", json={"agents": {"allow_remote": False,
                                                           "sandbox": True, "enabled": True}})
    assert local.status_code == 200 and local.json()["restart_required"] == []
    assert local.json()["agents"]["max_concurrent"] == 3
    # The agents routes see the new value at once (no restart).
    status = env.client.get("/api/agents/status").json()
    assert status["limits"]["max_concurrent"] == 3


# -- regressions (PR #185 review) ---------------------------------------------------


def _remote_client():
    return TestClient(server.app, base_url="http://127.0.0.1:8756", client=("10.0.0.5", 1),
                      headers={"X-Localflow-Token": TOKEN})


def test_remote_callers_cannot_replace_agent_keys_via_api_secrets(env):
    remote = _remote_client()
    for name in ("agent_devin_api_key", "agent_claude_api_key"):
        response = remote.put(f"/api/secrets/{name}", json={"value": "remote-key-0123456789"})
        assert response.status_code == 403, name
    assert env.keyring.values == {}
    local = env.client.put("/api/secrets/agent_devin_api_key",
                           json={"value": "local-key-0123456789"})
    assert local.status_code == 200 and local.json()["agent_devin_api_key"] is True
    env.set_config(agent_allow_remote=True)
    allowed = remote.put("/api/secrets/agent_devin_api_key", json={"value": ""})
    assert allowed.status_code == 200 and allowed.json()["agent_devin_api_key"] is False


def test_env_token_is_not_sent_after_an_origin_change(env, monkeypatch):
    route = "/api/agents/http-profiles"
    body = {"label": "Agent", "base_url": "https://agents.example.com",
            "headers": {"Authorization": "Bearer {secret}"},
            "start": {"method": "POST", "path": "/runs"}}
    profile = validate_response(env.client.post(route, json=body), "POST", route)
    pid = profile["id"]
    monkeypatch.setenv(f"LOCALFLOW_AGENT_HTTP_{pid.upper()}_TOKEN", "env-token-0123456789")
    stored = env.manager.profiles.get(pid)
    assert stored["secret_origin"] == "https://agents.example.com:443"
    assert env.manager.readiness("http", pid).ready
    moved = env.client.put(f"{route}/{pid}", json={"base_url": "https://evil.example.net"})
    assert validate_response(moved, "PUT", "/api/agents/http-profiles/{id}")[
        "secret_set"] is False
    assert env.manager.readiness("http", pid).code == "missing_secret"
    assert env.manager.provider_for("http", pid)._secret() == ""
    # The token stays withheld across further moves until it is set again.
    moved_again = env.client.put(f"{route}/{pid}", json={"base_url": "https://new.example.org"})
    assert validate_response(moved_again, "PUT", "/api/agents/http-profiles/{id}")[
        "secret_set"] is False
    entry = env.client.put("/api/agents/providers/http/secret",
                           json={"profile": pid, "value": "new-token-0123456789"})
    assert validate_response(entry, "PUT", "/api/agents/providers/{id}/secret")[
        "readiness"]["ready"] is True
    assert env.manager.profiles.get(pid)["secret_origin"] == "https://new.example.org:443"
    listed = validate_response(env.client.get(route), "GET", route)["profiles"]
    assert listed[0]["secret_set"] is True and "secret_origin" not in listed[0]


def test_removing_a_missing_secret_does_not_log_its_name(env, caplog):
    import logging

    caplog.set_level(logging.DEBUG, logger="localflow.agents.api")
    agents_api._delete_secret_quietly("agent_http_deadbeef_token")
    assert caplog.records and not any("agent_http_deadbeef_token" in r.getMessage()
                                      for r in caplog.records)
