"""Bounded logs: rotation for localflow.log, in-place trims for the rest."""

import logging
import logging.handlers
import os

import pytest

from localflow import launchagent, logfiles
from localflow import log as log_module

posix_only = pytest.mark.skipif(os.name == "nt", reason="POSIX file semantics")


def _lines(count, prefix="line"):
    return b"".join(f"{prefix} {n:08d} xxxxxxxxxxxxxxxxxxxxxxxx\n".encode() for n in range(count))


@pytest.fixture
def root(tmp_path, monkeypatch):
    home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(home))
    folder = home / ".localflow"
    folder.mkdir(parents=True)
    return folder


def test_oversized_file_keeps_newest_lines_and_its_inode(root):
    path = root / "agent.err.log"
    data = _lines(200_000)  # ~8 MB
    path.write_bytes(data)
    inode = path.stat().st_ino

    assert logfiles.trim_in_place(path, root=root) is True

    kept = path.read_bytes()
    assert len(kept) <= logfiles.KEEP_BYTES
    assert data.endswith(kept)                      # the newest bytes
    assert kept.startswith(b"line ")                # starts on a line boundary
    assert path.stat().st_ino == inode              # same file for open writers


def test_trim_streams_in_chunks(root, monkeypatch):
    path = root / "app.log"
    path.write_bytes(_lines(40_000))
    sizes = []
    real = logfiles._pread
    monkeypatch.setattr(logfiles, "CHUNK_BYTES", 4096)
    monkeypatch.setattr(logfiles, "_pread", lambda fd, n, off: sizes.append(n) or real(fd, n, off))

    assert logfiles.trim_in_place(path, 100_000, 50_000, root=root)
    assert max(sizes) <= 64 * 1024  # never the whole file in memory
    assert len(path.read_bytes()) <= 50_000


def test_small_files_are_left_alone(root):
    path = root / "agent.out.log"
    path.write_bytes(b"hello\n")
    stamp = path.stat().st_mtime_ns
    assert logfiles.trim_in_place(path, root=root) is False
    assert path.read_bytes() == b"hello\n" and path.stat().st_mtime_ns == stamp
    assert logfiles.trim_in_place(root / "missing.log", root=root) is False


@posix_only
def test_symlinks_hardlinks_and_outside_files_are_never_touched(root, tmp_path):
    outside = tmp_path / "elsewhere.log"
    outside.write_bytes(_lines(1000))
    big = outside.read_bytes()
    link = root / "agent.out.log"
    link.symlink_to(outside)
    hard = root / "agent.err.log"
    os.link(outside, hard)

    assert logfiles.trim_in_place(link, 1000, 500, root=root) is False
    assert logfiles.trim_in_place(hard, 1000, 500, root=root) is False
    assert logfiles.trim_in_place(outside, 1000, 500, root=root) is False
    assert outside.read_bytes() == big


@posix_only
def test_append_writer_keeps_writing_at_the_new_end(root):
    path = root / "app.log"
    path.write_bytes(_lines(5000))
    with open(path, "ab") as writer:  # like the server's inherited stdout
        logfiles.trim_in_place(path, 10_000, 5_000, root=root)
        writer.write(b"after trim\n")
        writer.flush()
    data = path.read_bytes()
    assert data.endswith(b"after trim\n")
    assert len(data) <= 5_000 + len(b"after trim\n")
    assert b"\0" not in data  # no hole from a stale write offset


@posix_only
def test_redirected_stdout_is_made_append_only(root):
    import fcntl

    path = root / "agent.out.log"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT, 0o600)  # launchd-style, no O_APPEND
    try:
        assert not fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_APPEND
        assert logfiles.ensure_append(fd) is True
        os.write(fd, _lines(5000))
        logfiles.trim_in_place(path, 10_000, 5_000, root=root)
        os.write(fd, b"later\n")
    finally:
        os.close(fd)
    data = path.read_bytes()
    assert data.endswith(b"later\n") and b"\0" not in data
    read, write = os.pipe()
    try:
        assert logfiles.ensure_append(write) is False  # not a regular file
    finally:
        os.close(read)
        os.close(write)


