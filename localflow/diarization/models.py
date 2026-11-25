"""Speaker-label (diarization) models: pinned catalog, offline checks, download.

The two ONNX files live in the Hugging Face hub cache in its own layout
(`models--<org>--<name>/blobs/<sha256>` plus a relative symlink in
`snapshots/<revision>/<filename>`, or a plain copy in `snapshots/` where
symlinks are unavailable), so "Free up space" (model_storage) sizes, lists
and deletes them like any other model.

Readiness is strictly local: it never touches the network and never imports
`huggingface_hub`, `requests`, `localflow.models` or `sherpa_onnx`. Every
check re-hashes the bytes; a file that fails is removed so the next
`lf speakers download` replaces it. Deletion only ever happens inside the
model's own plain folder: a link that points anywhere else is removed as a
link, and its target is left alone.

Downloads happen only when `download_diarization_models` is called
explicitly (`lf speakers download`, `POST /api/diarization/download`).

This module imports only the standard library at module level, so
`localflow.model_storage` can import it without a cycle.
"""

from __future__ import annotations

import hashlib
import importlib.util
import logging
import os
import stat
import tempfile
import threading
import time
import urllib.parse
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class DiarModel:
    id: str
    label: str        # shown in Free up space
    repo: str
    revision: str     # pinned 40-hex commit
    filename: str
    sha256: str
    size_bytes: int
    licence: str
    source: str       # provenance of the weights and of the licence claim
    # Embedders only: cosine-similarity floor for naming a recognised speaker (None = never).
    match_threshold: float | None = None


# Both repos are public and ungated. Revisions, sizes and SHA-256 were
# verified against the Hugging Face API (git/LFS metadata) and by streaming
# the full pinned bytes through sha256sum. The embedding licence was checked
# at the exact upstream: ModelScope iic/speech_campplus_sv_zh_en_16k-common_advanced
# declares Apache License 2.0. CAM++ is the design's first embedder candidate;
# the final choice (and any match threshold) belongs to B10.
DIAR_MODELS: tuple[DiarModel, ...] = (
    DiarModel(
        id="pyannote-segmentation-3.0",
        label="Speaker labels: segmentation",
        repo="csukuangfj/sherpa-onnx-pyannote-segmentation-3-0",
        revision="9403a6902bb58e3d5ae8c7e77c3422de279db2e0",
        filename="model.onnx",
        sha256="220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        size_bytes=5992913,
        licence="MIT",
        source=(
            "sherpa-onnx export of pyannote/segmentation-3.0; "
            "MIT per the LICENSE file at the pinned revision"
        ),
    ),
    DiarModel(
        id="3dspeaker-campplus-zh-en-16k-advanced",
        label="Speaker labels: voice embedding",
        repo="csukuangfj/speaker-embedding-models",
        revision="a8055d6daef723a6a4ecf4de233708575bf4994f",
        filename="3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        sha256="53cab20ba8f2eb2f6e2d89ecc18e88ef3222169b61b4419cbc13666744a4574b",
        size_bytes=28281165,
        licence="Apache-2.0",
        source=(
            "sherpa-onnx export of 3D-Speaker CAM++; upstream ModelScope model "
            "iic/speech_campplus_sv_zh_en_16k-common_advanced declares Apache License 2.0"
        ),
        match_threshold=0.6,  # provisional and deliberately strict; B10 sets the measured value
    ),
)

# Config flags reported by `lf speakers status` and the status endpoint (design §5.8).
FLAG_NAMES = ("speaker_diarization", "diarize_mic", "speaker_recognition")

_HASH_CHUNK_BYTES = 1024 * 1024
_CHUNK_BYTES = 256 * 1024
_PART_SUFFIX = ".lf-part"
_STALE_PART_SECONDS = 60 * 60  # a live download rewrites its partial every chunk
# Windows junction reparse tag; the stat module only defines it on Windows.
_MOUNT_POINT_TAG = getattr(stat, "IO_REPARSE_TAG_MOUNT_POINT", 0xA0000003)
# Held around check+evict and around publication, so an in-process status
# check can never evict a file a download is publishing.
_LOCK = threading.RLock()

