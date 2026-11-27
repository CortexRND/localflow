# LocalFlow performance eval system — design spec

Status: design only, nothing built. Written against `CortexRND/localflow` main at `a8e951f`.
Audience: the Bot that implements it. Suggested home in the repo: `docs/plans/PLAN-perf-evals.md` (commit it with PR 1).

## 1. Goal

One system that answers three questions for both runtimes (Python `lf`, Rust `lf-rs` / `lf-hub` / `lf-desktop`):

1. **Did this change make LocalFlow slower, heavier or less accurate?** (PR check)
2. **How has performance moved over time, and is Rust at least as good as Python?** (nightly trend, cutover evidence)
3. **How fast is it on the user's own machine in real use?** (in-app, local-only)

Non-goals: no remote telemetry (nothing leaves the machine), no new judge model, no UI benchmark of rendering frame rates, no change to product defaults.

## 2. What exists today (verified in the repo)

| Piece | What it measures | Gap |
|---|---|---|
| `rust/lf-stt/examples/stt_bench.rs` + `rust-bench.yml` | Rust STT load ms, first/median/p95 ms, RTF, RSS, one fixed clip, macOS only | No accuracy score, no Python counterpart, no baseline compare, tables only |
| `scripts/ci_parakeet_memory.py` | Parakeet RSS/footprint/latency across 12 ORT configurations, exact-match vs default transcript | macOS only, not human-labelled, no gate |
| `scripts/ci_speech_helper.py` | Real `lf-hub` + speech helper: memory per stage, cold/warm/reload ms, reference transcript | macOS only, asserts shape, no numeric budget |
| `scripts/ci_memory_analysis.py`, `ci_hub_pages.sh`, `ci_hub_rss.sh` | Hub page RSS/footprint, first-Ask attribution, idle RSS, same-runner before/after build | Tables only, no stored history |
| `rust/lf-runtime/examples/idle.rs` | Idle RSS and CPU time, Linux | Not run in CI |
| `rust/lf-ask/examples/bench_search.rs` | Recall search ms on a prepared DB/vault | Needs a hand-made corpus, not in CI |
| `evals/meeting_sim` (`driver.py`, `gates.py`, `scenarios.json`, `jev.ts`) | Meeting notes/chat quality: 13 deterministic gates, 6 scenarios × 3 repeats, Wilson intervals, optional Jev judge | Python `TestClient` only, hardcoded `~/work/sim-sut`, `~/sim-runs`, `~/work/venv-sys`, records no timings, not in CI |
| `tests/fixtures/dictation_corpus.jsonl` | Text pipeline correctness (no LLM) | Correctness only |
| Product code | `process_clip` logs `transcribed N chars in Xms` (both runtimes); `POST /transcribe` returns `{"text","ms"}` (both servers); `lf_stt::Timings{audio_seconds, inference_ms}`, `LoadInfo{load_ms, download_ms, provider}` | Total only: STT, LLM and paste are not separated; nothing is stored |

So the measurements mostly exist; what is missing is a **common result format, a baseline compare with gates, coverage of the Python runtime and Linux/Windows, a trend store, and in-app numbers**. The design adds those and wraps the existing helpers instead of replacing them.

## 3. Architecture

```
            ┌─ suites (produce raw results) ─────────────────────────┐
            │ stt · dictation · meetings · recall · footprint        │
            │   Rust: stt_bench, parakeet_memory, bench_search, idle │
            │   Python: new thin benches over localflow.*            │
            │   existing CI helpers via adapters (unchanged)         │
            └───────────────┬────────────────────────────────────────┘
                            ▼
   evals/perf/  run ──► result.json (schema lf-perf-result/v1)
                compare (baseline vs candidate, policy.toml) ──► report.md + exit code
                            ▼
   .github/workflows/perf-evals.yml: PR smoke · nightly full · manual deep
                            ▼
   trend store (perf-data branch) ──► trend.md, one regression issue

   In-app: perf ledger (~/.localflow/perf/events.jsonl) ──► GET /api/perf, `lf perf`, Hub card
```

Everything in `evals/perf` is plain Python 3.11+ stdlib (no new product dependency); it never imports `localflow` except inside the Python benches.

## 4. Result schema `lf-perf-result/v1`

One JSON file per run. All suites and adapters emit only this.

