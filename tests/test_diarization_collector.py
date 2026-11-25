import gc
import subprocess
import sys
import threading
import time
import weakref
from dataclasses import dataclass

import numpy as np
import pytest

from localflow.diarization import collector as collector_module
from localflow.diarization.collector import (
    HARD_MAX_SPEAKERS,
    QUEUE_MAX_CHUNKS,
    DiarizationCollector,
)
from localflow.diarization.mock import MockDiarizer, MockEmbedder, tone_index
from localflow.diarization.types import SpeakerTurn, TimedText

RATE = 16000


def tone(i, seconds):
    return np.full(int(RATE * seconds), 0.1 + i * 0.001, dtype=np.float32)


def silence(seconds):
    return np.zeros(int(RATE * seconds), dtype=np.float32)


class ToneTimedTranscriber:
    """One TimedText per tone run, text 'line <tone index>'."""

    def transcribe_timed(self, audio):
        return [
            TimedText(t.start, t.end,
                      f"line {tone_index(audio[int(t.start * RATE):int(t.end * RATE)])}")
            for t in MockDiarizer().diarize(audio, RATE)
        ]

    def transcribe(self, audio):
        return " ".join(t.text for t in self.transcribe_timed(audio))


@dataclass
class Seg:
    offset_seconds: float
    text: str
    source: str = "me"
    end_seconds: float | None = None
    speaker_id: str | None = None
    speaker_label: str | None = None


STT = ToneTimedTranscriber()


def _chunk(collector, source, offset, audio, *, wait=False):
    """Feed one chunk like MeetingSession does; return its live segment."""
    timed = STT.transcribe_timed(audio)
    fed = collector.feed(source, offset, audio, timed)
    if wait:
        _wait_for(collector, lambda s: s["processed"] + s["failed"] + s["skipped"]
                  >= s["offered"] - s["dropped"])
    return fed, Seg(offset, " ".join(t.text for t in timed), source)


def _wait_for(collector, predicate, deadline=5.0):
    end = time.monotonic() + deadline
    while time.monotonic() < end:
        if predicate(collector.stats()):
            return
        time.sleep(0.005)
    raise AssertionError(f"timed out waiting; stats={collector.stats()}")


def _rows(segments):
    return [(s.offset_seconds, s.end_seconds, s.source, s.speaker_id, s.speaker_label, s.text)
            for s in segments]


def _threads(prefix="localflow-diarization"):
    return [t for t in threading.enumerate() if t.name.startswith(prefix)]


def _wait_no_threads(deadline=5.0):
    end = time.monotonic() + deadline
    while _threads() and time.monotonic() < end:
        time.sleep(0.01)
    assert _threads() == []


_MADE: list = []


def _make(diarizer, embedder, **kwargs):
    c = DiarizationCollector(diarizer, embedder, sample_rate=RATE, **kwargs)
    _MADE.append(c)
    return c


def _new(**kwargs):
    return _make(MockDiarizer(), MockEmbedder(), **kwargs)


@pytest.fixture(autouse=True)
def _no_leftover_threads():
    """Every collector is closed and its worker gone before the next test."""
    yield
    while _MADE:
        c = _MADE.pop()
        c.close()
        if c._worker is not None:
            c._worker.join(2.0)
            assert not c._worker.is_alive()
    _wait_no_threads()


class BlockingDiarizer(MockDiarizer):
    """Blocks every diarize call until ``release`` is set; ``entered`` marks the first."""

    def __init__(self):
        super().__init__()
        self.entered = threading.Event()
        self.release = threading.Event()

    def diarize(self, audio, sample_rate):
        self.entered.set()
        self.release.wait(10)
        return super().diarize(audio, sample_rate)


class FlakyDiarizer(MockDiarizer):
    def __init__(self, fail_calls):
        super().__init__()
        self.fail_calls = set(fail_calls)
        self.calls = 0

    def diarize(self, audio, sample_rate):
        call, self.calls = self.calls, self.calls + 1
        if call in self.fail_calls:
            raise RuntimeError("backend broke")
        return super().diarize(audio, sample_rate)


@pytest.fixture
def blocking():
    diarizer = BlockingDiarizer()
    made = []

    def make(**kwargs):
        c = _make(diarizer, MockEmbedder(), **kwargs)
        made.append(c)
        return c

    try:
        yield diarizer, make
    finally:
        diarizer.release.set()
        for c in made:
            c.close()
        _wait_no_threads()


def _two_voice():
    return np.concatenate([tone(0, 2), tone(1, 2)])


