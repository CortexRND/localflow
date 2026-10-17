"""User-defined hot phrases: a short spoken trigger expands into a saved
long prompt mid-dictation ("review checklist" -> the full review prompt).

Storage is a JSON list at ~/.localflow/hot_phrases.json, shared between the
web server (writer) and the desktop dictation app (reader) — the same
two-process pattern as PromptQueue in dispatch.py: a threading.Lock for
threads within this process plus an flock on a sidecar .lock file across
processes, with atomic writes via a per-pid temp file and os.replace.
"""

from __future__ import annotations

import json
import logging
import os
import re
import secrets
import threading
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path

try:
    import fcntl
except ImportError:  # non-POSIX; the store falls back to in-process locking
    fcntl = None

log = logging.getLogger("localflow.hotphrases")

HOT_PHRASES_PATH = Path("~/.localflow/hot_phrases.json").expanduser()

MAX_TRIGGER_CHARS = 80
MAX_TRIGGER_WORDS = 8
MAX_TEXT_CHARS = 20000


class HotPhraseError(ValueError):
    """Invalid trigger or expansion text."""


class HotPhraseConflict(HotPhraseError):
    """A trigger that duplicates an existing entry after normalization."""


def normalize_trigger(s: str) -> str:
    """Lowercase words separated by single spaces — punctuation and
    underscore are stripped, so 'Review, checklist' == 'review checklist'."""
    return " ".join(re.findall(r"[^\W_]+", s.lower()))


def _now() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _validate_trigger(trigger: str) -> str:
    trigger = trigger.strip()
    if len(trigger) > MAX_TRIGGER_CHARS:
        raise HotPhraseError(
            f"trigger is too long ({len(trigger)} chars, max {MAX_TRIGGER_CHARS})"
        )
    normalized = normalize_trigger(trigger)
    if not normalized:
        raise HotPhraseError("trigger has no usable words")
    words = len(normalized.split())
    if words > MAX_TRIGGER_WORDS:
        raise HotPhraseError(
            f"trigger is too long ({words} words, max {MAX_TRIGGER_WORDS})"
        )
    return trigger


def _validate_text(text: str) -> str:
    if not text or not text.strip():
        raise HotPhraseError("expansion text is empty")
    if len(text) > MAX_TEXT_CHARS:
        raise HotPhraseError(
            f"expansion text is too long ({len(text)} chars, max {MAX_TEXT_CHARS})"
        )
    return text  # verbatim: newlines and indentation matter in prompts


