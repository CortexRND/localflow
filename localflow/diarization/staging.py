"""Staged enrollment audio: the short-lived seekable copy of an M4A/MP4 clip (slice B9).

The copy lives in the voiceprint folder (``~/.localflow/speakers``, 0700, design §4) as
``.enroll-<32 hex>.audio`` (0600) and is deleted as soon as ffmpeg exits. A killed or crashed
server can't run that cleanup, so the server sweeps leftovers at startup and every
``forget --all`` sweeps them too. Only regular files with exactly that name are removed:
nothing else in the folder, no symlink (or its target), no directory.

Standard library only, and independent of the registry, so the startup sweep imports no
registry or model code and creates nothing (a missing folder is simply "nothing to do").
"""

from __future__ import annotations

import logging
import os
import re
import stat
import uuid
from pathlib import Path

log = logging.getLogger("localflow.diarization")

_STAGED_RE = re.compile(r"\.enroll-[0-9a-f]{32}\.audio")


def default_root() -> Path:
    """``~/.localflow/speakers`` (the voiceprint folder), resolved at call time."""
    return Path.home() / ".localflow" / "speakers"


def staged_name() -> str:
    """A fresh staging file name."""
    return f".enroll-{uuid.uuid4().hex}.audio"


def is_staged_name(name: object) -> bool:
    return isinstance(name, str) and _STAGED_RE.fullmatch(name) is not None


def plain_dir_or_missing(root: Path | str) -> bool:
    """True when ``root`` is absent or a real directory (not a symlink, not a file).
    Raises OSError (other than FileNotFoundError) when ``root`` can't be examined."""
    try:
        return stat.S_ISDIR(os.lstat(root).st_mode)
    except FileNotFoundError:
        return True


def sweep(root: Path | str) -> tuple[int, int]:
    """Delete abandoned staged enrollment audio in ``root``; ``(removed, failed)``.

    Never creates ``root``, never raises, never logs a path or file name. A ``root`` that is
    a symlink or not a directory is not ours to clean and is skipped (nothing is staged in
    one either).
    """
    removed = failed = 0
    try:
        if not plain_dir_or_missing(root):
            log.warning("skipped the enrollment audio check: the speakers folder is not a "
                        "plain folder")
            return 0, 0
        names = os.listdir(root)
    except (FileNotFoundError, NotADirectoryError):
        return 0, 0
    except OSError:
        log.warning("could not check for abandoned enrollment audio")
        return 0, 1
    for name in names:
        if not is_staged_name(name):
            continue
        path = Path(root) / name
        try:
            if not stat.S_ISREG(os.lstat(path).st_mode):
                continue  # a symlink or directory is not something this module wrote
            path.unlink()
            removed += 1
        except FileNotFoundError:
            pass
        except OSError:
            failed += 1
    if failed:
        log.warning("could not delete %d abandoned enrollment audio files", failed)
    elif removed:
        log.info("deleted %d abandoned enrollment audio files", removed)
    return removed, failed
