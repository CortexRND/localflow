import pytest
from fixtures.agents.stub_agent_server import StubAgentServer
from fixtures.agents.support import (
    FakeClock,
    agents_config,
    drive,
    isolate,
    make_manager,
)

from localflow.agents.models import UpstreamError
from localflow.agents.providers import devin
from localflow.agents.providers.base import ProviderContext

KEY = "apk_user_testdevinkey0123456789"


@pytest.fixture
def stub(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    server = StubAgentServer()
    monkeypatch.setenv("LOCALFLOW_DEVIN_BASE_URL", server.url)
    monkeypatch.setenv("LOCALFLOW_AGENT_DEVIN_API_KEY", KEY)
    yield server
    server.close()


def _manager(tmp_path):
    clock = FakeClock()
    config = agents_config(tmp_path, agent_sandbox=False, agent_poll_seconds=10)
    return make_manager(tmp_path, config, supervise=False, inline_start=True, clock=clock), clock


def _status(status, messages=(), **extra):
    return (200, {"session_id": "devin-1", "status_enum": status,
                  "messages": [{"type": t, "message": m} for t, m in messages], **extra})


def _launch(manager, prompt="fix the build"):
    return manager.launch({"prompt": prompt, "provider": "devin", "repo": "proj"})[0]


def test_create_poll_message_cancel_sequence(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1",
                                               "url": "https://app.devin.ai/sessions/1"}))
    stub.script("GET", "/v1/sessions/devin-1",
                _status("working", [("devin_message", "Looking at the build.")]),
                _status("blocked", [("devin_message", "Looking at the build."),
                                    ("devin_message", "Which branch?")]),
                _status("working", [("devin_message", "Looking at the build."),
                                    ("devin_message", "Which branch?"),
                                    ("user_message", "main")]))
    stub.script("POST", "/v1/sessions/devin-1/message", (200, {}))
    stub.script("DELETE", "/v1/sessions/devin-1", (200, {}))
    manager, clock = _manager(tmp_path)
    session = _launch(manager)
    current = manager.store.get(session.id)
    assert current.state == "running" and current.external_id == "devin-1"
    assert current.external_url == "https://app.devin.ai/sessions/1"
    clock.advance(10)
    manager.tick()
    clock.advance(30)
    manager.tick()
    assert manager.store.get(session.id).state == "waiting_input"
    updated, event = manager.send_message(session.id, "main")
    assert updated.state == "running" and event.text == "main"
    manager.tick()
    assert manager.cancel(session.id).state == "cancelled"
    calls = [(r["method"], r["path"]) for r in stub.requests]
    assert calls == [("POST", "/v1/sessions"), ("GET", "/v1/sessions/devin-1"),
                     ("GET", "/v1/sessions/devin-1"), ("POST", "/v1/sessions/devin-1/message"),
                     ("GET", "/v1/sessions/devin-1"), ("DELETE", "/v1/sessions/devin-1")]
    assert all(r["headers"]["Authorization"] == f"Bearer {KEY}" for r in stub.requests)
    assert stub.requests[0]["json"] == {"prompt": "Repository: proj\n\nfix the build",
                                        "title": "fix the build", "idempotent": True}
    assert stub.requests[3]["json"] == {"message": "main"}
    outputs = [e.text for e in manager.store.events(session.id) if e.type == "output"]
    assert outputs == ["Looking at the build.", "Which branch?"]  # user messages not echoed


