"""Nightly trend store (docs/plans/PLAN-perf-evals.md §8). Stdlib only.

Layout of a store directory (the orphan ``perf-data`` branch in CI)::

    runs/<YYYY-MM-DD>-<short_sha>-<os>-<runtime>.json   full lf-perf-result/v1
    index.jsonl                                           one line per (run, case)
    trend.md                                              rendered by render_trend

``index.jsonl`` line (compact JSON)::

    {"ts", "date", "sha", "os", "runner_label", "runtime", "tier", "suite",
     "case", "status", "metrics": {name: {value, median, p95, unit, better}}}

``value`` is the comparison scalar (median when samples exist, else value),
the same one ``compare.primary_value`` uses.

Regression detection reuses ``compare.compare_results`` and ``policy.toml``:
each point of a ``(runner, runtime, suite, case, metric)`` series is compared
against a rolling healthy baseline (median of the last healthy points).
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

from .compare import DEFAULT_POLICY_PATH, compare_results, load_policy, primary_value
from .schema import (
    compute_median,
    load_result,
    make_case,
    make_result,
    make_run,
    validate_result,
)

INDEX_NAME = "index.jsonl"
RUNS_DIR = "runs"
TREND_NAME = "trend.md"
SHORT_SHA = 12
DEFAULT_LIMIT = 30
DEFAULT_CONSECUTIVE = 2
HEALTHY_WINDOW = 5
RUNTIMES = ("python", "rust")
SPARK = "▁▂▃▄▅▆▇█"
ISSUE_MARKER_PREFIX = "lf-perf-regression"

_SAFE = re.compile(r"[^A-Za-z0-9._-]+")


# --------------------------------------------------------------------------
# Append
# --------------------------------------------------------------------------


def _safe(text: Any, fallback: str) -> str:
    cleaned = _SAFE.sub("-", str(text or "")).strip("-.")
    return cleaned or fallback


def _load_input(result: Path | str | dict) -> dict:
    if isinstance(result, dict):
        data = result
    else:
        data = load_result(Path(result))
    errors = validate_result(data)
    if errors:
        raise ValueError("not a valid lf-perf-result/v1:\n  " + "\n  ".join(errors))
    return data


def _index_metric(metric: dict) -> dict | None:
    try:
        value = primary_value(metric)
    except (TypeError, ValueError):
        return None
    if value is None:
        return None
    return {
        "value": value,
        "median": metric.get("median"),
        "p95": metric.get("p95"),
        "unit": metric.get("unit"),
        "better": metric.get("better"),
    }


def index_lines(result: dict) -> list[dict]:
    """One index entry per case of ``result``."""
    run = result["run"]
    ts = str(run.get("started_at") or "")
    runner = run.get("runner") or {}
    lines = []
    for case in result["cases"]:
        metrics = {}
        if case.get("status") == "ok":
            for name, metric in sorted((case.get("metrics") or {}).items()):
                entry = _index_metric(metric) if isinstance(metric, dict) else None
                if entry is not None:
                    metrics[name] = entry
        lines.append({
            "ts": ts,
            "date": ts[:10],
            "sha": run.get("sha"),
            "os": run.get("os"),
            "runner_label": runner.get("label") or run.get("os"),
            "runtime": run.get("runtime"),
            "tier": run.get("tier"),
            "suite": case["suite"],
            "case": case["case"],
            "dims": case.get("dims") or {},
            "status": case["status"],
            "metrics": metrics,
        })
    return lines


def _dedup_key(line: dict) -> tuple:
    return (
        line.get("sha"), line.get("os"), line.get("runner_label"), line.get("runtime"),
        line.get("tier"), line.get("suite"), line.get("case"), line.get("ts"),
    )


def read_index(index_path: Path | str) -> list[dict]:
    """Parse ``index.jsonl``; blank lines are skipped, bad lines raise ``ValueError``."""
    path = Path(index_path)
    if not path.is_file():
        return []
    entries = []
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        try:
            entry = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ValueError(f"{path}:{lineno}: invalid JSON: {exc}") from exc
        if not isinstance(entry, dict):
            raise TypeError(f"{path}:{lineno}: expected an object")
        entries.append(entry)
    return entries


def _run_file(runs_dir: Path, result: dict) -> tuple[Path, bool]:
    """Pick ``runs/<date>-<sha>-<os>-<runtime>[-N].json``; ``True`` if already stored.

    The same run (same ``run.id`` and ``started_at``) maps onto its existing
    file (idempotent rewrite); a different run on the same date/sha/os/runtime
    gets a ``-2``, ``-3`` ... suffix instead of clobbering the first.
    """
    run = result["run"]
    ts = str(run.get("started_at") or "")
    stem = "-".join((
        _safe(ts[:10], "undated"),
        _safe(str(run.get("sha") or "")[:SHORT_SHA], "nosha"),
        _safe(run.get("os"), "noos"),
        _safe(run.get("runtime"), "noruntime"),
    ))
    identity = (run.get("id"), run.get("started_at"))
    n = 1
    while True:
        path = runs_dir / (f"{stem}.json" if n == 1 else f"{stem}-{n}.json")
        if not path.exists():
            return path, False
        try:
            other = json.loads(path.read_text(encoding="utf-8")).get("run") or {}
        except (OSError, ValueError, AttributeError):
            other = {}
        if (other.get("id"), other.get("started_at")) == identity:
            return path, True
        n += 1


def append_run(store_dir: Path | str, result: Path | str | dict) -> dict:
    """Store one result under ``store_dir`` and append its cases to ``index.jsonl``.

    Returns ``{"run_file", "replaced", "appended", "skipped"}``.
    """
    data = _load_input(result)
    store = Path(store_dir)
    runs_dir = store / RUNS_DIR
    runs_dir.mkdir(parents=True, exist_ok=True)
    run_file, replaced = _run_file(runs_dir, data)
    tmp = run_file.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(run_file)

    index = store / INDEX_NAME
    seen = {_dedup_key(e) for e in read_index(index)}
    new = []
    skipped = 0
    for line in index_lines(data):
        key = _dedup_key(line)
        if key in seen:
            skipped += 1
            continue
        seen.add(key)
        new.append(json.dumps(line, sort_keys=True, separators=(",", ":"), ensure_ascii=True))
    if new:
        prefix = ""
        if index.is_file() and index.stat().st_size:
            with index.open("rb") as fh:
                fh.seek(-1, 2)
                prefix = "" if fh.read(1) == b"\n" else "\n"
        with index.open("a", encoding="utf-8", newline="\n") as fh:
            fh.write(prefix + "\n".join(new) + "\n")
    return {
        "run_file": str(run_file.relative_to(store).as_posix()),
        "replaced": replaced,
        "appended": len(new),
        "skipped": skipped,
    }


# --------------------------------------------------------------------------
# Series helpers
# --------------------------------------------------------------------------


def _label(entry: dict) -> str:
    return str(entry.get("runner_label") or entry.get("os") or "?")


def _points(entries: list[dict], tier: str | None = None) -> dict[tuple, list[dict]]:
    """``(runner, runtime, suite, case, metric) -> [point, ...]`` ordered by ts."""
    series: dict[tuple, list[dict]] = {}
    for entry in entries:
        if tier is not None and entry.get("tier") != tier:
            continue
        if entry.get("status") != "ok":
            continue
        for name, metric in (entry.get("metrics") or {}).items():
            if not isinstance(metric, dict) or metric.get("value") is None:
                continue
            key = (_label(entry), entry.get("runtime"), entry.get("suite"), entry.get("case"), name)
            series.setdefault(key, []).append({
                "ts": str(entry.get("ts") or ""),
                "sha": entry.get("sha"),
                "value": float(metric["value"]),
                "unit": metric.get("unit"),
                "better": metric.get("better") or "lower",
                "dims": entry.get("dims") or {},
            })
    for points in series.values():
        points.sort(key=lambda p: p["ts"])
    return series


_STT_ENGINE_ALIASES = (
    ("faster-whisper-", "whisper-"),
    ("whisper-rs-", "whisper-"),
    ("sherpa-onnx-", "parakeet-"),
    ("parakeet-onnx-", "parakeet-"),
)


def case_base(case: str, runtime: str | None = None) -> str:
    """Case name normalized across runtimes (``rust/1k`` -> ``1k``, engine prefixes unified)."""
    parts = str(case).split("/")
    drop = (runtime,) if runtime else RUNTIMES
    kept = [p for p in parts if p not in drop]
    base = "/".join(kept) if kept and len(kept) < len(parts) else str(case)
    head, sep, tail = base.partition("/")
    for prefix, canonical in _STT_ENGINE_ALIASES:
        if head.startswith(prefix):
            head = canonical + head[len(prefix):]
            return f"{head}{sep}{tail}" if sep else head
    return base


def _fmt(value: float | None, unit: str | None = None) -> str:
    if value is None:
        return "—"
    if unit == "bytes":
        for scale, suffix in ((1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")):
            if abs(value) >= scale:
                return f"{value / scale:.1f} {suffix}"
        return f"{value:g} B"
    text = f"{value:.4g}" if abs(value) < 1e6 else f"{value:.3e}"
    return f"{text} {unit}" if unit and unit not in ("ratio", "bool", "count") else text


def sparkline(values: list[float]) -> str:
    if not values:
        return ""
    lo, hi = min(values), max(values)
    if hi == lo:
        return SPARK[3] * len(values)
    span = hi - lo
    return "".join(SPARK[min(len(SPARK) - 1, int((v - lo) / span * (len(SPARK) - 1) + 0.5))] for v in values)


def _delta(values: list[float], better: str) -> str:
    if len(values) < 2:
        return "—"
    prev, last = values[-2], values[-1]
    if prev == last:
        return "= 0%"
    pct = f"{(last - prev) / abs(prev) * 100:+.1f}%" if prev else f"{last - prev:+g}"
    if better == "lower":
        arrow = "🔻" if last < prev else "🔺"
    elif better == "higher":
        arrow = "🔺" if last > prev else "🔻"
    else:
        arrow = "≠"
    good = (better == "lower" and last < prev) or (better == "higher" and last > prev)
    return f"{pct} {arrow}{' (better)' if good else ' (worse)' if better != 'exact' else ''}"


def _cell(text: Any) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


# --------------------------------------------------------------------------
# Ratio
# --------------------------------------------------------------------------


def ratio_status(ratio: float, better: str, unit: str | None) -> str:
    """Rust ÷ Python verdict from the Rust point of view."""
    if better not in ("lower", "higher") or abs(ratio - 1.0) < 1e-9:
        return "same" if abs(ratio - 1.0) < 1e-9 else "differs"
    rust_better = ratio < 1.0 if better == "lower" else ratio > 1.0
    if unit == "bytes":
        good, bad = "lighter", "heavier"
    elif unit in ("ms", "s", "us") or better == "lower":
        good, bad = "faster", "slower"
    else:
        good, bad = "better", "worse"
    return good if rust_better else bad


def ratio_rows(entries: list[dict], *, tier: str | None = None) -> list[dict]:
    """Latest Rust vs latest Python value per ``(runner, suite, case_base, metric)``."""
    latest: dict[tuple, dict[str, dict]] = {}
    for (label, runtime, suite, case, metric), points in _points(entries, tier).items():
        if runtime not in RUNTIMES or not points:
            continue
        key = (label, suite, case_base(case, runtime), metric)
        slot = latest.setdefault(key, {})
        point = points[-1]
        if runtime not in slot or point["ts"] > slot[runtime]["ts"]:
            slot[runtime] = {**point, "case": case}
    rows = []
    for (label, suite, base, metric), slot in sorted(latest.items()):
        if "rust" not in slot or "python" not in slot:
            continue
        rust, py = slot["rust"], slot["python"]
        better = rust["better"] or py["better"] or "lower"
        unit = rust["unit"] or py["unit"]
        ratio = rust["value"] / py["value"] if py["value"] else None
        rows.append({
            "os": label,
            "suite": suite,
            "case": base,
            "metric": metric,
            "unit": unit,
            "better": better,
            "rust": rust["value"],
            "python": py["value"],
            "rust_case": rust["case"],
            "python_case": py["case"],
            "ratio": ratio,
            "status": ratio_status(ratio, better, unit) if ratio is not None else "n/a",
        })
    return rows


# --------------------------------------------------------------------------
# Render
# --------------------------------------------------------------------------


def render_trend(index_path: Path | str, *, limit: int = DEFAULT_LIMIT, tier: str | None = None) -> str:
    """Render ``trend.md`` from ``index.jsonl``."""
    entries = read_index(index_path)
    limit = max(1, int(limit))
    series = _points(entries, tier)
    runs = {(e.get("sha"), _label(e), e.get("runtime"), e.get("ts")) for e in entries
            if tier is None or e.get("tier") == tier}
    lines = [
        "# LocalFlow perf trend",
        "",
        f"{len(runs)} runs, {len(series)} metric series"
        + (f" (tier `{tier}`)" if tier else "")
        + f". Window: last {limit} runs per series. Generated by `python -m evals.perf trend`.",
        "",
        "## Rust ÷ Python",
        "",
        (
            "Latest Rust value divided by the latest Python value on the same runner."
            " Below 1.00x is less (faster/lighter for lower-is-better metrics)."
        ),
        "",
    ]
    ratios = ratio_rows(entries, tier=tier)
    if ratios:
        lines += [
            "| Runner | Suite | Case | Metric | Rust | Python | Rust ÷ Python | Status |",
            "|---|---|---|---|---:|---:|---:|---|",
        ]
        for r in ratios:
            ratio = f"{r['ratio']:.2f}x" if r["ratio"] is not None else "n/a"
            lines.append(
                f"| {_cell(r['os'])} | {_cell(r['suite'])} | `{_cell(r['case'])}` | {_cell(r['metric'])} "
                f"| {_fmt(r['rust'], r['unit'])} | {_fmt(r['python'], r['unit'])} | {ratio} | {r['status']} |"
            )
    else:
        lines.append("_No runner has both a Rust and a Python value for the same case yet._")
    lines.append("")

    by_suite: dict[str, list[tuple]] = {}
    for key in sorted(series, key=lambda k: tuple(str(x) for x in k)):
        by_suite.setdefault(str(key[2]), []).append(key)
    for suite, keys in by_suite.items():
        lines += [
            f"## `{_cell(suite)}`",
            "",
            "| Runner | Runtime | Case | Metric | Latest | Median | Min | Max | n | Δ prev | Trend |",
            "|---|---|---|---|---:|---:|---:|---:|---:|---|---|",
        ]
        for key in keys:
            label, runtime, _suite, case, metric = key
            window = series[key][-limit:]
            values = [p["value"] for p in window]
            unit, better = window[-1]["unit"], window[-1]["better"]
            lines.append(
                f"| {_cell(label)} | {_cell(runtime)} | `{_cell(case)}` | {_cell(metric)} "
                f"| {_fmt(values[-1], unit)} | {_fmt(compute_median(values), unit)} "
                f"| {_fmt(min(values), unit)} | {_fmt(max(values), unit)} | {len(values)} "
                f"| {_delta(values, better)} | `{sparkline(values)}` |"
            )
        lines.append("")
    if not series:
        lines += ["_No metric data in the index yet._", ""]
    return "\n".join(lines)


# --------------------------------------------------------------------------
# Regressions
# --------------------------------------------------------------------------


def metric_key(label: str, runtime: str, suite: str, case: str, metric: str) -> str:
    return "/".join(str(x) for x in (label, runtime, suite, case, metric))


def issue_marker(key: str) -> str:
    return f"<!-- {ISSUE_MARKER_PREFIX}: {key} -->"


_SYNTH_RUNNER = {"label": "trend", "cpu": "", "cores": 0, "ram_gb": 0}


def _one_value_result(suite: str, case: str, name: str, point: dict, value: float) -> dict:
    run = make_run(
        tier="nightly", runtime="python", run_id="trend", sha="trend", ref="trend",
        os_name="linux", arch="x86_64", runner=_SYNTH_RUNNER, started_at="1970-01-01T00:00:00Z",
    )
    metric = {"unit": point["unit"] or "ratio", "better": point["better"], "value": value}
    return make_result(run, [make_case(suite=suite, case=case, dims=point.get("dims") or {}, metrics={name: metric})])


def judge(policy: dict, suite: str, case: str, name: str, baseline: float, point: dict) -> tuple[str, str]:
    """``(verdict, note)`` for ``point`` vs a scalar ``baseline`` under ``policy``.

    Index points carry no raw samples, so noisy metrics are judged on
    ``rel_tol`` alone (no MAD term) and never hit ``low_power``.
    """
    report = compare_results(
        _one_value_result(suite, case, name, point, baseline),
        _one_value_result(suite, case, name, point, point["value"]),
        policy,
    )
    for row in report.rows:
        if row.metric == name:
            return row.verdict, row.note
    return "pass", ""


def _regression_text(key: str, item: dict, consecutive: int) -> tuple[str, str]:
    unit = item["unit"]
    title = f"Perf regression: {key}"
    rows = "\n".join(
        f"| `{(sha or '')[:SHORT_SHA]}` | {ts} | {_fmt(v, unit)} |"
        for sha, ts, v in zip(item["shas"], item["timestamps"], item["recent_vals"], strict=True)
    )
    body = "\n".join([
        issue_marker(key),
        f"**{key}** regressed in the last {consecutive} consecutive nightly runs.",
        "",
        f"- Healthy baseline (median of last healthy runs): **{_fmt(item['baseline_val'], unit)}**",
        f"- Better: `{item['better']}`; rule: {item['note'] or 'policy.toml'}",
        "",
        "| SHA | Run started | Value |",
        "|---|---|---:|",
        rows,
        "",
        (
            "Opened by the perf-evals nightly (`python -m evals.perf regressions`);"
            " it closes itself once the metric recovers. Trend: `trend.md` on the `perf-data` branch."
        ),
    ])
    return title, body


def detect_consecutive_regressions(
    index_path: Path | str,
    policy_path: Path | str | None = DEFAULT_POLICY_PATH,
    *,
    consecutive: int = DEFAULT_CONSECUTIVE,
    tier: str | None = "nightly",
) -> dict:
    """Scan every series for regressions that persist ``consecutive`` runs.

    Each point is judged against the median of the last ``HEALTHY_WINDOW``
    healthy points before it; regressed points never enter the baseline.
    The first point of a series is judged against itself (only exact gates
    can fail there).
    """
    consecutive = max(1, int(consecutive))
    policy = load_policy(Path(policy_path or DEFAULT_POLICY_PATH))
    active, recovered, pending = [], [], []
    entries = read_index(index_path)
    series = _points(entries, tier)
    # Track timestamps of non-ok runs per (runner, runtime, suite, case) so an
    # unmeasured run between two slow runs breaks a consecutive regression streak.
    gap_ts: dict[tuple, list[str]] = {}
    for entry in entries:
        if tier is not None and entry.get("tier") != tier:
            continue
        if entry.get("status") != "ok":
            gkey = (_label(entry), entry.get("runtime"), entry.get("suite"), entry.get("case"))
            gap_ts.setdefault(gkey, []).append(str(entry.get("ts") or ""))
    for skey in sorted(series, key=lambda k: tuple(str(x) for x in k)):
        label, runtime, suite, case, name = skey
        points = series[skey]
        healthy: list[float] = []
        verdicts: list[tuple[str, str, float | None]] = []
        for point in points:
            base = compute_median(healthy[-HEALTHY_WINDOW:]) if healthy else point["value"]
            verdict, note = judge(policy, suite, case, name, base, point)
            verdicts.append((verdict, note, base))
            if verdict != "regressed":
                healthy.append(point["value"])
        key = metric_key(label, runtime, suite, case, name)
        tail = verdicts[-consecutive:]
        recent = points[-consecutive:]
        gaps = gap_ts.get((label, runtime, suite, case), [])
        has_gap = len(recent) == consecutive and any(
            recent[0]["ts"] < g < recent[-1]["ts"] or g > recent[-1]["ts"] for g in gaps
        )
        if len(tail) == consecutive and not has_gap and all(v[0] == "regressed" for v in tail):
            item = {
                "metric_key": key,
                "os": label,
                "runtime": runtime,
                "suite": suite,
                "case": case,
                "metric": name,
                "unit": recent[-1]["unit"],
                "better": recent[-1]["better"],
                "baseline_val": tail[-1][2],
                "recent_vals": [p["value"] for p in recent],
                "shas": [p["sha"] for p in recent],
                "timestamps": [p["ts"] for p in recent],
                "note": tail[-1][1],
            }
            item["title"], item["body"] = _regression_text(key, item, consecutive)
            active.append(item)
        elif verdicts and verdicts[-1][0] == "regressed":
            pending.append({"metric_key": key, "value": points[-1]["value"], "sha": points[-1]["sha"]})
        elif len(verdicts) >= 2 and any(v[0] == "regressed" for v in verdicts[:-1]):
            last = points[-1]
            recovered.append({
                "metric_key": key,
                "value": last["value"],
                "sha": last["sha"],
                "ts": last["ts"],
                "body": (
                    f"{issue_marker(key)}\nRecovered: latest nightly value {_fmt(last['value'], last['unit'])}"
                    f" at `{(last['sha'] or '')[:SHORT_SHA]}` ({last['ts']}) is within policy again."
                ),
            })
    return {
        "schema": "lf-perf-regressions/v1",
        "consecutive": consecutive,
        "tier": tier,
        "series": len(series),
        "active_regressions": active,
        "recovered": recovered,
        "pending": pending,
    }
