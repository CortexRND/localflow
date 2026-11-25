"""sherpa-onnx backends against a fake ``sherpa_onnx`` module; real-model tests skip unless
the extra and the verified models are present (LOCALFLOW_DIAR_TEST_WAVS adds a two-voice mix).
"""

import os
import socket
import subprocess
import sys
import types
import wave
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

import localflow.diarization as diar
from localflow.diarization import models as dm
from localflow.diarization.sherpa import (
    DEFAULT_THRESHOLD,
    SherpaDiarizer,
    SherpaEmbedder,
    load_sherpa_backends,
)
from localflow.diarization.types import (
    DiarizationUnavailable,
    Diarizer,
    SpeakerEmbedder,
    SpeakerTurn,
)

SEG_ID, EMB_ID = (model.id for model in dm.DIAR_MODELS)
LOADERS = {
    "diarizer": lambda: SherpaDiarizer().load(),
    "embedder": lambda: SherpaEmbedder().load(),
    "both": lambda: load_sherpa_backends(),
}


def _seg(start, end, speaker):
    return SimpleNamespace(start=start, end=end, speaker=speaker)


@pytest.fixture
def fake_module(monkeypatch):
    """A recording stand-in for ``sherpa_onnx``; returns its mutable state."""
    state = SimpleNamespace(
        valid=True, boom=None, engines=[], extractors=[], processed=[],
        accepted=[], ready=True, vector=[3.0, 4.0, 0.0],
        segments=[_seg(2.0, 3.0, 7), _seg(0.0, 1.0, 4), _seg(1.0, 2.0, 7),
                  _seg(3.0, 3.0, 1), _seg(3.5, 4.0, 9)],
    )

    class Config:
        def __init__(self, **kwargs):
            self.kw = kwargs

        def validate(self):
            return state.valid

    class Diarization:
        sample_rate = 16000

        def __init__(self, config):
            if state.boom:
                raise state.boom
            self.config = config
            state.engines.append(self)

        def process(self, samples):
            state.processed.append(samples)
            segments = sorted(state.segments, key=lambda s: s.start)
            return SimpleNamespace(sort_by_start_time=lambda: segments)

    class Stream:
        finished = False

        def accept_waveform(self, sample_rate, samples):
            state.accepted.append((sample_rate, samples))

        def input_finished(self):
            self.finished = True

    class Extractor:
        dim = 3

        def __init__(self, config):
            if state.boom:
                raise state.boom
            self.config = config
            state.extractors.append(self)

        def create_stream(self):
            return Stream()

        def is_ready(self, stream):
            return state.ready and stream.finished

        def compute(self, stream):
            return list(state.vector)

    module = types.ModuleType("sherpa_onnx")
    for name in ("OfflineSpeakerDiarizationConfig", "OfflineSpeakerSegmentationModelConfig",
                 "OfflineSpeakerSegmentationPyannoteModelConfig",
                 "SpeakerEmbeddingExtractorConfig", "FastClusteringConfig"):
        setattr(module, name, type(name, (Config,), {}))
    module.OfflineSpeakerDiarization = Diarization
    module.SpeakerEmbeddingExtractor = Extractor
    monkeypatch.setitem(sys.modules, "sherpa_onnx", module)
    return state


@pytest.fixture
def fake(fake_module, monkeypatch, tmp_path):
    paths = {SEG_ID: tmp_path / "seg.onnx", EMB_ID: tmp_path / "emb.onnx"}
    monkeypatch.setattr(dm, "verified_model_paths", lambda cache=None: paths)
    fake_module.paths = paths
    return fake_module


def test_import_is_lazy():
    code = (
        "import sys, localflow.diarization\n"
        "assert 'localflow.diarization.sherpa' not in sys.modules\n"
        "import localflow.diarization.sherpa\n"
        "assert 'sherpa_onnx' not in sys.modules\n"
        "assert 'sounddevice' not in sys.modules\n"
    )
    subprocess.run([sys.executable, "-c", code], check=True)


@pytest.mark.parametrize("load", LOADERS.values(), ids=LOADERS)
def test_extra_missing_is_reported_before_models(monkeypatch, load):
    monkeypatch.setitem(sys.modules, "sherpa_onnx", None)
    monkeypatch.setattr(dm, "verified_model_paths", lambda cache=None: pytest.fail("models checked"))
    with pytest.raises(DiarizationUnavailable, match=r"localflow\[diarization\]") as info:
        load()
    assert isinstance(info.value.__cause__, ImportError)


@pytest.mark.parametrize("cls", [SherpaDiarizer, SherpaEmbedder])
def test_models_missing(fake_module, tmp_path, cls):
    with pytest.raises(DiarizationUnavailable) as info:
        cls(cache=tmp_path).load()
    assert str(info.value) == dm.NOT_DOWNLOADED