```json
{
  "schema": "lf-perf-result/v1",
  "run": {"id": "...", "sha": "...", "ref": "...", "tier": "smoke|nightly|deep",
          "runtime": "python|rust", "os": "linux|macos|windows", "arch": "x86_64|arm64",
          "runner": {"label": "ubuntu-latest", "cpu": "...", "cores": 4, "ram_gb": 16},
          "started_at": "ISO-8601", "harness": "1"},
  "cases": [{
    "suite": "stt", "case": "whisper-base.en-cpu/clip-03",
    "dims": {"engine": "whisper-cpp", "model": "base.en", "provider": "cpu", "clip": "clip-03"},
    "status": "ok|failed|not_exercised", "error": null,
    "metrics": {
      "latency_ms": {"unit": "ms", "better": "lower", "samples": [..], "median": 0, "p95": 0},
      "rtf":        {"unit": "ratio", "better": "lower", "value": 0},
      "wer":        {"unit": "ratio", "better": "lower", "value": 0},
      "rss_loaded": {"unit": "bytes", "better": "lower", "value": 0}
    },
    "evidence": {"transcript": "...", "effective_provider": "..."}
  }]
}
```

Rules: a case that could not run is `not_exercised` with a reason, never silently absent (same rule `gates.py` applies to missing scenarios). Raw samples are kept so the compare step can compute spread. `evidence` holds only fixture-derived text, never user data.

## 5. Suites and metrics

Each case runs on **both runtimes wherever both implement it**, with the same inputs and the same scoring code.

### 5.1 `stt` — speech-to-text
- **Inputs:** `evals/perf/corpus/stt.json`: 10 short English clips with human reference transcripts (LibriSpeech test-clean, CC BY 4.0), 3–15 s each, plus the existing Parakeet `test_wavs/0.wav`. Clips are downloaded and SHA-256 pinned, cached with `actions/cache` (same approach as the HF model cache today), not committed.
- **Rust:** extend `stt_bench` with `--manifest <json>` (loop over clips; existing single-clip mode unchanged) and emit per-clip transcripts.
- **Python:** new `evals/perf/benches/py_stt.py` driving `localflow.stt` with the same CLI flags and output.
- **Metrics:** `download_ms`, `load_ms`, `first_ms`, `latency_ms` (median/p95 over N runs), `rtf`, `rss_before/loaded/after_unload`, `wer`, `effective_provider`.
- **WER is computed once, in the harness**, from the emitted transcripts (lowercase, strip punctuation, collapse whitespace, word-level edit distance), so both runtimes are scored by identical code.
- **Matrix:** smoke = `tiny.en` CPU, 3 clips, 3 runs. Nightly = Linux {faster-whisper (Py), whisper-cpp (Rs), parakeet-onnx CPU} × {tiny.en, base.en, small}; macOS adds Metal, CoreML and MLX. Rust has no CUDA path (known), so CUDA is Python-only and deep-tier only.

### 5.2 `dictation` — clip in, text out
- **HTTP end to end:** start `lf serve` and `lf-rs serve` on a temp config, `POST /transcribe` each corpus clip. Metrics: server-reported `ms`, client wall ms, cold first request, p50/p95, error rate, startup time to `/healthz` ok.
- **Text pipeline:** run `tests/fixtures/dictation_corpus.jsonl` through `render_dictation` (Python) and the Rust equivalent (`lf/examples/parity.rs` already exposes it): per-case µs and exact-match rate. Cheap and deterministic, so it gates on PRs.
- **Cleanup LLM (nightly, needs Ollama `llama3.2:3b`):** 20 fixed raw transcripts → `llm_ms` median/p95, plus deterministic fidelity checks reused from the sim (no unsupported numbers, no dropped vocabulary words).

### 5.3 `meetings` — notes and chat
- Reuse `evals/meeting_sim` as is, with three changes: paths come from flags/env instead of hardcoded home directories; `driver.py` records `summarize_ms`, per-chat `chat_ms` and per-LLM-call ms; `gates.py` output is converted by an adapter (gate M1–M13 → one case each, `value` = pooled rate, plus the Wilson interval).
- New metrics: `stop_to_notes_ms`, `chat_ms` p50/p95, LLM calls per meeting.
- **Rust:** the sim patches Python internals (scripted transcriber, `sd.InputStream`), so it cannot drive `lf-rs serve` today. Phase 1 is Python only and reports Rust as `not_exercised`. Phase 2 needs a scripted-transcript seam in `lf-meetings-backend`, enabled only by a test-only env var; that is a product change and should be its own PR (see open decisions). *Landed in PR 11 without any product hook:* the seam is the eval-only example `rust/lf-meetings-backend/examples/meeting_sim` (built with `cargo build -p lf-meetings-backend --example meeting_sim`, never part of `lf-rs`/`lf-hub`). It plugs a scripted `CaptureFactory` and a timing wrapper around the real `ConfiguredProviders` into `MeetingsBackend`, drives the real `lf-server` router through start → scripted lines + notes → stop → chat → restart chat, takes the same flags as `driver.py` and writes the same result JSON. `--runtime rust` runs it (`LF_PERF_MEETING_SIM_BIN` or `rust/target/{release,debug}/examples/meeting_sim`; `LF_PERF_MEETING_RUST_RUN_DIR` adapts a recorded run) and reports `not_exercised` when the binary, Ollama or the model is missing.
- Jev stays a second, advisory judge on the deep tier only (it needs `TYPESAFE_API_KEY`).

