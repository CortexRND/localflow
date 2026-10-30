from pathlib import Path

import pytest
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

    medium_result = CliRunner().invoke(
        cli_module.cli,
        [
            "setup", "--cli", "--yes", "--stt", "fw-tiny", "--llm", "none",
            "--cleanup-level", "medium",
        ],
    )

    assert medium_result.exit_code == 0, medium_result.output
    assert choices[-1].cleanup_level == "medium"


def test_interactive_setup_uses_configured_prompt_defaults(monkeypatch):
    config = Config(
        setup_version=1,
        stt_backend="faster-whisper",
        model_size="small",
        llm_model="qwen2.5:3b",
        cleanup_enabled=True,
        input_device="Studio Mic",
        hotkey="f19",
        language="fr",
    )
    choices = []
    prompts = {}
    confirmations = {}

    class OfflineOllama:
        def __init__(self, _base_url):
            pass

        def is_running(self):
            return False

    def prompt(text, default=None, **_kwargs):
        prompts[text] = default
        return default

    def confirm(text, default=False, **_kwargs):
        confirmations[text] = default
        return default

    monkeypatch.setattr(cli_module, "load_config", lambda: config)
    monkeypatch.setattr(
        "localflow.models.detect_hardware",
        lambda: HardwareInfo("Darwin", "arm64", 16, True, 50),
    )
    monkeypatch.setattr("localflow.models.is_stt_downloaded", lambda _model: True)
    monkeypatch.setattr("localflow.models.OllamaClient", OfflineOllama)
    monkeypatch.setattr("localflow.audio.list_input_devices", lambda: ["Studio Mic"])
    monkeypatch.setattr("localflow.launchagent.status", lambda: "running (LocalFlow)")
    monkeypatch.setattr("localflow.setup_flow.run_checks", lambda _cfg: [])
    monkeypatch.setattr(
        "localflow.setup_flow.apply", lambda selected: choices.append(selected)
    )
    monkeypatch.setattr(cli_module.click, "prompt", prompt)
    monkeypatch.setattr(cli_module.click, "confirm", confirm)

    default_result = CliRunner().invoke(cli_module.cli, ["setup", "--cli"])
    assert default_result.exit_code == 0, default_result.output
    assert prompts == {
        "Speech model": "fw-small",
        "Cleanup model": "qwen2.5:3b",
        "Cleanup level": "light",
        "Microphone": "Studio Mic",
        "Hotkey": "f19",
        "Language (blank = autodetect)": "fr",
    }
    assert confirmations["Start LocalFlow on login?"] is True
    assert choices[0].start_on_login is True

    prompts.clear()
    explicit_result = CliRunner().invoke(
        cli_module.cli,
        ["setup", "--cli", "--stt", "fw-tiny", "--llm", "none"],
    )
    assert explicit_result.exit_code == 0, explicit_result.output
    assert prompts["Speech model"] == "fw-tiny"
    assert prompts["Cleanup model"] == "none"
    assert prompts["Cleanup level"] == "light"


def test_setup_yes_rejects_unknown_models():
    result = CliRunner().invoke(
        cli_module.cli,
        ["setup", "--cli", "--yes", "--stt", "not-a-model"],
    )

    assert result.exit_code == 1
    assert "unknown speech model" in result.output


def test_setup_yes_preserves_non_ollama_provider_without_pulling(monkeypatch, tmp_path):
    choices = []
    monkeypatch.setattr(
        cli_module,
        "load_config",
        lambda: Config(
            llm_provider="openai-compatible",
            llm_model="configured-model",
        ),
    )
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
        ["setup", "--cli", "--yes", "--stt", "fw-tiny"],
    )

    assert result.exit_code == 0, result.output
    assert choices[0].llm_provider == "openai-compatible"
    assert choices[0].llm_id is None


def test_models_use_no_download_selects_local_speech_model(monkeypatch):
    updates = []
    monkeypatch.setattr(cli_module, "load_config", Config)
    monkeypatch.setattr(
        cli_module,
        "save_config",
        lambda config: updates.append(config) or Path("config.toml"),
    )

    result = CliRunner().invoke(
        cli_module.cli, ["models", "use", "fw-small", "--no-download"]
    )

    assert result.exit_code == 0, result.output
    assert updates[0].stt_backend == "faster-whisper"
    assert updates[0].model_size == "small"


