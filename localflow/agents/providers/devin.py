"""Devin sessions through the Devin REST API (cloud, polled).

Every vendor specific lives in the constants below. The base URL is a constant
(never a config key) so a config write can never send the key to another host;
`LOCALFLOW_DEVIN_BASE_URL` exists for tests and the sim harness only.
"""

from __future__ import annotations

import json
import os

from localflow.agents.models import (
    OK,
    AgentSession,
    ProviderCapabilities,
    ProviderError,
    Readiness,
)
from localflow.agents.providers.base import (
    AgentProvider,
    EventSink,
    http_request,
    is_loopback_url,
    status_error,
)

DEFAULT_BASE_URL = "https://api.devin.ai"
SECRET = "agent_devin_api_key"
# Operation table (Devin API v1).
START = ("POST", "/v1/sessions")
STATUS = ("GET", "/v1/sessions/{id}")
MESSAGE = ("POST", "/v1/sessions/{id}/message")
CANCEL = ("DELETE", "/v1/sessions/{id}")
CHECK = ("GET", "/v1/sessions?limit=1")
STATUS_MAP = {
    "working": "running",
    "blocked": "waiting_input",
    "finished": "completed",
    "expired": "timed_out",
}
USER_MESSAGE_TYPES = ("user_message",)


def base_url() -> str:
    return (os.environ.get("LOCALFLOW_DEVIN_BASE_URL") or DEFAULT_BASE_URL).rstrip("/")


def _json(body: bytes) -> dict:
    try:
        data = json.loads(body.decode("utf-8", "replace") or "{}")
    except ValueError:
        return {}
    return data if isinstance(data, dict) else {}


class DevinProvider(AgentProvider):
    id = "devin"
    label = "Devin"
    capabilities = ProviderCapabilities(
        kind="cloud", resume=False, live_message=True, cancel=True, streaming=False,
        needs_cwd=False, reattach=True, secret_names=(SECRET,),
    )

    def __init__(self, config, ctx, profile=None):
        super().__init__(config, ctx, profile)
        self._seen: dict[str, int] = {}

    def _call(self, op: tuple[str, str], *, session_id: str = "", body=None):
        key = self.ctx.get_secret(SECRET)
        if not key:
            raise ProviderError("auth_failed", "Add a Devin API key.")
        method, path = op
        url = base_url() + path.replace("{id}", session_id)
        return http_request(self.ctx, method, url, json_body=body,
                            headers={"Authorization": f"Bearer {key}"},
                            trust_env=not is_loopback_url(url))

    def readiness(self, *, deep: bool = False) -> Readiness:
        if not self.ctx.get_secret(SECRET):
            return Readiness(False, "missing_secret", "Add a Devin API key.")
        if not deep:
            return OK
        try:
            status, _body, _headers = self._call(CHECK)
        except ProviderError:
            return Readiness(False, "unreachable", "Could not reach the Devin API.")
        if status in (401, 403):
            return Readiness(False, "auth_failed", "Devin rejected the API key.")
        if status >= 400:
            return Readiness(False, "unreachable", f"The Devin API answered HTTP {status}.")
        return OK

    def _prompt(self, session: AgentSession) -> str:
        if session.repo:
            return f"Repository: {session.repo}\n\n{session.prompt}"
        return session.prompt

    def start(self, session: AgentSession, sink: EventSink) -> None:
        status, body, headers = self._call(START, body={
            "prompt": self._prompt(session), "title": session.title, "idempotent": True,
        })
        if status >= 300:
            raise status_error(status, headers.get("Retry-After"))
        data = _json(body)
        external = str(data.get("session_id") or "")
        if not external:
            raise ProviderError("bad_output", "Devin did not return a session id")
        sink.external(external, str(data.get("url") or ""))

    def poll(self, session: AgentSession, sink: EventSink) -> float | None:
        status, body, headers = self._call(STATUS, session_id=session.external_id)
        if status >= 300:
            raise status_error(status, headers.get("Retry-After"))
        data = _json(body)
        messages = data.get("messages") if isinstance(data.get("messages"), list) else []
        seen = self._seen.get(session.id)
        if seen is None:
            seen = int(session.internal.get("devin_seen", 0))
        last_text = ""
        for message in messages[seen:]:
            if not isinstance(message, dict):
                continue
            text = str(message.get("message") or "")
            if message.get("type") in USER_MESSAGE_TYPES or not text:
                continue
            sink.output("agent", text)
            last_text = text
        if len(messages) != seen:
            sink.note(devin_seen=len(messages))
        self._seen[session.id] = len(messages)
        raw = str(data.get("status_enum") or "")
        mapped = STATUS_MAP.get(raw)
        if mapped is None:
            if raw and session.internal.get("raw_status") != raw:
                sink.note(raw_status=raw)
            return float(self.config.agent_poll_seconds)
        if mapped == "completed":
            result = last_text or self._last_agent_text(messages)
            structured = data.get("structured_output")
            if not result and structured:
                result = json.dumps(structured, ensure_ascii=False)
            sink.finish("completed", result_text=result)
            return None
        if mapped == "timed_out":
            sink.finish("timed_out", error={"code": "timeout",
                                            "message": "the Devin session expired"})
            return None
        if mapped != sink.current_state():
            sink.state(mapped, raw_status=raw)
        return float(self.config.agent_poll_seconds)

    @staticmethod
    def _last_agent_text(messages: list) -> str:
        for message in reversed(messages):
            if isinstance(message, dict) and message.get("type") not in USER_MESSAGE_TYPES:
                text = str(message.get("message") or "")
                if text:
                    return text
        return ""

    def send_message(self, session: AgentSession, text: str, sink: EventSink) -> None:
        status, _body, headers = self._call(MESSAGE, session_id=session.external_id,
                                            body={"message": text})
        if status >= 300:
            raise status_error(status, headers.get("Retry-After"))

    def cancel(self, session: AgentSession, sink: EventSink) -> None:
        if not session.external_id:
            sink.finish("cancelled")
            return
        status, _body, _headers = self._call(CANCEL, session_id=session.external_id)
        if status < 300 or status == 404:
            sink.finish("cancelled")
            return
        raise ProviderError("cancel_failed", f"Devin did not stop the session (HTTP {status})")
