"""Speaker diarization: contracts, alignment, clustering, mock backends and the meeting collector.

``.types`` and ``.cluster`` (numpy only) are imported eagerly; align, mock, the collector and
its factory load on first access (PEP 562) so STT code importing ``TimedText`` stays cheap and
a meeting with diarization off never loads them. Binding the ``cluster``
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
    from .collector import (
        DIAR_FINALIZE_TIMEOUT_S,
        DiarizationCollector,
        DiarizationResult,
    )
    from .factory import build_diarization
    from .mock import MockDiarizer, MockEmbedder, tone_index
    from .sherpa import SherpaDiarizer, SherpaEmbedder, load_sherpa_backends

_LAZY = {
    "DIAR_FINALIZE_TIMEOUT_S": "collector",
    "DiarizationCollector": "collector",
    "DiarizationResult": "collector",
    "GAP_TOLERANCE_S": "align",
    "SpeakerRun": "align",
    "assign_speakers": "align",
    "merge_runs": "align",
    "MockDiarizer": "mock",
    "MockEmbedder": "mock",
    "tone_index": "mock",
    "SherpaDiarizer": "sherpa",
    "SherpaEmbedder": "sherpa",
    "load_sherpa_backends": "sherpa",
    "build_diarization": "factory",
}

__all__ = [
    "DIAR_FINALIZE_TIMEOUT_S",
    "GAP_TOLERANCE_S",
    "SOURCE_ME",
    "SOURCE_OTHERS",
    "DiarizationCollector",
    "DiarizationResult",
    "DiarizationUnavailable",
    "Diarizer",
    "MeetingSpeaker",
    "MockDiarizer",
    "MockEmbedder",
    "SherpaDiarizer",
    "SherpaEmbedder",
    "SpeakerEmbedder",
    "SpeakerRegistry",
    "SpeakerRun",
    "SpeakerTurn",
    "TimedText",
    "TurnEmbedding",
    "Voiceprint",
    "assign_speakers",
    "build_diarization",
    "cluster",
    "load_sherpa_backends",
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
