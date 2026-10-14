import sys
from types import ModuleType, SimpleNamespace

import numpy as np
import pytest
import requests

from localflow.cleanup import Cleaner
from localflow.providers.base import (
    LLMProvider,
    ModelInfo,
    ProviderError,
    STTProvider,
)
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM
from localflow.providers.registry import (
    list_llm_ids,
    list_stt_ids,
    llm_provider_class,
    stt_provider_class,
)
from localflow.providers.stt_faster_whisper import FasterWhisperSTT
from localflow.providers.stt_mlx_whisper import MlxWhisperSTT
from localflow.providers.stt_parakeet import ParakeetSTT
from localflow.stt import Transcriber
from localflow.workprompts import WorkPromptGenerator


class FakeResponse:
    def __init__(self, payload, status=200):
        self.payload = payload
        self.status = status

    def raise_for_status(self):
        if self.status >= 400:
            raise requests.HTTPError(f"status {self.status}")

    def json(self):
        return self.payload


def test_registry_resolves_builtins_and_lists_ids():
    assert stt_provider_class("faster-whisper") is FasterWhisperSTT
    assert llm_provider_class("ollama") is OllamaLLM
    assert set(list_stt_ids()) >= {"faster-whisper", "mlx-whisper", "parakeet"}
    assert set(list_llm_ids()) >= {"ollama", "openai-compatible"}


def test_registry_unknown_id_lists_valid_ids():
    with pytest.raises(KeyError) as stt_error:
        stt_provider_class("missing")
    assert all(
        provider_id in str(stt_error.value)
        for provider_id in ("faster-whisper", "mlx-whisper", "parakeet")
    )

    with pytest.raises(KeyError) as llm_error:
        llm_provider_class("missing")
    assert all(
        provider_id in str(llm_error.value)
        for provider_id in ("ollama", "openai-compatible")
    )


def test_registry_discovers_entry_point(monkeypatch):
    provider = type("FakeSTT", (), {})
    entry_point = SimpleNamespace(name="fake", load=lambda: provider)
    monkeypatch.setattr(
        "importlib.metadata.entry_points",
        lambda *, group: [entry_point] if group == "localflow.stt_providers" else [],
    )

    assert stt_provider_class("fake") is provider
    assert "fake" in list_stt_ids()


def test_ollama_complete_payload_and_quote_stripping(monkeypatch):
    calls = []

    def post(url, **kwargs):
        calls.append((url, kwargs))
        return FakeResponse({"response": '  "Cleaned text"  '})

    monkeypatch.setattr("requests.post", post)
    llm = OllamaLLM("http://ollama/", "llama", timeout=17, num_ctx=4096)

    assert llm.complete("System", "User") == "Cleaned text"
    assert calls == [
        (
            "http://ollama/api/generate",
            {
                "json": {
                    "model": "llama",
                    "prompt": "System\n\nUser",
                    "options": {"num_ctx": 4096, "num_predict": -1},
                    "stream": False,
                },
                "timeout": 17,
            },
        )
    ]


def test_ollama_empty_system_uses_raw_user_prompt(monkeypatch):
    calls = []

    def post(url, **kwargs):
        calls.append(kwargs)
        return FakeResponse({"response": "result"})

    monkeypatch.setattr("requests.post", post)

    OllamaLLM("http://ollama", "llama").complete("", "raw prompt")

    assert calls[0]["json"]["prompt"] == "raw prompt"


@pytest.mark.parametrize(
    "response",
    [FakeResponse({"response": "  "}), FakeResponse({}, status=500)],
)
def test_ollama_complete_errors(monkeypatch, response):
    monkeypatch.setattr("requests.post", lambda *args, **kwargs: response)

    with pytest.raises(ProviderError):
        OllamaLLM("http://ollama", "llama").complete("", "text")


