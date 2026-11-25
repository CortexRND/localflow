import logging
import tomllib
from dataclasses import fields, replace

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
        cleanup_level="medium",
        styles_enabled=False,
        style_work="excited",
        style_email="very_casual",
        style_personal="formal",
        style_other="casual",
        app_category_overrides=["com.example.editor=work"],
        cleanup_timeout=17,
        cleanup_num_ctx=4096,
        hotkey="<cmd>+<shift>+<space>",
        sounds_enabled=False,
        paste_method="clipboard",
        spoken_symbols=False,
        spoken_punctuation=False,
        silence_threshold_dbfs=-45.5,
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
        input_device="Studio Mic",
        setup_version=1,
        paste_last_hotkey="<ctrl>+<cmd>+v",
        command_hotkey="alt_r",
        polish_hotkey="<ctrl>+<alt>+1",
        prompt_hotkey="<ctrl>+<alt>+2",
        pause_media_while_dictating=True,
        max_recording_seconds=600,
        speaker_diarization=True,
        diarization_backend="mock",
        diarize_mic=True,
        diarization_max_speakers=4,
        speaker_recognition=True,
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


def test_invalid_cleanup_enums_are_logged_and_defaulted(isolated_config, caplog):
    isolated_config.write_text(
        '[cleanup]\nlevel = "aggressive"\nstyle_work = "wild"\n'
        'style_personal = 12\n',
        encoding="utf-8",
    )

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert loaded.cleanup_level == "light"
    assert loaded.style_work == "formal"
    assert loaded.style_personal == "casual"
    assert caplog.text.count("invalid config value for cleanup.") == 3


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


def test_save_keeps_file_meetings_value_forced_off_on_load(isolated_config, monkeypatch):
    monkeypatch.setattr(config.sys, "platform", "darwin")
    config.save_config(Config(meetings_enabled=True))
    monkeypatch.setattr(config.sys, "platform", "linux")
    loaded = config.load_config()
    assert loaded.meetings_enabled is False

    updated = config.apply_config_update(loaded, {"cleanup": {"style_work": "casual"}})
    config.save_config(replace(updated, sounds_enabled=False))

    written = tomllib.loads(isolated_config.read_text())
    assert written["cleanup"]["style_work"] == "casual"
    assert written["features"]["meetings"] is True
    assert config.load_config() == replace(updated, sounds_enabled=False)


def test_save_writes_explicit_meetings_edit_over_file_value(isolated_config, monkeypatch):
    monkeypatch.setattr(config.sys, "platform", "darwin")
    config.save_config(Config(meetings_enabled=True))
    monkeypatch.setattr(config.sys, "platform", "linux")

    config.save_config(
        config.apply_config_update(config.load_config(), {"features": {"meetings": False}})
    )
    assert tomllib.loads(isolated_config.read_text())["features"]["meetings"] is False

    config.save_config(
        config.apply_config_update(config.load_config(), {"features": {"meetings": True}})
    )
    assert tomllib.loads(isolated_config.read_text())["features"]["meetings"] is True


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


def test_fast_model_config_is_optional_and_round_trips(isolated_config):
    original = Config()
    assert config.config_to_dict(original)["llm"]["fast_model"] == ""
    updated = config.apply_config_update(original, {"llm": {"fast_model": "fast-model"}})
    config.save_config(updated)
    assert config.load_config().llm_fast_model == "fast-model"
    assert config.load_config().llm_model == original.llm_model


def test_config_update_drops_old_fast_model_on_endpoint_change():
    original = Config(llm_fast_model="old-fast")
    for llm, expected in (
        ({"model": "new-primary", "base_url": "http://localhost:11434/"}, "old-fast"),
        ({"provider": "openai-compatible"}, ""),
        ({"base_url": "http://localhost:11435"}, ""),
        ({"base_url": "http://localhost:11435", "fast_model": "new-fast"}, "new-fast"),
    ):
        assert config.apply_config_update(original, {"llm": llm}).llm_fast_model == expected


