"""Fast checks of the `agents` suite of the full-feature sim (evals/localflow_sim):
the frozen spec, `driver.py --list`, real A1/A2 cells graded by `gates.py`
(including `manifest.json` "suites" scoping), `jev_inputs.py` JA1-JA4 extraction
and a GREEN `report.py` verdict. No network, no Jev call, no vendor CLI."""

import copy
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SIM = ROOT / "evals" / "localflow_sim"
AGENT_CELLS = {
    "agents-lifecycle-state": "A1", "agents-hotcommands-seam": "A2", "agents-concurrency-bounds": "A3",
    "agents-api-contract-auth": "A4", "agents-secrets-redaction": "A5", "agents-providers-boundary": "A6",
    "agents-idle-efficiency": "A7", "agents-recovery-crossproc": "A8",
}
AGENT_GATES = [f"A{i}" for i in range(1, 9)]
JA_TYPES = ("agent_prompt_fidelity", "agent_launch_intent", "agent_error_helpfulness",
            "agent_result_faithfulness")


@pytest.fixture(scope="module")
def sim():
    """The harness modules (gates, jev_inputs, report, driver), imported from evals/."""
    sys.path.insert(0, str(SIM))
    try:
        import driver
        import gates
        import jev_inputs
        import report

        yield type("Sim", (), {"gates": gates, "jev_inputs": jev_inputs, "report": report, "driver": driver})
    finally:
        sys.path.remove(str(SIM))


def _env() -> dict:
    env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
    env.update(SIM_SUT=str(ROOT), PYNPUT_BACKEND=env.get("PYNPUT_BACKEND", "dummy"))
    return env


def _head() -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip()


def _write_manifest(run: Path, commit: str, suites=("agents",), repeats: int = 1) -> None:
    data = {"run_id": run.name, "commit": commit, "repeats": repeats}
    if suites is not None:
        data["suites"] = list(suites)
    (run / "manifest.json").write_text(json.dumps(data), encoding="utf-8")


@pytest.fixture(scope="module")
def agents_run(tmp_path_factory):
    """One repeat of the two fastest-to-check real cells (A1 lifecycle, A2 hot commands)."""
    if sys.platform == "win32":
        pytest.skip("evals/localflow_sim driver requires POSIX isolation")
    pytest.importorskip("fastapi")
    pytest.importorskip("sounddevice")
    if shutil.which("git") is None:
        pytest.skip("git is required for the cell's temp repo")
    run = tmp_path_factory.mktemp("agents-run")
    results = run / "r1" / "results"
    for cell in ("agents-lifecycle-state", "agents-hotcommands-seam"):
        proc = subprocess.run(
            [sys.executable, str(SIM / "driver.py"), "--suite", "agents", "--scenario", cell,
             "--timeout", "240", "--out-dir", str(results)],
            env=_env(), capture_output=True, text=True, timeout=300, check=False)
        line = json.loads(proc.stdout.strip().splitlines()[-1])
        assert line["status"] == "ok", (line, (results / "_logs" / f"{cell}.log").read_text()[-3000:])
    return run


@pytest.fixture
def pinned_to_head(sim, monkeypatch):
    """Grade against this checkout's HEAD (the cells record it as their commit)."""
    head = _head()
    monkeypatch.setitem(sim.gates.SPEC, "sut", {"commit": head})
    return head


# -- spec & driver ---------------------------------------------------------------------


def test_scenarios_declare_the_agents_suite_with_all_eight_gates():
    spec = json.loads((SIM / "scenarios.json").read_text(encoding="utf-8"))
    assert spec["sut"]["commit"] == "acde5821b6d220d4d024332755dbd4f0889ddd2d"
    cells = spec["suites"]["agents"]["cells"]
    assert {c["id"]: c["gate"] for c in cells} == AGENT_CELLS
    assert all(c["kind"].startswith("agents_") for c in cells)
    hot = next(c for c in cells if c["gate"] == "A2")["hot"]
    assert len(hot["positives"]) >= 20 and len(hot["negatives"]) >= 20
    cases = {n["case"] for n in hot["negatives"]}
    assert {"mid_sentence", "partial_word", "too_few_words", "disabled_rule", "flag_off", "alias_removed",
            "cooldown", "command_job", "transform_job", "history_retry"} <= cases
    assert len({p["id"] for p in hot["positives"]} | {n["id"] for n in hot["negatives"]}) == \
        len(hot["positives"]) + len(hot["negatives"])


