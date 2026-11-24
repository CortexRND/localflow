"""Baseline vs candidate comparison for ``lf-perf-result/v1`` files.

See ``docs/plans/PLAN-perf-evals.md`` §6. Stdlib only.
"""

from __future__ import annotations

import math
import tomllib
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from .schema import compute_mad, compute_median, validate_result

DEFAULT_POLICY_PATH = Path(__file__).resolve().parent / "policy.toml"

VERDICTS = ("pass", "regressed", "improved", "flaky", "not_exercised", "low_power")
VALID_CLASSES = ("exact", "quality", "noisy")
VALID_ON_PR = ("block", "warn")

CASE_METRIC = "(case)"

_BUILTIN_DEFAULTS: dict[str, Any] = {
    "noisy_min_samples": 3,
    "noisy_mad_multiplier": 3.0,
    "noisy_block_factor": 2.0,
    "noisy_latency_rel_tol": 0.15,
    "noisy_memory_rel_tol": 0.10,
}

_LATENCY_UNITS = ("ms", "s", "us", "ratio")


# --------------------------------------------------------------------------
# Policy
# --------------------------------------------------------------------------


def load_policy(path: Path = DEFAULT_POLICY_PATH) -> dict:
    """Load and sanity-check a ``policy.toml``. Raises ``ValueError`` if bad."""
    with Path(path).open("rb") as fh:
        try:
            policy = tomllib.load(fh)
        except tomllib.TOMLDecodeError as exc:
            raise ValueError(f"{path}: invalid TOML: {exc}") from exc
    errors = validate_policy(policy)
    if errors:
        raise ValueError(f"{path}: invalid policy:\n  " + "\n  ".join(errors))
    return policy


def _is_finite_number(value: Any) -> bool:
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(value)
    )


def _check_number(errors: list[str], where: str, value: Any, kind: str) -> None:
    """Append an error unless ``value`` satisfies ``kind``.

    Kinds: ``count`` (int >= 1), ``nonneg`` (finite >= 0), ``factor``
    (finite > 1.0), ``finite`` (any finite number). Bools never pass.
    """
    if kind == "count":
        if isinstance(value, bool) or not isinstance(value, int) or value < 1:
            errors.append(f"{where}: must be an integer >= 1, got {value!r}")
        return
    if not _is_finite_number(value):
        errors.append(f"{where}: must be a finite number, got {value!r}")
    elif kind == "nonneg" and value < 0:
        errors.append(f"{where}: must be >= 0, got {value!r}")
    elif kind == "factor" and value <= 1.0:
        errors.append(f"{where}: must be > 1.0, got {value!r}")


_DEFAULT_KINDS = {
    "noisy_min_samples": "count",
    "noisy_mad_multiplier": "nonneg",
    "noisy_block_factor": "factor",
    "noisy_latency_rel_tol": "nonneg",
    "noisy_memory_rel_tol": "nonneg",
}

_RULE_KINDS = {
    "abs_tol": "nonneg",
    "rel_tol": "nonneg",
    "mad_k": "nonneg",
    "block_factor": "factor",
    "min_samples": "count",
}


