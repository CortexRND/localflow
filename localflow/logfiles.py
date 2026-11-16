"""Keep LocalFlow's own log files bounded.

Python logging rotates `localflow.log` itself (log.py). The other files are
written by someone else's file descriptor and can't be renamed under them:
launchd's StandardOutPath/StandardErrorPath (`agent.out.log`,
`agent.err.log`) and the app-launched server/menu bar output (`app.log`).
Those are trimmed *in place* when oversized: the newest bytes are copied to
the front in fixed-size chunks and the file is truncated, so the inode (and
every writer's open descriptor) survives and memory use stays flat even for
a 100 MB file. Writers opened with O_APPEND continue at the new end.

Only regular files directly inside the LocalFlow data folder are touched:
symlinks, hard-linked files, other owners and anything elsewhere are left
alone.
"""

from __future__ import annotations

import logging
import os
import stat
import sys
from pathlib import Path

log = logging.getLogger("localflow.logfiles")

MAX_LOG_BYTES = 5 * 1024 * 1024
LOG_BACKUPS = 3
# What an oversized file is cut down to: half the cap, so a file that keeps
# growing is not re-trimmed on every start.
KEEP_BYTES = MAX_LOG_BYTES // 2
CHUNK_BYTES = 1024 * 1024
LOGGING_NAME = "localflow.log"
AGENT_LOGS = ("agent.out.log", "agent.err.log")
APP_LOG = "app.log"


def log_dir() -> Path:
    return Path.home() / ".localflow"


def runtime_log_names() -> tuple[str, ...]:
    backups = tuple(f"{LOGGING_NAME}.{n}" for n in range(1, LOG_BACKUPS + 1))
    return (LOGGING_NAME, *backups, *AGENT_LOGS, APP_LOG)


def _pread(fd: int, size: int, offset: int) -> bytes:
    if hasattr(os, "pread"):
        return os.pread(fd, size, offset)
    os.lseek(fd, offset, os.SEEK_SET)  # Windows has no pread
    return os.read(fd, size)


def _pwrite(fd: int, data: bytes, offset: int) -> None:
    view = memoryview(data)
    if not hasattr(os, "pwrite"):
        os.lseek(fd, offset, os.SEEK_SET)
    while view:
        if hasattr(os, "pwrite"):
            done = os.pwrite(fd, view, offset)
            offset += done
        else:
            done = os.write(fd, view)
        view = view[done:]


def trim_in_place(
    path: Path | str,
    max_bytes: int = MAX_LOG_BYTES,
    keep_bytes: int = KEEP_BYTES,
    *,
    root: Path | str | None = None,
) -> bool:
    """Cut `path` down to its newest ~`keep_bytes` if it exceeds `max_bytes`.

    Returns True when the file was trimmed. Never follows a symlink, never
    touches a file outside `root` (default: the LocalFlow data folder), and
    never raises for a missing or unsuitable file.
    """
    path = Path(path)
    keep_bytes = max(0, min(keep_bytes, max_bytes))
    base = Path(root) if root is not None else log_dir()
    try:
        if path.parent.resolve() != base.resolve():
            log.warning("not trimming %s: outside %s", path, base)
            return False
        if path.is_symlink():
            return False
        flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
        fd = os.open(path, flags)
    except OSError:
        return False
    try:
        info = os.fstat(fd)
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_nlink != 1
            or (hasattr(os, "getuid") and info.st_uid != os.getuid())
        ):
            return False
        size = info.st_size
        if size <= max_bytes:
            return False
        start = size - keep_bytes
        # Start on a line boundary when one is near, so the file never begins
        # with half a line.
        head = _pread(fd, min(64 * 1024, keep_bytes), start) if keep_bytes else b""
        newline = head.find(b"\n")
        if 0 <= newline < len(head) - 1:
            start += newline + 1
        written = 0
        offset = start
        while offset < size:
            chunk = _pread(fd, min(CHUNK_BYTES, size - offset), offset)
            if not chunk:
                break
            _pwrite(fd, chunk, written)
            written += len(chunk)
            offset += len(chunk)
        os.ftruncate(fd, written)
        log.info("trimmed %s from %d to %d bytes", path.name, size, written)
        return True
    except OSError as exc:
        log.warning("could not trim %s: %s", path, exc)
        return False
    finally:
        os.close(fd)


def trim_runtime_logs(
    root: Path | str | None = None, max_bytes: int = MAX_LOG_BYTES,
    keep_bytes: int = KEEP_BYTES,
) -> list[Path]:
    """Trim every oversized LocalFlow log file; returns the trimmed paths."""
    base = Path(root) if root is not None else log_dir()
    trimmed = []
    for name in runtime_log_names():
        path = base / name
        if trim_in_place(path, max_bytes, keep_bytes, root=base):
            trimmed.append(path)
    return trimmed


def ensure_append(fd: int) -> bool:
    """Make `fd` (e.g. stdout redirected by launchd) append-only if it is a
    regular file, so an in-place trim doesn't leave it writing past the new
    end. True when O_APPEND is set afterwards."""
    try:
        import fcntl
    except ImportError:  # Windows: no launchd redirection to fix
        return False
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            return False
        flags = fcntl.fcntl(fd, fcntl.F_GETFL)
        if not flags & os.O_APPEND:
            fcntl.fcntl(fd, fcntl.F_SETFL, flags | os.O_APPEND)
        return bool(fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_APPEND)
    except (OSError, ValueError):
        return False


def prepare_process_logs(root: Path | str | None = None) -> list[Path]:
    """At process start: stdout/stderr append-only, then trim oversized logs."""
    for stream in (sys.stdout, sys.stderr):
        try:
            ensure_append(stream.fileno())
        except (AttributeError, OSError, ValueError):
            pass
    return trim_runtime_logs(root)
