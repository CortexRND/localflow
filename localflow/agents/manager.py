"""AgentManager: admission, concurrency, supervision, timeouts and recovery.

No timers and no polling while idle: admission runs on launch, on every
terminal transition and on recovery; one supervisor thread exists only while
a session is active and sleeps on a Condition until the next cloud poll or
deadline is due.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import threading
import time
from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path

from localflow.agents import registry
from localflow.agents.hotcommands import parse_repos
from localflow.agents.models import (
    ERROR_CODES,
    MAX_PROMPT_CHARS,
    MAX_RESULT_CHARS,
    MAX_TIMEOUT_SECONDS,
    MAX_TITLE_CHARS,
    MIN_TIMEOUT_SECONDS,
    ORIGINS,
    PROVIDER_OPTIONS,
    RUNNING_STATES,
    STATES,
    TERMINAL_STATES,
    AgentEvent,
    AgentsDisabled,
    AgentSession,
    IdConflict,
    LaunchRequest,
    NotFound,
    PromptTooLong,
    ProviderCapabilities,
    ProviderError,
    ProviderNotReady,
    QueueFull,
    Readiness,
    SessionConflict,
    Unsupported,
    UpstreamError,
    ValidationError,
    is_active,
    is_terminal,
    new_session_id,
    one_line_title,
)
from localflow.agents.providers.base import AgentProvider, ProviderContext
from localflow.agents.redact import Redactor
from localflow.agents.store import (
    AgentSessionStore,
    HttpProfileStore,
    decode_cursor,
    encode_cursor,
)

log = logging.getLogger("localflow.agents.manager")

STATIC_SECRET_NAMES = ("agent_devin_api_key", "agent_claude_api_key")
SESSION_ID_RE = re.compile(r"^[0-9a-f]{12}$")
MAX_POLL_BACKOFF = 60.0
MAX_POLL_FAILURES = 5
MAX_ERROR_CHARS = 500
_BROKEN_CAPS = ProviderCapabilities(kind="cloud", resume=False, live_message=False,
                                    cancel=False, streaming=False, needs_cwd=False,
                                    reattach=False)


def _digest(request: LaunchRequest) -> str:
    raw = json.dumps(request.digest_fields(), sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def _parse_ts(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=UTC).timestamp()
    except ValueError:
        return None


class _NullSink:
    """For best-effort stops after the session already ended."""

    def output(self, stream, text): pass
    def external(self, external_id, url=""): pass
    def state(self, to, **data): return False
    def usage(self, cost_usd=None, duration_ms=None, turns=None): pass
    def finish(self, state, result_text="", error=None): return False
    def current_state(self): return "cancelled"
    def note(self, **internal): pass


class Sink:
    """EventSink bound to one session: redacts, persists, notifies."""

    def __init__(self, manager: AgentManager, session_id: str):
        self._m = manager
        self.session_id = session_id

    def _limit(self) -> int:
        return int(self._m.config.agent_max_output_kb) * 1024

    def output(self, stream: str, text: str) -> None:
        if not text:
            return
        session = self._m.store.get(self.session_id)
        if session is None or is_terminal(session.state):
            return
        stream = stream if stream in ("agent", "stderr", "user", "system") else "agent"
        self._m.store.append_event(self.session_id, "output", stream=stream,
                                   text=self._m.redactor(text), limit_bytes=self._limit())
        self._m._notify()

    def external(self, external_id: str, url: str = "") -> None:
        session = self._m.store.get(self.session_id)
        if session is None:
            return
        external_id = self._m.redactor.redact_literals(str(external_id))[:200]
        url = self._m.redactor(str(url))[:500] if url else session.external_url
        if session.external_id == external_id and session.external_url == url:
            return
        self._m.store.update(self.session_id, external_id=external_id, external_url=url)
        self._m._notify()

    def state(self, to: str, **data) -> bool:
        if to not in STATES or to in TERMINAL_STATES:
            return False
        try:
            self._m.store.transition(self.session_id, to, event_data={
                k: str(v)[:80] for k, v in data.items()})
        except (SessionConflict, NotFound):
            return False
        self._m._notify()
        return True

    def usage(self, cost_usd=None, duration_ms=None, turns=None) -> None:
        def num(value, kind):
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                return None
            return kind(value)

        usage = {"cost_usd": num(cost_usd, float), "duration_ms": num(duration_ms, int),
                 "turns": num(turns, int)}
        try:
            self._m.store.update(self.session_id, usage=usage)
            self._m.store.append_event(self.session_id, "usage", data=usage,
                                       limit_bytes=self._limit())
        except NotFound:
            return
        self._m._notify()

    def finish(self, state: str, result_text: str = "", error: dict | None = None) -> bool:
        return self._m._finish(self.session_id, state, result_text, error)

    def current_state(self) -> str:
        session = self._m.store.get(self.session_id)
        return session.state if session else "failed"

    def note(self, **internal) -> None:
        try:
            self._m.store.update(self.session_id, internal=internal)
        except NotFound:
            pass


class AgentManager:
    def __init__(
        self,
        get_config: Callable[[], object],
        *,
        store: AgentSessionStore | None = None,
        profiles: HttpProfileStore | None = None,
        ctx: ProviderContext | None = None,
        provider_classes: dict[str, type] | None = None,
        supervise: bool = True,
        inline_start: bool = False,
        clock: Callable[[], float] = time.monotonic,
        recover: bool = True,
    ):
        self._get_config = get_config
        self.store = store if store is not None else AgentSessionStore()
        self.profiles = profiles if profiles is not None else HttpProfileStore(
            self.store.root / "http_providers.json")
        if ctx is None:
            from localflow.secrets import get_keyring_secret, get_secret

            ctx = ProviderContext(get_secret=get_secret, get_stored_secret=get_keyring_secret)
        self.ctx = ctx
        self.ctx.profiles = self.profiles
        resolve = getattr(ctx.get_secret, "_lf_resolve", ctx.get_secret)
        self.redactor = Redactor(names=self.secret_names, get_secret=resolve)
        self.ctx.redactor = self.redactor
        self.ctx.get_secret = self._redacting_get_secret(resolve)
        self._classes = dict(provider_classes or {})
        self._providers: dict[tuple[str, str], tuple[str, AgentProvider]] = {}
        self._lock = threading.RLock()
        self._cond = threading.Condition(self._lock)
        self._tracked: dict[str, dict] = {}
        # The provider (and its HTTP profile) a session started with; a profile edit
        # never changes how an active session is polled, messaged or cancelled.
        self._session_providers: dict[str, AgentProvider] = {}
        self._supervisor: threading.Thread | None = None
        self._listeners: list[Callable[[], None]] = []
        self._closed = False
        self.supervise = supervise
        self.inline_start = inline_start
        self._clock = clock
        if recover:
            self._recover()

    def _redacting_get_secret(self, resolve: Callable[[str], str]) -> Callable[[str], str]:
        """Secrets are resolved at use time, so a key changed elsewhere (`lf agents secret
        set` while the server runs) joins the redaction table before a provider sends it."""
        redactor = self.redactor

        def get_secret(name: str) -> str:
            value = resolve(name) or ""
            if value:
                redactor.add(name, value)
            return value

        get_secret._lf_resolve = resolve
        return get_secret

    # -- config / providers ---------------------------------------------------

    @property
    def config(self):
        return self._get_config()

    def secret_names(self) -> list[str]:
        from localflow.agents.providers.http import secret_name

        names = list(STATIC_SECRET_NAMES)
        for profile in self.profiles.list():
            if isinstance(profile.get("id"), str):
                names.append(secret_name(profile["id"]))
        return names

    def provider_ids(self) -> list[str]:
        return sorted({*registry.list_agent_ids(), *self._classes})

    def known_provider(self, provider_id: str) -> bool:
        return provider_id in self.provider_ids()

    def provider_class(self, provider_id: str):
        if provider_id in self._classes:
            return self._classes[provider_id]
        return registry.agent_provider_class(provider_id)

    def profile(self, provider_id: str, profile_id: str) -> dict | None:
        if provider_id != "http" or not profile_id:
            return None
        return self.profiles.get(profile_id)

    def capabilities(self, provider_id: str, profile_id: str = "") -> ProviderCapabilities:
        try:
            cls = self.provider_class(provider_id)
            return cls.capabilities_for(self.config, self.profile(provider_id, profile_id))
        except Exception:  # noqa: BLE001 - a broken entry point never raises into a route
            return _BROKEN_CAPS

    def provider_for(self, provider_id: str, profile_id: str = "") -> AgentProvider:
        profile = self.profile(provider_id, profile_id)
        stamp = json.dumps(profile, sort_keys=True) if profile else ""
        key = (provider_id, profile_id)
        with self._lock:
            cached = self._providers.get(key)
            if cached is not None and cached[0] == stamp:
                cached[1].config = self.config
                return cached[1]
            provider = self.provider_class(provider_id)(self.config, self.ctx, profile)
            self._providers[key] = (stamp, provider)
            return provider

    def _session_provider(self, session: AgentSession) -> AgentProvider:
        with self._lock:
            provider = self._session_providers.get(session.id)
            if provider is not None:
                provider.config = self.config
                return provider
        return self.provider_for(session.effective_provider, session.profile)

    def _pin_provider(self, session_id: str, provider: AgentProvider) -> AgentProvider:
        """Pin the provider (profile snapshot and, for HTTP, its token) for a tracked run."""
        pinned = provider.pinned()
        with self._lock:
            if session_id in self._tracked:
                self._session_providers[session_id] = pinned
        return pinned

    def _session_capabilities(self, session: AgentSession) -> ProviderCapabilities:
        """Capabilities of the provider the session is pinned to, not the live profile."""
        try:
            provider = self._session_provider(session)
            return type(provider).capabilities_for(self.config, provider.profile)
        except Exception:  # noqa: BLE001 - a broken entry point never raises into a route
            return _BROKEN_CAPS

    def _forget(self, session_id: str) -> None:
        with self._lock:
            self._tracked.pop(session_id, None)
            self._session_providers.pop(session_id, None)

    def readiness(self, provider_id: str, profile_id: str = "", *, deep: bool = False
                  ) -> Readiness:
        try:
            cls = self.provider_class(provider_id)
            if deep:
                provider = self.provider_for(provider_id, profile_id)
            else:
                provider = cls(self.config, self.ctx, self.profile(provider_id, profile_id))
            result = provider.readiness(deep=deep)
        except Exception as exc:  # noqa: BLE001
            log.warning("agent provider %s unavailable (%s)", provider_id, type(exc).__name__)
            return Readiness(False, "disabled", "This provider could not be loaded.")
        return Readiness(result.ready, result.code, self.redactor(result.detail))

    def provider_entries(self, *, deep_for: tuple[str, str] | None = None) -> list[dict]:
        entries = []
        for provider_id in self.provider_ids():
            profiles = [""]
            if provider_id == "http":
                profiles = [p["id"] for p in self.profiles.list() if p.get("id")] or [""]
            for profile_id in profiles:
                entries.append(self.provider_entry(
                    provider_id, profile_id, deep=deep_for == (provider_id, profile_id)))
        return entries

    def provider_entry(self, provider_id: str, profile_id: str = "", *, deep: bool = False
                       ) -> dict:
        profile = self.profile(provider_id, profile_id)
        try:
            cls = self.provider_class(provider_id)
            label = cls.label
            names = cls.secret_names_for(profile)
            required = set(cls.required_secrets(self.config, profile))
        except Exception:  # noqa: BLE001
            label, names, required = provider_id, (), set()
        if profile:
            label = str(profile.get("label") or label)
        return {
            "id": provider_id,
            "profile": profile_id,
            "label": label,
            "builtin": registry.is_builtin(provider_id),
            "capabilities": self.capabilities(provider_id, profile_id).to_dict(),
            "secrets": [{"name": name, "set": bool(self.ctx.get_secret(name)),
                         "required": name in required} for name in names],
            "readiness": self.readiness(provider_id, profile_id, deep=deep).to_dict(
                "deep" if deep else "shallow"),
        }

    # -- notification ---------------------------------------------------------

    def subscribe(self, callback: Callable[[], None]) -> None:
        self._listeners.append(callback)

    def _notify(self) -> None:
        for callback in list(self._listeners):
            try:
                callback()
            except Exception:
                log.debug("agents change listener failed", exc_info=True)

    # -- launch ---------------------------------------------------------------

    def _repo_paths(self, config) -> dict[str, Path]:
        result = {}
        for alias, path in parse_repos(getattr(config, "agent_repos", [])).items():
            try:
                result[alias] = Path(path).expanduser().resolve()
            except (OSError, RuntimeError):
                continue
        return result

    @staticmethod
    def _inside_repos(cwd: str, repos: dict[str, Path]) -> bool:
        try:
            resolved = Path(cwd).resolve()
        except (OSError, RuntimeError):
            return False
        return os.path.isdir(resolved) and any(
            resolved == root or root in resolved.parents for root in repos.values())

    def _build_session(self, request: LaunchRequest, config) -> AgentSession:
        prompt = request.prompt
        if not prompt.strip():
            raise ValidationError("prompt is empty")
        if len(prompt) > MAX_PROMPT_CHARS:
            raise PromptTooLong(f"prompt is too long (max {MAX_PROMPT_CHARS} characters)")
        if request.id and not SESSION_ID_RE.match(request.id):
            raise ValidationError("id must be 12 lowercase hex characters")
        provider = request.provider or config.agent_default_provider
        if not self.known_provider(provider):
            raise ValidationError(f"unknown provider {provider[:40]!r}")
        sandbox = bool(config.agent_sandbox)
        if provider == "mock" and not sandbox:
            raise ValidationError("the mock provider only runs in sandbox mode")
        effective = "mock" if sandbox else provider
        if provider == "http":
            if not request.profile or self.profile("http", request.profile) is None:
                raise ValidationError("choose an existing HTTP agent profile")
        elif request.profile:
            raise ValidationError("profile is only used by the http provider")
        if request.origin not in ORIGINS:
            raise ValidationError(f"origin must be one of {', '.join(ORIGINS)}")
        if len(request.rule_id) > 16:
            raise ValidationError("rule_id is too long")
        allowed = PROVIDER_OPTIONS.get(provider, frozenset()) | PROVIDER_OPTIONS.get(
            effective, frozenset())
        for name, value in request.options.items():
            if name not in allowed:
                raise ValidationError(f"unknown option {name[:40]!r} for provider {provider}")
            if not isinstance(value, str) or len(value) > 80:
                raise ValidationError(f"option {name} must be a short string")
        if "model" in request.options:
            from localflow.agents.providers.claude import MODEL_RE

            if not MODEL_RE.match(request.options["model"]):
                raise ValidationError("model contains unsupported characters")
        if "script" in request.options:
            from localflow.agents.providers.mock import SCRIPTS

            if request.options["script"] not in SCRIPTS:
                raise ValidationError(f"script must be one of {', '.join(SCRIPTS)}")

        caps = self.capabilities(provider, request.profile)
        effective_caps = self.capabilities(effective, request.profile)
        repos = self._repo_paths(config)
        repo, cwd = request.repo, request.cwd
        parent = None
        internal: dict = {"effective_provider": effective, "options": dict(request.options)}
        if request.resume_of:
            if not effective_caps.resume:
                raise Unsupported("this provider cannot resume a session")
            parent = self.store.get(request.resume_of)
            if parent is None or not is_terminal(parent.state) or parent.provider != provider \
                    or parent.profile != request.profile or not parent.external_id:
                raise ValidationError("resume_of must name a finished session of the same "
                                      "provider")
            internal["resume_external_id"] = parent.external_id
            if not repo and not cwd:
                repo, cwd = parent.repo, parent.cwd
                if cwd and self._inside_repos(cwd, repos):
                    repo = ""  # keep the parent's (possibly nested) folder
                elif repo and repo in repos:
                    cwd = ""
        if repo and cwd:
            raise ValidationError("give either repo or cwd, not both")
        if cwd:
            if not os.path.isabs(cwd):
                raise ValidationError("cwd must be an absolute path")
            try:
                resolved = Path(cwd).resolve()
            except (OSError, RuntimeError):
                raise ValidationError("cwd cannot be resolved") from None
            match = next((alias for alias, root in repos.items()
                          if resolved == root or root in resolved.parents), None)
            if match is None:
                raise ValidationError("cwd must be inside a configured repo (agents.repos)")
            repo, cwd = match, str(resolved)
        elif repo:
            if repo not in repos:
                raise ValidationError(f"unknown repo {repo[:40]!r}")
            cwd = str(repos[repo])
        elif config.agent_default_repo and config.agent_default_repo in repos:
            repo = config.agent_default_repo
            cwd = str(repos[repo])
        if caps.needs_cwd:
            if not cwd:
                raise ValidationError("choose a repo for this provider (agents.repos)")
            if not os.path.isdir(cwd):
                raise ValidationError("the repo folder does not exist")
        title = request.title or one_line_title(prompt)
        if len(title) > MAX_TITLE_CHARS:
            raise ValidationError(f"title is too long (max {MAX_TITLE_CHARS} characters)")
        timeout = request.timeout_seconds
        if timeout is None:
            timeout = int(config.agent_timeout_seconds)
        if not MIN_TIMEOUT_SECONDS <= timeout <= MAX_TIMEOUT_SECONDS:
            raise ValidationError(
                f"timeout_seconds must be {MIN_TIMEOUT_SECONDS} to {MAX_TIMEOUT_SECONDS}")
        internal["digest"] = _digest(request)
        session_id = request.id or new_session_id()
        return AgentSession(
            id=session_id, provider=provider, profile=request.profile, origin=request.origin,
            rule_id=request.rule_id, title=self.redactor(" ".join(title.split())),
            prompt=self.redactor(prompt), repo=repo, cwd=cwd,
            parent_id=parent.id if parent else "",
            root_id=(parent.root_id or parent.id) if parent else session_id,
            sandbox=sandbox, timeout_seconds=timeout, internal=internal,
        )

    def launch(self, body: dict) -> tuple[AgentSession, bool]:
        """Validate and queue a launch. Returns (session, created)."""
        config = self.config
        if not config.agents_enabled:
            raise AgentsDisabled()
        request = LaunchRequest.from_body(body)
        with self._lock:
            if request.id:
                existing = self.store.get(request.id)
                if existing is not None:
                    if existing.internal.get("digest") == _digest(request):
                        return existing, False
                    raise IdConflict("a session with this id exists with a different request")
            session = self._build_session(request, config)
            readiness = self.readiness(session.effective_provider, session.profile)
            if not readiness.ready:
                raise ProviderNotReady(readiness)
            # Queued-but-not-yet-admitted sessions count too: admission runs after the
            # lock is released, so concurrent launches must not all see a free slot.
            sessions = self.store.list()
            active = sum(1 for s in sessions if s.state == "queued" or
                         s.state in RUNNING_STATES)
            if active >= config.agent_max_concurrent + config.agent_max_queued:
                raise QueueFull()
            created = self.store.create(session)
        log.info("agent session %s queued (provider %s, origin %s, prompt %d chars)",
                 created.id, created.provider, created.origin, len(created.prompt))
        self._notify()
        self._admit()
        return created, True

    # -- admission and start --------------------------------------------------

    def _admit(self) -> None:
        to_start: list[str] = []
        failed: list[str] = []
        with self._lock:
            if self._closed:
                return
            config = self.config
            sessions = self.store.list()
            running = sum(1 for s in sessions if s.state in RUNNING_STATES)
            for session in (s for s in sessions if s.state == "queued"):
                if running >= config.agent_max_concurrent:
                    break
                readiness = self.readiness(session.effective_provider, session.profile)
                error = None
                if not readiness.ready:
                    error = {"code": "provider_not_ready", "message": readiness.detail}
                elif self.capabilities(session.provider, session.profile).needs_cwd and \
                        not os.path.isdir(session.cwd or ""):
                    error = {"code": "spawn_failed", "message": "the repo folder does not exist"}
                if error is not None:
                    try:
                        self.store.transition(session.id, "failed", expect=("queued",),
                                              error=error, event_data={"code": error["code"]})
                        failed.append(session.id)
                    except SessionConflict:
                        pass
                    continue
                try:
                    self.store.transition(session.id, "starting", expect=("queued",))
                except SessionConflict:
                    continue
                running += 1
                self._tracked[session.id] = {
                    "deadline": self._clock() + session.timeout_seconds,
                    "next_poll": None, "base": None, "interval": None, "failures": 0,
                }
                to_start.append(session.id)
            if to_start:
                self._ensure_supervisor()
        if to_start or failed:
            self._notify()
        for session_id in to_start:
            if self.inline_start:
                self._start(session_id)
            else:
                threading.Thread(target=self._start, args=(session_id,), daemon=True,
                                 name=f"lf-agents-start-{session_id}").start()

    def _start(self, session_id: str) -> None:
        session = self.store.get(session_id)
        if session is None or session.state != "starting":
            return
        sink = Sink(self, session_id)
        provider = None
        try:
            provider = self._pin_provider(
                session_id, self.provider_for(session.effective_provider, session.profile))
            provider.start(session, sink)
        except ProviderError as exc:
            code = "spawn_failed" if exc.code == "internal" else exc.code
            self._finish(session_id, "failed", error={"code": code, "message": exc.message})
            return
        except Exception as exc:
            log.exception("agent session %s failed to start", session_id)
            self._finish(session_id, "failed", error={
                "code": "spawn_failed", "message": f"the provider failed ({type(exc).__name__})"})
            return
        current = self.store.get(session_id)
        if current is None:
            return
        if current.state == "starting":
            try:
                current = self.store.transition(session_id, "running", expect=("starting",))
                log.info("agent session %s running", session_id)
            except SessionConflict:
                current = self.store.get(session_id) or current
        if current.state in ("cancelling", "cancelled", "timed_out") and provider is not None:
            # Ended (cancel or deadline) while start() was in flight: stop the run it
            # just created so it is not orphaned.
            self._stop_quietly(provider, current)
        caps = self.capabilities(current.effective_provider, current.profile)
        with self._lock:
            tracked = self._tracked.get(session_id)
            if tracked is not None and caps.kind == "cloud" and is_active(current.state):
                base = (self.ctx.mock_poll_seconds if current.effective_provider == "mock"
                        else float(self.config.agent_poll_seconds))
                tracked.update(base=base, interval=base, next_poll=self._clock() + base)
                self._cond.notify_all()
        self._notify()

    def _stop_quietly(self, provider: AgentProvider, session: AgentSession) -> None:
        try:
            stop = getattr(provider, "stop", None)
            if stop is not None:
                stop(session)
            elif self.capabilities(session.effective_provider, session.profile).cancel:
                provider.cancel(session, _NullSink())
        except Exception:  # noqa: BLE001
            log.warning("agent session %s: stopping the provider failed", session.id)

    # -- terminal -------------------------------------------------------------

    def _finish(self, session_id: str, state: str, result_text: str = "",
                error: dict | None = None) -> bool:
        if state not in TERMINAL_STATES:
            raise ValueError(f"not a terminal state: {state}")
        session = self.store.get(session_id)
        if session is None or is_terminal(session.state):
            return False
        fields: dict = {}
        if result_text:
            text = self.redactor(str(result_text))
            fields["result_text"] = text[:MAX_RESULT_CHARS]
            fields["result_truncated"] = len(text) > MAX_RESULT_CHARS
        event_data: dict = {}
        if error:
            code = error.get("code") if error.get("code") in ERROR_CODES else "internal"
            message = self.redactor(str(error.get("message") or code))[:MAX_ERROR_CHARS]
            fields["error"] = {"code": code, "message": message}
            event_data = {"code": code}
            try:
                self.store.append_event(session_id, "error", text=message, data={"code": code},
                                        limit_bytes=int(self.config.agent_max_output_kb) * 1024)
            except NotFound:
                return False
        try:
            if session.state == "starting" and state in ("completed", "timed_out"):
                self.store.transition(session_id, "running", expect=("starting",))
            self.store.transition(session_id, state, event_data=event_data, **fields)
        except (SessionConflict, NotFound):
            return False
        log.info("agent session %s %s (provider %s%s)", session_id, state, session.provider,
                 f", {fields['error']['code']}" if "error" in fields else "")
        self._on_terminal(session_id)
        return True

    def _on_terminal(self, session_id: str) -> None:
        with self._lock:
            self._tracked.pop(session_id, None)
            self._session_providers.pop(session_id, None)
            self._cond.notify_all()
            try:
                removed = self.store.prune(int(self.config.agent_max_sessions))
            except Exception:
                removed = []
                log.warning("pruning agent sessions failed", exc_info=True)
        if removed:
            log.info("pruned %d old agent session(s)", len(removed))
        self._notify()
        self._admit()

    # -- operations -----------------------------------------------------------

    def cancel(self, session_id: str) -> AgentSession:
        for _attempt in range(3):
            session = self.store.require(session_id)
            if session.state in ("cancelling", "cancelled"):
                return session
            if is_terminal(session.state):
                raise SessionConflict(session_id, session.state,
                                      ("queued", "starting", "running", "waiting_input"))
            if session.state in ("queued", "starting"):
                try:
                    self.store.transition(session_id, "cancelled",
                                          expect=(session.state,), cancel_requested=True)
                except SessionConflict:
                    continue
                log.info("agent session %s cancelled", session_id)
                self._on_terminal(session_id)
                return self.store.require(session_id)
            try:
                self.store.transition(session_id, "cancelling", expect=("running", "waiting_input"),
                                      cancel_requested=True)
            except SessionConflict:
                continue
            self._notify()
            current = self.store.require(session_id)
            sink = Sink(self, session_id)
            if not self._session_capabilities(current).cancel:
                sink.output("system", "the remote run was not stopped: the provider has no "
                                      "cancel operation")
                self._finish(session_id, "cancelled")
                return self.store.require(session_id)
            try:
                self._session_provider(current).cancel(current, sink)
            except ProviderError as exc:
                self._finish(session_id, "failed",
                             error={"code": "cancel_failed", "message": exc.message})
            except Exception as exc:  # noqa: BLE001
                log.warning("agent session %s: cancel failed (%s)", session_id,
                            type(exc).__name__)
                self._finish(session_id, "failed", error={
                    "code": "cancel_failed", "message": "the provider could not stop the run"})
            return self.store.require(session_id)
        raise SessionConflict(session_id, self.store.require(session_id).state, ("running",))

    def send_message(self, session_id: str, text: object) -> tuple[AgentSession, AgentEvent]:
        if not self.config.agents_enabled:
            raise AgentsDisabled()
        if not isinstance(text, str) or not text.strip():
            raise ValidationError("text must be a non-empty string")
        if len(text) > MAX_PROMPT_CHARS:
            raise PromptTooLong(f"message is too long (max {MAX_PROMPT_CHARS} characters)")
        session = self.store.require(session_id)
        if not self._session_capabilities(session).live_message:
            raise Unsupported("this provider does not accept follow-up messages")
        if session.state not in ("running", "waiting_input"):
            raise SessionConflict(session_id, session.state, ("running", "waiting_input"))
        sink = Sink(self, session_id)
        try:
            self._session_provider(session).send_message(session, text, sink)
        except ProviderError as exc:
            raise UpstreamError(ProviderError(exc.code, self.redactor(exc.message))) from None
        event = self.store.append_event(session_id, "message", stream="user",
                                        text=self.redactor(text))
        if session.state == "waiting_input":
            try:
                self.store.transition(session_id, "running", expect=("waiting_input",))
            except SessionConflict:
                pass
        with self._lock:
            tracked = self._tracked.get(session_id)
            if tracked is not None and tracked.get("base") is not None:
                tracked.update(interval=tracked["base"], next_poll=self._clock())
                self._cond.notify_all()
        self._notify()
        return self.store.require(session_id), event

    def delete(self, session_id: str) -> None:
        if not self.store.delete(session_id):
            raise NotFound(f"no agent session {session_id}")
        self._notify()

    # -- supervisor -----------------------------------------------------------

    def _ensure_supervisor(self) -> None:
        if not self.supervise or self._closed:
            return
        if self._supervisor is not None and self._supervisor.is_alive():
            self._cond.notify_all()
            return
        self._supervisor = threading.Thread(target=self._supervise, daemon=True,
                                            name="lf-agents-supervisor")
        self._supervisor.start()

    def _due(self, now: float) -> list[tuple[str, str]]:
        due = []
        for session_id, tracked in self._tracked.items():
            if tracked["deadline"] <= now:
                due.append(("expire", session_id))
            elif tracked["next_poll"] is not None and tracked["next_poll"] <= now:
                due.append(("poll", session_id))
        return due

    def _next_wake(self) -> float | None:
        times = []
        for tracked in self._tracked.values():
            times.append(tracked["deadline"])
            if tracked["next_poll"] is not None:
                times.append(tracked["next_poll"])
        return min(times) if times else None

    def _supervise(self) -> None:
        while True:
            with self._cond:
                if self._closed or not self._tracked:
                    if self._supervisor is threading.current_thread():
                        self._supervisor = None
                    return
                now = self._clock()
                due = self._due(now)
                if not due:
                    wake = self._next_wake()
                    self._cond.wait(timeout=None if wake is None else max(0.0, wake - now))
                    continue
            self._run_due(due)

    def tick(self) -> int:
        """Run whatever is due now (tests drive the manager with this)."""
        with self._lock:
            due = self._due(self._clock())
        self._run_due(due)
        return len(due)

    def _run_due(self, due: list[tuple[str, str]]) -> None:
        for action, session_id in due:
            try:
                if action == "expire":
                    self._expire(session_id)
                else:
                    self._poll(session_id)
            except Exception:
                log.exception("agent session %s: supervisor step failed", session_id)
                with self._lock:
                    tracked = self._tracked.get(session_id)
                    if tracked is not None and tracked.get("next_poll") is not None:
                        tracked["next_poll"] = self._clock() + MAX_POLL_BACKOFF

    def _expire(self, session_id: str) -> None:
        session = self.store.get(session_id)
        if session is None or not is_active(session.state):
            self._forget(session_id)
            return
        if session.state == "cancelling":
            with self._lock:
                tracked = self._tracked.get(session_id)
                if tracked is not None:
                    tracked["deadline"] = self._clock() + MAX_POLL_BACKOFF
            return
        try:
            provider = self._session_provider(session)
        except Exception:  # noqa: BLE001
            provider = None
        ended = self._finish(session_id, "timed_out", error={
            "code": "timeout", "message": f"no result within {session.timeout_seconds} s"})
        if ended and provider is not None:
            self._stop_quietly(provider, self.store.get(session_id) or session)

    def _poll(self, session_id: str) -> None:
        session = self.store.get(session_id)
        if session is None or is_terminal(session.state):
            self._forget(session_id)
            return
        with self._lock:
            tracked = self._tracked.get(session_id)
            if tracked is None:
                return
            tracked["next_poll"] = None
        if session.state not in ("running", "waiting_input"):
            return
        before = (session.state, session.last_seq)
        sink = Sink(self, session_id)
        retry_after = None
        try:
            returned = self._session_provider(session).poll(session, sink)
            failures = 0
        except ProviderError as exc:
            returned = None
            failures = tracked["failures"]
            if exc.code == "rate_limited":
                retry_after = exc.retry_after or (tracked["interval"] or 1.0) * 2
            elif exc.retryable:
                failures += 1
                if failures >= MAX_POLL_FAILURES:
                    self._finish(session_id, "failed", error={
                        "code": "upstream_error",
                        "message": f"{exc.message} ({failures} attempts)"})
                    return
            else:
                self._finish(session_id, "failed", error={"code": exc.code,
                                                          "message": exc.message})
                return
        after = self.store.get(session_id)
        if after is None or is_terminal(after.state):
            return
        with self._lock:
            tracked = self._tracked.get(session_id)
            if tracked is None:
                return
            tracked["failures"] = failures
            base = float(returned) if returned else (tracked["base"] or 1.0)
            tracked["base"] = base
            if retry_after is not None:
                interval = float(retry_after)
            elif (after.state, after.last_seq) == before:
                interval = min(max(tracked["interval"] or base, base) * 1.5, MAX_POLL_BACKOFF)
            else:
                interval = base
            tracked["interval"] = interval
            tracked["next_poll"] = self._clock() + interval
            self._cond.notify_all()

    @property
    def supervisor_alive(self) -> bool:
        thread = self._supervisor
        return thread is not None and thread.is_alive()

    # -- recovery / shutdown --------------------------------------------------

    def _recover(self) -> None:
        now_wall = time.time()
        reattached = 0
        for session in self.store.list():
            if not is_active(session.state):
                continue
            caps = self.capabilities(session.effective_provider, session.profile)
            if session.state in ("queued", "starting", "cancelling") or not caps.reattach:
                try:
                    self.store.transition(session.id, "failed", error={
                        "code": "interrupted",
                        "message": "LocalFlow restarted while this session was active"},
                        event_data={"code": "interrupted"})
                except SessionConflict:
                    pass
                continue
            started = _parse_ts(session.started_at) or now_wall
            remaining = max(0.0, session.timeout_seconds - (now_wall - started))
            base = (self.ctx.mock_poll_seconds if session.effective_provider == "mock"
                    else float(self.config.agent_poll_seconds))
            self._tracked[session.id] = {"deadline": self._clock() + remaining,
                                         "next_poll": self._clock(), "base": base,
                                         "interval": base, "failures": 0}
            try:
                self._pin_provider(session.id, self.provider_for(session.effective_provider,
                                                                 session.profile))
            except Exception:  # noqa: BLE001 - polling falls back to the live profile
                log.warning("agent session %s: could not pin its provider", session.id)
            reattached += 1
        if reattached:
            log.info("re-attached %d agent session(s)", reattached)
            with self._lock:
                self._ensure_supervisor()
        self._admit()

    def shutdown(self) -> None:
        with self._lock:
            self._closed = True
            self._cond.notify_all()
            providers = [p for _stamp, p in self._providers.values()]
        for session in self.store.list():
            if is_active(session.state) and not self.capabilities(
                    session.effective_provider, session.profile).reattach:
                try:
                    self.store.transition(session.id, "failed", error={
                        "code": "interrupted", "message": "LocalFlow stopped"},
                        event_data={"code": "interrupted"})
                except SessionConflict:
                    pass
        for provider in providers:
            try:
                provider.close()
            except Exception:
                log.debug("closing an agent provider failed", exc_info=True)

    # -- queries --------------------------------------------------------------

    def status(self) -> dict:
        config = self.config
        repos = parse_repos(config.agent_repos)
        return {
            "api_version": 1,
            "enabled": bool(config.agents_enabled),
            "hot_commands": bool(config.agent_hot_commands),
            "sandbox": bool(config.agent_sandbox),
            "allow_remote": bool(config.agent_allow_remote),
            "default_provider": config.agent_default_provider,
            "default_repo": config.agent_default_repo,
            "repos": [{"alias": alias, "path": path,
                       "exists": os.path.isdir(os.path.expanduser(path))}
                      for alias, path in repos.items()],
            "limits": {"max_concurrent": config.agent_max_concurrent,
                       "max_queued": config.agent_max_queued,
                       "max_prompt_chars": MAX_PROMPT_CHARS, "max_wait_seconds": 25},
            "counts": self.store.counts(),
            "revision": self.store.revision,
        }

    def list_sessions(self, *, states: set[str] | None = None, provider: str = "",
                      origin: str = "", root_id: str = "", q: str = "", limit: int = 50,
                      before: str = "") -> tuple[list[AgentSession], str | None]:
        sessions = self.store.list()
        if states:
            sessions = [s for s in sessions if s.state in states]
        if provider:
            sessions = [s for s in sessions if s.provider == provider]
        if origin:
            sessions = [s for s in sessions if s.origin == origin]
        if root_id:
            sessions = [s for s in sessions if s.root_id == root_id]
        if q:
            needle = q.casefold()
            sessions = [s for s in sessions if needle in s.title.casefold()]
        sessions.sort(key=lambda s: (s.created_at, s.id), reverse=True)
        if before:
            try:
                cursor = decode_cursor(before)
            except (ValueError, UnicodeError):
                raise ValidationError("before is not a valid cursor") from None
            sessions = [s for s in sessions if (s.created_at, s.id) < cursor]
        page = sessions[:limit]
        next_cursor = encode_cursor(page[-1].created_at, page[-1].id) \
            if len(sessions) > limit else None
        return page, next_cursor
