"""sherpa-onnx diarization and speaker-embedding backends (the ``diarization`` extra).

``sherpa_onnx`` is imported only inside ``load()``, and models load from verified local
paths only, so nothing here can download. The extra is checked before the models because
it is the bigger missing piece.
"""

from __future__ import annotations

import logging
import time
from pathlib import Path
from typing import ClassVar

import numpy as np

from . import models
from .types import DiarizationUnavailable, SpeakerTurn

log = logging.getLogger(__name__)

SEGMENTATION_MODEL_ID = models.DIAR_MODELS[0].id
EMBEDDING_MODEL_ID = models.DIAR_MODELS[1].id
DEFAULT_THRESHOLD = 0.5
EMBED_SAMPLE_RATE = 16000
MIN_DURATION_ON = 0.3
MIN_DURATION_OFF = 0.5
MIN_AUDIO_S = MIN_DURATION_ON
PROVIDER = "cpu"

_INSTALL_HINT = (
    "Speaker labels need the diarization extra: pip install 'localflow[diarization]'."
)


def _runtime(cache: Path | None):
    """(sherpa_onnx module, {model id: local path str}); raises DiarizationUnavailable."""
    try:
        import sherpa_onnx
    except ImportError as exc:
        raise DiarizationUnavailable(_INSTALL_HINT) from exc
    try:
        paths = models.verified_model_paths(cache)
    except models.DiarizationModelsUnavailable as exc:
        raise DiarizationUnavailable(str(exc)) from exc
    return sherpa_onnx, {model_id: str(path) for model_id, path in paths.items()}


def _build(what: str, config, factory):
    """``factory(config)`` after ``config.validate()``; native errors become DiarizationUnavailable."""
    try:
        if not config.validate():
            raise DiarizationUnavailable(f"The speaker {what} configuration is invalid.")
        return factory(config)
    except DiarizationUnavailable:
        raise
    except Exception as exc:
        raise DiarizationUnavailable(f"Could not load the speaker {what} model.") from exc


def _samples(audio: np.ndarray, sample_rate: int) -> np.ndarray:
    audio = np.asarray(audio)
    if audio.ndim != 1:
        raise ValueError(f"audio must be 1-D, got shape {audio.shape}")
    if sample_rate <= 0:
        raise ValueError(f"sample_rate must be positive, got {sample_rate}")
    return np.ascontiguousarray(audio, dtype=np.float32)


def _at_rate(samples: np.ndarray, sample_rate: int, target: int) -> np.ndarray:
    if sample_rate == target or samples.size == 0:
        return samples
    from localflow.system_audio import resample

    return np.ascontiguousarray(resample(samples, sample_rate, target), dtype=np.float32)


