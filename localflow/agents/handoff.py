"""Dictation-side seam: match a clip against hot-command rules and hand the
launch to the local server without ever blocking the dictation worker.

Imported by the dictation app only after the first clip processed with
`agents.enabled` and `agents.hot_commands` both on.
"""

from __future__ import annotations

import logging
import queue
import threading
import time
from collections.abc import Callable

from localflow.agents import hotcommands
from localflow.agents.models import new_session_id
from localflow.agents.store import HotCommandStore

log = logging.getLogger("localflow.agents.handoff")

COOLDOWN_SECONDS = 3.0
QUEUE_SIZE = 8
POST_TIMEOUT_SECONDS = 5
IDLE_SECONDS = 30.0


def prompt_transform(config, dictionary_store=None) -> Callable[[str], str]:
    """Dictionary replacements, then spoken formatting (no LLM, no symbols, no style)."""
    from localflow.dictionary import DictionaryStore, apply_replacements
    from localflow.formatting import apply_spoken_formatting

    store = dictionary_store if dictionary_store is not None else DictionaryStore()
    try:
        pairs = store.replacements()
    except Exception:  # noqa: BLE001 - an unreadable dictionary never blocks a launch
        pairs = []
    spoken = bool(getattr(config, "spoken_punctuation", True))

    def transform(text: str) -> str:
        text = apply_replacements(text, pairs)
        return apply_spoken_formatting(text) if spoken else text

    return transform


def _default_post(url: str, payload: dict, token: str | None):
    import requests

    session = requests.Session()
    session.trust_env = False
    headers = {"X-Localflow-Token": token} if token else {}
    try:
        return session.post(url, json=payload, headers=headers, timeout=POST_TIMEOUT_SECONDS,
                            allow_redirects=False)
    finally:
        session.close()


def _default_base_url() -> str:
    from localflow.server_url import configured_base_url

    return configured_base_url()


def _default_token() -> str | None:
    from localflow.apitoken import read

    return read()


def _default_play(cue: str) -> None:
    from localflow import sounds

    sounds.play(cue)


class HotCommandLauncher:
    """`submit` enqueues and returns at once; one daemon thread (started on a
    submit, gone again after IDLE_SECONDS without work) POSTs each launch to
    the loopback server."""

    def __init__(self, *, base_url: Callable[[], str] = _default_base_url,
                 token: Callable[[], str | None] = _default_token,
                 post: Callable = _default_post, play: Callable[[str], None] = _default_play,
                 sounds_enabled: Callable[[], bool] = lambda: True):
        self._base_url = base_url
        self._token = token
        self._post = post
        self._play = play
        self._sounds_enabled = sounds_enabled
        self._queue: queue.Queue = queue.Queue(maxsize=QUEUE_SIZE)
        self._thread: threading.Thread | None = None
        # The thread exits once idle this long; the next submit starts a new one.
        self.idle_seconds = IDLE_SECONDS
        self._lock = threading.Lock()

    def submit(self, match: hotcommands.HotCommandMatch) -> str | None:
        session_id = new_session_id()
        payload = {"id": session_id, "provider": match.provider, "prompt": match.prompt,
                   "origin": "hot_command", "rule_id": match.rule_id}
        if match.profile:
            payload["profile"] = match.profile
        if match.repo:
            payload["repo"] = match.repo
        try:
            self._queue.put_nowait(payload)
        except queue.Full:
            log.warning("hot command dropped: launch queue full (session %s)", session_id)
            return None
        with self._lock:
            if self._thread is None or not self._thread.is_alive():
                self._thread = threading.Thread(target=self._run, daemon=True,
                                                name="lf-agents-handoff")
                self._thread.start()
        return session_id

    def join(self, timeout: float = 5.0) -> None:
        """Wait until the queue drains (tests)."""
        deadline = time.monotonic() + timeout
        while self._queue.unfinished_tasks and time.monotonic() < deadline:
            time.sleep(0.01)

    def _cue(self, ok: bool) -> None:
        try:
            if self._sounds_enabled():
                self._play("lock" if ok else "error")
        except Exception:
            log.debug("hot command cue failed", exc_info=True)

    def _run(self) -> None:
        while True:
            try:
                payload = self._queue.get(timeout=self.idle_seconds)
            except queue.Empty:
                with self._lock:  # submit() re-checks the thread under this lock
                    if self._queue.empty():
                        self._thread = None
                        return
                continue
            try:
                self._deliver(payload)
            except Exception:
                log.exception("hot command handoff failed (session %s)", payload.get("id"))
                self._cue(False)
            finally:
                self._queue.task_done()

    def _deliver(self, payload: dict) -> None:
        url = self._base_url().rstrip("/") + "/api/agents/sessions"
        response = None
        for attempt in range(2):
            try:
                response = self._post(url, payload, self._token())
                break
            except Exception as exc:  # noqa: BLE001 - connection refused / timeout
                if attempt == 1:
                    log.warning("hot command not launched: server unreachable (%s; session %s, "
                                "provider %s, prompt %d chars)", type(exc).__name__,
                                payload["id"], payload["provider"], len(payload["prompt"]))
                    self._cue(False)
                    return
        status = getattr(response, "status_code", 0)
        if status in (200, 202):
            log.info("hot command launched session %s (provider %s, prompt %d chars)",
                     payload["id"], payload["provider"], len(payload["prompt"]))
            self._cue(True)
            return
        code = ""
        try:
            code = str(response.json().get("code", ""))[:40]
        except Exception:  # noqa: BLE001 - a non-JSON error body has no code
            code = ""
        log.warning("hot command not launched: HTTP %s %s (session %s, provider %s, prompt %d "
                    "chars)", status, code, payload["id"], payload["provider"],
                    len(payload["prompt"]))
        self._cue(False)