def test_configs_use_verified_local_paths(fake):
    diarizer, embedder = load_sherpa_backends(num_threads=3)
    kw = fake.engines[0].config.kw
    seg, emb, clustering = kw["segmentation"].kw, kw["embedding"].kw, kw["clustering"].kw
    assert seg["pyannote"].kw == {"model": str(fake.paths[SEG_ID])}
    assert (seg["num_threads"], seg["provider"]) == (3, "cpu")
    assert emb == {"model": str(fake.paths[EMB_ID]), "num_threads": 3, "provider": "cpu"}
    assert clustering == {"num_clusters": -1, "threshold": DEFAULT_THRESHOLD}
    assert (kw["min_duration_on"], kw["min_duration_off"]) == (0.3, 0.5)
    assert fake.extractors[0].config.kw == emb
    assert embedder.dim == 3
    diarizer.load()
    embedder.load()
    assert len(fake.engines) == len(fake.extractors) == 1


def test_diarize_sorts_renumbers_and_drops_empty_turns(fake):
    diarizer = SherpaDiarizer()
    diarizer.load()
    turns = diarizer.diarize(np.zeros(16000 * 4, dtype=np.float64), 16000)
    assert turns == [SpeakerTurn(0.0, 1.0, 0), SpeakerTurn(1.0, 2.0, 1),
                     SpeakerTurn(2.0, 3.0, 1), SpeakerTurn(3.5, 4.0, 2)]
    assert all(type(t.start) is float and type(t.local_speaker) is int for t in turns)
    samples = fake.processed[0]
    assert samples.dtype == np.float32 and samples.flags.c_contiguous


def test_inputs_are_resampled(fake):
    diarizer, embedder = load_sherpa_backends()
    diarizer.diarize(np.zeros(48000, dtype=np.float32), 48000)
    embedder.embed(np.zeros(24000, dtype=np.float32), 48000)
    assert fake.processed[0].shape == (16000,) and fake.processed[0].dtype == np.float32
    rate, samples = fake.accepted[0]
    assert rate == 16000 and samples.shape == (8000,) and samples.dtype == np.float32


@pytest.mark.parametrize("size", [0, 100])
def test_empty_or_short_audio_skips_process(fake, size):
    diarizer = SherpaDiarizer()
    diarizer.load()
    assert diarizer.diarize(np.zeros(size, dtype=np.float32), 16000) == []
    assert fake.processed == []


def test_turns_are_clamped_to_the_input(fake):
    fake.segments = [_seg(1.5, 2.04, 3), _seg(-0.1, 0.5, 5), _seg(-1.0, -0.2, 8),
                     _seg(2.5, 3.0, 6), _seg(0.6, 1.0, 3)]
    diarizer = SherpaDiarizer()
    diarizer.load()
    turns = diarizer.diarize(np.zeros(16000 * 2, dtype=np.float32), 16000)
    assert turns == [SpeakerTurn(0.0, 0.5, 0), SpeakerTurn(0.6, 1.0, 1),
                     SpeakerTurn(1.5, 2.0, 1)]


def test_non_finite_samples_become_zero(fake):
    audio = np.ones(16000, dtype=np.float64)
    audio[[10, 20, 30]] = [np.nan, np.inf, -np.inf]
    diarizer, embedder = load_sherpa_backends()
    diarizer.diarize(audio, 16000)
    embedder.embed(audio, 16000)
    for samples in (fake.processed[0], fake.accepted[0][1]):
        assert samples.dtype == np.float32 and np.isfinite(samples).all()
        assert not samples[[10, 20, 30]].any() and samples[0] == 1.0


@pytest.mark.parametrize(
    "vector, ready, expected",
    [([3.0, 4.0, 0.0], True, [0.6, 0.8, 0.0]), ([0.0, 0.0, 0.0], True, [0.0] * 3),
     ([np.nan, 1.0, 0.0], True, [0.0] * 3), ([3.0, 4.0, 0.0], False, [0.0] * 3)],
)
def test_embed_is_normalised_or_zero(fake, vector, ready, expected):
    fake.vector, fake.ready = vector, ready
    embedder = SherpaEmbedder()
    embedder.load()
    out = embedder.embed(np.zeros(16000, dtype=np.float32), 16000)
    assert out.dtype == np.float32 and out.shape == (3,)
    np.testing.assert_allclose(out, expected, atol=1e-6)
    assert np.isfinite(out).all()


def test_embed_empty_audio_is_zero_vector(fake):
    embedder = SherpaEmbedder()
    embedder.load()
    assert not embedder.embed(np.zeros(0, dtype=np.float32), 16000).any()
    assert fake.accepted == []


