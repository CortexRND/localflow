import numpy as np

from localflow.providers.base import ModelInfo

PARAKEET_REPO = "mlx-community/parakeet-tdt-0.6b-v2"


class ParakeetSTT:
    id = "parakeet"
    offline = True

    def __init__(self) -> None:
        self._mx = None
        self._parakeet_logmel = None
        self._parakeet = None
        self.language: str | None = None

    def load(
        self, model: str, language: str | None, device: str = "auto"
    ) -> None:
        del device
        import mlx.core as mx
        from parakeet_mlx import from_pretrained
        from parakeet_mlx.audio import get_logmel

        self._mx = mx
        self._parakeet_logmel = get_logmel
        self.language = language
        # Downloads weights on first use; run a silent clip so the graph is
        # compiled before the first dictation.
        self._parakeet = from_pretrained(PARAKEET_REPO)
        mx.eval(self._parakeet.parameters())
        self._parakeet_generate(np.zeros(16000, dtype=np.float32))

    def _parakeet_generate(self, audio: np.ndarray) -> str:
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

    def transcribe(self, audio: np.ndarray) -> str:
        return self._parakeet_generate(audio)

    def list_models(self) -> list[ModelInfo]:
        return [ModelInfo(id=PARAKEET_REPO, label=PARAKEET_REPO, installed=None)]
