"""``diarization`` suite: speaker-label quality (DER), speed (RTF) and memory.

Stdlib only; never imports ``localflow`` or ``numpy``. The suite runs
``benches/py_diarization.py`` in a subprocess, which emits speaker turns per
mix; every hypothesis is scored here with :func:`evals.perf.der.compute_der`
(collar ``COLLAR_S``) so each backend is judged by the same code.

Plan (per tier):

* ``mock`` × ``whole``/``chunked`` on the synthetic constant-tone mix
  (``corpus/mixes.write_synthetic_mix``): deterministic quality metrics only,
  no timing or RSS (microsecond timings would trip the noisy hard block).
* ``sherpa-onnx`` × ``chunked`` (the meeting product path) on the smoke mix
  (smoke) or every mix (nightly/deep, plus ``whole``) of ``diarization.json``.
  The backend is probed once before any download; without the
  ``localflow[diarization]`` extra or the speaker models every sherpa case is
  ``not_exercised`` with the bench's reason.

Each entry yields one case per mix plus an aggregate ``…/all`` case pooled
over its mixes (``der.aggregate``); the ``[[caps]]`` in ``policy.toml`` hold the
sherpa chunked aggregate to DER, RTF and RSS-delta gates. Rust has no
diarization path, so with ``runtime == "rust"`` every case is ``not_exercised``.

``RunContext.options`` (all optional): ``manifest`` (corpus manifest path),
``cache_dir`` (clip cache), ``python`` (interpreter for the bench; env
``LF_PERF_PYTHON``), ``timeout_s`` (per bench subprocess), ``threshold``,
``cluster_threshold``, ``max_speakers``, ``num_threads``, ``chunk_seconds``
(passed to the sherpa bench runs).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from .. import der
from ..corpus.download import default_cache_dir, ensure_stt_clips
from ..corpus.mixes import (
    DEFAULT_DIARIZATION_MANIFEST_PATH,
    build_mix,
    load_diarization_manifest,
    write_synthetic_mix,
)
from ..registry import RunContext, register_suite
from ..schema import make_case, make_metric

SUITE = "diarization"
COLLAR_S = 0.25
RUST_REASON = "Rust has no speaker diarization path"
SYNTHETIC_MIX_ID = "synthetic-tones"
EXIT_UNAVAILABLE = 3  # py_diarization.py: runtime or models not available
DEFAULT_TIMEOUT_S = 1800
ERROR_TAIL = 800
MIX_SETS = ("synthetic", "smoke", "all")
QUALITY_RATIOS = ("der", "missed_speech", "false_alarm", "speaker_confusion")
#: Bench flags taken from ``RunContext.options`` for real-backend entries.
BENCH_OPTIONS = {
    "threshold": "--threshold",
    "cluster_threshold": "--cluster-threshold",
    "max_speakers": "--max-speakers",
    "num_threads": "--num-threads",
    "chunk_seconds": "--chunk-seconds",
}

PY_BENCH = Path(__file__).resolve().parents[1] / "benches" / "py_diarization.py"


# --------------------------------------------------------------------------
# Matrix
# --------------------------------------------------------------------------


def _entry(backend: str, pipeline: str, *, runs: int, warmup: int, mixes: str,
           not_exercised: str | None = None) -> dict:
    return {"backend": backend, "pipeline": pipeline, "runs": runs, "warmup": warmup,
            "mixes": mixes, "not_exercised": not_exercised}


def plan_diarization_matrix(tier: str, runtime: str) -> list[dict]:
    """The (backend, pipeline, mix set) entries for one tier/runtime. Pure function."""
    if runtime not in ("python", "rust"):
        raise ValueError(f"unknown runtime {runtime!r}")
    if tier not in ("smoke", "nightly", "deep"):
        raise ValueError(f"unknown tier {tier!r}")
    skip = RUST_REASON if runtime == "rust" else None
    plan = [
        _entry("mock", "whole", runs=1, warmup=0, mixes="synthetic", not_exercised=skip),
        _entry("mock", "chunked", runs=1, warmup=0, mixes="synthetic", not_exercised=skip),
    ]
    if tier == "smoke":
        plan.append(_entry("sherpa-onnx", "chunked", runs=3, warmup=1, mixes="smoke", not_exercised=skip))
    else:
        plan.append(_entry("sherpa-onnx", "chunked", runs=5, warmup=1, mixes="all", not_exercised=skip))
        plan.append(_entry("sherpa-onnx", "whole", runs=5, warmup=1, mixes="all", not_exercised=skip))
    return plan


def case_name(entry: dict, mix_id: str) -> str:
    return f"{entry['backend']}-{entry['pipeline']}/{mix_id}"


def _dims(entry: dict, mix_id: str, kind: str) -> dict:
    return {"backend": entry["backend"], "pipeline": entry["pipeline"], "mix": mix_id, "kind": kind}


def _non_ok(entry: dict, mix_ids: list[str], status: str, reason: str) -> list[dict]:
    """Every mix case of ``entry`` plus its aggregate with one status and reason."""
    cases = [
        make_case(suite=SUITE, case=case_name(entry, mid), dims=_dims(entry, mid, "mix"),
                  status=status, error=reason)
        for mid in mix_ids
    ]
    cases.append(make_case(suite=SUITE, case=case_name(entry, "all"), dims=_dims(entry, "all", "aggregate"),
                           status=status, error=reason))
    return cases


def _is_mock(entry: dict) -> bool:
    return entry["backend"] == "mock"


# --------------------------------------------------------------------------
# Bench invocation
# --------------------------------------------------------------------------


def _python(ctx: RunContext) -> str:
    return str(ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable)


def bench_command(ctx: RunContext, entry: dict, manifest: Path, out: Path) -> list[str]:
    cmd = [
        _python(ctx), str(PY_BENCH),
        "--manifest", str(manifest),
        "--backend", entry["backend"],
        "--pipeline", entry["pipeline"],
        "--runs", str(entry["runs"]),
        "--warmup", str(entry["warmup"]),
        "--json", str(out),
    ]
    if not _is_mock(entry):
        for key, flag in BENCH_OPTIONS.items():
            if ctx.options.get(key) is not None:
                cmd += [flag, str(ctx.options[key])]
    return cmd


def _bench_env(ctx: RunContext) -> dict:
    env = dict(os.environ)
    root = str(ctx.repo_root)  # let the bench import localflow and evals from this checkout
    env["PYTHONPATH"] = os.pathsep.join(p for p in (root, env.get("PYTHONPATH")) if p)
    return env


def _tail(text: str | bytes | None) -> str:
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    text = (text or "").strip()
    return text[-ERROR_TAIL:] if len(text) > ERROR_TAIL else text


def _timeout(ctx: RunContext) -> float:
    return float(ctx.options.get("timeout_s", DEFAULT_TIMEOUT_S))


def _run(ctx: RunContext, cmd: list[str]) -> tuple[subprocess.CompletedProcess | None, str]:
    """``(proc, "")`` or ``(None, failure reason)`` for a timeout or start error."""
    timeout = _timeout(ctx)
    try:
        proc = subprocess.run(
            cmd, cwd=str(ctx.repo_root), env=_bench_env(ctx), capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, check=False,
        )
    except subprocess.TimeoutExpired as exc:
        return None, f"bench timed out after {timeout:g} s: {_tail(exc.stderr)}".strip()
    except OSError as exc:
        return None, f"could not start bench: {exc}"
    return proc, ""


def probe_backend(ctx: RunContext, backend: str) -> tuple[str, str]:
    """``("ok", "")``, ``("not_exercised", reason)`` or ``("failed", reason)``; offline."""
    cmd = [_python(ctx), str(PY_BENCH), "--probe", "--backend", backend]
    if ctx.options.get("num_threads") is not None:
        cmd += ["--num-threads", str(ctx.options["num_threads"])]
    proc, error = _run(ctx, cmd)
    if proc is None:
        return "failed", error
    if proc.returncode == 0:
        return "ok", ""
    if proc.returncode == EXIT_UNAVAILABLE:
        return "not_exercised", _tail(proc.stderr) or f"{backend} is not available"
    return "failed", f"bench probe exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}"


# --------------------------------------------------------------------------
# Result conversion
# --------------------------------------------------------------------------


def _ratio(value: float, better: str = "lower") -> dict:
    return make_metric(unit="ratio", better=better, value=round(float(value), 6))


def _quality_metrics(scored: dict) -> dict:
    metrics = {name: _ratio(scored[name]) for name in QUALITY_RATIOS}
    count = scored["speaker_count_error"]  # int per mix, mean (float) for an aggregate
    if isinstance(count, bool) or not isinstance(count, (int, float)):
        raise TypeError(f"speaker_count_error must be a number, got {count!r}")
    metrics["speaker_count_error"] = make_metric(
        unit="count", better="lower", value=count if isinstance(count, int) else round(float(count), 6))
    metrics["speaker_count_exact"] = _ratio(float(scored["speaker_count_exact"]), "higher")  # 1.0/0.0
    return metrics


def _score(mix_result: dict, mix: dict) -> dict:
    turns = mix_result["turns"]
    if not isinstance(turns, list):
        raise TypeError("turns must be a list")
    return der.compute_der(mix["reference"], turns, collar=COLLAR_S)


def mix_case(entry: dict, report: dict, mix_result: dict, mix: dict, *, scored: dict | None = None) -> dict:
    """One ``ok`` mix case; raises ``KeyError``/``TypeError``/``ValueError`` on a malformed result."""
    scored = scored if scored is not None else _score(mix_result, mix)
    metrics = _quality_metrics(scored)
    evidence = {
        "ref_speakers": scored["ref_speakers"],
        "hyp_speakers": scored["hyp_speakers"],
        "mapping": scored["mapping"],
        "scored_s": round(scored["scored_s"], 3),
        "missed_s": round(scored["missed_s"], 3),
        "false_alarm_s": round(scored["false_alarm_s"], 3),
        "confusion_s": round(scored["confusion_s"], 3),
        "collar": scored["collar"],
        "unlabelled_turns": int(mix_result.get("unlabelled_turns", 0)),
        "reference_rttm": der.to_rttm(mix["reference"], mix["id"]),
        "hypothesis_rttm": der.to_rttm(mix_result["turns"], mix["id"]),
        "vad": mix.get("vad"),
    }
    if _is_mock(entry):
        evidence["median_ms"] = float(mix_result["median_ms"])
    else:
        metrics["latency_ms"] = make_metric(unit="ms", better="lower",
                                            samples=[float(x) for x in mix_result["latencies_ms"]])
        metrics["first_ms"] = make_metric(unit="ms", better="lower", value=float(mix_result["first_ms"]))
        metrics["rtf"] = make_metric(unit="ratio", better="lower", value=float(mix_result["real_time_factor"]))
    return make_case(suite=SUITE, case=case_name(entry, mix["id"]), dims=_dims(entry, mix["id"], "mix"),
                     metrics=metrics, evidence=evidence)


def aggregate_case(entry: dict, report: dict, scored: list[dict]) -> dict:
    """The ``…/all`` case; ``scored`` items are ``{"id", "der", "median_ms", "audio_seconds"}``."""
    pooled = der.aggregate([s["der"] for s in scored])
    metrics = _quality_metrics(pooled)
    evidence = {
        "mixes": [s["id"] for s in scored],
        "scored_s": round(pooled["scored_s"], 3),
        "missed_s": round(pooled["missed_s"], 3),
        "false_alarm_s": round(pooled["false_alarm_s"], 3),
        "confusion_s": round(pooled["confusion_s"], 3),
        "collar": COLLAR_S,
        "threshold": report.get("threshold"),
        "cluster_threshold": report.get("cluster_threshold"),
        "max_speakers": report.get("max_speakers"),
        "chunk_seconds": report.get("chunk_seconds"),
        "num_threads": report.get("num_threads"),
        "segmentation_model": report.get("segmentation_model"),
        "embedding_model": report.get("embedding_model"),
    }
    if not _is_mock(entry):
        audio_s = sum(float(s["audio_seconds"]) for s in scored)
        if audio_s <= 0:
            raise ValueError("aggregate audio_seconds must be > 0")
        metrics["rtf"] = make_metric(unit="ratio", better="lower",
                                     value=sum(float(s["median_ms"]) for s in scored) / (audio_s * 1000.0))
        metrics["load_ms"] = make_metric(unit="ms", better="lower", value=float(report["load_ms"]))
        for name, key in (("rss_before", "rss_before_bytes"), ("rss_loaded", "rss_loaded_bytes"),
                          ("rss_peak", "rss_peak_bytes"), ("rss_delta_bytes", "rss_delta_bytes")):
            metrics[name] = make_metric(unit="bytes", better="lower", value=int(report[key]))
    return make_case(suite=SUITE, case=case_name(entry, "all"), dims=_dims(entry, "all", "aggregate"),
                     metrics=metrics, evidence=evidence)


def cases_from_report(entry: dict, report: dict, mixes: list[dict]) -> list[dict]:
    """One case per expected mix plus the aggregate, from a bench report."""
    results = report.get("mixes") if isinstance(report, dict) else None
    if not isinstance(results, list):
        return _non_ok(entry, [m["id"] for m in mixes], "failed", "bench report must hold a 'mixes' list")
    by_id = {r.get("id"): r for r in results if isinstance(r, dict)}
    cases: list[dict] = []
    scored: list[dict] = []
    for mix in mixes:
        found = by_id.get(mix["id"])
        if found is None:
            cases += _non_ok(entry, [mix["id"]], "failed", "bench report has no result for this mix")[:1]
            continue
        try:
            result = _score(found, mix)
            cases.append(mix_case(entry, report, found, mix, scored=result))
            scored.append({"id": mix["id"], "der": result, "median_ms": found["median_ms"],
                           "audio_seconds": found["audio_seconds"]})
        except (KeyError, TypeError, ValueError) as exc:
            reason = f"malformed bench result: {type(exc).__name__}: {exc}"
            cases += _non_ok(entry, [mix["id"]], "failed", reason)[:1]
    agg_name, agg_dims = case_name(entry, "all"), _dims(entry, "all", "aggregate")
    if len(scored) != len(mixes):
        cases.append(make_case(suite=SUITE, case=agg_name, dims=agg_dims, status="failed",
                               error="aggregate needs every mix"))
        return cases
    try:
        cases.append(aggregate_case(entry, report, scored))
    except (KeyError, TypeError, ValueError) as exc:
        cases.append(make_case(suite=SUITE, case=agg_name, dims=agg_dims, status="failed",
                               error=f"malformed bench result: {type(exc).__name__}: {exc}"))
    return cases


def run_entry(ctx: RunContext, entry: dict, mixes: list[dict], work: Path) -> list[dict]:
    mix_ids = [m["id"] for m in mixes]
    slug = case_name(entry, "x").replace("/", "_")
    manifest = work / f"{slug}.manifest.json"
    out = work / f"{slug}.out.json"
    manifest.write_text(
        json.dumps({"mixes": [{"id": m["id"], "path": str(Path(m["path"]).resolve())} for m in mixes]}),
        encoding="utf-8",
    )
    proc, error = _run(ctx, bench_command(ctx, entry, manifest, out))
    if proc is None:
        return _non_ok(entry, mix_ids, "failed", error)
    if proc.returncode == EXIT_UNAVAILABLE:
        return _non_ok(entry, mix_ids, "not_exercised", _tail(proc.stderr) or f"{entry['backend']} not available")
    if proc.returncode != 0:
        reason = f"bench exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}"
        return _non_ok(entry, mix_ids, "failed", reason)
    try:
        report = json.loads(out.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return _non_ok(entry, mix_ids, "failed", f"could not read bench JSON: {exc}")
    return cases_from_report(entry, report, mixes)


def _planned_mix_ids(manifest: dict, entry: dict) -> list[str]:
    if entry["mixes"] == "synthetic":
        return [SYNTHETIC_MIX_ID]
    return [m["id"] for m in manifest["mixes"] if m["smoke"] or entry["mixes"] == "all"]


@register_suite(SUITE)
def run_diarization(ctx: RunContext) -> list[dict]:
    opts = ctx.options
    plan = plan_diarization_matrix(ctx.tier, ctx.runtime)
    manifest = load_diarization_manifest(Path(opts.get("manifest") or DEFAULT_DIARIZATION_MANIFEST_PATH))
    ids = [_planned_mix_ids(manifest, e) for e in plan]

    by_entry: list[list[dict] | None] = [None] * len(plan)
    for idx, entry in enumerate(plan):
        if entry["not_exercised"]:
            by_entry[idx] = _non_ok(entry, ids[idx], "not_exercised", entry["not_exercised"])
    pending = [idx for idx, cases in enumerate(by_entry) if cases is None]
    if not pending:
        return [case for cases in by_entry for case in cases or []]

    with tempfile.TemporaryDirectory(prefix="lf-perf-diarization-") as tmp:
        work = Path(tmp)
        mock = [idx for idx in pending if _is_mock(plan[idx])]
        if mock:
            synthetic = write_synthetic_mix(work / f"{SYNTHETIC_MIX_ID}.wav")
            for idx in mock:
                by_entry[idx] = run_entry(ctx, plan[idx], [synthetic], work)

        real = [idx for idx in pending if not _is_mock(plan[idx])]
        for backend in dict.fromkeys(plan[idx]["backend"] for idx in real):
            group = [idx for idx in real if plan[idx]["backend"] == backend]
            status, reason = probe_backend(ctx, backend)  # once, before any download
            if status != "ok":
                for idx in group:
                    by_entry[idx] = _non_ok(plan[idx], ids[idx], status, reason)
                continue
            wanted = list(dict.fromkeys(mid for idx in group for mid in ids[idx]))
            by_mix = {m["id"]: m for m in manifest["mixes"]}
            clip_ids = list(dict.fromkeys(cid for mid in wanted for cid in by_mix[mid]["clips"]))
            try:
                resolved = ensure_stt_clips(
                    manifest, Path(opts.get("cache_dir") or default_cache_dir()), clip_ids=clip_ids
                )
                entries = {c["id"]: c for c in manifest["clips"]}
                clips = {c["id"]: {**entries[c["id"]], **c} for c in resolved}
                built = {mid: build_mix(by_mix[mid], clips, work / f"{mid}.wav") for mid in wanted}
            except Exception as exc:  # noqa: BLE001 - network/ffmpeg/digest/WAV: report, never crash
                reason = f"diarization corpus unavailable: {type(exc).__name__}: {exc}"
                for idx in group:
                    by_entry[idx] = _non_ok(plan[idx], ids[idx], "failed", reason)
                continue
            for idx in group:
                by_entry[idx] = run_entry(ctx, plan[idx], [built[mid] for mid in ids[idx]], work)
    return [case for cases in by_entry for case in cases or []]
