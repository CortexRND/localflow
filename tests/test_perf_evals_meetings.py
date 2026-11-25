"""Tests for the perf-eval ``meetings`` suite, the meeting-gates adapter and
the configurable ``evals/meeting_sim`` driver/gates.

No Ollama needed: synthetic driver results (built from ``scenarios.json``)
are scored by the real ``gates.py``; the live path uses a stub driver or a
stub Ollama HTTP server.
"""

from __future__ import annotations

import ast
import copy
import importlib.util
import json
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from evals.perf import (
    compare_results,
    load_policy,
    make_metric,
    make_result,
    make_run,
    registry,
    validate_result,
)
from evals.perf.adapters import meeting_gates as mg
from evals.perf.registry import RunContext
from evals.perf.suites import BUILTIN_SUITES, meetings

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
SIM_DIR = REPO_ROOT / "evals" / "meeting_sim"
GATES_PY = SIM_DIR / "gates.py"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
SCENARIOS = json.loads((SIM_DIR / "scenarios.json").read_text(encoding="utf-8"))["scenarios"]
EXACT = ("M1", "M2", "M3", "M4", "M11", "M12", "M13", "M14")
QUALITY = ("M5", "M6", "M7", "M8", "M9", "M10")
LOG_PATH = "/sim/vault/log.md"


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    for var in ("LF_PERF_MEETING_RUN_DIR", "LF_PERF_MEETING_OUT", "LF_PERF_OLLAMA_URL",
                "LF_PERF_OLLAMA_MODEL", "LF_PERF_PYTHON", "SIM_SUT", "SIM_LLM_URL", "SIM_LLM_MODEL"):
        monkeypatch.delenv(var, raising=False)


# --------------------------------------------------------------------------
# Synthetic driver results (what a perfect run of driver.py would write)
# --------------------------------------------------------------------------


def _safe_title(title: str) -> str:
    from localflow.meetings import _safe_title as safe

    return safe(title)


def _template_sections(name: str) -> list[str]:
    from localflow.meeting_templates import resolve_template

    return resolve_template(name).section_names()


DIARIZED_ID = "launch-sync-diarized"


def _is_diarized(sc: dict) -> bool:
    return bool(sc.get("diarization_backend")) and sc["system_audio"] == "ok"


def speaker_labels(sc: dict) -> dict[int, str]:
    """Line index -> "Speaker k" (first appearance among remote speakers)."""
    rank: dict[str, int] = {}
    return {i: f"Speaker {rank.setdefault(who, len(rank) + 1)}"
            for i, ((src, _), who) in enumerate(zip(sc["lines"], sc["line_speakers"])) if src == "others"}


def perfect_result(sc: dict, *, rep: int = 1, stop_ms: float = 1200.0) -> dict:
    nf = sc["notes_final"] + sc.get("notes_final_extra", "")
    lines = []
    for line in nf.strip().splitlines():
        if line.startswith("#"):
            lines.append(line)
    if not nf.strip():
        lines += [f"## {h}" for h in _template_sections(sc["template"])]
    words = [p["keywords"][0] for p in sc["user_points"]] + [f["any"][0] for f in sc["facts"]]
    md = "\n".join(lines) + "\n\n" + "\n".join(f"- {w}" for w in words) + "\n"
    notes_text = f"---\ntemplate: {sc['template']}\n---\n# {_safe_title(sc['title'])}\n{md}\nRaw transcript: [[log]]\n"
    ok = sc["system_audio"] == "ok"
    labels = speaker_labels(sc) if _is_diarized(sc) else {}
    log_lines = [
        f"**00:{i * 5:02d}** **{'Me' if src == 'me' else labels.get(i, 'Others')}:** {text}"
        for i, (src, text) in enumerate(sc["lines"]) if ok or src == "me"
    ]
    speakers = [{"id": f"S{label.split()[1]}", "label": label, "name": None, "source": "others", "seconds": 10.0}
                for label in dict.fromkeys(labels.values())]
    log_text = "# Log\n\n" + "\n".join(log_lines) + "\n"
    if nf.strip():
        log_text += f"\n## My raw notes\n\n{nf.strip()}\n"
    chats = []
    for c in sc["chat"]:
        answer = f"It was {c['any'][0]}." if c["kind"] == "answerable" else "The transcript doesn't mention that."
        chats.append({**c, "status": 200, "answer": answer})
    start = {"system_audio": "ok"} if ok else {"system_audio": "error", "system_audio_error": "sim: no loopback"}
    chat_ms = [300.0 + 10 * i + rep for i in range(len(chats))]
    llm_ms = [stop_ms - 50.0, *[x - 20 for x in chat_ms]]
    http = [{"call": "GET /meeting/templates", "status": 200, "secs": 0.0, "ms": 1.0},
            {"call": "POST /meeting/start", "status": 200, "secs": 0.0, "ms": 2.0},
            {"call": "POST /meeting/stop", "status": 200, "secs": round(stop_ms / 1000, 2), "ms": stop_ms}]
    http += [{"call": "POST /meeting/chat", "status": 200, "secs": round(m / 1000, 2), "ms": m} for m in chat_ms]
    http.append({"call": "POST /meeting/chat", "status": 200, "secs": 0.2, "ms": 200.0})
    return {
        "scenario": sc["id"], "commit": "abc", "notes_final": nf, "tmp": "/tmp/x",
        "templates": [200, {}], "start": [200, start],
        "status_mid": [200, {"system_audio": start["system_audio"]}],
        "notes_save": [200, {}],
        "stop": [200, {"template": sc["template"], "enhanced": bool(nf.strip()), "notes_path": "/sim/n.md",
                       "log_path": LOG_PATH, "diarization": "ok" if labels else "off", "speakers": speakers}],
        "notes_text": notes_text, "log_text": log_text, "summarize_calls": 1, "chats": chats,
        "restart": {"status": 200, "log_path": LOG_PATH, "expected": LOG_PATH},
        "http": http,
        "llm_calls": [{"prompt": md, "output": md, "secs": round(m / 1000, 2), "ms": m} for m in llm_ms],
        "timings": {"stop_to_notes_ms": stop_ms, "summarize_ms": stop_ms, "chat_ms": chat_ms,
                    "restart_chat_ms": 200.0, "llm_call_ms": llm_ms, "llm_calls_count": len(llm_ms)},
    }


