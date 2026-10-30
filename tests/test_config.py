import logging
from dataclasses import fields

import pytest
from click.testing import CliRunner

from localflow import config
from localflow.cli import cli
from localflow.config import Config, ConfigError
from localflow.providers.factory import build_llm


class MemoryKeyring:
    def __init__(self, values=None):
        self.values = dict(values or {})

    def get_password(self, service, name):
        return self.values.get((service, name))

    def set_password(self, service, name, value):
        self.values[(service, name)] = value

    def delete_password(self, service, name):
        self.values.pop((service, name), None)


@pytest.fixture
def isolated_config(monkeypatch, tmp_path):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setattr(config, "legacy_config_path", lambda: tmp_path / ".localflow.toml")
    return path


def test_defaults_when_no_file(isolated_config):
    loaded = config.load_config()

    assert loaded == Config()


def test_v2_round_trip_for_non_default_config(isolated_config, monkeypatch):
    monkeypatch.setattr(config.sys, "platform", "darwin")
    expected = Config(
        model_size="small",
        stt_backend="mlx-whisper",
        language="en",
        stt_device="cpu",
        llm_provider="openai-compatible",
        llm_base_url="https://example.test/v1",
        llm_model="model",
        cleanup_enabled=False,
        cleanup_timeout=17,
        cleanup_num_ctx=4096,
        hotkey="<cmd>+<shift>+<space>",
        sounds_enabled=False,
        paste_method="clipboard",
        spoken_symbols=False,
        command_names=["one", "two"],
        skills_dirs=["/skills"],
        sample_rate=8000,
        server_host="0.0.0.0",
        server_port=9000,
        meetings_enabled=True,
        vault_path="/vault",
        notes_folder="meetings",
        logs_folder="transcripts",
        meeting_watch=False,
        meeting_chunk_seconds=15,
        meeting_min_busy_seconds=4,
        meeting_auto_start=False,
        work_prompts=False,
        fireworks_model="fireworks",
        prompts_dir="/prompts",
        orca_repo="path:/repo",
        orca_agent="agent",
    )

    config.save_config(expected)

    assert config.load_config() == expected


def test_unknown_section_and_key_are_warned(isolated_config, caplog):
    isolated_config.write_text(
        '[stt]\nmodel = "tiny"\nunknown = true\n[unknown]\nvalue = 1\n'
    )

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert loaded.model_size == "tiny"
    assert "unknown config key stt.unknown" in caplog.text
    assert "unknown config section 'unknown'" in caplog.text


def test_malformed_toml_raises_config_error(isolated_config):
    isolated_config.write_text("[stt\n")

    with pytest.raises(ConfigError) as exc_info:
        config.load_config()

    assert str(isolated_config) in str(exc_info.value)
    assert "Expected" in str(exc_info.value)


def test_newer_config_version_raises_config_error(isolated_config):
    isolated_config.write_text("config_version = 3\n")

    with pytest.raises(ConfigError, match="config_version 3 is newer"):
        config.load_config()


def test_missing_config_version_warns_and_loads(isolated_config, caplog):
    isolated_config.write_text("[server]\nport = 9001\n")

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert loaded.server_port == 9001
    assert "missing config_version" in caplog.text


def test_empty_language_maps_to_none(isolated_config):
    isolated_config.write_text('[stt]\nlanguage = ""\n')

    assert config.load_config().language is None


def test_migration_maps_every_v1_field_and_stores_secret(
    isolated_config, monkeypatch, tmp_path
):
    from localflow import secrets

    keyring = MemoryKeyring()
    monkeypatch.setattr(secrets, "keyring", keyring)
    legacy = tmp_path / ".localflow.toml"
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)
    legacy.write_text(
        """
model_size = "small"
stt_backend = "mlx"
language = "en"
ollama_url = "http://ollama"
ollama_model = "model"
cleanup_enabled = false
cleanup_timeout = 30
cleanup_num_ctx = 2048
hotkey = "space"
sounds_enabled = false
paste_method = "clipboard"
spoken_symbols = false
command_names = ["go"]
skills_dirs = ["/skills"]
sample_rate = 8000
server_host = "0.0.0.0"
server_port = 9999
meetings_enabled = true
vault_path = "/vault"
notes_folder = "notes"
logs_folder = "logs"
meeting_watch = false
meeting_chunk_seconds = 10
meeting_min_busy_seconds = 5
meeting_auto_start = false
work_prompts = false
fireworks_api_key = "secret"
fireworks_model = "fw"
prompts_dir = "/prompts"
orca_repo = "repo"
orca_agent = "agent"
"""
    )

    loaded = config.load_config()

    assert loaded.stt_backend == "mlx-whisper"
    assert loaded.llm_base_url == "http://ollama"
    assert loaded.llm_model == "model"
    assert loaded.fireworks_model == "fw"
    assert keyring.values[("localflow", "fireworks_api_key")] == "secret"
    assert "fireworks_api_key" not in isolated_config.read_text()
    assert legacy.exists()


def test_second_load_does_not_remigrate(isolated_config, monkeypatch, tmp_path):
    legacy = tmp_path / ".localflow.toml"
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)
    legacy.write_text('model_size = "tiny"\n')
    config.load_config()
    mtime = legacy.stat().st_mtime_ns
    monkeypatch.setattr(config, "migrate_legacy", lambda *_args: pytest.fail("remigrated"))

    assert config.load_config().model_size == "tiny"
    assert legacy.stat().st_mtime_ns == mtime


