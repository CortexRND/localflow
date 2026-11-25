"""Speaker diarization core: contracts, alignment, clustering and mock backends.

``.types`` and ``.cluster`` (numpy only) are imported eagerly; align and mock load on first
access (PEP 562) so STT code importing ``TimedText`` stays cheap. Binding the ``cluster``
function here, after its submodule has loaded, makes the function win in every import order.
"""

from __future__ import annotations

import importlib
from typing import TYPE_CHECKING

from .cluster import cluster
from .types import (
    SOURCE_ME,
    SOURCE_OTHERS,
    DiarizationUnavailable,
    Diarizer,
    MeetingSpeaker,
    SpeakerEmbedder,
    SpeakerRegistry,
    SpeakerTurn,
    TimedText,
    TurnEmbedding,
    Voiceprint,
)

if TYPE_CHECKING:
    from .align import GAP_TOLERANCE_S, SpeakerRun, assign_speakers, merge_runs
    from .mock import MockDiarizer, MockEmbedder, tone_index

_LAZY = {
    "GAP_TOLERANCE_S": "align",
    "SpeakerRun": "align",
    "assign_speakers": "align",
    "merge_runs": "align",
    "MockDiarizer": "mock",
    "MockEmbedder": "mock",
    "tone_index": "mock",
}

__all__ = [
    "GAP_TOLERANCE_S",
    "SOURCE_ME",
    "SOURCE_OTHERS",
    "DiarizationUnavailable",
    "Diarizer",
    "MeetingSpeaker",
    "MockDiarizer",
    "MockEmbedder",
    "SpeakerEmbedder",
    "SpeakerRegistry",
    "SpeakerRun",
    "SpeakerTurn",
    "TimedText",
    "TurnEmbedding",
    "Voiceprint",
    "assign_speakers",
    "cluster",
    "merge_runs",
    "tone_index",
]


def __getattr__(name: str):
    if name not in _LAZY:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    value = getattr(importlib.import_module(f"{__name__}.{_LAZY[name]}"), name)
    globals()[name] = value
    return value


def __dir__() -> list[str]:
    return sorted(set(globals()) | set(_LAZY))
