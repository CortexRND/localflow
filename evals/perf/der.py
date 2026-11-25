"""Diarization error rate, scored in the harness (stdlib only).

Diarization benches only emit speaker turns; the harness scores them here so
every backend (mock, sherpa-onnx, a future one) is judged by identical code,
like :mod:`evals.perf.wer` for transcripts.

DER is frame based (10 ms frames by default): each scored frame adds the
number of reference speakers to the total, and the missed, false-alarm and
confused speaker counts to the errors. Frames within ``collar`` seconds of a
reference turn boundary are not scored (0.25 s by default, the usual NIST
forgiveness for boundary placement). Hypothesis speakers are mapped one to one
onto reference speakers so that the total overlap is maximal (Hungarian
algorithm), so labels need not match by name.
"""

from __future__ import annotations

import math
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

DEFAULT_COLLAR_S = 0.25
DEFAULT_FRAME_S = 0.01

_RATE_KEYS = ("missed_speech", "false_alarm", "speaker_confusion")
_SECONDS_KEYS = ("scored_s", "missed_s", "false_alarm_s", "confusion_s")


@dataclass(frozen=True)
class Turn:
    start: float
    end: float
    speaker: str


def _time(value: object, what: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"turn {what} must be a number, got {value!r}")  # noqa: TRY004 - one error type for bad turns
    value = float(value)
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"turn {what} must be finite and >= 0, got {value!r}")
    return value


def as_turns(items: Iterable) -> list[Turn]:
    """Normalise ``Turn``s, ``(start, end, speaker)`` and ``{"start","end","speaker"}`` items.

    Empty or reversed turns (``end <= start``) are dropped; bad times raise ``ValueError``.
    """
    out: list[Turn] = []
    for item in items:
        if isinstance(item, Turn):
            start, end, speaker = item.start, item.end, item.speaker
        elif isinstance(item, dict):
            try:
                start, end, speaker = item["start"], item["end"], item["speaker"]
            except KeyError as exc:
                raise ValueError(f"turn dict needs start/end/speaker, missing {exc}") from None
        elif isinstance(item, (tuple, list)) and len(item) == 3:
            start, end, speaker = item
        else:
            raise ValueError(f"unsupported turn {item!r}")
        start, end = _time(start, "start"), _time(end, "end")
        if end <= start:
            continue
        out.append(Turn(start, end, str(speaker)))
    out.sort(key=lambda t: (t.start, t.end, t.speaker))
    return out


# --------------------------------------------------------------------------
# RTTM
# --------------------------------------------------------------------------


def parse_rttm(text: str, *, file_id: str | None = None) -> list[Turn]:
    """Turns from RTTM ``SPEAKER`` lines (other record types and comments are skipped)."""
    turns: list[Turn] = []
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith((";", "#")):
            continue
        fields = line.split()
        if fields[0] != "SPEAKER":
            continue
        if len(fields) < 8:
            raise ValueError(f"RTTM line {lineno}: SPEAKER needs at least 8 fields, got {len(fields)}")
        try:
            tbeg, tdur = float(fields[3]), float(fields[4])
        except ValueError:
            raise ValueError(f"RTTM line {lineno}: tbeg/tdur must be numbers") from None
        if not (math.isfinite(tbeg) and math.isfinite(tdur)) or tbeg < 0 or tdur < 0:
            raise ValueError(f"RTTM line {lineno}: tbeg/tdur must be finite and >= 0")
        if file_id is not None and fields[1] != file_id:
            continue
        turns.append(Turn(tbeg, tbeg + tdur, fields[7]))
    return as_turns(turns)


def load_rttm(path: Path | str, *, file_id: str | None = None) -> list[Turn]:
    return parse_rttm(Path(path).read_text(encoding="utf-8"), file_id=file_id)


def to_rttm(turns: Iterable, file_id: str = "mix") -> str:
    """One newline-terminated ``SPEAKER`` line per turn (times to 1 ms)."""
    return "".join(
        f"SPEAKER {file_id} 1 {t.start:.3f} {t.end - t.start:.3f} <NA> <NA> {t.speaker} <NA> <NA>\n"
        for t in as_turns(turns)
    )


# --------------------------------------------------------------------------
# Assignment
# --------------------------------------------------------------------------


def hungarian(cost: list[list[float]]) -> list[tuple[int, int]]:
    """Minimum-cost assignment of a rectangular matrix (Kuhn-Munkres with potentials, O(n³)).

    Returns ``min(rows, cols)`` ``(row, col)`` pairs sorted by row.
    """
    rows = len(cost)
    if rows == 0:
        return []
    cols = len(cost[0])
    if any(len(r) != cols for r in cost):
        raise ValueError("cost matrix rows must all have the same length")
    if cols == 0:
        return []
    transposed = rows > cols
    a = [list(col) for col in zip(*cost)] if transposed else [list(r) for r in cost]
    n, m = len(a), len(a[0])  # n <= m
    inf = math.inf
    u = [0.0] * (n + 1)
    v = [0.0] * (m + 1)
    p = [0] * (m + 1)  # p[j] = row (1-based) assigned to column j
    way = [0] * (m + 1)
    for i in range(1, n + 1):
        p[0] = i
        j0 = 0
        minv = [inf] * (m + 1)
        used = [False] * (m + 1)
        while True:
            used[j0] = True
            i0 = p[j0]
            delta = inf
            j1 = 0
            for j in range(1, m + 1):
                if used[j]:
                    continue
                cur = a[i0 - 1][j - 1] - u[i0] - v[j]
                if cur < minv[j]:
                    minv[j] = cur
                    way[j] = j0
                if minv[j] < delta:
                    delta = minv[j]
                    j1 = j
            for j in range(m + 1):
                if used[j]:
                    u[p[j]] += delta
                    v[j] -= delta
                else:
                    minv[j] -= delta
            j0 = j1
            if p[j0] == 0:
                break
        while True:
            j1 = way[j0]
            p[j0] = p[j1]
            j0 = j1
            if j0 == 0:
                break
    pairs = [(p[j] - 1, j - 1) for j in range(1, m + 1) if p[j] != 0]
    if transposed:
        pairs = [(c, r) for r, c in pairs]
    return sorted(pairs)


