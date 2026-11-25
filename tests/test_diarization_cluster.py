import time

import numpy as np
import pytest

from localflow.diarization import cluster


def _partition(labels):
    groups = {}
    for i, label in enumerate(labels):
        groups.setdefault(label, set()).add(i)
    return {frozenset(g) for g in groups.values()}


def _groups(rng, n_per=10, dim=32, spread=0.05):
    centers = np.eye(dim)[:3]
    vecs = [c + spread * rng.standard_normal(dim) for c in centers for _ in range(n_per)]
    return np.array(vecs), [k for k in range(3) for _ in range(n_per)]


def test_three_groups_plus_noise_recovered():
    rng = np.random.default_rng(0)
    vecs, truth = _groups(rng)
    order = rng.permutation(len(vecs))
    vecs, truth = vecs[order], [truth[i] for i in order]
    noise = np.zeros((2, vecs.shape[1]))
    noise[0, 10], noise[1, 20] = 1.0, 1.0
    labels = cluster(np.vstack([vecs, noise]), threshold=0.4)
    assert _partition(labels[: len(truth)]) == _partition(truth)
    assert len(set(labels)) == 5
    assert len({labels[-2], labels[-1]} & set(labels[: len(truth)])) == 0


def test_threshold_extremes():
    vecs, _ = _groups(np.random.default_rng(1))
    assert cluster(vecs, threshold=-1.0) == list(range(len(vecs)))
    assert cluster(vecs, threshold=3.0) == [0] * len(vecs)


def test_max_speakers_caps_clusters():
    vecs, truth = _groups(np.random.default_rng(2))
    assert len(set(cluster(vecs, threshold=-1.0, max_speakers=3))) == 3
    assert _partition(cluster(vecs, threshold=-1.0, max_speakers=3)) == _partition(truth)
    assert set(cluster(vecs, threshold=0.4, max_speakers=1)) == {0}
    assert _partition(cluster(vecs, threshold=0.4, max_speakers=5)) == _partition(truth)


def test_deterministic_and_permutation_invariant():
    rng = np.random.default_rng(3)
    vecs = rng.standard_normal((40, 16))
    a = cluster(vecs, threshold=0.8)
    assert a == cluster(vecs.copy(), threshold=0.8)
    perm = rng.permutation(40)
    b = cluster(vecs[perm], threshold=0.8)
    unpermuted = [None] * 40
    for pos, idx in enumerate(perm):
        unpermuted[idx] = b[pos]
    assert _partition(unpermuted) == _partition(a)


def test_first_appearance_labels_are_ints():
    x, y = np.array([1.0, 0.0]), np.array([0.0, 1.0])
    labels = cluster([y, x, y, x], threshold=0.1)
    assert labels == [0, 1, 0, 1]
    assert all(type(label) is int for label in labels)


def test_edge_inputs():
    assert cluster([], threshold=0.5) == []
    assert cluster(np.zeros((0, 4)), threshold=0.5) == []
    assert cluster([np.ones(4)], threshold=0.5) == [0]
    assert cluster(np.array([[1.0, 0.0], [2.0, 0.0]]), threshold=0.1) == [0, 0]
    assert cluster([np.zeros(3), np.array([1.0, 0, 0])], threshold=0.99) == [0, 1]
    assert cluster([np.zeros(3), np.zeros(3)], threshold=0.99) == [0, 1]
    with pytest.raises(ValueError):
        cluster([np.ones(3), np.ones(4)], threshold=0.5)
    with pytest.raises(ValueError):
        cluster(np.ones(3), threshold=0.5)
    for bad in (0, -1):
        with pytest.raises(ValueError):
            cluster([], threshold=0.5, max_speakers=bad)


def test_timing_1000x192():
    vecs = np.random.default_rng(4).standard_normal((1000, 192)).astype(np.float32)
    t = time.perf_counter()
    labels = cluster(vecs, threshold=0.7, max_speakers=1)
    assert time.perf_counter() - t < 10
    assert len(labels) == 1000 and set(labels) == {0}
