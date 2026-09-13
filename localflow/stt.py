import numpy as np

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
    ):
        self.language = language
        self.model_size = model_size
        self.model = None
        self._mlx = None
        self._parakeet = None
        self.provider: STTProvider

        if backend not in ("auto", "mlx", "faster-whisper", "parakeet"):
            raise ValueError(f"unknown stt backend: {backend!r}")

        if backend == "parakeet":
            provider: STTProvider = ParakeetSTT()
            provider.load(model_size, language)
            self.provider = provider
            self.backend = "parakeet"
            self._sync_legacy_attributes()
            return

        if backend in ("auto", "mlx") and model_size in _MLX_REPOS:
            provider = MlxWhisperSTT()
            try:
                provider.load(model_size, language)
                self.provider = provider
                self.backend = "mlx"
                self._sync_legacy_attributes()
                return
            except ImportError:
                if backend == "mlx":
                    raise

        provider = FasterWhisperSTT()
        provider.load(model_size, language)
        self.provider = provider
        self.backend = "faster-whisper"
        self._sync_legacy_attributes()

    def _sync_legacy_attributes(self) -> None:
        self.model = getattr(self.provider, "model", None)
        self._mlx = getattr(self.provider, "_mlx", None)
        self._parakeet = getattr(self.provider, "_parakeet", None)
        if hasattr(self.provider, "_parakeet_logmel"):
            self._parakeet_logmel = self.provider._parakeet_logmel
        if hasattr(self.provider, "_mx"):
            self._mx = self.provider._mx

    def transcribe(self, audio: np.ndarray) -> str:
        """audio: 1-D float32 16kHz. Return joined stripped text."""
        return self.provider.transcribe(audio)
