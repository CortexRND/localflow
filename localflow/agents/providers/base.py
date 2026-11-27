"""The provider-neutral interface every agent provider implements (stdlib only)."""

from __future__ import annotations

import os
import shutil
import subprocess
import time
from abc import ABC, abstractmethod
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from typing import Any, ClassVar, Protocol

from localflow.agents.models import (
    OK,
    AgentSession,
    ProviderCapabilities,
    ProviderError,
    Readiness,
)
from localflow.agents.redact import Redactor

MAX_RESPONSE_BYTES = 1024 * 1024
HTTP_TIMEOUT_SECONDS = 30


class EventSink(Protocol):
    """What a provider may do to its session; the sink redacts, persists and notifies."""

    def output(self, stream: str, text: str) -> None: ...
    def external(self, external_id: str, url: str = "") -> None: ...
    def state(self, to: str, **data) -> bool: ...
    def usage(self, cost_usd=None, duration_ms=None, turns=None) -> None: ...
    def finish(self, state: str, result_text: str = "", error: dict | None = None) -> bool: ...
    def current_state(self) -> str: ...
    def note(self, **internal) -> None: ...


def _default_http():
    import requests

    return requests.Session()


@dataclass
class ProviderContext:
    get_secret: Callable[[str], str] = lambda _name: ""
    redactor: Redactor = field(default_factory=Redactor)
    clock: Callable[[], float] = time.time
    popen: Callable[..., Any] = subprocess.Popen
    run: Callable[..., Any] = subprocess.run
    which: Callable[[str], str | None] = shutil.which
    http: Callable[[], Any] = _default_http
    environ: Mapping[str, str] = field(default_factory=lambda: os.environ)
    profiles: Any = None  # HttpProfileStore for the http provider
    mock_poll_seconds: float = 0.05
    kill_grace_seconds: float = 5.0


class AgentProvider(ABC):
    id: ClassVar[str]
    label: ClassVar[str]
    capabilities: ClassVar[ProviderCapabilities]

    def __init__(self, config, ctx: ProviderContext, profile: dict | None = None):
        self.config = config
        self.ctx = ctx
        self.profile = profile

    @classmethod
    def capabilities_for(cls, config, profile: dict | None = None) -> ProviderCapabilities:
        return cls.capabilities

    @classmethod
    def secret_names_for(cls, profile: dict | None = None) -> tuple[str, ...]:
        return cls.capabilities.secret_names

    @classmethod
    def required_secrets(cls, config, profile: dict | None = None) -> tuple[str, ...]:
        return cls.secret_names_for(profile)

    def readiness(self, *, deep: bool = False) -> Readiness:
        return OK

    @abstractmethod
    def start(self, session: AgentSession, sink: EventSink) -> None: ...

    def poll(self, session: AgentSession, sink: EventSink) -> float | None:
        return None

    def send_message(self, session: AgentSession, text: str, sink: EventSink) -> None:
        raise ProviderError("internal", f"{self.label} does not accept messages")

    def cancel(self, session: AgentSession, sink: EventSink) -> None:
        sink.finish("cancelled")

    def close(self) -> None:
        return None


# -- shared HTTP helpers (cloud providers) -------------------------------------


def status_error(status: int, retry_after: str | None = None) -> ProviderError:
    if status in (401, 403):
        return ProviderError("auth_failed", f"the service rejected the credentials (HTTP {status})")
    if status == 429:
        try:
            delay = float(retry_after) if retry_after else None
        except ValueError:
            delay = None
        return ProviderError("rate_limited", "the service is rate limiting requests (HTTP 429)",
                             retryable=True, retry_after=delay)
    if status >= 500:
        return ProviderError("upstream_error", f"the service failed (HTTP {status})",
                             retryable=True)
    return ProviderError("upstream_error", f"the service refused the request (HTTP {status})")


def http_request(ctx: ProviderContext, method: str, url: str, *, headers: dict | None = None,
                 json_body=None, trust_env: bool = True):
    """(status, body bytes, headers) — no redirects, bounded time and size.
    Connection failures and timeouts raise a retryable ProviderError."""
    session = ctx.http()
    try:
        session.trust_env = trust_env
    except AttributeError:
        pass
    try:
        response = session.request(method, url, headers=headers or {}, json=json_body,
                                   timeout=HTTP_TIMEOUT_SECONDS, allow_redirects=False,
                                   stream=True)
    except Exception as exc:  # noqa: BLE001 - requests' many transport errors
        raise ProviderError("network_error",
                            f"could not reach the service ({type(exc).__name__})",
                            retryable=True) from None
    try:
        body = b""
        for chunk in response.iter_content(65536):
            body += chunk
            if len(body) > MAX_RESPONSE_BYTES:
                body = body[:MAX_RESPONSE_BYTES]
                break
    except Exception:  # noqa: BLE001
        raise ProviderError("network_error", "the service response was interrupted",
                            retryable=True) from None
    finally:
        try:
            response.close()
        except Exception:  # noqa: BLE001 - closing a dead connection
            body = body or b""
    return response.status_code, body, dict(response.headers or {})


def is_loopback_url(url: str) -> bool:
    from urllib.parse import urlparse

    host = (urlparse(url).hostname or "").lower()
    return host in ("localhost", "127.0.0.1", "::1") or host.startswith("127.")
