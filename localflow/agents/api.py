"""`/api/agents/*` (agents-api v1, docs/contracts/agents-api.v1.schema.json).

The manager is built on the first request to one of these routes; nothing here
runs for other routes. Blocking work goes to the threadpool; long-polls wait on
asyncio events, holding no thread.
"""

from __future__ import annotations

import asyncio
import functools
import json
import logging
import threading
from collections.abc import Callable

from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse
from starlette.concurrency import run_in_threadpool

from localflow.agents import API_VERSION
from localflow.agents.models import (
    ACTIVE_STATES,
    STATES,
    TERMINAL_STATES,
    AgentError,
    AgentsDisabled,
    NotFound,
    SecretsUnavailableError,
    SessionConflict,
    ValidationError,
    new_short_id,
    utc_now,
)

log = logging.getLogger("localflow.agents.api")

AgentApiError = AgentError
router = APIRouter(prefix="/api/agents")
MAX_BODY_BYTES = 256 * 1024
MAX_WAIT_SECONDS = 25
MAX_WAITERS = 16
SECRET_NAME_RE = r"^agent_[a-z0-9_]{1,60}$"

_get_config: Callable[[], object] | None = None
_is_local: Callable[[Request], bool] = lambda _request: False
_manager = None
_manager_lock = threading.Lock()
_waiters: set[tuple[asyncio.AbstractEventLoop, asyncio.Event]] = set()
_waiters_lock = threading.Lock()


class LocalOnly(AgentError):
    """This change is only allowed from this computer (agents.allow_remote is off)."""

    code = "local_only"
    status = 403


def configure(get_config: Callable[[], object], is_local: Callable[[Request], bool]) -> None:
    global _get_config, _is_local
    _get_config = get_config
    _is_local = is_local


def _config():
    if _get_config is None:
        from localflow.config import load_config

        return load_config()
    return _get_config()


def manager_if_built():
    return _manager


def get_manager():
    global _manager
    if _manager is None:
        with _manager_lock:
            if _manager is None:
                from localflow.agents.manager import AgentManager

                manager = AgentManager(_config)
                manager.subscribe(_wake_waiters)
                _manager = manager
    return _manager


def set_manager(manager) -> None:
    """Install a prepared manager (tests), or None to drop it."""
    global _manager
    with _manager_lock:
        _manager = manager
        if manager is not None:
            manager.subscribe(_wake_waiters)


def shutdown() -> None:
    manager = _manager
    if manager is not None:
        manager.shutdown()


def error_response(exc: AgentError) -> JSONResponse:
    redactor = getattr(_manager, "redactor", None)
    detail = redactor(exc.detail) if redactor is not None else exc.detail
    body = {"detail": detail, "code": exc.code, **exc.extra}
    headers = {"Retry-After": "5"} if exc.code == "queue_full" else None
    return JSONResponse(body, status_code=exc.status, headers=headers)


async def agent_error_handler(_request: Request, exc: AgentError) -> JSONResponse:
    return error_response(exc)


def _guard(handler):
    @functools.wraps(handler)
    async def wrapper(*args, **kwargs):
        try:
            return await handler(*args, **kwargs)
        except AgentError as exc:
            return error_response(exc)
        except Exception:
            log.exception("agents route failed")
            return JSONResponse({"detail": "internal error", "code": "internal"},
                                status_code=500)

    return wrapper


def _require_local(request: Request) -> None:
    if getattr(_config(), "agent_allow_remote", False):
        return
    if not _is_local(request):
        raise LocalOnly()


async def _body(request: Request, *, optional: bool = False) -> dict:
    raw = bytearray()
    async for chunk in request.stream():
        raw += chunk
        if len(raw) > MAX_BODY_BYTES:
            raise ValidationError("request body is too large")
    if not raw.strip():
        if optional:
            return {}
        raise ValidationError("expected a JSON object")
    try:
        body = json.loads(bytes(raw))
    except (ValueError, RecursionError):
        raise ValidationError("invalid JSON") from None
    if not isinstance(body, dict):
        raise ValidationError("expected a JSON object")
    return body


