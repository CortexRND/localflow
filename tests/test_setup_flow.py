from pathlib import Path

from localflow import setup_flow
from localflow.config import Config
from localflow.models import HardwareInfo
from localflow.setup_flow import SetupChoices


def test_initial_choices_use_hardware_recommendations_before_setup():
    choices = setup_flow.initial_choices(
        Config(),
        HardwareInfo("Linux", "x86_64", 4, False, 50),
    )

    assert choices.stt_id == "fw-tiny"
    assert choices.llm_id is None
    assert choices.hotkey == "alt_l"


def test_initial_choices_reuse_existing_config_after_setup(monkeypatch):
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Linux")

    choices = setup_flow.initial_choices(
        Config(
            setup_version=1,
            model_size="small",
            stt_backend="faster-whisper",
            llm_model="qwen2.5:3b",
            input_device="Studio Mic",
            hotkey="f19",
            language="fr",
        ),
        HardwareInfo("Linux", "x86_64", 16, False, 50),
    )

    assert choices == SetupChoices("fw-small", "qwen2.5:3b", "Studio Mic", "f19", "fr", False)


def test_initial_choices_replaces_models_unavailable_on_current_hardware():
    choices = setup_flow.initial_choices(
        Config(setup_version=1, stt_backend="mlx", model_size="small"),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )

    assert choices.stt_id == "fw-base"


def test_initial_choices_skips_ollama_recommendation_for_other_provider():
    choices = setup_flow.initial_choices(
        Config(llm_provider="openai-compatible"),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )

    assert choices.llm_id is None
    assert choices.llm_provider == "openai-compatible"


def test_initial_choices_skips_ollama_default_when_not_installed(monkeypatch):
    monkeypatch.setattr(setup_flow, "ollama_installed", lambda: False)

    choices = setup_flow.initial_choices(
        Config(),
        HardwareInfo("Linux", "x86_64", 16, False, 50),
    )

    assert choices.llm_id is None


def test_initial_choices_uses_ollama_recommendation_when_installed(monkeypatch):
    monkeypatch.setattr(setup_flow, "ollama_installed", lambda: True)

    choices = setup_flow.initial_choices(
        Config(),
        HardwareInfo("Linux", "x86_64", 16, False, 50),
    )

    assert choices.llm_id == "llama3.2:3b"


def test_run_checks_reports_all_cross_platform_requirements(monkeypatch):
    monkeypatch.setattr(setup_flow.shutil, "which", lambda command: "/usr/bin/" + command)
    monkeypatch.setattr(setup_flow, "ollama_installed", lambda: True)
    monkeypatch.setattr(setup_flow.OllamaClient, "is_running", lambda _self: False)
    monkeypatch.setattr(setup_flow, "list_input_devices", lambda: ["USB mic"])
    monkeypatch.setattr(setup_flow.sys, "platform", "linux")

    checks = setup_flow.run_checks(Config())

    assert [check.key for check in checks] == [
        "ffmpeg", "ollama-installed", "ollama-running", "microphone-device",
    ]
    assert [check.ok for check in checks] == [True, True, False, True]
    assert checks[0].fix_url == "https://formulae.brew.sh/formula/ffmpeg"


def test_run_checks_uses_provider_info_without_ollama_checks(monkeypatch):
    monkeypatch.setattr(setup_flow.shutil, "which", lambda _command: None)
    monkeypatch.setattr(
        setup_flow, "ollama_installed",
        lambda: (_ for _ in ()).throw(AssertionError("must not probe Ollama")),
    )
    monkeypatch.setattr(
        setup_flow.OllamaClient,
        "is_running",
        lambda _self: (_ for _ in ()).throw(AssertionError("must not probe Ollama")),
    )
    monkeypatch.setattr(setup_flow, "list_input_devices", list)

    checks = setup_flow.run_checks(
        Config(llm_provider="openai-compatible", llm_model="gpt-local")
    )

    assert [check.key for check in checks] == [
        "ffmpeg", "llm-provider", "microphone-device",
    ]
    assert checks[0].detail == "Not found — run: brew install ffmpeg"
    assert checks[1] == setup_flow.Check(
        "llm-provider", "Cleanup model", None,
        "Using openai-compatible (gpt-local)", "Open settings", "",
    )