# ----------------------------------------------------------------------------- tests ---

def test_three_speakers_two_chunks_cluster_globally():
    c = _new()
    me = Seg(0.0, "hello", "me")
    _, a = _chunk(c, "others", 0.0, np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4), tone(0, 2)]))
    _, b = _chunk(c, "others", 14.0, np.concatenate([tone(2, 3), tone(1, 3)]))
    segments = [me, a, b]
    result = c.finalize(segments)
    assert result.status == "ok"
    assert result.segments[0] is me
    assert _rows(result.segments) == [
        (0.0, None, "me", None, None, "hello"),
        (0.0, 4.0, "others", "S1", "Speaker 1", "line 0"),
        (4.0, 8.0, "others", "S2", "Speaker 2", "line 1"),
        (8.0, 12.0, "others", "S3", "Speaker 3", "line 2"),
        (12.0, 14.0, "others", "S1", "Speaker 1", "line 0"),
        (14.0, 17.0, "others", "S3", "Speaker 3", "line 2"),
        (17.0, 20.0, "others", "S2", "Speaker 2", "line 1"),
    ]
    assert [(s.id, s.label, s.source, s.name, s.voiceprint_id) for s in result.speakers] == [
        ("S1", "Speaker 1", "others", None, None),
        ("S2", "Speaker 2", "others", None, None),
        ("S3", "Speaker 3", "others", None, None),
    ]
    assert [s.seconds for s in result.speakers] == pytest.approx([6.0, 7.0, 7.0])
    assert segments == [me, a, b] and a.speaker_id is None and b.text == "line 2 line 1"


def test_single_remote_speaker_keeps_channel_labels():
    c = _new()
    _, seg = _chunk(c, "others", 0.0, tone(0, 4))
    segments = [Seg(0.0, "hi"), seg]
    result = c.finalize(segments)
    assert result.status == "ok" and result.speakers == []
    assert result.segments == segments and result.segments is not segments
    assert all(x is y for x, y in zip(result.segments, segments))


def test_mic_is_not_diarized_by_default():
    c = _new()
    fed, _ = _chunk(c, "me", 0.0, _two_voice())
    assert fed is False
    assert c.stats()["offered"] == 0
    assert not any(t.is_alive() for t in _threads())
    assert c.feed("elsewhere", 0.0, _two_voice(), STT.transcribe_timed(_two_voice())) is False
    assert c.stats()["offered"] == 0


def test_diarize_mic_labels_two_mic_voices():
    c = _new(diarize_mic=True)
    _, seg = _chunk(c, "me", 0.0, np.concatenate([tone(3, 3), tone(4, 3)]))
    result = c.finalize([seg])
    assert result.status == "ok"
    assert _rows(result.segments) == [
        (0.0, 3.0, "me", "S1", "Speaker 1", "line 3"),
        (3.0, 6.0, "me", "S2", "Speaker 2", "line 4"),
    ]
    assert [s.source for s in result.speakers] == ["me", "me"]


def test_one_numbering_sequence_across_sources():
    c = _new(diarize_mic=True)
    _, mic = _chunk(c, "me", 0.0, np.concatenate([tone(3, 3), tone(4, 3)]))
    _, others = _chunk(c, "others", 0.0, np.concatenate([tone(0, 2), tone(1, 2)]))
    result = c.finalize([mic, others])
    assert [(s.offset_seconds, s.source, s.speaker_id, s.text) for s in result.segments] == [
        (0.0, "me", "S1", "line 3"),
        (0.0, "others", "S2", "line 0"),
        (2.0, "others", "S3", "line 1"),
        (3.0, "me", "S4", "line 4"),
    ]
    assert [(s.id, s.source) for s in result.speakers] == [
        ("S1", "me"), ("S2", "others"), ("S3", "others"), ("S4", "me"),
    ]


def test_short_turns_are_not_embedded_and_inherit_or_drop_their_label():
    class SpyEmbedder(MockEmbedder):
        def __init__(self):
            super().__init__()
            self.lengths = []

        def embed(self, audio, sample_rate):
            self.lengths.append(len(audio))
            return super().embed(audio, sample_rate)

    spy = SpyEmbedder()
    c = _make(MockDiarizer(), spy)
    audio = np.concatenate([tone(0, 2), tone(1, 2), tone(0, 0.5), silence(1.0), tone(2, 0.5)])
    _, seg = _chunk(c, "others", 0.0, audio)
    result = c.finalize([seg])
    assert spy.lengths == [2 * RATE, 2 * RATE]
    assert [(s.offset_seconds, s.speaker_id, s.text) for s in result.segments] == [
        (0.0, "S1", "line 0"), (2.0, "S2", "line 1"), (4.0, "S1", "line 0"), (5.5, None, "line 2"),
    ]
    assert result.segments[3].speaker_label is None and result.segments[3].source == "others"
    assert [s.seconds for s in result.speakers] == pytest.approx([2.5, 2.0])


