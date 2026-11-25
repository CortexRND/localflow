"""Shared dataclasses and protocols for speaker diarization."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar, Protocol, runtime_checkable

import numpy as np

SOURCE_ME = "me"
SOURCE_OTHERS = "others"


@dataclass(frozen=True)
class TimedText:
    """One STT segment inside a chunk (seconds from chunk start)."""

    start: float
    end: float
    text: str


@dataclass(frozen=True)
class SpeakerTurn:
    """Chunk-local diarization output; ``local_speaker`` is only meaningful within the chunk."""

    start: float
    end: float
    local_speaker: int


@dataclass(frozen=True, eq=False)
class TurnEmbedding:
    """A turn's L2-normalised float32 embedding, kept in RAM until finalize."""

    source: str
    chunk_offset: float
    turn: SpeakerTurn
    vector: np.ndarray


@dataclass(frozen=True)
class MeetingSpeaker:
    id: str
    source: str
    label: str
    name: str | None
    voiceprint_id: str | None
    seconds: float


@dataclass(frozen=True)
class Voiceprint:
    """An enrolled speaker; ``created`` is an ISO-8601 UTC timestamp."""

    id: str
    name: str
    model_id: str
    created: str
    samples: int


class DiarizationUnavailable(RuntimeError):
    """Raised when a diarization backend cannot be loaded or used."""


@runtime_checkable
class Diarizer(Protocol):
    id: ClassVar[str]

    def load(self) -> None: ...

    def diarize(self, audio: np.ndarray, sample_rate: int) -> list[SpeakerTurn]: ...


@runtime_checkable
class SpeakerEmbedder(Protocol):
    id: ClassVar[str]
    dim: int

    def load(self) -> None: ...

    def embed(self, audio: np.ndarray, sample_rate: int) -> np.ndarray: ...


@runtime_checkable
class SpeakerRegistry(Protocol):
    def list(self) -> list[Voiceprint]: ...

    def enroll(self, name: str, vectors: list[np.ndarray], model_id: str) -> Voiceprint: ...

    def match(self, vector: np.ndarray, model_id: str) -> tuple[Voiceprint, float] | None: ...

    def rename(self, voiceprint_id: str, name: str) -> None: ...

    def forget(self, voiceprint_id: str | None = None) -> int: ...

    def export(self, target: Path) -> Path: ...
