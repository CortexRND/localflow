"""Local speech and cleanup model catalogs, downloads, and hardware guidance."""

import ctypes
import importlib.util
import json
import os
import platform
import shutil
import subprocess
import sys
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import requests
from faster_whisper.utils import _MODELS

os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "15")

from huggingface_hub import HfApi, hf_hub_download, snapshot_download

from localflow.config import Config
from localflow.stt import PARAKEET_REPO, _MLX_REPOS


@dataclass(frozen=True)
class SttModel:
    id: str
    label: str
    backend: str
    model_size: str
    repo: str
    size_mb: int
    english_only: bool
    speed: int
    accuracy: int
    blurb: str
    apple_silicon_only: bool


@dataclass(frozen=True)
class LlmModel:
    id: str
    label: str
    size_mb: int
    min_ram_gb: int
    blurb: str


# HF sizes measured 2026-10-30 with HfApi().model_info(files_metadata=True).
_STT_SIZES_MB = {
    "fw-tiny": 78,
    "fw-base": 148,
    "fw-small": 486,
    "fw-medium": 1531,
    "fw-large-v3-turbo": 1622,
    "fw-large-v3": 3091,
    "fw-distil-large-v3": 1516,
    "mlx-tiny": 74,
    "mlx-base": 144,
    "mlx-small": 481,
    "mlx-medium": 1525,
    "mlx-large-v3": 3084,
    "mlx-large-v3-turbo": 1614,
    "parakeet-v2": 2472,
}


def _stt_model(
    model_id: str,
    label: str,
    backend: str,
    model_size: str,
    repo: str,
    speed: int,
    accuracy: int,
    blurb: str,
    english_only: bool = False,
    apple_silicon_only: bool = False,
) -> SttModel:
    return SttModel(
        id=model_id,
        label=label,
        backend=backend,
        model_size=model_size,
        repo=repo,
        size_mb=_STT_SIZES_MB[model_id],
        english_only=english_only,
        speed=speed,
        accuracy=accuracy,
        blurb=blurb,
        apple_silicon_only=apple_silicon_only,
    )


_FW_CATALOG = (
    ("tiny", "Tiny", 5, 2, "Smallest download; best for quick tests."),
    ("base", "Base", 5, 3, "A balanced starting point for CPU transcription."),
    ("small", "Small", 4, 4, "More accurate while staying practical on most machines."),
    ("medium", "Medium", 3, 5, "High accuracy with a larger download."),
    ("large-v3-turbo", "Large v3 Turbo", 5, 5, "Near-large accuracy with faster decoding."),
    ("large-v3", "Large v3", 2, 5, "Maximum Whisper accuracy; the largest model."),
    ("distil-large-v3", "Distil Large v3", 4, 5, "Fast, English-only distillation of Large v3."),
)

_MLX_CATALOG = (
    ("tiny", "Tiny", 5, 2, "Smallest download; best for quick tests."),
    ("base", "Base", 5, 3, "A balanced Apple Silicon starting point."),
    ("small", "Small", 4, 4, "Recommended balance for Apple Silicon."),
    ("medium", "Medium", 3, 5, "High accuracy with a larger download."),
    ("large-v3", "Large v3", 4, 5, "Maximum Whisper accuracy on Apple Silicon."),
)

STT_MODELS = tuple(
    [
        _stt_model(
            f"fw-{size}", label, "faster-whisper", size, _MODELS[size],
            speed, accuracy, blurb, english_only=size == "distil-large-v3",
        )
        for size, label, speed, accuracy, blurb in _FW_CATALOG
    ]
    + [
        _stt_model(
            f"mlx-{size}", label, "mlx-whisper", size, _MLX_REPOS[size],
            speed, accuracy, blurb, apple_silicon_only=True,
        )
        for size, label, speed, accuracy, blurb in _MLX_CATALOG
    ]
    + [
        _stt_model(
            "mlx-large-v3-turbo", "Large v3 Turbo", "mlx-whisper", "large-v3-turbo",
            _MLX_REPOS["large-v3-turbo"], 5, 5,
            "Near-large accuracy with fast Apple Silicon decoding.",
            apple_silicon_only=True,
        ),
        _stt_model(
            "parakeet-v2", "Parakeet v2", "parakeet", "large-v3",
            PARAKEET_REPO, 5, 5, "Fast English-only speech recognition.",
            english_only=True, apple_silicon_only=True,
        ),
    ]
)