def test_driver_lists_the_eight_agents_cells():
    proc = subprocess.run([sys.executable, str(SIM / "driver.py"), "--suite", "agents", "--list"],
                          env=_env(), capture_output=True, text=True, timeout=60, check=True)
    assert proc.stdout.split() == list(AGENT_CELLS)


def test_driver_scrubs_planted_secrets_to_a_countable_marker(sim):
    planted = "sim-secret-devin-0123456789abcdef"
    env = {"LOCALFLOW_AGENT_DEVIN_API_KEY": planted, "FIREWORKS_API_KEY": "fw-key-0123456789"}
    text = sim.driver.scrub(f"leak {planted} and fw-key-0123456789", env)
    assert text == f"leak {sim.driver.PLANTED_MARK} and [REDACTED]"
    assert sim.driver.PLANTED_MARK == sim.gates.PLANTED_MARK


@pytest.mark.skipif(sys.platform == "win32", reason="run_matrix.sh requires POSIX bash")
def test_run_matrix_only_needs_the_llm_key_for_llm_suites(tmp_path):
    env = {k: v for k, v in _env().items() if k != "FIREWORKS_API_KEY"}
    env.update(SIM_SUT=str(tmp_path / "missing"), SUITES="dictation agents", SIM_RUNS=str(tmp_path))
    proc = subprocess.run(["bash", str(SIM / "run_matrix.sh")], env=env, capture_output=True, text=True,
                          timeout=60, check=False)
    assert proc.returncode == 2 and "FIREWORKS_API_KEY" in proc.stderr
    env["SUITES"] = "agents"
    proc = subprocess.run(["bash", str(SIM / "run_matrix.sh")], env=env, capture_output=True, text=True,
                          timeout=60, check=False)
    assert proc.returncode == 2 and "FIREWORKS_API_KEY" not in proc.stderr  # fails later, on the missing SUT


