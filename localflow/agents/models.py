"""Provider-neutral data model, state machine and error vocabulary (stdlib only)."""

from __future__ import annotations

import secrets
from dataclasses import asdict, dataclass, field, fields
from datetime import UTC, datetime
from typing import ClassVar

MAX_PROMPT_CHARS = 20000
MAX_TITLE_CHARS = 120
MAX_RESULT_CHARS = 16 * 1024
MAX_EVENT_TEXT = 8 * 1024
RESULT_PREVIEW_CHARS = 200
MIN_TIMEOUT_SECONDS = 10
MAX_TIMEOUT_SECONDS = 86400

ACTIVE_STATES = ("queued", "starting", "running", "waiting_input", "cancelling")
TERMINAL_STATES = ("completed", "failed", "cancelled", "timed_out")
STATES = ACTIVE_STATES + TERMINAL_STATES
RUNNING_STATES = ("starting", "running", "waiting_input", "cancelling")  # hold a slot

TRANSITIONS: dict[str, frozenset[str]] = {
    "queued": frozenset({"starting", "cancelled", "failed"}),
    "starting": frozenset({"running", "failed", "cancelled", "cancelling"}),
    # A remote provider may report its run cancelled without a local cancel request.
    "running": frozenset({"waiting_input", "completed", "failed", "timed_out", "cancelling",
                          "cancelled"}),
    "waiting_input": frozenset({"running", "completed", "failed", "timed_out", "cancelling",
                                "cancelled"}),
    "cancelling": frozenset({"cancelled", "failed"}),
    "completed": frozenset(),
    "failed": frozenset(),
    "cancelled": frozenset(),
    "timed_out": frozenset(),
}

ERROR_CODES = (
    "provider_not_ready", "spawn_failed", "exit_nonzero", "bad_output", "auth_failed",
    "rate_limited", "upstream_error", "network_error", "timeout", "interrupted",
    "cancel_failed", "internal",
)
ORIGINS = ("api", "cli", "hot_command")
EVENT_TYPES = ("state", "output", "message", "error", "usage", "truncated")
EVENT_STREAMS = ("agent", "stderr", "user", "system")
READINESS_CODES = (
    "ok", "disabled", "missing_binary", "missing_wrapper", "missing_secret",
    "missing_profile", "unreachable", "auth_failed",
)
PROVIDER_OPTIONS = {"claude": frozenset({"model"}), "mock": frozenset({"script"})}


def utc_now() -> str:
    return datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def new_session_id() -> str:
    return secrets.token_hex(6)


def new_short_id() -> str:
    return secrets.token_hex(4)


def is_active(state: str) -> bool:
    return state in ACTIVE_STATES


def is_terminal(state: str) -> bool:
    return state in TERMINAL_STATES


def one_line_title(prompt: str, limit: int = 80) -> str:
    return " ".join(prompt.split())[:limit]


# -- errors -------------------------------------------------------------------


class AgentError(Exception):
    """Base of every launcher error; `code` and `status` map to the HTTP envelope."""

    code = "internal"
    status = 500

    def __init__(self, detail: str = "", **extra):
        self.detail = detail or self.__class__.__doc__ or self.code
        self.extra = extra
        super().__init__(self.detail)


class ValidationError(AgentError):
    """The request is invalid."""

    code = "validation"
    status = 400


class PromptTooLong(ValidationError):
    """The prompt is too long."""

    code = "prompt_too_long"
    status = 413


class NotFound(AgentError):
    """Not found."""

    code = "not_found"
    status = 404


class AgentsDisabled(AgentError):
    """Agents are turned off; enable them in settings (agents.enabled)."""

    code = "agents_disabled"
    status = 409


class SessionConflict(AgentError):
    """A compare-and-set lost: the session was not in an expected state."""

    code = "session_conflict"
    status = 409

    def __init__(self, session_id: str, current: str, expected: tuple[str, ...]):
        self.session_id = session_id
        self.current = current
        self.expected = tuple(expected)
        super().__init__(
            f"cannot change session {session_id} in state {current!r}; "
            f"expected one of {', '.join(self.expected) or 'none'}",
            state=current,
            expected=list(self.expected),
        )


class IdConflict(AgentError):
    """A session with this id already exists with a different request."""

    code = "id_conflict"
    status = 409


class HotCommandConflict(AgentError):
    """The trigger duplicates or overlaps another trigger."""

    code = "rule_conflict"
    status = 409


class ProviderNotReady(AgentError):
    """The provider is not ready."""

    code = "provider_not_ready"
    status = 409

    def __init__(self, readiness: Readiness):
        super().__init__(readiness.detail or "the provider is not ready",
                         readiness=readiness.to_dict("shallow"))
        self.readiness = readiness


class Unsupported(AgentError):
    """The provider does not support this."""

    code = "unsupported"
    status = 409


class QueueFull(AgentError):
    """Too many sessions are waiting; try again shortly."""

    code = "queue_full"
    status = 429


class SecretsUnavailableError(AgentError):
    """The keyring is unavailable."""

    code = "secrets_unavailable"
    status = 503