# Ollama manifest sizes measured 2026-10-30 by summing manifest layer sizes.
LLM_MODELS = (
    LlmModel("llama3.2:1b", "Llama 3.2 · 1B", 1321, 8, "The lightest cleanup option."),
    LlmModel("llama3.2:3b", "Llama 3.2 · 3B", 2019, 16, "Strong cleanup in a compact model."),
    LlmModel("qwen2.5:3b", "Qwen 2.5 · 3B", 1930, 8, "Compact multilingual cleanup."),
    LlmModel("qwen3:4b", "Qwen 3 · 4B", 2497, 8, "A capable small reasoning model."),
    LlmModel("gemma3:4b", "Gemma 3 · 4B", 3350, 8, "A polished general-purpose small model."),
    LlmModel("qwen2.5:7b", "Qwen 2.5 · 7B", 4683, 16, "Higher quality, with a larger memory footprint."),
    LlmModel("llama3.1:8b", "Llama 3.1 · 8B", 4921, 16, "A larger, capable cleanup model."),
)

_STT_BY_ID = {model.id: model for model in STT_MODELS}
_LLM_BY_ID = {model.id: model for model in LLM_MODELS}


@dataclass(frozen=True)
class HardwareInfo:
    system: str
    machine: str
    ram_gb: float
    apple_silicon: bool
    free_disk_gb: float


