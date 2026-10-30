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
            ollama_model="qwen2.5:3b",
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


def test_apply_writes_configuration_and_can_install_launchagent(tmp_path, monkeypatch):
    written = {}
    monkeypatch.setattr(setup_flow, "save_config", lambda updates: written.update(updates) or tmp_path / "config.toml")
    installed = []
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Darwin")
    monkeypatch.setattr("localflow.launchagent.install", lambda: installed.append(True))
    choices = SetupChoices("fw-small", "qwen2.5:3b", "USB mic", "f19", "en", True)

    result = setup_flow.apply(choices)

    assert result == tmp_path / "config.toml"
    assert written == {
        "stt_backend": "faster-whisper",
        "model_size": "small",
        "ollama_model": "qwen2.5:3b",
        "cleanup_enabled": True,
        "input_device": "USB mic",
        "hotkey": "f19",
        "language": "en",
        "setup_version": 1,
    }
    assert installed == [True]


def test_apply_disables_cleanup_when_no_model_is_selected(monkeypatch):
    written = {}
    monkeypatch.setattr(setup_flow, "save_config", lambda updates: written.update(updates) or Path("config"))
    monkeypatch.setattr(setup_flow.platform, "system", lambda: "Linux")

    setup_flow.apply(SetupChoices("fw-base", None, "", "alt_l", None, False))

    assert written["cleanup_enabled"] is False
    assert written["ollama_model"] == Config().ollama_model
    assert "language" not in written


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
