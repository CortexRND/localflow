"""Open the web UI without putting a secret in any URL or process argument.

URLs handed to a browser or xdg-open show up in other users' process lists
and in browser history. Callers ask the server for a short-lived,
single-use launch code and write it into a private (0600) HTML file that
POSTs the code to ``<path>``; the opener only ever sees that file's path.
"""

import html
import os
import secrets
import time
import webbrowser
from collections.abc import Callable
from pathlib import Path

import requests

from localflow.apitoken import load
from localflow.config import config_dir

# Matches the server's launch-code TTL; older launch files are dead weight.
LAUNCH_FILE_TTL_SECONDS = 60

_LAUNCH_PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><meta name="referrer" content="no-referrer">
<title>Opening LocalFlow…</title></head>
<body>
<form method="post" action="{action}">
<input type="hidden" name="launch" value="{code}">
<noscript><button type="submit">Open LocalFlow</button></noscript>
</form>
<p id="used" hidden>This launch link was already used — reopen LocalFlow from lf ui or Settings.</p>
<script>
// Submit only on a fresh open. Back, reload or session restore would resend
// an already-used code, which the server treats as theft and rotates the token.
(() => {{
  const showUsed = () => {{ document.getElementById('used').hidden = false; }};
  window.addEventListener('pageshow', (event) => {{ if (event.persisted) showUsed(); }});
  const key = 'localflow-launch-submitted:' + location.pathname;
  const nav = performance.getEntriesByType('navigation')[0];
  let submitted = false;
  try {{ submitted = sessionStorage.getItem(key) === '1'; }} catch (e) {{ /* storage off */ }}
  if ((nav && nav.type !== 'navigate') || submitted) {{ showUsed(); return; }}
  try {{ sessionStorage.setItem(key, '1'); }} catch (e) {{ /* storage off */ }}
  document.forms[0].submit();
}})();
</script>
</body></html>
"""


def launch_dir() -> Path:
    return config_dir() / "launch"


def _launch_code(base: str) -> str | None:
    token = load()
    if not token:
        return None
    try:
        response = requests.post(
            base.rstrip("/") + "/api/launch-code",
            headers={"X-Localflow-Token": token},
            timeout=3,
        )
        response.raise_for_status()
        code = response.json()["code"]
    except (requests.RequestException, ValueError, KeyError, TypeError):
        return None
    return code if isinstance(code, str) and code else None


def _remove_stale_launch_files(directory: Path) -> None:
    cutoff = time.time() - LAUNCH_FILE_TTL_SECONDS
    for old in directory.glob("open-*.html"):
        try:
            if old.stat().st_mtime < cutoff:
                old.unlink()
        except OSError:
            pass


def _write_launch_file(url: str, code: str) -> Path | None:
    directory = launch_dir()
    try:
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(directory, 0o700)
    except OSError:
        return None
    _remove_stale_launch_files(directory)
    path = directory / f"open-{secrets.token_hex(8)}.html"
    page = _LAUNCH_PAGE.format(action=html.escape(url), code=html.escape(code))
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError:
        return None
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(page)
    return path


def launch_target(base: str, path: str = "/settings") -> str:
    """Return what to hand the opener: a private launch file's URI, or the bare URL."""
    url = base.rstrip("/") + path
    code = _launch_code(base)
    if code is None:
        return url
    launch_file = _write_launch_file(url, code)
    return launch_file.as_uri() if launch_file is not None else url


def open_web_ui(
    base: str, path: str = "/settings", opener: Callable[[str], object] = webbrowser.open
) -> None:
    opener(launch_target(base, path))
