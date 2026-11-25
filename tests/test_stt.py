import dataclasses
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

from localflow.diarization.types import TimedText
from localflow.providers.base import STTProvider
from localflow.providers.stt_mlx_whisper import MlxWhisperSTT
from localflow.stt import PARAKEET_REPO, Transcriber


@pytest.fixture
def parakeet(monkeypatch):
    owner_thread = threading.get_ident()
    parameters = {"embedding": object()}
    state = SimpleNamespace(evaluated=False)

    def evaluate(values):
        assert values is parameters
        assert threading.get_ident() == owner_thread
        state.evaluated = True

    def generate(mel):
        if np.any(mel):
            if not state.evaluated:
                if threading.get_ident() != owner_thread:
                    raise RuntimeError("There is no Stream(cpu, 1) in current thread.")
                state.evaluated = True
            return [SimpleNamespace(text="  The meeting starts tomorrow morning.  ")]
        return [SimpleNamespace(text="")]

    model = SimpleNamespace(
        preprocessor_config=SimpleNamespace(n_fft=512),
        parameters=Mock(return_value=parameters),
        generate=Mock(side_effect=generate),
    )
    core = ModuleType("mlx.core")
    core.array = Mock(side_effect=np.asarray)
    core.eval = Mock(side_effect=evaluate)
    mlx = ModuleType("mlx")
    mlx.core = core
    package = ModuleType("parakeet_mlx")
    package.from_pretrained = Mock(return_value=model)
    audio = ModuleType("parakeet_mlx.audio")
    audio.get_logmel = Mock(side_effect=lambda x, cfg: x)
    package.audio = audio
    for name, module in (
        ("mlx", mlx),
        ("mlx.core", core),
        ("parakeet_mlx", package),
        ("parakeet_mlx.audio", audio),
    ):
        monkeypatch.setitem(sys.modules, name, module)
    return SimpleNamespace(
        model=model,
        core=core,
        load=package.from_pretrained,
        logmel=audio.get_logmel,
        parameters=parameters,
    )


def test_parakeet_eagerly_loads_evaluates_and_warms_up(parakeet):
    transcriber = Transcriber(backend="parakeet", model_size="tiny", language="fr")

    assert transcriber.backend == "parakeet"
    parakeet.load.assert_called_once_with(PARAKEET_REPO)
    parakeet.core.eval.assert_called_once_with(parakeet.parameters)
    parakeet.logmel.assert_called_once()
    audio, cfg = parakeet.logmel.call_args.args
    np.testing.assert_array_equal(audio, np.zeros(16000, dtype=np.float32))
    assert cfg is parakeet.model.preprocessor_config
    parakeet.model.generate.assert_called_once()


def test_parakeet_transcribes_on_worker_after_main_thread_warmup(parakeet):
    transcriber = Transcriber(backend="parakeet")
    audio = np.full(16000, 0.1, dtype=np.float32)

    with ThreadPoolExecutor(max_workers=1) as worker:
        text = worker.submit(transcriber.transcribe, audio).result(timeout=5)

    assert text == "The meeting starts tomorrow morning."


@pytest.mark.parametrize("samples", [0, 1, 100, 511])
def test_parakeet_short_inputs_do_not_run_model(parakeet, samples):
    transcriber = Transcriber(backend="parakeet")
    parakeet.logmel.reset_mock()
    parakeet.model.generate.reset_mock()

    assert transcriber.transcribe(np.zeros(samples, dtype=np.float32)) == ""
    parakeet.logmel.assert_not_called()
    parakeet.model.generate.assert_not_called()


def test_parakeet_accepts_one_full_fft_frame(parakeet):
    transcriber = Transcriber(backend="parakeet")
    parakeet.logmel.reset_mock()
    parakeet.model.generate.reset_mock()

    assert transcriber.transcribe(np.zeros(512, dtype=np.float32)) == ""
    parakeet.logmel.assert_called_once()
    parakeet.model.generate.assert_called_once()


def test_parakeet_uses_contiguous_float32_audio_in_memory(parakeet):
    transcriber = Transcriber(backend="parakeet")
    audio = np.linspace(-0.5, 0.5, 2048)[::2]

    assert transcriber.transcribe(audio) == "The meeting starts tomorrow morning."
    x, cfg = parakeet.logmel.call_args.args
    assert x.dtype == np.float32
    assert x.flags.c_contiguous
    assert cfg is parakeet.model.preprocessor_config
    np.testing.assert_array_equal(x, audio.astype(np.float32))