def validate_policy(policy: dict) -> list[str]:
    errors: list[str] = []
    defaults = policy.get("defaults", {})
    if not isinstance(defaults, dict):
        errors.append("defaults: must be a table")
    else:
        for key, kind in _DEFAULT_KINDS.items():
            if key in defaults:
                _check_number(errors, f"defaults.{key}", defaults[key], kind)
    metrics = policy.get("metrics", {})
    if not isinstance(metrics, dict):
        errors.append("metrics: must be a table")
        metrics = {}
    for name, rule in metrics.items():
        if not isinstance(rule, dict):
            errors.append(f"metrics.{name}: must be a table")
            continue
        if rule.get("class") not in VALID_CLASSES:
            errors.append(f"metrics.{name}.class: {rule.get('class')!r} not in {VALID_CLASSES}")
        if "on_pr" in rule and rule["on_pr"] not in VALID_ON_PR:
            errors.append(f"metrics.{name}.on_pr: {rule['on_pr']!r} not in {VALID_ON_PR}")
        if "better" in rule and rule["better"] not in ("lower", "higher", "exact"):
            errors.append(f"metrics.{name}.better: {rule['better']!r} invalid")
        if rule.get("class") == "quality" and "abs_tol" not in rule:
            errors.append(f"metrics.{name}.abs_tol: required for quality class")
        for key, kind in _RULE_KINDS.items():
            if key in rule:
                _check_number(errors, f"metrics.{name}.{key}", rule[key], kind)
    caps = policy.get("caps", [])
    if not isinstance(caps, list):
        errors.append("caps: must be an array of tables")
        caps = []
    for idx, cap in enumerate(caps):
        if not isinstance(cap, dict):
            errors.append(f"caps[{idx}]: must be a table")
            continue
        for key in ("suite", "metric"):
            if not isinstance(cap.get(key), str):
                errors.append(f"caps[{idx}].{key}: required string")
        if "max_value" not in cap and "min_value" not in cap:
            errors.append(f"caps[{idx}]: needs max_value and/or min_value")
        for key in ("min_value", "max_value"):
            if key in cap:
                _check_number(errors, f"caps[{idx}].{key}", cap[key], "finite")
        lo, hi = cap.get("min_value"), cap.get("max_value")
        if _is_finite_number(lo) and _is_finite_number(hi) and lo > hi:
            errors.append(f"caps[{idx}]: min_value {lo!r} > max_value {hi!r}")
        if not isinstance(cap.get("dims_match", {}), dict):
            errors.append(f"caps[{idx}].dims_match: must be a table")
    return errors


def _defaults(policy: dict) -> dict:
    merged = dict(_BUILTIN_DEFAULTS)
    merged.update(policy.get("defaults", {}) or {})
    return merged


def resolve_rule(policy: dict, name: str, metric: dict) -> dict:
    """Return the effective rule for metric ``name`` with defaults filled in."""
    defaults = _defaults(policy)
    rule = dict((policy.get("metrics") or {}).get(name) or {})
    if not rule:
        unit = metric.get("unit")
        if unit == "bool":
            rule = {"class": "exact", "on_pr": "block"}
        elif unit == "bytes":
            rule = {"class": "noisy", "rel_tol": defaults["noisy_memory_rel_tol"], "on_pr": "warn"}
        else:
            rule = {"class": "noisy", "rel_tol": defaults["noisy_latency_rel_tol"], "on_pr": "warn"}
        rule["fallback"] = True
    rule.setdefault("better", metric.get("better", "lower"))
    if rule["class"] == "noisy":
        rule.setdefault(
            "rel_tol",
            defaults["noisy_memory_rel_tol"]
            if metric.get("unit") == "bytes"
            else defaults["noisy_latency_rel_tol"],
        )
        rule.setdefault("mad_k", defaults["noisy_mad_multiplier"])
        rule.setdefault("block_factor", defaults["noisy_block_factor"])
        rule.setdefault("min_samples", defaults["noisy_min_samples"])
        rule.setdefault("on_pr", "warn")
    else:
        rule.setdefault("on_pr", "block")
        rule.setdefault("abs_tol", 0.0)
    return rule


# --------------------------------------------------------------------------
# Report types
# --------------------------------------------------------------------------


@dataclass
class CompareRow:
    suite: str
    case: str
    metric: str
    verdict: str
    blocking: bool = False
    warning: bool = False
    unit: str | None = None
    baseline: float | None = None
    candidate: float | None = None
    delta: float | None = None
    delta_pct: float | None = None
    metric_class: str | None = None
    note: str = ""


@dataclass
class CompareReport:
    rows: list[CompareRow] = field(default_factory=list)
    baseline_run: dict = field(default_factory=dict)
    candidate_run: dict = field(default_factory=dict)
    strict: bool = False

    @property
    def blocked(self) -> bool:
        return any(r.blocking for r in self.rows)

    @property
    def exit_code(self) -> int:
        return 1 if self.blocked else 0

    @property
    def counts(self) -> dict[str, int]:
        counts = {v: 0 for v in VERDICTS}
        for row in self.rows:
            counts[row.verdict] += 1
        counts["blocking"] = sum(1 for r in self.rows if r.blocking)
        counts["warnings"] = sum(1 for r in self.rows if r.warning and not r.blocking)
        return counts

    @property
    def status(self) -> str:
        if self.blocked:
            return "FAIL"
        if self.counts["warnings"]:
            return "WARN"
        return "PASS"

    def summary_md(self) -> str:
        return render_markdown(self)

    def to_dict(self) -> dict:
        return {
            "status": self.status,
            "blocked": self.blocked,
            "exit_code": self.exit_code,
            "strict": self.strict,
            "counts": self.counts,
            "baseline_run": self.baseline_run,
            "candidate_run": self.candidate_run,
            "rows": [asdict(r) for r in self.rows],
        }


