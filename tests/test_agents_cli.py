import dataclasses
import json
from types import SimpleNamespace
from urllib.parse import urlsplit

import pytest
import requests
from click.testing import CliRunner
from fixtures.agents.support import (
    agents_config,
    api_client,
    isolate,
    make_manager,
    wait_for,
)

from localflow import server
from localflow.agents import api as agents_api
from localflow.agents.providers.mock import MockProvider
from localflow.cli import cli


class FailingMock(MockProvider):
    def _run(self, session):
        run = super()._run(session)
        run["script"] = "fail"
        return run


class HangMock(MockProvider):
    def _run(self, session):
        run = super()._run(session)
        run["script"] = "hang"
        return run


@pytest.fixture
def env(tmp_path, monkeypatch):
    keyring = isolate(tmp_path, monkeypatch)
    config = agents_config(tmp_path)
    manager = make_manager(tmp_path, lambda: server._config, supervise=True)
    client = api_client(monkeypatch, config, manager)
    state = {"down": False}

    def fake_request(method, url, json=None, params=None, headers=None, timeout=None,
                     allow_redirects=None):
        if state["down"]:
            raise requests.ConnectionError("refused")
        response = client.request(method, urlsplit(url).path, json=json, params=params)
        return SimpleNamespace(ok=response.status_code < 400, status_code=response.status_code,
                               reason=response.reason_phrase, json=response.json)

    monkeypatch.setattr(requests, "request", fake_request)
    yield SimpleNamespace(manager=manager, state=state, keyring=keyring,
                          runner=CliRunner(), monkeypatch=monkeypatch)
    manager.shutdown()
    agents_api.set_manager(None)


def run(env, *args, input=None):
    return env.runner.invoke(cli, ["agents", *args], input=input, catch_exceptions=False)


def test_group_is_registered_next_to_the_autostart_alias():
    assert "agents" in cli.commands and "agent" in cli.commands
    assert cli.commands["agents"] is not cli.commands["agent"]
    # §7 specifies `lf agents status`; it is the only name the autostart alias also has.
    shared = set(cli.commands["agents"].commands) & set(cli.commands["agent"].commands)
    assert shared == {"status"}


def test_launch_follow_list_show_logs(env):
    result = run(env, "launch", "say hello", "-p", "claude", "--follow")
    assert result.exit_code == 0, result.output
    assert "step 2: working on it" in result.output
    listed = json.loads(run(env, "list", "--json").output)["sessions"]
    assert len(listed) == 1 and listed[0]["state"] == "completed"
    session_id = listed[0]["id"]
    assert listed[0]["origin"] == "cli"
    shown = run(env, "show", session_id[:5])
    assert f"id: {session_id}" in shown.output and "done: say hello" in shown.output
    logs = run(env, "logs", session_id, "--json")
    events = [json.loads(line) for line in logs.output.splitlines()]
    assert [e["seq"] for e in events] == list(range(1, len(events) + 1))
    status = json.loads(run(env, "status", "--json").output)
    assert status["counts"]["terminal"] == 1


def test_follow_exits_nonzero_unless_completed(env):
    env.manager._classes["mock"] = FailingMock
    result = run(env, "launch", "this will fail", "--follow")
    assert result.exit_code == 1 and "exit_nonzero" in result.output


def test_launch_from_stdin_and_errors(env):
    result = run(env, "launch", "-F", "-", "--json", input="prompt from stdin\n")
    assert result.exit_code == 0
    assert json.loads(result.output)["session"]["prompt"] == "prompt from stdin\n"
    both = run(env, "launch", "x", "-F", "-")
    assert both.exit_code != 0
    bad = run(env, "launch", "x", "-r", "nowhere")
    assert bad.exit_code == 1 and "unknown repo" in bad.output


