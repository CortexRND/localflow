"""Speech models LocalFlow downloaded into the Hugging Face cache.

Used by Models & keys -> "Free up space". Only the exact repositories
LocalFlow's speech catalog can download are ever listed or deleted (Whisper
via faster-whisper or MLX, Parakeet); any other Hugging Face model (a 15 GB
LLM, say) and every Ollama model are invisible here. Nothing is loaded,
downloaded or initialised to build the list: it is only `stat` calls.

Sizes count each file once: Hugging Face stores data in `blobs/` and points
`snapshots/<rev>/<file>` symlinks at them, so files are deduplicated by inode
and a symlink only counts when it resolves inside the model's own folder.

Deletion refuses (re-checked at click time):
- any model a running LocalFlow process actually loaded (stt_lease: the
  dictation app and server keep their model until restarted, even after the
  saved config changes, and `auto` can fall back from MLX at load time);
- any model the saved config could load (both MLX and faster-whisper when
  the MLX import can't be ruled out);
- everything, when a plug-in backend or an unreadable lease makes the use
  unknown;
- anything not in the catalog, and any folder that is a symlink or resolves
  outside the cache.
"""

from __future__ import annotations

import importlib.util
import os
import shutil
import stat
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path

from localflow.config import Config
from localflow.providers.stt_mlx_whisper import _MLX_REPOS
from localflow.providers.stt_parakeet import PARAKEET_REPO

# faster_whisper.utils._MODELS (size name -> repo), copied so listing models
# never imports faster-whisper/ctranslate2; a test keeps the two in sync.
FASTER_WHISPER_REPOS = {
    "tiny.en": "Systran/faster-whisper-tiny.en",
    "tiny": "Systran/faster-whisper-tiny",
    "base.en": "Systran/faster-whisper-base.en",
    "base": "Systran/faster-whisper-base",
    "small.en": "Systran/faster-whisper-small.en",
    "small": "Systran/faster-whisper-small",
    "medium.en": "Systran/faster-whisper-medium.en",
    "medium": "Systran/faster-whisper-medium",
    "large-v1": "Systran/faster-whisper-large-v1",
    "large-v2": "Systran/faster-whisper-large-v2",
    "large-v3": "Systran/faster-whisper-large-v3",
    "large": "Systran/faster-whisper-large-v3",
    "distil-large-v2": "Systran/faster-distil-whisper-large-v2",
    "distil-medium.en": "Systran/faster-distil-whisper-medium.en",
    "distil-small.en": "Systran/faster-distil-whisper-small.en",
    "distil-large-v3": "Systran/faster-distil-whisper-large-v3",
    "distil-large-v3.5": "distil-whisper/distil-large-v3.5-ct2",
    "large-v3-turbo": "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
    "turbo": "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
}
# The sizes LocalFlow's catalog offers (models.STT_MODELS).
_FW_CATALOG = ("tiny", "base", "small", "medium", "large-v3-turbo", "large-v3", "distil-large-v3")
_MLX_CATALOG = ("tiny", "base", "small", "medium", "large-v3", "large-v3-turbo")
_SIZE_LABELS = {
    "tiny": "Tiny", "base": "Base", "small": "Small", "medium": "Medium",
    "large-v3": "Large v3", "large-v3-turbo": "Large v3 Turbo",
    "distil-large-v3": "Distil Large v3",
}


def catalog() -> dict[str, str]:
    """repo id -> display label for every speech model LocalFlow downloads."""
    repos = {
        FASTER_WHISPER_REPOS[size]: f"Whisper {_SIZE_LABELS[size]}"
        for size in _FW_CATALOG
    }
    repos.update({
        _MLX_REPOS[size]: f"Whisper {_SIZE_LABELS[size]} (Apple Silicon)"
        for size in _MLX_CATALOG
    })
    repos[PARAKEET_REPO] = "Parakeet v2"
    return repos


class ModelStorageError(RuntimeError):
    """A refused or failed deletion; the message is user-facing."""


@dataclass(frozen=True)
class CachedModel:
    repo: str
    label: str
    path: Path
    size_bytes: int
    status: str  # "In use", "Selected", "Unknown use" or "" (deletable)

    @property
    def protected(self) -> bool:
        return bool(self.status)