def _int_param(request: Request, name: str, default: int, low: int, high: int) -> int:
    raw = request.query_params.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ValidationError(f"{name} must be an integer") from None
    if not low <= value <= high:
        raise ValidationError(f"{name} must be {low} to {high}")
    return value


def _wait_param(request: Request) -> float:
    raw = request.query_params.get("wait")
    if raw is None or raw == "":
        return 0.0
    try:
        value = float(raw)
    except ValueError:
        raise ValidationError("wait must be a number") from None
    if not 0 <= value <= MAX_WAIT_SECONDS:
        raise ValidationError(f"wait must be 0 to {MAX_WAIT_SECONDS}")
    return value


def _wake_waiters() -> None:
    with _waiters_lock:
        waiters = list(_waiters)
    for loop, event in waiters:
        try:
            loop.call_soon_threadsafe(event.set)
        except RuntimeError:
            pass


async def _hold(ready: Callable[[], bool], seconds: float) -> bool:
    """Wait until `ready()` or `seconds` pass. False when not held (no slot)."""
    loop = asyncio.get_running_loop()
    event = asyncio.Event()
    entry = (loop, event)
    with _waiters_lock:
        if len(_waiters) >= MAX_WAITERS:
            return False
        _waiters.add(entry)
    try:
        deadline = loop.time() + seconds
        while not ready():
            remaining = deadline - loop.time()
            if remaining <= 0:
                break
            try:
                await asyncio.wait_for(event.wait(), remaining)
            except TimeoutError:
                break
            event.clear()
    finally:
        with _waiters_lock:
            _waiters.discard(entry)
    return True


def _enabled_or_raise() -> None:
    if not getattr(_config(), "agents_enabled", False):
        raise AgentsDisabled()


# -- status, providers, secrets ------------------------------------------------


@router.get("/status")
@_guard
async def agents_status(request: Request):
    return await run_in_threadpool(lambda: get_manager().status())


@router.get("/providers")
@_guard
async def agents_providers(request: Request):
    entries = await run_in_threadpool(lambda: get_manager().provider_entries())
    return {"providers": entries}


def _known_entry(manager, provider_id: str, profile_id: str) -> None:
    if not manager.known_provider(provider_id):
        raise NotFound(f"unknown provider {provider_id[:40]!r}")
    if provider_id == "http":
        if not profile_id or manager.profile("http", profile_id) is None:
            raise NotFound("unknown HTTP agent profile")
    elif profile_id:
        raise ValidationError("profile is only used by the http provider")


@router.post("/providers/{provider_id}/check")
@_guard
async def agents_provider_check(request: Request, provider_id: str):
    _require_local(request)
    body = await _body(request, optional=True)
    if set(body) - {"profile"} or not isinstance(body.get("profile", ""), str):
        raise ValidationError("body may only contain a profile string")
    profile_id = body.get("profile", "")
    manager = get_manager()
    _known_entry(manager, provider_id, profile_id)

    def check():
        return manager.provider_entry(provider_id, profile_id, deep=True)

    return await asyncio.wait_for(run_in_threadpool(check), timeout=20)


@router.put("/providers/{provider_id}/secret")
@_guard
async def agents_provider_secret(request: Request, provider_id: str):
    import re

    _require_local(request)
    body = await _body(request)
    if set(body) - {"name", "value", "profile"}:
        raise ValidationError("unknown field(s)")
    profile_id = body.get("profile", "")
    value = body.get("value")
    name = body.get("name", "")
    if not isinstance(value, str) or not isinstance(name, str) or not isinstance(profile_id, str):
        raise ValidationError("name, value and profile must be strings")
    manager = get_manager()
    _known_entry(manager, provider_id, profile_id)
    names = manager.provider_class(provider_id).secret_names_for(
        manager.profile(provider_id, profile_id))
    if not name:
        if len(names) != 1:
            raise ValidationError("name is required for this provider")
        name = names[0]
    if not re.match(SECRET_NAME_RE, name) or name not in names:
        raise ValidationError("this provider does not use that secret")

    def write():
        from localflow.secrets import (
            SecretsUnavailable,
            delete_secret,
            get_secret,
            set_secret,
        )

        try:
            if value:
                set_secret(name, value)
                manager.redactor.add(name, value)
                if provider_id == "http":
                    from localflow.agents.providers.http import bind_secret

                    bind_secret(manager.profiles, profile_id)
            else:
                try:
                    delete_secret(name)
                except SecretsUnavailable:
                    if get_secret(name):
                        raise
        except SecretsUnavailable:
            raise SecretsUnavailableError("The keyring is unavailable; the key was not saved.") \
                from None
        manager.redactor.refresh()
        log.info("agent secret %s %s", name, "set" if value else "cleared")
        return manager.provider_entry(provider_id, profile_id)

    return await run_in_threadpool(write)