# --------------------------------------------------------------------------
# Comparison
# --------------------------------------------------------------------------


def _num(value: Any) -> float | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return 1.0 if value else 0.0
    return float(value)


def primary_value(metric: dict) -> float | None:
    """Scalar used for comparison: median, else median(samples), else value."""
    if metric.get("median") is not None:
        return _num(metric["median"])
    samples = metric.get("samples")
    if samples:
        return compute_median([_num(x) for x in samples])  # type: ignore[misc]
    return _num(metric.get("value"))


def _samples(metric: dict | None) -> list[float]:
    if not metric:
        return []
    return [_num(x) for x in metric.get("samples") or []]  # type: ignore[misc]


def _dims_match(dims: dict, match: dict) -> bool:
    for key, want in match.items():
        if key in dims:
            if dims[key] != want:
                return False
        elif key.endswith("_prefix"):
            got = dims.get(key[: -len("_prefix")])
            if not isinstance(got, str) or not got.startswith(str(want)):
                return False
        else:
            return False
    return True


def _cap_violation(policy: dict, case: dict, name: str, value: float | None) -> str | None:
    if value is None:
        return None
    for cap in policy.get("caps") or []:
        if cap.get("suite") != case["suite"] or cap.get("metric") != name:
            continue
        if not _dims_match(case.get("dims") or {}, cap.get("dims_match") or {}):
            continue
        reason = cap.get("reason") or "absolute cap"
        if "max_value" in cap and value > float(cap["max_value"]):
            return f"cap: {value:g} > max {float(cap['max_value']):g} ({reason})"
        if "min_value" in cap and value < float(cap["min_value"]):
            return f"cap: {value:g} < min {float(cap['min_value']):g} ({reason})"
    return None


def _is_binary_metric(metric: dict) -> bool:
    """True only for real pass/fail samples: ``unit == "bool"`` or Python bools.

    Float ratios that happen to hit 0.0/1.0 (e.g. per-clip WER) are not binary.
    """
    raw = metric.get("samples") or []
    if not raw:
        return False
    return metric.get("unit") == "bool" or all(isinstance(x, bool) for x in raw)


def _delta(base: float | None, cand: float | None) -> tuple[float | None, float | None]:
    if base is None or cand is None:
        return None, None
    delta = cand - base
    pct = (delta / abs(base) * 100.0) if base != 0 else None
    return delta, pct


def _gate_target(rule: dict, metric: dict, values: list[float]) -> float | None:
    """Pass value of an exact gate, or ``None`` if the metric is not a gate.

    A gate is a bool or a pass rate in [0, 1]: it passes only at 1.0
    (or 0.0 when lower is better, e.g. a mismatch rate). Other exact metrics
    (arbitrary counts) are compared for equality/direction against baseline.
    """
    unit = metric.get("unit")
    is_gate = unit == "bool" or (
        unit == "ratio" and bool(values) and all(0.0 <= v <= 1.0 for v in values)
    )
    if not is_gate:
        return None
    return 0.0 if rule["better"] == "lower" else 1.0


def _compare_exact(
    row: CompareRow, rule: dict, cand_m: dict, base: float | None, cand: float
) -> None:
    """Exact class. ``base`` is ``None`` for new cases/metrics."""
    soft_block = rule["on_pr"] == "block"
    cand_samples = _samples(cand_m)
    better = rule["better"]
    if len(set(cand_samples)) > 1:
        row.verdict, row.blocking = "flaky", soft_block
        row.note = f"repeats differ: {sorted(set(cand_samples))}"
        return
    values = [cand, *cand_samples] + ([base] if base is not None else [])
    target = _gate_target(rule, cand_m, values)
    if target is not None:
        base_failing = base is not None and base != target
        if cand != target:
            row.verdict, row.blocking = "regressed", soft_block
            row.note = "gate failing" + (" (baseline also failing)" if base_failing else "")
        elif base_failing:
            row.verdict = "improved"
            row.note = "gate now passing"
        else:
            row.verdict = "pass"
        return
    if base is None:
        row.verdict = "pass"
    elif (
        (better == "exact" and cand != base)
        or (better == "higher" and cand < base)
        or (better == "lower" and cand > base)
    ):
        row.verdict, row.blocking = "regressed", soft_block
    elif cand != base:
        row.verdict = "improved"
    else:
        row.verdict = "pass"