def hf_hub_cache(env: Mapping[str, str] | None = None) -> Path:
    """The hub cache huggingface_hub uses, from the same variables."""
    env = os.environ if env is None else env

    def expand(value: str) -> Path:
        return Path(os.path.expandvars(os.path.expanduser(value)))

    if env.get("HF_HUB_CACHE"):
        return expand(env["HF_HUB_CACHE"])
    if env.get("HUGGINGFACE_HUB_CACHE"):
        return expand(env["HUGGINGFACE_HUB_CACHE"])
    if env.get("HF_HOME"):
        return expand(env["HF_HOME"]) / "hub"
    xdg = env.get("XDG_CACHE_HOME") or os.path.join(os.path.expanduser("~"), ".cache")
    return expand(xdg) / "huggingface" / "hub"


def repo_folder_name(repo: str) -> str:
    return "models--" + repo.replace("/", "--")


def _mlx_available() -> bool:
    try:
        return importlib.util.find_spec("mlx_whisper") is not None
    except (ImportError, ValueError):
        return False


UNKNOWN = object()  # a third-party STT backend: can't tell what it uses


def _faster_whisper_repo(size) -> str | None:
    if size in FASTER_WHISPER_REPOS:
        return FASTER_WHISPER_REPOS[size]
    if isinstance(size, str) and size.count("/") == 1 and not size.startswith(("/", ".", "~")):
        return size  # WhisperModel also takes a Hugging Face repo id
    return None


def selected_repos(config: Config, mlx_available: Callable[[], bool] = _mlx_available):
    """Repos the saved config could load (a frozenset), or UNKNOWN.

    Mirrors stt.Transcriber without loading anything. parakeet -> Parakeet.
    auto/mlx with device auto and an MLX size tries MLX first and falls back
    to faster-whisper on ImportError. A missing `mlx_whisper` spec means the
    import will fail, so only faster-whisper is possible; a present spec does
    not prove the import (or its native deps) will work, so both candidates
    are kept: this says "could be in use", never "is in use".
    """
    backend, size, device = config.stt_backend, config.model_size, config.stt_device
    if backend == "parakeet":
        return frozenset({PARAKEET_REPO})
    if backend not in ("auto", "mlx", "mlx-whisper", "faster-whisper"):
        return UNKNOWN
    repos = set()
    tries_mlx = (
        backend in ("auto", "mlx", "mlx-whisper") and device == "auto" and size in _MLX_REPOS
    )
    if tries_mlx and mlx_available():
        repos.add(_MLX_REPOS[size])
    fallback = _faster_whisper_repo(size)
    if fallback is not None:
        repos.add(fallback)
    return frozenset(repos)


@dataclass(frozen=True)
class Protection:
    """Why models can't be deleted right now."""

    running: frozenset   # repos a live LocalFlow process actually loaded
    selected: frozenset  # repos the saved config could load
    unknown: str         # non-empty: refuse every deletion, with this reason

    def status(self, repo: str) -> str:
        if repo in self.running:
            return "In use"
        if repo in self.selected:
            return "Selected"
        return ""


def protection(
    config: Config,
    mlx_available: Callable[[], bool] = _mlx_available,
    leases=None,
) -> Protection:
    """Combine the saved config with the live STT leases (stt_lease)."""
    if leases is None:
        from localflow.stt_lease import live_leases

        leases = live_leases()
    running, unknown = set(), ""
    for lease in leases:
        if not lease.known:
            unknown = (
                "A running LocalFlow process uses a speech model LocalFlow can't "
                "identify, so nothing was deleted. Quit it or restart LocalFlow first."
            )
        elif lease.repo:
            running.add(lease.repo)
    selected = selected_repos(config, mlx_available)
    if selected is UNKNOWN:
        unknown = unknown or (
            "Your speech provider is a plug-in, so LocalFlow can't tell which "
            "model it uses. Nothing was deleted."
        )
        selected = frozenset()
    return Protection(frozenset(running), selected, unknown)


