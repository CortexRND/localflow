import numpy as np
import pytest

from localflow.diarization import (
    Diarizer,
    MockDiarizer,
    MockEmbedder,
    SpeakerEmbedder,
    SpeakerTurn,
    cluster,
    tone_index,
)

RATE = 16000


def tone(i, seconds):
    return np.full(int(RATE * seconds), 0.1 + i * 0.001, dtype=np.float32)


def silence(seconds):
    return np.zeros(int(RATE * seconds), dtype=np.float32)


def meeting():
    return np.concatenate([tone(0, 2), tone(1, 1.5), silence(1), tone(2, 2), tone(0, 1)])


def test_tone_index():
    assert [tone_index(tone(i, 0.5)) for i in (0, 1, 7, 42)] == [0, 1, 7, 42]
    assert tone_index(silence(0.5)) is None
    assert tone_index(np.full(100, 0.005, np.float32)) is None
    assert tone_index(np.zeros(0, np.float32)) is None


def test_diarize_three_lines_with_silence():
    turns = MockDiarizer().diarize(meeting(), RATE)
    assert turns == [
        SpeakerTurn(0.0, 2.0, 0),
        SpeakerTurn(2.0, 3.5, 1),
        SpeakerTurn(4.5, 6.5, 2),
        SpeakerTurn(6.5, 7.5, 0),
    ]
    assert MockDiarizer().diarize(silence(2), RATE) == []
    assert MockDiarizer().diarize(np.zeros(0, np.float32), RATE) == []


def test_diarize_partial_last_frame():
    audio = np.concatenate([tone(3, 1), tone(5, 0.75)])
    assert MockDiarizer().diarize(audio, RATE) == [
        SpeakerTurn(0.0, 1.0, 0),
        SpeakerTurn(1.0, 1.75, 1),
    ]


@pytest.mark.parametrize("speaker_of", [{0: 9, 1: 9}, lambda i: 9 if i < 2 else i])
def test_speaker_of_merges_lines(speaker_of):
    audio = np.concatenate([tone(0, 1), tone(1, 1), tone(2, 1), tone(1, 1)])
    assert MockDiarizer(speaker_of=speaker_of).diarize(audio, RATE) == [
        SpeakerTurn(0.0, 2.0, 0),
        SpeakerTurn(2.0, 3.0, 1),
        SpeakerTurn(3.0, 4.0, 0),
    ]
    emb = MockEmbedder(speaker_of=speaker_of)
    assert np.array_equal(emb.embed(tone(0, 1), RATE), emb.embed(tone(1, 1), RATE))


def test_embedder_vectors():
    emb = MockEmbedder()
    vecs = [emb.embed(tone(i, 1), RATE) for i in range(64)]
    for v in vecs:
        assert v.dtype == np.float32 and v.shape == (64,)
        assert np.linalg.norm(v) == pytest.approx(1.0)
    assert np.array_equal(vecs[5], emb.embed(tone(5, 0.5), RATE))
    sims = np.array(vecs) @ np.array(vecs).T
    assert np.max(sims - np.eye(64)) <= 0.2
    unknown = emb.embed(silence(1), RATE)
    assert np.allclose(unknown, 1 / 8) and np.linalg.norm(unknown) == pytest.approx(1.0)
    assert np.allclose(np.array(vecs) @ unknown, 0.125)


def test_end_to_end_cluster():
    audio = meeting()
    emb = MockEmbedder()
    turns = MockDiarizer().diarize(audio, RATE)
    vecs = [emb.embed(audio[int(t.start * RATE) : int(t.end * RATE)], RATE) for t in turns]
    assert cluster(vecs, threshold=0.5) == [0, 1, 2, 0]


def test_protocols():
    assert isinstance(MockDiarizer(), Diarizer)
    assert isinstance(MockEmbedder(), SpeakerEmbedder)
    assert MockDiarizer.id == "mock" and MockEmbedder.id == "mock"
    MockDiarizer().load()
    MockEmbedder().load()


@pytest.mark.parametrize("backend", ["diarize", "embed"])
def test_validation_errors(backend):
    fn = MockDiarizer().diarize if backend == "diarize" else MockEmbedder().embed
    with pytest.raises(ValueError):
        fn(np.zeros((2, 100), np.float32), RATE)
    for rate in (0, -16000):
        with pytest.raises(ValueError):
            fn(tone(0, 1), rate)
    with pytest.raises(ValueError):
        MockEmbedder(dim=0)
