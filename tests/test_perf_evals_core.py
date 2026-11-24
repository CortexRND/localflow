"""Unit tests for the stdlib-only perf eval core (evals/perf)."""

from __future__ import annotations

import ast
import copy
import json
import random
import subprocess
import sys
from pathlib import Path

import pytest

from evals.perf import (
    SCHEMA_VERSION,
    compare_results,
    compute_mad,
    compute_median,
    compute_p95,
    detect_git_info,
    detect_runner_info,
    load_policy,
    load_result,
    make_case,
    make_metric,
    make_result,
    make_run,
    registry,
    render_markdown,
    validate_result,
    write_result,
)
from evals.perf import __main__ as cli
from evals.perf.compare import DEFAULT_POLICY_PATH, format_value, resolve_rule

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"

RUNNER = {"label": "test", "cpu": "test-cpu", "cores": 4, "ram_gb": 16}


@pytest.fixture(scope="module")
def policy() -> dict:
    return load_policy(DEFAULT_POLICY_PATH)


def _run(**overrides) -> dict:
    kwargs = {
        "tier": "smoke",
        "runtime": "python",
        "run_id": "run-1",
        "sha": "abc123",
        "ref": "main",
        "os_name": "linux",
        "arch": "x86_64",
        "runner": RUNNER,
        "started_at": "2026-11-24T00:00:00Z",
    }
    kwargs.update(overrides)
    return make_run(**kwargs)


def _result(*cases: dict, **run_overrides) -> dict:
    return make_result(_run(**run_overrides), list(cases))


def _case(metrics: dict, *, suite="stt", case="c1", dims=None, **kw) -> dict:
    return make_case(suite=suite, case=case, dims=dims or {}, metrics=metrics, **kw)


def _lat(samples, unit="ms", better="lower", name="latency_ms", **kw) -> dict:
    return _case({name: make_metric(unit=unit, better=better, samples=samples)}, **kw)


def _cmp(base: dict, cand: dict, policy: dict, **kw):
    return compare_results(_result(base), _result(cand), policy, **kw)


def _only_row(report):
    assert len(report.rows) == 1, report.rows
    return report.rows[0]


# --------------------------------------------------------------------------
# Stdlib-only constraint
# --------------------------------------------------------------------------

STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}


@pytest.mark.parametrize("path", sorted(PERF_DIR.glob("*.py")), ids=lambda p: p.name)
def test_core_modules_import_only_stdlib(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom):
            if node.level:  # relative import inside evals.perf
                continue
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            top = mod.split(".")[0]
            assert top != "localflow", f"{path.name} imports localflow"
            assert top in STDLIB_OK, f"{path.name} imports non-stdlib {mod}"


# --------------------------------------------------------------------------
# Stats helpers
# --------------------------------------------------------------------------


def test_compute_median():
    assert compute_median([3, 1, 2]) == 2.0
    assert compute_median([1, 2, 3, 4]) == 2.5
    assert compute_median([7]) == 7.0
    with pytest.raises(ValueError):
        compute_median([])


def test_compute_p95():
    assert compute_p95([5.0]) == 5.0
    assert compute_p95([1, 2]) == pytest.approx(1.95)
    assert compute_p95(list(range(1, 101))) == pytest.approx(95.05)
    assert compute_p95([10, 1, 5, 3, 7]) == compute_p95([1, 3, 5, 7, 10])
    with pytest.raises(ValueError):
        compute_p95([])


def test_compute_mad():
    assert compute_mad([]) == 0.0
    assert compute_mad([4.0]) == 0.0
    assert compute_mad([1, 1, 1]) == 0.0
    # median 3, |dev| = [2,1,0,1,2] -> median 1
    assert compute_mad([1, 2, 3, 4, 5]) == 1.0
    assert compute_mad([1, 2, 3, 4, 100]) == 1.0  # robust to outliers


# --------------------------------------------------------------------------
# Schema builders
# --------------------------------------------------------------------------


def test_make_metric_samples_and_value():
    m = make_metric(unit="ms", better="lower", samples=[10, 30, 20])
    assert m["samples"] == [10, 30, 20]
    assert m["median"] == 20.0
    assert m["p95"] == pytest.approx(29.0)
    assert "value" not in m

    m = make_metric(unit="ms", better="lower", samples=[1.0], value=5)
    assert m["value"] == 5

    m = make_metric(unit="ratio", better="lower", value=0.1)
    assert m == {"unit": "ratio", "better": "lower", "value": 0.1}

    m = make_metric(unit="bool", better="exact", value=False)
    assert m["value"] is False


