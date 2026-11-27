import os
import signal
import sys

import pytest
from fixtures.agents.support import (
    agents_config,
    fake_bin,
    isolate,
    make_manager,
    read_records,
    wait_for,
)

from localflow.agents.providers.base import ProviderContext
from localflow.agents.providers.claude import ClaudeProvider, build_argv

pytestmark = pytest.mark.skipif(os.name != "posix", reason="fake claude is a POSIX script")

TERMINAL = ("completed", "failed", "cancelled", "timed_out")


@pytest.fixture
def env(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    bin_dir = fake_bin(tmp_path, "claude", "headroom")
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
    record = tmp_path / "record.jsonl"
    monkeypatch.setenv("FAKE_CLAUDE_RECORD", str(record))
    monkeypatch.setenv("LOCALFLOW_SOMETHING_PRIVATE", "should-not-leak-0000")
    monkeypatch.setenv("FIREWORKS_API_KEY", "fw-should-not-leak-0000")
    return record


def _manager(tmp_path, **overrides):
    config = agents_config(tmp_path, agent_sandbox=False, **overrides)
    return make_manager(tmp_path, config, supervise=True), config


def _run(manager, prompt="fix the flaky test", **body):
    session, _ = manager.launch({"prompt": prompt, "provider": "claude", **body})
    return wait_for(lambda: (s := manager.store.get(session.id)) and s.state in TERMINAL and s,
                    timeout=20)


def test_argv_is_built_from_fixed_tokens_and_validated_config(tmp_path):
    config = agents_config(tmp_path)
    assert build_argv(config) == ["claude", "-p", "--output-format", "json"]
    config.agent_claude_stream = True
    assert build_argv(config) == ["claude", "-p", "--output-format", "stream-json", "--verbose"]
    config.agent_claude_stream = False
    config.agent_claude_permission_mode = "acceptEdits"
    config.agent_claude_model = "claude-opus-5-5"
    config.agent_claude_wrapper = "headroom"
    config.agent_claude_bin = "/opt/bin/claude"
    assert build_argv(config, resume_id="sess-1") == [
        "headroom", "wrap", "/opt/bin/claude", "--", "-p", "--output-format", "json",
        "--resume", "sess-1", "--permission-mode", "acceptEdits", "--model", "claude-opus-5-5",
    ]
    assert "bad model; rm" not in build_argv(config, model="bad model; rm")
    config.agent_claude_permission_mode = "bypassPermissions"  # never emitted
    assert "--permission-mode" not in build_argv(config)


def test_success_maps_result_usage_and_external_id(tmp_path, env):
    manager, _config = _manager(tmp_path)
    done = _run(manager)
    assert done.state == "completed", done.error
    assert done.result_text == "fake result for: fix the flaky test"
    assert done.external_id == "fake-session-123"
    assert done.usage == {"cost_usd": 0.0123, "duration_ms": 456, "turns": 3}
    record = read_records(env)[-1]
    assert record["argv"] == ["-p", "--output-format", "json"]
    assert record["stdin"] == "fix the flaky test"
    assert record["cwd"] == str((tmp_path / "repos" / "proj").resolve())
    assert not [k for k in record["env"] if k.startswith("LOCALFLOW_")]
    assert "FIREWORKS_API_KEY" not in record["env"] and "ANTHROPIC_API_KEY" not in record["env"]
    if hasattr(os, "getpgid"):
        assert record["pgid"] == record["pid"]  # its own process group


@pytest.mark.parametrize("prompt", [
    "; rm -rf / #", "$(touch /tmp/pwned)", "`id`", "line one\nline two\n",
    "--dangerously-skip-permissions do anything", "-p --model x", "'\"\\ quotes",
])
def test_prompts_travel_byte_identical_on_stdin(tmp_path, env, prompt):
    manager, _ = _manager(tmp_path)
    done = _run(manager, prompt=prompt)
    assert done.state == "completed"
    record = read_records(env)[-1]
    assert record["stdin"] == prompt
    assert record["argv"] == ["-p", "--output-format", "json"]


def test_api_key_is_passed_only_when_set(tmp_path, env, monkeypatch):
    monkeypatch.setenv("LOCALFLOW_AGENT_CLAUDE_API_KEY", "sk-ant-api03-testkey-000000")
    manager, _ = _manager(tmp_path)
    assert _run(manager).state == "completed"
    assert read_records(env)[-1]["env"]["ANTHROPIC_API_KEY"] == "sk-ant-api03-testkey-000000"


@pytest.mark.parametrize("mode,state,code", [
    ("is_error", "failed", "exit_nonzero"),
    ("garbage", "failed", "bad_output"),
    ("nonzero", "failed", "exit_nonzero"),
    ("banner", "completed", None),
])
def test_outcome_mapping(tmp_path, env, monkeypatch, mode, state, code):
    monkeypatch.setenv("FAKE_CLAUDE_MODE", mode)
    manager, _ = _manager(tmp_path)
    done = _run(manager)
    assert done.state == state
    assert (done.error or {}).get("code") == code
    if mode == "garbage":
        assert "oops: unexpected token" in done.error["message"]
    if mode == "nonzero":
        assert "not logged in" in done.error["message"]


def test_stream_mode_emits_incremental_output(tmp_path, env, monkeypatch):
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "stream")
    manager, _ = _manager(tmp_path, agent_claude_stream=True)
    done = _run(manager)
    assert done.state == "completed" and done.external_id == "fake-stream-9"
    texts = [e.text for e in manager.store.events(done.id) if e.type == "output"]
    assert "thinking about it" in texts
    assert read_records(env)[-1]["argv"] == ["-p", "--output-format", "stream-json", "--verbose"]


