import json

import pytest
from fixtures.agents.stub_agent_server import StubAgentServer
from fixtures.agents.support import (
    FakeClock,
    agents_config,
    drive,
    isolate,
    make_manager,
)

from localflow.agents.models import Unsupported, new_short_id
from localflow.agents.providers import http as http_provider
from localflow.agents.providers.base import is_loopback_url
from localflow.agents.providers.http import ProfileError, validate_profile
from localflow.secrets import delete_secret, set_secret

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


# -- regressions (PR #185 review) ---------------------------------------------------


@pytest.mark.parametrize("url,loopback", [
    ("http://127.attacker.com", False),
    ("http://127.0.0.1.attacker.com:8080", False),
    ("http://localhost.attacker.com", False),
    ("http://127.0.0.1:8080", True),
    ("http://127.9.9.9", True),
    ("http://localhost:3000", True),
    ("http://[::1]:8080", True),
    ("http://10.0.0.1", False),
])
def test_loopback_url_needs_a_loopback_address_not_a_prefix(url, loopback):
    assert is_loopback_url(url) is loopback


def test_plain_http_to_a_127_dns_name_is_rejected():
    with pytest.raises(ProfileError, match="https"):
        validate_profile(_profile("http://127.attacker.com"))


def test_remote_cancelled_ends_a_running_session(tmp_path, stub):
    manager, clock = _manager(tmp_path)
    status = {**_profile(stub.url)["status"],
              "state_map": {"running": "running", "stopped": "cancelled"}}
    profile = _install(manager, _profile(stub.url, headers={}, status=status))
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    stub.script("GET", "/v1/runs/r1", (200, {"status": "running"}), (200, {"status": "stopped"}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    clock.advance(10)
    manager.tick()
    assert manager.store.get(session.id).state == "running"
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "cancelled" and done.error is None
    assert len(stub.requests) == 3  # ended on the remote report, not at the deadline


def test_profile_edit_does_not_change_an_active_session(tmp_path, stub):
    manager, clock = _manager(tmp_path)
    profile = _install(manager, _profile(stub.url, headers={}))
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    stub.script("GET", "/v1/runs/r1", (200, {"status": "running"}),
                (200, {"status": "done", "output": {"text": "ok"}}))
    stub.script("GET", "/v2/jobs/r1", (200, {"status": "error"}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    with manager.profiles.mutate() as profiles:
        entry = next(p for p in profiles if p["id"] == profile["id"])
        entry["status"] = {**entry["status"], "path": "/v2/jobs/{external_id}"}
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "completed" and done.result_text == "ok"
    assert all(r["path"].startswith("/v1/") for r in stub.requests)
    assert session.id not in manager._session_providers  # dropped on the terminal state
    # A new session uses the edited profile.
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    second, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    assert drive(manager, clock, second.id, step=5).state == "failed"
    assert stub.requests[-1]["path"] == "/v2/jobs/r1"


def test_token_is_withheld_after_an_origin_change(tmp_path, stub, monkeypatch):
    other = StubAgentServer()
    try:
        manager, clock = _manager(tmp_path)
        profile = _install(manager, _profile(stub.url))
        with manager.profiles.mutate() as profiles:
            entry = next(p for p in profiles if p["id"] == profile["id"])
            entry["secret_origin"] = http_provider.origin(stub.url)
            entry["base_url"] = other.url  # moved without setting the token again
        monkeypatch.setenv(f"LOCALFLOW_AGENT_HTTP_{profile['id'].upper()}_TOKEN", TOKEN)
        assert manager.readiness("http", profile["id"]).code == "missing_secret"
        assert not http_provider.secret_bound(manager.profiles.get(profile["id"]))
        provider = manager.provider_for("http", profile["id"])
        assert provider._secret() == ""
        # Setting the token again (keyring) binds it to the new origin.
        set_secret(http_provider.secret_name(profile["id"]), TOKEN)
        http_provider.bind_secret(manager.profiles, profile["id"])
        assert manager.readiness("http", profile["id"]).ready
        other.script("POST", "/v1/runs", (200, {"id": "r9"}))
        other.script("GET", "/v1/runs/r9", (200, {"status": "done"}))
        session, _ = manager.launch({"prompt": "go", "provider": "http",
                                     "profile": profile["id"]})
        drive(manager, clock, session.id, step=5)
        assert other.requests[0]["headers"]["Authorization"] == f"Bearer {TOKEN}"
        assert stub.requests == []
    finally:
        other.close()


def test_profiles_without_a_secret_origin_keep_their_token():
    assert http_provider.secret_bound({"id": "a", "base_url": "https://x.example.com"})
    assert http_provider.secret_bound({"id": "a", "base_url": "https://x.example.com",
                                       "secret_origin": "https://x.example.com:443"})
    assert not http_provider.secret_bound({"id": "a", "base_url": "https://y.example.com",
                                           "secret_origin": "https://x.example.com:443"})
    assert not http_provider.secret_bound({"id": "a", "base_url": "https://y.example.com",
                                           "secret_origin": ""})


# -- regressions (PR #185 review, round 2) -------------------------------------------


def _set_token(manager, profile_id, value):
    set_secret(http_provider.secret_name(profile_id), value)
    http_provider.bind_secret(manager.profiles, profile_id)


def test_an_active_run_never_receives_a_replacement_token(tmp_path, stub):
    other = StubAgentServer()
    try:
        manager, clock = _manager(tmp_path)
        profile = _install(manager, _profile(stub.url))
        _set_token(manager, profile["id"], TOKEN)
        stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
        stub.script("GET", "/v1/runs/r1", (200, {"status": "done", "output": {"text": "ok"}}))
        session, _ = manager.launch({"prompt": "go", "provider": "http",
                                     "profile": profile["id"]})
        # The profile moves and gets a replacement token for the new origin.
        with manager.profiles.mutate() as profiles:
            entry = next(p for p in profiles if p["id"] == profile["id"])
            entry["base_url"] = other.url
        replacement = "replacement-token-for-new-origin-0123"
        _set_token(manager, profile["id"], replacement)
        done = drive(manager, clock, session.id, step=5)
        assert done.state == "completed"
        assert [r["headers"]["Authorization"] for r in stub.requests] == [f"Bearer {TOKEN}"] * 2
        assert other.requests == []
    finally:
        other.close()


def test_keyring_token_wins_over_a_stale_environment_token(tmp_path, stub, monkeypatch):
    manager, clock = _manager(tmp_path)
    profile = _install(manager, _profile(stub.url))
    monkeypatch.setenv(f"LOCALFLOW_AGENT_HTTP_{profile['id'].upper()}_TOKEN",
                       "stale-env-token-for-old-origin")
    _set_token(manager, profile["id"], TOKEN)
    assert manager.profiles.get(profile["id"])["secret_source"] == "keyring"
    assert manager.provider_for("http", profile["id"])._secret() == TOKEN
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    stub.script("GET", "/v1/runs/r1", (200, {"status": "done"}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    drive(manager, clock, session.id, step=5)
    assert {r["headers"]["Authorization"] for r in stub.requests} == {f"Bearer {TOKEN}"}


def test_cancel_and_message_use_the_pinned_profile_capabilities(tmp_path, stub):
    manager, _clock = _manager(tmp_path)
    base = _profile(stub.url, headers={})
    del base["cancel"], base["message"]
    profile = _install(manager, base)
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    with manager.profiles.mutate() as profiles:  # added after the run started
        entry = next(p for p in profiles if p["id"] == profile["id"])
        entry["message"] = _profile(stub.url)["message"]
        entry["cancel"] = _profile(stub.url)["cancel"]
    assert manager.capabilities("http", profile["id"]).cancel  # the live profile
    with pytest.raises(Unsupported):
        manager.send_message(session.id, "hello")
    cancelled = manager.cancel(session.id)
    assert cancelled.state == "cancelled" and cancelled.error is None
    assert [r["path"] for r in stub.requests] == ["/v1/runs"]


# -- regressions (PR #185 review, round 3) -------------------------------------------


def _leaks(manager, session_id, secret):
    session = manager.store.get(session_id)
    events = json.dumps([e.to_api() if hasattr(e, "to_api") else repr(e)
                         for e in manager.store.events(session_id)])
    on_disk = "".join(f.read_text(errors="replace")
                      for f in manager.store.root.rglob("*") if f.is_file())
    return any(secret in text for text in (json.dumps(session.to_api()), events, on_disk))


def test_same_origin_token_rotation_reaches_an_active_run(tmp_path, stub):
    manager, clock = _manager(tmp_path)
    profile = _install(manager, _profile(stub.url))
    _set_token(manager, profile["id"], TOKEN)
    stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
    stub.script("GET", "/v1/runs/r1", (200, {"status": "running"}),
                (200, {"status": "done", "output": {"text": f"old was {TOKEN}"}}))
    session, _ = manager.launch({"prompt": "go", "provider": "http", "profile": profile["id"]})
    rotated = "rotated-token-same-origin-0123456"
    _set_token(manager, profile["id"], rotated)  # the old token is revoked upstream
    manager.redactor.refresh()
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "completed"
    assert [r["headers"]["Authorization"] for r in stub.requests] == [
        f"Bearer {TOKEN}", f"Bearer {rotated}", f"Bearer {rotated}"]
    # The replaced token is still redacted after the rotation dropped it from the keyring.
    assert TOKEN not in done.result_text and not _leaks(manager, session.id, TOKEN)


@pytest.mark.parametrize("change", ["move", "delete_and_replace"])
def test_moved_or_deleted_profile_keeps_the_pinned_token_redacted(tmp_path, stub, change):
    other = StubAgentServer()
    try:
        manager, clock = _manager(tmp_path)
        profile = _install(manager, _profile(stub.url))
        _set_token(manager, profile["id"], TOKEN)
        stub.script("POST", "/v1/runs", (200, {"id": "r1"}))
        stub.script("GET", "/v1/runs/r1",
                    (200, {"status": "done", "output": {"text": f"echo {TOKEN}"}}))
        session, _ = manager.launch({"prompt": "go", "provider": "http",
                                     "profile": profile["id"]})
        with manager.profiles.mutate() as profiles:
            if change == "move":
                entry = next(p for p in profiles if p["id"] == profile["id"])
                entry["base_url"] = other.url
            else:  # deleted, then recreated under the same id for another origin
                profiles[:] = [p for p in profiles if p["id"] != profile["id"]]
                profiles.append({"id": profile["id"], **validate_profile(_profile(other.url))})
        delete_secret(http_provider.secret_name(profile["id"]))
        manager.redactor.refresh()  # what the API does; the old token is no longer resolvable
        replacement = "replacement-token-for-new-origin-0123"
        _set_token(manager, profile["id"], replacement)
        manager.redactor.refresh()
        done = drive(manager, clock, session.id, step=5)
        assert done.state == "completed"
        assert [r["headers"]["Authorization"] for r in stub.requests] == [f"Bearer {TOKEN}"] * 2
        assert other.requests == []
        assert done.result_text == f"echo [redacted:{http_provider.secret_name(profile['id'])}]"
        assert not _leaks(manager, session.id, TOKEN)
    finally:
        other.close()


@pytest.mark.parametrize("base", ["https://agents.example.com:99999",
                                  "https://agents.example.com:abc"])
def test_invalid_ports_are_rejected(base):
    with pytest.raises(ProfileError, match="invalid port"):
        validate_profile(_profile(base))