@pytest.mark.parametrize("call", ["diarize", "embed"])
def test_bad_input_and_unloaded(fake, call):
    backend = SherpaDiarizer() if call == "diarize" else SherpaEmbedder()
    method = getattr(backend, call)
    with pytest.raises(ValueError, match="1-D"):
        method(np.zeros((2, 100), dtype=np.float32), 16000)
    with pytest.raises(ValueError, match="sample_rate"):
        method(np.zeros(100, dtype=np.float32), 0)
    with pytest.raises(DiarizationUnavailable, match="not loaded"):
        method(np.zeros(16000, dtype=np.float32), 16000)


@pytest.mark.parametrize("cls", [SherpaDiarizer, SherpaEmbedder])
def test_invalid_config_and_native_errors(fake, cls):
    fake.valid = False
    with pytest.raises(DiarizationUnavailable, match="invalid"):
        cls().load()
    fake.valid, fake.boom = True, RuntimeError("onnx says no")
    with pytest.raises(DiarizationUnavailable, match="Could not load") as info:
        cls().load()
    assert info.value.__cause__ is fake.boom


def test_load_works_with_sockets_blocked(fake, monkeypatch):
    def blocked(*args, **kwargs):
        raise AssertionError("network used during load")

    monkeypatch.setattr(socket, "socket", blocked)
    monkeypatch.setattr(socket, "create_connection", blocked)
    diarizer, embedder = load_sherpa_backends()
    assert diarizer.diarize(np.zeros(16000, dtype=np.float32), 16000)
    assert embedder.embed(np.ones(16000, dtype=np.float32), 16000).shape == (3,)


def test_protocols_ids_and_lazy_exports():
    assert isinstance(SherpaDiarizer(), Diarizer)
    assert isinstance(SherpaEmbedder(), SpeakerEmbedder)
    assert (SherpaDiarizer.id, SherpaEmbedder.id) == (SEG_ID, EMB_ID)
    assert SherpaEmbedder().dim == 0
    assert diar.SherpaDiarizer is SherpaDiarizer
    assert diar.SherpaEmbedder is SherpaEmbedder
    assert diar.load_sherpa_backends is load_sherpa_backends


# ------------------------------------------------------------ real models ---


def _require_real_backend() -> None:
    pytest.importorskip("sherpa_onnx")
    ready, reason = dm.models_ready()
    if not ready:
        pytest.skip(reason)


def _read_wav(path: str) -> np.ndarray:
    with wave.open(path, "rb") as handle:
        assert (handle.getnchannels(), handle.getsampwidth(), handle.getframerate()) == (1, 2, 16000)
        frames = handle.readframes(handle.getnframes())
    return np.frombuffer(frames, dtype="<i2").astype(np.float32) / 32768.0


def _cosine(a: np.ndarray, b: np.ndarray) -> float:
    return float(np.dot(a, b))


def test_real_two_voice_mix():
    _require_real_backend()
    wavs = [p for p in os.environ.get("LOCALFLOW_DIAR_TEST_WAVS", "").split(os.pathsep) if p]
    if len(wavs) < 2:
        pytest.skip("set LOCALFLOW_DIAR_TEST_WAVS to >= 2 16 kHz mono wavs of different speakers")
    voices = [_read_wav(p) for p in wavs]
    silence = np.zeros(8000, dtype=np.float32)
    mix = np.concatenate([part for voice in voices for part in (voice, silence)])
    diarizer, embedder = load_sherpa_backends()
    turns = diarizer.diarize(mix, 16000)
    assert len({t.local_speaker for t in turns}) >= 2
    halves = [[embedder.embed(half, 16000) for half in np.array_split(v, 2)] for v in voices[:2]]
    same = min(_cosine(*halves[0]), _cosine(*halves[1]))
    cross = max(_cosine(a, b) for a in halves[0] for b in halves[1])
    assert same > cross


def test_real_backend_coexists_with_onnxruntime():
    _require_real_backend()
    onnxruntime = pytest.importorskip("onnxruntime")
    faster_whisper = pytest.importorskip("faster_whisper")
    vad = sorted((Path(faster_whisper.__file__).parent / "assets").glob("*.onnx"))
    if not vad:
        pytest.skip("faster-whisper ships no bundled VAD model")
    session = onnxruntime.InferenceSession(str(vad[0]), providers=["CPUExecutionProvider"])
    diarizer, _embedder = load_sherpa_backends()
    noise = np.random.default_rng(0).normal(0, 0.1, 32000).astype(np.float32)
    assert isinstance(diarizer.diarize(noise, 16000), list)
    assert session.get_inputs()