@pytest.mark.parametrize(
    "kwargs",
    [
        {"unit": "ms", "better": "sideways", "value": 1},
        {"unit": "", "better": "lower", "value": 1},
        {"unit": "ms", "better": "lower"},
        {"unit": "ms", "better": "lower", "value": "fast"},
        {"unit": "ms", "better": "lower", "value": float("nan")},
    ],
)
def test_make_metric_rejects_bad_input(kwargs):
    with pytest.raises(ValueError):
        make_metric(**kwargs)


def test_make_case_requires_reason_for_non_ok():
    with pytest.raises(ValueError):
        make_case(suite="s", case="c", dims={}, status="failed")
    with pytest.raises(ValueError):
        make_case(suite="s", case="c", dims={}, status="not_exercised", error="  ")
    with pytest.raises(ValueError):
        make_case(suite="s", case="c", dims={}, status="bogus", error="x")
    c = make_case(suite="s", case="c", dims={"a": 1}, status="failed", error="boom")
    assert c == {
        "suite": "s", "case": "c", "dims": {"a": 1}, "status": "failed",
        "error": "boom", "metrics": {}, "evidence": {},
    }


@pytest.mark.parametrize(
    ("os_name", "expected"),
    [("darwin", "macos"), ("win32", "windows"), ("linux", "linux"), ("Linux", "linux")],
)
def test_make_run_normalizes_os(os_name, expected):
    assert _run(os_name=os_name)["os"] == expected


@pytest.mark.parametrize(
    ("arch", "expected"),
    [("amd64", "x86_64"), ("AMD64", "x86_64"), ("x86_64", "x86_64"),
     ("aarch64", "arm64"), ("arm64", "arm64")],
)
def test_make_run_normalizes_arch(arch, expected):
    assert _run(arch=arch)["arch"] == expected


def test_make_run_rejects_bad_values():
    with pytest.raises(ValueError):
        _run(tier="weekly")
    with pytest.raises(ValueError):
        _run(runtime="go")
    with pytest.raises(ValueError):
        _run(os_name="plan9")
    with pytest.raises(ValueError):
        _run(arch="mips")


def test_make_run_autodetects():
    run = make_run(tier="nightly", runtime="rust")
    assert run["os"] in ("linux", "macos", "windows")
    assert run["id"] and run["sha"] and run["ref"]
    assert run["harness"] == "1"
    assert validate_result(make_result(run, [])) == []


def test_detect_runner_info(monkeypatch):
    monkeypatch.delenv("LF_PERF_RUNNER_LABEL", raising=False)
    monkeypatch.delenv("RUNNER_NAME", raising=False)
    info = detect_runner_info()
    assert info["label"] == "local"
    assert isinstance(info["cpu"], str) and info["cpu"]
    assert isinstance(info["cores"], int) and info["cores"] >= 1
    assert isinstance(info["ram_gb"], int) and info["ram_gb"] >= 0
    assert detect_runner_info("ubuntu-latest")["label"] == "ubuntu-latest"
    monkeypatch.setenv("RUNNER_NAME", "GitHub Actions 3")
    assert detect_runner_info()["label"] == "GitHub Actions 3"


def test_detect_git_info(tmp_path, monkeypatch):
    sha, ref = detect_git_info(REPO_ROOT)
    assert sha and ref
    for var in ("GITHUB_SHA", "GITHUB_REF", "GITHUB_HEAD_REF"):
        monkeypatch.delenv(var, raising=False)
    assert detect_git_info(tmp_path) == ("unknown", "unknown")


# --------------------------------------------------------------------------
# Validation
# --------------------------------------------------------------------------


def _valid() -> dict:
    return _result(
        _lat([10, 11, 12]),
        _case({"wer": make_metric(unit="ratio", better="lower", value=0.05)}, case="c2"),
        make_case(suite="stt", case="c3", dims={}, status="not_exercised", error="no GPU"),
    )


def test_validate_valid_result():
    assert validate_result(_valid()) == []
    assert _valid()["schema"] == SCHEMA_VERSION == "lf-perf-result/v1"


def _mutated(fn) -> list[str]:
    data = copy.deepcopy(_valid())
    fn(data)
    return validate_result(data)