NOT_DOWNLOADED = "Speaker models are not downloaded. Run `lf speakers download`."
_NOT_PLAIN = (
    "The speaker model cache folder isn't a plain folder; move or remove it "
    "and retry."
)


class DiarizationModelsUnavailable(RuntimeError):
    """The models are missing or failed verification; str(exc) is user-facing."""


# ------------------------------------------------------------------ helpers ---


def _cache(cache: Path | None) -> Path:
    if cache is not None:
        return Path(cache)
    from localflow.model_storage import hf_hub_cache

    return hf_hub_cache()


def _folder_name(repo: str) -> str:
    from localflow.model_storage import repo_folder_name

    return repo_folder_name(repo)


def _is_plain_dir(path: Path) -> bool:
    """A real directory: not a symlink, not a Windows junction."""
    try:
        info = os.lstat(path)
    except OSError:
        return False
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        return False
    # Windows junctions (any Python version): lstat reports a directory with this tag.
    return getattr(info, "st_reparse_tag", 0) != _MOUNT_POINT_TAG


def _repo_folder(cache: Path, repo: str) -> Path | None:
    """The repo's folder if it is a plain directory directly inside `cache`."""
    folder = cache / _folder_name(repo)
    if not _is_plain_dir(folder):
        return None
    try:
        if folder.resolve().parent != cache.resolve():
            return None
    except OSError:
        return None
    return folder


def _links_to(pointer: Path, blob: Path) -> bool:
    """True when the symlink `pointer` fully resolves to exactly `blob`.

    Resolved, never normalised lexically: `blobs/alias/../<sha256>` through a
    linked `alias` lands somewhere else. Missing targets and link loops are
    False.
    """
    try:
        target = pointer.resolve(strict=True)
        expected = blob.resolve(strict=True)
    except (OSError, RuntimeError):  # missing, or a loop (RuntimeError before 3.13)
        return False
    return os.path.normcase(str(target)) == os.path.normcase(str(expected))


def _identity(info: os.stat_result) -> tuple[int, int, int, int]:
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)


def _unlink_if_same(path: Path, identity: tuple[int, int, int, int]) -> bool:
    """Remove the regular file at `path` only if it is still the file we checked."""
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return True
    if not stat.S_ISREG(info.st_mode) or _identity(info) != identity:
        return False
    os.unlink(path)
    return True


def _unlink_link(path: Path) -> None:
    """Remove `path` if it is a symlink; never touches what it points at."""
    try:
        if os.path.islink(path):
            os.unlink(path)
    except FileNotFoundError:
        pass


def _hash_file(path: Path, model: DiarModel):
    """(identity, size_ok, digest_ok) of the regular file at `path`, never following a link."""
    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags)
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise IsADirectoryError(f"{model.filename} is not a regular file")
        identity = _identity(info)
        if info.st_size != model.size_bytes:
            return identity, False, False
        digest = hashlib.sha256()
        while chunk := handle.read(_HASH_CHUNK_BYTES):
            digest.update(chunk)
    return identity, True, digest.hexdigest() == model.sha256


# -------------------------------------------------------------- public API ---


def diarization_repos() -> dict[str, str]:
    """repo id -> Free-up-space label, in catalog order."""
    repos: dict[str, str] = {}
    for model in DIAR_MODELS:
        repos.setdefault(model.repo, model.label)
    return repos


def runtime_installed() -> bool:
    """Whether the sherpa-onnx runtime is importable; never imports it."""
    try:
        return importlib.util.find_spec("sherpa_onnx") is not None
    except (ImportError, ValueError):
        return False


def model_path(model: DiarModel, cache: Path | None = None) -> Path:
    """The snapshot pointer for `model` (not verified, may not exist)."""
    return (
        _cache(cache) / _folder_name(model.repo) / "snapshots" / model.revision / model.filename
    )


