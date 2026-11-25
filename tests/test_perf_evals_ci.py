"""Tests for the perf smoke CI: evals/perf/ci_report.py and perf-evals.yml."""

from __future__ import annotations

import ast
import json
import subprocess
import sys
from pathlib import Path

import pytest
import yaml

from evals.perf import ci_report
from evals.perf.ci_report import (
    GITHUB_COMMENT_LIMIT,
    STICKY_MARKER,
    combine_smoke_reports,
    evaluate_entry,
    parse_entry,
    truncate,
)
from evals.perf.compare import compare_results, load_policy, render_markdown
from evals.perf.schema import make_case, make_metric, make_result, make_run

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "perf-evals.yml"
CI_REPORT = REPO_ROOT / "evals" / "perf" / "ci_report.py"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


def _result(runtime: str, latency: list[float], *, wer: float = 0.0, status: str = "ok") -> dict:
    run = make_run(tier="smoke", runtime=runtime, run_id="r", sha="abc", os_name="linux", arch="x86_64")
    if status == "ok":
        case = make_case(
            suite="stt", case="whisper/clip1", dims={"clip": "clip1"},
            metrics={
                "latency_ms": make_metric(unit="ms", better="lower", samples=latency),
                "wer": make_metric(unit="ratio", better="lower", value=wer),
            },
        )
    else:
        case = make_case(suite="stt", case="whisper/clip1", dims={"clip": "clip1"}, status=status, error="boom")
    return make_result(run, [case])


def _write_compare(tmp: Path, rt: str, baseline: dict, candidate: dict) -> dict:
    report = compare_results(baseline, candidate, load_policy())
    compare = tmp / f"compare-{rt}.json"
    md = tmp / f"report-{rt}.md"
    cand = tmp / f"candidate-{rt}.json"
    compare.write_text(json.dumps(report.to_dict()), encoding="utf-8")
    md.write_text(render_markdown(report), encoding="utf-8")
    cand.write_text(json.dumps(candidate), encoding="utf-8")
    return {"runtime": rt, "compare_json": compare, "report_md": md, "candidate_json": cand, "note": None}


BASE = [100.0, 101.0, 99.0, 100.0, 100.5]
SAME = [100.0, 100.5, 99.5, 100.0, 101.0]
WARN_SLOWER = [130.0, 131.0, 129.0, 130.0, 130.5]  # +30 %: noisy warn
FAIL_SLOWER = [250.0, 251.0, 249.0, 250.0, 250.5]  # 2.5x: hard block


@pytest.fixture
def pass_entry(tmp_path):
    return _write_compare(tmp_path, "python", _result("python", BASE), _result("python", SAME))


# --------------------------------------------------------------------------
# combine_smoke_reports
# --------------------------------------------------------------------------