def test_suites_all_expands_to_suite_order_in_gates_and_run_matrix(sim, tmp_path):
    order = list(sim.gates.SUITE_ORDER)
    assert sim.gates.active_suites({"suites": ["all"]}) == order
    assert sim.gates.active_suites({"suites": ["agents", "all"]}) == order
    assert sim.gates.active_suites({"suites": ["agents"]}) == ["agents"]
    with pytest.raises(ValueError, match="setting_api"):
        sim.gates.active_suites({"suites": ["mcp", "setting_api"]})
    script = (SIM / "run_matrix.sh").read_text(encoding="utf-8")
    assert f'ALL_SUITES="{" ".join(order)}"' in script  # kept in sync with gates.SUITE_ORDER
    if sys.platform == "win32":
        return
    env = {k: v for k, v in _env().items() if k != "FIREWORKS_API_KEY"}
    env.update(SIM_SUT=str(tmp_path / "missing"), SIM_RUNS=str(tmp_path), SUITES="all")
    proc = subprocess.run(["bash", str(SIM / "run_matrix.sh")], env=env, capture_output=True, text=True,
                          timeout=60, check=False)
    assert proc.returncode == 2 and "FIREWORKS_API_KEY" in proc.stderr  # "all" includes the LLM suites
    # REPEATS=0 writes the manifest without running a cell: "all" is recorded expanded.
    env.update(SIM_SUT=str(ROOT), SIM_ALLOW_COMMIT_OVERRIDE="1", SIM_PYTHON=sys.executable, REPEATS="0",
               FIREWORKS_API_KEY="fw-not-used", RUN_ID="all-suites", SUITES="all agents")
    proc = subprocess.run(["bash", str(SIM / "run_matrix.sh")], env=env, capture_output=True, text=True,
                          timeout=60, check=False)
    assert proc.returncode == 0, proc.stderr
    manifest = json.loads((tmp_path / "all-suites" / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["suites"] == order


# -- gates -------------------------------------------------------------------------------


def test_gates_grade_real_agents_cells_scoped_to_manifest_suites(sim, agents_run, pinned_to_head, tmp_path):
    run = tmp_path / "scoped"
    shutil.copytree(agents_run, run)
    _write_manifest(run, pinned_to_head)
    out = sim.gates.compute(run)
    assert out["active_suites"] == ["agents"] and out["expected_cells_per_repeat"] == 8
    assert list(out["summary"]) == AGENT_GATES and list(out["per_repeat"]["r1"]) == AGENT_GATES
    per = out["per_repeat"]["r1"]
    real = {item: ok for item, ok in per["A1"]["items"].items() if not item.endswith(":not-exercised")}
    assert len(real) > 20 and all(real.values()), [item for item, ok in real.items() if not ok]
    assert per["A2"]["pass"] is True and per["A2"]["n"] >= 150, per["A2"]["fail"][:5]
    missing = {n["cell"] for n in out["not_exercised"]}
    assert missing == set(AGENT_CELLS) - {"agents-lifecycle-state", "agents-hotcommands-seam"}
    for gate in AGENT_GATES[2:]:
        assert per[gate]["pass"] is False
        assert all(":not-exercised" in item for item in per[gate]["fail"])
    assert any(item.endswith(":not-exercised") for item in per["A1"]["items"])  # missing cells feed A1 too
    assert out["live"] and all(x["status"] == "not exercised" for x in out["live"])


def test_gates_without_suites_in_the_manifest_keep_the_full_matrix(sim, agents_run, pinned_to_head, tmp_path):
    run = tmp_path / "full"
    shutil.copytree(agents_run, run)
    _write_manifest(run, pinned_to_head, suites=None)
    out = sim.gates.compute(run)
    assert out["active_suites"] == list(sim.gates.SUITE_ORDER)
    assert {"D1", "M1", "R1", "P1", "S1", "A1"} <= set(out["summary"])
    assert out["expected_cells_per_repeat"] == len(sim.gates.cell_index())
    assert any(n["suite"] == "dictation" for n in out["not_exercised"])


def test_gates_catch_a_broken_state_sequence_and_a_planted_leak(sim, agents_run, pinned_to_head, tmp_path):
    run = tmp_path / "tampered"
    shutil.copytree(agents_run, run)
    _write_manifest(run, pinned_to_head)
    path = run / "r1" / "results" / "agents-lifecycle-state.json"
    result = json.loads(path.read_text(encoding="utf-8"))
    broken = copy.deepcopy(result)
    broken["runs"]["ok"]["states"] = ["queued", "running", "completed"]
    broken["jev_result_items"][0]["result_text"] = f"key {sim.gates.PLANTED_MARK}"
    path.write_text(json.dumps(broken), encoding="utf-8")
    per = sim.gates.compute(run)["per_repeat"]["r1"]
    assert per["A1"]["items"]["agents-lifecycle-state:ok:states"] is False
    assert per["A5"]["items"]["agents-lifecycle-state:result-json-clean"] is False
    assert per["A2"]["pass"] is True


# -- jev_inputs ------------------------------------------------------------------------


def test_jev_inputs_extract_ja1_to_ja4(sim, agents_run, pinned_to_head):
    _write_manifest(agents_run, pinned_to_head)
    items = sim.jev_inputs.collect(agents_run)
    by_type = {t: [i for i in items if i["type"] == t] for t in JA_TYPES}
    spec = json.loads((SIM / "scenarios.json").read_text(encoding="utf-8"))["suites"]["agents"]["cells"]
    sample = next(c for c in spec if c["gate"] == "A2")["jev_sample"]
    assert len(by_type["agent_prompt_fidelity"]) == len(sample["prompt"])
    assert len(by_type["agent_launch_intent"]) == len(sample["intent"])
    assert len(by_type["agent_error_helpfulness"]) == 4 and len(by_type["agent_result_faithfulness"]) == 1
    assert len(items) <= 30
    ja1 = by_type["agent_prompt_fidelity"][0]
    assert ja1["gate"] == "A2" and ja1["transcript"] and ja1["prompt"] == ja1["expected"]
    assert "launch claude" in ja1["rules"]
    intent = {i["item"].rsplit(":", 1)[1]: i for i in by_type["agent_launch_intent"]}
    assert intent["p03"]["launched"] is True and intent["n01"]["launched"] is False
    assert {i["kind"] for i in intent.values()} == {"expect_launch", "expect_no_launch"}
    errors = by_type["agent_error_helpfulness"]
    assert all(e["gate"] == "A1" and e["cause"] and e["message"] for e in errors)
    result = by_type["agent_result_faithfulness"][0]
    assert result["result_text"].startswith("done: ") and "#1 state -> queued" in result["event_log"]
    text = json.dumps(items)
    assert "sim-secret-" not in text and sim.gates.PLANTED_MARK not in text
    # Every Jev item lines up with deterministic gate items (report.gate_ok).
    gate_data = sim.gates.compute(agents_run)
    for item in items:
        row = {**item, "repeat": "r1"}
        assert sim.report.gate_ok(gate_data, "r1", row) is True, item["item"]


# -- report ------------------------------------------------------------------------------


def _synthetic_run(sim, tmp_path: Path, *, intent_p: float = 0.9) -> Path:
    """gates.json with every agents gate passing plus an advisory Jev pass (JA1-JA4)."""
    run = tmp_path / "synthetic"
    run.mkdir()
    pinned = sim.gates.SPEC["sut"]["commit"]
    _write_manifest(run, pinned)
    items = {gate: {f"{cell}:check": True} for cell, gate in AGENT_CELLS.items()}
    per = {gate: {"value": 1.0, "k": 1, "n": 1, "pass": True, "fail": [], "items": items[gate]}
           for gate in AGENT_GATES}
    summary = {gate: {"name": sim.gates.GATES[gate], "threshold": "all", "status": "pass", "pass_count": "1/1",
                      "min": 1.0, "max": 1.0, "spread": 0.0, "pooled_k": 1, "pooled_n": 1,
                      "wilson95_pooled": [0.207, 1.0], "low_power": True} for gate in AGENT_GATES}
    gate_data = {"run": str(run), "pinned_commit": pinned, "pinned_source": "scenarios.json",
                 "commits_seen": [pinned], "commit_ok": True, "repeats": ["r1"], "active_suites": ["agents"],
                 "expected_cells_per_repeat": 8, "live": [], "not_exercised": [], "notes": [],
                 "extras": {}, "per_repeat": {"r1": per}, "summary": summary}
    (run / "gates.json").write_text(json.dumps(gate_data), encoding="utf-8")
    base = {"repeat": "r1", "suite": "agents"}
    rows = [
        {**base, "type": "agent_prompt_fidelity", "cell": "agents-hotcommands-seam", "gate": "A2",
         "item": "agents-hotcommands-seam:check", "kind": "positive",
         "advisory": {"preservesTask": {"p": 0.96}, "addedOrDropped": {"p": 0.03}}},
        {**base, "type": "agent_launch_intent", "cell": "agents-hotcommands-seam", "gate": "A2",
         "item": "agents-hotcommands-seam:check", "kind": "expect_launch", "launched": True, "expected": True,
         "advisory": {"shouldLaunch": {"p": intent_p}}},
        {**base, "type": "agent_error_helpfulness", "cell": "agents-lifecycle-state", "gate": "A1",
         "item": "agents-lifecycle-state:check", "kind": "error",
         "advisory": {"statesCauseAndNextStep": {"p": 0.88}, "leaksCredential": {"p": 0.01}}},
        {**base, "type": "agent_result_faithfulness", "cell": "agents-providers-boundary", "gate": "A6",
         "item": "agents-providers-boundary:check", "kind": "result",
         "advisory": {"supportedByLog": {"p": 0.91}, "inventedByTruncation": {"p": 0.02}}},
    ]
    (run / "jev_out.json").write_text(json.dumps({"model": "synthetic", "usage": {}, "calls": len(rows),
                                                  "errors": 0, "rows": rows}), encoding="utf-8")
    return run


def _report(sim, run: Path, out: Path, monkeypatch) -> dict:
    monkeypatch.setattr(sys, "argv", ["report.py", str(run), str(out)])
    assert sim.report.main() == 0
    return json.loads((out / "scorecard.json").read_text(encoding="utf-8"))


def test_report_is_green_on_passing_agents_gates_with_advisory_jev(sim, tmp_path, monkeypatch):
    run = _synthetic_run(sim, tmp_path)
    card = _report(sim, run, tmp_path / "findings", monkeypatch)
    assert card["verdict"] == "GREEN", card["reasons"]
    assert card["active_suites"] == ["agents"] and list(card["gates"]) == AGENT_GATES
    assert card["needs_adjudication"] == {"trusted_gate_vs_jev": 0, "advisory_gate_vs_jev": 0,
                                          "failing_gate_items": 0}
    text = (tmp_path / "findings" / "report.md").read_text(encoding="utf-8")
    assert text.startswith("# VERDICT: GREEN") and "Suites gated: agents." in text
    assert "| D1 |" not in text and "| A8 |" in text
    jev = (tmp_path / "findings" / "jev.md").read_text(encoding="utf-8")
    for name in ("preservesTask", "shouldLaunch", "statesCauseAndNextStep", "inventedByTruncation"):
        assert name in jev


def test_an_advisory_jev_disagreement_queues_but_never_turns_red(sim, tmp_path, monkeypatch):
    run = _synthetic_run(sim, tmp_path, intent_p=0.1)  # Jev: "should not launch", product launched
    card = _report(sim, run, tmp_path / "findings", monkeypatch)
    assert card["verdict"] == "GREEN"
    queued = [q for q in card["disagreement_queue"] if q["source"] == "gate-vs-jev"]
    assert [(q["type"], q["trust"]) for q in queued] == [("agent_launch_intent", "advisory")]


# -- focused gate & report checks ---------------------------------------------------------


def _tally():
    seen: dict[str, bool] = {}

    def tally(gate, ok, item, detail=""):
        seen[item] = bool(ok)
    return seen, tally


def _xproc(alternations: int, barrier: bool = True) -> dict:
    return {"writers": [{"exit": 0, "unreadable": 0}] * 2, "barrier": barrier, "entries": 200, "unique": 200,
            "expected": 200, "missing": [], "per_writer_in_order": True, "alternations": alternations}


def test_xproc_interleaving_needs_a_barrier_and_two_alternations(sim):
    cell = {"id": "c", "xproc": {"writes_per_process": 100}}
    for alternations, barrier, want in ((1, True, False), (2, True, True), (50, False, False)):
        seen, tally = _tally()
        sim.gates._agents_xproc(_xproc(alternations, barrier), cell, tally)
        assert seen["c:xproc:interleaved"] is want, (alternations, barrier)
        assert seen["c:xproc:none-lost"] and seen["c:xproc:never-unreadable"]


def _filters(devin_ids) -> dict:
    ids = ["s1", "s2", "s3"]
    row = lambda got: {"status": 200, "ids": got, "states": [], "origins": []}
    f = {"state_failed": {**row(["s1"]), "states": ["failed"]}, "state_terminal": row(ids), "state_active": row([]),
         "provider_mock": row(ids), "origin_cli": row(["s3"]), "q": row(["s2"]), "root_id": row(["s1"])}
    if devin_ids is not None:
        f["provider_devin"] = row(devin_ids)
    f["_expect"] = {"failed": ["s1"], "terminal": ids, "q": ["s2"], "cli": ["s3"], "root": ["s1"], "devin": []}
    return f


def test_provider_filter_must_also_exclude(sim):
    for devin_ids, want in (([], True), (["s1", "s2", "s3"], False), (None, False)):
        seen, tally = _tally()
        sim.gates._agents_api_filters(_filters(devin_ids), "c", tally)
        assert seen["c:filter:provider-exclusion"] is want, devin_ids
        assert seen["c:filter:provider"]


def test_jev_pass_is_complete_for_deterministic_only_runs(sim):
    done = {"calls": 0, "errors": 0, "rows": []}
    assert sim.report.jev_complete(done, []) is True  # mcp / settings_api: nothing to grade
    assert sim.report.jev_complete({**done, "errors": 1}, []) is False
    assert sim.report.jev_complete(done, [{"id": "x"}]) is False  # items to grade, none graded
    assert sim.report.jev_complete(done, None) is False  # jev_inputs.json missing
    assert sim.report.jev_complete(None, []) is False  # jev never ran
    graded = {"calls": 1, "errors": 0, "rows": [{"verdict": "pass"}]}
    assert sim.report.jev_complete(graded, [{"id": "x"}]) is True