# -- HTTP profiles -----------------------------------------------------------------


def _profile_out(manager, profile: dict) -> dict:
    from localflow.agents.providers.http import secret_bound, secret_name

    out = {k: v for k, v in profile.items() if k not in ("secret_origin", "secret_source")}
    out["secret_set"] = secret_bound(profile) and bool(
        manager.ctx.get_secret(secret_name(profile["id"])))
    return out


@router.get("/http-profiles")
@_guard
async def agents_http_profiles(request: Request):
    def run():
        manager = get_manager()
        return {"profiles": [_profile_out(manager, p) for p in manager.profiles.list()
                             if p.get("id")]}

    return await run_in_threadpool(run)


@router.post("/http-profiles")
@_guard
async def agents_http_profile_add(request: Request):
    from localflow.agents.providers.http import ProfileError, origin, validate_profile

    _require_local(request)
    body = await _body(request)
    for name in ("id", "created_at", "updated_at", "secret_set"):
        body.pop(name, None)
    try:
        profile = validate_profile(body)
    except ProfileError as exc:
        raise ValidationError(str(exc)) from None

    def run():
        manager = get_manager()
        now = utc_now()
        entry = {"id": new_short_id(), **profile, "created_at": now, "updated_at": now,
                 "secret_origin": origin(profile["base_url"])}
        with manager.profiles.mutate() as profiles:
            if len(profiles) >= 32:
                raise ValidationError("at most 32 HTTP agent profiles")
            profiles.append(entry)
        manager.redactor.refresh()
        return JSONResponse(_profile_out(manager, entry), status_code=201)

    return await run_in_threadpool(run)


def _active_session_using(manager, profile_id: str):
    for session in manager.store.list():
        if session.provider == "http" and session.profile == profile_id and \
                session.state in ACTIVE_STATES:
            return session
    return None


@router.put("/http-profiles/{profile_id}")
@_guard
async def agents_http_profile_update(request: Request, profile_id: str):
    from localflow.agents.providers.http import (
        ProfileError,
        origin,
        secret_name,
        validate_profile,
    )

    _require_local(request)
    body = await _body(request)

    def run():
        manager = get_manager()
        with manager.profiles.mutate() as profiles:
            entry = next((p for p in profiles if p.get("id") == profile_id), None)
            if entry is None:
                raise NotFound("unknown HTTP agent profile")
            try:
                updated = validate_profile(body, current=entry)
            except ProfileError as exc:
                raise ValidationError(str(exc)) from None
            origin_changed = origin(updated["base_url"]) != origin(entry["base_url"])
            # The token stays bound to the old origin (so neither the keyring nor an
            # environment token is sent to the new one) until it is set again.
            entry.setdefault("secret_origin", origin(entry["base_url"]))
            for op in ("status", "message", "cancel"):
                entry.pop(op, None)
            entry.update(updated)
            entry["updated_at"] = utc_now()
            result = dict(entry)
        if origin_changed:
            _delete_secret_quietly(secret_name(profile_id))
            log.info("HTTP agent profile %s moved to a new origin; its token was removed",
                     profile_id)
        manager.redactor.refresh()
        return _profile_out(manager, result)

    return await run_in_threadpool(run)


def _delete_secret_quietly(name: str) -> None:
    from localflow.secrets import delete_secret

    try:
        delete_secret(name)
    except Exception:  # noqa: BLE001 - keyring unavailable or "not found" variants
        log.debug("no agent secret to remove")