def test_ollama_list_models(monkeypatch):
    monkeypatch.setattr(
        "requests.get",
        lambda *args, **kwargs: FakeResponse(
            {"models": [{"name": "llama3"}, {"name": "qwen"}]}
        ),
    )

    assert OllamaLLM("http://ollama", "llama").list_models() == [
        ModelInfo(id="llama3", label="llama3", installed=True),
        ModelInfo(id="qwen", label="qwen", installed=True),
    ]


def test_openai_compat_complete_and_bearer(monkeypatch):
    calls = []

    def post(url, **kwargs):
        calls.append((url, kwargs))
        return FakeResponse({"choices": [{"message": {"content": " result "}}]})

    monkeypatch.setattr("requests.post", post)
    llm = OpenAICompatLLM(
        "https://example/v1/", "model", api_key="secret", temperature=0.3
    )

    assert llm.complete("system", "user", max_tokens=123) == "result"
    url, kwargs = calls[0]
    assert url == "https://example/v1/chat/completions"
    assert kwargs["headers"]["Authorization"] == "Bearer secret"
    assert kwargs["json"] == {
        "model": "model",
        "messages": [
            {"role": "system", "content": "system"},
            {"role": "user", "content": "user"},
        ],
        "max_tokens": 123,
        "temperature": 0.3,
    }

    calls.clear()
    OpenAICompatLLM("https://example/v1", "model").complete("s", "u")
    assert "Authorization" not in calls[0][1]["headers"]
    assert "max_tokens" not in calls[0][1]["json"]


def test_openai_compat_list_models(monkeypatch):
    monkeypatch.setattr(
        "requests.get",
        lambda *args, **kwargs: FakeResponse(
            {"data": [{"id": "model-a"}, {"id": "model-b"}]}
        ),
    )

    models = OpenAICompatLLM("https://example/v1", "model").list_models()
    assert [model.id for model in models] == ["model-a", "model-b"]


class FakeLLM:
    id = "fake"
    offline = True

    def __init__(self, result="cleaned", error=None):
        self.result = result
        self.error = error
        self.calls = []

    def complete(self, system, user, *, max_tokens=None):
        self.calls.append((system, user, max_tokens))
        if self.error:
            raise self.error
        return self.result

    def list_models(self):
        return []

    def healthcheck(self):
        return None


def test_cleaner_uses_provider_and_preserves_fallback():
    llm = FakeLLM()
    assert Cleaner(llm).clean("um hello") == "cleaned"
    assert llm.calls[0][1] == "Text:\num hello"
    assert isinstance(llm, LLMProvider)

    assert Cleaner(FakeLLM(error=RuntimeError("failed"))).clean("input") == "input"
    assert Cleaner(FakeLLM(result="")).clean("input") == "input"


def test_work_prompt_generator_without_provider_is_unavailable():
    generator = WorkPromptGenerator(None)

    assert generator.available is False
    assert generator.generate("notes") == ""


def test_transcriber_faster_whisper_provider(monkeypatch):
    class WhisperModel:
        def __init__(self, model, device, compute_type):
            assert (model, device, compute_type) == ("tiny", "cpu", "int8")

        def transcribe(self, audio, language, vad_filter):
            assert language == "en"
            assert vad_filter is True
            return (
                [SimpleNamespace(text=" first "), SimpleNamespace(text="second ")],
                None,
            )

    module = ModuleType("faster_whisper")
    module.WhisperModel = WhisperModel
    monkeypatch.setitem(sys.modules, "faster_whisper", module)

    transcriber = Transcriber(model_size="tiny", language="en", backend="faster-whisper")
    assert isinstance(transcriber.provider, FasterWhisperSTT)
    assert isinstance(transcriber.provider, STTProvider)
    assert transcriber.transcribe(np.zeros(10, dtype=np.float32)) == "first second"


def test_builtin_stt_providers_satisfy_protocol():
    assert isinstance(FasterWhisperSTT(), STTProvider)
    assert isinstance(MlxWhisperSTT(), STTProvider)
    assert isinstance(ParakeetSTT(), STTProvider)
