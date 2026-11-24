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
from collections.abc import Iterable, Iterator
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


# --- read side: the `/api/perf` / `lf perf` summary ---------------------------
#
# `lf_core::perf_ledger::summarize_events` is a line-for-line port: the same
# window filter, percentile formula, rounding and key order, so both runtimes
# serialise byte-identical JSON (`summary_json`) and tables
# (`render_summary`) for the same events and `now`.
#
# Canonical rules (shared with Rust):
# - window: `ts >= now - window_seconds` (later timestamps, e.g. clock skew,
#   are kept); events whose `ts` is not a real calendar time are skipped.
# - `by_kind`: only kinds with events, in `KINDS` order.
# - timings, RTF and model loads use `ok` events only (a failure's partial
#   timings would skew them); `failure_rate` covers every event.
# - `stages`: only stages with at least one sample, in `STAGES` order.
# - percentiles: linear interpolation at `p * (n - 1)` on the sorted samples,
#   then 3 decimals (RTF and rates 4), ties away from zero.
# Privacy: only counts, numbers and the identifier-shaped labels above.

WINDOWS = {"24h": 86_400, "7d": 604_800, "30d": 2_592_000}
DEFAULT_WINDOW = "7d"
WINDOW_ERROR = "window must be one of 24h, 7d, 30d"
STAGES = ("queue_ms", "stt_ms", "llm_ms", "render_ms", "paste_ms", "total_ms")
# Dictation stages that can be "the slow step" (total_ms is their sum).
STEP_STAGES = STAGES[:-1]
LOAD_KINDS = ("stt_load", "llm_warmup")
# p95 dictation time above which the summary names the slowest stage.
BUDGET_MS = 1500.0
HINTS = {
    "queue_ms": "waiting for the previous dictation is the slow step",
    "stt_ms": "speech-to-text is the slow step",
    "llm_ms": "cleanup LLM is the slow step",
    "render_ms": "text formatting is the slow step",
    "paste_ms": "pasting is the slow step",
}


def parse_window(window: object) -> int:
    """Seconds in `window` ("24h", "7d" or "30d"); ValueError otherwise."""
    if not isinstance(window, str) or window not in WINDOWS:
        raise ValueError(WINDOW_ERROR)
    return WINDOWS[window]


def _round(value: float, digits: int) -> float:
    """`digits` decimals, ties away from zero (non-negative input), as Rust's
    `(x * 10^d).round() / 10^d`."""
    factor = 10**digits
    scaled = float(value) * factor
    whole = math.floor(scaled)
    if scaled - whole >= 0.5:
        whole += 1
    return whole / factor


def _percentile(ordered: list[float], p: float) -> float:
    """Linear interpolation at `p * (n - 1)` of non-empty sorted samples."""
    index = p * (len(ordered) - 1)
    low = math.floor(index)
    high = math.ceil(index)
    return ordered[low] + (ordered[high] - ordered[low]) * (index - low)


def _timing(samples: list[float]) -> dict[str, Any]:
    ordered = sorted(samples)
    return {
        "p50": _round(_percentile(ordered, 0.5), 3),
        "p95": _round(_percentile(ordered, 0.95), 3),
        "count": len(ordered),
    }


def _median(samples: list[float], digits: int) -> float | None:
    if not samples:
        return None
    return _round(_percentile(sorted(samples), 0.5), digits)


def _instant(ts: str) -> tuple[int, int] | None:
    """(epoch seconds, nanoseconds) of a ledger `ts`, None when it is not a
    real UTC calendar time (the format is already checked)."""
    try:
        year, month, day = int(ts[0:4]), int(ts[5:7]), int(ts[8:10])
        hour, minute, second = int(ts[11:13]), int(ts[14:16]), int(ts[17:19])
        if second > 59:  # no leap seconds (chrono would accept 60)
            return None
        moment = datetime(year, month, day, hour, minute, second, tzinfo=UTC)
    except ValueError:
        return None
    fraction = ts[20:-1]
    nanos = int(fraction.ljust(9, "0")) if fraction else 0
    return int(moment.timestamp()), nanos


def _rtf(event: PerfEvent) -> float | None:
    if event.audio_s is None or event.audio_s <= 0 or event.stt_ms is None:
        return None
    return event.stt_ms / (event.audio_s * 1000.0)


def _load_labels(event: PerfEvent) -> tuple[str, str]:
    if event.kind == "stt_load":
        return (event.stt.backend, event.stt.model) if event.stt else ("", "")
    return (event.llm.provider, event.llm.model) if event.llm else ("", "")


