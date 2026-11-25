"""Speech and speaker-label models LocalFlow downloaded into the Hugging Face cache.

Used by Models & keys -> "Free up space". Only the exact repositories
LocalFlow's catalogs can download are ever listed or deleted (Whisper via
faster-whisper or MLX, Parakeet, and the speaker-label models in
localflow.diarization.models); any other Hugging Face model (a 15 GB
LLM, say) and every Ollama model are invisible here. Nothing is loaded,
downloaded or initialised to build the list: it is only `stat` calls.

Sizes count each file once: Hugging Face stores data in `blobs/` and points
`snapshots/<rev>/<file>` symlinks at them, so files are deduplicated by inode
and a symlink only counts when it resolves inside the model's own folder.

Speaker-label repos can be shared with files LocalFlow never fetched (the
CAM++ repo holds many embedders), so for them only the pinned
`snapshots/<revision>/<filename>` of each catalog model is counted and
deleted, plus its `blobs/<sha256>` when nothing else in the folder can still
point at it. Everything else in such a folder is left alone.

Deletion refuses (re-checked at click time):
- any model a running LocalFlow process actually loaded (stt_lease: the
  dictation app and server keep their model until restarted, even after the
  saved config changes, and `auto` can fall back from MLX at load time);
- any model the saved config could load (both MLX and faster-whisper when
  the MLX import can't be ruled out, and the speaker-label models while
  `speaker_diarization` is on);
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
from localflow.diarization import models as diar_models
from localflow.diarization.models import diarization_repos
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
    """repo id -> display label for every speech and speaker-label model LocalFlow downloads.

    Speaker-label models come last.
    """
    repos = {
        FASTER_WHISPER_REPOS[size]: f"Whisper {_SIZE_LABELS[size]}"
        for size in _FW_CATALOG
    }
    repos.update({
        _MLX_REPOS[size]: f"Whisper {_SIZE_LABELS[size]} (Apple Silicon)"
        for size in _MLX_CATALOG
    })
    repos[PARAKEET_REPO] = "Parakeet v2"
    repos.update(diarization_repos())
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

    The speech model(s) from `_selected_stt_repos`, plus the speaker-label
    models while `speaker_diarization` is on. A plug-in speech backend still
    fails closed (UNKNOWN) whatever the flag.
    """
    repos = _selected_stt_repos(config, mlx_available)
    if repos is UNKNOWN:
        return UNKNOWN
    if config.speaker_diarization:
        repos = repos | frozenset(diarization_repos())
    return repos


def _selected_stt_repos(config: Config, mlx_available: Callable[[], bool]):
    """Speech repos the saved config could load (a frozenset), or UNKNOWN.

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


@dataclass(frozen=True)
class _Pinned:
    """One catalog speaker model's pinned snapshot file, found by `lstat` only."""

    revision_dir: Path
    pointer: Path               # snapshots/<revision>/<filename>
    pointer_info: os.stat_result
    blob: Path | None           # blobs/<sha256> when `pointer` links to it; None for a copy
    blob_info: os.stat_result | None


def _link_parts(target: str) -> list[str]:
    """Link text split by this OS's separators: `\\` is one only on Windows."""
    if os.altsep:
        target = target.replace(os.altsep, os.sep)
    return target.split(os.sep)


def _is_pinned_link(pointer: Path, sha256: str) -> bool:
    """The link text is exactly `../../blobs/<sha256>` (what the hub writes); never resolved."""
    try:
        target = os.readlink(pointer)
    except OSError:
        return False
    return _link_parts(target) == ["..", "..", "blobs", sha256]


def _pinned_files(folder: Path, repo: str) -> list[_Pinned]:
    """The current catalog's pinned files for `repo` inside its plain `folder`.

    Only plain directories are entered; a pinned name that is a link to
    anything but `../../blobs/<sha256>` (a regular file, under a plain
    `blobs/`), or that can't be inspected, is skipped as not ours.
    """
    found = []
    snapshots, blobs = folder / "snapshots", folder / "blobs"
    for model in diar_models.DIAR_MODELS:
        if model.repo != repo:
            continue
        revision_dir = snapshots / model.revision
        if not (diar_models._is_plain_dir(snapshots) and diar_models._is_plain_dir(revision_dir)):
            continue
        pointer = revision_dir / model.filename
        try:
            info = os.lstat(pointer)
            if stat.S_ISREG(info.st_mode):  # copy layout (no symlinks)
                found.append(_Pinned(revision_dir, pointer, info, None, None))
                continue
            if not stat.S_ISLNK(info.st_mode) or not _is_pinned_link(pointer, model.sha256):
                continue
            if not diar_models._is_plain_dir(blobs):
                continue
            blob = blobs / model.sha256
            blob_info = os.lstat(blob)
        except OSError:
            continue
        if stat.S_ISREG(blob_info.st_mode):
            found.append(_Pinned(revision_dir, pointer, info, blob, blob_info))
    return found