def test_runtime_logs_cover_every_localflow_file_only(root):
    names = ["localflow.log", "localflow.log.1", "localflow.log.3", "agent.out.log",
             "agent.err.log", "app.log", "unrelated.log"]
    for name in names:
        (root / name).write_bytes(_lines(3000))
    trimmed = logfiles.trim_runtime_logs(root, max_bytes=20_000, keep_bytes=10_000)
    assert sorted(p.name for p in trimmed) == sorted(names[:-1])
    assert (root / "unrelated.log").stat().st_size > 20_000
    assert logfiles.trim_runtime_logs(root, max_bytes=20_000, keep_bytes=10_000) == []


def test_prepare_process_logs_runs_at_startup(root, monkeypatch):
    fds = []
    # Don't change pytest's own capture descriptors; just see they are offered.
    monkeypatch.setattr(logfiles, "ensure_append", lambda fd: fds.append(fd) or True)
    (root / "agent.err.log").write_bytes(_lines(200_000))
    assert [p.name for p in logfiles.prepare_process_logs()] == ["agent.err.log"]


def test_launchagent_install_trims_its_own_log_files(root):
    (root / "agent.out.log").write_bytes(_lines(200_000))
    (root / "agent.err.log").write_bytes(b"small\n")
    assert [p.name for p in launchagent.trim_agent_logs()] == ["agent.out.log"]
    assert (root / "agent.err.log").read_bytes() == b"small\n"


def test_launchagent_install_calls_the_trim(root, monkeypatch):
    calls = []
    monkeypatch.setattr(launchagent, "trim_agent_logs", lambda: calls.append("trim"))
    monkeypatch.setattr(
        launchagent.subprocess, "run",
        lambda *a, **k: type("R", (), {"returncode": 0, "stderr": ""})(),
    )
    launchagent.install(program="/bin/true")
    assert calls == ["trim"]


def test_app_startup_paths_trim_logs():
    from pathlib import Path

    root = Path(logfiles.__file__).parent
    assert "prepare_process_logs()" in (root / "app.py").read_text(encoding="utf-8")
    assert "prepare_process_logs()" in (root / "server.py").read_text(encoding="utf-8")
    main_app = (root / "main_app.py").read_text(encoding="utf-8")
    assert "_trim_logs()" in main_app.split("def ensure_runtime", 1)[1]


def test_main_app_trims_app_log(root):
    from localflow import main_app

    (root / "app.log").write_bytes(_lines(200_000))
    main_app._trim_logs()
    assert (root / "app.log").stat().st_size <= logfiles.KEEP_BYTES


@pytest.fixture
def clean_logger():
    logger = logging.getLogger("localflow")
    saved = list(logger.handlers)
    for handler in saved:
        logger.removeHandler(handler)
    yield logger
    for handler in list(logger.handlers):
        handler.close()
        logger.removeHandler(handler)
    for handler in saved:
        logger.addHandler(handler)


def test_setup_logging_rotates_with_three_backups(tmp_path, monkeypatch, clean_logger):
    path = tmp_path / "localflow.log"
    path.write_bytes(_lines(200_000))  # oversized from before rotation existed
    monkeypatch.setattr(log_module, "LOG_PATH", path)

    assert log_module.setup_logging() == path
    assert path.stat().st_size <= logfiles.KEEP_BYTES  # trimmed, not backed up whole
    log_module.setup_logging()  # idempotent
    handlers = [h for h in clean_logger.handlers if isinstance(h, logging.FileHandler)]
    assert len(handlers) == 1
    handler = handlers[0]
    assert isinstance(handler, logging.handlers.RotatingFileHandler)
    assert (handler.maxBytes, handler.backupCount) == (5 * 1024 * 1024, 3)

    handler.maxBytes = 4000  # rotate quickly
    for n in range(400):
        clean_logger.info("message %d %s", n, "y" * 40)
    names = sorted(p.name for p in tmp_path.iterdir())
    assert names == ["localflow.log", "localflow.log.1", "localflow.log.2", "localflow.log.3"]
    assert all((tmp_path / n).stat().st_size <= 4200 for n in names)


def test_portable_pread_pwrite_fallbacks(tmp_path, monkeypatch):
    path = tmp_path / "f.bin"
    path.write_bytes(b"0123456789")
    monkeypatch.delattr(os, "pread", raising=False)
    monkeypatch.delattr(os, "pwrite", raising=False)
    fd = os.open(path, os.O_RDWR)
    try:
        assert logfiles._pread(fd, 3, 5) == b"567"
        logfiles._pwrite(fd, b"ab", 1)
    finally:
        os.close(fd)
    assert path.read_bytes() == b"0ab3456789"
