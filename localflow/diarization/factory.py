"""Build the meeting diarization collector from config (design §5.6).

``(collector, "")`` = collecting, ``(None, "")`` = off, ``(None, reason)`` = unavailable;
never raises. With the flag off nothing else is imported.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from localflow.config import Config

    from .collector import DiarizationCollector
    from .types import SpeakerEmbedder

log = logging.getLogger("localflow.diarization")


def _max_speakers(value: object) -> int | None:
    if isinstance(value, int) and not isinstance(value, bool) and 1 <= value <= 10:
        return value
    return None


def build_diarization(config: Config) -> tuple[DiarizationCollector | None, str]:
    """The collector per ``config``, or ``(None, reason)``. Never raises."""
    try:
        if not getattr(config, "speaker_diarization", False):
            return None, ""
        backend = getattr(config, "diarization_backend", "sherpa-onnx")
        if backend not in ("sherpa-onnx", "mock"):
            return None, f"unknown diarization backend {backend!r}"
        from .collector import DiarizationCollector

        if backend == "sherpa-onnx":
            from .sherpa import load_sherpa_backends

            diarizer, embedder = load_sherpa_backends()
        else:
            from .mock import MockDiarizer, MockEmbedder

            diarizer, embedder = MockDiarizer(), MockEmbedder()
            diarizer.load()
            embedder.load()
        collector = DiarizationCollector(
            diarizer, embedder,
            sample_rate=config.sample_rate,
            diarize_mic=bool(config.diarize_mic),
            max_speakers=_max_speakers(getattr(config, "diarization_max_speakers", 0)),
        )
        return collector, ""
    except Exception as exc:  # noqa: BLE001 - diarization is optional
        reason = str(exc) or type(exc).__name__
        log.warning("speaker diarization unavailable: %s", reason)
        return None, reason


def build_embedder(config: Config) -> SpeakerEmbedder:
    """A loaded embedder for enrollment from audio; raises DiarizationUnavailable.

    Not cached: enrollment is rare, and keeping the model loaded would change idle memory.
    """
    from .types import DiarizationUnavailable

    backend = getattr(config, "diarization_backend", "sherpa-onnx")
    if backend == "mock":
        from .mock import MockEmbedder

        embedder = MockEmbedder()
    elif backend == "sherpa-onnx":
        from .sherpa import SherpaEmbedder

        embedder = SherpaEmbedder()
    else:
        raise DiarizationUnavailable("unknown diarization backend")
    try:
        embedder.load()
    except DiarizationUnavailable:
        raise
    except Exception as exc:  # noqa: BLE001 - surfaced as "unavailable"
        raise DiarizationUnavailable(
            f"The voice model could not be loaded ({type(exc).__name__}).") from None
    return embedder