def test_apply_writes_configuration_and_can_install_launchagent(tmp_path, monkeypatch):
    written = []
    monkeypatch.setattr(setup_flow, "load_config", Config)
    monkeypatch.setattr(
        setup_flow,
        "save_config",
        lambda config: written.append(config) or tmp_path / "config.toml",
    )
    installed = []
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Darwin")
    monkeypatch.setattr("localflow.launchagent.install", lambda: installed.append(True))
    choices = SetupChoices("fw-small", "qwen2.5:3b", "USB mic", "f19", "en", True)

    result = setup_flow.apply(choices)

    assert result == tmp_path / "config.toml"
    saved = written[0]
    assert saved.stt_backend == "faster-whisper"
    assert saved.model_size == "small"
    assert saved.llm_model == "qwen2.5:3b"
    assert saved.cleanup_enabled is True
    assert saved.input_device == "USB mic"
    assert saved.hotkey == "f19"
    assert saved.language == "en"
    assert saved.setup_version == 1
    assert installed == [True]


def test_apply_uninstalls_launchagent_when_startup_is_disabled(monkeypatch, tmp_path):
    monkeypatch.setattr(setup_flow, "load_config", Config)
    monkeypatch.setattr(
        setup_flow, "save_config", lambda _config: tmp_path / "config.toml"
    )
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(
        "localflow.launchagent.status", lambda: "loaded, not running"
    )
    uninstalled = []
    monkeypatch.setattr(
        "localflow.launchagent.uninstall", lambda: uninstalled.append(True)
    )

    setup_flow.apply(SetupChoices("fw-base", None, "", "alt_l", None, False))

    assert uninstalled == [True]


def test_apply_does_not_uninstall_missing_launchagent(monkeypatch, tmp_path):
    monkeypatch.setattr(setup_flow, "load_config", Config)
    monkeypatch.setattr(
        setup_flow, "save_config", lambda _config: tmp_path / "config.toml"
    )
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Darwin")
    monkeypatch.setattr("localflow.launchagent.status", lambda: "not installed")
    uninstalled = []
    monkeypatch.setattr(
        "localflow.launchagent.uninstall", lambda: uninstalled.append(True)
    )

    setup_flow.apply(SetupChoices("fw-base", None, "", "alt_l", None, False))

    assert uninstalled == []


def test_apply_disables_cleanup_when_no_model_is_selected(monkeypatch):
    written = []
    monkeypatch.setattr(setup_flow, "load_config", Config)
    monkeypatch.setattr(
        setup_flow, "save_config", lambda config: written.append(config) or Path("config")
    )
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Linux")

    setup_flow.apply(SetupChoices("fw-base", None, "", "alt_l", None, False))

    assert written[0].cleanup_enabled is False
    assert written[0].llm_model == Config().llm_model
    assert written[0].language is None


def test_apply_keeps_non_ollama_provider_configuration(monkeypatch):
    current = Config(
        llm_provider="openai-compatible",
        llm_base_url="https://example.test/v1",
        llm_model="configured-model",
    )
    written = []
    monkeypatch.setattr(setup_flow, "load_config", lambda: current)
    monkeypatch.setattr(
        setup_flow, "save_config", lambda config: written.append(config) or Path("config")
    )
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Linux")

    setup_flow.apply(
        SetupChoices(
            "fw-base", None, "", "alt_l", None, False,
            llm_provider="openai-compatible",
        )
    )

    assert written[0].llm_provider == "openai-compatible"
    assert written[0].llm_base_url == "https://example.test/v1"
    assert written[0].llm_model == "configured-model"
    assert written[0].cleanup_enabled is True


def test_try_dictation_records_and_transcribes(monkeypatch):
    calls = []

    class FakeRecorder:
        def __init__(self, sample_rate, device):
            calls.append(("init", sample_rate, device))

        def start(self):
            calls.append(("start",))

        def stop(self):
            calls.append(("stop",))
            return "audio"

    monkeypatch.setattr(setup_flow, "Recorder", FakeRecorder)

    class Transcriber:
        def transcribe(self, audio):
            calls.append(("transcribe", audio))
            return "hello"

    assert setup_flow.try_dictation(0, "USB mic", Transcriber()) == "hello"
    assert calls == [
        ("init", 16000, "USB mic"), ("start",), ("stop",), ("transcribe", "audio"),
    ]
