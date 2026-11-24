"""Word error rate, shared by both runtimes (stdlib only).

Both the Rust and the Python STT benches only emit transcripts; the harness
scores them here so the two runtimes are judged by identical code
(``docs/plans/PLAN-perf-evals.md`` §5.1).
"""

from __future__ import annotations

import re

_PUNCT = re.compile(r"[^\w\s]")


def normalize_text(text: str) -> list[str]:
    """Lowercase, replace punctuation with spaces, split on whitespace.

    Apostrophes are punctuation too, so ``country's`` becomes ``country s`` on
    both sides; reference and hypothesis are normalized identically.
    """
    return _PUNCT.sub(" ", (text or "").lower()).split()


def word_edit_distance(ref_words: list[str], hyp_words: list[str]) -> int:
    """Levenshtein distance over words (substitution/insertion/deletion = 1)."""
    if len(ref_words) < len(hyp_words):
        ref_words, hyp_words = hyp_words, ref_words  # distance is symmetric
    prev = list(range(len(hyp_words) + 1))
    for i, ref in enumerate(ref_words, 1):
        cur = [i] + [0] * len(hyp_words)
        for j, hyp in enumerate(hyp_words, 1):
            cur[j] = min(
                prev[j] + 1,  # deletion
                cur[j - 1] + 1,  # insertion
                prev[j - 1] + (ref != hyp),  # substitution / match
            )
        prev = cur
    return prev[-1]


def compute_wer(reference: str, hypothesis: str) -> float:
    """``edits / len(reference words)``; 0.0 if both empty, 1.0 if only ref empty."""
    ref = normalize_text(reference)
    hyp = normalize_text(hypothesis)
    if not ref:
        return 0.0 if not hyp else 1.0
    return word_edit_distance(ref, hyp) / len(ref)