# --------------------------------------------------------------------------
# DER
# --------------------------------------------------------------------------


def _frames(turns: list[Turn], frame_s: float) -> dict[str, list[tuple[int, int]]]:
    out: dict[str, list[tuple[int, int]]] = {}
    for t in turns:
        out.setdefault(t.speaker, []).append((round(t.start / frame_s), round(t.end / frame_s)))
    return out


def _rates(missed: float, fa: float, conf: float, total: float) -> dict:
    if total > 0:
        return {
            "der": (missed + fa + conf) / total,
            "missed_speech": missed / total,
            "false_alarm": fa / total,
            "speaker_confusion": conf / total,
        }
    flag = 1.0 if fa > 0 else 0.0
    return {"der": flag, "missed_speech": 0.0, "false_alarm": flag, "speaker_confusion": 0.0}


def compute_der(
    reference: Iterable,
    hypothesis: Iterable,
    *,
    collar: float = DEFAULT_COLLAR_S,
    frame_s: float = DEFAULT_FRAME_S,
) -> dict:
    """Frame-based DER of ``hypothesis`` against ``reference`` (overlap supported)."""
    if isinstance(collar, bool) or not isinstance(collar, (int, float)) or not collar >= 0:
        raise ValueError(f"collar must be >= 0, got {collar!r}")
    if isinstance(frame_s, bool) or not isinstance(frame_s, (int, float)) or not frame_s > 0:
        raise ValueError(f"frame_s must be > 0, got {frame_s!r}")
    ref = as_turns(reference)
    hyp = as_turns(hypothesis)
    max_end = max([t.end for t in ref] + [t.end for t in hyp] + [0.0])
    n = math.ceil(max_end / frame_s)

    ref_sets: list[set[str]] = [set() for _ in range(n)]
    hyp_sets: list[set[str]] = [set() for _ in range(n)]
    for sets, turns in ((ref_sets, ref), (hyp_sets, hyp)):
        for speaker, spans in _frames(turns, frame_s).items():
            for a, b in spans:
                for f in range(max(0, a), min(n, b)):
                    sets[f].add(speaker)

    scored = [True] * n
    if collar > 0:
        for t in ref:
            for b in (t.start, t.end):
                for f in range(max(0, round((b - collar) / frame_s)), min(n, round((b + collar) / frame_s))):
                    scored[f] = False

    ref_names = sorted({t.speaker for t in ref})
    hyp_names = sorted({t.speaker for t in hyp})
    r_idx = {s: i for i, s in enumerate(ref_names)}
    h_idx = {s: i for i, s in enumerate(hyp_names)}
    overlap = [[0] * len(ref_names) for _ in hyp_names]
    for f in range(n):
        if not scored[f]:
            continue
        for h in hyp_sets[f]:
            row = overlap[h_idx[h]]
            for r in ref_sets[f]:
                row[r_idx[r]] += 1
    mapping: dict[str, str] = {}
    if hyp_names and ref_names:
        for hi, ri in hungarian([[-float(x) for x in row] for row in overlap]):
            mapping[hyp_names[hi]] = ref_names[ri]

    total = missed = fa = conf = 0
    for f in range(n):
        if not scored[f]:
            continue
        r_set, h_set = ref_sets[f], hyp_sets[f]
        nr, nh = len(r_set), len(h_set)
        total += nr
        missed += max(0, nr - nh)
        fa += max(0, nh - nr)
        correct = sum(1 for h in h_set if mapping.get(h) in r_set)
        conf += min(nr, nh) - correct

    out = _rates(missed, fa, conf, total)
    out.update({
        "scored_s": total * frame_s,
        "missed_s": missed * frame_s,
        "false_alarm_s": fa * frame_s,
        "confusion_s": conf * frame_s,
        "ref_speakers": len(ref_names),
        "hyp_speakers": len(hyp_names),
        "speaker_count_error": abs(len(hyp_names) - len(ref_names)),
        "speaker_count_exact": len(hyp_names) == len(ref_names),
        "mapping": mapping,
        "collar": float(collar),
        "frame_s": float(frame_s),
    })
    return out


def aggregate(results: list[dict]) -> dict:
    """Pool ``compute_der`` results: rates from summed seconds, mean count error."""
    if not results:
        raise ValueError("aggregate needs at least one result")
    sums = {k: sum(float(r[k]) for r in results) for k in _SECONDS_KEYS}
    out = _rates(sums["missed_s"], sums["false_alarm_s"], sums["confusion_s"], sums["scored_s"])
    out.update(sums)
    out["speaker_count_error"] = sum(float(r["speaker_count_error"]) for r in results) / len(results)
    out["speaker_count_exact"] = sum(1.0 for r in results if r["speaker_count_exact"]) / len(results)
    out["n"] = len(results)
    return out


def compute_der_rttm(reference_rttm: str, hypothesis_rttm: str, **kw) -> dict:
    return compute_der(parse_rttm(reference_rttm), parse_rttm(hypothesis_rttm), **kw)