@pytest.mark.parametrize(
    ("mutate", "needle"),
    [
        (lambda d: d.update(schema="lf-perf-result/v0"), "schema"),
        (lambda d: d.pop("run"), "run"),
        (lambda d: d["run"].pop("sha"), "run.sha"),
        (lambda d: d["run"].update(tier="weekly"), "run.tier"),
        (lambda d: d["run"].update(runtime="go"), "run.runtime"),
        (lambda d: d["run"].update(os="darwin"), "run.os"),
        (lambda d: d["run"].update(arch="amd64"), "run.arch"),
        (lambda d: d["run"].update(started_at="yesterday"), "run.started_at"),
        (lambda d: d["run"]["runner"].update(cores="4"), "run.runner.cores"),
        (lambda d: d["run"].pop("runner"), "run.runner"),
        (lambda d: d.update(cases={}), "cases"),
        (lambda d: d["cases"][0].pop("dims"), "dims"),
        (lambda d: d["cases"][0].pop("suite"), "suite"),
        (lambda d: d["cases"][0].update(status="skipped"), "status"),
        (lambda d: d["cases"][0].pop("evidence"), "evidence"),
        (lambda d: d["cases"][0].update(metrics=[]), "metrics"),
        (lambda d: d["cases"].append(copy.deepcopy(d["cases"][0])), "duplicate"),
        (lambda d: d["cases"][2].update(error=None), "reason"),
        (lambda d: d["cases"][2].update(error=""), "reason"),
        (lambda d: d["cases"][0].update(status="failed"), "reason"),
        (lambda d: d["cases"][1]["metrics"]["wer"].pop("unit"), "unit"),
        (lambda d: d["cases"][1]["metrics"]["wer"].update(better="up"), "better"),
        (lambda d: d["cases"][1]["metrics"]["wer"].pop("value"), "at least one"),
        (lambda d: d["cases"][1]["metrics"]["wer"].update(value=None), "at least one"),
        (lambda d: d["cases"][0]["metrics"]["latency_ms"].update(samples=["x"]), "samples"),
        (lambda d: d["cases"][0]["metrics"].update(bad="nope"), "must be an object"),
    ],
)
def test_validate_catches(mutate, needle):
    errors = _mutated(mutate)
    assert errors, "expected a validation error"
    assert any(needle in e for e in errors), errors


def test_validate_non_dict():
    assert validate_result([]) == ["result: must be a JSON object"]


def test_write_and_load_roundtrip(tmp_path):
    path = tmp_path / "nested" / "dir" / "result.json"
    write_result(path, _valid())
    assert load_result(path) == _valid()


def test_write_rejects_invalid(tmp_path):
    bad = _valid()
    bad["schema"] = "nope"
    with pytest.raises(ValueError, match="schema"):
        write_result(tmp_path / "r.json", bad)
    assert not (tmp_path / "r.json").exists()


# --------------------------------------------------------------------------
# Policy
# --------------------------------------------------------------------------

EXACT = "exact_match_rate citation_equal pid_cleanup_ok lifecycle_ok gate_pass M1 M2 M3 M4 M11 M12 M13"
LATENCY = (
    "latency_ms first_ms load_ms download_ms rtf startup_ms search_ms llm_ms summarize_ms "
    "chat_ms stop_to_notes_ms server_ms wall_ms render_us cpu_s cold_ms warm_ms reload_ms"
)
MEMORY = "rss_before rss_loaded rss_after_unload rss_idle rss_peak footprint_bytes package_bytes rss_bytes"


def test_policy_defaults(policy):
    d = policy["defaults"]
    assert d["noisy_min_samples"] == 3
    assert d["noisy_mad_multiplier"] == 3.0
    assert d["noisy_block_factor"] == 2.0
    assert d["noisy_latency_rel_tol"] == 0.15
    assert d["noisy_memory_rel_tol"] == 0.10


def test_policy_metric_classes(policy):
    m = policy["metrics"]
    for name in EXACT.split():
        assert m[name] == {"class": "exact", "on_pr": "block"}, name
    assert m["wer"] == {"class": "quality", "better": "lower", "abs_tol": 0.02, "on_pr": "block"}
    for name in ("M5", "M6", "M7", "M8", "M9", "M10"):
        assert m[name]["class"] == "quality" and m[name]["abs_tol"] == 0.05
        assert m[name]["better"] == "higher" and m[name]["on_pr"] == "block"
    for name in ("fidelity_rate", "number_fidelity_rate", "vocab_fidelity_rate"):
        assert m[name]["abs_tol"] == 0.02 and m[name]["better"] == "higher"
    for names, tol in ((LATENCY, 0.15), (MEMORY, 0.10)):
        for name in names.split():
            rule = m[name]
            assert rule["class"] == "noisy" and rule["rel_tol"] == tol, name
            assert rule["mad_k"] == 3.0 and rule["block_factor"] == 2.0
            assert rule["on_pr"] == "warn"


