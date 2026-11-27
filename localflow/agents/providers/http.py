"""Generic config-only HTTP agent (`http_providers.json` profiles).

Placeholders are substituted on JSON string leaves of `body` (never by string
concatenation, so a prompt cannot inject JSON structure) and on `path` with
percent-encoding; the host comes only from the profile's validated `base_url`.
`{secret}` is legal in header values only.
"""

from __future__ import annotations

import json
import re
from dataclasses import replace
from urllib.parse import quote, urlparse

from localflow.agents.models import (
    MAX_RESULT_CHARS,
    OK,
    STATES,
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

METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE")
OPERATIONS = ("start", "status", "message", "cancel")
PLACEHOLDERS = ("prompt", "title", "session_id", "external_id", "repo", "message")
_PLACEHOLDER = re.compile(r"\{(prompt|title|session_id|external_id|repo|message|secret)\}")
_SELECTOR = re.compile(r"^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*$")
_SENSITIVE_HEADER = re.compile(r"(?i)(key|token|secret)")
_HEADER_NAME = re.compile(r"^[A-Za-z0-9-]{1,64}$")
_PROFILE_FIELDS = {"label", "base_url", "enabled", "headers", *OPERATIONS}
_OP_FIELDS = {
    "start": {"method", "path", "body", "id_path", "url_path"},
    "status": {"method", "path", "state_path", "state_map", "result_path", "poll_seconds"},
    "message": {"method", "path", "body"},
    "cancel": {"method", "path", "body"},
}
MAX_BODY_CHARS = 16384


class ProfileError(ValueError):
    """An invalid HTTP agent profile."""


def secret_name(profile_id: str) -> str:
    return f"agent_http_{profile_id}_token"


def origin(base_url: str) -> str:
    parsed = urlparse(base_url)
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    return f"{parsed.scheme}://{(parsed.hostname or '').lower()}:{port}"


def secret_bound(profile: dict | None) -> bool:
    """Whether the profile's token (keyring or LOCALFLOW_AGENT_HTTP_<ID>_TOKEN) may be sent.

    `secret_origin` records the origin the token was set for; after `base_url` moves to
    another origin the token is withheld until it is set again. Profiles written before
    `secret_origin` existed have no record and keep working.
    """
    if not profile:
        return False
    bound = profile.get("secret_origin")
    if bound is None:
        return True
    try:
        return bound == origin(str(profile.get("base_url") or ""))
    except ValueError:
        return False


def bind_secret(profiles, profile_id: str) -> None:
    """Record that the profile's token was just saved to the keyring for its current origin.

    `secret_source: "keyring"` makes the provider read the keyring only, so a stale
    LOCALFLOW_AGENT_HTTP_<ID>_TOKEN left over for an old origin is never sent.
    """
    with profiles.mutate() as entries:
        for entry in entries:
            if entry.get("id") == profile_id and isinstance(entry.get("base_url"), str):
                entry["secret_origin"] = origin(entry["base_url"])
                entry["secret_source"] = "keyring"


def _walk_strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from _walk_strings(item)
    elif isinstance(value, dict):
        for key, item in value.items():
            yield key
            yield from _walk_strings(item)


def _check_op(name: str, op) -> dict:
    if not isinstance(op, dict):
        raise ProfileError(f"{name} must be an object")
    unknown = sorted(set(op) - _OP_FIELDS[name])
    if unknown:
        raise ProfileError(f"unknown {name} field(s): {', '.join(unknown)}")
    method = op.get("method", "POST" if name != "status" else "GET")
    if method not in METHODS:
        raise ProfileError(f"{name}.method must be one of {', '.join(METHODS)}")
    path = op.get("path")
    if not isinstance(path, str) or not path.startswith("/") or len(path) > 512:
        raise ProfileError(f"{name}.path must start with /")
    if "{secret}" in path:
        raise ProfileError("{secret} is only allowed in header values")
    cleaned = {"method": method, "path": path}
    if "body" in op:
        try:
            encoded = json.dumps(op["body"])
        except (TypeError, ValueError):
            raise ProfileError(f"{name}.body must be JSON") from None
        if len(encoded) > MAX_BODY_CHARS:
            raise ProfileError(f"{name}.body is too large")
        if any("{secret}" in s for s in _walk_strings(op["body"])):
            raise ProfileError("{secret} is only allowed in header values")
        cleaned["body"] = op["body"]
    for selector in ("id_path", "url_path", "state_path", "result_path"):
        if selector in op:
            value = op[selector]
            if not isinstance(value, str) or (value and not _SELECTOR.match(value)):
                raise ProfileError(f"{name}.{selector} must be a dotted key path")
            cleaned[selector] = value
    if name == "start" and "id_path" not in cleaned:
        cleaned["id_path"] = "id"
    if name == "status":
        if not cleaned.get("state_path"):
            raise ProfileError("status.state_path is required")
        state_map = op.get("state_map")
        if not isinstance(state_map, dict) or not state_map:
            raise ProfileError("status.state_map is required")
        for key, value in state_map.items():
            if not isinstance(key, str) or value not in STATES:
                raise ProfileError("status.state_map values must be session states")
        cleaned["state_map"] = dict(state_map)
        if "poll_seconds" in op:
            poll = op["poll_seconds"]
            if type(poll) is not int or not 2 <= poll <= 300:
                raise ProfileError("status.poll_seconds must be 2 to 300")
            cleaned["poll_seconds"] = poll
    return cleaned


def validate_profile(body, *, current: dict | None = None) -> dict:
    """Validate a profile body (merged over `current` for updates). Raises ProfileError."""
    if not isinstance(body, dict):
        raise ProfileError("expected a JSON object")
    allowed = _PROFILE_FIELDS | ({"id", "created_at", "updated_at", "secret_set"}
                                 if current is None else set())
    unknown = sorted(set(body) - allowed)
    if unknown:
        raise ProfileError(f"unknown field(s): {', '.join(unknown)}")
    merged = {k: v for k, v in (current or {}).items() if k in _PROFILE_FIELDS}
    merged.update({k: v for k, v in body.items() if k in _PROFILE_FIELDS})
    label = merged.get("label", "")
    if not isinstance(label, str) or not label.strip() or len(label) > 80:
        raise ProfileError("label must be 1 to 80 characters")
    base = merged.get("base_url")
    if not isinstance(base, str):
        raise ProfileError("base_url is required")
    parsed = urlparse(base)
    if parsed.scheme not in ("https", "http") or not parsed.hostname:
        raise ProfileError("base_url must be an https:// URL")
    try:
        parsed.port  # noqa: B018 - raises ValueError on a malformed or out-of-range port
    except ValueError:
        raise ProfileError("invalid port in base_url") from None
    if parsed.scheme == "http" and not is_loopback_url(base):
        raise ProfileError("base_url must be https:// (http:// only on a loopback host)")
    if parsed.query or parsed.fragment or parsed.username or parsed.password:
        raise ProfileError("base_url must not carry credentials, a query or a fragment")
    enabled = merged.get("enabled", True)
    if type(enabled) is not bool:
        raise ProfileError("enabled must be a boolean")
    headers = merged.get("headers", {})
    if not isinstance(headers, dict) or len(headers) > 32:
        raise ProfileError("headers must be an object")
    for name, value in headers.items():
        if not _HEADER_NAME.match(str(name)) or not isinstance(value, str) or len(value) > 1024:
            raise ProfileError("headers must map names to string values")
        if "\n" in value or "\r" in value:
            raise ProfileError("header values must be one line")
        sensitive = name.lower() in ("authorization", "cookie", "proxy-authorization") or \
            _SENSITIVE_HEADER.search(name)
        if sensitive and "{secret}" not in value:
            raise ProfileError(f"header {name} must use {{secret}}; never store a literal "
                               "credential in a profile")
    if "start" not in merged:
        raise ProfileError("start is required")
    profile = {"label": label.strip(), "base_url": base.rstrip("/"), "enabled": enabled,
               "headers": dict(headers)}
    for op in OPERATIONS:
        if op in merged and merged[op] is not None:
            profile[op] = _check_op(op, merged[op])
    return profile


def select(data, path: str):
    if not path:
        return data
    for part in path.split("."):
        if isinstance(data, dict):
            data = data.get(part)
        elif isinstance(data, list) and part.isdigit() and int(part) < len(data):
            data = data[int(part)]
        else:
            return None
    return data


def substitute(value, values: dict[str, str]):
    if isinstance(value, str):
        return _PLACEHOLDER.sub(lambda m: values.get(m.group(1), m.group(0)), value)
    if isinstance(value, list):
        return [substitute(v, values) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, values) for k, v in value.items()}
    return value


