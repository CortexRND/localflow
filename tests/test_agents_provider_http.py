import pytest
from fixtures.agents.stub_agent_server import StubAgentServer
from fixtures.agents.support import (
    FakeClock,
    agents_config,
    drive,
    isolate,
    make_manager,
)

from localflow.agents.models import new_short_id
from localflow.agents.providers import http as http_provider
from localflow.agents.providers.http import ProfileError, validate_profile

TOKEN = "http-agent-token-0123456789"


@pytest.fixture
def stub(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    server = StubAgentServer()
    yield server
    server.close()


def _profile(base, **extra):
    profile = {
        "label": "My agent", "base_url": base, "enabled": True,
        "headers": {"Authorization": "Bearer {secret}", "X-Source": "localflow"},
        "start": {"method": "POST", "path": "/v1/runs",
                  "body": {"input": "{prompt}", "name": "{title}", "ref": "{session_id}",
                           "meta": {"repo": "{repo}", "n": 1}},
                  "id_path": "id", "url_path": "links.web"},
        "status": {"method": "GET", "path": "/v1/runs/{external_id}", "state_path": "status",
                   "state_map": {"queued": "running", "running": "running",
                                 "needs_input": "waiting_input", "done": "completed",
                                 "error": "failed"},
                   "result_path": "output.text", "poll_seconds": 5},
        "message": {"method": "POST", "path": "/v1/runs/{external_id}/messages",
                    "body": {"text": "{message}"}},
        "cancel": {"method": "POST", "path": "/v1/runs/{external_id}/cancel"},
    }
    profile.update(extra)
    return profile


def _install(manager, profile):
    entry = {"id": new_short_id(), **validate_profile(profile)}
    with manager.profiles.mutate() as profiles:
        profiles.append(entry)
    return entry


def _manager(tmp_path):
    clock = FakeClock()
    config = agents_config(tmp_path, agent_sandbox=False)
    return make_manager(tmp_path, config, supervise=False, inline_start=True, clock=clock), clock


def test_full_profile_lifecycle(tmp_path, stub, monkeypatch):
    manager, clock = _manager(tmp_path)
    profile = _install(manager, _profile(stub.url))
    monkeypatch.setenv(f"LOCALFLOW_AGENT_HTTP_{profile['id'].upper()}_TOKEN", TOKEN)
    stub.script("POST", "/v1/runs", (201, {"id": "run/7", "links": {"web": "https://w/7"}}))
    stub.script("GET", "/v1/runs/run%2F7", (200, {"status": "needs_input"}),
                (200, {"status": "running"}),
                (200, {"status": "done", "output": {"text": "all green"}}))
    stub.script("POST", "/v1/runs/run%2F7/messages", (200, {}))
    prompt = 'fix "quotes", {"inject": true} and }{'
    session, _ = manager.launch({"prompt": prompt, "provider": "http",
                                 "profile": profile["id"], "repo": "proj"})
    assert manager.store.get(session.id).external_id == "run/7"
    assert manager.store.get(session.id).external_url == "https://w/7"
    clock.advance(10)
    manager.tick()
    assert manager.store.get(session.id).state == "waiting_input"
    manager.send_message(session.id, "yes, go")
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "completed" and done.result_text == "all green"
    start = stub.requests[0]
    assert start["json"] == {"input": prompt, "name": prompt, "ref": session.id,
                             "meta": {"repo": "proj", "n": 1}}
    assert start["headers"]["Authorization"] == f"Bearer {TOKEN}"
    assert start["headers"]["X-Source"] == "localflow"
    assert [r["path"] for r in stub.requests] == ["/v1/runs", "/v1/runs/run%2F7",
                                                  "/v1/runs/run%2F7/messages",
                                                  "/v1/runs/run%2F7", "/v1/runs/run%2F7"]
    assert stub.requests[2]["json"] == {"text": "yes, go"}


def test_webhook_only_profile_completes_on_2xx(tmp_path, stub):
    manager, _clock = _manager(tmp_path)
    profile = _install(manager, {"label": "hook", "base_url": stub.url,
                                 "start": {"method": "POST", "path": "/hook",
                                           "body": {"text": "{prompt}"}}})
    stub.script("POST", "/hook", (200, {"accepted": True}))
    session, _ = manager.launch({"prompt": "ping", "provider": "http", "profile": profile["id"]})
    done = manager.store.get(session.id)
    assert done.state == "completed" and '"accepted": true' in done.result_text
    caps = manager.capabilities("http", profile["id"])
    assert not caps.live_message and not caps.cancel and not caps.reattach


def test_failed_state_and_cancel(tmp_path, stub):
    manager, clock = _manager(tmp_path)
    profile = _install(manager, _profile(stub.url, headers={}))
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    stub.script("GET", "/v1/runs/r1", (200, {"status": "error"}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "failed" and done.error["code"] == "upstream_error"
    stub.script("POST", "/v1/runs", (200, {"id": "r2"}))
    stub.script("POST", "/v1/runs/r2/cancel", (202, {}))
    second, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    assert manager.cancel(second.id).state == "cancelled"


def test_readiness_needs_profile_enabled_and_secret(tmp_path, stub):
    manager, _clock = _manager(tmp_path)
    assert manager.readiness("http", "").code == "missing_profile"
    profile = _install(manager, _profile(stub.url))
    assert manager.readiness("http", profile["id"]).code == "missing_secret"
    disabled = _install(manager, _profile(stub.url, enabled=False))
    assert manager.readiness("http", disabled["id"]).code == "disabled"
    assert http_provider.secret_name(profile["id"]) == f"agent_http_{profile['id']}_token"


@pytest.mark.parametrize("change,message", [
    ({"base_url": "http://agents.example.com"}, "https"),
    ({"base_url": "ftp://agents.example.com"}, "https"),
    ({"base_url": "https://user:pw@agents.example.com"}, "credentials"),
    ({"headers": {"Authorization": "Bearer abc123"}}, "secret"),
    ({"headers": {"X-Api-Key": "abc123"}}, "secret"),
    ({"headers": {"Cookie": "session=1"}}, "secret"),
    ({"headers": {"X-Bad": "a\r\nInjected: 1"}}, "one line"),
    ({"start": {"method": "TRACE", "path": "/x"}}, "method"),
    ({"start": {"method": "POST", "path": "x"}}, "start with /"),
    ({"start": {"method": "POST", "path": "/x", "body": {"k": "{secret}"}}}, "header values"),
    ({"start": {"method": "POST", "path": "/x", "id_path": "a..b"}}, "dotted"),
    ({"status": {"method": "GET", "path": "/s", "state_path": "s",
                 "state_map": {"x": "exploded"}}}, "session states"),
    ({"extra": 1}, "unknown field"),
])
def test_profile_validation(change, message):
    with pytest.raises(ProfileError, match=message):
        validate_profile(_profile("https://agents.example.com", **change))


def test_loopback_http_is_allowed():
    assert validate_profile(_profile("http://127.0.0.1:8080"))["base_url"] == \
        "http://127.0.0.1:8080"
    assert http_provider.origin("https://A.example.com/x") == "https://a.example.com:443"


def test_templating_never_builds_json_by_concatenation():
    values = {"prompt": '"}, "admin": true, {"', "title": "t"}
    body = http_provider.substitute({"input": "{prompt}", "list": ["{title}", 3]}, values)
    assert body == {"input": '"}, "admin": true, {"', "list": ["t", 3]}