def check_model(model: DiarModel, cache: Path | None = None) -> tuple[Path | None, str]:
    """(verified snapshot path, "") or (None, user-facing reason).

    Offline. Removes bytes that fail the size or checksum check, but only
    inside the model's own plain folder. Never raises.
    """
    cache = _cache(cache)
    with _LOCK:
        try:
            return _check_locked(model, cache)
        except OSError as exc:
            return None, f"Could not check {model.filename}: {exc.strerror or exc}."


def _check_locked(model: DiarModel, cache: Path) -> tuple[Path | None, str]:
    folder = cache / _folder_name(model.repo)
    if not os.path.lexists(folder):
        return None, NOT_DOWNLOADED
    if _repo_folder(cache, model.repo) is None:
        return None, _NOT_PLAIN
    snapshots = folder / "snapshots"
    revision_dir = snapshots / model.revision
    for directory in (snapshots, revision_dir):
        if not os.path.lexists(directory):
            return None, NOT_DOWNLOADED
        if not _is_plain_dir(directory):
            return None, _NOT_PLAIN  # never read through a linked directory
    pointer = revision_dir / model.filename
    try:
        info = os.lstat(pointer)
    except FileNotFoundError:
        return None, NOT_DOWNLOADED

    checksum = (
        f"{model.filename} failed its checksum and was removed. "
        "Run `lf speakers download`."
    )
    if stat.S_ISLNK(info.st_mode):
        blobs = folder / "blobs"
        blob = blobs / model.sha256
        foreign = (
            f"{model.filename} didn't point to its pinned download and was removed. "
            "Run `lf speakers download`."
        )
        if not _is_plain_dir(blobs):
            _unlink_link(pointer)  # the link only; its target is not ours
            log.warning("Removed a speaker model link that pointed elsewhere: %s", model.filename)
            return None, foreign
        try:
            blob_info = os.lstat(blob)
        except FileNotFoundError:
            return None, NOT_DOWNLOADED
        if not stat.S_ISREG(blob_info.st_mode):
            return None, NOT_DOWNLOADED
        if not _links_to(pointer, blob):
            _unlink_link(pointer)  # the link only; its target is not ours
            log.warning("Removed a speaker model link that pointed elsewhere: %s", model.filename)
            return None, foreign
        identity, size_ok, digest_ok = _hash_file(blob, model)
        if size_ok and digest_ok:
            return pointer, ""
        if not _unlink_if_same(blob, identity):
            return None, f"{model.filename} changed while it was being checked; try again."
        _unlink_link(pointer)
        log.warning("Removed speaker model %s: size or checksum mismatch", model.filename)
        return None, checksum

    if stat.S_ISREG(info.st_mode):  # copy layout (no symlinks)
        identity, size_ok, digest_ok = _hash_file(pointer, model)
        if size_ok and digest_ok:
            return pointer, ""
        if not _unlink_if_same(pointer, identity):
            return None, f"{model.filename} changed while it was being checked; try again."
        log.warning("Removed speaker model %s: size or checksum mismatch", model.filename)
        return None, checksum

    return None, NOT_DOWNLOADED  # a directory or something else: left alone


def models_ready(cache: Path | None = None) -> tuple[bool, str]:
    """(True, "") when every model is present and verified, else (False, reason)."""
    for model in DIAR_MODELS:
        path, reason = check_model(model, cache)
        if path is None:
            return False, reason
    return True, ""


def verified_model_paths(cache: Path | None = None) -> dict[str, Path]:
    """model id -> verified local ONNX path; raises DiarizationModelsUnavailable."""
    paths: dict[str, Path] = {}
    for model in DIAR_MODELS:
        path, reason = check_model(model, cache)
        if path is None:
            raise DiarizationModelsUnavailable(reason)
        paths[model.id] = path
    return paths


def flags(config) -> dict[str, bool]:
    """The speaker-label config flags, for status output."""
    return {name: bool(getattr(config, name, False)) for name in FLAG_NAMES}


