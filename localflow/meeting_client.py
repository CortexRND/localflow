"""HTTP client for the local meeting endpoints."""

import requests

from localflow.apitoken import load
from localflow.config import load_config


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
            base_url = f"http://127.0.0.1:{load_config().server_port}"
        self.base_url = base_url.rstrip("/")
        self.token = token if token is not None else load()
        self.http = http

    def _request(self, method: str, path: str, *, timeout=3, json=None) -> dict:
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
            raise MeetingClientError(
                "LocalFlow server not running — start it with `lf serve`"
            ) from exc
        except Exception as exc:
            # Injected HTTP adapters can expose their own connection exception.
            if isinstance(exc, OSError):
                raise MeetingClientError(
                    "LocalFlow server not running — start it with `lf serve`"
                ) from exc
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

    def start(self, title, category, template, user_notes) -> dict:
        return self._request(
            "post",
            "/meeting/start",
            timeout=10,
            json={
                "title": title,
                "category": category,
                "template": template,
                "user_notes": user_notes,
            },
        )

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