@router.delete("/http-profiles/{profile_id}")
@_guard
async def agents_http_profile_delete(request: Request, profile_id: str):
    from localflow.agents.providers.http import secret_name

    _require_local(request)

    def run():
        manager = get_manager()
        active = _active_session_using(manager, profile_id)
        if active is not None:
            raise SessionConflict(active.id, active.state, TERMINAL_STATES)
        with manager.profiles.mutate() as profiles:
            remaining = [p for p in profiles if p.get("id") != profile_id]
            if len(remaining) == len(profiles):
                raise NotFound("unknown HTTP agent profile")
            profiles[:] = remaining
        _delete_secret_quietly(secret_name(profile_id))
        manager.redactor.refresh()
        return {"deleted": True}

    return await run_in_threadpool(run)


# -- sessions ------------------------------------------------------------------------


@router.post("/sessions")
@_guard
async def agents_launch(request: Request):
    _require_local(request)
    _enabled_or_raise()
    body = await _body(request)

    def run():
        session, created = get_manager().launch(body)
        return JSONResponse({"session": session.to_api()}, status_code=202 if created else 200)

    return await run_in_threadpool(run)


def _state_filter(raw: str) -> set[str] | None:
    if not raw:
        return None
    states: set[str] = set()
    for part in (p.strip() for p in raw.split(",") if p.strip()):
        if part == "active":
            states.update(ACTIVE_STATES)
        elif part == "terminal":
            states.update(TERMINAL_STATES)
        elif part in STATES:
            states.add(part)
        else:
            raise ValidationError(f"unknown state {part[:40]!r}")
    return states


@router.get("/sessions")
@_guard
async def agents_sessions(request: Request):
    params = request.query_params
    unknown = set(params) - {"state", "provider", "origin", "root_id", "q", "limit", "before",
                             "token"}
    if unknown:
        raise ValidationError(f"unknown parameter(s): {', '.join(sorted(unknown))}")
    states = _state_filter(params.get("state", ""))
    limit = _int_param(request, "limit", 50, 1, 200)
    origin = params.get("origin", "")
    if origin and origin not in ("api", "cli", "hot_command"):
        raise ValidationError("origin must be api, cli or hot_command")

    def run():
        manager = get_manager()
        page, cursor = manager.list_sessions(
            states=states, provider=params.get("provider", ""), origin=origin,
            root_id=params.get("root_id", ""), q=params.get("q", "")[:200], limit=limit,
            before=params.get("before", ""))
        return {"sessions": [s.to_summary() for s in page], "next_cursor": cursor,
                "revision": manager.store.revision, "counts": manager.store.counts()}

    return await run_in_threadpool(run)


@router.get("/sessions/{session_id}")
@_guard
async def agents_session(request: Request, session_id: str):
    session = await run_in_threadpool(lambda: get_manager().store.require(session_id))
    return {"session": session.to_api()}


@router.post("/sessions/{session_id}/cancel")
@_guard
async def agents_cancel(request: Request, session_id: str):
    _require_local(request)
    if await _body(request, optional=True):
        raise ValidationError("cancel takes no body fields")
    session = await run_in_threadpool(lambda: get_manager().cancel(session_id))
    return {"session": session.to_api()}


@router.post("/sessions/{session_id}/messages")
@_guard
async def agents_message(request: Request, session_id: str):
    _require_local(request)
    _enabled_or_raise()
    body = await _body(request)
    if set(body) - {"text"}:
        raise ValidationError("body may only contain text")

    def run():
        session, event = get_manager().send_message(session_id, body.get("text"))
        return JSONResponse({"session": session.to_api(), "event": event.to_dict()},
                            status_code=202)

    return await run_in_threadpool(run)


@router.delete("/sessions/{session_id}")
@_guard
async def agents_delete(request: Request, session_id: str):
    _require_local(request)
    await run_in_threadpool(lambda: get_manager().delete(session_id))
    return {"deleted": True}