class HotPhraseStore:
    """A JSON list of hot-phrase entries shared across processes.

    See PromptQueue in dispatch.py for the storage pattern this mirrors:
    threading.Lock + flock on a sidecar .lock file around each
    read-modify-write, and per-pid tmp + os.replace so readers never see a
    partial file. An unreadable or missing file reads as an empty list.
    """

    def __init__(self, path: Path = HOT_PHRASES_PATH):
        self.path = Path(path)
        self._lock = threading.Lock()
        self._lock_path = self.path.with_suffix(self.path.suffix + ".lock")
        self._enabled_cache: list[tuple[str, str]] | None = None
        self._enabled_stat: tuple[int, int] | None = None

    # -- storage --

    @contextmanager
    def _exclusive(self):
        """Hold the in-process lock and the cross-process flock together."""
        with self._lock:
            handle = None
            if fcntl is not None:  # non-POSIX: in-process safety only
                try:
                    self.path.parent.mkdir(parents=True, exist_ok=True)
                    handle = open(self._lock_path, "a+")
                    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                except OSError:
                    log.warning("hot phrase lock unavailable, proceeding unlocked: %s",
                                self._lock_path, exc_info=True)
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

    def _read(self) -> list[dict]:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return []
        except Exception:
            log.exception("hot phrases unreadable; treating as empty: %s", self.path)
            return []
        return data if isinstance(data, list) else []

    def _write(self, entries: list[dict]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        # Per-pid temp name: two processes sharing one temp path race to
        # os.replace it and the loser dies on a missing file.
        tmp = self.path.with_suffix(f"{self.path.suffix}.{os.getpid()}.tmp")
        tmp.write_text(json.dumps(entries, indent=2), encoding="utf-8")
        os.replace(tmp, self.path)

    @staticmethod
    def _check_conflict(entries: list[dict], normalized: str, exclude_id: str | None) -> None:
        for entry in entries:
            if entry.get("id") == exclude_id:
                continue
            if normalize_trigger(entry.get("trigger", "")) == normalized:
                raise HotPhraseConflict(
                    f"trigger duplicates existing hot phrase {entry.get('trigger')!r}"
                )

    # -- api --

    def list(self) -> list[dict]:
        with self._exclusive():
            return self._read()

    def add(self, trigger: str, text: str, enabled: bool = True) -> dict:
        trigger = _validate_trigger(trigger)
        text = _validate_text(text)
        normalized = normalize_trigger(trigger)
        with self._exclusive():
            entries = self._read()
            self._check_conflict(entries, normalized, exclude_id=None)
            now = _now()
            entry = {
                "id": secrets.token_hex(4),
                "trigger": trigger,
                "text": text,
                "enabled": bool(enabled),
                "created_at": now,
                "updated_at": now,
            }
            entries.append(entry)
            self._write(entries)
        return entry

    def update(
        self,
        entry_id: str,
        *,
        trigger: str | None = None,
        text: str | None = None,
        enabled: bool | None = None,
    ) -> dict | None:
        with self._exclusive():
            entries = self._read()
            for entry in entries:
                if entry.get("id") != entry_id:
                    continue
                if trigger is not None:
                    trigger = _validate_trigger(trigger)
                    self._check_conflict(
                        entries, normalize_trigger(trigger), exclude_id=entry_id
                    )
                    entry["trigger"] = trigger
                if text is not None:
                    entry["text"] = _validate_text(text)
                if enabled is not None:
                    entry["enabled"] = bool(enabled)
                entry["updated_at"] = _now()
                self._write(entries)
                return entry
        return None

    def delete(self, entry_id: str) -> bool:
        with self._exclusive():
            entries = self._read()
            remaining = [e for e in entries if e.get("id") != entry_id]
            if len(remaining) == len(entries):
                return False
            self._write(remaining)
        return True

    def enabled(self) -> list[tuple[str, str]]:
        """(trigger, expansion) pairs for enabled entries.

        The desktop app calls this once per clip; the file is only re-read
        when its (st_mtime_ns, st_size) changes, so UI edits made by the
        server process are picked up without a restart or a read per clip.
        """
        try:
            stat = self.path.stat()
        except OSError:
            self._enabled_cache = []
            self._enabled_stat = None
            return []
        sig = (stat.st_mtime_ns, stat.st_size)
        if self._enabled_stat == sig and self._enabled_cache is not None:
            return self._enabled_cache
        with self._exclusive():
            entries = self._read()
        enabled = [
            (e["trigger"], e["text"])
            for e in entries
            if e.get("enabled") and e.get("trigger") and e.get("text")
        ]
        self._enabled_cache = enabled
        self._enabled_stat = sig
        return enabled


def match_whole(text: str, phrases: list[tuple[str, str]]) -> str | None:
    """Whole-utterance match: 'Review checklist.' expands verbatim."""
    normalized = normalize_trigger(text)
    if not normalized:
        return None
    for trigger, expansion in phrases:
        if normalize_trigger(trigger) == normalized:
            return expansion
    return None


_PARK = re.compile("\x01(\\d+)\x01")  # distinct from symbols.py's \x00 placeholders


def _trigger_regex(
    phrases: list[tuple[str, str]],
) -> tuple[re.Pattern, list[tuple[str, str]]] | None:
    """One case-insensitive regex over all triggers, longest first (word
    count, then length) so 'pr review full' wins over 'pr review'. Each
    trigger is a named group — the expansion is looked up by group name, so
    a spelling that normalizes differently from the match (e.g. IGNORECASE
    matching dotless 'ı' against trigger 'i') can never key the wrong entry.
    Returns (regex, (trigger, expansion) pairs by group index) or None."""
    ordered = sorted(
        phrases,
        key=lambda p: (
            -len(normalize_trigger(p[0]).split()),
            -len(normalize_trigger(p[0])),
        ),
    )
    alternatives = []
    pairs: list[tuple[str, str]] = []
    seen: set[str] = set()
    for trigger, expansion in ordered:
        normalized = normalize_trigger(trigger)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        pattern = r"[\W_]+".join(re.escape(w) for w in normalized.split())
        alternatives.append(f"(?P<h{len(pairs)}>{pattern})")
        pairs.append((trigger, expansion))
    if not alternatives:
        return None
    regex = re.compile(
        r"(?<![^\W_])(?:"
        + "|".join(alternatives)
        + r")(?![^\W_])(?:[.,!?;:]+(?=\s*$))?",
        re.IGNORECASE,
    )
    return regex, pairs


def park_hot_phrases(text: str, phrases: list[tuple[str, str]]) -> tuple[str, list[str]]:
    """Replace inline trigger matches with \\x01N\\x01 placeholders.

    Parking before spoken-symbols runs keeps triggers intact ("review dash
    checklist" becomes "review-checklist" under symbols and would never
    match); the placeholders are word characters to the symbol pass, so
    they survive it. Returns the parked text and the expansions by N —
    literal strings, never re-scanned, so "slash" in an expansion stays
    "slash".
    """
    built = _trigger_regex(phrases)
    if built is None or not text:
        return text, []
    regex, pairs = built

    parked: list[str] = []

    def replace(match: re.Match) -> str:
        expansion = pairs[int(match.lastgroup[1:])][1]
        parked.append(expansion)
        return f"\x01{len(parked) - 1}\x01"

    return regex.sub(replace, text), parked


def restore_hot_phrases(text: str, parked: list[str]) -> str:
    if not parked:
        return text
    return _PARK.sub(lambda m: parked[int(m.group(1))], text)


def matched_triggers(text: str, phrases: list[tuple[str, str]]) -> list[str]:
    """The triggers that matched in `text`: a whole-utterance match, or the
    distinct triggers hit by whole-word inline matches, in match order."""
    if not phrases or not text:
        return []
    for trigger, _ in phrases:
        if normalize_trigger(trigger) == normalize_trigger(text):
            return [trigger]
    built = _trigger_regex(phrases)
    if built is None:
        return []
    regex, pairs = built
    matched: list[str] = []
    for m in regex.finditer(text):
        trigger = pairs[int(m.lastgroup[1:])][0]
        if trigger not in matched:
            matched.append(trigger)
    return matched


def expand_hot_phrases(text: str, phrases: list[tuple[str, str]]) -> str:
    """Whole-utterance match -> verbatim expansion; otherwise inline."""
    if not phrases or not text:
        return text
    whole = match_whole(text, phrases)
    if whole is not None:
        return whole
    parked_text, parked = park_hot_phrases(text, phrases)
    return restore_hot_phrases(parked_text, parked)


def render_dictation(
    text: str,
    phrases: list[tuple[str, str]],
    *,
    clean=None,
    symbols=None,
) -> str:
    """Shared dictation pipeline: cleanup -> park triggers -> symbols -> restore.

    A whole-utterance trigger expands verbatim and skips cleanup and symbols
    entirely. `clean`/`symbols` are callables or None; the callers own their
    conditions (short-clip skip, config flags) and pass None when a step is
    disabled.
    """
    whole = match_whole(text, phrases)
    if whole is not None:
        return whole
    if clean is not None:
        text = clean(text)
    text, parked = park_hot_phrases(text, phrases)
    if symbols is not None:
        text = symbols(text)
    return restore_hot_phrases(text, parked)
