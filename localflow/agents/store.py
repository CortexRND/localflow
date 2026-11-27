"""Persistence for agent sessions, event logs, hot-command rules and HTTP profiles.

Same storage pattern as `HotPhraseStore` / `PromptQueue`: a threading lock plus
an flock on a sidecar `.lock` file around each read-modify-write, atomic writes
through a per-pid temp file and `os.replace`, and an unreadable or missing file
reads as empty. Without `fcntl` (Windows) or when the lock cannot be taken the
stores degrade to in-process locking.

The server is the only writer of `sessions.json` and `events/`; it keeps the
sessions in memory and writes the whole index through on every change.
"""

from __future__ import annotations

import base64
import copy
import json
import logging
import os
import threading
from collections.abc import Callable
from contextlib import contextmanager
from pathlib import Path

try:
    import fcntl
except ImportError:  # non-POSIX; the stores fall back to in-process locking
    fcntl = None

from localflow.agents.models import (
    MAX_EVENT_TEXT,
    TERMINAL_STATES,
    TRANSITIONS,
    AgentEvent,
    AgentSession,
    NotFound,
    SessionConflict,
    utc_now,
)

log = logging.getLogger("localflow.agents.store")

STORE_VERSION = 1
EVENT_RESERVE_BYTES = 32 * 1024
MAX_TOMBSTONES = 1000


def agents_dir() -> Path:
    override = os.environ.get("LOCALFLOW_AGENTS_DIR")
    if override:
        return Path(override)
    return Path.home() / ".localflow" / "agents"


def _ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    try:
        path.chmod(0o700)
    except OSError:
        pass


