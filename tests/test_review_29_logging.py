"""PR29 review finding 6: the server must not share dictation's rotating log.

Dictation (`localflow.app`) and the local server (`localflow.server`, started
by the app as its own process) both call `log.setup_logging()`, so two
processes hold independent `RotatingFileHandler`s on the same
`~/.localflow/localflow.log`. Each decides on its own when to roll over: the
first to roll renames the file under the other, which keeps writing into the
backup (POSIX) or fails the rename (Windows). The approved minimal fix is a
separate `server.log` for the server; `localflow.log` stays dictation's.

The intended API (pick the exact spelling when implementing, then keep these
names): `log.setup_logging(path=None)` with `None` meaning `log.LOG_PATH`,
`log.SERVER_LOG_PATH` (~/.localflow/server.log), and `server.log` plus its
backups listed in `logfiles.runtime_log_names()`.

Only tmp_path is written; `localflow.server` is never imported (it would load
config, an API token and FastAPI at import time).
"""

import ast
import logging
import logging.handlers
import os
from pathlib import Path

import pytest

from localflow import log as log_module
from localflow import logfiles

ROOT = Path(__file__).resolve().parents[1]
posix_only = pytest.mark.skipif(os.name == "nt", reason="POSIX file semantics")


def _lines(count, prefix="line"):
    return b"".join(
        f"{prefix} {n:08d} xxxxxxxxxxxxxxxxxxxxxxxx\n".encode() for n in range(count)
    )


@pytest.fixture
def home(tmp_path, monkeypatch):
    folder = tmp_path / "home" / ".localflow"
    folder.mkdir(parents=True)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("USERPROFILE", str(tmp_path / "home"))  # Path.home() on Windows
    monkeypatch.setattr(log_module, "LOG_PATH", folder / "localflow.log")
    monkeypatch.setattr(log_module, "SERVER_LOG_PATH", folder / "server.log", raising=False)
    return folder


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


def file_handlers(logger):
    return [h for h in logger.handlers if isinstance(h, logging.FileHandler)]


def process_handler(logger, *args):
    """setup_logging as a fresh process would run it; returns its handler,
    detached so the next "process" starts from an empty logger again."""
    log_module.setup_logging(*args)
    [handler] = file_handlers(logger)
    logger.removeHandler(handler)
    return handler


def server_setup_args():
    """The argument list server.py passes to its module-level setup_logging()."""
    tree = ast.parse((ROOT / "localflow" / "server.py").read_text(encoding="utf-8"))
    calls = [
        node.value for node in tree.body
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
        and isinstance(node.value.func, ast.Name) and node.value.func.id == "setup_logging"
    ]
    assert len(calls) == 1, "server.py should set logging up exactly once at import"
    call = calls[0]
    namespace = dict(vars(log_module))
    return [
        eval(compile(ast.Expression(arg), "server.py", "eval"), namespace)
        for arg in call.args
    ] + [
        eval(compile(ast.Expression(kw.value), "server.py", "eval"), namespace)
        for kw in call.keywords
    ]


def record(text):
    return logging.LogRecord("localflow.test", logging.INFO, __file__, 1, text, None, None)


# ------------------------------------------------------------- the race ---


def test_dictation_and_server_processes_log_to_different_files(home, clean_logger):
    dictation = process_handler(clean_logger)
    server = process_handler(clean_logger, *server_setup_args())
    try:
        assert Path(dictation.baseFilename).name == "localflow.log"
        assert Path(server.baseFilename).name == "server.log"
        assert dictation.baseFilename != server.baseFilename
    finally:
        dictation.close()
        server.close()


@posix_only
def test_server_rollover_never_moves_dictations_open_file(home, clean_logger):
    """Two handlers on one path (today) lose dictation's newest lines into a
    backup when the server rolls over; separate files keep each intact."""
    dictation = process_handler(clean_logger)
    server = process_handler(clean_logger, *server_setup_args())
    server.maxBytes = 2000  # make the server roll over quickly
    try:
        for n in range(200):
            server.handle(record(f"server {n:04d} " + "s" * 40))
            dictation.handle(record(f"dictation {n:04d}"))
        dictation.flush()
        text = Path(dictation.baseFilename).read_text(encoding="utf-8")
        assert "dictation 0199" in text  # the newest line is in localflow.log
        assert "dictation 0000" in text  # and nothing was rotated away under it
        assert "server " not in text
    finally:
        dictation.close()
        server.close()