def detect_hardware() -> HardwareInfo:
    system = platform.system()
    machine = platform.machine()
    try:
        if system == "Darwin":
            output = subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True)
            ram_bytes = int(output.strip())
        elif system == "Windows":
            class MemoryStatusEx(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("sllAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            status = MemoryStatusEx()
            status.dwLength = ctypes.sizeof(status)
            if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
                raise OSError("GlobalMemoryStatusEx failed")
            ram_bytes = status.ullTotalPhys
        else:
            ram_bytes = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except Exception:  # noqa: BLE001
        ram_bytes = 0
    free_bytes = shutil.disk_usage(Path.home()).free
    return HardwareInfo(
        system=system,
        machine=machine,
        ram_gb=ram_bytes / (1024**3),
        apple_silicon=system == "Darwin" and machine in ("arm64", "aarch64"),
        free_disk_gb=free_bytes / (1024**3),
    )


def available_stt_models(hw: HardwareInfo) -> tuple[SttModel, ...]:
    return tuple(
        model for model in STT_MODELS
        if not model.apple_silicon_only or hw.apple_silicon
        if runtime_available(model.backend)
    )


def runtime_available(backend: str) -> bool:
    module = {
        "mlx-whisper": "mlx_whisper",
        "parakeet": "parakeet_mlx",
    }.get(backend)
    if module is None:
        return True
    try:
        return importlib.util.find_spec(module) is not None
    except (ImportError, ValueError):
        return False


def recommend(hw: HardwareInfo) -> tuple[str, str | None]:
    if hw.apple_silicon and runtime_available("mlx-whisper"):
        stt_id = "mlx-small" if hw.ram_gb >= 16 else "mlx-base"
    else:
        stt_id = "fw-base" if hw.ram_gb >= 8 else "fw-tiny"
    if hw.ram_gb >= 16:
        llm_id = "llama3.2:3b"
    elif hw.ram_gb >= 8:
        llm_id = "llama3.2:1b"
    else:
        llm_id = None
    return stt_id, llm_id


def stt_config(model: SttModel) -> dict:
    stt = {"provider": model.backend, "model": model.model_size}
    if model.backend in ("mlx-whisper", "parakeet"):
        stt["device"] = "auto"
    return {"stt": stt}


def current_stt_id(config: Config) -> str:
    backend = config.stt_backend
    size = config.model_size
    if backend == "parakeet":
        return "parakeet-v2"
    if backend in ("mlx", "mlx-whisper"):
        candidate = f"mlx-{size}"
    elif backend == "faster-whisper":
        candidate = f"fw-{size}"
    elif backend == "auto":
        mlx_available = (
            platform.system() == "Darwin"
            and platform.machine() in ("arm64", "aarch64")
            and importlib.util.find_spec("mlx_whisper") is not None
        )
        candidate = f"mlx-{size}" if mlx_available else f"fw-{size}"
    else:
        candidate = f"fw-{size}"
    return candidate if candidate in _STT_BY_ID else "fw-base"


def is_stt_downloaded(model: SttModel) -> bool:
    try:
        snapshot_download(repo_id=model.repo, local_files_only=True)
        return True
    except Exception:
        return False


class DownloadError(Exception):
    """A model download failed with a user-facing error message."""


class DownloadCancelled(DownloadError):
    """A model download was cancelled by the caller."""


def _stt_cache_blobs(repo: str) -> Path:
    from huggingface_hub.constants import HF_HUB_CACHE

    return Path(HF_HUB_CACHE) / f"models--{repo.replace('/', '--')}" / "blobs"


def download_stt(
    model: SttModel,
    on_progress: Callable[[int, int], None],
    cancel: threading.Event | None = None,
) -> Path:
    os.environ.setdefault("HF_HUB_DOWNLOAD_TIMEOUT", "30")
    os.environ.setdefault("HF_HUB_ETAG_TIMEOUT", "15")
    if cancel is not None and cancel.is_set():
        raise DownloadCancelled("Download cancelled")
    try:
        info = HfApi().model_info(model.repo, files_metadata=True)
        files = [(item.rfilename, item.size or 0) for item in info.siblings]
        total_bytes = sum(size for _name, size in files)
        if not files or total_bytes <= 0:
            raise DownloadError("The model repository has no downloadable files")
    except DownloadError:
        raise
    except Exception as exc:
        raise DownloadError(f"Could not inspect model files: {exc}") from exc

    completed_bytes = 0
    last_reported = 0
    polling = threading.Event()
    cancellation_seen = threading.Event()
    callback_lock = threading.Lock()

    def report(done: int) -> None:
        nonlocal last_reported
        done = min(total_bytes, max(last_reported, done))
        last_reported = done
        on_progress(done, total_bytes)

    def poll_progress() -> None:
        try:
            while not polling.wait(0.25):
                if cancel is not None and cancel.is_set():
                    raise DownloadCancelled("Download cancelled")
                incomplete_bytes = 0
                try:
                    incomplete_bytes = sum(
                        path.stat().st_size
                        for path in _stt_cache_blobs(model.repo).glob("*.incomplete")
                    )
                except OSError:
                    pass
                with callback_lock:
                    report(completed_bytes + incomplete_bytes)
        except DownloadCancelled:
            cancellation_seen.set()

    report(0)
    poller = threading.Thread(target=poll_progress, name="stt-download-progress", daemon=True)
    poller.start()
    try:
        for filename, size in files:
            if (cancel is not None and cancel.is_set()) or cancellation_seen.is_set():
                raise DownloadCancelled("Download cancelled")
            try:
                path = Path(hf_hub_download(repo_id=model.repo, filename=filename))
            except Exception as exc:
                raise DownloadError(f"Could not download {filename}: {exc}") from exc
            completed_bytes += size or path.stat().st_size
            with callback_lock:
                report(completed_bytes)
        return path
    finally:
        polling.set()
        poller.join(timeout=1)


class OllamaClient:
    def __init__(self, url: str):
        self.url = url.rstrip("/")

    def is_running(self) -> bool:
        try:
            return requests.get(f"{self.url}/api/tags", timeout=2).ok
        except requests.RequestException:
            return False

    def installed(self) -> list[str]:
        try:
            response = requests.get(f"{self.url}/api/tags", timeout=5)
            response.raise_for_status()
            return sorted(
                model["name"]
                for model in response.json().get("models", [])
                if model.get("name")
            )
        except requests.RequestException as exc:
            raise DownloadError(f"Could not list Ollama models: {exc}") from exc
        except (KeyError, TypeError, ValueError) as exc:
            raise DownloadError("Ollama returned an invalid model list") from exc

    def pull(
        self,
        tag: str,
        on_progress: Callable[[int, int, str], None],
        cancel: threading.Event | None = None,
    ) -> None:
        if cancel is not None and cancel.is_set():
            raise DownloadCancelled("Download cancelled")
        progress: dict[str, tuple[int, int]] = {}
        try:
            response = requests.post(
                f"{self.url}/api/pull",
                json={"model": tag, "stream": True},
                stream=True,
                timeout=(5, None),
            )
            response.raise_for_status()
            with response:
                for raw_line in response.iter_lines(decode_unicode=True):
                    if cancel is not None and cancel.is_set():
                        raise DownloadCancelled("Download cancelled")
                    if not raw_line:
                        continue
                    try:
                        event = json.loads(raw_line)
                    except (TypeError, ValueError) as exc:
                        raise DownloadError("Ollama returned an invalid pull response") from exc
                    if event.get("error"):
                        raise DownloadError(str(event["error"]))
                    digest = event.get("digest")
                    if digest and ("completed" in event or "total" in event):
                        progress[digest] = (
                            int(event.get("completed", 0)),
                            int(event.get("total", 0)),
                        )
                    completed = sum(item[0] for item in progress.values())
                    total = sum(item[1] for item in progress.values())
                    on_progress(completed, total, str(event.get("status", "")))
        except DownloadError:
            raise
        except DownloadCancelled:
            raise
        except requests.RequestException as exc:
            raise DownloadError(f"Could not pull {tag}: {exc}") from exc

def ollama_installed() -> bool:
    return shutil.which("ollama") is not None or Path("/Applications/Ollama.app").exists()