def status(cache: Path | None = None) -> dict:
    """Offline readiness of models and runtime, reported separately."""
    models = []
    first_reason = ""
    for model in DIAR_MODELS:
        path, reason = check_model(model, cache)
        if path is None and not first_reason:
            first_reason = reason
        models.append({
            "id": model.id,
            "label": model.label,
            "repo": model.repo,
            "revision": model.revision,
            "filename": model.filename,
            "size_bytes": model.size_bytes,
            "licence": model.licence,
            "ready": path is not None,
            "reason": reason,
        })
    return {
        "models_ready": all(item["ready"] for item in models),
        "reason": first_reason,
        "extra_installed": runtime_installed(),
        "models": models,
    }


# ---------------------------------------------------------------- download ---


def _open_stream(url: str):
    """The single network seam (tests replace it). Public repos: no token sent."""
    import requests

    return requests.get(url, stream=True, timeout=(10, 30), allow_redirects=True)


def _endpoint() -> str:
    return (os.environ.get("HF_ENDPOINT") or "https://huggingface.co").rstrip("/")


def _model_url(model: DiarModel) -> str:
    return (
        f"{_endpoint()}/{model.repo}/resolve/{model.revision}/"
        f"{urllib.parse.quote(model.filename)}"
    )


def download_diarization_models(
    on_progress: Callable[[int, int], None],
    cancel: threading.Event | None = None,
    *,
    cache: Path | None = None,
) -> dict[str, Path]:
    """Fetch and verify every pinned model; returns model id -> verified path.

    Already-verified files are kept and not fetched again. Raises
    DownloadCancelled when `cancel` is set (before, during, or after any
    file, including the last) and DownloadError with a user-facing message.
    """
    from localflow.models import DownloadCancelled

    cache = _cache(cache)
    models = DIAR_MODELS
    total = sum(model.size_bytes for model in models)
    last = 0

    def check_cancel() -> None:
        if cancel is not None and cancel.is_set():
            raise DownloadCancelled("Download cancelled")

    def report(done: int) -> None:
        nonlocal last
        last = min(total, max(last, done))
        on_progress(last, total)

    check_cancel()
    report(0)
    done = 0
    paths: dict[str, Path] = {}
    for model in models:
        check_cancel()
        path, _reason = check_model(model, cache)
        if path is None:
            base = done
            path = _fetch(model, cache, lambda received, base=base: report(base + received), cancel)
        done += model.size_bytes
        report(done)
        paths[model.id] = path
        check_cancel()
    return paths


def _ensure_plain_dir(path: Path) -> None:
    """Create `path` (its parent is already a plain directory) or confirm it is plain.

    Checked before creating: an existing link is never created through.
    """
    from localflow.models import DownloadError

    if not os.path.lexists(path):
        try:
            os.mkdir(path)
        except FileExistsError:
            pass
    if not _is_plain_dir(path):
        raise DownloadError(_NOT_PLAIN)


def _prepare_folders(model: DiarModel, cache: Path) -> Path:
    from localflow.models import DownloadError

    cache.mkdir(parents=True, exist_ok=True)  # the cache root itself may be a link
    folder = cache / _folder_name(model.repo)
    _ensure_plain_dir(folder)
    if _repo_folder(cache, model.repo) is None:
        raise DownloadError(_NOT_PLAIN)
    _ensure_plain_dir(folder / "blobs")
    _ensure_plain_dir(folder / "snapshots")
    _ensure_plain_dir(folder / "snapshots" / model.revision)
    return folder


def _remove_stale_parts(blobs: Path, model: DiarModel) -> None:
    """Delete this model's old partial downloads (left by a killed process).

    Only regular files directly in the plain `blobs/` folder named
    `<sha256[:16]>.*.lf-part` and untouched for `_STALE_PART_SECONDS` are
    removed: a fresh one may belong to a download running in another process.
    Nothing is traversed, followed or resolved. Best effort.
    """
    prefix = model.sha256[:16] + "."
    cutoff = time.time() - _STALE_PART_SECONDS
    try:
        entries = list(os.scandir(blobs))
    except OSError:
        return
    for entry in entries:
        if not (entry.name.startswith(prefix) and entry.name.endswith(_PART_SUFFIX)):
            continue
        try:
            info = entry.stat(follow_symlinks=False)
            if stat.S_ISREG(info.st_mode) and info.st_mtime < cutoff:
                os.unlink(entry.path)
        except OSError:
            continue


