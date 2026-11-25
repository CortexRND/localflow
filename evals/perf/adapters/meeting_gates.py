"""Adapter: ``evals/meeting_sim`` gates + driver results → ``meetings`` cases.

Inputs (``docs/plans/PLAN-perf-evals.md`` §5.3):

* ``gates.json`` written by ``evals/meeting_sim/gates.py``:
  ``{"per_repeat": {"r1": {"M1": {"value", "k", "n", "pass", "fail"}, ...}},
  "summary": {"M1": {"status", "pass_count", "pooled_k", "pooled_n", ...}},
  "timings": {...}}`` (``timings`` only from the configurable gates.py).
* optionally the driver results ``<run>/rK/results/<scenario>.json``; their
  ``timings`` dict (or, for older files, the ``http``/``llm_calls`` ``secs``)
  supplies the latency samples.

Output:

* one case per gate, ``meetings/gate/M1`` … ``meetings/gate/M14``, with
  metric ``M1`` … ``M14`` (unit ``ratio``; the names the ``policy.toml``
  rules and caps use). ``value`` is the pooled rate over all repeats,
  ``k / n``; for ``M7`` it is the *unsupported*-number rate ``(n - k) / n``
  (lower is better), as ``gates.py`` reports it. ``evidence`` holds ``k``,
  ``n``, the Wilson 95 % interval of that rate, the ``gates.py`` verdict,
  per-repeat values and the failing items. A gate with ``n == 0`` is
  ``not_exercised``.
* one timings case ``meetings/timings/<runtime>`` with ``stop_to_notes_ms``,
  ``chat_ms``, ``llm_call_ms`` (ms samples) and ``llm_calls_per_meeting``
  (count samples), all lower-is-better.

Unreadable or malformed input becomes ``failed`` cases with a reason, never
an exception. Stdlib only; never imports ``localflow``.
"""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any

from ..schema import make_case, make_metric

SUITE = "meetings"
GATES = tuple(f"M{i}" for i in range(1, 15))
TIMING_METRICS = ("stop_to_notes_ms", "chat_ms", "llm_call_ms", "llm_calls_per_meeting")
MAX_FAILURES = 50  # failing items kept per gate in evidence

#: gate -> (short name, policy class, gates.py threshold or None for "all").
GATE_INFO: dict[str, tuple[str, str, float | None]] = {
    "M1": ("lifecycle", "exact", None),
    "M2": ("attribution", "exact", None),
    "M3": ("raw_notes", "exact", None),
    "M4": ("structure", "exact", None),
    "M5": ("user_point_coverage", "quality", 0.90),
    "M6": ("fact_recall", "quality", 0.70),
    "M7": ("unsupported_numbers", "quality", 0.05),
    "M8": ("template_fidelity", "quality", 1.0),
    "M9": ("chat_answerable", "quality", 0.80),
    "M10": ("chat_abstention", "quality", 0.80),
    "M11": ("notes_only", "exact", None),
    "M12": ("mic_only_fallback", "exact", None),
    "M13": ("restart_chat", "exact", None),
    "M14": ("speaker_labels", "exact", None),
}
LOWER_IS_BETTER = frozenset({"M7"})


def gate_case_name(gate: str) -> str:
    return f"meetings/gate/{gate}"


def timings_case_name(runtime: str) -> str:
    return f"meetings/timings/{runtime}"


def gate_dims(gate: str, runtime: str) -> dict:
    name, cls, _ = GATE_INFO[gate]
    return {"kind": "gate", "gate": gate, "check": name, "class": cls, "runtime": runtime}


def timings_dims(runtime: str) -> dict:
    return {"kind": "timings", "runtime": runtime}


def wilson(k: int, n: int, z: float = 1.96) -> tuple[float | None, float | None]:
    """Wilson score interval for ``k`` successes in ``n`` (same formula as ``gates.py``)."""
    if n <= 0:
        return (None, None)
    p = k / n
    d = 1 + z * z / n
    c = p + z * z / (2 * n)
    r = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))
    return (round(max(0.0, (c - r) / d), 3), round(min(1.0, (c + r) / d), 3))


