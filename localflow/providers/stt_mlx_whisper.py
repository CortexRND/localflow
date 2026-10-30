import numpy as np

from localflow.providers.base import ModelInfo

_MLX_REPOS = {
    "tiny": "mlx-community/whisper-tiny-mlx",
    "base": "mlx-community/whisper-base-mlx",
    "small": "mlx-community/whisper-small-mlx",
    "medium": "mlx-community/whisper-medium-mlx",
    "large-v3": "mlx-community/whisper-large-v3-mlx",
}


class MlxWhisperSTT:
    id = "mlx-whisper"
    offline = True

    def __init__(self) -> None:
        self._mlx = None
        self._mlx_repo = ""
        self.language: str | None = None

    def load(
        self, model: str, language: str | None, device: str = "auto"
    ) -> None:
        if device != "auto":
            raise ValueError(f"mlx-whisper does not support device {device!r}")
        import mlx_whisper

        self._mlx = mlx_whisper
        self._mlx_repo = _MLX_REPOS[model]
        self.language = language
        # Trigger model download/load now, not on first dictation.
        mlx_whisper.transcribe(
            np.zeros(1600, dtype=np.float32), path_or_hf_repo=self._mlx_repo
        )

    def transcribe(self, audio: np.ndarray) -> str:
        result = self._mlx.transcribe(
            audio, path_or_hf_repo=self._mlx_repo, language=self.language
        )
        return result["text"].strip()

    def list_models(self) -> list[ModelInfo]:
        return [
            ModelInfo(id=model, label=model, installed=None)
            for model in _MLX_REPOS
        ]