def test_policy_caps(policy):
    (cap,) = policy["caps"]
    assert cap["suite"] == "stt" and cap["metric"] == "latency_ms"
    assert cap["dims_match"] == {"provider": "cpu", "model_prefix": "tiny"}
    assert cap["max_value"] == 3000.0
    assert "3 s" in cap["reason"]


def test_load_policy_rejects_bad(tmp_path):
    bad = tmp_path / "p.toml"
    bad.write_text('[metrics.x]\nclass = "fuzzy"\n')
    with pytest.raises(ValueError, match="class"):
        load_policy(bad)
    bad.write_text("not = [valid")
    with pytest.raises(ValueError, match="TOML"):
        load_policy(bad)


def test_resolve_rule_unit_fallback(policy):
    assert resolve_rule(policy, "mystery", {"unit": "bytes", "better": "lower"})["rel_tol"] == 0.10
    assert resolve_rule(policy, "mystery", {"unit": "ms", "better": "lower"})["rel_tol"] == 0.15
    assert resolve_rule(policy, "mystery", {"unit": "s", "better": "lower"})["class"] == "noisy"
    assert resolve_rule(policy, "mystery", {"unit": "bool", "better": "exact"})["class"] == "exact"


# --------------------------------------------------------------------------
# Compare: exact class
# --------------------------------------------------------------------------


def _exact(value=None, samples=None, name="exact_match_rate", unit="ratio", better="higher"):
    return _case({name: make_metric(unit=unit, better=better, value=value, samples=samples)})


def test_exact_pass(policy):
    report = _cmp(_exact(1.0), _exact(1.0), policy)
    assert _only_row(report).verdict == "pass"
    assert report.exit_code == 0 and report.status == "PASS"


