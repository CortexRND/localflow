import json

from click.testing import CliRunner

from localflow import cli as cli_module
from localflow import config
from localflow.config import Config


def test_status_json_reports_unloaded_stt(monkeypatch, tmp_path):
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setattr(config, "legacy_config_path", lambda: tmp_path / "legacy.toml")
    monkeypatch.setattr(cli_module, "current", lambda: type(
        "Platform",
        (),
        {"name": "linux", "autostart_status": lambda self: "not installed"},
    )())

    result = CliRunner().invoke(cli_module.cli, ["status", "--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["stt"]["loaded"] is False


def test_stt_and_llm_use_save_nested_config(monkeypatch, tmp_path):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setattr(config, "legacy_config_path", lambda: tmp_path / "legacy.toml")
    config.save_config(Config())
    runner = CliRunner()

    stt_result = runner.invoke(
        cli_module.cli,
        ["stt", "use", "faster-whisper", "small", "--device", "cuda"],
    )
    llm_result = runner.invoke(
        cli_module.cli,
        ["llm", "use", "openai-compatible", "--model", "remote"],
    )

    assert stt_result.exit_code == 0, stt_result.output
    assert llm_result.exit_code == 0, llm_result.output
    loaded = config.load_config()
    assert loaded.stt_backend == "faster-whisper"
    assert loaded.model_size == "small"
    assert loaded.stt_device == "cuda"
    assert loaded.llm_provider == "openai-compatible"
    assert loaded.llm_model == "remote"