def _compare_quality(row: CompareRow, rule: dict, cand_m: dict, base: float, cand: float) -> None:
    soft_block = rule["on_pr"] == "block"
    cand_samples = _samples(cand_m)
    tol = float(rule.get("abs_tol", 0.0))
    if _is_binary_metric(cand_m) and len(set(cand_samples)) > 1:
        row.verdict, row.blocking = "flaky", soft_block
        row.note = f"repeats differ: {sorted(set(cand_samples))}"
        return
    delta = cand - base
    worse = delta if rule["better"] == "lower" else -delta
    if rule["better"] == "exact":
        worse = abs(delta)
    if worse > tol + 1e-12:
        row.verdict, row.blocking = "regressed", soft_block
        row.note = f"beyond abs_tol {tol:g}"
    elif worse < -tol - 1e-12:
        row.verdict = "improved"
    else:
        row.verdict = "pass"


def _compare_noisy(
    row: CompareRow,
    rule: dict,
    base_m: dict,
    cand_m: dict,
    base: float,
    cand: float,
    strict: bool,
) -> None:
    base_samples = _samples(base_m)
    cand_samples = _samples(cand_m)
    base_mad = compute_mad(base_samples) if base_samples else 0.0
    rel_tol = float(rule["rel_tol"])
    mad_k = float(rule["mad_k"])
    block_factor = float(rule["block_factor"])
    threshold = max(rel_tol * abs(base), mad_k * base_mad)
    better = rule["better"]

    if better == "higher":
        worse = base - cand
        if cand > 0:
            ratio = base / cand
        else:
            ratio = math.inf if base > 0 else 1.0
    elif better == "lower":
        worse = cand - base
        ratio = cand / base if base > 0 else 1.0  # no hard block off a zero baseline
    else:  # "exact" on a noisy metric: any drift counts as worse
        worse = abs(cand - base)
        ratio = 1.0

    ratio_txt = "inf" if math.isinf(ratio) else f"{ratio:.2f}x"
    row.note = f"threshold ±{threshold:g}"
    if ratio >= block_factor and worse > 0:
        row.verdict, row.blocking = "regressed", True
        row.note = f"{ratio_txt} worse >= hard block {block_factor:g}x"
    elif (cand_samples and len(cand_samples) < int(rule["min_samples"])) or (
        base_samples and len(base_samples) < int(rule["min_samples"])
    ):
        row.verdict = "low_power"
        row.note = (
            f"n(baseline)={len(base_samples)}, n(candidate)={len(cand_samples)}"
            f" < min_samples {int(rule['min_samples'])}"
        )
    elif worse > threshold:
        row.verdict = "regressed"
        row.blocking = bool(strict) or rule["on_pr"] == "block"
        row.warning = True
        row.note = f"{ratio_txt} worse, beyond threshold {threshold:g}"
    elif worse < -threshold:
        row.verdict = "improved"
    else:
        row.verdict = "pass"