def test_exact_regression_blocks(policy):
    report = _cmp(_exact(1.0), _exact(0.98), policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and row.blocking
    assert report.exit_code == 1 and report.status == "FAIL"


def test_exact_bool_change_blocks(policy):
    base = _exact(True, name="citation_equal", unit="bool", better="exact")
    cand = _exact(False, name="citation_equal", unit="bool", better="exact")
    assert _only_row(_cmp(base, cand, policy)).verdict == "regressed"
    assert _only_row(_cmp(base, base, policy)).verdict == "pass"


def test_exact_flaky_repeats_block(policy):
    base = _exact(samples=[1, 1, 1], name="lifecycle_ok", unit="bool", better="exact")
    cand = _exact(samples=[1, 0, 1], name="lifecycle_ok", unit="bool", better="exact")
    report = _cmp(base, cand, policy)
    row = _only_row(report)
    assert row.verdict == "flaky" and row.blocking
    assert report.exit_code == 1


def test_exact_improvement(policy):
    assert _only_row(_cmp(_exact(0.9), _exact(1.0), policy)).verdict == "improved"


# --------------------------------------------------------------------------
# Compare: quality class
# --------------------------------------------------------------------------


def _wer(v):
    return _case({"wer": make_metric(unit="ratio", better="lower", value=v)})


def test_wer_within_tolerance_passes(policy):
    report = _cmp(_wer(0.10), _wer(0.12), policy)
    assert _only_row(report).verdict == "pass"
    assert report.exit_code == 0


def test_wer_regression_blocks(policy):
    """Acceptance criterion: a corrupted transcript fails the WER gate."""
    report = _cmp(_wer(0.10), _wer(0.13), policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and row.blocking
    assert report.exit_code == 1


def test_wer_improvement(policy):
    assert _only_row(_cmp(_wer(0.10), _wer(0.05), policy)).verdict == "improved"


@pytest.mark.parametrize(
    ("base", "cand", "verdict"),
    [(0.90, 0.86, "pass"), (0.90, 0.84, "regressed"), (0.80, 0.90, "improved")],
)
def test_sim_gates_higher_is_better(policy, base, cand, verdict):
    for gate in ("M5", "M6", "M7", "M8", "M9", "M10"):
        b = _case({gate: make_metric(unit="ratio", better="higher", value=base)}, suite="meetings")
        c = _case({gate: make_metric(unit="ratio", better="higher", value=cand)}, suite="meetings")
        row = _only_row(_cmp(b, c, policy))
        assert row.verdict == verdict, gate
        assert row.blocking == (verdict == "regressed")


def test_quality_binary_flaky(policy):
    b = _case({"M5": make_metric(unit="ratio", better="higher", samples=[1, 1, 1])})
    c = _case({"M5": make_metric(unit="ratio", better="higher", samples=[1, 0, 1])})
    row = _only_row(_cmp(b, c, policy))
    assert row.verdict == "flaky" and row.blocking


# --------------------------------------------------------------------------
# Compare: noisy class
# --------------------------------------------------------------------------


def test_noisy_small_latency_noise_passes(policy):
    report = _cmp(_lat([100, 102, 98, 101, 99]), _lat([110, 112, 108, 111, 109]), policy)
    assert _only_row(report).verdict == "pass"


def test_noisy_small_memory_noise_passes(policy):
    mib = 1024**2
    base = _lat([200 * mib] * 3, unit="bytes", name="rss_loaded")
    cand = _lat([215 * mib] * 3, unit="bytes", name="rss_loaded")
    assert _only_row(_cmp(base, cand, policy)).verdict == "pass"
    cand = _lat([225 * mib] * 3, unit="bytes", name="rss_loaded")
    assert _only_row(_cmp(base, cand, policy)).verdict == "regressed"


def test_noisy_mad_widens_threshold(policy):
    # median 100, MAD 10 -> threshold max(15, 30) = 30; +25 % is within noise.
    base = _lat([80, 90, 100, 110, 120])
    assert _only_row(_cmp(base, _lat([125] * 5), policy)).verdict == "pass"
    assert _only_row(_cmp(base, _lat([135] * 5), policy)).verdict == "regressed"


def test_noisy_1_3x_warns_then_blocks_with_strict(policy):
    base, cand = _lat([100] * 5), _lat([130] * 5)
    report = _cmp(base, cand, policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and not row.blocking and row.warning
    assert report.exit_code == 0 and report.status == "WARN"
    assert report.counts["warnings"] == 1

    strict = _cmp(base, cand, policy, strict=True)
    assert _only_row(strict).blocking
    assert strict.exit_code == 1 and strict.status == "FAIL"


def test_noisy_2x_slowdown_blocks(policy):
    """Acceptance criterion: an injected 2x STT slowdown makes compare fail."""
    report = _cmp(_lat([100, 105, 95, 102, 98]), _lat([200, 210, 190, 204, 196]), policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and row.blocking
    assert report.exit_code == 1


def test_noisy_higher_is_better(policy):
    def tput(v):
        return _case({"tput": make_metric(unit="ratio", better="higher", samples=[v] * 3)})

    assert _only_row(_cmp(tput(100), tput(95), policy)).verdict == "pass"
    row = _only_row(_cmp(tput(100), tput(80), policy))
    assert row.verdict == "regressed" and not row.blocking
    row = _only_row(_cmp(tput(100), tput(50), policy))
    assert row.verdict == "regressed" and row.blocking
    row = _only_row(_cmp(tput(100), tput(0), policy))
    assert row.verdict == "regressed" and row.blocking
    assert _only_row(_cmp(tput(100), tput(130), policy)).verdict == "improved"


def test_noisy_improvement(policy):
    assert _only_row(_cmp(_lat([100] * 5), _lat([60] * 5), policy)).verdict == "improved"


def test_self_comparison_never_regresses_10_runs(policy):
    """Acceptance criterion: unchanged runs compared to themselves never regress."""
    rng = random.Random(1234)

    def noisy_result():
        cases = []
        for clip in range(3):
            base_ms = 400 + 100 * clip
            cases.append(_case(
                {
                    "latency_ms": make_metric(
                        unit="ms", better="lower",
                        samples=[base_ms * rng.uniform(0.95, 1.05) for _ in range(5)],
                    ),
                    "rss_loaded": make_metric(
                        unit="bytes", better="lower",
                        samples=[int(300e6 * rng.uniform(0.98, 1.02)) for _ in range(5)],
                    ),
                    "wer": make_metric(unit="ratio", better="lower", value=0.08),
                },
                case=f"tiny.en-cpu/clip-{clip}",
                dims={"provider": "cpu", "model": "tiny.en"},
            ))
        return _result(*cases)

    baseline = noisy_result()
    for _ in range(10):
        candidate = noisy_result()
        report = compare_results(baseline, candidate, policy)
        assert report.counts["regressed"] == 0, render_markdown(report)
        assert report.exit_code == 0
        identical = compare_results(candidate, copy.deepcopy(candidate), policy)
        assert all(r.verdict == "pass" for r in identical.rows)


def test_noisy_zero_baseline_is_safe(policy):
    row = _only_row(_cmp(_lat([0, 0, 0]), _lat([5, 5, 5]), policy))
    assert row.verdict == "regressed" and not row.blocking
    assert row.delta_pct is None
    assert _only_row(_cmp(_lat([0, 0, 0]), _lat([0, 0, 0]), policy)).verdict == "pass"
    hb = _case({"tput": make_metric(unit="ratio", better="higher", value=0)})
    assert _only_row(_cmp(hb, hb, policy)).verdict == "pass"
    md = render_markdown(_cmp(_lat([0, 0, 0]), _lat([5, 5, 5]), policy))
    assert "latency_ms" in md


def test_noisy_low_power(policy):
    row = _only_row(_cmp(_lat([100, 100]), _lat([130, 130]), policy))
    assert row.verdict == "low_power" and not row.blocking
    report = _cmp(_lat([100, 100]), _lat([200, 200]), policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and row.blocking
    assert report.exit_code == 1


def test_noisy_value_only(policy):
    def rtf(v):
        return _case({"rtf": make_metric(unit="ratio", better="lower", value=v)})

    assert _only_row(_cmp(rtf(0.10), rtf(0.11), policy)).verdict == "pass"
    assert _only_row(_cmp(rtf(0.10), rtf(0.20), policy)).blocking


# --------------------------------------------------------------------------
# Compare: absolute caps
# --------------------------------------------------------------------------

TINY_CPU = {"engine": "whisper-cpp", "model": "tiny.en", "provider": "cpu"}


def test_cap_blocks_even_if_baseline_was_slow(policy):
    base = _lat([3500] * 5, dims=TINY_CPU)
    cand = _lat([3400] * 5, dims=TINY_CPU)
    row = _only_row(_cmp(base, cand, policy))
    assert row.verdict == "regressed" and row.blocking
    assert "3 s" in row.note


def test_cap_respects_dims(policy):
    base_dims = dict(TINY_CPU, model="base.en")
    row = _only_row(_cmp(_lat([3500] * 5, dims=base_dims), _lat([3400] * 5, dims=base_dims), policy))
    assert row.verdict == "pass"
    gpu = dict(TINY_CPU, provider="cuda")
    assert _only_row(_cmp(_lat([3500] * 5, dims=gpu), _lat([3400] * 5, dims=gpu), policy)).verdict == "pass"
    assert _only_row(_cmp(_lat([900] * 5, dims=TINY_CPU), _lat([950] * 5, dims=TINY_CPU), policy)).verdict == "pass"


def test_cap_applies_to_new_case(policy):
    other = _wer(0.1)
    cand = _lat([3200] * 5, dims=TINY_CPU, case="new")
    report = compare_results(_result(other), _result(other, cand), policy)
    rows = [r for r in report.rows if r.case == "new"]
    assert rows[0].verdict == "regressed" and rows[0].blocking
    ok = _lat([800] * 5, dims=TINY_CPU, case="new")
    report = compare_results(_result(other), _result(other, ok), policy)
    row = next(r for r in report.rows if r.case == "new")
    assert row.verdict == "pass" and row.note == "new case"


# --------------------------------------------------------------------------
# Compare: case status handling
# --------------------------------------------------------------------------


def _ne(reason="no GPU", case="c1"):
    return make_case(suite="stt", case=case, dims={}, status="not_exercised", error=reason)


def test_candidate_failed_blocks(policy):
    cand = make_case(suite="stt", case="c1", dims={}, status="failed", error="model crashed")
    report = _cmp(_lat([100] * 3), cand, policy)
    row = _only_row(report)
    assert row.verdict == "regressed" and row.blocking and "model crashed" in row.note


def test_not_exercised_both_sides(policy):
    report = _cmp(_ne(), _ne(), policy)
    row = _only_row(report)
    assert row.verdict == "not_exercised" and not row.blocking and not row.warning
    assert report.status == "PASS"


def test_not_exercised_after_ok_is_noted(policy):
    report = _cmp(_lat([100] * 3), _ne("runner lost GPU"), policy)
    row = _only_row(report)
    assert row.verdict == "not_exercised" and not row.blocking and row.warning
    assert "was ok in baseline" in row.note
    assert report.exit_code == 0 and report.status == "WARN"


def test_missing_in_candidate_blocks(policy):
    report = compare_results(
        _result(_lat([100] * 3), _wer(0.1) | {"case": "c2"}), _result(_lat([100] * 3)), policy
    )
    assert report.rows[0].verdict == "pass"
    missing = [r for r in report.rows if r.note == "missing in candidate"]
    assert len(missing) == 1 and missing[0].blocking
    assert report.exit_code == 1


def test_missing_metric_in_candidate_blocks(policy):
    base = _case({
        "latency_ms": make_metric(unit="ms", better="lower", samples=[1, 1, 1]),
        "wer": make_metric(unit="ratio", better="lower", value=0.1),
    })
    cand = _lat([1, 1, 1])
    report = _cmp(base, cand, policy)
    assert [r.verdict for r in report.rows] == ["pass", "regressed"]
    assert report.exit_code == 1


def test_case_order_is_preserved(policy):
    a, b, c = (_wer(0.1) | {"case": n} for n in "abc")
    report = compare_results(_result(a, b), _result(c, b, a), policy)
    assert [r.case for r in report.rows] == ["a", "b", "c"]


def test_compare_rejects_invalid_inputs(policy):
    bad = _result(_wer(0.1))
    bad["schema"] = "x"
    with pytest.raises(ValueError, match="baseline"):
        compare_results(bad, _result(_wer(0.1)), policy)
    with pytest.raises(ValueError, match="candidate"):
        compare_results(_result(_wer(0.1)), bad, policy)


# --------------------------------------------------------------------------
# Markdown / report
# --------------------------------------------------------------------------


def test_markdown_report(policy):
    base = _result(
        _lat([100] * 5, dims=TINY_CPU),
        _wer(0.10) | {"case": "w"},
        _lat([100 * 1024**2] * 3, unit="bytes", name="rss_loaded", case="m"),
    )
    cand = _result(
        _lat([130] * 5, dims=TINY_CPU),
        _wer(0.20) | {"case": "w"},
        _lat([101 * 1024**2] * 3, unit="bytes", name="rss_loaded", case="m"),
    )
    report = compare_results(base, cand, policy)
    md = report.summary_md()
    assert md.startswith("**Perf compare: FAIL**")
    assert "1 blocking, 1 warnings" in md
    assert "| Suite | Case | Metric | Baseline | Candidate | Delta | Verdict |" in md
    assert "100.0 MiB" in md and "130 ms" in md
    assert "+30.0 ms (+30.0%)" in md
    assert "0.1000" in md and "0.2000" in md
    d = report.to_dict()
    assert d["status"] == "FAIL" and d["exit_code"] == 1
    json.dumps(d)  # serialisable


@pytest.mark.parametrize(
    ("value", "unit", "text"),
    [
        (None, "ms", "–"),
        (12.345, "ms", "12.3 ms"),
        (1234.5, "ms", "1234 ms"),
        (3 * 1024**2, "bytes", "3.0 MiB"),
        (2048, "bytes", "2.0 KiB"),
        (5 * 1024**3, "bytes", "5.00 GiB"),
        (100, "bytes", "100 B"),
        (0.0312, "ratio", "0.0312"),
        (1.0, "bool", "true"),
        (0.0, "bool", "false"),
        (12.5, "us", "12.5 µs"),
        (1.5, "s", "1.50 s"),
        (3, "count", "3 count"),
    ],
)
def test_format_value(value, unit, text):
    assert format_value(value, unit) == text


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _write(path: Path, data: dict) -> Path:
    path.write_text(json.dumps(data), encoding="utf-8")
    return path


def _cli(*args: str, cwd: Path = REPO_ROOT) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "evals.perf", *args],
        cwd=cwd, capture_output=True, text=True, timeout=60, check=False,
    )


def test_cli_help():
    proc = _cli("--help")
    assert proc.returncode == 0
    for cmd in ("compare", "validate", "run"):
        assert cmd in proc.stdout


def test_cli_compare_exit_codes(tmp_path):
    base = _write(tmp_path / "b.json", _result(_lat([100] * 5)))
    warn = _write(tmp_path / "w.json", _result(_lat([130] * 5)))
    slow = _write(tmp_path / "s.json", _result(_lat([200] * 5)))
    report = tmp_path / "out" / "report.md"
    out_json = tmp_path / "out" / "report.json"

    proc = _cli("compare", "--baseline", str(base), "--candidate", str(base))
    assert proc.returncode == 0, proc.stderr
    assert "**Perf compare: PASS**" in proc.stdout

    proc = _cli("compare", "--baseline", str(base), "--candidate", str(warn),
                "--output", str(report), "--json", str(out_json))
    assert proc.returncode == 0, proc.stderr
    assert "WARN" in proc.stdout
    assert report.read_text() == proc.stdout
    assert json.loads(out_json.read_text())["status"] == "WARN"

    proc = _cli("compare", "--baseline", str(base), "--candidate", str(warn), "--strict")
    assert proc.returncode == 1

    proc = _cli("compare", "--baseline", str(base), "--candidate", str(slow),
                "--policy", str(DEFAULT_POLICY_PATH))
    assert proc.returncode == 1
    assert "FAIL" in proc.stdout


def test_cli_compare_invalid_input(tmp_path):
    good = _write(tmp_path / "g.json", _result(_lat([100] * 5)))
    garbage = tmp_path / "x.json"
    garbage.write_text("{not json")
    invalid = _write(tmp_path / "i.json", {"schema": "nope"})
    for cand in (garbage, invalid, tmp_path / "missing.json"):
        proc = _cli("compare", "--baseline", str(good), "--candidate", str(cand))
        assert proc.returncode == 2, (cand, proc.stdout)
        assert "error" in proc.stderr
    bad_policy = tmp_path / "p.toml"
    bad_policy.write_text("[[caps]]\nsuite = 1\n")
    proc = _cli("compare", "--baseline", str(good), "--candidate", str(good),
                "--policy", str(bad_policy))
    assert proc.returncode == 2


def test_cli_validate(tmp_path):
    good = _write(tmp_path / "g.json", _valid())
    proc = _cli("validate", str(good))
    assert proc.returncode == 0 and "ok" in proc.stdout
    bad = _write(tmp_path / "b.json", _valid() | {"cases": [{"suite": "stt"}]})
    proc = _cli("validate", str(bad))
    assert proc.returncode == 2 and "case" in proc.stderr


def test_cli_run_emits_valid_result(tmp_path):
    out = tmp_path / "r" / "result.json"
    proc = _cli("run", "--tier", "smoke", "--runtime", "rust", "--output", str(out),
                "--runner-label", "ci-test")
    assert proc.returncode == 0, proc.stderr
    data = load_result(out)
    assert validate_result(data) == []
    assert data["run"]["tier"] == "smoke" and data["run"]["runtime"] == "rust"
    assert data["run"]["runner"]["label"] == "ci-test"
    assert {c["suite"] for c in data["cases"]} == set(registry.PLANNED_SUITES)
    assert all(c["status"] == "not_exercised" and c["error"] for c in data["cases"])

    proc = _cli("validate", str(out))
    assert proc.returncode == 0


def test_cli_run_unknown_suite_to_stdout():
    proc = _cli("run", "--tier", "nightly", "--runtime", "python", "--suite", "warp-drive")
    assert proc.returncode == 0, proc.stderr
    data = json.loads(proc.stdout)
    assert validate_result(data) == []
    (case,) = data["cases"]
    assert case["suite"] == "warp-drive" and case["status"] == "not_exercised"
    assert case["error"] == "suite not implemented yet"


def test_cli_bad_args():
    assert _cli("run", "--tier", "weekly", "--runtime", "python").returncode == 2
    assert _cli().returncode == 2


def test_run_suites_with_registered_suite(monkeypatch, tmp_path):
    monkeypatch.setattr(registry, "SUITES", {})
    monkeypatch.setattr(cli, "SUITES", registry.SUITES)

    @registry.register_suite("stt")
    def fake_stt(ctx):
        return [_lat([float(len(ctx.tier))] * 3, case=f"{ctx.runtime}/tiny")]

    @registry.register_suite("recall")
    def broken(ctx):
        raise RuntimeError("db missing")

    with pytest.raises(ValueError):
        registry.register_suite("stt")(lambda ctx: [])

    out = tmp_path / "res.json"
    code = cli.main(["run", "--tier", "smoke", "--runtime", "python",
                     "--suite", "stt,recall", "--suite", "meetings", "--output", str(out)])
    assert code == 0
    cases = {(c["suite"], c["case"]): c for c in load_result(out)["cases"]}
    assert cases[("stt", "python/tiny")]["status"] == "ok"
    assert cases[("recall", "*")]["status"] == "failed"
    assert "db missing" in cases[("recall", "*")]["error"]
    assert cases[("meetings", "*")]["error"] == "suite not implemented yet"
