"""Local perf ledger: one JSON line of timings per dictation, meeting or recall.

`~/.localflow/perf/events.jsonl`, append-only, rotated to `events.jsonl.1` at
5 MB (one previous file kept). The Rust port (`lf_core::perf_ledger`) writes
and reads the same file in the same format, so either runtime reads what the
other wrote.

Canonical line: every key of `FIELDS`, in that order, compact separators,
`null` for an absent value (never omitted); `stt`/`llm` are `null` or an
object with all their keys. Readers also accept missing keys as `null`, ignore
unknown keys and skip lines that do not parse.

Privacy: numbers and labels only. There is no field for transcript text, user
text, app names, window titles, URLs or prompts, unknown fields are rejected,
and every label must look like an identifier (`_LABEL`), so a sentence or a
URL can never be written.

Writing is best effort: `record` never raises; a failure is logged at debug.
`perf_metrics = false` (`[features]`) makes it a no-op.
"""

from __future__ import annotations

import json
import logging
import math
import os
import re
from collections.abc import Iterator
from dataclasses import dataclass, field, fields
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

log = logging.getLogger("localflow.perf_ledger")

MAX_BYTES = 5 * 1024 * 1024
ENV_PATH = "LOCALFLOW_PERF_LEDGER"
RUNTIME = "python"

KINDS = (
    "dictate",
    "command",
    "transform",
    "meeting_summarize",
    "meeting_chat",
    "recall",
    "stt_load",
    "llm_warmup",
    "startup",
)
STATUSES = ("ok", "error")
RUNTIMES = ("python", "rust")
NUMBERS = (
    "audio_s",
    "queue_ms",
    "stt_ms",
    "llm_ms",
    "render_ms",
    "paste_ms",
    "total_ms",
)
FIELDS = (
    "ts", "runtime", "version", "kind", "status", *NUMBERS, "stt", "llm", "chars",
)
STT_KEYS = ("backend", "model", "provider")
LLM_KEYS = ("provider", "model")

# Identifier-shaped labels only: no spaces, so no sentence fits.
_LABEL = re.compile(r"[A-Za-z0-9._:/@+\-]{0,128}")
OTHER_LABEL = "other"
_TS = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,9})?Z")


def label(value: object) -> str:
    """`value` when it is identifier-shaped, else "other" (URLs included)."""
    if not isinstance(value, str):
        return OTHER_LABEL
    if _LABEL.fullmatch(value) is None or "://" in value:
        return OTHER_LABEL
    return value


def _number(value: object) -> float | None:
    if value is None or isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        scaled = float(value) * 1000
    except OverflowError:
        return None
    if not math.isfinite(scaled) or scaled < 0:
        return None
    # 3 decimals, ties away from zero: Rust's `(x * 1000.0).round() / 1000.0`
    # (Python's round() differs on ties), so both write identical lines.
    whole = math.floor(scaled)
    if scaled - whole >= 0.5:
        whole += 1
    return whole / 1000


def _count(value: object) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return None
    return value


def _now_ts() -> str:
    now = datetime.now(UTC)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond // 1000:03d}Z"


def _version() -> str:
    try:
        from importlib.metadata import PackageNotFoundError, version
    except ImportError:  # pragma: no cover
        return OTHER_LABEL
    try:
        return label(version("localflow"))
    except PackageNotFoundError:
        return OTHER_LABEL


_VERSION: str | None = None


def runtime_version() -> str:
    global _VERSION
    if _VERSION is None:
        _VERSION = _version()
    return _VERSION


@dataclass(frozen=True)
class SttLabels:
    backend: str = ""
    model: str = ""
    provider: str = ""

    def __post_init__(self) -> None:
        for key in STT_KEYS:
            object.__setattr__(self, key, label(getattr(self, key)))

    def to_dict(self) -> dict[str, str]:
        return {key: getattr(self, key) for key in STT_KEYS}


@dataclass(frozen=True)
class LlmLabels:
    provider: str = ""
    model: str = ""

    def __post_init__(self) -> None:
        for key in LLM_KEYS:
            object.__setattr__(self, key, label(getattr(self, key)))

    def to_dict(self) -> dict[str, str]:
        return {key: getattr(self, key) for key in LLM_KEYS}