def _metric_rows(
    policy: dict,
    base_case: dict,
    cand_case: dict,
    strict: bool,
) -> list[CompareRow]:
    rows: list[CompareRow] = []
    suite, case = cand_case["suite"], cand_case["case"]
    base_metrics = base_case.get("metrics") or {}
    cand_metrics = cand_case.get("metrics") or {}
    names = list(base_metrics) + [n for n in cand_metrics if n not in base_metrics]
    for name in names:
        base_m, cand_m = base_metrics.get(name), cand_metrics.get(name)
        ref_m = cand_m or base_m
        rule = resolve_rule(policy, name, ref_m)
        base = primary_value(base_m) if base_m else None
        cand = primary_value(cand_m) if cand_m else None
        delta, pct = _delta(base, cand)
        row = CompareRow(
            suite=suite,
            case=case,
            metric=name,
            verdict="pass",
            unit=ref_m.get("unit"),
            baseline=base,
            candidate=cand,
            delta=delta,
            delta_pct=pct,
            metric_class=rule["class"],
        )
        if cand_m is None:
            row.verdict, row.blocking = "regressed", True
            row.note = "metric missing in candidate"
        elif base_m is not None and base_m.get("unit") != cand_m.get("unit"):
            row.verdict, row.blocking = "regressed", True
            row.unit, row.delta, row.delta_pct = None, None, None  # raw numbers, no unit
            row.note = (
                f"unit mismatch: baseline {base_m.get('unit')!r} vs candidate "
                f"{cand_m.get('unit')!r}; not compared"
            )
        elif base_m is None or base is None:
            if rule["class"] == "exact" and cand is not None:
                _compare_exact(row, rule, cand_m, None, cand)
            row.note = "new metric" + (f"; {row.note}" if row.note else "")
        elif cand is None:
            row.verdict = "not_exercised"
            row.warning = True
            row.note = "candidate has no value"
        elif rule["class"] == "exact":
            _compare_exact(row, rule, cand_m, base, cand)
        elif rule["class"] == "quality":
            _compare_quality(row, rule, cand_m, base, cand)
        else:
            _compare_noisy(row, rule, base_m, cand_m, base, cand, strict)
        if rule.get("fallback") and row.verdict != "pass":
            row.note = (row.note + "; " if row.note else "") + "no policy rule, unit fallback"
        if cand_m is not None:
            _apply_cap(policy, cand_case, name, cand, row)
        rows.append(row)
    return rows


def _apply_cap(policy: dict, case: dict, name: str, value: float | None, row: CompareRow) -> None:
    violation = _cap_violation(policy, case, name, value)
    if violation:
        row.verdict, row.blocking = "regressed", True
        row.note = violation if not row.note else f"{violation}; {row.note}"


def _new_case_rows(policy: dict, cand_case: dict, note: str) -> list[CompareRow]:
    rows: list[CompareRow] = []
    for name, metric in (cand_case.get("metrics") or {}).items():
        value = primary_value(metric)
        rule = resolve_rule(policy, name, metric)
        row = CompareRow(
            suite=cand_case["suite"],
            case=cand_case["case"],
            metric=name,
            verdict="pass",
            unit=metric.get("unit"),
            candidate=value,
            metric_class=rule["class"],
        )
        # Correctness gates have an absolute pass value, so they gate new cases too.
        if rule["class"] == "exact" and value is not None:
            _compare_exact(row, rule, metric, None, value)
        row.note = note + (f"; {row.note}" if row.note else "")
        _apply_cap(policy, cand_case, name, value, row)
        rows.append(row)
    if not rows:
        rows.append(
            CompareRow(
                suite=cand_case["suite"], case=cand_case["case"], metric=CASE_METRIC,
                verdict="pass", note=note,
            )
        )
    return rows


def compare_results(
    baseline: dict,
    candidate: dict,
    policy: dict,
    *,
    strict: bool = False,
) -> CompareReport:
    """Compare two validated results under ``policy``.

    Raises ``ValueError`` if either input is not a valid ``lf-perf-result/v1``.
    """
    for label, data in (("baseline", baseline), ("candidate", candidate)):
        errors = validate_result(data)
        if errors:
            raise ValueError(f"invalid {label}:\n  " + "\n  ".join(errors))

    base_cases = {(c["suite"], c["case"]): c for c in baseline["cases"]}
    cand_cases = {(c["suite"], c["case"]): c for c in candidate["cases"]}
    keys = list(base_cases) + [k for k in cand_cases if k not in base_cases]

    report = CompareReport(
        baseline_run=baseline["run"], candidate_run=candidate["run"], strict=strict
    )
    for key in keys:
        suite, name = key
        base_case, cand_case = base_cases.get(key), cand_cases.get(key)
        base_status = base_case["status"] if base_case else None

        if cand_case is None:
            if base_status == "ok":
                report.rows.append(CompareRow(
                    suite, name, CASE_METRIC, "regressed", blocking=True,
                    note="missing in candidate",
                ))
            else:
                report.rows.append(CompareRow(
                    suite, name, CASE_METRIC, "not_exercised", warning=True,
                    note=f"missing in candidate (baseline {base_status}: {base_case['error']})",
                ))
            continue

        cand_status = cand_case["status"]
        if cand_status == "failed":
            report.rows.append(CompareRow(
                suite, name, CASE_METRIC, "regressed", blocking=True,
                note=f"failed: {cand_case['error']}",
            ))
            continue
        if cand_status == "not_exercised":
            if base_status == "ok":
                report.rows.append(CompareRow(
                    suite, name, CASE_METRIC, "not_exercised", warning=True,
                    note=f"was ok in baseline, now not exercised: {cand_case['error']}",
                ))
            else:
                report.rows.append(CompareRow(
                    suite, name, CASE_METRIC, "not_exercised", note=cand_case["error"],
                ))
            continue

        if base_case is None:
            report.rows.extend(_new_case_rows(policy, cand_case, "new case"))
        elif base_status != "ok":
            report.rows.extend(
                _new_case_rows(policy, cand_case, f"baseline was {base_status}")
            )
        else:
            rows = _metric_rows(policy, base_case, cand_case, strict)
            if not rows:
                rows = [CompareRow(suite, name, CASE_METRIC, "pass", note="no metrics")]
            report.rows.extend(rows)
    return report


