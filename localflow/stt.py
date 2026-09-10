import numpy as np

_MLX_REPOS = {
    "tiny": "mlx-community/whisper-tiny-mlx",
    "base": "mlx-community/whisper-base-mlx",
    "small": "mlx-community/whisper-small-mlx",
    "medium": "mlx-community/whisper-medium-mlx",
    "large-v3": "mlx-community/whisper-large-v3-mlx",
}

# NVIDIA Parakeet (NeMo TDT) via parakeet-mlx. Fixed checkpoint; English-only,
# so model_size and language are ignored on this backend.
PARAKEET_REPO = "mlx-community/parakeet-tdt-0.6b-v2"


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
        self._mlx = None
        self._parakeet = None
        self.model = None

        if backend not in ("auto", "mlx", "faster-whisper", "parakeet"):
            raise ValueError(f"unknown stt backend: {backend!r}")

        if backend == "parakeet":
            import mlx.core as mx
            from parakeet_mlx import from_pretrained
            from parakeet_mlx.audio import get_logmel

            self._mx = mx
            self._parakeet_logmel = get_logmel
            # Downloads weights on first use; run a silent clip so the graph is
            # compiled before the first dictation.
            self._parakeet = from_pretrained(PARAKEET_REPO)
            self._parakeet_generate(np.zeros(16000, dtype=np.float32))
            self.backend = "parakeet"
            return

        if backend in ("auto", "mlx") and model_size in _MLX_REPOS:
            try:
                import mlx_whisper

                self._mlx = mlx_whisper
                self._mlx_repo = _MLX_REPOS[model_size]
                # Trigger model download/load now, not on first dictation.
                mlx_whisper.transcribe(
                    np.zeros(1600, dtype=np.float32), path_or_hf_repo=self._mlx_repo
                )
                self.backend = "mlx"
                return
            except ImportError:
                if backend == "mlx":
                    raise

        from faster_whisper import WhisperModel

        self.model = WhisperModel(model_size, device="cpu", compute_type="int8")
        self.backend = "faster-whisper"

    def _parakeet_generate(self, audio: "np.ndarray") -> str:
        # Bypass model.transcribe(path), which shells out to ffmpeg on a file;
        # feed the in-memory array straight into the log-mel front-end instead.
        cfg = self._parakeet.preprocessor_config
        x = self._mx.array(np.ascontiguousarray(audio, dtype=np.float32))
        # The reflect-padded STFT needs at least one full FFT frame of input.
        if x.shape[-1] < cfg.n_fft:
            return ""
        mel = self._parakeet_logmel(x, cfg)
        result = self._parakeet.generate(mel)[0]
        return result.text.strip()

    def transcribe(self, audio: "np.ndarray") -> str:
        """audio: 1-D float32 16kHz. Return joined stripped text."""
        if self._parakeet is not None:
            return self._parakeet_generate(audio)
        if self._mlx is not None:
            result = self._mlx.transcribe(
                audio, path_or_hf_repo=self._mlx_repo, language=self.language
            )
            return result["text"].strip()
        segments, _info = self.model.transcribe(
            audio,
            language=self.language,
            vad_filter=True,
        )
        return " ".join(segment.text.strip() for segment in segments).strip()
