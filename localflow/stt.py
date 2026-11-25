import numpy as np

from localflow.diarization.types import TimedText
from localflow.providers import registry
from localflow.providers.base import STTProvider
from localflow.providers.stt_faster_whisper import FasterWhisperSTT
from localflow.providers.stt_mlx_whisper import _MLX_REPOS, MlxWhisperSTT
from localflow.providers.stt_parakeet import PARAKEET_REPO, ParakeetSTT

__all__ = ["PARAKEET_REPO", "Transcriber"]


class Transcriber:
    """STT front-end. backend='auto' uses mlx-whisper (Apple GPU) when installed
    and the model size has an MLX build, else falls back to faster-whisper CPU int8.
    Measured on M-series: MLX small 951ms vs CPU small 3202ms on a 13.8s clip.
    backend='parakeet' uses NVIDIA Parakeet TDT 0.6B via parakeet-mlx (English-only)."""

    def __init__(
        self,
        model_size: str = "small",
        language: str | None = None,
        backend: str = "auto",
        device: str = "auto",
    ):
        self.language = language
        self.model_size = model_size
        self.provider: STTProvider

        if backend not in ("auto", "mlx", "mlx-whisper", "faster-whisper", "parakeet"):
            try:
                provider_class = registry.stt_provider_class(backend)
            except KeyError as exc:
                raise ValueError(f"unknown stt backend: {backend!r}") from exc
            provider = provider_class()
            provider.load(model_size, language, device)
            self.provider = provider
            self.backend = backend
            return

        if backend in ("mlx", "mlx-whisper") and device != "auto":
            raise ValueError(f"{backend} does not support device {device!r}")
        if backend == "parakeet" and device != "auto":
            raise ValueError(f"{backend} does not support device {device!r}")

        if backend == "parakeet":
            provider: STTProvider = ParakeetSTT()
            provider.load(model_size, language, device)
            self.provider = provider
            self.backend = "parakeet"
            return

        if (
            backend in ("auto", "mlx", "mlx-whisper")
            and device == "auto"
            and model_size in _MLX_REPOS
        ):
            provider = MlxWhisperSTT()
            try:
                provider.load(model_size, language, device)
                self.provider = provider
                self.backend = "mlx"
                return
            except ImportError:
                if backend in ("mlx", "mlx-whisper"):
                    raise

        provider = FasterWhisperSTT()
        provider.load(model_size, language, device)
        self.provider = provider
        self.backend = "faster-whisper"

    def transcribe(self, audio: np.ndarray, *, prompt: str | None = None) -> str:
        """audio: 1-D float32 16kHz. Return joined stripped text."""
        if prompt is None:
            return self.provider.transcribe(audio)
        return self.provider.transcribe(audio, prompt=prompt)

    def transcribe_timed(
        self, audio: np.ndarray, *, prompt: str | None = None
    ) -> list[TimedText]:
        """Segments timed in seconds from audio start. Providers without
        transcribe_timed yield one segment spanning the whole audio."""
        timed = getattr(self.provider, "transcribe_timed", None)
        if callable(timed):
            if prompt is None:
                return timed(audio)
            return timed(audio, prompt=prompt)
        text = self.transcribe(audio, prompt=prompt)
        return [TimedText(0.0, len(audio) / 16000, text)]