def test_migration_keeps_secret_in_process_when_keyring_unavailable(
    isolated_config, monkeypatch, tmp_path
):
    from localflow import secrets

    class RaisingKeyring:
        def set_password(self, service, name, value):
            raise RuntimeError("no backend")

    monkeypatch.setattr(secrets, "keyring", RaisingKeyring())
    monkeypatch.delenv("FIREWORKS_API_KEY", raising=False)
    monkeypatch.delenv("LOCALFLOW_FIREWORKS_API_KEY", raising=False)
    legacy = tmp_path / ".localflow.toml"
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)
    legacy.write_text('fireworks_api_key = "secret"\nmodel_size = "tiny"\n')

    loaded = config.load_config()

    assert loaded.model_size == "tiny"
    assert not isolated_config.exists()
    assert legacy.exists()
    assert secrets.get_secret("fireworks_api_key") == "secret"


def test_migration_with_existing_secret_environment_writes_v2(
    isolated_config, monkeypatch, tmp_path
):
    from localflow import secrets

    class RaisingKeyring:
        def set_password(self, service, name, value):
            raise RuntimeError("no backend")

    monkeypatch.setattr(secrets, "keyring", RaisingKeyring())
    monkeypatch.setenv("FIREWORKS_API_KEY", "existing")
    legacy = tmp_path / ".localflow.toml"
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)
    legacy.write_text('fireworks_api_key = "legacy"\n')

    config.load_config()

    assert isolated_config.exists()


def test_meetings_enabled_platform_override(isolated_config, monkeypatch):
    monkeypatch.setattr(config.sys, "platform", "darwin")
    config.save_config(Config(meetings_enabled=True))
    assert config.load_config().meetings_enabled is True
    monkeypatch.setattr(config.sys, "platform", "linux")
    assert config.load_config().meetings_enabled is False


def test_config_table_covers_every_dataclass_field():
    field_names = {field.name for field in fields(Config)}
    mapped = {
        field_name
        for section in config._SECTIONS.values()
        for field_name in section.values()
    }

    assert field_names == mapped
    assert len(mapped) == sum(len(section) for section in config._SECTIONS.values())


def test_config_to_dict_and_partial_update():
    original = Config()
    serialized = config.config_to_dict(original)

    assert serialized["stt"]["model"] == original.model_size
    assert serialized["llm"]["provider"] == original.llm_provider

    updated = config.apply_config_update(
        original,
        {"stt": {"model": "small", "language": None}, "server": {"port": 9000}},
    )
    assert updated.model_size == "small"
    assert updated.language is None
    assert updated.server_port == 9000
    assert original.server_port == 8756


@pytest.mark.parametrize(
    "update, message",
    [
        ({"stt": {"missing": "x"}}, "unknown config key 'stt.missing'"),
        ({"missing": {"key": "x"}}, "unknown config key 'missing'"),
        ({"server": {"port": "9000"}}, "invalid type for config key 'server.port'"),
        ({"paste": {"method": "invalid"}}, "invalid paste method"),
    ],
)
def test_apply_config_update_validates(update, message):
    with pytest.raises(ConfigError, match=message):
        config.apply_config_update(Config(), update)


def test_factory_builds_ollama(isolated_config):
    loaded = Config()
    provider = build_llm(loaded, timeout=7, num_ctx=123)

    assert provider.base_url == loaded.llm_base_url
    assert provider.model == loaded.llm_model
    assert provider.timeout == 7
    assert provider.num_ctx == 123


def test_factory_builds_openai_with_secret(monkeypatch, isolated_config):
    from localflow.providers import factory

    monkeypatch.setattr(factory, "get_secret", lambda name: "api-key")
    loaded = Config(
        llm_provider="openai-compatible",
        llm_base_url="https://example/v1",
        llm_model="model",
    )

    provider = build_llm(loaded, timeout=9, temperature=0.2)

    assert provider.api_key == "api-key"
    assert provider.timeout == 9
    assert provider.temperature == 0.2


def test_factory_openai_environment_secret_overrides_keyring(
    monkeypatch, isolated_config
):
    from localflow import secrets

    monkeypatch.setattr(
        secrets,
        "keyring",
        MemoryKeyring({("localflow", "llm_api_key"): "keyring"}),
    )
    monkeypatch.setenv("LOCALFLOW_LLM_API_KEY", "environment")

    provider = build_llm(Config(llm_provider="openai-compatible"))

    assert provider.api_key == "environment"


def test_factory_unknown_provider_uses_registry_error(isolated_config):
    with pytest.raises(KeyError, match="valid ids"):
        build_llm(Config(llm_provider="missing"))


def test_cli_config_commands(isolated_config):
    runner = CliRunner()

    result = runner.invoke(cli, ["config", "set", "server.port", "9001"])
    assert result.exit_code == 0, result.output
    assert runner.invoke(cli, ["config", "get", "server.port"]).output.strip() == "9001"
    assert "config_version = 2" in runner.invoke(cli, ["config", "show"]).output
    assert str(isolated_config) in runner.invoke(cli, ["config", "path"]).output


def test_cli_unknown_config_key_fails(isolated_config):
    result = CliRunner().invoke(cli, ["config", "set", "bad.key", "value"])

    assert result.exit_code != 0
    assert "valid keys" in result.output


def test_cli_secret_status(monkeypatch):
    monkeypatch.setattr(
        "localflow.cli.get_secret",
        lambda name: "configured" if name == "llm_api_key" else "",
    )

    result = CliRunner().invoke(cli, ["secret", "status"])

    assert result.exit_code == 0
    assert "llm_api_key: configured" in result.output
    assert "fireworks_api_key: not configured" in result.output
