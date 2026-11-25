"""Deterministic agglomerative clustering (average linkage, cosine distance)."""

from __future__ import annotations

from collections.abc import Sequence

import numpy as np


def _as_matrix(vectors: np.ndarray | Sequence[np.ndarray]) -> np.ndarray:
    try:
        x = np.array(vectors, dtype=np.float64)
    except ValueError as exc:
        raise ValueError(f"vectors must share one dimension: {exc}") from None
    if x.ndim != 2:
        raise ValueError(f"expected a 2-D array or a list of 1-D vectors, got shape {x.shape}")
    norms = np.linalg.norm(x, axis=1, keepdims=True)
    return np.divide(x, norms, out=np.zeros_like(x), where=norms > 0)


def cluster(
    vectors: np.ndarray | Sequence[np.ndarray],
    *,
    threshold: float,
    max_speakers: int | None = None,
) -> list[int]:
    """Cluster vectors and return labels 0..k-1 numbered by first appearance.

    Merges the closest pair (ties: lowest index pair) while its distance is ``<= threshold``,
    then keeps merging until at most ``max_speakers`` clusters remain. Zero vectors have
    distance 1 to everything else.
    """
    if max_speakers is not None and max_speakers < 1:
        raise ValueError("max_speakers must be >= 1")
    if len(vectors) == 0:
        return []
    x = _as_matrix(vectors)
    n = len(x)
    dist = 1.0 - x @ x.T
    dist[np.tril_indices(n)] = np.inf
    sizes = np.ones(n)
    parent = np.arange(n)
    alive = n
    limit = max_speakers or n
    while alive > 1:
        flat = int(np.argmin(dist))
        i, j = divmod(flat, n)
        d = dist[i, j]
        if d > threshold and alive <= limit:
            break
        col_i = np.minimum(dist[:, i], dist[i, :])
        col_j = np.minimum(dist[:, j], dist[j, :])
        merged = (sizes[i] * col_i + sizes[j] * col_j) / (sizes[i] + sizes[j])
        dist[:i, i] = merged[:i]
        dist[i, i + 1 :] = merged[i + 1 :]
        dist[j, :] = np.inf
        dist[:, j] = np.inf
        sizes[i] += sizes[j]
        parent[parent == j] = i
        alive -= 1
    labels: dict[int, int] = {}
    return [labels.setdefault(int(p), len(labels)) for p in parent]