def test_pass_pass_is_pass(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", SAME))
    md, code = combine_smoke_reports([pass_entry, rust], sha="a" * 40, base_sha="b" * 40)
    assert code == 0
    assert md.startswith(STICKY_MARKER)
    assert "Perf smoke: **PASS**" in md
    assert "`aaaaaaaaaaaa`" in md and "`bbbbbbbbbbbb`" in md
    assert "`python` — PASS" in md and "`rust` — PASS" in md
    assert "| Suite | Case | Metric |" in md  # embedded compare table


def test_pass_warn_is_warn(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", WARN_SLOWER))
    assert json.loads(rust["compare_json"].read_text())["status"] == "WARN"
    md, code = combine_smoke_reports([pass_entry, rust])
    assert code == 0
    assert "Perf smoke: **WARN**" in md
    assert "`rust` — WARN" in md


def test_pass_fail_is_fail(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", FAIL_SLOWER))
    md, code = combine_smoke_reports([pass_entry, rust])
    assert code == 1
    assert "Perf smoke: **FAIL**" in md
    assert "`rust` — FAIL" in md


def test_quality_regression_fails(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", SAME, wer=0.5))
    _, code = combine_smoke_reports([pass_entry, rust])
    assert code == 1


def test_blocking_count_forces_fail_even_if_status_says_pass(tmp_path):
    path = tmp_path / "c.json"
    path.write_text(json.dumps({"status": "PASS", "counts": {"blocking": 2}}), encoding="utf-8")
    ev = evaluate_entry({"runtime": "rust", "compare_json": path})
    assert ev["status"] == "FAIL"


def test_unknown_compare_status_fails_closed(tmp_path):
    path = tmp_path / "c.json"
    path.write_text(json.dumps({"status": "weird"}), encoding="utf-8")
    assert evaluate_entry({"runtime": "rust", "compare_json": path})["status"] == "FAIL"


def test_missing_compare_json_fails(tmp_path):
    md, code = combine_smoke_reports(
        [{"runtime": "rust", "compare_json": tmp_path / "nope.json", "candidate_json": None}]
    )
    assert code == 1
    assert "compare report" in md and "not found" in md


def test_bootstrap_candidate_only_passes_with_note(tmp_path):
    cand = tmp_path / "candidate-rust.json"
    cand.write_text(json.dumps(_result("rust", SAME)), encoding="utf-8")
    note = "Bootstrap: merge-base 0123456789ab stt_bench has no --manifest; no Rust baseline."
    md, code = combine_smoke_reports(
        [{"runtime": "rust", "compare_json": None, "report_md": None, "candidate_json": cand, "note": note}]
    )
    assert code == 0
    assert "Perf smoke: **PASS**" in md
    assert "No baseline compare" in md
    assert note in md
    assert "1 ok, 0 failed, 0 not_exercised" in md


def test_bootstrap_failed_candidate_case_warns(tmp_path):
    cand = tmp_path / "candidate-python.json"
    cand.write_text(json.dumps(_result("python", SAME, status="failed")), encoding="utf-8")
    md, code = combine_smoke_reports([{"runtime": "python", "candidate_json": cand, "note": "bootstrap"}])
    assert code == 0
    assert "Perf smoke: **WARN**" in md
    assert "stt/whisper/clip1: boom" in md


def test_bootstrap_without_candidate_fails(tmp_path):
    md, code = combine_smoke_reports(
        [{"runtime": "python", "candidate_json": tmp_path / "missing.json", "note": "bootstrap"}]
    )
    assert code == 1
    assert "candidate result" in md


def test_no_entries_is_pass():
    md, code = combine_smoke_reports([])
    assert code == 0 and md.startswith(STICKY_MARKER)


def test_huge_report_is_truncated_under_github_limit(tmp_path, pass_entry):
    big = tmp_path / "big.md"
    big.write_text("| row |\n" * 40000, encoding="utf-8")  # ~320k chars
    rust = dict(pass_entry, runtime="rust", report_md=big)
    md, _ = combine_smoke_reports([pass_entry, rust], sha="a" * 40)
    assert len(md) < GITHUB_COMMENT_LIMIT
    assert len(md) <= ci_report.MAX_REPORT_CHARS
    assert md.startswith(STICKY_MARKER)
    assert "truncated" in md and "artifact" in md
    assert "`python` — PASS" in md and "`rust` — PASS" in md  # both headers survive
    assert md.count("<details>") == md.count("</details>")


def test_huge_note_is_bounded(tmp_path):
    cand = tmp_path / "c.json"
    cand.write_text(json.dumps(_result("rust", SAME)), encoding="utf-8")
    md, _ = combine_smoke_reports([{"runtime": "rust", "candidate_json": cand, "note": "x" * 200000}])
    assert len(md) <= ci_report.MAX_REPORT_CHARS


def test_truncate_keeps_short_text_and_cuts_on_line_boundary():
    assert truncate("abc", 10) == "abc"
    out = truncate("line1\nline2\nline3\n" * 100, 120)
    assert len(out) <= 120
    assert "truncated" in out


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def test_parse_entry_note_keeps_commas():
    entry = parse_entry("runtime=rust,candidate=c.json,note=a, b, c=d")
    assert entry["runtime"] == "rust"
    assert entry["candidate_json"] == Path("c.json")
    assert entry["compare_json"] is None
    assert entry["note"] == "a, b, c=d"


def test_parse_entry_note_file(tmp_path):
    note = tmp_path / "note.txt"
    note.write_text("from file, with comma\n", encoding="utf-8")
    entry = parse_entry(f"runtime=python,note_file={note}")
    assert entry["note"] == "from file, with comma"


@pytest.mark.parametrize("spec", ["runtime=go", "candidate=x.json", "runtime=rust,bogus=1", "runtime=rust,oops"])
def test_parse_entry_rejects_bad_specs(spec):
    with pytest.raises(ValueError):
        parse_entry(spec)


def _cli_args(tmp_path: Path, fail_entry: dict, pass_entry: dict) -> list[str]:
    return [
        "--output", str(tmp_path / "out" / "report.md"),
        "--sha", "c" * 40,
        "--base-sha", "d" * 40,
        "--status-json", str(tmp_path / "out" / "status.json"),
        "--entry",
        (
            f"runtime=python,compare={pass_entry['compare_json']},report={pass_entry['report_md']},"
            f"candidate={pass_entry['candidate_json']}"
        ),
        "--entry",
        (
            f"runtime=rust,compare={fail_entry['compare_json']},report={fail_entry['report_md']},"
            f"candidate={fail_entry['candidate_json']},note=slow, on purpose"
        ),
    ]


def test_cli_fail_exit_code_and_status_json(tmp_path, pass_entry, capsys):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", FAIL_SLOWER))
    code = ci_report.main(_cli_args(tmp_path, rust, pass_entry))
    assert code == 1
    status = json.loads((tmp_path / "out" / "status.json").read_text(encoding="utf-8"))
    assert status["status"] == "FAIL" and status["exit_code"] == 1
    assert status["runtimes"]["rust"]["status"] == "FAIL"
    assert status["runtimes"]["python"]["status"] == "PASS"
    assert status["runtimes"]["rust"]["note"] == "slow, on purpose"
    report = (tmp_path / "out" / "report.md").read_text(encoding="utf-8")
    assert report.startswith(STICKY_MARKER)
    assert STICKY_MARKER in capsys.readouterr().out


def test_cli_no_fail_exits_zero_but_records_fail(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", FAIL_SLOWER))
    code = ci_report.main([*_cli_args(tmp_path, rust, pass_entry), "--no-fail"])
    assert code == 0
    status = json.loads((tmp_path / "out" / "status.json").read_text(encoding="utf-8"))
    assert status["status"] == "FAIL" and status["exit_code"] == 1


def test_cli_pass_exits_zero(tmp_path, pass_entry):
    rust = _write_compare(tmp_path, "rust", _result("rust", BASE), _result("rust", SAME))
    assert ci_report.main(_cli_args(tmp_path, rust, pass_entry)) == 0


def test_cli_bad_entry_is_usage_error(tmp_path):
    assert ci_report.main(["--output", str(tmp_path / "r.md"), "--entry", "runtime=nope"]) == 2


def test_cli_runs_as_module(tmp_path):
    cand = tmp_path / "candidate-rust.json"
    cand.write_text(json.dumps(_result("rust", SAME)), encoding="utf-8")
    proc = subprocess.run(
        [sys.executable, "-m", "evals.perf.ci_report", "--output", str(tmp_path / "r.md"),
         "--entry", f"runtime=rust,candidate={cand},note=bootstrap"],
        cwd=REPO_ROOT, capture_output=True, text=True, encoding="utf-8", check=False,
    )
    assert proc.returncode == 0, proc.stderr
    assert (tmp_path / "r.md").read_text(encoding="utf-8").startswith(STICKY_MARKER)


# --------------------------------------------------------------------------
# Stdlib-only
# --------------------------------------------------------------------------


def test_ci_report_imports_only_stdlib():
    tree = ast.parse(CI_REPORT.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom):
            if node.level:
                continue
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            top = mod.split(".")[0]
            assert top != "localflow"
            assert top in STDLIB_OK, f"ci_report imports non-stdlib {mod}"


def test_stt_stdlib_check_covers_ci_report():
    # tests/test_perf_evals_stt.py parametrizes its recursive stdlib check over
    # this exact selection; ci_report.py must be in it (only benches/ is exempt).
    stt_tests = (REPO_ROOT / "tests" / "test_perf_evals_stt.py").read_text(encoding="utf-8")
    assert 'PERF_DIR.rglob("*.py") if "benches" not in' in stt_tests
    perf_dir = CI_REPORT.parent
    harness = [p for p in perf_dir.rglob("*.py") if "benches" not in p.relative_to(perf_dir).parts]
    assert CI_REPORT in harness


# --------------------------------------------------------------------------
# Workflow structure
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def wf() -> dict:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


@pytest.fixture(scope="module")
def steps(wf) -> list[dict]:
    return wf["jobs"]["smoke"]["steps"]


def _on(wf: dict) -> dict:
    return wf.get("on", wf.get(True))  # PyYAML 1.1 parses bare `on` as True


def _step(steps: list[dict], *, uses: str | None = None, name: str | None = None) -> dict:
    for step in steps:
        if uses and str(step.get("uses", "")).startswith(uses):
            return step
        if name and step.get("name") == name:
            return step
    raise AssertionError(f"no step uses={uses} name={name}")


def _all_run(steps: list[dict]) -> str:
    return "\n".join(str(s.get("run", "")) for s in steps)


def test_workflow_name_and_triggers(wf):
    assert wf["name"] == "Perf evals"
    on = _on(wf)
    assert on["pull_request"]["paths"] == [
        "localflow/**", "rust/**", "evals/**", ".github/workflows/perf-evals.yml",
    ]
    inputs = on["workflow_dispatch"]["inputs"]
    assert inputs["suites"]["default"] == "stt"
    assert inputs["runtimes"]["default"] == "python,rust"
    assert not inputs["suites"].get("required") and not inputs["runtimes"].get("required")


def test_workflow_permissions_and_concurrency(wf):
    assert wf["permissions"] == {"contents": "read", "pull-requests": "write"}
    assert wf["concurrency"]["group"] == (
        "perf-evals-${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}"
    )
    assert wf["concurrency"]["cancel-in-progress"] is True


def test_workflow_job_env(wf):
    job = wf["jobs"]["smoke"]
    assert job["runs-on"] == "ubuntu-latest"
    assert job["timeout-minutes"] == 25
    env = job["env"]
    assert env["CARGO_TERM_COLOR"] == "always"
    assert env["HF_HUB_CACHE"] == "${{ github.workspace }}/.hf-cache/hub"
    assert env["LF_PERF_CORPUS_CACHE"] == "${{ github.workspace }}/.perf-cache/corpus"
    # The harness reads LF_PERF_CACHE_DIR; it must land inside the cached .perf-cache.
    assert env["LF_PERF_CACHE_DIR"].startswith("${{ github.workspace }}/.perf-cache")


def test_workflow_untrusted_inputs_not_interpolated_into_scripts(steps):
    for step in steps:
        assert "${{ inputs." not in str(step.get("run", ""))
        assert "${{ github.event.pull_request.title" not in str(step.get("run", ""))


def test_workflow_setup_steps(steps):
    checkout = _step(steps, uses="actions/checkout@v4")
    assert checkout["with"]["fetch-depth"] == 0
    assert _step(steps, uses="actions/setup-python@v5")["with"]["python-version"] == "3.12"
    _step(steps, uses="dtolnay/rust-toolchain@stable")
    assert _step(steps, uses="Swatinem/rust-cache@v2")["with"]["workspaces"] == "rust"
    run = _all_run(steps)
    assert "sudo apt-get update && sudo apt-get install -y --no-install-recommends ffmpeg" in run
    assert "pip install -e ." in run
    assert "cargo build --manifest-path rust/Cargo.toml --locked --release -p lf-stt --example stt_bench" in run


def test_workflow_caches(steps):
    caches = {s["with"]["path"]: s["with"]["key"] for s in steps if str(s.get("uses", "")).startswith("actions/cache@v4")}
    assert caches[".hf-cache"] == "perf-hf-v1-${{ runner.os }}-${{ runner.arch }}"
    assert caches[".perf-cache"] == "perf-corpus-v1-${{ hashFiles('evals/perf/corpus/*.json') }}"


def test_workflow_candidate_runs(steps):
    run = _step(steps, name="Run candidate smoke suites")["run"]
    assert "python -m evals.perf run --tier smoke --runtime \"$rt\"" in run
    assert "--runner-label ubuntu-latest" in run
    assert "perf-results/candidate-$rt.json" in run


def test_workflow_same_runner_ab_worktree(steps):
    step = _step(steps, name="Same-runner A/B baseline (merge-base)")
    assert step["if"] == "github.event_name == 'pull_request'"
    run = step["run"]
    assert 'git merge-base "origin/${GITHUB_BASE_REF}" HEAD' in run
    assert "git worktree add --detach ../perf-baseline" in run
    assert "baseline-$rt.json" in run
    assert "Bootstrap:" in run  # predates evals/perf or --manifest: note, not failure
    assert "manifest" in run and "stt_bench" in run
    cleanup = _step(steps, name="Remove baseline worktree")
    assert "always()" in cleanup["if"]
    assert "git worktree remove --force ../perf-baseline || true" in cleanup["run"]


def test_workflow_compare_and_combine(steps):
    step = _step(steps, name="Compare and combine")
    assert "always()" in step["if"]
    run = step["run"]
    assert "python -m evals.perf compare" in run
    assert "--output \"perf-results/report-$rt.md\" --json \"perf-results/compare-$rt.json\"" in run
    assert "set -e" not in run.replace("set -uo", "")  # compare failures must not abort the step
    assert "python -m evals.perf.ci_report" in run
    assert "--status-json perf-results/status.json" in run and "--no-fail" in run
    assert "perf-results/report.md >> \"$GITHUB_STEP_SUMMARY\"" in run


def test_workflow_artifact_upload(steps):
    up = _step(steps, uses="actions/upload-artifact@v4")
    assert up["if"] == "always()"
    assert up["with"]["name"] == "perf-smoke-${{ runner.os }}-${{ runner.arch }}"
    assert up["with"]["path"].rstrip("/") == "perf-results"


def test_workflow_sticky_comment_with_fork_guard(steps):
    step = _step(steps, uses="actions/github-script@v7")
    cond = step["if"]
    assert "always()" in cond
    assert "github.event_name == 'pull_request'" in cond
    assert "github.event.pull_request.head.repo.full_name == github.repository" in cond
    script = step["with"]["script"]
    assert STICKY_MARKER in script
    assert "updateComment" in script and "createComment" in script
    assert "perf-results/report.md" in script


def test_workflow_gate_is_last_and_reads_status(steps):
    gate = steps[-1]
    assert gate["if"] == "always()"
    assert "perf-results/status.json" in gate["run"]
    assert "exit_code" in gate["run"]
    names = [s.get("name") or s.get("uses") for s in steps]
    assert names.index("Sticky PR comment") < names.index(gate["name"])
    assert names.index("Upload perf results") < names.index(gate["name"])


def test_workflow_does_not_touch_other_workflows():
    for name in ("rust-bench.yml", "rust.yml"):
        assert "perf-evals" not in (REPO_ROOT / ".github" / "workflows" / name).read_text(encoding="utf-8")