@pytest.mark.parametrize("timed", [[], [TimedText(0, 1, "  ")], [TimedText(0, 1, ""),
                                                                   TimedText(1, 2, "\n")]])
def test_blank_text_is_not_fed(timed):
    c = _new()
    assert c.feed("others", 0.0, _two_voice(), timed) is False
    assert c.stats()["offered"] == 0


def _fill(make, diarizer, total):
    """1 chunk taken by the blocked worker, QUEUE_MAX_CHUNKS queued, the rest dropped."""
    c = make()
    segments, fed = [], []
    for i in range(total):
        ok, seg = _chunk(c, "others", 4.0 * i, _two_voice())
        if i == 0:
            assert diarizer.entered.wait(5)
        segments.append(seg)
        fed.append(ok)
    return c, segments, fed


def test_full_queue_drops_without_blocking(blocking):
    diarizer, make = blocking
    c, segments, fed = _fill(make, diarizer, 1 + QUEUE_MAX_CHUNKS + 1)
    assert fed == [True] * (1 + QUEUE_MAX_CHUNKS) + [False]
    assert c.stats()["dropped"] == 1
    diarizer.release.set()
    result = c.finalize(segments)
    assert result.status == "ok"
    assert result.segments[-1] is segments[-1]
    assert sum(s.speaker_id is not None for s in result.segments) == 2 * (1 + QUEUE_MAX_CHUNKS)


def test_more_than_half_lost_is_channel(blocking):
    diarizer, make = blocking
    c, segments, _ = _fill(make, diarizer, 11)
    assert (c.stats()["offered"], c.stats()["dropped"]) == (11, 6)
    diarizer.release.set()
    original = list(segments)
    result = c.finalize(segments)
    assert result.status == "channel" and result.speakers == []
    assert result.segments == original and result.segments is not segments


def test_exactly_half_lost_is_not_channel(blocking):
    diarizer, make = blocking
    c, segments, _ = _fill(make, diarizer, 10)
    assert (c.stats()["offered"], c.stats()["dropped"]) == (10, 5)
    diarizer.release.set()
    assert c.finalize(segments).status == "ok"


def test_one_backend_failure_keeps_that_chunk():
    c = _make(FlakyDiarizer({1}), MockEmbedder())
    segments = [_chunk(c, "others", 4.0 * i, _two_voice(), wait=True)[1] for i in range(4)]
    assert c.stats()["failed"] == 1 and c.disabled is False
    result = c.finalize(segments)
    assert result.status == "ok"
    assert segments[1] in result.segments
    assert any(s is segments[1] for s in result.segments)
    assert sum(s.speaker_id is not None for s in result.segments) == 6


def test_three_consecutive_failures_disable():
    c = _make(FlakyDiarizer({0, 1, 2}), MockEmbedder())
    segments = [_chunk(c, "others", 4.0 * i, _two_voice(), wait=True)[1] for i in range(3)]
    assert c.disabled is True and c.error
    fed, seg = _chunk(c, "others", 12.0, _two_voice())
    assert fed is False
    assert c.stats()["skipped"] == 1
    assert c.finalize([*segments, seg]).status == "channel"


def test_failure_counter_resets_after_success():
    c = _make(FlakyDiarizer({0, 1, 3, 4}), MockEmbedder())
    for i in range(5):
        _chunk(c, "others", 4.0 * i, _two_voice(), wait=True)
    assert c.stats()["failed"] == 4 and c.disabled is False and c.error == ""


def test_invalid_embeddings_are_not_failures():
    class BadEmbedder(MockEmbedder):
        def __init__(self):
            super().__init__()
            self.bad = [np.full(64, np.nan, dtype=np.float32), np.zeros(64, dtype=np.float32),
                        np.ones((2, 64), dtype=np.float32)]

        def embed(self, audio, sample_rate):
            return self.bad.pop(0) if self.bad else super().embed(audio, sample_rate)

    c = _make(MockDiarizer(), BadEmbedder())
    audio = np.concatenate([tone(0, 2), tone(1, 2), tone(2, 2), tone(3, 2), tone(4, 2)])
    _chunk(c, "others", 0.0, audio, wait=True)
    stats = c.stats()
    assert (stats["failed"], stats["processed"]) == (0, 1) and c.disabled is False
    assert sorted(c._records[0].vectors) == [3, 4]
    for vec in c._records[0].vectors.values():
        assert vec.dtype == np.float32 and vec.ndim == 1
        assert float(np.linalg.norm(vec)) == pytest.approx(1.0)


