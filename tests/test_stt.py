import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

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
