import dataclasses
import threading
import time

import pytest
from fixtures.agents.support import (
    FakeClock,
    agents_config,
    drive,
    isolate,
    make_manager,
    wait_for,
)

from localflow.agents.models import (
    AgentsDisabled,
    IdConflict,
    ProviderNotReady,
    QueueFull,
    SessionConflict,
    Unsupported,
    ValidationError,
)
from localflow.agents.providers.mock import MockProvider


@pytest.fixture(autouse=True)
def _env(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)


def _manager(tmp_path, config=None, **kw):
    clock = FakeClock()
    config = config or agents_config(tmp_path)
    manager = make_manager(tmp_path, config, supervise=False, inline_start=True, clock=clock,
                           **kw)
    return manager, clock


def _states(manager, session_id):
    return [e.state for e in manager.store.events(session_id, limit=1000) if e.type == "state"]


def _launch(manager, script="ok", **body):
    body = {"prompt": f"run the {script} script", "provider": "mock",
            "options": {"script": script}, **body}
    return manager.launch(body)[0]


def test_ok_script_lifecycle(tmp_path):
    manager, clock = _manager(tmp_path)
    session = _launch(manager)
    assert session.state == "queued" and session.sandbox is True
    done = drive(manager, clock, session.id)
    assert done.state == "completed" and done.result_text.startswith("done:")
    assert _states(manager, session.id) == ["queued", "starting", "running", "completed"]
    outputs = [e.text for e in manager.store.events(session.id) if e.type == "output"]
    assert outputs == ["step 1: working on it", "step 2: working on it"]
    seqs = [e.seq for e in manager.store.events(session.id)]
    assert seqs == list(range(1, done.last_seq + 1))


def test_fail_script(tmp_path):
    manager, clock = _manager(tmp_path)
    done = drive(manager, clock, _launch(manager, "fail").id)
    assert done.state == "failed" and done.error == {"code": "exit_nonzero",
                                                     "message": "mock run failed"}


def test_needs_input_then_message_completes(tmp_path):
    manager, clock = _manager(tmp_path)
    session = _launch(manager, "needs_input")
    clock.advance(1)
    manager.tick()
    assert manager.store.get(session.id).state == "waiting_input"
    with pytest.raises(ValidationError):
        manager.send_message(session.id, "   ")
    updated, event = manager.send_message(session.id, "use the blue button")
    assert updated.state == "running" and event.type == "message" and event.stream == "user"
    done = drive(manager, clock, session.id)
    assert done.state == "completed" and done.result_text == "received: use the blue button"
    assert _states(manager, session.id) == ["queued", "starting", "running", "waiting_input",
                                            "running", "completed"]
    with pytest.raises(SessionConflict):
        manager.send_message(session.id, "too late")


def test_hang_times_out(tmp_path):
    manager, clock = _manager(tmp_path)
    session = _launch(manager, "hang", timeout_seconds=30)
    done = drive(manager, clock, session.id, step=5)
    assert done.state == "timed_out" and done.error["code"] == "timeout"


def test_cancel_while_queued_and_while_running(tmp_path):
    config = agents_config(tmp_path, agent_max_concurrent=1)
    manager, clock = _manager(tmp_path, config)
    first = _launch(manager, "hang")
    second = _launch(manager, "hang")
    assert manager.store.get(second.id).state == "queued"
    cancelled = manager.cancel(second.id)
    assert cancelled.state == "cancelled" and cancelled.cancel_requested
    assert manager.cancel(second.id).state == "cancelled"  # idempotent
    running = manager.cancel(first.id)
    assert running.state == "cancelled"
    assert _states(manager, first.id)[-2:] == ["cancelling", "cancelled"]
    done = drive(manager, clock, _launch(manager, "ok").id)
    with pytest.raises(SessionConflict) as conflict:
        manager.cancel(done.id)
    assert conflict.value.current == "completed"


def test_flaky_cancel_fails_with_cancel_failed(tmp_path):
    manager, _clock = _manager(tmp_path)
    session = _launch(manager, "flaky_cancel")
    result = manager.cancel(session.id)
    assert result.state == "failed" and result.error["code"] == "cancel_failed"