def test_finalize_times_out_with_original_segments(blocking, monkeypatch):
    diarizer, make = blocking
    c = make()
    _, seg = _chunk(c, "others", 0.0, _two_voice())
    assert diarizer.entered.wait(5)
    segments = [Seg(0.0, "hi"), seg]
    before = list(segments)
    t0 = time.monotonic()
    result = c.finalize(segments, timeout=0.2)
    assert time.monotonic() - t0 < 2
    assert result.status == "timeout" and result.speakers == []
    assert result.segments == before and all(x is y for x, y in zip(result.segments, before))
    assert segments == before and result.segments is not segments

    monkeypatch.setattr(collector_module, "DIAR_FINALIZE_TIMEOUT_S", 0.2)
    c2 = make()
    _chunk(c2, "others", 0.0, _two_voice())
    t0 = time.monotonic()
    assert c2.finalize(segments).status == "timeout"
    assert time.monotonic() - t0 < 2


def test_finalize_error_is_failed(monkeypatch):
    def boom(*args, **kwargs):
        raise RuntimeError("cluster broke")

    monkeypatch.setattr(collector_module, "cluster", boom)
    c = _new()
    _, seg = _chunk(c, "others", 0.0, _two_voice())
    result = c.finalize([seg])
    assert result.status == "failed" and result.speakers == []
    assert result.segments[0] is seg and len(result.segments) == 1


def test_finalize_is_idempotent_and_close_is_safe():
    c = _new()
    _, seg = _chunk(c, "others", 0.0, _two_voice())
    first = c.finalize([seg])
    assert c.finalize([seg]) is first
    assert _chunk(c, "others", 4.0, _two_voice())[0] is False
    c.close()
    c.close()
    assert c.finalize([seg]) is first

    closed = _new()
    _, seg = _chunk(closed, "others", 0.0, _two_voice())
    closed.close()
    closed.close()
    assert closed.feed("others", 4.0, _two_voice(), STT.transcribe_timed(_two_voice())) is False
    result = closed.finalize([seg])
    assert result.status == "failed" and result.segments == [seg]
    _wait_no_threads()


def test_unembedded_interjection_is_not_credited_to_a_neighbour():
    class ScriptedDiarizer(MockDiarizer):
        def diarize(self, audio, sample_rate):
            return [SpeakerTurn(0.0, 2.0, 0), SpeakerTurn(2.2, 2.7, 1), SpeakerTurn(3.0, 5.0, 2)]

    c = _make(ScriptedDiarizer(), MockEmbedder())
    audio = np.concatenate([tone(0, 2), silence(1.0), tone(1, 2)])
    timed = [TimedText(0.0, 2.0, "first"), TimedText(2.2, 2.7, "yeah"),
             TimedText(3.0, 5.0, "second")]
    assert c.feed("others", 0.0, audio, timed) is True
    result = c.finalize([Seg(0.0, "first yeah second", "others")])
    assert result.status == "ok"
    assert [(s.offset_seconds, s.speaker_id, s.text) for s in result.segments] == [
        (0.0, "S1", "first"), (2.2, None, "yeah"), (3.0, "S2", "second"),
    ]
    assert [s.seconds for s in result.speakers] == pytest.approx([2.0, 2.0])


@pytest.mark.parametrize("vec", [[[1.0, 2.0], [3.0]], ["a", "b"], object()])
def test_unusable_embedder_output_is_no_embedding(vec):
    assert collector_module._unit_vector(vec) is None


def test_finalize_that_cannot_start_closes_and_stays_idempotent(monkeypatch):
    c = _new()
    _, seg = _chunk(c, "others", 0.0, _two_voice(), wait=True)
    worker = c._worker

    class NoThread:
        def __init__(self, *args, **kwargs):
            pass

        def start(self):
            raise RuntimeError("can't start new thread")

    monkeypatch.setattr(collector_module.threading, "Thread", NoThread)
    result = c.finalize([seg])
    monkeypatch.undo()
    assert result.status == "failed" and result.segments == [seg]
    assert c.finalize([seg]) is result
    worker.join(2.0)
    assert not worker.is_alive()


