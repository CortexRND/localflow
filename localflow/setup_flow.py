"""UI-independent onboarding choices, checks, and local audio helpers."""

import ctypes
import platform
import shutil
import sys
import threading
import time
from dataclasses import dataclass
from typing import Callable

import numpy as np
import sounddevice as sd

from localflow.audio import Recorder, list_input_devices
from localflow.config import Config, apply_config_update, load_config, save_config
from localflow.models import (
    HardwareInfo,
    STT_MODELS,
    LLM_MODELS,
    OllamaClient,
    available_stt_models,
    current_stt_id,
    detect_hardware,
    ollama_installed,
    recommend,
    stt_config,
)


@dataclass(frozen=True)
class Check:
    key: str
    label: str
    ok: bool | None
    detail: str
    fix_label: str
    fix_url: str


@dataclass(frozen=True)
class SetupChoices:
    stt_id: str
    llm_id: str | None
    input_device: str
    hotkey: str
    language: str | None
    start_on_login: bool
    llm_provider: str = "ollama"


HOTKEY_OPTIONS = [
    ("alt_l", "Hold Left Option"),
    ("alt_r", "Hold Right Option"),
    ("cmd_r", "Hold Right Command"),
    ("ctrl_r", "Hold Right Control"),
    ("f19", "Hold F19"),
    ("<cmd>+<shift>+<space>", "Press ⌘⇧Space to start/stop"),
]