### 5.4 `recall`
- Deterministic generator `evals/perf/fixtures/make_recall_corpus.py` (seeded) writes a `history.sqlite` + vault at 1k and 10k entries. Rust: `bench_search` with a `--queries` file; Python: same loop over `localflow.recall`.
- Metrics: search ms p50/p95, peak RSS, and **citation-set equality between runtimes** (a correctness gate).

### 5.5 `footprint` — memory, idle cost, startup, size
- Adapters over outputs that CI already produces: `ci_speech_helper.py` (`result.json`), `ci_parakeet_memory.py` (`results.json`), `ci_hub_pages.sh` (`pages.csv`), `ci_hub_rss.sh` (idle table). The scripts stay unchanged; adapters live in `evals/perf/adapters/`.
- Add: `idle.rs` on Linux (RSS + CPU seconds over 30 s idle), a Python idle equivalent, process start → `/healthz` ms for both servers, and installed package size from the `gpui-package.yml` artifacts.

## 6. Compare and gates

`python -m evals.perf compare --baseline B.json --candidate C.json --policy evals/perf/policy.toml` → `report.md` (table: metric, baseline, candidate, delta, verdict) and an exit code.

Policy per metric, three classes:

| Class | Examples | Rule | On PR |
|---|---|---|---|
| **Exact** | lifecycle gates, text-pipeline exact match, citation equality, helper PID cleanup | must equal / pass in every repeat | Blocks |
| **Quality** | WER, sim M5–M10 rates | candidate ≤ baseline + absolute tolerance (WER +0.02; sim uses its existing thresholds) | Blocks |
| **Noisy** | latency, RTF, RSS, startup | regression only if median worsens by more than `max(rel_tol, 3 × MAD of baseline samples)`; `rel_tol` 15 % latency, 10 % memory | Warns; blocks only above 2× |

Noise control (hosted runners vary run to run):
- **Same-runner A/B for PRs:** build the merge-base and the PR head in one job and compare them there, as the `memory` job in `rust.yml` already does. Never compare a PR against numbers from another machine.
- N ≥ 5 timed runs after one discarded warm-up; compare medians; report spread.
- Verdicts: `pass`, `regressed`, `improved`, `flaky` (differs between repeats), `not_exercised`, `low_power` (n too small), mirroring `gates.py`.
- Absolute budgets are **not** invented up front: the first nightly run on main becomes the baseline, and budgets are then written into `policy.toml` from it. The one existing product target (text within 3 s for `tiny` on CPU, from the cross-platform test plan) goes in as an absolute cap.

## 7. CI: `.github/workflows/perf-evals.yml`

| Tier | Trigger | Runners | Contents | Target time |
|---|---|---|---|---|
| Smoke | PRs touching `localflow/**`, `rust/**`, `evals/**` | ubuntu-latest | stt tiny (both runtimes), text pipeline, recall 1k, same-runner A/B | < 10 min |
| Nightly | schedule on main, `workflow_dispatch` | ubuntu, macos-latest, windows-latest (stt + dictation only) | full matrix §5, cleanup LLM, meeting sim (Python), footprint adapters | < 60 min |
| Deep | `workflow_dispatch` only | macOS + any self-hosted GPU | large models, CUDA, Jev judge, 10k recall | manual |

Outputs every run: `result.json` + `report.md` as artifacts and in `$GITHUB_STEP_SUMMARY`; on PRs one sticky comment with the compare table. `rust-bench.yml` and the `memory` job keep running; once the nightly tier covers them, `rust-bench.yml` can be folded in (separate, later PR).

## 8. Trend store

- Nightly appends `runs/<date>-<sha>-<os>-<runtime>.json` and one line per case to `index.jsonl` on an orphan `perf-data` branch (needs `contents: write` in that one job only).
- `python -m evals.perf trend` renders `trend.md`: last 30 runs per headline metric, and a **Rust ÷ Python ratio** column per case, which is the evidence the cutover gates in `docs/gpui-migration.md` currently lack.
- A nightly regression that persists for 2 consecutive runs opens or updates one GitHub issue per metric (`perf-regression` label); it closes itself when the metric recovers.

## 9. In-app monitoring: the perf ledger

Purpose: real-use numbers on the user's machine, local only.