def write_run(run_dir: Path, repeats: int = 2, mutate=None) -> Path:
    for k in range(1, repeats + 1):
        out = run_dir / f"r{k}" / "results"
        out.mkdir(parents=True, exist_ok=True)
        for sc in SCENARIOS:
            res = perfect_result(sc, rep=k, stop_ms=1000.0 + 100 * k)
            if mutate:
                mutate(k, res)
            (out / f"{sc['id']}.json").write_text(json.dumps(res), encoding="utf-8")
    return run_dir


def run_gates_py(*args: str, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(GATES_PY), *args], capture_output=True, text=True,
                          cwd=str(SIM_DIR), env=env, check=False, timeout=120)


def _valid(cases: list[dict], runtime: str = "python") -> dict:
    run = make_run(tier="smoke", runtime=runtime, sha="abc", ref="t", os_name="linux", arch="x86_64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    result = make_result(run, cases)
    assert validate_result(result) == []
    assert all(c["suite"] == "meetings" for c in cases)
    return result


def _ctx(runtime: str = "python", tier: str = "smoke", **options) -> RunContext:
    return RunContext(tier=tier, runtime=runtime, repo_root=REPO_ROOT, options=options)


def _by_case(cases: list[dict]) -> dict[str, dict]:
    return {c["case"]: c for c in cases}


@pytest.fixture(scope="module")
def scored_run(tmp_path_factory) -> Path:
    run = write_run(tmp_path_factory.mktemp("meet") / "run")
    proc = run_gates_py(str(run), "--sut", str(REPO_ROOT))
    assert proc.returncode == 0, proc.stderr
    return run


# --------------------------------------------------------------------------
# Registration, policy and import hygiene
# --------------------------------------------------------------------------


def test_meetings_suite_is_registered():
    assert "meetings" in BUILTIN_SUITES
    assert registry.get_suite("meetings") is meetings.run_meetings


@pytest.mark.parametrize("path", [PERF_DIR / "adapters" / "meeting_gates.py", PERF_DIR / "suites" / "meetings.py"])
def test_modules_import_only_stdlib(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            assert mod.split(".")[0] in STDLIB_OK, f"{path} imports non-stdlib {mod}"


def test_policy_has_meetings_rules():
    rules = load_policy()["metrics"]
    for gate in EXACT:
        assert rules[gate]["class"] == "exact" and rules[gate]["on_pr"] == "block"
    for gate in QUALITY:
        assert rules[gate]["class"] == "quality" and rules[gate]["on_pr"] == "block"
    assert rules["M7"]["better"] == "lower"
    for name in ("stop_to_notes_ms", "chat_ms", "llm_call_ms", "llm_calls_per_meeting", "summarize_ms"):
        assert rules[name]["class"] == "noisy" and rules[name]["better"] == "lower"
    caps = {c["metric"]: c for c in load_policy()["caps"] if c["suite"] == "meetings"}
    assert set(caps) == set(QUALITY)
    # Floors mirror gates.py THRESH.
    assert caps["M5"]["min_value"] == 0.9 and caps["M7"]["max_value"] == 0.05


def test_adapter_gate_info_matches_policy_and_gates_py():
    rules = load_policy()["metrics"]
    for gate, (_name, cls, _thr) in mg.GATE_INFO.items():
        assert rules[gate]["class"] == cls
    tree = ast.parse(GATES_PY.read_text(encoding="utf-8"))
    thresh = next(ast.literal_eval(n.value) for n in tree.body
                  if isinstance(n, ast.Assign) and getattr(n.targets[0], "id", "") == "THRESH")
    for gate, value in thresh.items():
        assert mg.GATE_INFO[gate][2] == value


# --------------------------------------------------------------------------
# gates.py (configurable paths, --json, timings)
# --------------------------------------------------------------------------


def test_gates_py_positional_run_dir_writes_gates_json(scored_run):
    data = json.loads((scored_run / "gates.json").read_text())
    assert set(data) == {"per_repeat", "summary", "timings"}
    assert set(data["per_repeat"]) == {"r1", "r2"}
    for gate in mg.GATES:
        assert data["summary"][gate]["status"] == "pass", (gate, data["summary"][gate])
    t = data["timings"]
    assert t["meetings"] == 2 * len(SCENARIOS) and t["meetings_without_timings"] == 0
    assert sorted(t["stop_to_notes_ms"]) == sorted([1100.0] * len(SCENARIOS) + [1200.0] * len(SCENARIOS))
    assert len(t["chat_ms"]) == 2 * sum(len(s["chat"]) for s in SCENARIOS)


def test_gates_py_sut_env_and_json_flag(tmp_path, scored_run):
    out = tmp_path / "elsewhere" / "g.json"
    env = {**__import__("os").environ, "SIM_SUT": str(REPO_ROOT)}
    proc = run_gates_py(str(scored_run), "--json", str(out), env=env)
    assert proc.returncode == 0, proc.stderr
    assert json.loads(out.read_text())["summary"] == json.loads((scored_run / "gates.json").read_text())["summary"]
    assert "M1 pass 2/2" in proc.stdout
    proc = run_gates_py(str(scored_run), "--sut", str(REPO_ROOT), "--output", str(out))
    assert proc.returncode == 0, proc.stderr


def test_gates_py_scenarios_subset_and_missing_scenario(tmp_path):
    run = tmp_path / "run"
    out = run / "r1" / "results"
    out.mkdir(parents=True)
    sc = SCENARIOS[0]
    (out / f"{sc['id']}.json").write_text(json.dumps(perfect_result(sc)))
    # Default scenarios file: the other missing scenarios fail M1.
    assert run_gates_py(str(run), "--sut", str(REPO_ROOT)).returncode == 0
    m1 = json.loads((run / "gates.json").read_text())["summary"]["M1"]
    assert m1["status"] == "fail" and m1["pooled_n"] == len(SCENARIOS)
    subset = tmp_path / "subset.json"
    subset.write_text(json.dumps({"scenarios": [sc]}))
    assert run_gates_py(str(run), "--sut", str(REPO_ROOT), "--scenarios", str(subset)).returncode == 0
    summary = json.loads((run / "gates.json").read_text())["summary"]
    assert summary["M1"]["status"] == "pass"
    assert summary["M12"]["status"] == "not exercised"  # mic fallback scenario not in the subset


def test_gates_py_handles_results_without_timings(tmp_path):
    def drop(_k, res):
        del res["timings"]

    run = write_run(tmp_path / "run", repeats=1, mutate=drop)
    assert run_gates_py(str(run), "--sut", str(REPO_ROOT)).returncode == 0
    t = json.loads((run / "gates.json").read_text())["timings"]
    assert t["meetings"] == 0 and t["meetings_without_timings"] == len(SCENARIOS)


# --------------------------------------------------------------------------
# Adapter
# --------------------------------------------------------------------------


def test_adapter_converts_real_gates_output(scored_run):
    cases = mg.load_run_dir(scored_run)
    _valid(cases)
    by = _by_case(cases)
    assert set(by) == {f"meetings/gate/{g}" for g in mg.GATES} | {"meetings/timings/python"}
    for gate in mg.GATES:
        case = by[f"meetings/gate/{gate}"]
        assert case["status"] == "ok", case
        assert case["dims"]["kind"] == "gate" and case["dims"]["gate"] == gate
        metric = case["metrics"][gate]
        assert metric["unit"] == "ratio"
        assert metric["better"] == ("lower" if gate == "M7" else "higher")
        assert metric["value"] == (0.0 if gate == "M7" else 1.0)
        ev = case["evidence"]
        assert ev["verdict"] == "pass" and ev["n"] > 0 and set(ev["per_repeat"]) == {"r1", "r2"}
        lo, hi = ev["wilson95"]
        assert 0.0 <= lo <= metric["value"] <= hi <= 1.0
    timing = by["meetings/timings/python"]
    assert timing["status"] == "ok" and timing["evidence"]["source"] == "driver_results"
    m = timing["metrics"]
    assert set(m) == set(mg.TIMING_METRICS)
    assert m["stop_to_notes_ms"]["unit"] == "ms" and m["stop_to_notes_ms"]["median"] == pytest.approx(1150.0)
    assert m["llm_calls_per_meeting"]["unit"] == "count"
    assert all(metric["better"] == "lower" for metric in m.values())
    assert len(m["llm_call_ms"]["samples"]) == sum(m["llm_calls_per_meeting"]["samples"])


def _gates_doc(**summary_overrides) -> dict:
    summary = {g: {"status": "pass", "pass_count": "3/3", "pooled_k": 30, "pooled_n": 30} for g in mg.GATES}
    for gate, entry in summary_overrides.items():
        summary[gate] = entry
    per = {f"r{i}": {g: {"value": 1.0, "k": 10, "n": 10, "pass": True, "fail": []} for g in mg.GATES} for i in (1, 2, 3)}
    per["r2"]["M6"] = {"value": 0.6, "k": 6, "n": 10, "pass": False, "fail": ["pricing-review:churn-reason"]}
    return {"per_repeat": per, "summary": summary}


def test_adapter_pooled_rate_wilson_and_failures():
    doc = _gates_doc(M6={"status": "flaky", "pass_count": "2/3", "pooled_k": 26, "pooled_n": 30},
                     M7={"status": "fail", "pass_count": "0/3", "pooled_k": 27, "pooled_n": 30},
                     M8={"status": "not exercised", "pass_count": "0/3", "pooled_k": 0, "pooled_n": 0})
    by = _by_case(mg.convert_gates(doc))
    m6 = by["meetings/gate/M6"]
    assert m6["metrics"]["M6"]["value"] == pytest.approx(26 / 30, abs=1e-6)
    assert m6["evidence"]["wilson95"] == list(mg.wilson(26, 30))
    assert m6["evidence"]["verdict"] == "flaky"
    assert m6["evidence"]["failures"] == {"r2": ["pricing-review:churn-reason"]}
    assert m6["evidence"]["per_repeat"]["r2"] == 0.6
    m7 = by["meetings/gate/M7"]  # unsupported-number rate, lower is better
    assert m7["metrics"]["M7"]["value"] == pytest.approx(0.1)
    assert m7["evidence"]["unsupported"] == 3 and m7["evidence"]["wilson95"] == list(mg.wilson(3, 30))
    m8 = by["meetings/gate/M8"]
    assert m8["status"] == "not_exercised" and "n=0" in m8["error"] and m8["metrics"] == {}
    _valid(list(by.values()))


def test_wilson_matches_gates_py():
    spec = importlib.util.spec_from_file_location("sim_gates", GATES_PY)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)  # module level only defines functions
    for k, n in ((0, 5), (5, 5), (26, 30), (117, 117)):
        assert mg.wilson(k, n) == mod.wilson(k, n)
    assert mg.wilson(0, 0) == (None, None)


@pytest.mark.parametrize("bad", [None, [], {"summary": "x"}, {"per_repeat": {}}])
def test_adapter_malformed_gates_doc_fails_every_gate(bad):
    cases = mg.convert_gates(bad)
    assert len(cases) == len(mg.GATES) == 14 and all(c["status"] == "failed" and c["error"] for c in cases)


def test_adapter_bad_entries_fail_that_gate_only():
    doc = _gates_doc(M2={"pooled_k": 5, "pooled_n": 3}, M3="nope")
    doc["summary"].pop("M13")
    by = _by_case(mg.convert_gates(doc))
    assert by["meetings/gate/M2"]["status"] == "failed"
    assert by["meetings/gate/M3"]["status"] == "failed"
    assert "missing" in by["meetings/gate/M13"]["error"]
    assert by["meetings/gate/M1"]["status"] == "ok"


def test_load_run_dir_with_malformed_files(tmp_path):
    run = tmp_path / "run"
    (run / "r1" / "results").mkdir(parents=True)
    (run / "gates.json").write_text("{not json")
    (run / "r1" / "results" / "x.json").write_text("[1, 2")
    cases = mg.load_run_dir(run)
    _valid(cases)
    by = _by_case(cases)
    assert all(by[f"meetings/gate/{g}"]["status"] == "failed" for g in mg.GATES)
    assert "gates.json" in by["meetings/gate/M1"]["error"]
    timing = by["meetings/timings/python"]
    assert timing["status"] == "not_exercised"
    assert timing["evidence"]["unreadable_results"] == ["r1/results/x.json: JSONDecodeError"]


def test_timings_fall_back_to_secs_and_gates_json():
    res = perfect_result(SCENARIOS[0])
    res.pop("timings")
    t, source = mg.driver_timings(res)
    assert source == "secs"
    assert t["stop_to_notes_ms"] == pytest.approx(1200.0)
    assert len(t["chat_ms"]) == len(SCENARIOS[0]["chat"])  # restart chat excluded
    assert t["llm_calls_per_meeting"] == len(res["llm_calls"])
    case = mg.convert_timings(None, gates={"timings": {"stop_to_notes_ms": [5.0, 6.0], "chat_ms": []}})
    assert case["status"] == "ok" and case["evidence"]["source"] == "gates.json"
    assert set(case["metrics"]) == {"stop_to_notes_ms"}
    assert mg.convert_timings([{"scenario": "x"}])["status"] == "not_exercised"


# --------------------------------------------------------------------------
# Compare against policy.toml
# --------------------------------------------------------------------------


def _verdicts(base_cases, cand_cases) -> dict:
    report = compare_results(_valid(base_cases), _valid(cand_cases), load_policy())
    return {(r.case, r.metric): r for r in report.rows}, report


def _set_gate(cases, gate, value):
    cases = copy.deepcopy(cases)
    for c in cases:
        if c["case"] == f"meetings/gate/{gate}":
            c["metrics"][gate]["value"] = value
    return cases


def test_compare_identical_runs_pass(scored_run):
    cases = mg.load_run_dir(scored_run)
    rows, report = _verdicts(cases, cases)
    assert not report.blocked
    assert all(r.verdict == "pass" for r in rows.values())


@pytest.mark.parametrize(("gate", "value", "blocked"), [
    ("M1", 0.98, True),    # exact gate must be 1.0
    ("M4", 0.8, True),
    ("M13", 1.0, False),
    ("M5", 0.85, True),    # below the 0.90 floor (cap)
    ("M6", 0.96, False),   # within abs_tol of 1.0 and above floor
    ("M6", 0.9, True),     # beyond abs_tol 0.05
    ("M7", 0.04, False),   # within abs_tol, under 0.05 cap
    ("M7", 0.06, True),    # over the 0.05 cap
    ("M9", 0.79, True),
    ("M10", 0.97, False),
])
def test_compare_gate_regressions(scored_run, gate, value, blocked):
    base = mg.load_run_dir(scored_run)
    rows, report = _verdicts(base, _set_gate(base, gate, value))
    row = rows[(f"meetings/gate/{gate}", gate)]
    assert row.blocking is blocked, row
    assert report.blocked is blocked


def test_compare_new_meetings_cases_gated_by_caps(scored_run):
    """A first run (no baseline cases) still enforces exact gates and floors."""
    cand = _set_gate(mg.load_run_dir(scored_run), "M5", 0.5)
    rows, _report = _verdicts(mg.not_exercised_cases("python", "none"), cand)
    assert rows[("meetings/gate/M5", "M5")].blocking
    assert not rows[("meetings/gate/M1", "M1")].blocking


def test_compare_slower_stop_to_notes_warns_then_blocks(scored_run):
    base = mg.load_run_dir(scored_run)
    key = ("meetings/timings/python", "stop_to_notes_ms")
    base_samples = _by_case(base)[key[0]]["metrics"][key[1]]["samples"]

    def scaled(factor):
        cand = copy.deepcopy(base)
        _by_case(cand)[key[0]]["metrics"][key[1]] = make_metric(
            unit="ms", better="lower", samples=[x * factor for x in base_samples])
        return cand

    rows, report = _verdicts(base, scaled(1.5))
    assert rows[key].verdict == "regressed" and rows[key].warning and not rows[key].blocking
    assert not report.blocked
    rows, report = _verdicts(base, scaled(2.5))
    assert rows[key].blocking and report.blocked


# --------------------------------------------------------------------------
# Suite
# --------------------------------------------------------------------------


def test_rust_is_not_exercised():
    cases = meetings.run_meetings(_ctx("rust"))
    _valid(cases, "rust")
    assert len(cases) == len(mg.GATES) + 1 == 15
    assert all(c["status"] == "not_exercised" and c["error"] == meetings.RUST_REASON for c in cases)
    assert "meetings/timings/rust" in _by_case(cases)


def test_python_run_dir_option_and_env(scored_run, monkeypatch):
    cases = meetings.run_meetings(_ctx(run_dir=str(scored_run)))
    _valid(cases)
    assert all(c["status"] == "ok" for c in cases)
    assert _by_case(cases)["meetings/timings/python"]["evidence"]["run_dir"] == str(scored_run)
    monkeypatch.setenv("LF_PERF_MEETING_RUN_DIR", str(scored_run))
    assert [c["status"] for c in meetings.run_meetings(_ctx())] == ["ok"] * (len(mg.GATES) + 1)


def test_python_run_dir_without_gates_json_is_scored(tmp_path):
    run = write_run(tmp_path / "run", repeats=1)
    cases = meetings.run_meetings(_ctx(run_dir=str(run)))
    assert (run / "gates.json").is_file()
    assert all(c["status"] == "ok" for c in cases)


def test_python_missing_run_dir_fails(tmp_path):
    cases = meetings.run_meetings(_ctx(run_dir=str(tmp_path / "nope")))
    _valid(cases)
    assert all(c["status"] == "failed" for c in cases)


def _closed_port_url() -> str:
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    return f"http://127.0.0.1:{port}"


def test_python_without_ollama_is_not_exercised(monkeypatch, tmp_path):
    monkeypatch.setenv("LF_PERF_OLLAMA_URL", _closed_port_url())
    monkeypatch.setenv("LF_PERF_MEETING_OUT", str(tmp_path / "out"))
    cases = meetings.run_meetings(_ctx(tier="nightly", ollama_timeout_s=0.5))
    _valid(cases)
    assert len(cases) == len(mg.GATES) + 1
    assert all(c["status"] == "not_exercised" and "Ollama not reachable" in c["error"] for c in cases)
    assert not (tmp_path / "out").exists()


class _FakeOllama:
    """Minimal Ollama: ``/api/tags`` and ``/api/generate`` (what llm_ollama.py uses)."""

    def __init__(self, models: list[str]):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _send(self, body: dict):
                data = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self._send({"models": [{"name": m} for m in fake.models]})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
                fake.prompts.append(body.get("prompt", ""))
                self._send({"response": "## Summary\n- The transcript doesn't mention that.\n", "done": True})

        self.models = models
        self.prompts: list[str] = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture
def fake_ollama():
    servers = []

    def start(models):
        servers.append(_FakeOllama(models))
        return servers[-1]

    yield start
    for s in servers:
        s.close()


def test_model_matching_and_missing_model(fake_ollama):
    assert meetings.model_available(["llama3.2:3b"], "llama3.2:3b")
    assert meetings.model_available(["qwen:latest"], "qwen")
    assert meetings.model_available(["qwen"], "qwen:latest")
    assert not meetings.model_available(["llama3.2:1b"], "llama3.2:3b")
    srv = fake_ollama(["llama3.2:1b"])
    cases = meetings.run_meetings(_ctx(ollama_url=srv.url))
    assert all(c["status"] == "not_exercised" and "'llama3.2:3b' not available" in c["error"] for c in cases)
    assert meetings.ollama_status(srv.url, "llama3.2:1b") == (True, "ok")


def test_missing_product_deps_is_not_exercised(fake_ollama, tmp_path):
    srv = fake_ollama(["llama3.2:3b"])
    # An interpreter with nothing installed: isolated mode, no site-packages.
    stub = tmp_path / "py"
    stub.write_text(f"#!/bin/sh\nexec {sys.executable} -I -S \"$@\"\n")
    stub.chmod(0o755)
    if sys.platform == "win32":
        pytest.skip("shell stub interpreter")
    cases = meetings.run_meetings(_ctx(ollama_url=srv.url, python=str(stub)))
    assert all(c["status"] == "not_exercised" and "lacks meeting-sim dependencies" in c["error"] for c in cases)


def test_plan_repeats_and_scenarios():
    assert meetings.plan_repeats("smoke") == 1
    assert meetings.plan_repeats("nightly") == 3
    assert meetings.plan_repeats("smoke", "2") == 2
    with pytest.raises(ValueError):
        meetings.plan_repeats("smoke", 0)
    ids = [s["id"] for s in meetings.plan_scenarios(SCENARIOS, "eng-standup, pricing-review")]
    assert ids == ["eng-standup", "pricing-review"]
    assert len(meetings.plan_scenarios(SCENARIOS)) == len(SCENARIOS)
    with pytest.raises(ValueError):
        meetings.plan_scenarios(SCENARIOS, ["nope"])


STUB_DRIVER = """
import argparse, json, shutil, sys
from pathlib import Path
ap = argparse.ArgumentParser()
for flag in ("--scenario", "--out", "--sut", "--scenarios", "--llm-url", "--llm-model"):
    ap.add_argument(flag, required=True)
ap.add_argument("--cleanup", action="store_true")
a = ap.parse_args()
ids = [s["id"] for s in json.load(open(a.scenarios))["scenarios"]]
assert a.scenario in ids, (a.scenario, ids)
if a.scenario == {fail!r}:
    sys.exit("stub: boom")
Path(a.out).parent.mkdir(parents=True, exist_ok=True)
shutil.copy(Path({fixtures!r}) / (a.scenario + ".json"), a.out)
with open(Path({fixtures!r}) / "calls.log", "a") as fh:
    fh.write(" ".join([a.scenario, a.llm_url, a.llm_model, str(a.cleanup)]) + "\\n")
"""


def _stub_driver(tmp_path: Path, fail: str = "") -> tuple[list[str], Path]:
    fixtures = tmp_path / "fixtures"
    fixtures.mkdir()
    for sc in SCENARIOS:
        (fixtures / f"{sc['id']}.json").write_text(json.dumps(perfect_result(sc)))
    script = tmp_path / "stub_driver.py"
    script.write_text(STUB_DRIVER.format(fixtures=str(fixtures), fail=fail))
    return [sys.executable, str(script)], fixtures


def test_live_run_with_stub_driver_and_real_gates(tmp_path):
    cmd, fixtures = _stub_driver(tmp_path)
    out = tmp_path / "out"
    cases = meetings.run_meetings(_ctx(tier="nightly", driver_cmd=cmd, output_dir=str(out), repeats=2,
                                       ollama_url="http://ollama.test:1", llm_model="m:1"))
    _valid(cases)
    by = _by_case(cases)
    assert all(c["status"] == "ok" for c in cases), [c for c in cases if c["status"] != "ok"]
    assert json.loads((out / "gates.json").read_text())["summary"]["M2"]["pass_count"] == "2/2"
    assert sorted(p.parent.parent.name for p in out.glob("r*/results/*.json")) == ["r1"] * len(SCENARIOS) + ["r2"] * len(SCENARIOS)
    calls = (fixtures / "calls.log").read_text().splitlines()
    assert len(calls) == 2 * len(SCENARIOS) and all(c.endswith("http://ollama.test:1 m:1 True") for c in calls)
    ev = by["meetings/timings/python"]["evidence"]
    assert ev["repeats"] == 2 and ev["driver_failures"] == [] and ev["llm_model"] == "m:1"
    assert "MATRIX_DONE" in (out / "matrix.log").read_text()


def test_live_run_scenario_subset_and_driver_failure(tmp_path):
    cmd, _ = _stub_driver(tmp_path, fail="one-on-one")
    cases = meetings.run_meetings(_ctx(driver_cmd=cmd, output_dir=str(tmp_path / "out"),
                                       scenarios="pricing-review,one-on-one"))
    _valid(cases)
    by = _by_case(cases)
    m1 = by["meetings/gate/M1"]
    assert m1["metrics"]["M1"]["value"] == 0.5  # the failed scenario counts against M1
    assert m1["evidence"]["failures"] == {"r1": ["one-on-one:missing"]}
    assert by["meetings/gate/M12"]["status"] == "not_exercised"
    failures = by["meetings/timings/python"]["evidence"]["driver_failures"]
    assert [f["scenario"] for f in failures] == ["one-on-one"] and "boom" in failures[0]["tail"]
    rows, _report = _verdicts(cases, cases)
    assert rows[("meetings/gate/M1", "M1")].blocking


def test_live_run_every_driver_failing_is_failed(tmp_path):
    cmd = [sys.executable, "-c", "import sys; sys.exit('nope')"]
    cases = meetings.run_meetings(_ctx(driver_cmd=cmd, output_dir=str(tmp_path / "out"), scenarios="eng-standup"))
    _valid(cases)
    assert all(c["status"] == "failed" and "every driver run failed" in c["error"] for c in cases)


def test_cli_run_rust_smoke(tmp_path):
    out = tmp_path / "r.json"
    proc = subprocess.run([sys.executable, "-m", "evals.perf", "run", "--suite", "meetings", "--runtime", "rust",
                           "--tier", "smoke", "--output", str(out)], cwd=str(REPO_ROOT), capture_output=True,
                          text=True, check=False, timeout=120)
    assert proc.returncode == 0, proc.stderr
    data = json.loads(out.read_text())
    assert validate_result(data) == [] and len(data["cases"]) == len(mg.GATES) + 1


# --------------------------------------------------------------------------
# Real driver.py against a stub Ollama (needs the product deps)
# --------------------------------------------------------------------------


HAS_DRIVER_DEPS = all(importlib.util.find_spec(m) for m in meetings.PRODUCT_DEPS)


@pytest.mark.skipif(not HAS_DRIVER_DEPS, reason=f"driver.py needs {', '.join(meetings.PRODUCT_DEPS)}")
def test_real_driver_records_ms_timings(fake_ollama, tmp_path, monkeypatch):
    srv = fake_ollama(["llama3.2:3b"])
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    out = tmp_path / "out"
    cases = meetings.run_meetings(_ctx(ollama_url=srv.url, output_dir=str(out), scenarios="design-standup-nonotes",
                                       timeout_s=300))
    _valid(cases)
    res = json.loads((out / "r1" / "results" / "design-standup-nonotes.json").read_text())
    t = res["timings"]
    assert t["summarize_ms"] == t["stop_to_notes_ms"] > 0
    assert len(t["chat_ms"]) == len(SCENARIOS[4]["chat"])
    assert t["llm_calls_count"] == len(res["llm_calls"]) == len(t["llm_call_ms"]) >= 1
    assert all("ms" in c and "secs" in c for c in res["llm_calls"] + res["http"])
    assert res["llm"] == {"url": srv.url, "model": "llama3.2:3b"}
    assert not Path(res["tmp"]).exists()  # --cleanup
    assert srv.prompts
    assert _by_case(cases)["meetings/timings/python"]["status"] == "ok"


# --------------------------------------------------------------------------
# M14 speaker labels (diarized scenario, mock backend)
# --------------------------------------------------------------------------


def _diarized() -> dict:
    return next(s for s in SCENARIOS if s["id"] == DIARIZED_ID)


def _gates_for(tmp_path: Path, mutate=None, repeats: int = 1, scenarios: list[dict] | None = None) -> dict:
    run = write_run(tmp_path / "run", repeats=repeats, mutate=mutate)
    args = [str(run), "--sut", str(REPO_ROOT)]
    if scenarios is not None:
        subset = tmp_path / "subset.json"
        subset.write_text(json.dumps({"scenarios": scenarios}))
        keep = {s["id"] for s in scenarios}
        for path in run.glob("r*/results/*.json"):
            if path.stem not in keep:
                path.unlink()
        args += ["--scenarios", str(subset)]
    proc = run_gates_py(*args)
    assert proc.returncode == 0, proc.stderr
    return json.loads((run / "gates.json").read_text())


def _relabel(sid: str, fn):
    def mutate(_k, res):
        if res["scenario"] == sid:
            res["log_text"] = fn(res["log_text"])
    return mutate


def test_diarized_scenario_shape():
    sc = _diarized()
    assert SCENARIOS.index(sc) == 6 and sc["diarization_backend"] == "mock"
    assert len(sc["line_speakers"]) == len(sc["lines"]) <= 13
    assert all((src == "me") == (who == "me") for (src, _), who in zip(sc["lines"], sc["line_speakers"]))
    labels = speaker_labels(sc)
    assert {sc["line_speakers"][i]: lab for i, lab in labels.items()} == {
        "Ana": "Speaker 1", "Ben": "Speaker 2", "Chloe": "Speaker 3"}
    assert sum(1 for s in SCENARIOS if s.get("diarization_backend")) == 1


def test_m14_passes_on_perfect_run(scored_run):
    summary = json.loads((scored_run / "gates.json").read_text())["summary"]
    assert summary["M14"]["status"] == "pass"
    assert summary["M14"]["pooled_n"] == 2 * 8  # 7 remote lines + 1 speakers check per repeat
    assert summary["M2"]["status"] == "pass"


def test_m14_fails_when_labels_missing_but_m2_holds(tmp_path):
    import re

    data = _gates_for(tmp_path, _relabel(DIARIZED_ID, lambda t: re.sub(r"\*\*Speaker \d+:\*\*", "**Others:**", t)))
    s = data["summary"]
    assert s["M2"]["status"] == "pass" and data["per_repeat"]["r1"]["M2"]["value"] == 1.0
    assert s["M14"]["status"] == "fail"
    assert f"{DIARIZED_ID}#1" in data["per_repeat"]["r1"]["M14"]["fail"]


def test_m14_fails_when_speakers_swapped(tmp_path):
    def swap(text):
        return text.replace("**Speaker 2:**", "**Speaker X:**").replace(
            "**Speaker 3:**", "**Speaker 2:**").replace("**Speaker X:**", "**Speaker 3:**")

    data = _gates_for(tmp_path, _relabel(DIARIZED_ID, swap))
    assert data["summary"]["M2"]["status"] == "pass"
    assert data["summary"]["M14"]["status"] == "fail"
    fails = data["per_repeat"]["r1"]["M14"]["fail"]
    assert f"{DIARIZED_ID}#2" in fails and f"{DIARIZED_ID}:speakers" not in fails


def test_m14_speakers_check_needs_diarization_ok(tmp_path):
    def channel(_k, res):
        if res["scenario"] == DIARIZED_ID:
            res["stop"][1]["diarization"] = "channel"

    data = _gates_for(tmp_path, channel)
    assert data["per_repeat"]["r1"]["M14"]["fail"] == [f"{DIARIZED_ID}:speakers"]
    assert data["summary"]["M2"]["status"] == "pass"


def test_m2_relaxation_is_scenario_scoped(tmp_path):
    sid = SCENARIOS[0]["id"]
    data = _gates_for(tmp_path, _relabel(sid, lambda t: t.replace("**Others:**", "**Speaker 1:**")))
    fails = data["per_repeat"]["r1"]["M2"]["fail"]
    assert data["summary"]["M2"]["status"] == "fail"
    assert any(item.startswith(f"{sid}#") for item in fails)
    assert not any(item.startswith(DIARIZED_ID) for item in fails)
    assert data["summary"]["M14"]["status"] == "pass"


def test_m14_not_exercised_without_diarized_scenario(tmp_path):
    subset = [s for s in SCENARIOS if s["id"] != DIARIZED_ID]
    data = _gates_for(tmp_path, scenarios=subset)
    assert data["summary"]["M14"]["status"] == "not exercised" and data["summary"]["M14"]["pooled_n"] == 0
    case = _by_case(mg.convert_gates(data))["meetings/gate/M14"]
    assert case["status"] == "not_exercised"


def test_adapter_m14_dims_and_compare_blocks(scored_run):
    base = mg.load_run_dir(scored_run)
    case = _by_case(base)["meetings/gate/M14"]
    assert case["status"] == "ok" and case["metrics"]["M14"]["value"] == 1.0
    assert case["dims"]["check"] == "speaker_labels" and case["dims"]["class"] == "exact"
    rows, report = _verdicts(base, _set_gate(base, "M14", 0.9))
    assert rows[("meetings/gate/M14", "M14")].blocking and report.blocked


@pytest.mark.skipif(not HAS_DRIVER_DEPS, reason=f"driver.py needs {', '.join(meetings.PRODUCT_DEPS)}")
def test_real_driver_diarized_scenario_labels_speakers(fake_ollama, tmp_path, monkeypatch):
    """End to end: driver tone coding -> mock diarization collector -> Speaker k in the log."""
    srv = fake_ollama(["llama3.2:3b"])
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    out = tmp_path / "out"
    cases = meetings.run_meetings(_ctx(ollama_url=srv.url, output_dir=str(out), scenarios=DIARIZED_ID,
                                       timeout_s=300))
    _valid(cases)
    res = json.loads((out / "r1" / "results" / f"{DIARIZED_ID}.json").read_text())
    assert "**Speaker 3:**" in res["log_text"], res["log_text"]
    stop = res["stop"][1]
    assert stop["diarization"] == "ok"
    assert sorted(s["label"] for s in stop["speakers"]) == ["Speaker 1", "Speaker 2", "Speaker 3"]
    stats = res["diarization"]["stats"]
    assert stats["dropped"] == stats["failed"] == 0 and stats["processed"] == stats["offered"] == 7
    by = _by_case(cases)
    for gate in ("M2", "M14"):
        case = by[f"meetings/gate/{gate}"]
        assert case["status"] == "ok" and case["metrics"][gate]["value"] == 1.0, case