def test_resume_creates_a_child(tmp_path):
    manager, clock = _manager(tmp_path)
    parent = drive(manager, clock, _launch(manager).id)
    child = _launch(manager, resume_of=parent.id, prompt="and now the follow-up")
    assert child.parent_id == parent.id and child.root_id == parent.id
    assert child.internal["resume_external_id"] == parent.external_id
    grandchild = _launch(manager, resume_of=drive(manager, clock, child.id).id,
                         prompt="third turn")
    assert grandchild.root_id == parent.id and grandchild.parent_id == child.id
    with pytest.raises(ValidationError):
        _launch(manager, resume_of="ffffffffffff")
    active = _launch(manager, "hang")
    with pytest.raises(ValidationError):
        _launch(manager, resume_of=active.id)


def test_resume_needs_the_capability(tmp_path):
    config = agents_config(tmp_path, agent_sandbox=False)
    manager, _clock = _manager(tmp_path, config)
    manager.ctx.get_secret = lambda name: "devin-key-123456" if "devin" in name else ""
    with pytest.raises(Unsupported):
        manager.launch({"prompt": "go on", "provider": "devin", "resume_of": "a" * 12})


def test_concurrency_queue_and_fifo(tmp_path):
    config = agents_config(tmp_path, agent_max_concurrent=2, agent_max_queued=3)
    manager, clock = _manager(tmp_path, config)
    sessions = [_launch(manager, "slow") for _ in range(5)]
    with pytest.raises(QueueFull):
        _launch(manager, "slow")
    states = [manager.store.get(s.id).state for s in sessions]
    assert states == ["running", "running", "queued", "queued", "queued"]
    started_order = []
    for _ in range(200):
        clock.advance(1)
        manager.tick()
        active = [s for s in manager.store.list() if s.state in ("starting", "running")]
        assert len(active) <= 2
        for s in active:
            if s.id not in started_order:
                started_order.append(s.id)
        if all(manager.store.get(s.id).state == "completed" for s in sessions):
            break
    assert started_order == [s.id for s in sessions]


