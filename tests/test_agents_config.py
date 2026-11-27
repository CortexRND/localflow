import dataclasses

import pytest
from click.testing import CliRunner
from fixtures.agents.support import isolate

from localflow import api as api_logic
from localflow import config as config_module
from localflow import live_config
from localflow.config import (
    Config,
    ConfigError,
    apply_config_update,
    load_config,
    save_config,
)

AGENT_KEYS = {
    "enabled": ("agents_enabled", True),
    "hot_commands": ("agent_hot_commands", True),
    "default_provider": ("agent_default_provider", "devin"),
    "default_repo": ("agent_default_repo", "proj"),
    "repos": ("agent_repos", ["proj=/home/u/proj", "web app=/home/u/web"]),
    "max_concurrent": ("agent_max_concurrent", 4),
    "max_queued": ("agent_max_queued", 0),
    "timeout_seconds": ("agent_timeout_seconds", 600),
    "max_output_kb": ("agent_max_output_kb", 64),
    "max_sessions": ("agent_max_sessions", 50),
    "poll_seconds": ("agent_poll_seconds", 30),
    "allow_remote": ("agent_allow_remote", True),
    "sandbox": ("agent_sandbox", True),
    "claude_bin": ("agent_claude_bin", "/opt/claude/bin/claude"),
    "claude_wrapper": ("agent_claude_wrapper", "headroom"),
    "claude_permission_mode": ("agent_claude_permission_mode", "plan"),
    "claude_model": ("agent_claude_model", "claude-opus-5-5[1m]"),
    "claude_stream": ("agent_claude_stream", True),
}


@pytest.fixture(autouse=True)
def _env(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)


def test_defaults_are_off():
    config = Config()
    assert config.agents_enabled is False and config.agent_hot_commands is False
    assert config.agent_default_provider == "claude" and config.agent_repos == []
    assert (config.agent_max_concurrent, config.agent_max_queued) == (2, 8)
    assert config.agent_claude_wrapper == "none"
    assert config.agent_claude_permission_mode == "default"
    keys = config_module.valid_config_keys()
    assert {f"agents.{key}" for key in AGENT_KEYS} <= set(keys)
    assert set(config_module._SECTIONS["agents"]) == set(AGENT_KEYS)


def test_every_key_round_trips(tmp_path):
    update = {"agents": {key: value for key, (_field, value) in AGENT_KEYS.items()}}
    updated = apply_config_update(Config(), update)
    for key, (field, value) in AGENT_KEYS.items():
        assert getattr(updated, field) == value, key
    path = save_config(updated)
    assert "[agents]" in path.read_text(encoding="utf-8")
    loaded = load_config()
    assert config_module.config_to_dict(loaded)["agents"] == update["agents"]


@pytest.mark.parametrize("key,value", [
    ("max_concurrent", 0), ("max_concurrent", 9), ("max_queued", 65), ("max_queued", -1),
    ("timeout_seconds", 9), ("timeout_seconds", 86401), ("max_output_kb", 15),
    ("max_sessions", 2001), ("poll_seconds", 1), ("max_concurrent", True),
    ("claude_wrapper", "sudo"), ("claude_permission_mode", "bypassPermissions"),
    ("repos", ["no-equals-sign"]), ("repos", ["Upper=/x"]), ("repos", ["rel=relative/path"]),
    ("repos", ["a=/x", "a=/y"]), ("repos", ["a b=/x", "a_b=/y"]),
    ("repos", [f"r{i}=/x{i}" for i in range(33)]),
    ("claude_bin", "claude --dangerously-skip-permissions"), ("claude_bin", "bin/claude"),
    ("claude_bin", ""), ("claude_bin", "-claude"), ("claude_model", "model; rm -rf"),
    ("default_provider", ""), ("default_provider", "Bad Provider"),
    ("enabled", "yes"), ("default_repo", "missing"),
])
def test_invalid_values_are_rejected(key, value):
    with pytest.raises(ConfigError):
        apply_config_update(Config(), {"agents": {key: value}})


def test_invalid_values_in_the_file_fall_back_to_defaults(tmp_path, caplog):
    path = tmp_path / "config.toml"
    path.write_text(
        "config_version = 2\n[agents]\nmax_concurrent = 99\nclaude_wrapper = \"sudo\"\n"
        "repos = [\"proj=/p\"]\ndefault_repo = \"other\"\nenabled = true\n",
        encoding="utf-8",
    )
    loaded = load_config()
    assert loaded.agent_max_concurrent == 2 and loaded.agent_claude_wrapper == "none"
    assert loaded.agent_repos == ["proj=/p"] and loaded.agent_default_repo == ""
    assert loaded.agents_enabled is True
    assert "agents.max_concurrent" in caplog.text


def test_agent_keys_never_need_a_restart():
    update = {"agents": {key: value for key, (_field, value) in AGENT_KEYS.items()}}
    assert api_logic.restart_required(update) == []


def test_the_dictation_app_adopts_the_agent_fields_live():
    running = Config()
    fresh = dataclasses.replace(running, agents_enabled=True, agent_hot_commands=True,
                                agent_repos=["proj=/p"], agent_default_repo="proj",
                                agent_default_provider="devin")
    merged, changed = live_config.merge_live(running, fresh)
    assert changed == {"agents_enabled", "agent_hot_commands", "agent_repos",
                       "agent_default_repo", "agent_default_provider"}
    assert merged.agent_repos == ["proj=/p"]
    assert live_config.AGENT_FIELDS == {f for f, _v in AGENT_KEYS.values()}


def test_lf_config_set_reaches_the_server_without_restart(monkeypatch):
    from localflow import server
    from localflow.cli import cli

    result = CliRunner().invoke(cli, ["config", "set", "agents.enabled", "true"])
    assert result.exit_code == 0, result.output
    result = CliRunner().invoke(cli, ["config", "set", "agents.max_concurrent", "5"])
    assert result.exit_code == 0, result.output
    monkeypatch.setattr(server, "_config", Config(meetings_enabled=False, meeting_watch=False))
    server._apply_live_config(load_config())
    assert server._config.agents_enabled is True
    assert server._config.agent_max_concurrent == 5
    assert server._agents_config().agent_max_concurrent == 5


def test_secret_names_and_legacy_env(monkeypatch):
    from localflow.secrets import get_secret

    assert {"agent_devin_api_key", "agent_claude_api_key"} <= set(api_logic.SECRET_NAMES)
    monkeypatch.setenv("DEVIN_API_KEY", "legacy-devin-key")
    assert get_secret("agent_devin_api_key") == "legacy-devin-key"
    monkeypatch.setenv("LOCALFLOW_AGENT_DEVIN_API_KEY", "new-devin-key")
    assert get_secret("agent_devin_api_key") == "new-devin-key"
