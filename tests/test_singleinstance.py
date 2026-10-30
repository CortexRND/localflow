import logging
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

from localflow import singleinstance as SI
from localflow.log import setup_logging

REPO_ROOT = Path(__file__).resolve().parents[1]

POSIX_LOCK_TEST = pytest.mark.skipif(
    sys.platform == "win32" or os.environ.get("LOCALFLOW_PLATFORM") == "win32",
    reason="POSIX file locking tests are not supported on Windows",
)


@pytest.fixture(autouse=True)
def home_in_tmp(tmp_path, monkeypatch):
    """Point HOME at tmp_path so lock files never touch the real ~/.localflow."""
    monkeypatch.setenv("HOME", str(tmp_path))
    return tmp_path


def _child_env(tmp_path: Path) -> dict:
    env = os.environ.copy()
    env["HOME"] = str(tmp_path)
    existing_path = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = os.pathsep.join(
        p for p in [str(REPO_ROOT), existing_path] if p
    )
    return env


def _spawn_acquire(tmp_path: Path, name: str, hold_seconds: float = 0.0) -> subprocess.Popen:
    """Spawn a child process that tries to acquire the named lock, prints
    ACQUIRED or FAILED, then (if it acquired) sleeps for hold_seconds before
    exiting — releasing the lock only when it dies."""
    code = (
        "import sys, time\n"
        "from localflow import singleinstance as SI\n"
        f"lock = SI.acquire({name!r})\n"
        "if lock is None:\n"
        "    print('FAILED', flush=True)\n"
        "else:\n"
        "    print('ACQUIRED', flush=True)\n"
        f"    time.sleep({hold_seconds})\n"
    )
    return subprocess.Popen(
        [sys.executable, "-c", code],
        env=_child_env(tmp_path),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


@POSIX_LOCK_TEST
def test_acquire_returns_handle_and_writes_pid_timestamp(tmp_path):
    lock = SI.acquire("test")
    try:
        assert lock is not None
        path = SI.lock_file_path("test")
        assert path == tmp_path / ".localflow" / "test.lock"
        content = path.read_text().strip()
        pid_str, _, ts = content.partition(" ")
        assert int(pid_str) == os.getpid()
        assert ts  # ISO timestamp present
    finally:
        lock.close()


@POSIX_LOCK_TEST
def test_read_holder_reflects_written_pid_and_timestamp(tmp_path):
    lock = SI.acquire("test")
    try:
        pid, ts = SI.read_holder("test")
        assert pid == os.getpid()
        assert ts is not None
    finally:
        lock.close()


@POSIX_LOCK_TEST
def test_second_acquire_fails_in_child_process(tmp_path):
    """The real incident: two separate OS processes both grabbing the lock.
    flock is associated with the open file description, so a genuine
    cross-process contention check needs a real second process."""
    lock = SI.acquire("test")
    assert lock is not None
    try:
        proc = _spawn_acquire(tmp_path, "test")
        out, err = proc.communicate(timeout=10)
        assert proc.returncode == 0, err
        assert out.strip() == "FAILED"
    finally:
        lock.close()


@POSIX_LOCK_TEST
def test_acquire_succeeds_after_release(tmp_path):
    lock = SI.acquire("test")
    assert lock is not None
    lock.close()

    proc = _spawn_acquire(tmp_path, "test")
    out, err = proc.communicate(timeout=10)
    assert proc.returncode == 0, err
    assert out.strip() == "ACQUIRED"


@POSIX_LOCK_TEST
def test_stale_lock_releases_on_holder_death(tmp_path):
    """Mirrors the Jul 30 incident: a holder process dies (killed, not
    graceful exit) without explicit cleanup. flock must release the lock
    anyway since it's held by the kernel for the fd's lifetime, not by any
    pidfile bookkeeping."""
    holder = _spawn_acquire(tmp_path, "test", hold_seconds=5)
    # Wait for the child to actually acquire before racing it.
    line = holder.stdout.readline().strip()
    assert line == "ACQUIRED"

    # While the holder is alive, a fresh acquire attempt must fail.
    contender = SI.acquire("test")
    assert contender is None

    # Simulate a crash (SIGKILL, no graceful shutdown / lock release code runs).
    holder.send_signal(signal.SIGKILL)
    holder.wait(timeout=10)

    # The OS releases flock automatically on process death — no stale-pid
    # cleanup logic needed. Poll briefly since kernel cleanup on kill isn't
    # instantaneous with respect to this process observing it.
    deadline = time.monotonic() + 5
    lock = None
    while time.monotonic() < deadline:
        lock = SI.acquire("test")
        if lock is not None:
            break
        time.sleep(0.05)
    assert lock is not None, "lock was not released after holder was killed"
    lock.close()


def test_setup_logging_is_idempotent(tmp_path, monkeypatch):
    log_path = tmp_path / "localflow.log"
    monkeypatch.setattr("localflow.log.LOG_PATH", log_path)

    logger = logging.getLogger("localflow")
    for h in list(logger.handlers):
        logger.removeHandler(h)
        h.close()

    try:
        first_path = setup_logging()
        second_path = setup_logging()

        assert first_path == second_path == log_path
        file_handlers = [
            h for h in logger.handlers if isinstance(h, logging.FileHandler)
        ]
        assert len(file_handlers) == 1
    finally:
        for h in list(logger.handlers):
            logger.removeHandler(h)
            h.close()
