"""HTTP client for the local meeting endpoints."""

from pathlib import Path
from urllib.parse import quote

import requests

from localflow.apitoken import load
from localflow.config import load_config
from localflow.server_url import configured_base_url

SERVER_DOWN = (
    "LocalFlow server not running — open LocalFlow.app (it starts the server) "
    "or run `lf serve`"
)


def vault_notes_path(notes_path, vault_path) -> str | None:
    """Return the resolved notes file if it is an existing ``.md`` file inside
    the vault, else None. The path comes from whatever answers on the local
    port, so it must never reach the OS opener unchecked."""
    if not isinstance(notes_path, str) or not notes_path.strip() or not vault_path:
        return None
    try:
        vault = Path(vault_path).expanduser().resolve(strict=True)
        target = Path(notes_path).expanduser().resolve(strict=True)
    except (OSError, RuntimeError, ValueError):
        return None
    if target.suffix.lower() != ".md" or not target.is_file():
        return None
    if not target.is_relative_to(vault):
        return None
    return str(target)


class MeetingClientError(RuntimeError):
    """A meeting request failed or the local server is unavailable."""


class MeetingClient:
    def __init__(
        self,
        base_url: str | None = None,
        token: str | None = None,
        http=requests,
    ):
        if base_url is None:
            base_url = configured_base_url(load_config())
        self.base_url = base_url.rstrip("/")
        self._fixed_token = token
        self.token = token if token is not None else load()
        self.http = http

    def _request(self, method: str, path: str, *, timeout=3, json=None) -> dict:
        if self._fixed_token is None:
            # The server issues a new token at every start; read the current one.
            self.token = load()
        options = {"timeout": timeout}
        if self.token:
            options["headers"] = {"X-Localflow-Token": self.token}
        try:
            response = getattr(self.http, method)(
                f"{self.base_url}{path}",
                **options,
                **({"json": json} if json is not None else {}),
            )
        except requests.RequestException as exc:
            raise MeetingClientError(SERVER_DOWN) from exc
        except Exception as exc:
            # Injected HTTP adapters can expose their own connection exception.
            if isinstance(exc, OSError):
                raise MeetingClientError(SERVER_DOWN) from exc
            raise
        if not 200 <= response.status_code < 300:
            try:
                detail = response.json().get("detail")
            except (ValueError, AttributeError):
                detail = None
            raise MeetingClientError(
                str(detail or getattr(response, "reason", "request failed"))
            )
        if response.status_code == 204:
            return {}
        return response.json()

    def status(self) -> dict:
        return self._request("get", "/meeting/status", timeout=3)

    def templates(self) -> dict:
        return self._request("get", "/meeting/templates")

    def start(self, title, category, template, user_notes, auto_title=None) -> dict:
        """`auto_title`: True = the title is a placeholder to name on save,
        False = the user typed it (kept, even "Meeting"), None = let the
        server decide from the title (older callers)."""
        body = {
            "title": title,
            "category": category,
            "template": template,
            "user_notes": user_notes,
        }
        if auto_title is not None:
            body["auto_title"] = bool(auto_title)
        return self._request("post", "/meeting/start", timeout=10, json=body)

    def save_notes(self, notes: str) -> dict:
        return self._request("post", "/meeting/notes", json={"notes": notes})

    def stop(self) -> dict:
        return self._request("post", "/meeting/stop", timeout=600)

    def chat(self, question: str) -> dict:
        return self._request(
            "post", "/meeting/chat", timeout=300, json={"question": question}
        )

    def dismiss(self) -> None:
        self._request("post", "/meeting/dismiss")

    def speakers(self, meeting_id: str) -> dict:
        return self._request("get", f"/api/meetings/{quote(str(meeting_id), safe='')}/speakers")

    def rename_speaker(self, meeting_id: str, speaker_id: str, name: str) -> dict:
        return self._request(
            "put",
            f"/api/meetings/{quote(str(meeting_id), safe='')}"
            f"/speakers/{quote(str(speaker_id), safe='')}",
            timeout=10, json={"name": name},
        )

    def merge_speakers(self, meeting_id: str, from_id: str, into_id: str) -> dict:
        return self._request(
            "post", f"/api/meetings/{quote(str(meeting_id), safe='')}/speakers/merge",
            timeout=10, json={"from": from_id, "into": into_id},
        )
