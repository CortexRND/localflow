"""Open the web UI without putting the long-lived API token in a URL.

URLs handed to a browser or xdg-open show up in other users' process lists
and in browser history, so callers ask the server for a short-lived,
single-use launch code and open ``<path>?launch=<code>`` instead.
"""

import webbrowser
from collections.abc import Callable
from urllib.parse import quote

import requests

from localflow.apitoken import load


def launch_url(base: str, path: str = "/settings") -> str:
    url = base.rstrip("/") + path
    token = load()
    if not token:
        return url
    try:
        response = requests.post(
            base.rstrip("/") + "/api/launch-code",
            headers={"X-Localflow-Token": token},
            timeout=3,
        )
        response.raise_for_status()
        code = response.json()["code"]
    except (requests.RequestException, ValueError, KeyError, TypeError):
        return url
    if not isinstance(code, str) or not code:
        return url
    return f"{url}?launch={quote(code)}"


def open_web_ui(
    base: str, path: str = "/settings", opener: Callable[[str], object] = webbrowser.open
) -> None:
    opener(launch_url(base, path))
