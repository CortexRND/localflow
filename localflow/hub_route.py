"""Page routing for the single LocalFlow Hub instance (AppKit-free).

LaunchServices does not deliver new `open --args` arguments to an app that is
already running, so a caller that wants the running Hub on a specific page
leaves a small request file and the app consumes it from a run-loop timer:

    caller  -> write_request("Hot Phrases")      (atomic: tmp + os.replace)
    app     -> consume_request() every ~0.25 s   (atomic claim, then delete)

The page is checked against a whitelist on both sides, the file is never
evaluated (plain JSON, size capped), it is removed when consumed and a
request older than STALE_SECONDS is dropped, so a leftover file can never
steer a later, unrelated launch. Every failure degrades to "no request".
"""

import json
import os
import time
from pathlib import Path

from localflow.singleinstance import lock_dir

HUB_PAGES = (
    "Home", "Hot Phrases", "Dictionary", "Prompts",
    "Meetings", "History", "Style", "Models",
)
DEFAULT_PAGE = "Home"
REQUEST_NAME = "page-request.json"
STALE_SECONDS = 20.0
_FUTURE_SKEW = 5.0
_MAX_BYTES = 4096


def validate_page(page) -> str | None:
    """The canonical page name, or None when `page` is not a Hub page."""
    if not isinstance(page, str):
        return None
    page = page.strip()
    return page if page in HUB_PAGES else None


def request_path() -> Path:
    return lock_dir() / REQUEST_NAME


def write_request(page, now=time.time) -> bool:
    """Atomically leave a request for the running app. False on any failure."""
    page = validate_page(page)
    if page is None:
        return False
    path = request_path()
    tmp = path.with_name(f"{REQUEST_NAME}.{os.getpid()}.tmp")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = json.dumps({"page": page, "ts": now()}).encode()
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
        os.replace(tmp, path)
    except OSError:
        try:
            tmp.unlink()
        except OSError:
            pass
        return False
    return True


def consume_request(now=time.time) -> str | None:
    """Take (and delete) the pending request; None when absent, stale or bad.

    The file is first renamed to a per-process claim name, so when several
    consumers race exactly one of them gets the request.
    """
    path = request_path()
    claim = path.with_name(f"{REQUEST_NAME}.{os.getpid()}.claim")
    try:
        os.replace(path, claim)
    except OSError:
        return None
    try:
        with claim.open("rb") as handle:
            raw = handle.read(_MAX_BYTES)
        data = json.loads(raw)
    except (OSError, ValueError):
        data = None
    finally:
        try:
            claim.unlink()
        except OSError:
            pass
    if not isinstance(data, dict):
        return None
    stamp = data.get("ts")
    if isinstance(stamp, bool) or not isinstance(stamp, (int, float)):
        return None
    age = now() - stamp
    if age > STALE_SECONDS or age < -_FUTURE_SKEW:
        return None
    return validate_page(data.get("page"))


def clear_request() -> None:
    try:
        request_path().unlink()
    except OSError:
        pass
