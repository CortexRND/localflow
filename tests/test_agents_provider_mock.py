import pytest
from fixtures.agents.support import (
    FakeClock,
    agents_config,
    drive,
    isolate,
    make_manager,
)

from localflow.agents.models import MAX_RESULT_CHARS
from localflow.agents.providers.mock import PLANTED_FALLBACK, SCRIPTS, MockProvider
from localflow.agents.store import EVENT_RESERVE_BYTES


@pytest.fixture(autouse=True)
def _env(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)


def _manager(tmp_path, **overrides):
    clock = FakeClock()
    config = agents_config(tmp_path, **overrides)
    return make_manager(tmp_path, config, supervise=False, inline_start=True, clock=clock), clock


def test_capabilities_and_readiness(tmp_path):
    caps = MockProvider.capabilities
    assert caps.kind == "cloud" and caps.secret_names == ()
    assert all([caps.resume, caps.live_message, caps.cancel, caps.streaming, caps.reattach])
    manager, _ = _manager(tmp_path, agent_sandbox=False)
    assert manager.readiness("mock").code == "disabled"
    manager, _ = _manager(tmp_path / "b")
    assert manager.readiness("mock").ready


def test_flood_is_bounded(tmp_path):
    manager, clock = _manager(tmp_path, agent_max_output_kb=16)
    session, _ = manager.launch({"prompt": "flood it", "provider": "mock",
                                 "options": {"script": "flood"}})
    done = drive(manager, clock, session.id)
    assert done.state == "completed"
    events = manager.store.events(session.id, limit=10**6)
    assert [e.type for e in events].count("truncated") == 1
    assert events[-1].type == "state" and events[-1].state == "completed"
    size = (manager.store.events_dir / f"{session.id}.jsonl").stat().st_size
    assert size <= 16 * 1024 + EVENT_RESERVE_BYTES
    assert len(done.result_text) <= MAX_RESULT_CHARS and done.result_truncated is True


def test_secret_echo_is_redacted(tmp_path, monkeypatch):
    planted = "sim-secret-devin-0123456789abcdef"
    monkeypatch.setenv("LOCALFLOW_AGENT_DEVIN_API_KEY", planted)
    manager, clock = _manager(tmp_path)
    manager.redactor.refresh()
    session, _ = manager.launch({"prompt": f"echo {planted}", "provider": "mock",
                                 "options": {"script": "secret_echo"}})
    done = drive(manager, clock, session.id)
    assert planted not in done.prompt and "[redacted:agent_devin_api_key]" in done.prompt
    assert planted not in done.result_text
    texts = " ".join(e.text for e in manager.store.events(session.id))
    assert planted not in texts and "[redacted:agent_devin_api_key]" in texts


def test_secret_echo_fallback_matches_a_pattern(tmp_path):
    manager, clock = _manager(tmp_path)
    session, _ = manager.launch({"prompt": "echo", "provider": "mock",
                                 "options": {"script": "secret_echo"}})
    done = drive(manager, clock, session.id)
    assert PLANTED_FALLBACK not in done.result_text


def test_sandbox_routes_every_launch_to_mock(tmp_path):
    manager, clock = _manager(tmp_path)
    session, _ = manager.launch({"prompt": "fix the tests please", "provider": "claude"})
    assert session.provider == "claude" and session.sandbox is True
    assert session.effective_provider == "mock"
    assert drive(manager, clock, session.id).state == "completed"


def test_unknown_script_is_rejected(tmp_path):
    from localflow.agents.models import ValidationError

    manager, _ = _manager(tmp_path)
    with pytest.raises(ValidationError, match="script"):
        manager.launch({"prompt": "x", "provider": "mock", "options": {"script": "rm -rf"}})
    assert "ok" in SCRIPTS
