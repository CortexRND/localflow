"""Single-instance guard so two `lf dictate` processes never fight over the
hotkey and mic at once (see incident: a stale Jul 30 process plus a fresh
Aug 2 process both grabbed the mic and transcriptions silently degraded to
0 chars).

Uses an OS-native advisory lock rather than a pidfile-existence check: locks are
held by the kernel for the life of the holding process's file descriptor,
so a crashed/killed holder releases the lock automatically — no stale-pid
cleanup logic needed. A pidfile-existence check would have to guess whether
a pid found in the file is still alive (and isn't a reused pid), which is
exactly the class of bug that let the Jul 30 process go undetected.
"""

import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from localflow.platform import current


def lock_dir() -> Path:
    """Directory holding lock files. Computed at call time (not module import
    time) so tests can point HOME at a tmp_path and have it take effect."""
    return Path.home() / ".localflow"


class SingleInstance:
    """Holds an open, flock'd file descriptor for the process lifetime.

    The lock is released when this object is closed/GC'd (its fd closes) or
    when the holding process dies for any reason, since flock locks do not
    survive process death.
    """

    def __init__(self, path: Path, fd: int) -> None:
        self.path = path
        self._fd = fd

    def release(self) -> None:
        """Explicitly release the lock and close the fd. Idempotent."""
        if self._fd is not None:
            try:
                current().unlock(self._fd)
            except OSError:
                pass
            try:
                os.close(self._fd)
            except OSError:
                pass
            self._fd = None

    def close(self) -> None:
        self.release()

    def __del__(self) -> None:
        self.release()

    def __enter__(self) -> "SingleInstance":
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.release()


def _lock_path(name: str) -> Path:
    return lock_dir() / f"{name}.lock"


def _read_holder_info(path: Path) -> tuple[Optional[int], Optional[str]]:
    """Best-effort read of the pid/timestamp left by the current lock holder."""
    try:
        content = path.read_text().strip()
    except OSError:
        return None, None
    pid_str, _, ts_str = content.partition(" ")
    try:
        pid = int(pid_str)
    except ValueError:
        pid = None
    return pid, (ts_str or None)


def acquire(name: str = "localflow") -> Optional[SingleInstance]:
    """Try to become the single running instance identified by `name`.

    On success, writes "<pid> <iso-timestamp>" into the lock file and
    returns a SingleInstance holding the lock for the process lifetime.

    On failure (another live process already holds the lock), returns None.
    Callers can then inspect the lock file directly (path from
    `lock_file_path(name)`) to report who holds it.
    """
    lock_dir().mkdir(parents=True, exist_ok=True)
    path = _lock_path(name)

    fd = os.open(str(path), os.O_CREAT | os.O_RDWR, 0o644)
    if not current().try_lock(fd):
        os.close(fd)
        return None

    pid = os.getpid()
    timestamp = datetime.now(timezone.utc).isoformat()
    try:
        os.ftruncate(fd, 0)
        os.lseek(fd, 0, os.SEEK_SET)
        os.write(fd, f"{pid} {timestamp}\n".encode())
    except OSError:
        # Lock is held either way; a write failure shouldn't drop it.
        pass

    return SingleInstance(path, fd)


def lock_file_path(name: str = "localflow") -> Path:
    return _lock_path(name)


def read_holder(name: str = "localflow") -> tuple[Optional[int], Optional[str]]:
    """Read (pid, timestamp) of whoever currently holds (or last held) the
    lock file, for use in error messages when acquire() returns None."""
    return _read_holder_info(_lock_path(name))
