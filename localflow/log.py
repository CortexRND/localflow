import logging
import logging.handlers
from pathlib import Path

from localflow.logfiles import KEEP_BYTES, LOG_BACKUPS, MAX_LOG_BYTES, trim_in_place

LOG_PATH = Path.home() / ".localflow" / "localflow.log"


def setup_logging() -> Path:
    """File logger at ~/.localflow/localflow.log; console keeps the terse prints.

    The file rotates at MAX_LOG_BYTES (5 MiB) keeping LOG_BACKUPS (3) old
    copies. A file that grew past the cap before rotation existed is first
    trimmed in place to its newest part, so the first rollover never turns a
    huge file into an equally huge backup.

    Idempotent: calling this more than once in a process (e.g. a caller that
    re-inits logging after a retry) must not stack a second FileHandler,
    which would double every log line.
    """
    root = logging.getLogger("localflow")
    root.setLevel(logging.DEBUG)

    if not any(isinstance(h, logging.FileHandler) for h in root.handlers):
        LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
        trim_in_place(LOG_PATH, MAX_LOG_BYTES, KEEP_BYTES, root=LOG_PATH.parent)
        handler = logging.handlers.RotatingFileHandler(
            LOG_PATH, maxBytes=MAX_LOG_BYTES, backupCount=LOG_BACKUPS,
            encoding="utf-8",
        )
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
        )
        root.addHandler(handler)

    return LOG_PATH