@pytest.mark.parametrize(
    "update, message",
    [
        ({"stt": {"missing": "x"}}, "unknown config key 'stt.missing'"),
        ({"missing": {"key": "x"}}, "unknown config key 'missing'"),
        ({"server": {"port": "9000"}}, "invalid type for config key 'server.port'"),
        ({"paste": {"method": "invalid"}}, "invalid paste method"),
        ({"cleanup": {"level": "aggressive"}}, "invalid value.*cleanup.level"),
        ({"cleanup": {"level": []}}, "invalid type for config key 'cleanup.level'"),
        ({"cleanup": {"style_work": "wild"}}, "invalid value.*cleanup.style_work"),
        ({"meetings": {"max_speakers": -1}}, "invalid value.*meetings.max_speakers"),
        ({"meetings": {"max_speakers": 11}}, "invalid value.*meetings.max_speakers"),
        (
            {"meetings": {"max_speakers": True}},
            "invalid type for config key 'meetings.max_speakers'",
        ),
        (
            {"meetings": {"max_speakers": 1.5}},
            "invalid type for config key 'meetings.max_speakers'",
        ),
        (
            {"meetings": {"speaker_diarization": 1}},
            "invalid type for config key 'meetings.speaker_diarization'",
        ),
        (
            {"meetings": {"diarization_backend": "pyannote"}},
            "invalid value.*meetings.diarization_backend.*choose from: mock, sherpa-onnx",
        ),
        (
            {"meetings": {"diarization_backend": 1}},
            "invalid type for config key 'meetings.diarization_backend'",
        ),
    ],
)
def test_apply_config_update_validates(update, message):
    with pytest.raises(ConfigError, match=message):
        config.apply_config_update(Config(), update)


def test_hotkey_and_dictation_fields_validate_and_round_trip(isolated_config):
    expected = Config(
        paste_last_hotkey="<ctrl>+<cmd>+v",
        command_hotkey="",
        polish_hotkey="<ctrl>+<alt>+1",
        prompt_hotkey="<ctrl>+<alt>+2",
        pause_media_while_dictating=True,
        max_recording_seconds=0,
    )

    config.save_config(expected)

    assert config.load_config() == expected
    assert config.apply_config_update(
        Config(),
        {
            "hotkey": {"command": ""},
            "dictation": {"pause_media": True, "max_recording_seconds": 0},
        },
    ).pause_media_while_dictating is True


def test_history_config_defaults_round_trip_and_validation(isolated_config):
    assert Config().history_enabled is True
    assert Config().keep_audio_clips == 5

    expected = Config(history_enabled=False, keep_audio_clips=0)
    config.save_config(expected)

    assert config.load_config() == expected
    updated = config.apply_config_update(
        Config(), {"history": {"enabled": False, "keep_audio_clips": 3}}
    )
    assert updated.history_enabled is False
    assert updated.keep_audio_clips == 3


@pytest.mark.parametrize(
    "contents",
    [
        '[hotkey]\ncommand = 7\n',
        '[dictation]\npause_media = "yes"\n',
        "[dictation]\nmax_recording_seconds = -1\n",
        '[history]\nenabled = "yes"\n',
        "[history]\nkeep_audio_clips = -1\n",
    ],
)
def test_invalid_new_config_fields_default_when_loaded(isolated_config, caplog, contents):
    isolated_config.write_text(contents, encoding="utf-8")

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert "invalid config value" in caplog.text
    assert loaded.command_hotkey == Config().command_hotkey
    assert loaded.pause_media_while_dictating is False
    assert loaded.max_recording_seconds == 1200
    assert loaded.history_enabled is True
    assert loaded.keep_audio_clips == 5


def test_apply_config_update_rejects_negative_max_recording_duration():
    with pytest.raises(ConfigError, match="invalid value.*max_recording_seconds"):
        config.apply_config_update(
            Config(), {"dictation": {"max_recording_seconds": -1}}
        )


def test_apply_config_update_rejects_negative_history_clip_retention():
    with pytest.raises(ConfigError, match="invalid value.*keep_audio_clips"):
        config.apply_config_update(
            Config(), {"history": {"keep_audio_clips": -1}}
        )


DIARIZATION_FIELDS = (
    "speaker_diarization",
    "diarization_backend",
    "diarize_mic",
    "diarization_max_speakers",
    "speaker_recognition",
)


def _assert_diarization_defaults(loaded):
    assert loaded.speaker_diarization is False
    assert loaded.diarization_backend == "sherpa-onnx"
    assert loaded.diarize_mic is False
    assert loaded.diarization_max_speakers == 0
    assert loaded.speaker_recognition is False


def test_diarization_config_defaults_round_trip_and_update(isolated_config):
    _assert_diarization_defaults(Config())

    expected = Config(
        speaker_diarization=True,
        diarization_backend="mock",
        diarize_mic=True,
        diarization_max_speakers=10,
        speaker_recognition=True,
    )
    config.save_config(expected)
    assert config.load_config() == expected

    updated = config.apply_config_update(
        Config(),
        {
            "meetings": {
                "speaker_diarization": True,
                "diarization_backend": "mock",
                "diarize_mic": True,
                "max_speakers": 10,
                "speaker_recognition": True,
            }
        },
    )
    assert updated == expected
    zero = config.apply_config_update(expected, {"meetings": {"max_speakers": 0}})
    assert zero.diarization_max_speakers == 0