class ProviderError(Exception):
    """Raised by providers; `code` is one of ERROR_CODES."""

    def __init__(self, code: str, message: str, *, retryable: bool = False,
                 retry_after: float | None = None):
        super().__init__(message)
        self.code = code if code in ERROR_CODES else "internal"
        self.message = message
        self.retryable = retryable
        self.retry_after = retry_after


class UpstreamError(AgentError):
    """The provider's service refused the request."""

    status = 502

    def __init__(self, err: ProviderError):
        super().__init__(err.message)
        self.code = err.code


# -- data ---------------------------------------------------------------------


@dataclass(frozen=True)
class ProviderCapabilities:
    kind: str  # local_process | cloud
    resume: bool = False
    live_message: bool = False
    cancel: bool = True
    streaming: bool = False
    needs_cwd: bool = False
    reattach: bool = False
    secret_names: tuple[str, ...] = ()

    def to_dict(self) -> dict:
        data = asdict(self)
        data.pop("secret_names")
        return data


@dataclass(frozen=True)
class Readiness:
    ready: bool
    code: str
    detail: str = ""

    def to_dict(self, checked: str = "shallow") -> dict:
        return {"ready": self.ready, "code": self.code, "detail": self.detail,
                "checked": checked}


OK = Readiness(True, "ok", "Ready.")


@dataclass
class AgentSession:
    id: str
    provider: str
    profile: str = ""
    state: str = "queued"
    origin: str = "api"
    rule_id: str = ""
    title: str = ""
    prompt: str = ""
    repo: str = ""
    cwd: str = ""
    parent_id: str = ""
    root_id: str = ""
    external_id: str = ""
    external_url: str = ""
    sandbox: bool = False
    created_at: str = ""
    started_at: str | None = None
    updated_at: str = ""
    finished_at: str | None = None
    timeout_seconds: int = 1800
    last_seq: int = 0
    revision: int = 0
    result_text: str = ""
    result_truncated: bool = False
    error: dict | None = None
    usage: dict | None = None
    cancel_requested: bool = False
    # Server-private bookkeeping (request digest, effective provider, resume
    # target, options); never returned by the API.
    internal: dict = field(default_factory=dict)

    @property
    def effective_provider(self) -> str:
        return self.internal.get("effective_provider") or self.provider

    @property
    def options(self) -> dict:
        return dict(self.internal.get("options") or {})

    def to_record(self) -> dict:
        return asdict(self)

    @classmethod
    def from_record(cls, data: dict) -> AgentSession:
        names = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in data.items() if k in names})

    def to_api(self) -> dict:
        data = asdict(self)
        data.pop("internal")
        return data

    def to_summary(self) -> dict:
        data = self.to_api()
        data.pop("prompt")
        result = data.pop("result_text")
        data["result_preview"] = result[:RESULT_PREVIEW_CHARS]
        return data


@dataclass
class AgentEvent:
    seq: int
    ts: str
    type: str
    stream: str = "system"
    text: str = ""
    state: str | None = None
    data: dict = field(default_factory=dict)

    def to_dict(self) -> dict:
        return asdict(self)


SESSION_FIELDS = tuple(f.name for f in fields(AgentSession) if f.name != "internal")


@dataclass
class LaunchRequest:
    prompt: str
    provider: str = ""
    id: str = ""
    profile: str = ""
    repo: str = ""
    cwd: str = ""
    resume_of: str = ""
    title: str = ""
    timeout_seconds: int | None = None
    origin: str = "api"
    rule_id: str = ""
    options: dict = field(default_factory=dict)

    _TYPES: ClassVar[dict[str, type]] = {
        "prompt": str, "provider": str, "id": str, "profile": str, "repo": str, "cwd": str,
        "resume_of": str, "title": str, "timeout_seconds": int, "origin": str,
        "rule_id": str, "options": dict,
    }

    @classmethod
    def from_body(cls, body: object) -> LaunchRequest:
        if not isinstance(body, dict):
            raise ValidationError("expected a JSON object")
        unknown = sorted(set(body) - set(cls._TYPES))
        if unknown:
            raise ValidationError(f"unknown field(s): {', '.join(unknown)}")
        values: dict = {}
        for name, value in body.items():
            expected = cls._TYPES[name]
            if name == "timeout_seconds" and value is None:
                continue
            if type(value) is not expected:
                kind = {str: "string", int: "integer", dict: "object"}[expected]
                raise ValidationError(f"{name} must be a {kind}")
            values[name] = value
        if "prompt" not in values:
            raise ValidationError("prompt is required")
        return cls(**values)

    def digest_fields(self) -> dict:
        data = asdict(self)
        data.pop("id")
        return data


@dataclass
class HotCommandRule:
    id: str
    trigger: str
    provider: str
    profile: str = ""
    repo: str = ""
    prompt_template: str = "{prompt}"
    min_prompt_words: int = 3
    enabled: bool = True
    created_at: str = ""
    updated_at: str = ""

    def to_dict(self) -> dict:
        return asdict(self)
