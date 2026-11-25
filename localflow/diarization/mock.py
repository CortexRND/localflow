"""Deterministic CI-only backends decoding the tone encoding of ``evals/meeting_sim/driver.py``."""

from __future__ import annotations

from collections import Counter
from collections.abc import Callable, Mapping
from typing import ClassVar

import numpy as np

from .types import SpeakerTurn

FRAME_S = 0.5
_SILENCE_PEAK = 0.01

SpeakerOf = Mapping[int, int] | Callable[[int], int] | None


def tone_index(frame: np.ndarray) -> int | None:
    """Line index of a constant tone ``0.1 + i*0.001`` from the median of the voiced samples
    (``abs >= 0.01``); ``None`` when no sample is voiced (silence or an empty frame)."""
    frame = np.asarray(frame)
    voiced = frame[np.abs(frame) >= _SILENCE_PEAK]
    if voiced.size == 0:
        return None
    return round((float(np.median(voiced)) - 0.1) / 0.001)


def _speaker_key(index: int, speaker_of: SpeakerOf) -> int:
    if speaker_of is None:
        return index
    if isinstance(speaker_of, Mapping):
        return speaker_of.get(index, index)
    return speaker_of(index)


def _frame_keys(
    audio: np.ndarray, sample_rate: int, speaker_of: SpeakerOf
) -> tuple[np.ndarray, int, list[int | None]]:
    """Validate audio; return it, the frame step and each ``FRAME_S`` frame's key or None."""
    audio = np.asarray(audio)
    if audio.ndim != 1:
        raise ValueError(f"audio must be 1-D, got shape {audio.shape}")
    if sample_rate <= 0:
        raise ValueError(f"sample_rate must be positive, got {sample_rate}")
    step = max(1, round(FRAME_S * sample_rate))
    keys = []
    for begin in range(0, len(audio), step):
        idx = tone_index(audio[begin : begin + step])
        keys.append(None if idx is None else _speaker_key(idx, speaker_of))
    return audio, step, keys


class MockDiarizer:
    """Turns from 0.5 s frames: each tone index (or its ``speaker_of`` key) is one voice.

    Unmapped lines keep their own index as key."""

    id: ClassVar[str] = "mock"

    def __init__(self, speaker_of: SpeakerOf = None) -> None:
        self.speaker_of = speaker_of

    def load(self) -> None:
        pass

    def diarize(self, audio: np.ndarray, sample_rate: int) -> list[SpeakerTurn]:
        audio, step, keys = _frame_keys(audio, sample_rate, self.speaker_of)
        turns: list[SpeakerTurn] = []
        local: dict[int, int] = {}
        first = 0
        for f, key in enumerate(keys):
            if f + 1 < len(keys) and keys[f + 1] == key:
                continue
            if key is not None:
                end = min((f + 1) * step, len(audio))
                speaker = local.setdefault(key, len(local))
                turns.append(
                    SpeakerTurn(first * step / sample_rate, end / sample_rate, speaker)
                )
            first = f + 1
        return turns


class MockEmbedder:
    """One-hot float32 vector on ``key % dim`` for the clip's most common speaker key.

    Each voiced ``FRAME_S`` frame is decoded and mapped through ``speaker_of`` (unmapped lines
    keep their own index as key); ties go to the key that appears first. Keys alias modulo
    ``dim``. An all-silent clip yields the fixed "unknown" unit vector ``1/sqrt(dim)`` in
    every component (cosine ``1/sqrt(dim)`` to every key, 0.125 at dim 64).
    """

    id: ClassVar[str] = "mock"

    def __init__(self, dim: int = 64, speaker_of: SpeakerOf = None) -> None:
        if dim < 1:
            raise ValueError(f"dim must be >= 1, got {dim}")
        self.dim = dim
        self.speaker_of = speaker_of

    def load(self) -> None:
        pass

    def embed(self, audio: np.ndarray, sample_rate: int) -> np.ndarray:
        _, _, keys = _frame_keys(audio, sample_rate, self.speaker_of)
        counts = Counter(k for k in keys if k is not None)
        if not counts:
            return np.full(self.dim, 1.0 / np.sqrt(self.dim), dtype=np.float32)
        vec = np.zeros(self.dim, dtype=np.float32)
        vec[counts.most_common(1)[0][0] % self.dim] = 1.0
        return vec