def test_cancel_and_rm(env):
    env.monkeypatch.setattr(server, "_config", dataclasses.replace(server._config,
                                                                   agent_max_concurrent=1))
    env.manager._classes["mock"] = HangMock
    first = json.loads(run(env, "launch", "one", "--json").output)["session"]["id"]
    second = json.loads(run(env, "launch", "two", "--json").output)["session"]["id"]
    wait_for(lambda: env.manager.store.get(first).state == "running")
    result = run(env, "cancel", second)
    assert result.exit_code == 0 and "cancelled" in result.output
    result = run(env, "cancel", "--all", "-y")
    assert f"{first}: cancelled" in result.output
    removed = run(env, "rm", "--finished", "-y")
    assert removed.exit_code == 0 and removed.output.count("deleted") == 2
    assert run(env, "list").output.strip() == "no agent sessions"


def test_offline_fallbacks(env):
    assert run(env, "launch", "say hello", "--follow").exit_code == 0
    session_id = env.manager.store.list()[0].id
    env.state["down"] = True
    listed = run(env, "list", "--json")
    assert json.loads(listed.output)["sessions"][0]["id"] == session_id
    assert "state: completed" in run(env, "show", session_id).output
    assert "step 1" in run(env, "logs", session_id).output
    down = run(env, "launch", "anything")
    assert down.exit_code == 1 and "server not running" in down.output


def test_rules_online_and_offline(env, monkeypatch):
    monkeypatch.setattr("localflow.agents.rules.hot_phrase_triggers", list)
    added = run(env, "rules", "add", "launch claude", "-p", "claude", "-r", "proj")
    assert added.exit_code == 0, added.output
    rule_id = added.output.split()[1].rstrip(":")
    env.monkeypatch.setattr(server, "_config", dataclasses.replace(server._config,
                                                                   agent_hot_commands=True))
    tested = json.loads(run(env, "rules", "test", "launch claude to fix all tests",
                            "--json").output)
    assert tested["matched"] and tested["prompt"] == "fix all tests"
    assert run(env, "rules", "disable", rule_id).output.strip() == f"{rule_id}: disabled"
    listed = json.loads(run(env, "rules", "list", "--json").output)["rules"]
    assert listed[0]["enabled"] is False
    env.state["down"] = True  # offline writes go straight to the shared file
    from localflow.config import save_config

    save_config(server._config)
    assert run(env, "rules", "enable", rule_id).exit_code == 0
    offline = json.loads(run(env, "rules", "list", "--json").output)["rules"]
    assert offline[0]["enabled"] is True
    assert run(env, "rules", "rm", rule_id).exit_code == 0
    assert run(env, "rules", "list").output.strip() == "no hot commands"


def test_providers_and_http(env, tmp_path):
    providers = json.loads(run(env, "providers", "--json").output)["providers"]
    assert {p["id"] for p in providers} >= {"claude", "devin", "mock", "http"}
    profile_file = tmp_path / "profile.json"
    profile_file.write_text(json.dumps({
        "label": "Hook", "base_url": "https://hooks.example.com",
        "start": {"method": "POST", "path": "/go", "body": {"t": "{prompt}"}}}))
    added = run(env, "http", "add", str(profile_file))
    assert added.exit_code == 0, added.output
    profiles = json.loads(run(env, "http", "list", "--json").output)["profiles"]
    assert profiles[0]["label"] == "Hook"
    assert run(env, "http", "rm", profiles[0]["id"]).exit_code == 0


def test_secret_set_never_echoes(env):
    result = run(env, "secret", "set", "devin", input="apk_user_secretvalue1234\n")
    assert result.exit_code == 0 and "apk_user_secretvalue1234" not in result.output
    assert env.keyring.values[("localflow", "agent_devin_api_key")] == "apk_user_secretvalue1234"
    assert run(env, "secret", "unset", "devin").exit_code == 0
    assert ("localflow", "agent_devin_api_key") not in env.keyring.values
    assert run(env, "secret", "set", "http").exit_code != 0  # needs --profile