def test_close_wakes_an_idle_worker_at_once(monkeypatch):
    # With a 30 s poll the worker could only exit this fast via the _STOP wake-up.
    monkeypatch.setattr(collector_module, "_POLL_S", 30.0)
    c = _new()
    assert _chunk(c, "others", 0.0, _two_voice(), wait=True)[0] is True
    worker = c._worker
    t0 = time.monotonic()
    c.close()
    worker.join(5)
    assert not worker.is_alive() and time.monotonic() - t0 < 5


def test_constructor_starts_nothing_and_clamps_speakers():
    assert _new().max_speakers == HARD_MAX_SPEAKERS
    assert _new(max_speakers=0).max_speakers == 1
    assert _new(max_speakers=99).max_speakers == HARD_MAX_SPEAKERS
    assert _new(max_speakers=3).max_speakers == 3
    assert not any(t.is_alive() for t in _threads())


def test_no_audio_is_retained():
    class Tracked(np.ndarray):
        pass

    c = _new()
    audio = _two_voice().view(Tracked)
    ref = weakref.ref(audio)
    timed = STT.transcribe_timed(np.asarray(audio))
    assert c.feed("others", 0.0, audio, timed) is True
    _wait_for(c, lambda s: s["processed"] == 1)
    del audio
    gc.collect()
    assert ref() is None
    assert c.finalize([Seg(0.0, "line 0 line 1", "others")]).status == "ok"


