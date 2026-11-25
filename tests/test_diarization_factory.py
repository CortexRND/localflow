import subprocess
import sys
import threading
from types import ModuleType, SimpleNamespace

import pytest

from localflow import diarization
from localflow.config import Config
from localflow.diarization import factory, models, sherpa
from localflow.diarization.collector import HARD_MAX_SPEAKERS, DiarizationCollector
from localflow.diarization.factory import build_diarization
from localflow.diarization.mock import MockDiarizer, MockEmbedder
from localflow.diarization.types import DiarizationUnavailable


def _run(code):
    proc = subprocess.run(
        [sys.executable, "-W", "error", "-c", code],
        check=False,
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr


def _threads():
    return [t for t in threading.enumerate() if t.name.startswith("localflow-diarization")]


def test_flag_off_builds_nothing():
    assert build_diarization(Config()) == (None, "")
    assert build_diarization(Config(diarization_backend="mock")) == (None, "")


def test_flag_off_imports_nothing():
    _run(
        "import sys\n"
        "from localflow.config import Config\n"
        "import localflow.diarization.factory as f\n"
        "assert f.build_diarization(Config(diarization_backend='mock')) == (None, '')\n"
        "assert f.build_diarization(Config()) == (None, '')\n"
        "bad = {'localflow.diarization.collector', 'localflow.diarization.mock',\n"
        "       'localflow.diarization.align', 'localflow.diarization.sherpa',\n"
        "       'sherpa_onnx'} & set(sys.modules)\n"
        "assert not bad, bad\n"
    )


def test_mock_backend_builds_a_collector():
    before = _threads()
    collector, reason = build_diarization(Config(
        speaker_diarization=True, diarization_backend="mock", diarize_mic=True,
        diarization_max_speakers=3,
    ))
    assert reason == ""
    assert isinstance(collector, DiarizationCollector)
    assert isinstance(collector.diarizer, MockDiarizer)
    assert isinstance(collector.embedder, MockEmbedder)
    assert collector.diarize_mic is True and collector.max_speakers == 3
    assert collector.sample_rate == 16000
    assert _threads() == before
    auto, _ = build_diarization(Config(speaker_diarization=True, diarization_backend="mock"))
    assert auto.max_speakers == HARD_MAX_SPEAKERS and auto.diarize_mic is False


def test_sherpa_extra_missing_is_unavailable(monkeypatch):
    monkeypatch.setitem(sys.modules, "sherpa_onnx", None)
    collector, reason = build_diarization(Config(speaker_diarization=True))
    assert collector is None and "localflow[diarization]" in reason


def test_sherpa_models_missing_is_unavailable(monkeypatch):
    def missing(cache=None):
        raise models.DiarizationModelsUnavailable(models.NOT_DOWNLOADED)

    monkeypatch.setitem(sys.modules, "sherpa_onnx", ModuleType("sherpa_onnx"))
    monkeypatch.setattr(models, "verified_model_paths", missing)
    assert build_diarization(Config(speaker_diarization=True)) == (None, models.NOT_DOWNLOADED)


def test_sherpa_backend_builds_a_collector(monkeypatch):
    diarizer, embedder = MockDiarizer(), MockEmbedder()
    monkeypatch.setattr(sherpa, "load_sherpa_backends", lambda: (diarizer, embedder))
    collector, reason = build_diarization(Config(
        speaker_diarization=True, diarization_backend="sherpa-onnx", diarize_mic=True,
        diarization_max_speakers=4, sample_rate=48000,
    ))
    assert reason == ""
    assert collector.diarizer is diarizer and collector.embedder is embedder
    assert collector.diarize_mic is True and collector.max_speakers == 4
    assert collector.sample_rate == 48000


def test_sherpa_loader_failure_is_the_reason(monkeypatch):
    def boom():
        raise DiarizationUnavailable("x")

    monkeypatch.setattr(sherpa, "load_sherpa_backends", boom)
    assert build_diarization(Config(speaker_diarization=True)) == (None, "x")


def test_unknown_backend_is_named():
    config = SimpleNamespace(speaker_diarization=True, diarization_backend="nope",
                             sample_rate=16000, diarize_mic=False, diarization_max_speakers=0)
    collector, reason = build_diarization(config)
    assert collector is None and "'nope'" in reason


def test_never_raises(monkeypatch):
    def boom(self):
        raise DiarizationUnavailable("boom")

    monkeypatch.setattr(MockDiarizer, "load", boom)
    config = Config(speaker_diarization=True, diarization_backend="mock")
    assert build_diarization(config) == (None, "boom")

    monkeypatch.setattr(MockDiarizer, "load", lambda self: (_ for _ in ()).throw(RuntimeError()))
    assert build_diarization(config) == (None, "RuntimeError")


@pytest.mark.parametrize("value", [0, -1, 11, True, "3", None])
def test_out_of_range_max_speakers_means_auto(value):
    assert factory._max_speakers(value) is None


def test_lazy_exports_resolve():
    assert diarization.build_diarization is build_diarization
    assert diarization.DiarizationCollector is DiarizationCollector
    assert diarization.DiarizationResult.__name__ == "DiarizationResult"
    assert diarization.DIAR_FINALIZE_TIMEOUT_S == 20
    for name in ("build_diarization", "DiarizationCollector", "DiarizationResult",
                 "DIAR_FINALIZE_TIMEOUT_S"):
        assert name in diarization.__all__ and name in dir(diarization)


# ---------------------------------------------------- recognition (slice B9) ---

def test_build_diarization_never_attaches_a_registry():
    """The server attaches the registry only when both flags are on."""
    for recognition in (False, True):
        config = Config(speaker_diarization=True, diarization_backend="mock",
                        speaker_recognition=recognition)
        collector, _ = build_diarization(config)
        assert collector.registry is None
        collector.close()


def test_diarization_without_recognition_never_imports_the_registry():
    _run(
        "import sys\n"
        "from localflow.config import Config\n"
        "from localflow.diarization.factory import build_diarization\n"
        "c, _ = build_diarization(Config(speaker_diarization=True,"
        " diarization_backend='mock', speaker_recognition=True))\n"
        "c.close()\n"
        "assert 'localflow.diarization.registry' not in sys.modules\n"
        "assert 'sherpa_onnx' not in sys.modules\n"
    )


def test_build_embedder_mock_is_loaded():
    embedder = factory.build_embedder(Config(diarization_backend="mock"))
    assert isinstance(embedder, MockEmbedder) and embedder.id == "mock"


def test_build_embedder_sherpa_without_runtime_is_unavailable(monkeypatch):
    def missing(cache):
        raise DiarizationUnavailable("install localflow[diarization]")

    monkeypatch.setattr(sherpa, "_runtime", missing)
    with pytest.raises(DiarizationUnavailable, match="install"):
        factory.build_embedder(Config(diarization_backend="sherpa-onnx"))


def test_build_embedder_load_errors_are_unavailable_without_detail(monkeypatch):
    monkeypatch.setattr(MockEmbedder, "load",
                        lambda self: (_ for _ in ()).throw(OSError("/secret/path")))
    with pytest.raises(DiarizationUnavailable) as info:
        factory.build_embedder(Config(diarization_backend="mock"))
    assert "secret" not in str(info.value)