class LockedJsonFile:
    """One JSON document guarded by a thread lock and a sidecar flock."""

    def __init__(self, path: Path):
        self.path = Path(path)
        self._lock = threading.RLock()
        self._lock_path = self.path.with_suffix(self.path.suffix + ".lock")

    @contextmanager
    def exclusive(self):
        with self._lock:
            handle = None
            if fcntl is not None:
                try:
                    _ensure_dir(self.path.parent)
                    fd = os.open(self._lock_path, os.O_RDWR | os.O_CREAT, 0o600)
                    handle = os.fdopen(fd, "a+")
                    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                except OSError:
                    log.warning("agents lock unavailable, proceeding unlocked: %s",
                                self._lock_path)
                    if handle is not None:
                        handle.close()
                        handle = None
            try:
                yield
            finally:
                if handle is not None:
                    try:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
                    finally:
                        handle.close()

    def read(self, default):
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return default
        except Exception:  # noqa: BLE001 - unreadable reads as empty
            log.warning("agents file unreadable; treating as empty: %s", self.path)
            return default
        return data if isinstance(data, type(default)) else default

    def write(self, data) -> None:
        _ensure_dir(self.path.parent)
        tmp = self.path.with_suffix(f"{self.path.suffix}.{os.getpid()}.tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(json.dumps(data, indent=1, ensure_ascii=False))
        os.replace(tmp, self.path)
        try:
            self.path.chmod(0o600)
        except OSError:
            pass

    def signature(self) -> tuple[int, int] | None:
        try:
            stat = self.path.stat()
        except OSError:
            return None
        return (stat.st_mtime_ns, stat.st_size)


def encode_cursor(created_at: str, session_id: str) -> str:
    raw = f"{created_at}|{session_id}".encode()
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def decode_cursor(cursor: str) -> tuple[str, str]:
    padded = cursor + "=" * (-len(cursor) % 4)
    created_at, session_id = base64.urlsafe_b64decode(padded.encode()).decode().split("|", 1)
    return created_at, session_id


class AgentSessionStore:
    """`sessions.json` (index) plus `events/<id>.jsonl` (append-only logs)."""

    def __init__(self, root: Path | None = None, *, clock: Callable[[], str] = utc_now):
        self.root = Path(root) if root is not None else agents_dir()
        self.events_dir = self.root / "events"
        self._file = LockedJsonFile(self.root / "sessions.json")
        self._lock = threading.RLock()
        self._clock = clock
        self._sessions: dict[str, AgentSession] = {}
        self._revision = 0
        self._tombstones: list[tuple[int, str]] = []
        self._floor = 0
        self._log_bytes: dict[str, int] = {}
        self.reload()

    # -- storage --------------------------------------------------------------

    def reload(self) -> None:
        with self._lock, self._file.exclusive():
            data = self._file.read({})
            sessions: dict[str, AgentSession] = {}
            for record in data.get("sessions", []) if isinstance(data, dict) else []:
                if not isinstance(record, dict) or not record.get("id"):
                    continue
                try:
                    session = AgentSession.from_record(record)
                except TypeError:
                    continue
                session.last_seq = max(session.last_seq, self._tail_seq(session.id))
                sessions[session.id] = session
            self._sessions = sessions
            revision = data.get("revision", 0) if isinstance(data, dict) else 0
            self._revision = revision if isinstance(revision, int) else 0
            self._floor = self._revision
            self._tombstones = []
            self._log_bytes = {}

    def _event_path(self, session_id: str) -> Path:
        return self.events_dir / f"{session_id}.jsonl"

    def _tail_seq(self, session_id: str) -> int:
        path = self._event_path(session_id)
        try:
            with open(path, "rb") as handle:
                handle.seek(0, os.SEEK_END)
                size = handle.tell()
                handle.seek(max(0, size - 65536))
                lines = handle.read().split(b"\n")
        except OSError:
            return 0
        for line in reversed(lines):
            try:
                return int(json.loads(line)["seq"])
            except (ValueError, KeyError, TypeError):
                continue
        return 0

    def _persist(self) -> None:
        with self._file.exclusive():
            self._file.write({
                "version": STORE_VERSION,
                "revision": self._revision,
                "sessions": [s.to_record() for s in self._sessions.values()],
            })

    def _bump(self, session: AgentSession) -> None:
        self._revision += 1
        session.revision = self._revision
        session.updated_at = self._clock()

    @staticmethod
    def _copy(session: AgentSession) -> AgentSession:
        return AgentSession.from_record(copy.deepcopy(session.to_record()))

    # -- sessions -------------------------------------------------------------

    @property
    def revision(self) -> int:
        with self._lock:
            return self._revision

    def create(self, session: AgentSession) -> AgentSession:
        with self._lock:
            if session.id in self._sessions:
                raise SessionConflict(session.id, self._sessions[session.id].state, ())
            session = self._copy(session)
            session.created_at = session.created_at or self._clock()
            session.state = "queued"
            self._bump(session)
            self._sessions[session.id] = session
            self._append(session, "state", state="queued")
            self._persist()
            return self._copy(session)

    def get(self, session_id: str) -> AgentSession | None:
        with self._lock:
            session = self._sessions.get(session_id)
            return self._copy(session) if session is not None else None

    def require(self, session_id: str) -> AgentSession:
        session = self.get(session_id)
        if session is None:
            raise NotFound(f"no agent session {session_id}")
        return session

    def list(self) -> list[AgentSession]:
        with self._lock:
            return [self._copy(s) for s in self._sessions.values()]

    def resolve_prefix(self, prefix: str) -> list[str]:
        with self._lock:
            return [sid for sid in self._sessions if sid.startswith(prefix)]

    def counts(self) -> dict[str, int]:
        with self._lock:
            states = [s.state for s in self._sessions.values()]
        return {
            "active": sum(1 for s in states if s not in TERMINAL_STATES and s != "queued"),
            "queued": states.count("queued"),
            "terminal": sum(1 for s in states if s in TERMINAL_STATES),
        }

    def transition(self, session_id: str, to: str, expect: tuple[str, ...] | None = None,
                   *, event_data: dict | None = None, **fields) -> AgentSession:
        """Compare-and-set state change; raises SessionConflict on a lost race
        or an illegal move. Appends a `state` event."""
        with self._lock:
            session = self._sessions.get(session_id)
            if session is None:
                raise NotFound(f"no agent session {session_id}")
            current = session.state
            if expect is not None and current not in expect:
                raise SessionConflict(session_id, current, tuple(expect))
            if to not in TRANSITIONS.get(current, frozenset()):
                allowed = tuple(sorted(s for s, nxt in TRANSITIONS.items() if to in nxt))
                raise SessionConflict(session_id, current, allowed)
            for name, value in fields.items():
                setattr(session, name, value)
            session.state = to
            now = self._clock()
            if to == "starting" and not session.started_at:
                session.started_at = now
            if to in TERMINAL_STATES:
                session.finished_at = now
            self._bump(session)
            self._append(session, "state", state=to, data=event_data or {})
            if to in TERMINAL_STATES:
                self._fsync_events(session_id)
            self._persist()
            return self._copy(session)

    def update(self, session_id: str, **fields) -> AgentSession:
        with self._lock:
            session = self._sessions.get(session_id)
            if session is None:
                raise NotFound(f"no agent session {session_id}")
            for name, value in fields.items():
                if name == "internal":
                    session.internal.update(value)
                else:
                    setattr(session, name, value)
            self._bump(session)
            self._persist()
            return self._copy(session)

    def delete(self, session_id: str, *, require_terminal: bool = True) -> bool:
        with self._lock:
            session = self._sessions.get(session_id)
            if session is None:
                return False
            if require_terminal and session.state not in TERMINAL_STATES:
                raise SessionConflict(session_id, session.state, TERMINAL_STATES)
            del self._sessions[session_id]
            self._revision += 1
            self._tombstones.append((self._revision, session_id))
            if len(self._tombstones) > MAX_TOMBSTONES:
                dropped = self._tombstones[: len(self._tombstones) - MAX_TOMBSTONES]
                self._floor = max(self._floor, dropped[-1][0])
                self._tombstones = self._tombstones[-MAX_TOMBSTONES:]
            self._log_bytes.pop(session_id, None)
            try:
                self._event_path(session_id).unlink()
            except OSError:
                pass
            self._persist()
            return True

    def prune(self, max_sessions: int) -> list[str]:
        """Drop the oldest terminal sessions beyond `max_sessions` (never an active one)."""
        with self._lock:
            excess = len(self._sessions) - max_sessions
            if excess <= 0:
                return []
            victims = [s.id for s in self._sessions.values() if s.state in TERMINAL_STATES]
            victims = victims[:excess]
            for session_id in victims:
                self.delete(session_id)
            return victims

    def changes(self, since: int) -> dict:
        with self._lock:
            reset = since < self._floor or since > self._revision
            sessions = [] if reset else [
                self._copy(s) for s in self._sessions.values() if s.revision > since
            ]
            deleted = [] if reset else [sid for rev, sid in self._tombstones if rev > since]
            return {"revision": self._revision, "sessions": sessions, "deleted": deleted,
                    "reset": reset}

    # -- events ---------------------------------------------------------------

    def _fsync_events(self, session_id: str) -> None:
        try:
            fd = os.open(self._event_path(session_id), os.O_RDONLY)
        except OSError:
            return
        try:
            os.fsync(fd)
        except OSError:
            pass
        finally:
            os.close(fd)

    def _log_size(self, session_id: str) -> int:
        size = self._log_bytes.get(session_id)
        if size is None:
            try:
                size = self._event_path(session_id).stat().st_size
            except OSError:
                size = 0
            self._log_bytes[session_id] = size
        return size

    def _write_line(self, session: AgentSession, event: AgentEvent) -> None:
        _ensure_dir(self.root)
        _ensure_dir(self.events_dir)
        line = json.dumps(event.to_dict(), ensure_ascii=False) + "\n"
        data = line.encode("utf-8")
        fd = os.open(self._event_path(session.id), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        self._log_bytes[session.id] = self._log_size(session.id) + len(data)

    def _append(self, session: AgentSession, type_: str, *, stream: str = "system",
                text: str = "", state: str | None = None, data: dict | None = None,
                limit_bytes: int | None = None) -> AgentEvent | None:
        if limit_bytes is not None:
            size = self._log_size(session.id)
            estimate = len(text.encode("utf-8")) + 200
            if type_ == "output":
                if session.internal.get("log_truncated"):
                    return None
                if size + estimate > limit_bytes:
                    session.internal["log_truncated"] = True
                    session.last_seq += 1
                    marker = AgentEvent(session.last_seq, self._clock(), "truncated",
                                        "system", "output limit reached; later output dropped",
                                        None, {"limit_kb": limit_bytes // 1024})
                    self._write_line(session, marker)
                    return None
            elif type_ != "state" and size + estimate > limit_bytes + EVENT_RESERVE_BYTES:
                return None
        session.last_seq += 1
        event = AgentEvent(session.last_seq, self._clock(), type_, stream, text, state,
                           data or {})
        self._write_line(session, event)
        return event

    def append_event(self, session_id: str, type_: str, *, stream: str = "system",
                     text: str = "", state: str | None = None, data: dict | None = None,
                     limit_bytes: int | None = None) -> AgentEvent | None:
        """Append one event (text longer than 8 KiB is split); returns the last
        event written or None when the bounded log dropped it."""
        with self._lock:
            session = self._sessions.get(session_id)
            if session is None:
                raise NotFound(f"no agent session {session_id}")
            chunks = [text[i:i + MAX_EVENT_TEXT] for i in range(0, len(text), MAX_EVENT_TEXT)]
            last = None
            for chunk in chunks or [""]:
                event = self._append(session, type_, stream=stream, text=chunk, state=state,
                                     data=data, limit_bytes=limit_bytes)
                if event is None and type_ == "output":
                    break
                last = event or last
            truncated = session.internal.get("log_truncated")
            if truncated and not session.internal.get("log_truncated_saved"):
                session.internal["log_truncated_saved"] = True
                self._persist()
            return last

    def events(self, session_id: str, after: int = 0, limit: int = 200) -> list[AgentEvent]:
        with self._lock:
            path = self._event_path(session_id)
            try:
                lines = path.read_text(encoding="utf-8").split("\n")
            except OSError:
                return []
        events: list[AgentEvent] = []
        for line in lines:
            try:
                data = json.loads(line)
            except ValueError:
                continue
            if not isinstance(data, dict) or data.get("seq", 0) <= after:
                continue
            events.append(AgentEvent(**{k: data.get(k) for k in
                                        ("seq", "ts", "type", "stream", "text", "state")},
                                     data=data.get("data") or {}))
            if len(events) >= limit:
                break
        return events


class _ListStore:
    """A cached JSON list (rules or profiles) shared between processes."""

    def __init__(self, path: Path):
        self.file = LockedJsonFile(path)
        self._cache: list[dict] | None = None
        self._sig: tuple[int, int] | None = None

    @property
    def path(self) -> Path:
        return self.file.path

    def list(self) -> list[dict]:
        sig = self.file.signature()
        if sig is None:
            return []
        if sig == self._sig and self._cache is not None:
            return copy.deepcopy(self._cache)
        with self.file.exclusive():
            entries = [e for e in self.file.read([]) if isinstance(e, dict)]
        self._cache, self._sig = entries, sig
        return copy.deepcopy(entries)

    def get(self, entry_id: str) -> dict | None:
        for entry in self.list():
            if entry.get("id") == entry_id:
                return entry
        return None

    @contextmanager
    def mutate(self):
        """Yield the current list for in-place edits; written back on success."""
        with self.file.exclusive():
            entries = [e for e in self.file.read([]) if isinstance(e, dict)]
            yield entries
            self.file.write(entries)
        self._cache, self._sig = None, None


class HotCommandStore(_ListStore):
    def __init__(self, path: Path | None = None):
        super().__init__(Path(path) if path is not None else agents_dir() / "hot_commands.json")


class HttpProfileStore(_ListStore):
    def __init__(self, path: Path | None = None):
        super().__init__(Path(path) if path is not None else agents_dir() / "http_providers.json")