def test_parakeet_parameter_evaluation_failure_is_not_swallowed(parakeet):
    parakeet.core.eval.side_effect = RuntimeError("cannot evaluate weights")

    with pytest.raises(RuntimeError, match="cannot evaluate weights"):
        Transcriber(backend="parakeet")

    parakeet.model.generate.assert_not_called()


def test_unknown_backend_is_rejected():
    with pytest.raises(ValueError, match="unknown stt backend"):
        Transcriber(backend="not-a-backend")


# --- timed STT -------------------------------------------------------------


def _with_sentences(parakeet, text, sentences):
    original = parakeet.model.generate.side_effect

    def generate(mel):
        original(mel)  # keep the fixture's evaluation/thread checks
        return [SimpleNamespace(text=text, sentences=sentences)]

    parakeet.model.generate.side_effect = generate


SENTENCES = [
    SimpleNamespace(text=" Hello there.", start=0.24, end=1.2),
    SimpleNamespace(text="   ", start=1.2, end=1.6),
    SimpleNamespace(text=" How are you?", start=1.6, end=2.4),
]
TIMED = [TimedText(0.24, 1.2, "Hello there."), TimedText(1.6, 2.4, "How are you?")]


def test_parakeet_timed_sentences_on_worker_after_warmup(parakeet):
    transcriber = Transcriber(backend="parakeet")
    _with_sentences(parakeet, " Hello there. How are you?", SENTENCES)
    audio = np.full(16000, 0.1, dtype=np.float32)

    with ThreadPoolExecutor(max_workers=1) as worker:
        timed = worker.submit(transcriber.transcribe_timed, audio).result(timeout=5)

    assert timed == TIMED
    assert transcriber.transcribe_timed(audio, prompt="ignored") == TIMED
    assert transcriber.transcribe(audio) == "Hello there. How are you?"


def test_parakeet_timed_without_sentences_is_empty(parakeet):
    transcriber = Transcriber(backend="parakeet")
    _with_sentences(parakeet, "", [])

    assert transcriber.transcribe_timed(np.full(16000, 0.1, np.float32)) == []


@pytest.mark.parametrize("samples", [0, 1, 100, 511])
def test_parakeet_timed_short_inputs_do_not_run_model(parakeet, samples):
    transcriber = Transcriber(backend="parakeet")
    parakeet.logmel.reset_mock()
    parakeet.model.generate.reset_mock()

    assert transcriber.transcribe_timed(np.zeros(samples, dtype=np.float32)) == []
    parakeet.logmel.assert_not_called()
    parakeet.model.generate.assert_not_called()


WHISPER_SEGMENTS = [
    SimpleNamespace(start=0.0, end=1.5, text=" first "),
    SimpleNamespace(start=1.5, end=2.0, text="  "),
    SimpleNamespace(start=2, end=3, text="second "),
]


class _PlainWhisper:  # **options has no "hotwords" parameter, so none is sent
    def __init__(self, *_args, **_kwargs):
        self.calls = []

    def transcribe(self, audio, **options):
        self.calls.append(options)
        return iter(WHISPER_SEGMENTS), None


class _HotwordsWhisper(_PlainWhisper):
    def transcribe(self, audio, hotwords=None, **options):
        return super().transcribe(audio, hotwords=hotwords, **options)


PROMPTED = {"language": "en", "vad_filter": True, "initial_prompt": "Ada"}


@pytest.mark.parametrize(
    ("model_class", "options"),
    [(_HotwordsWhisper, {**PROMPTED, "hotwords": "Ada"}), (_PlainWhisper, PROMPTED)],
)
def test_faster_whisper_timed_segments_share_options(
    monkeypatch, model_class, options
):
    module = ModuleType("faster_whisper")
    module.WhisperModel = model_class
    monkeypatch.setitem(sys.modules, "faster_whisper", module)
    transcriber = Transcriber(language="en", backend="faster-whisper")
    audio = np.zeros(10, dtype=np.float32)

    timed = transcriber.transcribe_timed(audio, prompt="Ada")
    assert timed == [TimedText(0.0, 1.5, "first"), TimedText(2.0, 3.0, "second")]
    assert all(type(t.start) is float and type(t.end) is float for t in timed)
    # Legacy join is unchanged, including its double space for a blank segment.
    assert transcriber.transcribe(audio, prompt="Ada") == "first  second"
    assert transcriber.provider.model.calls == [options, options]


