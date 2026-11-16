# lf-ask

Optional Rust port of `localflow/recall.py`, with Python remaining the default.

* `Corpus::from_config(&config, store)` locates the existing vault, notes and log
  folders using lf-core config and Python-compatible home expansion. Alternatively
  construct `Corpus` with explicit paths. Disabled history is not scanned.
* `search(question, &corpus, &options)` returns ranked citations only.
* `recall(question, &llm, &corpus, &options)` takes an lf-providers `LlmProvider`;
  callers can use its configured production factory or inject a fake. No second
  provider abstraction or credential handling is added here.
* `SearchOptions` controls k (default 8), `Since` (text/date/local datetime), the
  local reference time, and optional source names (empty = all).
* `build_corpus`, `rank`, `tokenize`, `stem`, `extract_phrases`, chunking, timestamp
  parsing, citation cleaning and exact Python prompt formatting are public for
  composition and parity tests. `RecallResult::to_dict` and serde serialization
  keep Python's answer/citations structure, including the `ref` key.

Retrieval reproduces Python's stopwords, small suffix stemmer, BM25 parameters,
quoted-phrase boosts, recency weighting, transcript/note metadata stripping and
word-bounded chunks. Answers use the exact recall prompt. No passages means
`NO_ANSWER` without an LLM call; provider failures return useful citations with
`answer: null`; empty/sentinel answers discard citations.

## Scope and differences

No lf-core/lf-providers changes, CLI commands, server routes, token access, or
Python changes. CLI/API-specific tests from `test_recall.py` are not ported here:
those integrations belong to their future Rust crates. The underlying retrieval
and serialization cases are covered directly. `sources` is additive: the current
Python recall module does not itself have a source filter.

Rust exposes typed options/results; k is unsigned (0 returns no results). The
date/time API uses chrono local naive date/time values, matching Python's local
time semantics. Filesystem timestamps and references retain platform-local paths.
Unreadable/invalid-UTF-8 note and transcript files are skipped without logging
their contents. Out-of-range relative dates return an error, and overflowing
transcript offsets safely fall back to the file start.

Tests use temporary files, a real temporary lf-core history database, and an
injected lf-providers fake; no network or model is used. macOS/Windows execution
has not been independently verified.