def test_finished_completes_with_the_last_message(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1", "url": ""}))
    stub.script("GET", "/v1/sessions/devin-1", _status("mystery_state"),
                _status("finished", [("devin_message", "PR opened: #42")]))
    manager, clock = _manager(tmp_path)
    session = _launch(manager)
    clock.advance(10)
    manager.tick()
    unknown = manager.store.get(session.id)
    assert unknown.state == "running" and unknown.internal["raw_status"] == "mystery_state"
    done = drive(manager, clock, session.id, step=20)
    assert done.state == "completed" and done.result_text == "PR opened: #42"


def test_expired_times_out(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("GET", "/v1/sessions/devin-1", _status("expired"))
    manager, clock = _manager(tmp_path)
    done = drive(manager, clock, _launch(manager).id, step=20)
    assert done.state == "timed_out" and done.error["code"] == "timeout"


@pytest.mark.parametrize("status,code", [(401, "auth_failed"), (403, "auth_failed"),
                                         (429, "rate_limited"), (500, "upstream_error")])
def test_start_errors(tmp_path, stub, status, code):
    stub.script("POST", "/v1/sessions", (status, {"detail": "nope"}))
    manager, _clock = _manager(tmp_path)
    done = manager.store.get(_launch(manager).id)
    assert done.state == "failed" and done.error["code"] == code


def test_rate_limit_honours_retry_after_then_recovers(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("GET", "/v1/sessions/devin-1", (429, {}, {"Retry-After": "42"}),
                _status("finished", [("devin_message", "done")]))
    manager, clock = _manager(tmp_path)
    session = _launch(manager)
    clock.advance(10)
    manager.tick()
    assert manager._tracked[session.id]["next_poll"] == pytest.approx(clock.now + 42)
    clock.advance(41)
    assert manager.tick() == 0
    clock.advance(1)
    manager.tick()
    assert manager.store.get(session.id).state == "completed"


def test_five_server_errors_fail_the_session(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("GET", "/v1/sessions/devin-1", (503, {}))
    manager, clock = _manager(tmp_path)
    done = drive(manager, clock, _launch(manager).id, step=120)
    assert done.state == "failed" and done.error["code"] == "upstream_error"
    assert sum(1 for r in stub.requests if r["method"] == "GET") == 5


def test_poll_auth_failure_is_final(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("GET", "/v1/sessions/devin-1", (401, {}))
    manager, clock = _manager(tmp_path)
    done = drive(manager, clock, _launch(manager).id, step=20)
    assert done.state == "failed" and done.error["code"] == "auth_failed"


def test_message_failure_is_an_upstream_error(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("POST", "/v1/sessions/devin-1/message", (500, {}))
    manager, _clock = _manager(tmp_path)
    session = _launch(manager)
    with pytest.raises(UpstreamError) as error:
        manager.send_message(session.id, "hi")
    assert error.value.code == "upstream_error" and error.value.status == 502


def test_cancel_404_counts_as_cancelled_and_500_as_cancel_failed(tmp_path, stub):
    stub.script("POST", "/v1/sessions", (200, {"session_id": "devin-1"}))
    stub.script("DELETE", "/v1/sessions/devin-1", (404, {}))
    manager, _clock = _manager(tmp_path)
    assert manager.cancel(_launch(manager).id).state == "cancelled"
    stub.script("DELETE", "/v1/sessions/devin-1", (500, {}))
    result = manager.cancel(_launch(manager).id)
    assert result.state == "failed" and result.error["code"] == "cancel_failed"


def test_network_error_at_start(tmp_path, stub, monkeypatch):
    monkeypatch.setenv("LOCALFLOW_DEVIN_BASE_URL", "http://127.0.0.1:9")
    manager, _clock = _manager(tmp_path)
    done = manager.store.get(_launch(manager).id)
    assert done.state == "failed" and done.error["code"] == "network_error"


def test_readiness_and_base_url(tmp_path, stub, monkeypatch):
    config = agents_config(tmp_path, agent_sandbox=False)
    from localflow.secrets import get_secret

    provider = devin.DevinProvider(config, ProviderContext(get_secret=get_secret))
    assert provider.readiness().ready
    stub.script("GET", "/v1/sessions?limit=1", (401, {}))
    assert provider.readiness(deep=True).code == "auth_failed"
    stub.script("GET", "/v1/sessions?limit=1", (200, {"sessions": []}))
    assert provider.readiness(deep=True).ready
    monkeypatch.delenv("LOCALFLOW_AGENT_DEVIN_API_KEY")
    assert provider.readiness().code == "missing_secret"
    monkeypatch.setenv("DEVIN_API_KEY", KEY)  # legacy env name
    assert provider.readiness().ready
    monkeypatch.delenv("LOCALFLOW_DEVIN_BASE_URL")
    assert devin.base_url() == "https://api.devin.ai"
    assert devin.DevinProvider.capabilities.reattach and not devin.DevinProvider.capabilities.resume
