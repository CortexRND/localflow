import numpy as np

from localflow.providers.base import ModelInfo


class FasterWhisperSTT:
    id = "faster-whisper"
    offline = True

    def __init__(self) -> None:
        self.model = None
        self.language: str | None = None

    def load(self, model: str, language: str | None) -> None:
        from faster_whisper import WhisperModel

        self.model = WhisperModel(model, device="cpu", compute_type="int8")
        self.language = language

    def transcribe(self, audio: np.ndarray) -> str:
        segments, _info = self.model.transcribe(
            audio,
            language=self.language,
            vad_filter=True,
        )
        return " ".join(segment.text.strip() for segment in segments).strip()

    def list_models(self) -> list[ModelInfo]:
        return [
            ModelInfo(id=model, label=model, installed=None)
            for model in ("tiny", "base", "small", "medium", "large-v3")
        ]