# --------------------------------------------------------------------------
# Markdown
# --------------------------------------------------------------------------


def format_value(value: float | None, unit: str | None) -> str:
    """Human-readable value for the report table."""
    if value is None:
        return "–"
    if unit == "bool":
        return "true" if value else "false"
    if unit == "bytes":
        mag = abs(value)
        if mag >= 1024**3:
            return f"{value / 1024**3:.2f} GiB"
        if mag >= 1024**2:
            return f"{value / 1024**2:.1f} MiB"
        if mag >= 1024:
            return f"{value / 1024:.1f} KiB"
        return f"{value:.0f} B"
    if unit == "ms":
        return f"{value:.1f} ms" if abs(value) < 100 else f"{value:.0f} ms"
    if unit == "us":
        return f"{value:.1f} µs"
    if unit == "s":
        return f"{value:.2f} s"
    if unit == "ratio":
        return f"{value:.4f}"
    if float(value).is_integer():
        return f"{value:.0f}" + (f" {unit}" if unit else "")
    return f"{value:.4g}" + (f" {unit}" if unit else "")


def _format_delta(row: CompareRow) -> str:
    if row.delta is None:
        return "–"
    if row.unit == "bool":
        return "changed" if row.delta else "="
    sign = "+" if row.delta > 0 else ("" if row.delta < 0 else "±")
    text = sign + format_value(row.delta, row.unit)
    if row.delta_pct is not None and row.unit != "ratio":
        text += f" ({row.delta_pct:+.1f}%)"
    return text


def _cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def render_markdown(report: CompareReport) -> str:
    counts = report.counts
    summary = (
        f"**Perf compare: {report.status}** — "
        f"{counts['blocking']} blocking, {counts['warnings']} warnings, "
        f"{counts['improved']} improved, {counts['pass']} pass, "
        f"{counts['flaky']} flaky, {counts['not_exercised']} not_exercised, "
        f"{counts['low_power']} low_power"
    )
    lines = [summary, ""]
    b, c = report.baseline_run, report.candidate_run
    if b or c:
        lines.append(
            f"Baseline `{str(b.get('sha', '?'))[:12]}` vs candidate `{str(c.get('sha', '?'))[:12]}` "
            f"({c.get('tier', '?')}, {c.get('runtime', '?')}, {c.get('os', '?')}/{c.get('arch', '?')})"
            + (" — strict" if report.strict else "")
        )
        lines.append("")
    if not report.rows:
        lines.append("_No cases to compare._")
        return "\n".join(lines) + "\n"
    lines.append("| Suite | Case | Metric | Baseline | Candidate | Delta | Verdict |")
    lines.append("|---|---|---|---|---|---|---|")
    for row in report.rows:
        verdict = f"**{row.verdict}**" if row.blocking else row.verdict
        if row.blocking:
            verdict += " (blocking)"
        elif row.warning:
            verdict += " (warn)"
        if row.note:
            verdict += f" — {row.note}"
        lines.append(
            "| "
            + " | ".join(
                _cell(x)
                for x in (
                    row.suite,
                    row.case,
                    row.metric,
                    format_value(row.baseline, row.unit),
                    format_value(row.candidate, row.unit),
                    _format_delta(row),
                    verdict,
                )
            )
            + " |"
        )
    return "\n".join(lines) + "\n"