def _fetch(
    model: DiarModel,
    cache: Path,
    on_received: Callable[[int], None],
    cancel: threading.Event | None,
) -> Path:
    import requests

    from localflow.models import DownloadCancelled, DownloadError

    try:
        folder = _prepare_folders(model, cache)
    except OSError as exc:
        raise DownloadError(f"Could not save {model.filename}: {exc.strerror or exc}") from exc
    blobs = folder / "blobs"
    url = _model_url(model)
    _remove_stale_parts(blobs, model)
    try:
        fd, tmp_name = tempfile.mkstemp(
            dir=blobs, prefix=model.sha256[:16] + ".", suffix=_PART_SUFFIX
        )
    except OSError as exc:
        raise DownloadError(f"Could not save {model.filename}: {exc.strerror or exc}") from exc
    tmp = Path(tmp_name)
    try:
        digest = hashlib.sha256()
        received = 0
        try:
            with os.fdopen(fd, "wb") as out, _open_stream(url) as response:
                response.raise_for_status()
                for chunk in response.iter_content(_CHUNK_BYTES):
                    if cancel is not None and cancel.is_set():
                        raise DownloadCancelled("Download cancelled")
                    if not chunk:
                        continue
                    received += len(chunk)
                    if received > model.size_bytes:
                        raise DownloadError(
                            f"{model.filename} failed its checksum, so it was discarded. Try again."
                        )
                    digest.update(chunk)
                    out.write(chunk)
                    on_received(received)
                out.flush()
                os.fsync(out.fileno())
            if cancel is not None and cancel.is_set():
                raise DownloadCancelled("Download cancelled")
            if received != model.size_bytes or digest.hexdigest() != model.sha256:
                raise DownloadError(
                    f"{model.filename} failed its checksum, so it was discarded. Try again."
                )
            with _LOCK:
                return _publish(tmp, folder, model)
        except DownloadError:
            raise
        except requests.RequestException as exc:
            # Never echo or chain the exception: its text can hold a signed CDN URL.
            response = getattr(exc, "response", None)
            status_code = getattr(response, "status_code", None)
            if status_code:
                detail = f"HTTP {status_code}"
            else:
                detail = "network problem; check your connection and try again"
            raise DownloadError(f"Could not download {model.filename}: {detail}") from None
        except OSError as exc:
            raise DownloadError(
                f"Could not save {model.filename}: {exc.strerror or exc}"
            ) from exc
    finally:
        try:
            if os.path.lexists(tmp):
                os.unlink(tmp)
        except OSError:
            log.warning("Could not remove a partial speaker model download")


def _publish(tmp: Path, folder: Path, model: DiarModel) -> Path:
    """Move verified bytes into the hub layout: blobs/<sha256> + snapshot link (or copy)."""
    blob = folder / "blobs" / model.sha256
    revision_dir = folder / "snapshots" / model.revision
    pointer = revision_dir / model.filename
    for directory in (folder / "blobs", folder / "snapshots", revision_dir):
        if not _is_plain_dir(directory):  # re-checked: never publish through a link
            from localflow.models import DownloadError

            raise DownloadError(_NOT_PLAIN)
    os.replace(tmp, blob)
    link_tmp = revision_dir / f".{model.filename}.{os.getpid()}{_PART_SUFFIX}"
    try:
        if os.path.lexists(link_tmp):
            os.unlink(link_tmp)
        os.symlink(os.path.join("..", "..", "blobs", model.sha256), link_tmp)
    except (OSError, NotImplementedError):
        # No symlink privilege (Windows): the copy layout huggingface_hub uses too.
        try:
            if os.path.lexists(link_tmp):
                os.unlink(link_tmp)
        except OSError:
            pass
        os.replace(blob, pointer)
        return pointer
    try:
        os.replace(link_tmp, pointer)
    except OSError:
        try:
            os.unlink(link_tmp)
        except OSError:
            pass
        raise
    return pointer