def test_mlx_whisper_timed_segments():
    calls = []
    segments = [
        {"start": 0.0, "end": 1.0, "text": " a"},
        {"start": 1.0, "end": 2.5, "text": " b "},
        {"start": 2.5, "end": 3.0, "text": "  "},
    ]
    provider = MlxWhisperSTT()
    provider._mlx_repo, provider.language = "repo", "en"
    provider._mlx = SimpleNamespace(
        transcribe=lambda audio, **options: (
            calls.append(options) or {"text": " a b ", "segments": segments}
        )
    )
    audio = np.zeros(10, dtype=np.float32)

    expected = [TimedText(0.0, 1.0, "a"), TimedText(1.0, 2.5, "b")]
    assert provider.transcribe_timed(audio) == expected
    assert provider.transcribe_timed(audio, prompt="p") == expected
    assert calls == [
        {"path_or_hf_repo": "repo", "language": "en"},
        {"path_or_hf_repo": "repo", "language": "en", "initial_prompt": "p"},
    ]

    provider._mlx = SimpleNamespace(transcribe=lambda audio, **_: {"text": ""})
    with pytest.raises(KeyError):
        provider.transcribe_timed(audio)


def _transcriber(monkeypatch, provider_class):
    entry_point = SimpleNamespace(name="fake", load=lambda: provider_class)
    monkeypatch.setattr(
        "importlib.metadata.entry_points",
        lambda *, group: [entry_point] if group == "localflow.stt_providers" else [],
    )
    return Transcriber(backend="fake")


@pytest.mark.parametrize("text", ["hello", "", "  "])
def test_timed_fallback_spans_whole_audio(monkeypatch, text):
    class Provider:
        id = "fake"
        offline = True

        def load(self, model, language, device="auto"):
            self.prompts = []

        def transcribe(self, audio, *, prompt=None):
            self.prompts.append(prompt)
            return text

        def list_models(self):
            return []

    transcriber = _transcriber(monkeypatch, Provider)
    audio = np.zeros(32000, dtype=np.float32)

    # The protocol is unchanged, so providers without the method still conform.
    assert isinstance(transcriber.provider, STTProvider)
    assert "transcribe_timed" not in STTProvider.__dict__
    assert transcriber.transcribe_timed(audio, prompt="p") == [
        TimedText(0.0, 2.0, text)
    ]
    assert transcriber.provider.prompts == ["p"]


def test_timed_fallback_keeps_legacy_prompt_omission(monkeypatch):
    class Legacy:
        def load(self, model, language, device="auto"):
            self.audio = []

        def transcribe(self, audio):
            self.audio.append(audio)
            return "legacy"

    transcriber = _transcriber(monkeypatch, Legacy)
    audio = np.zeros(8000, dtype=np.float32)

    assert transcriber.transcribe_timed(audio) == [TimedText(0.0, 0.5, "legacy")]
    assert transcriber.provider.audio == [audio]
    with pytest.raises(TypeError):
        transcriber.transcribe_timed(audio, prompt="x")


def test_timed_delegates_to_provider_and_propagates_errors(monkeypatch):
    calls = []
    result = [TimedText(0.0, 1.0, "x")]

    class Provider:
        error = None

        def load(self, model, language, device="auto"):
            pass

        def transcribe(self, audio, *, prompt=None):
            raise AssertionError("fallback must not run")

        def transcribe_timed(self, audio, **kwargs):
            calls.append(kwargs)
            if self.error is not None:
                raise self.error
            return result

    transcriber = _transcriber(monkeypatch, Provider)
    audio = np.zeros(10, dtype=np.float32)

    assert transcriber.transcribe_timed(audio) is result
    assert transcriber.transcribe_timed(audio, prompt="p") is result
    assert calls == [{}, {"prompt": "p"}]
    for error in (RuntimeError("boom"), AttributeError("inner")):
        transcriber.provider.error = error
        with pytest.raises(type(error), match=str(error)):
            transcriber.transcribe_timed(audio)


def test_timed_text_is_frozen():
    with pytest.raises(dataclasses.FrozenInstanceError):
        TimedText(0.0, 1.0, "x").text = "y"