def folder_size(folder: Path) -> int:
    """Bytes stored under `folder`, each file once, never leaving `folder`."""
    root = os.path.realpath(folder)
    seen: set[tuple[int, int]] = set()
    total = 0
    stack = [str(folder)]
    while stack:
        current = stack.pop()
        try:
            entries = list(os.scandir(current))
        except OSError:
            continue
        for entry in entries:
            try:
                if entry.is_symlink():
                    target = os.path.realpath(entry.path)
                    if os.path.commonpath([root, target]) != root:
                        continue  # points outside this model: not ours to count
                    info = os.stat(target)
                    if not stat.S_ISREG(info.st_mode):
                        continue
                elif entry.is_dir(follow_symlinks=False):
                    stack.append(entry.path)
                    continue
                elif entry.is_file(follow_symlinks=False):
                    info = entry.stat(follow_symlinks=False)
                else:
                    continue
            except (OSError, ValueError):
                continue
            key = (info.st_dev, info.st_ino)
            if key not in seen:
                seen.add(key)
                total += info.st_size
    return total


def _model_folder(cache: Path, repo: str) -> Path | None:
    """The repo's folder if it is a real directory directly inside `cache`."""
    folder = cache / repo_folder_name(repo)
    try:
        if folder.is_symlink() or not folder.is_dir():
            return None
        if folder.resolve().parent != cache.resolve():
            return None
    except OSError:
        return None
    return folder


def inventory(
    config: Config,
    cache: Path | None = None,
    mlx_available: Callable[[], bool] = _mlx_available,
    leases=None,
) -> list[CachedModel]:
    """Downloaded catalog models, biggest first.

    `status` is "In use" (a live process loaded it), "Selected" (the saved
    config could load it) or "" (safe to delete). With an unknown runtime
    every model is marked protected.
    """
    cache = cache if cache is not None else hf_hub_cache()
    guard = protection(config, mlx_available, leases)
    found = []
    for repo, label in catalog().items():
        folder = _model_folder(cache, repo)
        if folder is None:
            continue
        status = guard.status(repo) or ("Unknown use" if guard.unknown else "")
        found.append(CachedModel(repo, label, folder, folder_size(folder), status))
    return sorted(found, key=lambda model: (-model.size_bytes, model.label))


def delete_unused(
    repo: str,
    config: Config,
    cache: Path | None = None,
    mlx_available: Callable[[], bool] = _mlx_available,
    leases=None,
) -> int:
    """Delete one unused catalog model; returns the bytes freed.

    `config` must be freshly loaded by the caller and leases are re-read
    here: the check is redone at click time, so a model chosen or loaded
    since the list was drawn is safe.
    """
    label = catalog().get(repo)
    if label is None:
        raise ModelStorageError("That isn't a speech model LocalFlow manages.")
    guard = protection(config, mlx_available, leases)
    if guard.unknown:
        raise ModelStorageError(guard.unknown)
    if repo in guard.running:
        raise ModelStorageError(
            f"{label} is loaded by a running LocalFlow process, so it was kept."
        )
    if repo in guard.selected:
        raise ModelStorageError(
            f"{label} is the speech model in your settings, so it was kept."
        )
    cache = cache if cache is not None else hf_hub_cache()
    folder = _model_folder(cache, repo)
    if folder is None:
        raise ModelStorageError(f"{label} isn't downloaded (or isn't a plain folder).")
    before = folder_size(folder)
    try:
        shutil.rmtree(folder)
    except OSError as exc:
        left = folder_size(folder) if folder.exists() else 0
        freed = max(0, before - left)
        raise ModelStorageError(
            f"Couldn't fully delete {label} ({exc.strerror or exc}); "
            f"freed {format_bytes(freed)}."
        ) from exc
    return before


def format_bytes(size: int) -> str:
    value = float(size)
    for unit in ("bytes", "KB", "MB", "GB"):
        if value < 1000 or unit == "GB":
            if unit == "bytes":
                return f"{int(value)} bytes"
            return f"{value:.1f} {unit}" if value < 10 else f"{value:.0f} {unit}"
        value /= 1000
    return f"{value:.0f} GB"