def _pinned_size(pinned: list[_Pinned]) -> int:
    seen: set[tuple[int, int]] = set()
    total = 0
    for item in pinned:
        info = item.blob_info if item.blob_info is not None else item.pointer_info
        key = (info.st_dev, info.st_ino)
        if key not in seen:
            seen.add(key)
            total += info.st_size
    return total


def _link_names(folder: Path) -> set[str] | None:
    """Last path component of every symlink anywhere under `folder`.

    None when any part can't be inspected (unreadable, or a linked or
    junction directory): the caller then keeps every blob.
    """
    names: set[str] = set()
    stack = [folder]
    while stack:
        current = stack.pop()
        if not diar_models._is_plain_dir(current):
            return None
        try:
            entries = list(os.scandir(current))
        except OSError:
            return None
        for entry in entries:
            try:
                if entry.is_symlink():
                    if entry.is_dir():  # a linked folder could hide links: never entered
                        return None
                    parts = [part for part in _link_parts(os.readlink(entry.path)) if part]
                    if parts:
                        names.add(os.path.normcase(parts[-1]))
                elif entry.is_dir(follow_symlinks=False):
                    stack.append(Path(entry.path))
            except OSError:
                return None
    return names


def _delete_pinned(folder: Path, repo: str, label: str) -> int:
    """Delete only the pinned files of a speaker-label repo; returns bytes freed.

    Snapshot files go first (then their revision and `snapshots/` folders if
    left empty). A pinned blob is removed only when no link left anywhere in
    the folder names it, it has no other hard link, and it is still the file
    that was inspected. Refs and every other file are kept.
    """
    freed = 0
    try:
        with diar_models._LOCK:  # never race an in-process download publishing
            pinned = _pinned_files(folder, repo)
            if not pinned:
                raise ModelStorageError(f"{label} isn't downloaded (or isn't a plain folder).")
            for item in pinned:
                try:
                    info = os.lstat(item.pointer)
                except FileNotFoundError:
                    continue
                if (info.st_dev, info.st_ino) != (
                    item.pointer_info.st_dev, item.pointer_info.st_ino
                ):
                    continue  # replaced since it was inspected
                os.unlink(item.pointer)
                if item.blob is None and info.st_nlink == 1:
                    freed += info.st_size
            for item in pinned:
                for directory in (item.revision_dir, item.revision_dir.parent):
                    try:
                        os.rmdir(directory)  # only succeeds when empty
                    except OSError:
                        pass
            names = _link_names(folder)
            for item in pinned:
                inspected = item.blob_info
                if item.blob is None or inspected is None or names is None:
                    continue
                if os.path.normcase(item.blob.name) in names:
                    continue  # another snapshot (any revision) still points at it
                try:
                    info = os.lstat(item.blob)
                except FileNotFoundError:
                    continue
                if (
                    not stat.S_ISREG(info.st_mode)
                    or info.st_nlink != 1
                    or (info.st_dev, info.st_ino) != (inspected.st_dev, inspected.st_ino)
                ):
                    continue
                os.unlink(item.blob)
                freed += info.st_size
    except OSError as exc:
        raise ModelStorageError(
            f"Couldn't fully delete {label} ({exc.strerror or exc}); "
            f"freed {format_bytes(freed)}."
        ) from exc
    return freed


def _repo_size(folder: Path, repo: str) -> int:
    """A speaker repo counts only its pinned files; a speech repo its whole folder."""
    if repo in diarization_repos():
        return _pinned_size(_pinned_files(folder, repo))
    return folder_size(folder)


def model_size(repo: str, cache: Path | None = None) -> int:
    """The size `inventory` would list for `repo` (0 when it isn't a plain folder)."""
    cache = cache if cache is not None else hf_hub_cache()
    folder = _model_folder(cache, repo)
    return 0 if folder is None else _repo_size(folder, repo)


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
        size = _repo_size(folder, repo)
        if size == 0:  # an empty or leftover folder (e.g. a cancelled download)
            continue
        status = guard.status(repo) or ("Unknown use" if guard.unknown else "")
        found.append(CachedModel(repo, label, folder, size, status))
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
        if repo in diarization_repos():
            raise ModelStorageError(
                f"{label} is used for speaker labels in your settings, so it was kept."
            )
        raise ModelStorageError(
            f"{label} is the speech model in your settings, so it was kept."
        )
    cache = cache if cache is not None else hf_hub_cache()
    folder = _model_folder(cache, repo)
    if folder is None:
        raise ModelStorageError(f"{label} isn't downloaded (or isn't a plain folder).")
    if repo in diarization_repos():
        return _delete_pinned(folder, repo, label)
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
