"""Which speech model each running LocalFlow process actually loaded.

The dictation app and the server load their STT model once and keep it after
the saved config changes (live_config never touches STT), and `auto` may
fall back from MLX to faster-whisper at load time. So "Free up space" can't
judge "in use" from the config alone. Each process that finishes building a
Transcriber leaves a small lease here:

    ~/.localflow/stt-leases/<pid>.json   {"version", "pid", "backend", "repo", "known"}

A temporary model (a History retry in the Hub or CLI) gets its own scoped
lease, `<pid>.<token>.json`, removed as soon as that work ends.

A lease counts only while its pid is alive (checked with a signal-0 probe
or the Windows process API, never a subprocess). Leases hold no secrets and
live in LocalFlow's data folder, never in the Hugging Face cache.
Unreadable leases of a live pid are reported as "unknown" so callers can
refuse to delete anything rather than guess.
"""

from __future__ import annotations

import atexit
import json
import logging
import os
import re
import secrets
import stat
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

log = logging.getLogger("localflow.stt_lease")

VERSION = 1
MAX_LEASES = 64
MAX_LEASE_BYTES = 4096
# "<pid>.json" (long-lived) or "<pid>.<hex token>.json" (scoped, temporary).
_LEASE_NAME = re.compile(r"(\d{1,10})(?:\.[0-9a-f]{16})?\.json")
_registered: set[Path] = set()


@dataclass(frozen=True)
class Lease:
    pid: int
    backend: str
    repo: str | None   # Hugging Face repo loaded; None = nothing from the cache
    known: bool        # False: a plug-in backend or an unreadable lease


def lease_dir() -> Path:
    return Path.home() / ".localflow" / "stt-leases"


def runtime_repo(backend: str, model_size: str) -> tuple[str | None, bool]:
    """(repo, known) for what Transcriber actually loaded.

    `backend` is Transcriber.backend after loading: "mlx", "faster-whisper",
    "parakeet", or a plug-in id (unknown).
    """
    from localflow.model_storage import _MLX_REPOS, FASTER_WHISPER_REPOS, PARAKEET_REPO

    if backend == "parakeet":
        return PARAKEET_REPO, True
    if backend == "mlx":
        repo = _MLX_REPOS.get(model_size)
        return repo, repo is not None
    if backend == "faster-whisper":
        if model_size in FASTER_WHISPER_REPOS:
            return FASTER_WHISPER_REPOS[model_size], True
        if isinstance(model_size, str) and model_size.count("/") == 1 and not model_size.startswith(
            ("/", ".", "~")
        ):
            return model_size, True
        return None, True  # a local path: nothing in the HF cache
    return None, False


def selection_backend(stt_backend: object) -> str:
    """The lease backend for a speech model that is *about to* load.

    Used before loading, while a model may still be downloading: a named
    engine maps to the runtime backend it will use; `auto` (it may fall back
    at load time) and plug-in engines map to "unknown", whose lease is
    known=False, so Free up space conservatively deletes nothing meanwhile.
    """
    name = (stt_backend or "auto").strip() if isinstance(stt_backend, str) else ""
    return {
        "parakeet": "parakeet", "faster-whisper": "faster-whisper",
        "mlx": "mlx", "mlx-whisper": "mlx",
    }.get(name, "unknown")


def record(
    backend: str, model_size: str, root: Path | None = None, *, scoped: bool = False,
) -> Path | None:
    """Leave this process's lease; never raises.

    The default lease (`<pid>.json`) is the process's long-lived model and
    is replaced by a later default `record`. `scoped=True` writes a separate,
    uniquely named lease (`<pid>.<token>.json`) for a temporary model (a
    History retry); release it with `release(path)` when done. A scoped
    lease never overwrites or removes the long-lived one.
    """
    folder = Path(root) if root is not None else lease_dir()
    repo, known = runtime_repo(backend, model_size)
    data = {
        "version": VERSION, "pid": os.getpid(), "backend": str(backend)[:64],
        "repo": repo, "known": known, "created": time.time(),
    }
    name = f"{os.getpid()}.{secrets.token_hex(8)}" if scoped else str(os.getpid())
    path = folder / f"{name}.json"
    try:
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".lease.", suffix=".tmp", dir=folder)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(data, handle)
            os.replace(temporary, path)
        except BaseException:
            try:
                os.unlink(temporary)
            except OSError:
                pass
            raise
    except OSError:
        log.warning("could not record the speech model in use", exc_info=True)
        return None
    # Scoped leases are released by their caller (and are ignored once the
    # pid is gone), so only the long-lived one needs an exit hook.
    if not scoped and path not in _registered:
        _registered.add(path)
        atexit.register(release, path)
    return path


def release(path: Path) -> None:
    """Remove a lease this process wrote (at exit)."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
        if data.get("pid") == os.getpid():
            Path(path).unlink()
    except (OSError, ValueError, AttributeError):
        pass


def pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    if pid == os.getpid():
        return True
    if os.name == "nt":
        from localflow.status import _windows_pid_is_alive

        return _windows_pid_is_alive(pid)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists, owned by someone else
    except OSError:
        return True  # can't tell: assume alive (protects more, deletes less)
    return True


def live_leases(root: Path | None = None, alive=pid_alive) -> list[Lease]:
    """Leases of processes that are still running. Never raises."""
    folder = Path(root) if root is not None else lease_dir()
    try:
        entries = sorted(folder.iterdir())[: MAX_LEASES * 2]
    except OSError:
        return []
    found = []
    for path in entries:
        if len(found) >= MAX_LEASES:
            break
        match = _LEASE_NAME.fullmatch(path.name)
        if match is None:
            continue
        pid = int(match.group(1))
        if not alive(pid):
            continue
        found.append(_read(path, pid))
    return found


def _read(path: Path, pid: int) -> Lease:
    unknown = Lease(pid, "unknown", None, False)
    try:
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_LEASE_BYTES:
            return unknown
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return unknown
    if (
        not isinstance(data, dict) or data.get("version") != VERSION
        or data.get("pid") != pid or not isinstance(data.get("backend"), str)
        or not isinstance(data.get("known"), bool)
        or not (data.get("repo") is None or isinstance(data.get("repo"), str))
    ):
        return unknown
    return Lease(pid, data["backend"], data["repo"], data["known"])
