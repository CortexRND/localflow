import logging
from pathlib import Path

LOG_PATH = Path.home() / ".localflow" / "localflow.log"


def setup_logging() -> Path:
    """File logger at ~/.localflow/localflow.log; console keeps the terse prints.

    Idempotent: calling this more than once in a process (e.g. a caller that
    re-inits logging after a retry) must not stack a second FileHandler,
    which would double every log line.
    """
    root = logging.getLogger("localflow")
    root.setLevel(logging.DEBUG)

    if not any(isinstance(h, logging.FileHandler) for h in root.handlers):
        LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
        handler = logging.FileHandler(LOG_PATH)
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
        )
        root.addHandler(handler)

    return LOG_PATH