def _run(code):
    proc = subprocess.run(
        [sys.executable, "-W", "error", "-c", code],
        check=False,
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr


def test_collector_import_is_isolated():
    _run(
        "import sys\n"
        "import localflow.diarization.collector\n"
        "bad = {'localflow.meetings', 'localflow.server', 'sounddevice', 'sherpa_onnx'}\n"
        "assert not bad & set(sys.modules), bad & set(sys.modules)\n"
    )


# ------------------------------------------------------ recognition (slice B9) ---

class FakeVoiceprint:
    def __init__(self, vid, name):
        self.id, self.name = vid, name


class FakeRegistry:
    """Names tone 0 "Dana"; records every query."""

    def __init__(self, known=None, error=None, delay=0.0):
        self.known = {0: FakeVoiceprint("a" * 32, "Dana")} if known is None else known
        self.error, self.delay = error, delay
        self.calls = []

    def match(self, vector, model_id):
        self.calls.append((np.asarray(vector).copy(), model_id))
        if self.delay:
            time.sleep(self.delay)
        if self.error is not None:
            raise self.error
        vp = self.known.get(int(np.argmax(vector)))
        return None if vp is None else (vp, 1.0)


def _three_voice_meeting(c):
    _, a = _chunk(c, "others", 0.0, np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    return c.finalize([Seg(0.0, "hello", "me"), a])


def test_recognised_speaker_is_named_on_speaker_and_segments():
    registry = FakeRegistry()
    result = _three_voice_meeting(_new(registry=registry))
    assert result.status == "ok"
    assert [(s.id, s.label, s.name, s.voiceprint_id) for s in result.speakers] == [
        ("S1", "Dana", "Dana", "a" * 32), ("S2", "Speaker 2", None, None),
        ("S3", "Speaker 3", None, None)]
    assert [(s.speaker_id, s.speaker_label) for s in result.segments] == [
        (None, None), ("S1", "Dana"), ("S2", "Speaker 2"), ("S3", "Speaker 3")]
    assert [m for _, m in registry.calls] == ["mock"] * 3
    assert sorted(result.centroids) == ["S1", "S2", "S3"]
    for sid, idx in (("S1", 0), ("S2", 1), ("S3", 2)):
        vec = result.centroids[sid]
        assert vec.dtype == np.float32 and int(np.argmax(vec)) == idx
        assert float(np.linalg.norm(vec)) == pytest.approx(1.0)


def test_one_voiceprint_may_name_several_speakers():
    vp = FakeVoiceprint("a" * 32, "Dana")
    result = _three_voice_meeting(_new(registry=FakeRegistry({0: vp, 2: vp})))
    assert [s.label for s in result.speakers] == ["Dana", "Speaker 2", "Dana"]
    assert [s.speaker_id for s in result.segments] == [None, "S1", "S2", "S3"]


def test_no_registry_computes_no_centroids_and_names_nobody(monkeypatch):
    seen = []
    real = collector_module.DiarizationCollector._label_source

    def spy(self, records, **kwargs):
        seen.append(kwargs)
        return real(self, records, **kwargs)

    monkeypatch.setattr(collector_module.DiarizationCollector, "_label_source", spy)
    result = _three_voice_meeting(_new())
    assert result.centroids == {}
    assert seen and all(kw == {"centroids": False} for kw in seen)
    assert [s.label for s in result.speakers] == ["Speaker 1", "Speaker 2", "Speaker 3"]


def test_registry_attached_then_removed_before_finalize_is_never_called():
    registry = FakeRegistry()
    c = _new(registry=registry)
    c.registry = None  # the server does this when recognition was turned off mid-meeting
    result = _three_voice_meeting(c)
    assert registry.calls == [] and result.centroids == {}
    assert result.speakers[0].label == "Speaker 1"


def test_raising_registry_leaves_everyone_anonymous(caplog):
    registry = FakeRegistry(error=RuntimeError("secret Dana [0.1, 0.2]"))
    with caplog.at_level("DEBUG", "localflow.diarization"):
        result = _three_voice_meeting(_new(registry=registry))
    assert result.status == "ok"
    assert [s.label for s in result.speakers] == ["Speaker 1", "Speaker 2", "Speaker 3"]
    assert len(registry.calls) == 1  # stops after the first failure
    text = "\n".join(r.getMessage() + str(r.exc_info or "") for r in caplog.records)
    assert "secret" not in text and "Dana" not in text


def test_unknown_embedder_id_skips_recognition():
    class NoId(MockEmbedder):
        id = None

    registry = FakeRegistry()
    c = _make(MockDiarizer(), NoId(), registry=registry)
    result = _three_voice_meeting(c)
    assert registry.calls == []
    assert result.speakers[0].label == "Speaker 1"


def test_single_remote_voice_is_not_recognised():
    registry = FakeRegistry()
    c = _new(registry=registry)
    _, a = _chunk(c, "others", 0.0, tone(0, 4))
    result = c.finalize([a])
    assert result.speakers == [] and registry.calls == []
    assert result.segments[0].speaker_label is None


def test_slow_registry_is_bounded_by_the_finalize_timeout():
    registry = FakeRegistry(delay=0.5)
    c = _new(registry=registry)
    _, a = _chunk(c, "others", 0.0, np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    t0 = time.monotonic()
    result = c.finalize([a], timeout=0.2)
    assert time.monotonic() - t0 < 0.45
    assert result.status == "timeout" and result.segments == [a] and result.centroids == {}
    # The abandoned matcher (3 x 0.5 s) ends before the autouse thread check gives up.


def test_gate_closed_after_the_drain_skips_recognition_entirely():
    registry = FakeRegistry()
    gate = {"open": True}
    c = _new(registry=registry, recognition_allowed=lambda: gate["open"])
    _, a = _chunk(c, "others", 0.0, np.concatenate([tone(0, 4), tone(1, 4), tone(2, 4)]))
    gate["open"] = False  # opted out after capture, before finalize reads the gate
    result = c.finalize([Seg(0.0, "hello", "me"), a])
    assert result.status == "ok" and result.centroids == {} and registry.calls == []
    assert [s.label for s in result.speakers] == ["Speaker 1", "Speaker 2", "Speaker 3"]


def test_gate_closing_mid_match_drops_names_found_so_far():
    gate = {"open": True}

    class ClosingRegistry(FakeRegistry):
        def match(self, vector, model_id):
            hit = super().match(vector, model_id)
            gate["open"] = False  # the opt-out lands during the first match
            return hit

    registry = ClosingRegistry()
    c = _new(registry=registry, recognition_allowed=lambda: gate["open"])
    result = _three_voice_meeting(c)
    assert len(registry.calls) == 1  # no further registry reads after the opt-out
    assert result.status == "ok" and result.centroids == {}
    assert [(s.label, s.name, s.voiceprint_id) for s in result.speakers] == [
        ("Speaker 1", None, None), ("Speaker 2", None, None), ("Speaker 3", None, None)]
    assert [s.speaker_label for s in result.segments] == [
        None, "Speaker 1", "Speaker 2", "Speaker 3"]


def test_raising_gate_fails_closed():
    registry = FakeRegistry()

    def broken():
        raise RuntimeError("config unavailable")

    result = _three_voice_meeting(_new(registry=registry, recognition_allowed=broken))
    assert registry.calls == [] and result.centroids == {}
    assert result.speakers[0].label == "Speaker 1"
