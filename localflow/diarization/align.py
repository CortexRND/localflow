"""Pure alignment of STT segments to diarization turns (design §5.4).

Texts go to the speaker with the largest summed overlap, else the nearest turn within
``GAP_TOLERANCE_S``; float ties are resolved within ``_EPS``.
"""

from __future__ import annotations

from collections.abc import Hashable, Sequence
from dataclasses import dataclass
from typing import TypeVar

from .types import SpeakerTurn, TimedText

GAP_TOLERANCE_S = 0.5
_EPS = 1e-9

K = TypeVar("K", bound=Hashable)


@dataclass(frozen=True)
class SpeakerRun:
    """Consecutive same-speaker text, in seconds from meeting (or source) start."""

    start: float
    end: float
    text: str
    speaker: Hashable | None


def _better(cand: list, best: list | None, larger: bool) -> bool:
    """Compare ``[score, start, index, speaker]`` candidates: score, then start, then index."""
    if best is None:
        return True
    if abs(cand[0] - best[0]) > _EPS:
        return cand[0] > best[0] if larger else cand[0] < best[0]
    if abs(cand[1] - best[1]) > _EPS:
        return cand[1] < best[1]
    return cand[2] < best[2]


def _assign_one(t: TimedText, turns: Sequence[SpeakerTurn]) -> int | None:
    totals: dict[int, list] = {}
    gap_best = None
    for i, turn in enumerate(turns):
        overlap = min(t.end, turn.end) - max(t.start, turn.start)
        if overlap > 0:
            entry = totals.setdefault(
                turn.local_speaker, [0.0, turn.start, i, turn.local_speaker]
            )
            entry[0] += overlap
            entry[1] = min(entry[1], turn.start)
        elif -overlap <= GAP_TOLERANCE_S + _EPS:
            cand = [max(0.0, -overlap), turn.start, i, turn.local_speaker]
            if _better(cand, gap_best, larger=False):
                gap_best = cand
    best = None
    for cand in totals.values():
        if _better(cand, best, larger=True):
            best = cand
    best = best or gap_best
    return None if best is None else best[3]


def assign_speakers(
    timed: list[TimedText], turns: list[SpeakerTurn]
) -> list[tuple[TimedText, int | None]]:
    """Assign each text a ``local_speaker``; texts are never split, output keeps ``timed`` order.

    1. The speaker with the largest positive overlap summed over all of its turns; ties
       (within ``_EPS``) go to the speaker whose earliest overlapping turn starts first, then to
       the speaker whose first overlapping turn comes first in ``turns``.
    2. With no positive overlap: the nearest turn whose gap is ``<= GAP_TOLERANCE_S``; ties go to
       the earlier-starting turn, then the earlier turn in ``turns``.
    3. Otherwise ``None``.
    """
    return [(t, _assign_one(t, turns)) for t in timed]


def merge_runs(
    assigned: Sequence[tuple[TimedText, K]], *, chunk_offset: float = 0.0
) -> list[SpeakerRun]:
    """Merge consecutive items with an equal speaker key into one ``SpeakerRun``.

    Times are shifted by ``chunk_offset``; text is the stripped non-empty texts joined by a
    space. A run whose texts are all empty is kept with text ``""``. Order is preserved.
    """
    runs: list[SpeakerRun] = []
    group: list[TimedText] = []
    key = None

    def flush() -> None:
        text = " ".join(s for s in (g.text.strip() for g in group) if s)
        runs.append(
            SpeakerRun(
                chunk_offset + group[0].start, chunk_offset + group[-1].end, text, key
            )
        )

    for item, speaker in assigned:
        if group and speaker != key:
            flush()
            group = []
        if not group:
            key = speaker
        group.append(item)
    if group:
        flush()
    return runs
