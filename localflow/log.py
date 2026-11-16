import logging
import logging.handlers
import os
from pathlib import Path

from localflow.logfiles import KEEP_BYTES, LOG_BACKUPS, MAX_LOG_BYTES, trim_in_place

LOG_PATH = Path.home() / ".localflow" / "localflow.log"
# The local server is its own process: it gets its own rotating file, because
# two processes rotating one file rename it under each other.
SERVER_LOG_PATH = LOG_PATH.with_name("server.log")


def setup_logging(path: Path | None = None) -> Path:
    """File logger at `path` (default ~/.localflow/localflow.log); console
    keeps the terse prints.

    The file rotates at MAX_LOG_BYTES (5 MiB) keeping LOG_BACKUPS (3) old
    copies. A file that grew past the cap before rotation existed is first
    trimmed in place to its newest part, so the first rollover never turns a
    huge file into an equally huge backup.

    Idempotent per target: calling this more than once in a process for the
    same file (e.g. a caller that re-inits logging after a retry) must not
    stack a second FileHandler, which would double every log line.
    """
    target = LOG_PATH if path is None else Path(path)
    root = logging.getLogger("localflow")
    root.setLevel(logging.DEBUG)

    wanted = os.path.abspath(target)
    if not any(
        isinstance(h, logging.FileHandler) and h.baseFilename == wanted
        for h in root.handlers
    ):
        target.parent.mkdir(parents=True, exist_ok=True)
        trim_in_place(target, MAX_LOG_BYTES, KEEP_BYTES, root=target.parent)
        handler = logging.handlers.RotatingFileHandler(
            target, maxBytes=MAX_LOG_BYTES, backupCount=LOG_BACKUPS,
            encoding="utf-8",
        )
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
        )
        root.addHandler(handler)

    return target