def _needs_secret(profile: dict) -> bool:
    return any("{secret}" in v for v in (profile.get("headers") or {}).values())


class HttpProvider(AgentProvider):
    id = "http"
    label = "HTTP agent"
    capabilities = ProviderCapabilities(
        kind="cloud", resume=False, live_message=False, cancel=False, streaming=False,
        needs_cwd=False, reattach=False, secret_names=(),
    )

    @classmethod
    def capabilities_for(cls, config, profile=None) -> ProviderCapabilities:
        profile = profile or {}
        return replace(cls.capabilities, live_message="message" in profile,
                       cancel="cancel" in profile, reattach="status" in profile,
                       secret_names=cls.secret_names_for(profile))

    @classmethod
    def secret_names_for(cls, profile=None) -> tuple[str, ...]:
        return (secret_name(profile["id"]),) if profile and profile.get("id") else ()

    @classmethod
    def required_secrets(cls, config, profile=None) -> tuple[str, ...]:
        return cls.secret_names_for(profile) if profile and _needs_secret(profile) else ()

    def readiness(self, *, deep: bool = False) -> Readiness:
        if not self.profile:
            return Readiness(False, "missing_profile", "Choose an HTTP agent profile.")
        if not self.profile.get("enabled", True):
            return Readiness(False, "disabled", "This HTTP agent profile is turned off.")
        if _needs_secret(self.profile) and not self._secret():
            return Readiness(False, "missing_secret", "Add the token for this HTTP agent.")
        return OK

    _UNPINNED = object()
    _pinned_secret = _UNPINNED

    def pinned(self) -> HttpProvider:
        """A copy bound to this profile snapshot and the token resolved right now.

        An active run follows a same-origin token rotation (see `_secret`), but a
        replacement set after the profile moved to a new origin, or was deleted and
        recreated, is never sent to the run's (old) endpoint.
        """
        copy = type(self)(self.config, self.ctx, dict(self.profile) if self.profile else None)
        copy._pinned_secret = self._resolve_secret()
        return copy

    def _secret(self) -> str:
        if self._pinned_secret is self._UNPINNED:
            return self._resolve_secret()
        live = self._live_profile_for_pinned_origin()
        if live is not None:
            # Same origin and the token is still bound to it: a rotated token applies
            # to the active run. Reading it also registers it with the redactor.
            fresh = self._resolve_secret(live)
            # Re-check after reading: a concurrent move plus replacement between the
            # two reads would otherwise send the new origin's token to the old endpoint.
            if fresh and self._live_profile_for_pinned_origin() is not None:
                # Remember the rotation so a later move falls back to the latest
                # same-origin token rather than the revoked launch token.
                self._pinned_secret = fresh
                return fresh
        # Deleted, moved to another origin or token removed: the live token (if any)
        # belongs elsewhere. The snapshot stays redacted (the redactor never forgets).
        return self._pinned_secret

    def _live_profile_for_pinned_origin(self) -> dict | None:
        pinned = self.profile or {}
        store = self.ctx.profiles
        if store is None or not pinned.get("id"):
            return None
        try:
            live = store.get(pinned["id"])
            if not live or not secret_bound(live):
                return None
            if origin(str(live.get("base_url") or "")) != origin(str(pinned.get("base_url"))):
                return None
        except Exception:  # noqa: BLE001 - an unreadable store keeps the pinned token
            return None
        return live

    def _resolve_secret(self, profile: dict | None = None) -> str:
        profile = profile if profile is not None else (self.profile or {})
        if not profile.get("id") or not secret_bound(profile):
            return ""
        name = secret_name(profile["id"])
        stored = self.ctx.get_stored_secret
        if profile.get("secret_source") == "keyring" and stored is not None:
            value = stored(name) or ""
            self.ctx.redactor.add(name, value)
            return value
        return self.ctx.get_secret(name)

    def _values(self, session: AgentSession, message: str = "") -> dict[str, str]:
        return {"prompt": session.prompt, "title": session.title, "session_id": session.id,
                "external_id": session.external_id, "repo": session.repo, "message": message}

    def _send(self, op_name: str, session: AgentSession, message: str = ""):
        profile = self.profile or {}
        op = profile.get(op_name)
        if not op:
            raise ProviderError("internal", f"the profile has no {op_name} operation")
        values = self._values(session, message)
        path = _PLACEHOLDER.sub(
            lambda m: quote(values.get(m.group(1), ""), safe="") if m.group(1) != "secret"
            else "", op["path"])
        url = profile["base_url"] + path
        secret = self._secret()
        headers = {name: value.replace("{secret}", secret)
                   for name, value in (profile.get("headers") or {}).items()}
        body = substitute(op["body"], values) if "body" in op else None
        if body is not None:
            headers.setdefault("Content-Type", "application/json")
        status, raw, response_headers = http_request(
            self.ctx, op["method"], url, headers=headers, json_body=body,
            trust_env=not is_loopback_url(url))
        if status >= 300 and not (op_name == "cancel" and status == 404):
            raise status_error(status, response_headers.get("Retry-After"))
        return status, raw

    @staticmethod
    def _parse(raw: bytes):
        try:
            return json.loads(raw.decode("utf-8", "replace") or "null")
        except ValueError:
            return None

    def start(self, session: AgentSession, sink: EventSink) -> None:
        _status, raw = self._send("start", session)
        profile = self.profile or {}
        if "status" not in profile:
            text = raw.decode("utf-8", "replace")[:MAX_RESULT_CHARS]
            if text:
                sink.output("agent", text)
            sink.finish("completed", result_text=text)
            return
        data = self._parse(raw)
        external = select(data, profile["start"].get("id_path", "id"))
        if external in (None, "") or isinstance(external, (dict, list)):
            raise ProviderError("bad_output", "the agent did not return a run id")
        url = select(data, profile["start"].get("url_path", "")) if \
            profile["start"].get("url_path") else ""
        sink.external(str(external), str(url) if isinstance(url, str) else "")

    def poll(self, session: AgentSession, sink: EventSink) -> float | None:
        profile = self.profile or {}
        op = profile["status"]
        _status, raw = self._send("status", session)
        data = self._parse(raw)
        raw_state = select(data, op["state_path"])
        mapped = op["state_map"].get(str(raw_state)) if raw_state is not None else None
        interval = float(op.get("poll_seconds") or self.config.agent_poll_seconds)
        if mapped is None or mapped in ("queued", "starting", "cancelling"):
            return interval
        if mapped == "completed":
            result = select(data, op.get("result_path", "")) if op.get("result_path") else ""
            if not isinstance(result, str):
                result = json.dumps(result, ensure_ascii=False) if result is not None else ""
            if result:
                sink.output("agent", result)
            sink.finish("completed", result_text=result)
            return None
        if mapped in ("failed", "cancelled", "timed_out"):
            error = None if mapped == "cancelled" else {
                "code": "timeout" if mapped == "timed_out" else "upstream_error",
                "message": f"the agent reported {str(raw_state)[:40]}"}
            sink.finish(mapped, error=error)
            return None
        if mapped != sink.current_state():
            sink.state(mapped, raw_status=str(raw_state)[:80])
        return interval

    def send_message(self, session: AgentSession, text: str, sink: EventSink) -> None:
        self._send("message", session, message=text)

    def cancel(self, session: AgentSession, sink: EventSink) -> None:
        try:
            self._send("cancel", session)
        except ProviderError as exc:
            raise ProviderError("cancel_failed", exc.message) from None
        sink.finish("cancelled")
