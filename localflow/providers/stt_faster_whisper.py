import inspect

import numpy as np

from localflow.providers.base import ModelInfo


class FasterWhisperSTT:
    id = "faster-whisper"
    offline = True

    def __init__(self) -> None:
        self.model = None
        self.language: str | None = None
        self._supports_hotwords = False

    def load(self, model: str, language: str | None, device: str = "auto") -> None:
        if device in ("auto", "cpu"):
            runtime_device, compute_type = "cpu", "int8"
        elif device == "cuda":
            runtime_device, compute_type = "cuda", "float16"
        else:
            raise ValueError(f"unsupported stt device: {device!r}")
        from faster_whisper import WhisperModel

        self.model = WhisperModel(
            model, device=runtime_device, compute_type=compute_type
        )
        try:
            self._supports_hotwords = (
                "hotwords" in inspect.signature(self.model.transcribe).parameters
            )
        except (TypeError, ValueError):
            self._supports_hotwords = False
        self.language = language

    def transcribe(
        self, audio: np.ndarray, *, prompt: str | None = None
    ) -> str:
        options = {"language": self.language, "vad_filter": True}
        if prompt is not None:
            options["initial_prompt"] = prompt
            if self._supports_hotwords:
                options["hotwords"] = prompt
        segments, _info = self.model.transcribe(audio, **options)
        return " ".join(segment.text.strip() for segment in segments).strip()

    def list_models(self) -> list[ModelInfo]:
        return [
            ModelInfo(id=model, label=model, installed=None)
            for model in ("tiny", "base", "small", "medium", "large-v3")
        ]