- **Store:** `~/.localflow/perf/events.jsonl`, append-only, rotated at 5 MB (keep one previous file). A separate file, **not** new columns in `history.sqlite`: the history schema is version 1 in both runtimes and each refuses to open a newer `user_version`, so a schema bump would break running the two runtimes side by side.
- **Event:** `{ts, runtime, version, kind, status, audio_s, queue_ms, stt_ms, llm_ms, render_ms, paste_ms, total_ms, stt:{backend,model,provider}, llm:{provider,model}, chars}`. `kind` ∈ `dictate | command | transform | meeting_summarize | meeting_chat | recall | stt_load | llm_warmup | startup`. **Numbers and labels only; never transcript text, app names or prompts.**
- **Hook points:** `process_clip` / `process_job` (`rust/lf-runtime/src/pipeline.rs`, `localflow/app.py`) around `speech.transcribe`, the cleanup/transform LLM call and the paste in the worker; `LoadInfo` on model load; `warmup.rs`; meeting stop and chat handlers; recall handler. The existing `transcribed N chars in Xms` log line stays.
- **Config:** `perf_metrics` (bool, default `true`). Adding a key touches the config contract tests that compare the two servers byte for byte, so add it to both runtimes in one PR.
- **Read side:** `GET /api/perf?window=24h|7d|30d` (token required, both servers, identical JSON): per kind count, failure rate, p50/p95 of each stage, median RTF, model load times. `lf perf` / `lf-rs perf` print the same as a table, `--json` for raw, `--clear` to delete the ledger.
- **Hub card (last, optional):** a "Speed" card on Home: median dictation time, RTF, slowest stage, and a hint when p95 exceeds the budget ("cleanup LLM is the slow step").
- **Parity:** a new case in `tests/test_rust_parity.py` feeds both runtimes the same fixture events and compares `/api/perf` output; event files written by either runtime must be readable by the other.

## 10. Delivery plan (each PR safe to merge alone)

| # | PR | Touches product code? |
|---|---|---|
| 1 | This spec + `evals/perf` schema, `compare`, `policy.toml`, unit tests | No |
| 2 | STT suite: `stt_bench --manifest`, Python STT bench, corpus manifest, WER | Example only |
| 3 | `perf-evals.yml` smoke tier with same-runner A/B and PR comment | No |
| 4 | Footprint adapters over the existing memory/helper outputs; idle and startup probes | No |
| 5 | Dictation suite over HTTP for both servers + text-pipeline timing | No |
| 6 | Recall corpus generator and bench for both runtimes | Example only |
| 7 | Meeting sim: configurable paths, timings, gates adapter (Python) | No |
| 8 | Nightly tier + trend store on `perf-data` + regression issue | No |
| 9 | Perf ledger writer, Rust and Python together, with parity test | Yes |
| 10 | `/api/perf` and `lf perf` in both runtimes | Yes |
| 11 | Hub "Speed" card; Rust meeting-sim seam | Yes |

PRs 1–8 change no shipped behaviour and can start immediately; 9–11 are additive and default to recording only.

## 11. Acceptance criteria

- `python -m evals.perf run --tier smoke --runtime rust` and `--runtime python` both produce a valid `lf-perf-result/v1` file on a clean Ubuntu runner with no network beyond the pinned corpus/model downloads.
- An injected 2× slowdown in STT (sleep in the bench) makes `compare` exit non-zero; an unchanged rebuild compared with itself never reports `regressed` across 10 consecutive runs.
- A wrong transcript (corrupt one reference) fails the WER gate.
- Every case in the nightly matrix appears in the report as `ok`, `failed` or `not_exercised` with a reason.
- With `perf_metrics = false` no file is written under `~/.localflow/perf/`; with it on, a grep of the ledger for any fixture transcript word finds nothing.

## 12. Open decisions (defaults taken; overrule any)

1. **Ledger on by default** (local, numbers only). Alternative: opt-in.
2. **Trend data on a `perf-data` branch.** Alternative: artifacts only (90-day retention, no long trend).
3. **Latency and memory warn on PRs, block only above 2×**; exact and quality metrics block. Alternative: block on all.
4. **Jev judge only on the manual deep tier**, advisory.
5. **Rust meeting sim deferred** behind a test-only scripted-transcript seam (PR 11).
6. **STT corpus is LibriSpeech test-clean** (clean read speech). It will not show accent, noise or filler-word behaviour; a second, harder set can be added later without schema changes.

## 13. Not verified in this design pass

No prototype was run. Unchecked assumptions the implementer should confirm first: the `render` operation in `lf/examples/parity.rs` (it calls `hotphrases::render_dictation`) covers every option the corpus cases use; `localflow.recall` can be pointed at an arbitrary DB/vault the way `bench_search` can; GitHub-hosted runner noise is within the 15 %/10 % tolerances (if not, raise them from the first week of nightly data rather than blocking PRs).