@dataclass(frozen=True)
class PerfEvent:
    """One ledger line. Construction normalises every value: kind/status/
    runtime must be known, labels identifier-shaped, numbers finite and >= 0
    (else null). There is deliberately no field that can hold text."""

    kind: str
    status: str = "ok"
    audio_s: float | None = None
    queue_ms: float | None = None
    stt_ms: float | None = None
    llm_ms: float | None = None
    render_ms: float | None = None
    paste_ms: float | None = None
    total_ms: float | None = None
    stt: SttLabels | None = None
    llm: LlmLabels | None = None
    chars: int | None = None
    ts: str = field(default_factory=_now_ts)
    runtime: str = RUNTIME
    version: str = field(default_factory=runtime_version)

    def __post_init__(self) -> None:
        if self.kind not in KINDS:
            raise ValueError(f"unknown perf event kind: {self.kind!r}")
        if self.status not in STATUSES:
            raise ValueError(f"unknown perf event status: {self.status!r}")
        if self.runtime not in RUNTIMES:
            raise ValueError(f"unknown perf event runtime: {self.runtime!r}")
        if not isinstance(self.ts, str) or _TS.fullmatch(self.ts) is None:
            raise ValueError("perf event ts must be an ISO-8601 UTC timestamp")
        object.__setattr__(self, "version", label(self.version))
        for key in NUMBERS:
            object.__setattr__(self, key, _number(getattr(self, key)))
        object.__setattr__(self, "chars", _count(self.chars))
        if self.stt is not None and not isinstance(self.stt, SttLabels):
            raise TypeError("stt must be SttLabels")
        if self.llm is not None and not isinstance(self.llm, LlmLabels):
            raise TypeError("llm must be LlmLabels")

    def to_dict(self) -> dict[str, Any]:
        """The canonical mapping, keys in `FIELDS` order."""
        values: dict[str, Any] = {
            "ts": self.ts,
            "runtime": self.runtime,
            "version": self.version,
            "kind": self.kind,
            "status": self.status,
        }
        for key in NUMBERS:
            values[key] = getattr(self, key)
        values["stt"] = self.stt.to_dict() if self.stt is not None else None
        values["llm"] = self.llm.to_dict() if self.llm is not None else None
        values["chars"] = self.chars
        return values

    def to_line(self) -> str:
        """The canonical JSON line (no trailing newline)."""
        return json.dumps(self.to_dict(), separators=(",", ":"), ensure_ascii=True)

    @classmethod
    def from_dict(cls, data: object) -> PerfEvent:
        """Parse a ledger object written by either runtime; unknown keys are
        dropped, missing optional ones are null. Raises ValueError/TypeError."""
        if not isinstance(data, dict):
            raise TypeError("perf event must be an object")
        for key in ("ts", "runtime", "version", "kind", "status"):
            if not isinstance(data.get(key), str):
                raise TypeError(f"perf event {key} must be a string")
        stt = data.get("stt")
        llm = data.get("llm")
        return cls(
            ts=data["ts"],
            runtime=data["runtime"],
            version=data["version"],
            kind=data["kind"],
            status=data["status"],
            **{key: data.get(key) for key in NUMBERS},
            stt=SttLabels(**{k: stt.get(k, "") for k in STT_KEYS})
            if isinstance(stt, dict) else None,
            llm=LlmLabels(**{k: llm.get(k, "") for k in LLM_KEYS})
            if isinstance(llm, dict) else None,
            chars=data.get("chars"),
        )


_EVENT_ARGS = frozenset(f.name for f in fields(PerfEvent)) - {"ts", "runtime", "version"}


def default_ledger_path() -> Path:
    """`LOCALFLOW_PERF_LEDGER` when set, else `~/.localflow/perf/events.jsonl`."""
    override = os.environ.get(ENV_PATH)
    if override:
        return Path(override)
    return Path.home() / ".localflow" / "perf" / "events.jsonl"


def enabled(config: object) -> bool:
    """The `[features] perf_metrics` flag (on unless explicitly false)."""
    return getattr(config, "perf_metrics", True) is not False