def test_threaded_stress_never_exceeds_max_concurrent(tmp_path):
    config = agents_config(tmp_path, agent_max_concurrent=2, agent_max_queued=64)
    manager = make_manager(tmp_path, config, supervise=True)
    peak = {"value": 0}
    lock = threading.Lock()

    def observe():
        active = sum(1 for s in manager.store.list()
                     if s.state in ("starting", "running", "waiting_input", "cancelling"))
        with lock:
            peak["value"] = max(peak["value"], active)

    manager.subscribe(observe)
    ids = []

    def launcher():
        for _ in range(5):
            ids.append(_launch(manager, "ok").id)

    threads = [threading.Thread(target=launcher) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wait_for(lambda: all(manager.store.get(i).state == "completed" for i in ids), timeout=30)
    assert peak["value"] <= 2 and len(ids) == 20
    wait_for(lambda: not manager.supervisor_alive, timeout=2)


def test_idempotent_launch(tmp_path):
    manager, _clock = _manager(tmp_path)
    body = {"id": "0123456789ab", "prompt": "hello there", "provider": "mock"}
    first, created = manager.launch(body)
    again, created_again = manager.launch(dict(body))
    assert created and not created_again and again.id == first.id
    assert len(manager.store.list()) == 1
    with pytest.raises(IdConflict):
        manager.launch({**body, "prompt": "something else"})
    with pytest.raises(ValidationError):
        manager.launch({**body, "id": "NOT-HEX"})


def test_launch_validation(tmp_path):
    manager, _clock = _manager(tmp_path)
    with pytest.raises(ValidationError, match="unknown provider"):
        manager.launch({"prompt": "x y z", "provider": "nope"})
    with pytest.raises(ValidationError, match="unknown option"):
        manager.launch({"prompt": "x", "provider": "mock", "options": {"shell": "1"}})
    with pytest.raises(ValidationError, match="repo or cwd"):
        manager.launch({"prompt": "x", "repo": "proj", "cwd": "/tmp"})
    with pytest.raises(ValidationError, match="inside a configured repo"):
        manager.launch({"prompt": "x", "cwd": str(tmp_path)})
    with pytest.raises(ValidationError, match="unknown repo"):
        manager.launch({"prompt": "x", "repo": "elsewhere"})
    inner = tmp_path / "repos" / "proj" / "sub"
    inner.mkdir()
    session, _ = manager.launch({"prompt": "x", "cwd": str(inner)})
    assert session.repo == "proj" and session.cwd == str(inner.resolve())
    with pytest.raises(ValidationError, match="timeout_seconds"):
        manager.launch({"prompt": "x", "timeout_seconds": 5})
    with pytest.raises(ValidationError, match="empty"):
        manager.launch({"prompt": "  \n "})


def test_mock_needs_sandbox_and_claude_needs_a_repo(tmp_path):
    config = agents_config(tmp_path, agent_sandbox=False, agent_default_repo="",
                           agent_repos=[])
    manager, _clock = _manager(tmp_path, config)
    with pytest.raises(ValidationError, match="sandbox"):
        manager.launch({"prompt": "x", "provider": "mock"})
    with pytest.raises(ValidationError, match="choose a repo"):
        manager.launch({"prompt": "x", "provider": "claude"})


def test_disabled_and_not_ready(tmp_path, monkeypatch):
    config = agents_config(tmp_path)
    manager, _clock = _manager(tmp_path, dataclasses.replace(config, agents_enabled=False))
    with pytest.raises(AgentsDisabled):
        manager.launch({"prompt": "x"})
    real = agents_config(tmp_path, agent_sandbox=False)
    manager, _clock = _manager(tmp_path / "other", real)
    with pytest.raises(ProviderNotReady) as not_ready:
        manager.launch({"prompt": "x", "provider": "devin"})
    assert not_ready.value.readiness.code == "missing_secret"


def test_recovery_after_restart(tmp_path):
    config = agents_config(tmp_path, agent_max_concurrent=1)
    manager, _clock = _manager(tmp_path, config)
    running = _launch(manager, "hang")
    queued = _launch(manager, "hang")
    assert manager.store.get(running.id).state == "running"
    # A new server process: mock sessions re-attach, the queued one is interrupted.
    restarted, _ = _manager(tmp_path, config, store=None)
    assert restarted.store.get(queued.id).state == "failed"
    assert restarted.store.get(queued.id).error["code"] == "interrupted"
    assert restarted.store.get(running.id).state == "running"
    assert running.id in restarted._tracked
    assert restarted.cancel(running.id).state == "cancelled"


def test_recovery_interrupts_sessions_without_reattach(tmp_path):
    class LocalMock(MockProvider):
        capabilities = dataclasses.replace(MockProvider.capabilities, reattach=False)

    config = agents_config(tmp_path)
    manager, _clock = _manager(tmp_path, config, provider_classes={"mock": LocalMock})
    session = _launch(manager, "hang")
    restarted, _ = _manager(tmp_path, config, provider_classes={"mock": LocalMock})
    assert restarted.store.get(session.id).error["code"] == "interrupted"


def test_retention_prunes_on_terminal_transition(tmp_path):
    config = agents_config(tmp_path, agent_max_sessions=10)
    manager, clock = _manager(tmp_path, config)
    keep = _launch(manager, "hang")
    ids = [drive(manager, clock, _launch(manager, "fail").id).id for _ in range(12)]
    remaining = {s.id for s in manager.store.list()}
    assert keep.id in remaining and len(remaining) == 10
    assert set(ids[:3]).isdisjoint(remaining)


def test_supervisor_exits_after_the_last_session(tmp_path):
    config = agents_config(tmp_path)
    manager = make_manager(tmp_path, config, supervise=True)
    assert not manager.supervisor_alive
    session = _launch(manager, "ok")
    wait_for(lambda: manager.store.get(session.id).state == "completed", timeout=10)
    ended = time.monotonic()
    wait_for(lambda: not manager.supervisor_alive, timeout=2)
    assert time.monotonic() - ended <= 1.0


def test_delete_only_terminal(tmp_path):
    manager, clock = _manager(tmp_path)
    active = _launch(manager, "hang")
    with pytest.raises(SessionConflict):
        manager.delete(active.id)
    done = drive(manager, clock, _launch(manager).id)
    manager.delete(done.id)
    assert manager.store.get(done.id) is None