def _mac_permission_checks() -> list[Check]:
    checks = []
    try:
        import AVFoundation

        from AVFoundation import AVCaptureDevice, AVMediaTypeAudio
    except ImportError:
        checks.append(Check(
            "microphone", "Microphone permission", None, "Unknown (PyObjC unavailable)",
            "Open settings", "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        ))
    else:
        status = AVCaptureDevice.authorizationStatusForMediaType_(AVMediaTypeAudio)
        authorized = getattr(AVFoundation, "AVAuthorizationStatusAuthorized", 3)
        granted = status == authorized
        pending = status == getattr(AVFoundation, "AVAuthorizationStatusNotDetermined", 0)
        checks.append(Check(
            "microphone", "Microphone permission", None if pending else granted,
            "Unknown" if pending else ("Granted" if granted else "Not granted"),
            "Open settings", "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        ))

    try:
        application_services = ctypes.CDLL(
            "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices"
        )
        accessible = bool(application_services.AXIsProcessTrusted())
        accessibility = Check(
            "accessibility", "Accessibility permission", accessible,
            "Granted" if accessible else "Not granted", "Open settings",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        )
    except (AttributeError, OSError):
        accessibility = Check(
            "accessibility", "Accessibility permission", None, "Unknown",
            "Open settings", "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        )
    checks.append(accessibility)

    try:
        iokit = ctypes.CDLL("/System/Library/Frameworks/IOKit.framework/IOKit")
        iokit.IOHIDCheckAccess.argtypes = [ctypes.c_uint32]
        iokit.IOHIDCheckAccess.restype = ctypes.c_int
        granted = iokit.IOHIDCheckAccess(1) == 0
        input_monitoring = Check(
            "input-monitoring", "Input Monitoring permission", granted,
            "Granted" if granted else "Not granted", "Open settings",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
        )
    except (AttributeError, OSError):
        input_monitoring = Check(
            "input-monitoring", "Input Monitoring permission", None, "Unknown",
            "Open settings", "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
        )
    checks.append(input_monitoring)
    return checks


def run_checks(cfg: Config) -> list[Check]:
    ffmpeg_installed = shutil.which("ffmpeg") is not None
    ollama_exists = ollama_installed()
    checks = [
        Check(
            "ffmpeg", "ffmpeg", ffmpeg_installed,
            "Installed" if ffmpeg_installed else "Not found on PATH",
            "Install ffmpeg", "brew install ffmpeg",
        ),
        Check(
            "ollama-installed", "Ollama installed", ollama_exists,
            "Installed" if ollama_exists else "Not installed",
            "Get Ollama", "https://ollama.com/download",
        ),
    ]
    if cfg.llm_provider == "ollama":
        running = OllamaClient(cfg.llm_base_url).is_running()
        running_detail = "Ready" if running else "Not running"
    else:
        running = None
        running_detail = f"Not used (configured provider: {cfg.llm_provider})"
    checks.append(Check(
        "ollama-running", "Ollama running", running,
        running_detail,
        "Start Ollama", "https://ollama.com/download",
    ))
    devices = list_input_devices()
    checks.append(Check(
        "microphone-device", "Input device", bool(devices),
        f"{len(devices)} input device(s) found" if devices else "No input devices found",
        "Check microphone", "",
    ))
    if sys.platform == "darwin":
        checks.extend(_mac_permission_checks())
    return checks


def initial_choices(cfg: Config, hw: HardwareInfo) -> SetupChoices:
    recommended_stt, recommended_llm = recommend(hw)
    if cfg.setup_version > 0:
        stt_id = current_stt_id(cfg)
        if stt_id not in {model.id for model in available_stt_models(hw)}:
            stt_id = recommended_stt
        valid_llm = {model.id for model in LLM_MODELS}
        llm_id = (
            cfg.llm_model
            if cfg.llm_provider == "ollama"
            and cfg.cleanup_enabled
            and cfg.llm_model in valid_llm
            else None
        )
        start_on_login = False
        if hw.system == "Darwin":
            try:
                from localflow.launchagent import status

                start_on_login = status().startswith(("running", "loaded"))
            except (OSError, RuntimeError):
                pass
        return SetupChoices(
            stt_id=stt_id,
            llm_id=llm_id,
            input_device=cfg.input_device,
            hotkey=cfg.hotkey,
            language=cfg.language,
            start_on_login=start_on_login,
            llm_provider=cfg.llm_provider,
        )
    return SetupChoices(
        stt_id=recommended_stt,
        llm_id=recommended_llm if cfg.llm_provider == "ollama" else None,
        input_device=cfg.input_device,
        hotkey=cfg.hotkey,
        language=cfg.language,
        start_on_login=False,
        llm_provider=cfg.llm_provider,
    )


def apply(choices: SetupChoices):
    current = load_config()
    updates = stt_config_from_id(choices.stt_id)
    updates.update({
        "dictation": {"input_device": choices.input_device},
        "hotkey": {"key": choices.hotkey},
        "setup": {"version": 1},
    })
    updates["stt"]["language"] = choices.language
    if choices.llm_provider == "ollama":
        updates["llm"] = {
            "model": choices.llm_id or current.llm_model,
            "cleanup_enabled": choices.llm_id is not None,
        }
    path = save_config(apply_config_update(current, updates))
    if choices.start_on_login and platform.system() == "Darwin":
        from localflow.launchagent import install

        install()
    return path


def stt_config_from_id(model_id: str) -> dict:
    model = next((entry for entry in STT_MODELS if entry.id == model_id), None)
    if model is None:
        raise ValueError(f"unknown speech model: {model_id}")
    return stt_config(model)


class MicLevelMonitor:
    def __init__(self, device: str | None):
        self.device = device or None
        self._stream = None
        self._level = 0.0
        self._lock = threading.Lock()

    def start(self) -> None:
        def callback(indata, _frames, _time_info, _status):
            level = float(np.sqrt(np.mean(np.square(indata))))
            with self._lock:
                self._level = min(1.0, level)

        self._stream = sd.InputStream(
            samplerate=16000,
            channels=1,
            dtype="float32",
            device=self.device,
            callback=callback,
        )
        self._stream.start()

    def stop(self) -> None:
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None
        with self._lock:
            self._level = 0.0

    def level(self) -> float:
        with self._lock:
            return self._level


def try_dictation(
    seconds: float,
    device: str | None,
    transcriber,
) -> str:
    recorder = Recorder(sample_rate=16000, device=device or None)
    recorder.start()
    try:
        time.sleep(seconds)
    finally:
        audio = recorder.stop()
    return transcriber.transcribe(audio)