class PerfLedger:
    def __init__(
        self,
        path: Path | str | None = None,
        *,
        max_bytes: int = MAX_BYTES,
        enabled: bool = True,
    ) -> None:
        self.path = Path(path) if path is not None else default_ledger_path()
        self.max_bytes = max_bytes
        self.enabled = enabled

    @classmethod
    def for_config(cls, config: object, path: Path | str | None = None) -> PerfLedger:
        return cls(path, enabled=enabled(config))

    @property
    def previous_path(self) -> Path:
        return self.path.with_name(self.path.name + ".1")

    def record(self, event: PerfEvent | str, **values: Any) -> bool:
        """Append one event (a PerfEvent, or a kind plus PerfEvent fields).
        Never raises; False when disabled or the write failed."""
        if not self.enabled:
            return False
        try:
            if isinstance(event, PerfEvent):
                if values:
                    raise TypeError("pass a PerfEvent or field values, not both")
            else:
                unknown = set(values) - _EVENT_ARGS
                if unknown:
                    raise TypeError(f"unknown perf event fields: {sorted(unknown)}")
                event = PerfEvent(kind=event, **values)
            self._append(event.to_line() + "\n")
            return True
        except Exception:
            # Best effort by design: never break dictation over metrics.
            log.debug("perf ledger: could not record an event", exc_info=True)
            return False

    def _append(self, line: str) -> None:
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        self._rotate_if_full()
        data = line.encode("utf-8")
        # One O_APPEND write per line, so concurrent writers never interleave.
        flags = os.O_RDWR | os.O_APPEND | os.O_CREAT | getattr(os, "O_BINARY", 0)
        fd = os.open(self.path, flags, 0o600)
        try:
            if os.name != "nt":
                try:
                    os.fchmod(fd, 0o600)
                except OSError:
                    pass
            if os.fstat(fd).st_size > 0:
                os.lseek(fd, -1, os.SEEK_END)
                if os.read(fd, 1) != b"\n":
                    data = b"\n" + data
            os.write(fd, data)
        finally:
            os.close(fd)

    def _rotate_if_full(self) -> None:
        try:
            size = self.path.stat().st_size
        except FileNotFoundError:
            return
        if size >= self.max_bytes:
            # Another writer may rotate at the same moment; losing one
            # rotation's worth of old events is acceptable for local metrics.
            try:
                os.replace(self.path, self.previous_path)
            except FileNotFoundError:
                pass

    def iter_events(self) -> Iterator[PerfEvent]:
        """Older file first, then the current one; bad lines are skipped."""
        for path in (self.previous_path, self.path):
            try:
                with path.open("rb") as handle:
                    raw_lines = handle.read().splitlines()
            except OSError:
                continue
            for raw in raw_lines:
                event = parse_line(raw)
                if event is not None:
                    yield event

    def read_events(self) -> list[PerfEvent]:
        return list(self.iter_events())

    def clear(self) -> None:
        for path in (self.path, self.previous_path):
            try:
                path.unlink()
            except FileNotFoundError:
                pass


def parse_line(raw: bytes | str) -> PerfEvent | None:
    """One ledger line, or None when it is blank, truncated or invalid."""
    try:
        text = raw.decode("utf-8") if isinstance(raw, bytes) else raw
        if not text.strip():
            return None
        return PerfEvent.from_dict(json.loads(text))
    except (ValueError, TypeError, OverflowError, UnicodeDecodeError, RecursionError):
        return None


def record_for(config: object, kind: str, **values: Any) -> bool:
    """Hook entry point: record to the default ledger when `config` allows.
    Never raises."""
    try:
        if not enabled(config):
            return False
        return PerfLedger().record(kind, **values)
    except Exception:  # pragma: no cover - PerfLedger.record never raises
        log.debug("perf ledger: could not record an event", exc_info=True)
        return False


def stt_labels(config: object) -> SttLabels:
    """The configured speech model (as Rust labels job events)."""
    return SttLabels(
        backend=str(getattr(config, "stt_backend", "") or ""),
        model=str(getattr(config, "model_size", "") or ""),
        provider=str(getattr(config, "stt_device", "") or ""),
    )


def llm_labels(config: object, *, model: str | None = None) -> LlmLabels:
    return LlmLabels(
        provider=str(getattr(config, "llm_provider", "") or ""),
        model=str(model if model is not None else getattr(config, "llm_model", "") or ""),
    )
