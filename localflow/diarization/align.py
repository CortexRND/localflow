"""Pure alignment of STT segments to diarization turns (design §5.4)."""

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


def _better(score: float, turn: SpeakerTurn, index: int, best: tuple | None, larger: bool) -> bool:
    if best is None:
        return True
    b_score, b_turn, b_index = best
    if abs(score - b_score) > _EPS:
        return score > b_score if larger else score < b_score
    if abs(turn.start - b_turn.start) > _EPS:
        return turn.start < b_turn.start
    return index < b_index


def _assign_one(t: TimedText, turns: Sequence[SpeakerTurn]) -> int | None:
    overlap_best = None
    gap_best = None
    for i, turn in enumerate(turns):
        overlap = min(t.end, turn.end) - max(t.start, turn.start)
        if overlap > 0:
            if _better(overlap, turn, i, overlap_best, larger=True):
                overlap_best = (overlap, turn, i)
        else:
            gap = max(0.0, -overlap)
            if gap <= GAP_TOLERANCE_S + _EPS and _better(gap, turn, i, gap_best, larger=False):
                gap_best = (gap, turn, i)
    best = overlap_best or gap_best
    return None if best is None else best[1].local_speaker


def assign_speakers(
    timed: list[TimedText], turns: list[SpeakerTurn]
) -> list[tuple[TimedText, int | None]]:
    """Give each text the speaker of its largest-overlap turn, else the nearest turn within
    ``GAP_TOLERANCE_S`` (inclusive), else ``None``. Ties go to the earlier-starting turn, then
    the earlier turn in ``turns``. Texts are never split; output keeps the order of ``timed``."""
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
            SpeakerRun(chunk_offset + group[0].start, chunk_offset + group[-1].end, text, key)
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
