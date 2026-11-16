"""Explicit "load this speech model again" requests from the Hub to the app.

Picking the speech model that is already saved changes nothing in the config
file, so the running dictation app (another process) cannot tell a re-pick
from any other save. `models_layout.set_speech` therefore writes a small
request next to the config file - `<config name>.speech-retry.json`, scoped
to `config_path()` so LOCALFLOW_CONFIG keeps tests and profiles apart - and
the app's config watcher consumes it on its next tick (`RetryRequests.take`).

The request holds metadata only (a fresh random id and the speech selection,
STT_FIELDS); never credentials. Each re-pick gets a new id, so picking the
same model twice makes two requests; each is taken once per reader. A request
already present when a reader starts belongs to an earlier process and is
ignored. The file is written atomically (temporary file + os.replace).
"""

from __future__ import annotations

import json
import logging
import os
import secrets
import tempfile
from collections.abc import Callable
from pathlib import Path

from localflow.config import config_path
from localflow.live_config import STT_FIELDS

log = logging.getLogger("localflow.speech_retry")

VERSION = 1
MAX_BYTES = 4096


def request_path(config_file: Path | None = None) -> Path:
    """The retry request file for `config_file` (default: `config_path()`)."""
    target = Path(config_file) if config_file is not None else config_path()
    return target.with_name(f"{target.name}.speech-retry.json")


def request_retry(config, config_file: Path | None = None) -> Path:
    """Ask the running app to load `config`'s speech model again (if it failed)."""
    target = request_path(config_file)
    data = {
        "version": VERSION,
        "id": secrets.token_hex(16),
        "selection": {name: getattr(config, name) for name in STT_FIELDS},
    }
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", dir=target.parent, prefix=f".{target.name}.",
            suffix=".tmp", delete=False,
        ) as handle:
            temporary = Path(handle.name)
            json.dump(data, handle)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, target)
        temporary = None
    finally:
        if temporary is not None and temporary.exists():
            temporary.unlink()
    return target


def _read(path: Path) -> tuple[str, tuple] | None:
    """(id, selection) of a well-formed request at `path`, else None."""
    try:
        with path.open("rb") as handle:
            raw = handle.read(MAX_BYTES + 1)
    except OSError:
        return None
    if len(raw) > MAX_BYTES:
        return None
    try:
        data = json.loads(raw.decode("utf-8"))
    except ValueError:
        return None
    if not isinstance(data, dict) or data.get("version") != VERSION:
        return None
    request_id, selection = data.get("id"), data.get("selection")
    if not isinstance(request_id, str) or not request_id or not isinstance(selection, dict):
        return None
    values = []
    for name in STT_FIELDS:
        if name not in selection:
            return None
        value = selection[name]
        if value is not None and not isinstance(value, str):
            return None
        values.append(value)
    return request_id, tuple(values)


class RetryRequests:
    """Takes each new retry request once; never raises.

    `take()` returns the requested speech selection (values of STT_FIELDS, in
    order) the first time a request is seen, else None. A request present
    when this reader is created (an earlier process's) is never returned.
    """

    def __init__(self, config_file: Path | Callable[[], Path] | None = None):
        if callable(config_file):
            self._path = lambda: request_path(config_file())
        else:
            self._path = lambda: request_path(config_file)
        self._seen = self._current_id()

    def _current_id(self) -> str | None:
        try:
            found = _read(self._path())
        except Exception:  # noqa: BLE001 - a bad path must not stop the app
            return None
        return found[0] if found is not None else None

    def take(self) -> tuple | None:
        try:
            found = _read(self._path())
        except Exception:
            log.exception("could not read the speech retry request")
            return None
        if found is None or found[0] == self._seen:
            return None
        self._seen = found[0]
        return found[1]
