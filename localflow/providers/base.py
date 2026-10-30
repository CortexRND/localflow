from dataclasses import dataclass
from typing import ClassVar, Protocol, runtime_checkable

import numpy as np


class ProviderError(RuntimeError):
    """Raised when an inference provider cannot complete a request."""


@dataclass(frozen=True)
class ModelInfo:
    id: str
    label: str = ""
    installed: bool | None = None


@dataclass(frozen=True)
class Health:
    ok: bool
    detail: str = ""


@runtime_checkable
class STTProvider(Protocol):
    id: ClassVar[str]
    offline: ClassVar[bool]

    def load(
        self, model: str, language: str | None, device: str = "auto"
    ) -> None: ...

    def transcribe(self, audio: np.ndarray) -> str: ...

    def list_models(self) -> list[ModelInfo]: ...


@runtime_checkable
class LLMProvider(Protocol):
    id: ClassVar[str]
    offline: ClassVar[bool]

    def complete(
        self, system: str, user: str, *, max_tokens: int | None = None
    ) -> str: ...

    def list_models(self) -> list[ModelInfo]: ...

    def healthcheck(self) -> Health: ...