# ---------------------------------------------------------------- the API ---


def test_default_target_is_still_localflow_log(home, clean_logger):
    assert log_module.setup_logging() == home / "localflow.log"
    [handler] = file_handlers(clean_logger)
    assert Path(handler.baseFilename) == home / "localflow.log"


def test_server_log_path_lives_beside_localflow_log():
    assert log_module.SERVER_LOG_PATH.name == "server.log"
    assert log_module.SERVER_LOG_PATH.parent == log_module.LOG_PATH.parent


def test_server_target_rotates_like_localflow_log(home, clean_logger):
    path = log_module.setup_logging(log_module.SERVER_LOG_PATH)

    assert path == home / "server.log"
    [handler] = file_handlers(clean_logger)
    assert isinstance(handler, logging.handlers.RotatingFileHandler)
    assert Path(handler.baseFilename) == home / "server.log"
    assert (handler.maxBytes, handler.backupCount) == (
        logfiles.MAX_LOG_BYTES, logfiles.LOG_BACKUPS,
    )
    assert not (home / "localflow.log").exists()  # dictation's file is untouched


def test_repeated_server_setup_adds_no_second_handler(home, clean_logger):
    first = log_module.setup_logging(log_module.SERVER_LOG_PATH)
    second = log_module.setup_logging(log_module.SERVER_LOG_PATH)

    assert first == second == home / "server.log"
    assert len(file_handlers(clean_logger)) == 1


def test_oversized_server_log_is_trimmed_before_it_is_opened(home, clean_logger):
    path = home / "server.log"
    path.write_bytes(_lines(200_000))  # ~8 MB from before the split

    log_module.setup_logging(log_module.SERVER_LOG_PATH)

    assert path.stat().st_size <= logfiles.KEEP_BYTES


# --------------------------------------------------- server integration ---


def test_server_startup_selects_server_log():
    args = server_setup_args()
    assert args, "server.py must pass its own log target to setup_logging"
    assert [Path(a).name for a in args] == ["server.log"]


def test_dictation_startup_keeps_the_default_target():
    tree = ast.parse((ROOT / "localflow" / "app.py").read_text(encoding="utf-8"))
    calls = [
        node for node in ast.walk(tree)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name) and node.func.id == "setup_logging"
    ]
    assert len(calls) == 1
    assert calls[0].args == [] and calls[0].keywords == []


def test_server_still_trims_logs_at_startup():
    text = (ROOT / "localflow" / "server.py").read_text(encoding="utf-8")
    assert "prepare_process_logs()" in text


# -------------------------------------------------------- startup trims ---


def test_runtime_logs_include_server_log_and_its_backups():
    names = logfiles.runtime_log_names()
    expected = ["server.log", *(f"server.log.{n}" for n in range(1, logfiles.LOG_BACKUPS + 1))]
    for name in expected:
        assert name in names
    assert "localflow.log" in names and "app.log" in names  # nothing dropped


def test_startup_trim_covers_server_log_and_backups(home):
    names = ["server.log", "server.log.1", "server.log.3", "localflow.log", "unrelated.log"]
    for name in names:
        (home / name).write_bytes(_lines(3000))

    trimmed = logfiles.trim_runtime_logs(home, max_bytes=20_000, keep_bytes=10_000)

    assert sorted(p.name for p in trimmed) == sorted(names[:-1])
    assert (home / "unrelated.log").stat().st_size > 20_000


@posix_only
def test_server_log_symlink_is_never_followed(home, tmp_path):
    outside = tmp_path / "elsewhere.log"
    outside.write_bytes(_lines(3000))
    before = outside.read_bytes()
    (home / "server.log").symlink_to(outside)
    (home / "server.log.1").symlink_to(outside)

    trimmed = logfiles.trim_runtime_logs(home, max_bytes=20_000, keep_bytes=10_000)

    assert trimmed == []
    assert outside.read_bytes() == before