def summarize_events(
    events: Iterable[PerfEvent],
    *,
    window: str = DEFAULT_WINDOW,
    now: datetime | None = None,
) -> dict[str, Any]:
    """The `/api/perf` summary of `events` inside `window` before `now`
    (default: the current time). ValueError for an unknown window."""
    window_seconds = parse_window(window)
    moment = now if now is not None else datetime.now(UTC)
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=UTC)
    whole = moment.replace(microsecond=0)
    cutoff = (int(whole.timestamp()) - window_seconds, moment.microsecond * 1000)
    timed = []
    for event in events:
        instant = _instant(event.ts)
        if instant is not None and instant >= cutoff:
            timed.append((instant, event))
    # Stable: equal timestamps keep file order (the last load is the last line).
    timed.sort(key=lambda pair: pair[0])
    selected = [event for _, event in timed]

    by_kind: dict[str, Any] = {}
    for kind in KINDS:
        of_kind = [event for event in selected if event.kind == kind]
        if not of_kind:
            continue
        ok = [event for event in of_kind if event.status == "ok"]
        errors = len(of_kind) - len(ok)
        stages = {}
        for stage in STAGES:
            samples = [getattr(e, stage) for e in ok if getattr(e, stage) is not None]
            if samples:
                stages[stage] = _timing(samples)
        rtfs = [rtf for rtf in map(_rtf, ok) if rtf is not None]
        by_kind[kind] = {
            "count": len(of_kind),
            "ok_count": len(ok),
            "error_count": errors,
            "failure_rate": _round(errors / len(of_kind), 4),
            "stages": stages,
            "median_rtf": _median(rtfs, 4),
        }

    loads: dict[tuple[int, str, str], list[float]] = {}
    for event in selected:
        if event.kind in LOAD_KINDS and event.status == "ok" and event.total_ms is not None:
            key = (LOAD_KINDS.index(event.kind), *_load_labels(event))
            loads.setdefault(key, []).append(event.total_ms)
    model_loads = []
    for key in sorted(loads):
        samples = loads[key]
        timing = _timing(samples)
        model_loads.append({
            "kind": LOAD_KINDS[key[0]],
            "engine": key[1],
            "model": key[2],
            "count": timing["count"],
            "p50_ms": timing["p50"],
            "p95_ms": timing["p95"],
            "last_ms": samples[-1],
        })

    return {
        "window": window,
        "window_seconds": window_seconds,
        "total_events": len(selected),
        "by_kind": by_kind,
        "model_loads": model_loads,
        "dictation": _dictation_summary(by_kind.get("dictate")),
    }


def _dictation_summary(dictate: dict[str, Any] | None) -> dict[str, Any]:
    """The Hub "Speed" card numbers (`dictate` events only)."""
    stages = dictate["stages"] if dictate else {}
    total = stages.get("total_ms")
    slowest = None
    for stage in STEP_STAGES:
        if stage in stages and (slowest is None or stages[stage]["p50"] > stages[slowest]["p50"]):
            slowest = stage
    p95 = total["p95"] if total else None
    over_budget = p95 is not None and p95 > BUDGET_MS
    return {
        "count": dictate["count"] if dictate else 0,
        "median_total_ms": total["p50"] if total else None,
        "p95_total_ms": p95,
        "median_rtf": dictate["median_rtf"] if dictate else None,
        "slowest_stage": slowest,
        "budget_ms": BUDGET_MS,
        "hint": HINTS[slowest] if over_budget and slowest is not None else None,
    }


def summarize(
    ledger: PerfLedger | None = None,
    *,
    window: str = DEFAULT_WINDOW,
    now: datetime | None = None,
) -> dict[str, Any]:
    """`summarize_events` over the ledger (default: `default_ledger_path()`)."""
    parse_window(window)
    ledger = ledger if ledger is not None else PerfLedger()
    return summarize_events(ledger.iter_events(), window=window, now=now)


def summary_json(summary: dict[str, Any]) -> str:
    """Compact canonical JSON (the `/api/perf` body), byte-identical to Rust."""
    return json.dumps(summary, separators=(",", ":"), ensure_ascii=True, allow_nan=False)


def _cell(value: float | None) -> str:
    return "-" if value is None else repr(value)


def _table(rows: list[list[str]]) -> list[str]:
    """Left-aligned columns two spaces apart, trailing spaces trimmed."""
    widths = [max(len(row[col]) for row in rows) for col in range(len(rows[0]))]
    lines = []
    for row in rows:
        cells = [cell.ljust(widths[col]) for col, cell in enumerate(row[:-1])]
        lines.append("  ".join([*cells, row[-1]]).rstrip())
    return lines


def render_summary(summary: dict[str, Any]) -> list[str]:
    """The `lf perf` table, line by line (no trailing newlines)."""
    window = summary["window"]
    total = summary["total_events"]
    if total == 0:
        return [f"No perf events in the last {window}."]
    lines = [f"Perf events in the last {window}: {total}", ""]
    rows = [["Kind", "Count", "Errors", "Failure rate", "Median RTF"]]
    for kind, entry in summary["by_kind"].items():
        percent = _round(entry["error_count"] * 100 / entry["count"], 1)
        rows.append([
            kind, str(entry["count"]), str(entry["error_count"]),
            f"{percent!r}%", _cell(entry["median_rtf"]),
        ])
    lines += _table(rows)
    rows = [["Kind", "Stage", "p50 ms", "p95 ms", "Samples"]]
    for kind, entry in summary["by_kind"].items():
        for stage, timing in entry["stages"].items():
            rows.append([
                kind, stage, _cell(timing["p50"]), _cell(timing["p95"]), str(timing["count"]),
            ])
    if len(rows) > 1:
        lines += ["", *_table(rows)]
    loads = summary["model_loads"]
    if loads:
        rows = [["Load", "Engine", "Model", "Count", "p50 ms", "p95 ms", "Last ms"]]
        for load in loads:
            rows.append([
                load["kind"], load["engine"] or "-", load["model"] or "-", str(load["count"]),
                _cell(load["p50_ms"]), _cell(load["p95_ms"]), _cell(load["last_ms"]),
            ])
        lines += ["", *_table(rows)]
    dictation = summary["dictation"]
    if dictation["median_total_ms"] is not None:
        line = (
            f"Dictation: median {_cell(dictation['median_total_ms'])} ms,"
            f" p95 {_cell(dictation['p95_total_ms'])} ms"
        )
        if dictation["median_rtf"] is not None:
            line += f", RTF {_cell(dictation['median_rtf'])}"
        if dictation["slowest_stage"] is not None:
            line += f", slowest stage {dictation['slowest_stage']}"
        lines += ["", line]
        if dictation["hint"] is not None:
            lines.append(
                f"Hint: p95 is over {_cell(dictation['budget_ms'])} ms; {dictation['hint']}."
            )
    return lines