def _is_count(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _is_ms(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
    )


def not_exercised_cases(runtime: str, reason: str) -> list[dict]:
    """Every gate case plus the timings case as ``not_exercised`` with ``reason``."""
    cases = [
        make_case(suite=SUITE, case=gate_case_name(g), dims=gate_dims(g, runtime),
                  status="not_exercised", error=reason)
        for g in GATES
    ]
    cases.append(make_case(suite=SUITE, case=timings_case_name(runtime), dims=timings_dims(runtime),
                           status="not_exercised", error=reason))
    return cases


def failed_cases(runtime: str, reason: str, *, timings: bool = True) -> list[dict]:
    cases = [
        make_case(suite=SUITE, case=gate_case_name(g), dims=gate_dims(g, runtime),
                  status="failed", error=reason)
        for g in GATES
    ]
    if timings:
        cases.append(make_case(suite=SUITE, case=timings_case_name(runtime), dims=timings_dims(runtime),
                               status="failed", error=reason))
    return cases


# --------------------------------------------------------------------------
# Gates
# --------------------------------------------------------------------------


def _per_repeat(gates: dict, gate: str) -> tuple[dict, dict]:
    """``({repeat: value}, {repeat: [failing items]})`` for one gate."""
    values: dict[str, Any] = {}
    failures: dict[str, list] = {}
    per = gates.get("per_repeat")
    if not isinstance(per, dict):
        return values, failures
    for rep in sorted(per):
        entry = per[rep].get(gate) if isinstance(per[rep], dict) else None
        if not isinstance(entry, dict):
            continue
        values[rep] = entry.get("value")
        fails = entry.get("fail")
        if isinstance(fails, list) and fails:
            failures[rep] = [str(x) for x in fails[:MAX_FAILURES]]
    return values, failures


def convert_gate(gates: dict, gate: str, *, runtime: str = "python") -> dict:
    dims = gate_dims(gate, runtime)
    name = gate_case_name(gate)
    summary = gates.get("summary") if isinstance(gates, dict) else None
    entry = summary.get(gate) if isinstance(summary, dict) else None
    if not isinstance(entry, dict):
        return make_case(suite=SUITE, case=name, dims=dims, status="failed",
                         error=f"gate {gate} missing from gates.json summary")
    k, n = entry.get("pooled_k"), entry.get("pooled_n")
    if not (_is_count(k) and _is_count(n)) or k > n:
        return make_case(suite=SUITE, case=name, dims=dims, status="failed",
                         error=f"gate {gate}: bad pooled_k/pooled_n {k!r}/{n!r}")
    verdict = str(entry.get("status") or "")
    per_values, failures = _per_repeat(gates, gate)
    threshold = GATE_INFO[gate][2]
    evidence: dict[str, Any] = {
        "k": k,
        "n": n,
        "verdict": verdict,
        "pass_count": entry.get("pass_count"),
        "per_repeat": per_values,
        "failures": failures,
        "low_power": n < 10,
        "threshold": threshold,
    }
    if n == 0:
        return make_case(suite=SUITE, case=name, dims=dims, status="not_exercised",
                         error=f"gate {gate} not exercised (n=0 across {len(per_values)} repeat(s))",
                         evidence=evidence)
    if gate in LOWER_IS_BETTER:
        bad = n - k
        evidence["unsupported"] = bad
        rate, interval = bad / n, wilson(bad, n)
    else:
        rate, interval = k / n, wilson(k, n)
    evidence["wilson95"] = list(interval)
    better = "lower" if gate in LOWER_IS_BETTER else "higher"
    return make_case(
        suite=SUITE, case=name, dims=dims,
        metrics={gate: make_metric(unit="ratio", better=better, value=round(rate, 6))},
        evidence=evidence,
    )


def convert_gates(gates: Any, *, runtime: str = "python") -> list[dict]:
    """One case per gate M1–M14 from parsed ``gates.json``."""
    if not isinstance(gates, dict) or not isinstance(gates.get("summary"), dict):
        return failed_cases(runtime, "gates.json: expected an object with a 'summary' table", timings=False)
    return [convert_gate(gates, g, runtime=runtime) for g in GATES]


# --------------------------------------------------------------------------
# Timings
# --------------------------------------------------------------------------


def driver_timings(result: Any) -> tuple[dict | None, str]:
    """``(timings, source)`` of one driver result; ``source`` is ``timings`` or ``secs``.

    Older driver results lack ``timings``; for those the 10 ms resolution
    ``secs`` of the HTTP and LLM calls are used instead.
    """
    if not isinstance(result, dict):
        return None, "invalid"
    t = result.get("timings")
    if isinstance(t, dict):
        calls = t.get("llm_call_ms")
        count = t.get("llm_calls_count")
        if not _is_count(count) and isinstance(calls, list):
            count = len(calls)
        return {
            "stop_to_notes_ms": t.get("stop_to_notes_ms", t.get("summarize_ms")),
            "chat_ms": t.get("chat_ms"),
            "llm_call_ms": calls,
            "llm_calls_per_meeting": count,
        }, "timings"
    http, llm = result.get("http"), result.get("llm_calls")
    if not isinstance(http, list) or not isinstance(llm, list):
        return None, "missing"

    def ms(entry: Any) -> float | None:
        secs = entry.get("secs") if isinstance(entry, dict) else None
        return round(secs * 1000, 3) if _is_ms(secs) else None

    stop = [ms(h) for h in http if isinstance(h, dict) and h.get("call") == "POST /meeting/stop"]
    chats = [ms(h) for h in http if isinstance(h, dict) and h.get("call") == "POST /meeting/chat"]
    return {
        "stop_to_notes_ms": stop[0] if stop else None,
        "chat_ms": chats[:-1],  # the last chat is the simulated-restart check
        "llm_call_ms": [ms(c) for c in llm],
        "llm_calls_per_meeting": len(llm),
    }, "secs"


def pooled_timings(results: list[Any]) -> tuple[dict[str, list], dict]:
    """Pool timing samples over driver results; second item is evidence."""
    samples: dict[str, list] = {m: [] for m in TIMING_METRICS}
    sources: dict[str, int] = {}
    for result in results:
        t, source = driver_timings(result)
        sources[source] = sources.get(source, 0) + 1
        if t is None:
            continue
        if _is_ms(t["stop_to_notes_ms"]):
            samples["stop_to_notes_ms"].append(float(t["stop_to_notes_ms"]))
        for key in ("chat_ms", "llm_call_ms"):
            if isinstance(t[key], list):
                samples[key].extend(float(x) for x in t[key] if _is_ms(x))
        if _is_count(t["llm_calls_per_meeting"]):
            samples["llm_calls_per_meeting"].append(t["llm_calls_per_meeting"])
    return samples, {"meetings": len(results), "timing_sources": sources}


def _samples_from_gates(gates: Any) -> dict[str, list] | None:
    t = gates.get("timings") if isinstance(gates, dict) else None
    if not isinstance(t, dict):
        return None
    out: dict[str, list] = {}
    for key in TIMING_METRICS:
        vals = t.get(key)
        out[key] = [x for x in vals if _is_ms(x)] if isinstance(vals, list) else []
    return out


def convert_timings(
    results: list[Any] | None = None,
    *,
    gates: Any = None,
    runtime: str = "python",
    evidence: dict | None = None,
) -> dict:
    """The ``meetings/timings/<runtime>`` case from driver results (else gates.json ``timings``)."""
    extra = dict(evidence or {})
    samples: dict[str, list] | None = None
    if results:
        samples, ev = pooled_timings(results)
        extra.update(ev)
        extra["source"] = "driver_results"
    if not samples or not any(samples.values()):
        from_gates = _samples_from_gates(gates)
        if from_gates and any(from_gates.values()):
            samples = from_gates
            extra["source"] = "gates.json"
    name, dims = timings_case_name(runtime), timings_dims(runtime)
    if not samples or not any(samples.values()):
        return make_case(suite=SUITE, case=name, dims=dims, status="not_exercised",
                         error="no meeting timings in driver results or gates.json", evidence=extra)
    metrics = {}
    for key in TIMING_METRICS:
        if samples.get(key):
            unit = "count" if key == "llm_calls_per_meeting" else "ms"
            metrics[key] = make_metric(unit=unit, better="lower", samples=samples[key])
    extra["missing_metrics"] = [k for k in TIMING_METRICS if k not in metrics]
    return make_case(suite=SUITE, case=name, dims=dims, metrics=metrics, evidence=extra)


# --------------------------------------------------------------------------
# Files
# --------------------------------------------------------------------------


def read_results(run_dir: Path) -> tuple[list[dict], list[str]]:
    """Parsed ``r*/results/*.json`` under ``run_dir`` plus a list of unreadable files."""
    results: list[dict] = []
    bad: list[str] = []
    for path in sorted(Path(run_dir).glob("r*/results/*.json")):
        rel = path.relative_to(run_dir).as_posix()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            bad.append(f"{rel}: {type(exc).__name__}")
            continue
        if isinstance(data, dict):
            results.append(data)
        else:
            bad.append(f"{rel}: not an object")
    return results, bad


def load_run_dir(
    run_dir: Path | str,
    *,
    runtime: str = "python",
    gates_path: Path | str | None = None,
    evidence: dict | None = None,
) -> list[dict]:
    """Cases from a meeting-sim run directory (``gates.json`` + driver results)."""
    run_dir = Path(run_dir)
    gpath = Path(gates_path) if gates_path else run_dir / "gates.json"
    results, bad = read_results(run_dir) if run_dir.is_dir() else ([], [])
    gates: Any = None
    try:
        gates = json.loads(gpath.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        cases = failed_cases(runtime, f"cannot read {gpath.name}: {type(exc).__name__}: {exc}", timings=False)
    else:
        cases = convert_gates(gates, runtime=runtime)
    ev = dict(evidence or {})
    if bad:
        ev["unreadable_results"] = bad
    cases.append(convert_timings(results, gates=gates, runtime=runtime, evidence=ev))
    return cases
