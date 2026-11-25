"""Speaker diarization core: contracts, alignment, clustering and mock backends."""

from .align import GAP_TOLERANCE_S, SpeakerRun, assign_speakers, merge_runs
from .cluster import cluster
from .mock import MockDiarizer, MockEmbedder, tone_index
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