class SherpaDiarizer:
    """pyannote segmentation + embedding clustering; turns are chunk-local.

    ``max_speakers`` caps the count: clustering runs by ``threshold`` and, if it finds more
    speakers than the cap, runs once more with exactly ``max_speakers`` clusters.
    ``diarize`` raises DiarizationUnavailable before ``load()``.
    """

    id: ClassVar[str] = SEGMENTATION_MODEL_ID

    def __init__(
        self,
        *,
        cache: Path | None = None,
        num_threads: int = 1,
        threshold: float = DEFAULT_THRESHOLD,
        max_speakers: int | None = None,
    ) -> None:
        if max_speakers is not None and max_speakers < 1:
            raise ValueError(f"max_speakers must be None or >= 1, got {max_speakers}")
        self.cache = cache
        self.num_threads = num_threads
        self.threshold = threshold
        self.max_speakers = max_speakers
        self._engine = None
        self._config = None

    def load(self) -> None:
        if self._engine is not None:
            return
        sherpa_onnx, paths = _runtime(self.cache)

        def config(num_clusters: int):
            return sherpa_onnx.OfflineSpeakerDiarizationConfig(
                segmentation=sherpa_onnx.OfflineSpeakerSegmentationModelConfig(
                    pyannote=sherpa_onnx.OfflineSpeakerSegmentationPyannoteModelConfig(
                        model=paths[SEGMENTATION_MODEL_ID]
                    ),
                    num_threads=self.num_threads,
                    provider=PROVIDER,
                ),
                embedding=sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                    model=paths[EMBEDDING_MODEL_ID],
                    num_threads=self.num_threads,
                    provider=PROVIDER,
                ),
                clustering=sherpa_onnx.FastClusteringConfig(
                    num_clusters=num_clusters, threshold=self.threshold
                ),
                min_duration_on=MIN_DURATION_ON,
                min_duration_off=MIN_DURATION_OFF,
            )

        default = config(1 if self.max_speakers == 1 else -1)
        self._engine = _build("diarization", default, sherpa_onnx.OfflineSpeakerDiarization)
        self._config = config
        log.info("sherpa diarizer loaded (%d threads)", self.num_threads)

    def _run(self, samples: np.ndarray) -> list[tuple[float, float, int]]:
        result = self._engine.process(samples).sort_by_start_time()
        segments = [(float(s.start), float(s.end), int(s.speaker)) for s in result]
        return [s for s in segments if s[1] > s[0]]

    def diarize(self, audio: np.ndarray, sample_rate: int) -> list[SpeakerTurn]:
        samples = _samples(audio, sample_rate)
        if self._engine is None:
            raise DiarizationUnavailable("The speaker diarizer is not loaded.")
        rate = int(self._engine.sample_rate)
        samples = _at_rate(samples, sample_rate, rate)
        if samples.size < MIN_AUDIO_S * rate:
            return []
        started = time.monotonic()
        segments = self._run(samples)
        cap = self.max_speakers
        if cap is not None and cap > 1 and len({s[2] for s in segments}) > cap:
            self._engine.set_config(self._config(cap))
            try:
                segments = self._run(samples)
            finally:
                self._engine.set_config(self._config(-1))
        local: dict[int, int] = {}
        turns = [
            SpeakerTurn(start, end, local.setdefault(speaker, len(local)))
            for start, end, speaker in sorted(segments)
        ]
        log.debug(
            "diarized %.1fs of audio: %d turns, %d speakers in %.2fs",
            samples.size / rate, len(turns), len(local), time.monotonic() - started,
        )
        return turns


class SherpaEmbedder:
    """Speaker embeddings (L2-normalised float32); ``dim`` is 0 until ``load()``.

    ``embed`` raises DiarizationUnavailable before ``load()`` and returns the zero vector
    for audio too short to embed.
    """

    id: ClassVar[str] = EMBEDDING_MODEL_ID

    def __init__(self, *, cache: Path | None = None, num_threads: int = 1) -> None:
        self.cache = cache
        self.num_threads = num_threads
        self.dim = 0
        self._extractor = None

    def load(self) -> None:
        if self._extractor is not None:
            return
        sherpa_onnx, paths = _runtime(self.cache)
        config = sherpa_onnx.SpeakerEmbeddingExtractorConfig(
            model=paths[EMBEDDING_MODEL_ID], num_threads=self.num_threads, provider=PROVIDER
        )
        extractor = _build("embedding", config, sherpa_onnx.SpeakerEmbeddingExtractor)
        self.dim = int(extractor.dim)
        self._extractor = extractor
        log.info("sherpa embedder loaded (dim %d)", self.dim)

    def embed(self, audio: np.ndarray, sample_rate: int) -> np.ndarray:
        samples = _samples(audio, sample_rate)
        if self._extractor is None:
            raise DiarizationUnavailable("The speaker embedder is not loaded.")
        samples = _at_rate(samples, sample_rate, EMBED_SAMPLE_RATE)
        zeros = np.zeros(self.dim, dtype=np.float32)
        if samples.size == 0:
            return zeros
        stream = self._extractor.create_stream()
        stream.accept_waveform(EMBED_SAMPLE_RATE, samples)
        stream.input_finished()
        if not self._extractor.is_ready(stream):
            return zeros
        vector = np.asarray(self._extractor.compute(stream), dtype=np.float32).reshape(-1)
        norm = float(np.linalg.norm(vector))
        if not np.isfinite(norm) or norm == 0.0:
            return zeros
        return (vector / norm).astype(np.float32)


def load_sherpa_backends(
    *, cache: Path | None = None, num_threads: int = 1, max_speakers: int | None = None
) -> tuple[SherpaDiarizer, SherpaEmbedder]:
    """Construct and load both backends; raises only DiarizationUnavailable."""
    try:
        diarizer = SherpaDiarizer(
            cache=cache, num_threads=num_threads, max_speakers=max_speakers
        )
        embedder = SherpaEmbedder(cache=cache, num_threads=num_threads)
        diarizer.load()
        embedder.load()
    except DiarizationUnavailable:
        raise
    except Exception as exc:
        raise DiarizationUnavailable("Could not load the speaker models.") from exc
    return diarizer, embedder
