from pathlib import Path

from click.testing import CliRunner

from localflow import cli as cli_module
from localflow.config import Config
from localflow.models import HardwareInfo
from localflow.setup_flow import SetupChoices


def test_setup_yes_uses_recommended_choices_without_prompts(monkeypatch, tmp_path):
    choices = []
    monkeypatch.setattr(cli_module, "load_config", Config)
    monkeypatch.setattr(
        "localflow.models.detect_hardware",
        lambda: HardwareInfo("Linux", "x86_64", 4, False, 50),
    )
    monkeypatch.setattr("localflow.models.is_stt_downloaded", lambda _model: True)
    monkeypatch.setattr(
        "localflow.setup_flow.apply",
        lambda selected: choices.append(selected) or tmp_path / "config.toml",
    )

    result = CliRunner().invoke(
        cli_module.cli,
        ["setup", "--cli", "--yes", "--stt", "fw-tiny", "--llm", "none"],
    )

    assert result.exit_code == 0, result.output
    assert "setup saved" in result.output
    assert choices == [SetupChoices("fw-tiny", None, "", "alt_l", None, False)]


def test_setup_yes_rejects_unknown_models():
    result = CliRunner().invoke(
        cli_module.cli,
        ["setup", "--cli", "--yes", "--stt", "not-a-model"],
    )

    assert result.exit_code == 1
    assert "unknown speech model" in result.output


def test_models_use_no_download_selects_local_speech_model(monkeypatch):
    updates = []
    monkeypatch.setattr(cli_module, "load_config", Config)
    monkeypatch.setattr(
        "localflow.config.save_config",
        lambda values: updates.append(values) or Path("config.toml"),
    )

    result = CliRunner().invoke(
        cli_module.cli, ["models", "use", "fw-small", "--no-download"]
    )

    assert result.exit_code == 0, result.output
    assert updates == [{"stt_backend": "faster-whisper", "model_size": "small"}]
