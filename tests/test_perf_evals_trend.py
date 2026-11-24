"""Tests for the nightly trend store, regression detection and issue sync (PR 8)."""

from __future__ import annotations

import ast
import json
import subprocess
import sys
from pathlib import Path

import pytest
import yaml

from evals.perf import regression_issues as ri
from evals.perf import trend
from evals.perf.__main__ import main as perf_main
from evals.perf.schema import make_case, make_metric, make_result, make_run

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "perf-evals.yml"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
RUNNER = {"label": "ubuntu-latest", "cpu": "x", "cores": 4, "ram_gb": 16}


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


def _ts(day: int) -> str:
    return f"2026-11-{day:02d}T07:00:00Z"


def _result(
    *,
    runtime: str = "rust",
    day: int = 1,
    sha: str | None = None,
    os_name: str = "linux",
    label: str = "ubuntu-latest",
    tier: str = "nightly",
    run_id: str | None = None,
    cases: list[dict] | None = None,
    latency: float = 100.0,
) -> dict:
    run = make_run(
        tier=tier, runtime=runtime, run_id=run_id or f"run-{runtime}-{day}", sha=sha or f"{day:02d}" + "a" * 38,
        ref="refs/heads/main", os_name=os_name, arch="x86_64", runner={**RUNNER, "label": label},
        started_at=_ts(day),
    )
    if cases is None:
        cases = [
            make_case(
                suite="stt", case="whisper-tiny.en-cpu/clip-01", dims={"model": "tiny.en"},
                metrics={
                    "latency_ms": make_metric(unit="ms", better="lower", samples=[latency] * 5),
                    "wer": make_metric(unit="ratio", better="lower", value=0.05),
                },
            ),
            make_case(suite="stt", case="whisper-base.en-cpu/clip-01", dims={}, status="not_exercised",
                      error="model not cached"),
        ]
    return make_result(run, cases)


def _store_series(store: Path, values: list[float], *, runtime: str = "rust", start_day: int = 1) -> Path:
    for i, v in enumerate(values):
        trend.append_run(store, _result(runtime=runtime, day=start_day + i, latency=v))
    return store / trend.INDEX_NAME


# --------------------------------------------------------------------------
# append_run
# --------------------------------------------------------------------------


