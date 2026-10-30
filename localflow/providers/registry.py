import importlib.metadata
import importlib.util
import sys

from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM
from localflow.providers.stt_faster_whisper import FasterWhisperSTT
from localflow.providers.stt_mlx_whisper import MlxWhisperSTT
from localflow.providers.stt_parakeet import ParakeetSTT

STT_BUILTINS = {
    "faster-whisper": FasterWhisperSTT,
    "mlx-whisper": MlxWhisperSTT,
    "parakeet": ParakeetSTT,
}
LLM_BUILTINS = {
    "ollama": OllamaLLM,
    "openai-compatible": OpenAICompatLLM,
}


def _entry_points(group: str):
    return importlib.metadata.entry_points(group=group)


def _valid_ids(builtins: dict[str, type], group: str) -> list[str]:
    return sorted({*builtins, *(entry_point.name for entry_point in _entry_points(group))})


def stt_provider_class(id: str):
    if id in STT_BUILTINS:
        return STT_BUILTINS[id]
    for entry_point in _entry_points("localflow.stt_providers"):
        if entry_point.name == id:
            return entry_point.load()
    valid = ", ".join(_valid_ids(STT_BUILTINS, "localflow.stt_providers"))
    raise KeyError(f"unknown STT provider {id!r}; valid ids: {valid}")


def llm_provider_class(id: str):
    if id in LLM_BUILTINS:
        return LLM_BUILTINS[id]
    for entry_point in _entry_points("localflow.llm_providers"):
        if entry_point.name == id:
            return entry_point.load()
    valid = ", ".join(_valid_ids(LLM_BUILTINS, "localflow.llm_providers"))
    raise KeyError(f"unknown LLM provider {id!r}; valid ids: {valid}")


def list_stt_ids() -> list[str]:
    return _valid_ids(STT_BUILTINS, "localflow.stt_providers")


def stt_available(id: str) -> bool:
    if id not in STT_BUILTINS:
        return True
    if id == "faster-whisper":
        module = "faster_whisper"
    elif id == "mlx-whisper":
        if sys.platform != "darwin":
            return False
        module = "mlx_whisper"
    else:
        if sys.platform != "darwin":
            return False
        module = "parakeet_mlx"
    try:
        return importlib.util.find_spec(module) is not None
    except (ImportError, ValueError):
        return False


def list_llm_ids() -> list[str]:
    return _valid_ids(LLM_BUILTINS, "localflow.llm_providers")