def test_stt_download_uses_catalog(monkeypatch):
    downloaded = []
    monkeypatch.setattr(cli_module, "_download_stt_cli", lambda model: downloaded.append(model.id))

    result = CliRunner().invoke(cli_module.cli, ["stt", "download", "fw-tiny"])

    assert result.exit_code == 0, result.output
    assert downloaded == ["fw-tiny"]


def test_stt_download_uses_friendly_click_error(monkeypatch):
    def fail(*_args, **_kwargs):
        raise RuntimeError("connection timed out at https://example.test/model")

    monkeypatch.setattr("localflow.models.download_stt", fail)
    result = CliRunner().invoke(cli_module.cli, ["stt", "download", "fw-tiny"])

    assert result.exit_code == 1
    assert "Network problem while downloading" in result.output


@pytest.mark.parametrize(
    ("arguments", "extra"),
    [
        (["stt", "download", "mlx-small"], "mlx"),
        (["models", "use", "parakeet-v2", "--no-download"], "parakeet"),
    ],
)
def test_stt_commands_require_optional_runtime(arguments, extra, monkeypatch):
    monkeypatch.setattr("localflow.models.runtime_available", lambda _backend: False)

    result = CliRunner().invoke(cli_module.cli, arguments)

    assert result.exit_code == 1
    assert f"install the runtime: pip install -e '.[{extra}]'" in result.output


def test_llm_pull_does_not_attempt_download_for_non_ollama_provider(monkeypatch):
    monkeypatch.setattr(
        cli_module,
        "load_config",
        lambda: Config(llm_provider="openai-compatible"),
    )

    result = CliRunner().invoke(
        cli_module.cli, ["llm", "pull", "llama3.2:1b"]
    )

    assert result.exit_code == 1
    assert "only available for the Ollama provider" in result.output


def test_llm_pull_uses_configured_ollama_url(monkeypatch):
    observed = []

    class FakeClient:
        def __init__(self, base_url):
            observed.append(("url", base_url))

        def is_running(self):
            return True

    monkeypatch.setattr(
        cli_module,
        "load_config",
        lambda: Config(llm_base_url="http://ollama.local:11434"),
    )
    monkeypatch.setattr("localflow.models.OllamaClient", FakeClient)
    monkeypatch.setattr(
        cli_module, "_pull_llm_cli", lambda tag, _client: observed.append(("tag", tag))
    )

    result = CliRunner().invoke(
        cli_module.cli, ["llm", "pull", "llama3.2:1b"]
    )

    assert result.exit_code == 0, result.output
    assert observed == [
        ("url", "http://ollama.local:11434"),
        ("tag", "llama3.2:1b"),
    ]


def test_models_use_no_download_selects_ollama_model(monkeypatch):
    updates = []
    monkeypatch.setattr(cli_module, "load_config", Config)
    monkeypatch.setattr(
        cli_module,
        "save_config",
        lambda config: updates.append(config) or Path("config.toml"),
    )

    result = CliRunner().invoke(
        cli_module.cli, ["models", "use", "llama3.2:1b", "--no-download"]
    )

    assert result.exit_code == 0, result.output
    assert updates[0].llm_model == "llama3.2:1b"
    assert updates[0].cleanup_enabled is True


def test_models_list_does_not_contact_ollama_for_other_provider(monkeypatch):
    monkeypatch.setattr(
        cli_module,
        "load_config",
        lambda: Config(llm_provider="openai-compatible"),
    )

    def unexpected_client(*_args, **_kwargs):
        raise AssertionError("Ollama must not be contacted for another provider")

    monkeypatch.setattr("localflow.models.OllamaClient", unexpected_client)
    result = CliRunner().invoke(cli_module.cli, ["models", "list", "--kind", "llm"])

    assert result.exit_code == 0, result.output
    assert "managed by Settings" in result.output