def test_append_run_writes_run_file_and_index(tmp_path):
    res = _result()
    info = trend.append_run(tmp_path, res)
    sha12 = res["run"]["sha"][:12]
    assert info["run_file"] == f"runs/2026-11-01-{sha12}-linux-rust.json"
    assert info == {**info, "replaced": False, "appended": 2, "skipped": 0}
    stored = json.loads((tmp_path / info["run_file"]).read_text(encoding="utf-8"))
    assert stored["run"]["id"] == res["run"]["id"]

    lines = (tmp_path / "index.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(lines) == 2
    assert all(": " not in line for line in lines)  # compact
    first = json.loads(lines[0])
    assert set(first) == {"ts", "date", "sha", "os", "runner_label", "runtime", "tier", "suite",
                          "case", "status", "metrics"}
    assert first["date"] == "2026-11-01" and first["os"] == "linux" and first["runner_label"] == "ubuntu-latest"
    assert first["metrics"]["latency_ms"] == {"value": 100.0, "median": 100.0, "p95": 100.0,
                                              "unit": "ms", "better": "lower"}
    assert first["metrics"]["wer"]["value"] == 0.05 and first["metrics"]["wer"]["median"] is None
    second = json.loads(lines[1])
    assert second["status"] == "not_exercised" and second["metrics"] == {}


def test_append_run_is_idempotent(tmp_path):
    res = _result()
    trend.append_run(tmp_path, res)
    again = trend.append_run(tmp_path, res)
    assert again["replaced"] is True
    assert again["appended"] == 0 and again["skipped"] == 2
    assert len(list((tmp_path / "runs").iterdir())) == 1
    assert len((tmp_path / "index.jsonl").read_text(encoding="utf-8").splitlines()) == 2


def test_append_run_collision_gets_suffix(tmp_path):
    a = _result(run_id="first")
    b = _result(run_id="second")
    b["run"]["started_at"] = "2026-11-01T09:30:00Z"  # same date/sha/os/runtime, different run
    trend.append_run(tmp_path, a)
    info = trend.append_run(tmp_path, b)
    assert info["run_file"].endswith("-linux-rust-2.json")
    assert info["appended"] == 2
    assert json.loads((tmp_path / "runs" / Path(info["run_file"]).name).read_text())["run"]["id"] == "second"
    # The first file was not clobbered.
    first = tmp_path / info["run_file"].replace("-2.json", ".json")
    assert json.loads(first.read_text())["run"]["id"] == "first"


def test_append_run_from_path_and_rejects_invalid(tmp_path):
    path = tmp_path / "r.json"
    path.write_text(json.dumps(_result(runtime="python")), encoding="utf-8")
    assert trend.append_run(tmp_path / "store", path)["run_file"].endswith("-linux-python.json")
    with pytest.raises(ValueError, match="lf-perf-result/v1"):
        trend.append_run(tmp_path / "store", {"schema": "nope"})


def test_append_run_repairs_missing_trailing_newline(tmp_path):
    trend.append_run(tmp_path, _result(day=1))
    index = tmp_path / "index.jsonl"
    index.write_text(index.read_text(encoding="utf-8").rstrip("\n"), encoding="utf-8")
    trend.append_run(tmp_path, _result(day=2))
    assert len(trend.read_index(index)) == 4


# --------------------------------------------------------------------------
# render_trend
# --------------------------------------------------------------------------


def test_case_base_strips_runtime_segment():
    assert trend.case_base("rust/1k", "rust") == "1k"
    assert trend.case_base("text/python", "python") == "text"
    assert trend.case_base("http/rust/tiny.en") == "http/tiny.en"
    assert trend.case_base("idle/rust") == "idle"
    assert trend.case_base("whisper-tiny.en-cpu/clip-01", "rust") == "whisper-tiny.en-cpu/clip-01"


def test_render_trend_window_and_stats(tmp_path):
    values = [float(100 + i) for i in range(35)]
    index = tmp_path / "index.jsonl"
    for i, v in enumerate(values):
        r = _result(day=1, latency=v, run_id=f"r{i}")
        r["run"]["started_at"] = f"2026-11-01T{i // 60:02d}:{i % 60:02d}:00Z"
        trend.append_run(tmp_path, r)
    md = trend.render_trend(index)
    row = next(line for line in md.splitlines() if "| latency_ms |" in line and "clip-01" in line)
    cells = [c.strip() for c in row.strip("|").split("|")]
    # Runner | Runtime | Case | Metric | Latest | Median | Min | Max | n | Δ prev | Trend
    assert cells[0] == "ubuntu-latest" and cells[1] == "rust"
    assert cells[4] == "134 ms"  # latest
    assert cells[6] == "105 ms" and cells[7] == "134 ms"  # min/max of the last 30, not all 35
    assert cells[5] == "119.5 ms"
    assert cells[8] == "30"
    assert "worse" in cells[9] and cells[9].startswith("+0.8%")
    assert len(cells[10].strip("`")) == 30
    small = trend.render_trend(index, limit=3)
    assert "| 3 |" in small and "Window: last 3 runs" in small


def _pair_case(suite: str, case: str, metric: str, unit: str, better: str, value: float) -> dict:
    return make_case(suite=suite, case=case, dims={},
                     metrics={metric: make_metric(unit=unit, better=better, value=value)})


def test_render_trend_rust_python_ratio_table(tmp_path):
    rust_cases = [
        _pair_case("recall", "rust/1k", "search_ms", "ms", "lower", 2.0),
        _pair_case("footprint", "idle/rust", "rss_bytes", "bytes", "lower", 20 * 2**20),
        _pair_case("dictation", "text/rust", "fidelity_rate", "ratio", "higher", 0.99),
        _pair_case("stt", "whisper-tiny.en-cpu/clip-01", "latency_ms", "ms", "lower", 150.0),
    ]
    py_cases = [
        _pair_case("recall", "python/1k", "search_ms", "ms", "lower", 10.0),
        _pair_case("footprint", "idle/python", "rss_bytes", "bytes", "lower", 80 * 2**20),
        _pair_case("dictation", "text/python", "fidelity_rate", "ratio", "higher", 0.90),
        _pair_case("stt", "whisper-tiny.en-cpu/clip-01", "latency_ms", "ms", "lower", 100.0),
    ]
    trend.append_run(tmp_path, _result(runtime="python", day=1, cases=py_cases))
    # An older Rust run must lose to the newer one.
    trend.append_run(tmp_path, _result(runtime="rust", day=1, cases=[
        _pair_case("recall", "rust/1k", "search_ms", "ms", "lower", 50.0)]))
    trend.append_run(tmp_path, _result(runtime="rust", day=2, cases=rust_cases))
    # A Python value on another runner must not pair with ubuntu's Rust.
    trend.append_run(tmp_path, _result(runtime="python", day=3, os_name="macos", label="macos-latest",
                                       cases=[_pair_case("recall", "python/1k", "search_ms", "ms", "lower", 1.0)]))

    rows = {(r["suite"], r["case"], r["metric"]): r for r in trend.ratio_rows(trend.read_index(tmp_path / "index.jsonl"))}
    assert set(rows) == {
        ("recall", "1k", "search_ms"), ("footprint", "idle", "rss_bytes"),
        ("dictation", "text", "fidelity_rate"), ("stt", "whisper-tiny.en-cpu/clip-01", "latency_ms"),
    }
    assert rows[("recall", "1k", "search_ms")]["ratio"] == pytest.approx(0.2)
    assert rows[("recall", "1k", "search_ms")]["status"] == "faster"
    assert rows[("recall", "1k", "search_ms")]["os"] == "ubuntu-latest"
    assert rows[("footprint", "idle", "rss_bytes")]["ratio"] == pytest.approx(0.25)
    assert rows[("footprint", "idle", "rss_bytes")]["status"] == "lighter"
    assert rows[("dictation", "text", "fidelity_rate")]["ratio"] == pytest.approx(1.1)
    assert rows[("dictation", "text", "fidelity_rate")]["status"] == "better"  # higher is better
    assert rows[("stt", "whisper-tiny.en-cpu/clip-01", "latency_ms")]["status"] == "slower"

    md = trend.render_trend(tmp_path / "index.jsonl")
    assert "## Rust ÷ Python" in md
    assert "| ubuntu-latest | recall | `1k` | search_ms | 2 ms | 10 ms | 0.20x | faster |" in md
    assert "| 0.25x | lighter |" in md
    assert "| 1.10x | better |" in md
    assert "| 1.50x | slower |" in md


def test_ratio_status_directions():
    assert trend.ratio_status(0.5, "lower", "ms") == "faster"
    assert trend.ratio_status(2.0, "lower", "ms") == "slower"
    assert trend.ratio_status(0.5, "lower", "bytes") == "lighter"
    assert trend.ratio_status(2.0, "lower", "bytes") == "heavier"
    assert trend.ratio_status(2.0, "higher", "ratio") == "better"
    assert trend.ratio_status(0.5, "higher", "ratio") == "worse"
    assert trend.ratio_status(1.0, "lower", "ms") == "same"


def test_render_trend_empty_index(tmp_path):
    md = trend.render_trend(tmp_path / "missing.jsonl")
    assert "No metric data" in md and "No runner has both" in md


# --------------------------------------------------------------------------
# detect_consecutive_regressions
# --------------------------------------------------------------------------

KEY = "ubuntu-latest/rust/stt/whisper-tiny.en-cpu/clip-01/latency_ms"


def test_single_blip_does_not_trigger(tmp_path):
    index = _store_series(tmp_path, [100, 101, 99, 100, 140])
    report = trend.detect_consecutive_regressions(index)
    assert report["active_regressions"] == []
    assert [p["metric_key"] for p in report["pending"]] == [KEY]


def test_blip_then_healthy_does_not_trigger(tmp_path):
    index = _store_series(tmp_path, [100, 101, 140, 100, 140])
    assert trend.detect_consecutive_regressions(index)["active_regressions"] == []


def test_two_consecutive_regressions_trigger(tmp_path):
    index = _store_series(tmp_path, [100, 101, 99, 100, 130, 135])
    report = trend.detect_consecutive_regressions(index)
    assert len(report["active_regressions"]) == 1
    item = report["active_regressions"][0]
    assert item["metric_key"] == KEY
    assert item["baseline_val"] == pytest.approx(100.0)
    assert item["recent_vals"] == [130.0, 135.0]
    assert item["shas"] == ["05" + "a" * 38, "06" + "a" * 38]
    assert item["title"] == f"Perf regression: {KEY}"
    assert ri.marker(KEY) in item["body"]
    assert "130 ms" in item["body"] and "135 ms" in item["body"]
    assert report["recovered"] == []
    # Within rel_tol (15 %) and other metrics (wer is constant) never trigger.
    assert all(i["metric"] == "latency_ms" for i in report["active_regressions"])


def test_consecutive_three_needs_three(tmp_path):
    index = _store_series(tmp_path, [100, 100, 130, 135])
    assert trend.detect_consecutive_regressions(index, consecutive=3)["active_regressions"] == []
    index = _store_series(tmp_path, [136], start_day=5)
    assert len(trend.detect_consecutive_regressions(index, consecutive=3)["active_regressions"]) == 1


def test_recovery_after_regression(tmp_path):
    index = _store_series(tmp_path, [100, 100, 130, 135, 101])
    report = trend.detect_consecutive_regressions(index)
    assert report["active_regressions"] == []
    assert [r["metric_key"] for r in report["recovered"]] == [KEY]
    assert ri.marker(KEY) in report["recovered"][0]["body"]


def test_regressed_points_do_not_poison_baseline(tmp_path):
    # Slow creep in regressed points never becomes the baseline.
    index = _store_series(tmp_path, [100, 100, 130, 150, 170])
    item = trend.detect_consecutive_regressions(index)["active_regressions"][0]
    assert item["baseline_val"] == pytest.approx(100.0)


def test_higher_is_better_quality_metric(tmp_path):
    def run(day: int, rate: float) -> dict:
        case = _pair_case("dictation", "text/rust", "fidelity_rate", "ratio", "higher", rate)
        return _result(day=day, cases=[case])

    for day, rate in enumerate([0.99, 0.99, 0.90, 0.91], 1):
        trend.append_run(tmp_path, run(day, rate))
    report = trend.detect_consecutive_regressions(tmp_path / "index.jsonl")
    assert [i["metric_key"] for i in report["active_regressions"]] == [
        "ubuntu-latest/rust/dictation/text/rust/fidelity_rate"
    ]


def test_regressions_ignore_other_tiers_and_failed_cases(tmp_path):
    for day, v in enumerate([100, 100, 130, 135], 1):
        trend.append_run(tmp_path, _result(day=day, latency=v, tier="deep"))
    assert trend.detect_consecutive_regressions(tmp_path / "index.jsonl")["active_regressions"] == []
    assert len(trend.detect_consecutive_regressions(tmp_path / "index.jsonl", tier=None)["active_regressions"]) == 1


# --------------------------------------------------------------------------
# plan_issue_actions
# --------------------------------------------------------------------------


def _report(active: list[str] = (), recovered: list[str] = ()) -> dict:
    return {
        "active_regressions": [
            {"metric_key": k, "title": f"Perf regression: {k}", "body": f"{ri.marker(k)}\nregressed"} for k in active
        ],
        "recovered": [{"metric_key": k, "body": f"{ri.marker(k)}\nrecovered"} for k in recovered],
    }


def _issue(number: int, key: str, state: str = "open", body: str | None = None, title: str | None = None) -> dict:
    return {"number": number, "state": state, "title": title or f"Perf regression: {key}",
            "body": body if body is not None else f"{ri.marker(key)}\nold body"}


def test_plan_opens_new_issue():
    actions = ri.plan_issue_actions(_report(active=[KEY]), [])
    assert actions == [{
        "action": "open", "metric_key": KEY, "title": f"Perf regression: {KEY}",
        "body": f"{ri.marker(KEY)}\nregressed", "labels": ["perf-regression"],
    }]


def test_plan_updates_existing_open_issue():
    actions = ri.plan_issue_actions(_report(active=[KEY]), [_issue(7, KEY), _issue(3, "other/key")])
    assert [(a["action"], a["number"], a["reopen"]) for a in actions] == [("update", 7, False)]


def test_plan_skips_unchanged_open_issue():
    report = _report(active=[KEY])
    existing = [_issue(7, KEY, body=report["active_regressions"][0]["body"])]
    assert ri.plan_issue_actions(report, existing) == []


def test_plan_reopens_closed_issue_on_new_regression():
    actions = ri.plan_issue_actions(_report(active=[KEY]), [_issue(4, KEY, state="closed")])
    assert [(a["action"], a["number"], a["reopen"]) for a in actions] == [("update", 4, True)]


def test_plan_closes_open_issue_on_recovery():
    actions = ri.plan_issue_actions(_report(recovered=[KEY]), [_issue(9, KEY)])
    assert actions == [{"action": "close", "metric_key": KEY, "number": 9,
                        "comment": f"{ri.marker(KEY)}\nrecovered"}]


def test_plan_noop_when_already_closed_or_absent():
    assert ri.plan_issue_actions(_report(recovered=[KEY]), [_issue(9, KEY, state="closed")]) == []
    assert ri.plan_issue_actions(_report(recovered=[KEY]), []) == []


def test_plan_prefers_open_issue_and_ignores_pull_requests():
    existing = [_issue(2, KEY, state="closed"), _issue(5, KEY), {**_issue(8, KEY), "pull_request": {}}]
    actions = ri.plan_issue_actions(_report(recovered=[KEY]), existing)
    assert [(a["action"], a["number"]) for a in actions] == [("close", 5)]


def test_plan_caps_new_issues():
    keys = [f"k/{i}" for i in range(5)]
    actions = ri.plan_issue_actions(_report(active=keys), [], max_open=2)
    assert [a["action"] for a in actions] == ["open", "open", "skipped"]
    assert actions[-1]["metric_keys"] == keys[2:]


def test_issue_key_parses_marker():
    assert ri.issue_key({"body": f"hello\n{ri.marker(KEY)}\n"}) == KEY
    assert ri.issue_key({"body": "no marker"}) is None
    assert ri.issue_key({"body": None}) is None


class _FakeClient:
    def __init__(self):
        self.calls = []

    def request(self, method, path, payload=None):
        self.calls.append((method, path, payload))
        return {"number": 42} if (method, path) == ("POST", "/issues") else {}

    def ensure_label(self):
        self.calls.append(("LABEL",))


def test_sync_github_issues_calls_api():
    actions = ri.plan_issue_actions(
        _report(active=[KEY, "b/key"], recovered=["c/key"]),
        [_issue(4, "b/key", state="closed"), _issue(9, "c/key")],
    )
    client = _FakeClient()
    results = ri.sync_github_issues(actions, "o/r", "t", client=client)
    assert all(r["ok"] for r in results)
    assert results[0]["number"] == 42
    assert client.calls[0] == ("LABEL",)
    methods = [(c[0], c[1]) for c in client.calls[1:]]
    assert methods == [
        ("POST", "/issues"), ("PATCH", "/issues/4"), ("POST", "/issues/9/comments"), ("PATCH", "/issues/9"),
    ]
    assert client.calls[2][2]["state"] == "open"
    assert client.calls[4][2] == {"state": "closed", "state_reason": "completed"}


def test_sync_dry_run_makes_no_calls():
    client = _FakeClient()
    results = ri.sync_github_issues(ri.plan_issue_actions(_report(active=[KEY]), []), "o/r", "", dry_run=True,
                                    client=client)
    assert client.calls == [] and results[0]["dry_run"] is True


def test_github_client_rejects_bad_repo():
    with pytest.raises(ValueError):
        ri.GitHubClient("not a repo", "t")


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def test_cli_trend_append_trend_regressions(tmp_path, capsys):
    store = tmp_path / "store"
    for day, v in enumerate([100, 100, 130, 135], 1):
        path = tmp_path / f"r{day}.json"
        path.write_text(json.dumps(_result(day=day, latency=v)), encoding="utf-8")
        assert perf_main(["trend-append", "--store", str(store), "--result", str(path)]) == 0
    py = tmp_path / "py.json"
    py.write_text(json.dumps(_result(runtime="python", day=4, latency=500)), encoding="utf-8")
    assert perf_main(["trend", "--append", str(py), "--store", str(store),
                      "--output", str(tmp_path / "trend.md")]) == 0
    md = (tmp_path / "trend.md").read_text(encoding="utf-8")
    assert "| 0.27x | faster |" in md
    assert perf_main(["trend", "--index", str(store / "index.jsonl"), "--limit", "2"]) == 0
    assert "Window: last 2 runs" in capsys.readouterr().out

    out = tmp_path / "regressions.json"
    assert perf_main(["regressions", "--index", str(store / "index.jsonl"), "--output", str(out)]) == 0
    report = json.loads(out.read_text(encoding="utf-8"))
    assert [i["metric_key"] for i in report["active_regressions"]] == [KEY]

    plan = tmp_path / "plan.json"
    existing = tmp_path / "issues.json"
    existing.write_text("[]", encoding="utf-8")
    assert ri.main(["--report", str(out), "--repo", "o/r", "--existing", str(existing),
                    "--dry-run", "--output", str(plan)]) == 0
    assert [a["action"] for a in json.loads(plan.read_text(encoding="utf-8"))] == ["open"]


def test_cli_errors(tmp_path):
    bad = tmp_path / "bad.json"
    bad.write_text("{}", encoding="utf-8")
    assert perf_main(["trend-append", "--store", str(tmp_path / "s"), "--result", str(bad)]) == 2
    assert perf_main(["trend", "--append", str(bad)]) == 2  # --append needs --store
    broken = tmp_path / "index.jsonl"
    broken.write_text("{not json\n", encoding="utf-8")
    assert perf_main(["regressions", "--index", str(broken)]) == 2
    assert perf_main(["trend", "--index", str(broken)]) == 2
    broken.write_text("[1]\n", encoding="utf-8")  # valid JSON, not an object
    assert perf_main(["regressions", "--index", str(broken)]) == 2
    assert perf_main(["trend", "--index", str(broken)]) == 2


def test_cli_module_entry_points(tmp_path):
    path = tmp_path / "r.json"
    path.write_text(json.dumps(_result()), encoding="utf-8")
    proc = subprocess.run(
        [sys.executable, "-m", "evals.perf", "trend-append", "--store", str(tmp_path / "s"), "--result", str(path)],
        cwd=REPO_ROOT, capture_output=True, text=True, check=False,
    )
    assert proc.returncode == 0, proc.stderr
    report = tmp_path / "reg.json"
    proc = subprocess.run(
        [sys.executable, "-m", "evals.perf", "regressions", "--index", str(tmp_path / "s" / "index.jsonl"),
         "--output", str(report)],
        cwd=REPO_ROOT, capture_output=True, text=True, check=False,
    )
    assert proc.returncode == 0, proc.stderr
    proc = subprocess.run(
        [sys.executable, "-m", "evals.perf.regression_issues", "--report", str(report), "--dry-run"],
        cwd=REPO_ROOT, capture_output=True, text=True, check=False, env={"PATH": ""},
    )
    assert proc.returncode == 0, proc.stderr
    assert json.loads(proc.stdout) == []


# --------------------------------------------------------------------------
# Stdlib-only
# --------------------------------------------------------------------------


@pytest.mark.parametrize("name", ["trend.py", "regression_issues.py"])
def test_modules_import_only_stdlib(name):
    tree = ast.parse((REPO_ROOT / "evals" / "perf" / name).read_text(encoding="utf-8"))
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
            assert mod.split(".")[0] in STDLIB_OK, f"{name} imports non-stdlib {mod}"


# --------------------------------------------------------------------------
# Workflow: nightly / publish-trend / deep
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def wf() -> dict:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def _on(wf: dict) -> dict:
    return wf.get("on", wf.get(True))


def _run(job: dict) -> str:
    return "\n".join(str(s.get("run", "")) for s in job["steps"])


def _step(job: dict, name: str) -> dict:
    return next(s for s in job["steps"] if s.get("name") == name)


def test_workflow_schedule_and_tier_input(wf):
    on = _on(wf)
    assert on["schedule"] == [{"cron": "0 7 * * *"}]
    tier = on["workflow_dispatch"]["inputs"]["tier"]
    assert tier["type"] == "choice" and tier["options"] == ["smoke", "nightly", "deep"]
    assert tier["default"] == "smoke"
    assert set(wf["jobs"]) == {"smoke", "nightly", "publish-trend", "deep"}


def test_smoke_job_only_on_pr_or_smoke_dispatch(wf):
    cond = wf["jobs"]["smoke"]["if"]
    assert "github.event_name == 'pull_request'" in cond and "inputs.tier == 'smoke'" in cond
    assert "schedule" not in cond


def test_nightly_job_matrix(wf):
    job = wf["jobs"]["nightly"]
    assert "github.event_name == 'schedule'" in job["if"] and "inputs.tier == 'nightly'" in job["if"]
    assert job["strategy"]["fail-fast"] is False
    matrix = {e["os"]: e["suites"] for e in job["strategy"]["matrix"]["include"]}
    assert matrix == {
        "ubuntu-latest": "stt,dictation,recall,footprint,meetings",
        "macos-latest": "stt,dictation,recall,footprint",
        "windows-latest": "stt,dictation",
    }
    assert job["runs-on"] == "${{ matrix.os }}"
    assert "permissions" not in job
    run = _run(job)
    for build in ("-p lf-stt --example stt_bench", "-p lf --example parity", "-p lf-ask --example bench_search",
                  "-p lf-runtime --example idle", "-p lf --bin lf-rs"):
        assert build in run
    assert 'python -m evals.perf run --tier nightly --runtime "$rt" --suite "$PERF_SUITES"' in run
    assert '--runner-label "$PERF_RUNNER"' in run
    assert "GITHUB_STEP_SUMMARY" in run
    upload = next(s for s in job["steps"] if str(s.get("uses", "")).startswith("actions/upload-artifact@v4"))
    assert upload["if"] == "always()" and upload["with"]["name"] == "perf-nightly-${{ matrix.os }}"


def test_publish_trend_job(wf):
    job = wf["jobs"]["publish-trend"]
    assert job["needs"] == "nightly"
    cond = job["if"]
    assert cond.startswith("always() && (github.event_name == 'schedule' || inputs.tier == 'nightly')")
    assert "refs/heads/main" in cond
    assert job["permissions"] == {"contents": "write", "issues": "write"}
    assert job["concurrency"]["cancel-in-progress"] is False
    dl = next(s for s in job["steps"] if str(s.get("uses", "")).startswith("actions/download-artifact@v4"))
    assert dl["with"]["pattern"] == "perf-nightly-*"
    checkout = _step(job, "Check out perf-data branch")["run"]
    assert "git ls-remote --exit-code --heads origin perf-data" in checkout
    assert "git checkout --orphan perf-data" in checkout and "git rm -rfq" in checkout
    run = _run(job)
    assert "python -m evals.perf trend-append --store perf-data-store" in run
    assert "python -m evals.perf trend --index perf-data-store/index.jsonl" in run
    assert "git push origin HEAD:refs/heads/perf-data" in run
    assert "python -m evals.perf regressions --index perf-data-store/index.jsonl" in run
    assert "--consecutive 2" in run
    sync = _step(job, "Sync perf-regression issues")
    assert "python -m evals.perf.regression_issues" in sync["run"]
    assert sync["env"]["GITHUB_TOKEN"] == "${{ github.token }}"
    assert "pip install" not in run  # stdlib-only tooling


def test_write_permissions_scoped_to_publish_trend(wf):
    assert wf["permissions"] == {"contents": "read", "pull-requests": "write"}
    for name, job in wf["jobs"].items():
        perms = job.get("permissions") or {}
        if name == "publish-trend":
            continue
        assert perms.get("contents") != "write" and perms.get("issues") != "write", name


def test_deep_job(wf):
    job = wf["jobs"]["deep"]
    assert job["if"] == "github.event_name == 'workflow_dispatch' && inputs.tier == 'deep'"
    assert job["runs-on"] == "macos-latest"
    assert "permissions" not in job
    run = _run(job)
    assert "python -m evals.perf run --tier deep" in run
    assert "GITHUB_STEP_SUMMARY" in run
    assert any(str(s.get("uses", "")).startswith("actions/upload-artifact@v4") for s in job["steps"])


def test_new_jobs_do_not_interpolate_inputs_into_scripts(wf):
    for name in ("nightly", "publish-trend", "deep"):
        for step in wf["jobs"][name]["steps"]:
            assert "${{ inputs." not in str(step.get("run", "")), (name, step.get("name"))
            assert "${{ github.event" not in str(step.get("run", "")), (name, step.get("name"))