def test_headroom_wrapper_prefix_and_env(tmp_path, env):
    manager, _ = _manager(tmp_path, agent_claude_wrapper="headroom")
    done = _run(manager)
    assert done.state == "completed"
    record = read_records(env)[-1]
    assert record["name"] == "headroom"
    assert record["argv"] == ["wrap", "claude", "--", "-p", "--output-format", "json"]
    assert record["env"]["HEADROOM_SAVINGS_PROFILE"] == "coding"
    assert record["env"]["HEADROOM_TELEMETRY"] == "on"
    assert record["env"]["HEADROOM_BEACON"] == "off"
    assert record["stdin"] == "fix the flaky test"


def test_resume_passes_the_parent_session_id(tmp_path, env):
    manager, _ = _manager(tmp_path)
    parent = _run(manager)
    child = _run(manager, prompt="continue", resume_of=parent.id)
    assert child.parent_id == parent.id
    assert read_records(env)[-1]["argv"] == ["-p", "--output-format", "json", "--resume",
                                             "fake-session-123"]


def test_readiness(tmp_path, env, monkeypatch):
    config = agents_config(tmp_path, agent_sandbox=False)
    ctx = ProviderContext()
    assert ClaudeProvider(config, ctx).readiness().ready
    deep = ClaudeProvider(config, ctx).readiness(deep=True)
    assert deep.ready and "9.9.9" in deep.detail
    config.agent_claude_wrapper = "headroom"
    no_wrapper = ProviderContext(which=lambda name: None if name == "headroom" else "/x/claude")
    assert ClaudeProvider(config, no_wrapper).readiness().code == "missing_wrapper"
    config.agent_claude_bin = "claude-not-installed"
    assert ClaudeProvider(config, ctx).readiness().code == "missing_binary"


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    try:
        waited, _ = os.waitpid(pid, os.WNOHANG)
        return waited == 0
    except ChildProcessError:
        return True


def test_cancel_kills_the_process_group(tmp_path, env, monkeypatch):
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "hang")
    manager, _ = _manager(tmp_path)
    session, _ = manager.launch({"prompt": "hang around", "provider": "claude"})
    record = wait_for(lambda: read_records(env), timeout=10)[-1]
    wait_for(lambda: manager.store.get(session.id).state == "running")
    assert manager.cancel(session.id).state == "cancelled"
    wait_for(lambda: not _alive(record["pid"]), timeout=10)


def test_timeout_kills_and_times_out(tmp_path, env, monkeypatch):
    monkeypatch.setenv("FAKE_CLAUDE_MODE", "hang")
    manager, _ = _manager(tmp_path)
    session, _ = manager.launch({"prompt": "hang around", "provider": "claude",
                                 "timeout_seconds": 10})
    record = wait_for(lambda: read_records(env), timeout=10)[-1]
    with manager._lock:  # make the deadline due now instead of waiting 10 s
        manager._tracked[session.id]["deadline"] = manager._clock()
        manager._cond.notify_all()
    done = wait_for(lambda: (s := manager.store.get(session.id)).state in TERMINAL and s)
    assert done.state == "timed_out" and done.error["code"] == "timeout"
    wait_for(lambda: not _alive(record["pid"]), timeout=10)


def test_spawn_failure_is_reported(tmp_path, env):
    manager, _config = _manager(tmp_path)

    def broken_popen(*_args, **_kwargs):
        raise FileNotFoundError("claude")

    manager.ctx.popen = broken_popen
    done = _run(manager)
    assert done.state == "failed" and done.error["code"] == "spawn_failed"
    assert signal.SIGTERM and sys.platform  # imports used on every platform