class HotCommandSeam:
    """What `app._process_job` calls per live dictation clip."""

    def __init__(self, store: HotCommandStore | None = None,
                 launcher: HotCommandLauncher | None = None, *,
                 clock: Callable[[], float] = time.monotonic,
                 known_provider: Callable[[str], bool] | None = None):
        self.store = store if store is not None else HotCommandStore()
        self.sounds = True
        self.launcher = launcher if launcher is not None else HotCommandLauncher(
            sounds_enabled=lambda: self.sounds)
        self._clock = clock
        self._last_launch: float | None = None
        if known_provider is None:
            from localflow.agents.registry import list_agent_ids

            ids = frozenset(list_agent_ids())
            known_provider = ids.__contains__
        self._known = known_provider
        self._compiled: tuple[object, hotcommands.CompiledRules] | None = None

    def _rules(self, config) -> hotcommands.CompiledRules | None:
        rules = self.store.list()
        if not rules:
            return None
        key = (self.store.file.signature(), tuple(config.agent_repos), config.agent_default_repo)
        if self._compiled is None or self._compiled[0] != key:
            self._compiled = (key, hotcommands.compile_rules(rules, config.agent_repos,
                                                             config.agent_default_repo))
        return self._compiled[1]

    def intercept(self, text: str, config, dictionary_store=None) -> bool:
        """True when `text` launched (or was handed off as) a hot command."""
        if not (getattr(config, "agents_enabled", False)
                and getattr(config, "agent_hot_commands", False)) or not text:
            return False
        compiled = self._rules(config)
        if compiled is None:
            return False
        result = hotcommands.evaluate(text, compiled,
                                      transform=prompt_transform(config, dictionary_store),
                                      known_provider=self._known)
        if not result.matched:
            if result.reason in ("unknown_repo", "unknown_provider"):
                log.warning("hot command %s ignored: %s", result.rule_id, result.reason)
            return False
        now = self._clock()
        if self._last_launch is not None and now - self._last_launch < COOLDOWN_SECONDS:
            log.info("hot command %s ignored: repeated within %.0f s", result.rule_id,
                     COOLDOWN_SECONDS)
            return False
        self._last_launch = now
        self.sounds = bool(getattr(config, "sounds_enabled", True))
        self.launcher.submit(result.as_match())
        return True