@pytest.mark.parametrize(
    "contents",
    [
        "[meetings]\nspeaker_diarization = 1\n",
        '[meetings]\ndiarize_mic = "yes"\n',
        "[meetings]\nspeaker_recognition = 0\n",
        "[meetings]\nmax_speakers = -1\n",
        "[meetings]\nmax_speakers = 11\n",
        "[meetings]\nmax_speakers = true\n",
        "[meetings]\nmax_speakers = 1.5\n",
        '[meetings]\nmax_speakers = "3"\n',
        '[meetings]\ndiarization_backend = "pyannote"\n',
        "[meetings]\ndiarization_backend = 1\n",
    ],
)
def test_invalid_diarization_values_default_when_loaded(isolated_config, caplog, contents):
    isolated_config.write_text(contents, encoding="utf-8")

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert "invalid config value for meetings." in caplog.text
    assert loaded == Config()


def test_invalid_diarization_backend_is_logged_and_defaulted(isolated_config, caplog):
    isolated_config.write_text(
        '[meetings]\ndiarization_backend = "pyannote"\nspeaker_diarization = true\n',
        encoding="utf-8",
    )

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert (
        "invalid config value for meetings.diarization_backend: 'pyannote'; using default"
        in caplog.text
    )
    assert loaded.diarization_backend == Config().diarization_backend
    assert loaded.speaker_diarization is True


def test_config_without_diarization_keys_loads_without_warnings(isolated_config, caplog):
    isolated_config.write_text(
        'config_version = 2\n\n[meetings]\ncapture_system_audio = true\ntemplates_dir = "/t"\n',
        encoding="utf-8",
    )

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert caplog.records == []
    _assert_diarization_defaults(loaded)


def test_v1_migration_gets_diarization_defaults(
    isolated_config, monkeypatch, tmp_path, caplog
):
    legacy = tmp_path / "legacy.toml"
    legacy.write_text("server_port = 9999\nmeeting_chunk_seconds = 10\n", encoding="utf-8")
    monkeypatch.setattr(config, "legacy_config_path", lambda: legacy)

    with caplog.at_level(logging.WARNING):
        loaded = config.load_config()

    assert caplog.records == []
    _assert_diarization_defaults(loaded)
    written = tomllib.loads(isolated_config.read_text(encoding="utf-8"))
    assert written["config_version"] == 2
    meetings = written["meetings"]
    assert meetings["speaker_diarization"] is False
    assert meetings["diarization_backend"] == "sherpa-onnx"
    assert meetings["diarize_mic"] is False
    assert meetings["max_speakers"] == 0
    assert meetings["speaker_recognition"] is False


def test_v1_migration_maps_diarization_fields():
    from localflow import config_migrate

    assert config_migrate.migrate_v1(
        {
            "speaker_diarization": True,
            "diarization_backend": "mock",
            "diarize_mic": True,
            "diarization_max_speakers": 4,
            "speaker_recognition": True,
        }
    ) == (
        {
            "config_version": 2,
            "meetings": {
                "speaker_diarization": True,
                "diarization_backend": "mock",
                "diarize_mic": True,
                "max_speakers": 4,
                "speaker_recognition": True,
            },
        },
        None,
    )


def test_cli_config_get_set_diarization_keys(isolated_config):
    runner = CliRunner()

    def get(key):
        result = runner.invoke(cli, ["config", "get", key])
        assert result.exit_code == 0, result.output
        return result.output.strip()

    assert get("meetings.speaker_diarization") == "false"
    assert get("meetings.diarization_backend") == "sherpa-onnx"
    assert get("meetings.max_speakers") == "0"

    result = runner.invoke(cli, ["config", "set", "meetings.max_speakers", "10"])
    assert result.exit_code == 0, result.output
    assert get("meetings.max_speakers") == "10"
    before = isolated_config.read_bytes()

    for key, value in (
        ("meetings.max_speakers", "11"),
        ("meetings.diarization_backend", "pyannote"),
    ):
        result = runner.invoke(cli, ["config", "set", key, value])
        assert result.exit_code != 0, (key, value, result.output)
        assert isolated_config.read_bytes() == before


def test_diarization_keys_are_not_live_reloaded():
    from localflow import live_config

    for name in DIARIZATION_FIELDS:
        assert name not in live_config.LIVE_FIELDS


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
