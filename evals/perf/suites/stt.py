"""``stt`` suite: speech-to-text speed, memory and WER for both runtimes.

Stdlib only; never imports ``localflow``. The suite runs a bench in a
subprocess (Rust ``stt_bench --manifest`` or Python ``benches/py_stt.py
--manifest``, which share flags and JSON), then scores every transcript here
with :func:`evals.perf.wer.compute_wer` so both runtimes are judged by the
same code (``docs/plans/PLAN-perf-evals.md`` §5.1).

Every planned (engine, model, provider) × clip appears as a case: ``ok``,
``failed`` (bench crashed) or ``not_exercised`` (no binary, dependency not
installed, or a path the runtime does not have, e.g. Rust CUDA).

``RunContext.options`` (all optional): ``os`` (override detected OS),
``cache_dir`` (corpus cache), ``manifest`` (corpus manifest path),
``bench_bin`` (Rust binary), ``python`` (interpreter for the Python bench),
``timeout_s`` (per bench subprocess).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from ..corpus.download import (
    DEFAULT_MANIFEST_PATH,
    default_cache_dir,
    ensure_stt_clips,
    load_stt_manifest,
)
from ..registry import RunContext, register_suite
from ..schema import make_case, make_metric, normalize_os
from ..wer import compute_wer

SUITE = "stt"
SMOKE_MODELS = ("tiny.en",)
NIGHTLY_MODELS = ("tiny.en", "base.en", "small")
DEEP_EXTRA_MODELS = ("medium", "large-v3")
#: MLX Whisper builds exist only for multilingual sizes (no ``.en``); asking
#: Transcriber for ``tiny.en`` on MLX silently falls back to faster-whisper.
MLX_NIGHTLY_MODELS = ("tiny", "base", "small")
PARAKEET_MODEL = "base"
RUST_CUDA_REASON = "Rust has no CUDA STT path"
BUILD_HINT = "cargo build --release -p lf-stt --example stt_bench"
MISSING_BIN_REASON = (
    f"Rust stt_bench binary not found; build it with `{BUILD_HINT}` "
    "(in rust/) or set LF_PERF_STT_BENCH_BIN"
)
EXIT_UNAVAILABLE = 3  # py_stt.py: an engine dependency is not installed
DEFAULT_TIMEOUT_S = 3600
ERROR_TAIL = 800

PY_BENCH = Path(__file__).resolve().parents[1] / "benches" / "py_stt.py"


# --------------------------------------------------------------------------
# Matrix
# --------------------------------------------------------------------------


def _entry(engine: str, model: str, provider: str, *, runs: int, warmup: int, smoke_only: bool,
           not_exercised: str | None = None) -> dict:
    return {
        "engine": engine,
        "model": model,
        "provider": provider,
        "runs": runs,
        "warmup": warmup,
        "smoke_only": smoke_only,
        "not_exercised": not_exercised,
    }


def plan_stt_matrix(tier: str, runtime: str, os_name: str) -> list[dict]:
    """The (engine, model, provider) entries to bench for one tier/runtime/OS.

    Pure function. Entries with ``not_exercised`` set are reported, not run.
    """
    if runtime not in ("python", "rust"):
        raise ValueError(f"unknown runtime {runtime!r}")
    if tier not in ("smoke", "nightly", "deep"):
        raise ValueError(f"unknown tier {tier!r}")
    os_name = normalize_os(os_name)
    whisper = "faster-whisper" if runtime == "python" else "whisper-cpp"
    if tier == "smoke":
        return [_entry(whisper, m, "cpu", runs=3, warmup=1, smoke_only=True) for m in SMOKE_MODELS]

    deep = tier == "deep"
    models = NIGHTLY_MODELS + (DEEP_EXTRA_MODELS if deep else ())
    mlx_models = MLX_NIGHTLY_MODELS + (DEEP_EXTRA_MODELS if deep else ())
    kw = {"runs": 5, "warmup": 1, "smoke_only": False}
    plan = [_entry(whisper, m, "cpu", **kw) for m in models]
    if runtime == "rust" and os_name != "windows":
        plan.append(_entry("parakeet-onnx", PARAKEET_MODEL, "cpu", **kw))
    if os_name == "macos":
        if runtime == "rust":
            plan += [_entry("whisper-cpp", m, "metal", **kw) for m in models]
            plan.append(_entry("parakeet-onnx", PARAKEET_MODEL, "coreml", **kw))
        else:
            plan += [_entry("mlx-whisper", m, "mlx", **kw) for m in mlx_models]
    if deep and os_name != "macos":
        if runtime == "python":
            plan += [_entry("faster-whisper", m, "cuda", **kw) for m in models]
        else:
            plan += [_entry("whisper-cpp", m, "cuda", **kw, not_exercised=RUST_CUDA_REASON) for m in models]
    return plan


def case_name(entry: dict, clip_id: str) -> str:
    return f"{entry['engine']}-{entry['model']}-{entry['provider']}/{clip_id}"


def _dims(entry: dict, clip_id: str) -> dict:
    return {"engine": entry["engine"], "model": entry["model"], "provider": entry["provider"], "clip": clip_id}


def _non_ok(entry: dict, clip_ids: list[str], status: str, reason: str) -> list[dict]:
    return [
        make_case(suite=SUITE, case=case_name(entry, cid), dims=_dims(entry, cid), status=status, error=reason)
        for cid in clip_ids
    ]


# --------------------------------------------------------------------------
# Bench discovery and invocation
# --------------------------------------------------------------------------


def find_rust_bench(repo_root: Path, override: str | None = None) -> Path | None:
    """``override`` / ``LF_PERF_STT_BENCH_BIN``, else release then debug build."""
    explicit = override or os.environ.get("LF_PERF_STT_BENCH_BIN")
    if explicit:
        path = Path(explicit)
        return path.resolve() if path.is_file() else None
    exe = "stt_bench.exe" if sys.platform == "win32" else "stt_bench"
    for profile in ("release", "debug"):
        path = repo_root / "rust" / "target" / profile / "examples" / exe
        if path.is_file():
            return path
    return None


def bench_command(ctx: RunContext, entry: dict, manifest: Path, out: Path) -> list[str] | str:
    """The bench argv, or a ``not_exercised`` reason string."""
    if ctx.runtime == "rust":
        binary = find_rust_bench(ctx.repo_root, ctx.options.get("bench_bin"))
        if binary is None:
            return MISSING_BIN_REASON
        cmd = [str(binary)]
    else:
        python = ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable
        cmd = [str(python), str(PY_BENCH)]
    return cmd + [
        "--manifest", str(manifest),
        "--engines", entry["engine"],
        "--model", entry["model"],
        "--provider", entry["provider"],
        "--runs", str(entry["runs"]),
        "--warmup", str(entry["warmup"]),
        "--json", str(out),
    ]


def _bench_env(ctx: RunContext) -> dict:
    env = dict(os.environ)
    if ctx.runtime == "python":  # let the bench import localflow from this checkout
        root = str(ctx.repo_root)
        env["PYTHONPATH"] = os.pathsep.join(p for p in (root, env.get("PYTHONPATH")) if p)
    return env


def _tail(text: str | bytes | None) -> str:
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    text = (text or "").strip()
    return text[-ERROR_TAIL:] if len(text) > ERROR_TAIL else text


# --------------------------------------------------------------------------
# Result conversion
# --------------------------------------------------------------------------


def _ms(value: float) -> dict:
    return make_metric(unit="ms", better="lower", value=float(value))


def _bytes(value: int) -> dict:
    return make_metric(unit="bytes", better="lower", value=int(value))


def clip_case(entry: dict, engine_result: dict, clip_result: dict, reference: str) -> dict:
    """One ``ok`` case from a bench engine result and one of its clips."""
    cid = clip_result["id"]
    transcript = str(clip_result.get("transcript", ""))
    metrics = {
        "download_ms": _ms(engine_result["download_ms"]),
        "load_ms": _ms(engine_result["load_ms"]),
        "first_ms": _ms(clip_result["first_ms"]),
        "latency_ms": make_metric(unit="ms", better="lower", samples=[float(x) for x in clip_result["latencies_ms"]]),
        "rtf": make_metric(unit="ratio", better="lower", value=float(clip_result["real_time_factor"])),
        "rss_before": _bytes(engine_result["rss_before_bytes"]),
        "rss_loaded": _bytes(engine_result["rss_after_bytes"]),
        "rss_after_unload": _bytes(engine_result["rss_after_unload_bytes"]),
        "wer": make_metric(unit="ratio", better="lower", value=compute_wer(reference, transcript)),
    }
    return make_case(
        suite=SUITE,
        case=case_name(entry, cid),
        dims=_dims(entry, cid),
        metrics=metrics,
        evidence={"transcript": transcript, "effective_provider": engine_result.get("effective_provider")},
    )


def cases_from_report(entry: dict, report: dict, clips: list[dict]) -> list[dict]:
    """Convert a bench ``--manifest`` report into one case per expected clip."""
    results = report.get("results") if isinstance(report, dict) else None
    if not isinstance(results, list) or len(results) != 1:
        return _non_ok(entry, [c["id"] for c in clips], "failed", "bench report must hold exactly one engine result")
    engine_result = results[0]
    requested = str(entry.get("provider") or "").lower()
    effective = str(engine_result.get("effective_provider") or "").lower()
    provider_matched = effective == requested or (
        effective.startswith(f"{requested} ") and "fallback" not in effective
    )
    if requested and requested != "auto" and effective and not provider_matched:
        return _non_ok(
            entry,
            [c["id"] for c in clips],
            "not_exercised",
            f"requested provider {entry['provider']!r} fell back to {engine_result.get('effective_provider')!r}",
        )
    by_id = {c.get("id"): c for c in engine_result.get("clips") or [] if isinstance(c, dict)}
    cases = []
    for clip in clips:
        found = by_id.get(clip["id"])
        if found is None:
            cases += _non_ok(entry, [clip["id"]], "failed", "bench report has no result for this clip")
            continue
        try:
            cases.append(clip_case(entry, engine_result, found, clip["reference"]))
        except (KeyError, TypeError, ValueError) as exc:
            cases += _non_ok(entry, [clip["id"]], "failed", f"malformed bench result: {type(exc).__name__}: {exc}")
    return cases


def run_entry(ctx: RunContext, entry: dict, clips: list[dict], work: Path) -> list[dict]:
    clip_ids = [c["id"] for c in clips]
    slug = case_name(entry, "x").replace("/", "_")
    manifest = work / f"{slug}.manifest.json"
    out = work / f"{slug}.out.json"
    manifest.write_text(
        json.dumps({"clips": [{"id": c["id"], "path": str(Path(c["path"]).resolve())} for c in clips]}),
        encoding="utf-8",
    )
    cmd = bench_command(ctx, entry, manifest, out)
    if isinstance(cmd, str):
        return _non_ok(entry, clip_ids, "not_exercised", cmd)
    timeout = float(ctx.options.get("timeout_s", DEFAULT_TIMEOUT_S))
    try:
        proc = subprocess.run(
            cmd, cwd=str(ctx.repo_root), env=_bench_env(ctx), capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, check=False,
        )
    except subprocess.TimeoutExpired as exc:
        return _non_ok(entry, clip_ids, "failed", f"bench timed out after {timeout:g} s: {_tail(exc.stderr)}".strip())
    except OSError as exc:
        return _non_ok(entry, clip_ids, "failed", f"could not start bench: {exc}")
    if proc.returncode == EXIT_UNAVAILABLE and ctx.runtime == "python":
        return _non_ok(entry, clip_ids, "not_exercised", _tail(proc.stderr) or "engine not installed")
    if proc.returncode != 0:
        reason = f"bench exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}"
        return _non_ok(entry, clip_ids, "failed", reason)
    try:
        report = json.loads(out.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return _non_ok(entry, clip_ids, "failed", f"could not read bench JSON: {exc}")
    return cases_from_report(entry, report, clips)


def _planned_clips(manifest: dict, smoke_only: bool) -> list[dict]:
    return [c for c in manifest["clips"] if c.get("smoke") or not smoke_only]


@register_suite(SUITE)
def run_stt(ctx: RunContext) -> list[dict]:
    opts = ctx.options
    os_name = opts.get("os") or sys.platform
    plan = plan_stt_matrix(ctx.tier, ctx.runtime, os_name)
    manifest = load_stt_manifest(Path(opts.get("manifest") or DEFAULT_MANIFEST_PATH))
    smoke_only = all(e["smoke_only"] for e in plan)
    planned = _planned_clips(manifest, smoke_only)
    clip_ids = [c["id"] for c in planned]

    missing_bin = ctx.runtime == "rust" and find_rust_bench(ctx.repo_root, opts.get("bench_bin")) is None

    def ids(entry: dict) -> list[str]:
        return [c["id"] for c in _planned_clips(manifest, entry["smoke_only"])]

    # Report in plan order; fill skipped entries first so nothing is downloaded
    # when no entry can actually run.
    by_entry: list[list[dict] | None] = [None] * len(plan)
    for idx, entry in enumerate(plan):
        if entry["not_exercised"]:
            by_entry[idx] = _non_ok(entry, ids(entry), "not_exercised", entry["not_exercised"])
        elif missing_bin:
            by_entry[idx] = _non_ok(entry, ids(entry), "not_exercised", MISSING_BIN_REASON)
    pending = [idx for idx, cases in enumerate(by_entry) if cases is None]
    if pending:
        try:
            resolved = ensure_stt_clips(
                manifest, Path(opts.get("cache_dir") or default_cache_dir()), clip_ids=clip_ids
            )
        except Exception as exc:  # noqa: BLE001 - network/ffmpeg/digest: report, never crash
            reason = f"STT corpus unavailable: {type(exc).__name__}: {exc}"
            for idx in pending:
                by_entry[idx] = _non_ok(plan[idx], ids(plan[idx]), "failed", reason)
        else:
            with tempfile.TemporaryDirectory(prefix="lf-perf-stt-") as tmp:
                for idx in pending:
                    entry = plan[idx]
                    wanted = [c for c in resolved if c["smoke"] or not entry["smoke_only"]]
                    by_entry[idx] = run_entry(ctx, entry, wanted, Path(tmp))
    return [case for cases in by_entry for case in cases or []]