@router.get("/sessions/{session_id}/events")
@_guard
async def agents_events(request: Request, session_id: str):
    after = _int_param(request, "after", 0, 0, 2**62)
    limit = _int_param(request, "limit", 200, 1, 500)
    wait = _wait_param(request)
    manager = await run_in_threadpool(get_manager)
    session = manager.store.require(session_id)
    waited = False
    if wait > 0 and session.last_seq <= after and session.state not in TERMINAL_STATES:
        def ready():
            current = manager.store.get(session_id)
            return current is None or current.last_seq > after or \
                current.state in TERMINAL_STATES

        waited = await _hold(ready, wait)

    def read():
        current = manager.store.require(session_id)
        events = manager.store.events(session_id, after=after, limit=limit + 1)
        has_more = len(events) > limit
        events = events[:limit]
        return {
            "events": [e.to_dict() for e in events],
            "next_after": events[-1].seq if events else after,
            "last_seq": current.last_seq,
            "has_more": has_more,
            "state": current.state,
            "terminal": current.state in TERMINAL_STATES,
            "waited": waited,
        }

    return await run_in_threadpool(read)


@router.get("/changes")
@_guard
async def agents_changes(request: Request):
    since = _int_param(request, "since", 0, 0, 2**62)
    wait = _wait_param(request)
    manager = await run_in_threadpool(get_manager)
    waited = False
    if wait > 0 and manager.store.revision <= since:
        waited = await _hold(lambda: manager.store.revision != since, wait)

    def read():
        changes = manager.store.changes(since)
        return {"revision": changes["revision"],
                "sessions": [s.to_summary() for s in changes["sessions"]],
                "deleted": changes["deleted"], "reset": changes["reset"],
                "counts": manager.store.counts(), "waited": waited}

    return await run_in_threadpool(read)


# -- hot commands ----------------------------------------------------------------------


def _rules_store():
    from localflow.agents.store import HotCommandStore

    return HotCommandStore(get_manager().store.root / "hot_commands.json")


@router.get("/hot-commands")
@_guard
async def agents_rules(request: Request):
    return await run_in_threadpool(lambda: {"rules": _rules_store().list()})


@router.post("/hot-commands")
@_guard
async def agents_rule_add(request: Request):
    from localflow.agents import rules

    _require_local(request)
    body = await _body(request)

    def run():
        manager = get_manager()
        rule = rules.add_rule(_rules_store(), body, _config(),
                              known_provider=manager.known_provider)
        return JSONResponse(rule, status_code=201)

    return await run_in_threadpool(run)


@router.put("/hot-commands/{rule_id}")
@_guard
async def agents_rule_update(request: Request, rule_id: str):
    from localflow.agents import rules

    _require_local(request)
    body = await _body(request)
    return await run_in_threadpool(lambda: rules.update_rule(
        _rules_store(), rule_id, body, _config(), known_provider=get_manager().known_provider))


@router.delete("/hot-commands/{rule_id}")
@_guard
async def agents_rule_delete(request: Request, rule_id: str):
    from localflow.agents import rules

    _require_local(request)
    await run_in_threadpool(lambda: rules.delete_rule(_rules_store(), rule_id))
    return {"deleted": True}


@router.post("/hot-commands/test")
@_guard
async def agents_rule_test(request: Request):
    from localflow.agents import handoff, hotcommands

    _require_local(request)
    _enabled_or_raise()
    body = await _body(request)
    text = body.get("text")
    if set(body) - {"text"} or not isinstance(text, str) or len(text) > 20000:
        raise ValidationError("body must be {\"text\": \"…\"} (at most 20000 characters)")

    def run():
        config = _config()
        if not config.agent_hot_commands:
            return hotcommands.HotCommandResult(False, "feature_off").to_dict()
        manager = get_manager()
        compiled = hotcommands.compile_rules(_rules_store().list(), config.agent_repos,
                                             config.agent_default_repo)
        result = hotcommands.evaluate(text, compiled, transform=handoff.prompt_transform(config),
                                      known_provider=manager.known_provider).to_dict()
        result["prompt"] = manager.redactor(result["prompt"])
        return result

    return await run_in_threadpool(run)


__all__ = ["API_VERSION", "AgentApiError", "agent_error_handler", "configure", "get_manager",
           "router", "shutdown"]
