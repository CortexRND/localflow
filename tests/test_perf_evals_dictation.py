"""Unit tests for the perf-eval dictation suite (text pipeline + HTTP).

Fast and deterministic: no Rust build and no real server. The Python text
pipeline runs on the real ``tests/fixtures/dictation_corpus.jsonl``; the Rust
side drives a stub ``parity`` executable, and the HTTP probe talks to a real
stdlib ``http.server`` stub subprocess.
"""

from __future__ import annotations

import ast
import json
import stat
import sys
import wave
from pathlib import Path

import pytest

from evals.perf import (
    compare_results,
    load_policy,
    make_result,
    make_run,
    registry,
    validate_result,
)
from evals.perf.benches import py_dictation_text
from evals.perf.registry import RunContext
from evals.perf.suites import dictation

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
CORPUS = REPO_ROOT / "tests" / "fixtures" / "dictation_corpus.jsonl"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
POSIX_ONLY = pytest.mark.skipif(sys.platform == "win32", reason="uses shebang stub executables")

CORPUS_CASES = dictation.load_dictation_corpus(CORPUS)


def _valid(cases: list[dict], runtime: str = "python") -> dict:
    run = make_run(tier="smoke", runtime=runtime, sha="abc", ref="test", os_name="linux", arch="x86_64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    result = make_result(run, cases)
    errors = validate_result(result)
    assert errors == [], errors
    assert all(c["suite"] == "dictation" for c in cases)
    return result


def _ctx(runtime: str = "python", tier: str = "smoke", **options) -> RunContext:
    return RunContext(tier=tier, runtime=runtime, repo_root=REPO_ROOT, options=options)


def _value(case: dict, metric: str):
    return case["metrics"][metric]["value"]


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    for var in ("LF_PERF_PARITY_BIN", "LF_PERF_LF_RS_BIN", "LF_PERF_DICTATION_HTTP",
                "LF_PERF_DICTATION_SKIP_HTTP", "LF_PERF_PYTHON"):
        monkeypatch.delenv(var, raising=False)


# --------------------------------------------------------------------------
# Registration and import hygiene
# --------------------------------------------------------------------------


def test_dictation_suite_is_registered():
    from evals.perf.suites import BUILTIN_SUITES, load_builtin_suites

    load_builtin_suites()
    assert "dictation" in BUILTIN_SUITES
    assert registry.get_suite("dictation") is dictation.run_dictation


@pytest.mark.parametrize("path", [PERF_DIR / "suites" / "dictation.py"])
def test_dictation_suite_imports_only_stdlib(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
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
            assert top != "localflow", f"{path} imports localflow"
            assert top in STDLIB_OK, f"{path} imports non-stdlib {mod}"


def test_text_bench_imports_only_stdlib_at_module_level():
    tree = ast.parse((PERF_DIR / "benches" / "py_dictation_text.py").read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            assert mod.split(".")[0] in STDLIB_OK, mod


def test_policy_classifies_dictation_metrics():
    policy = load_policy(PERF_DIR / "policy.toml")
    rules = policy["metrics"]
    assert rules["exact_match_rate"]["class"] == "exact"
    assert rules["error_rate"]["class"] == "exact"
    assert rules["error_rate"]["better"] == "lower"
    assert rules["wer"]["class"] == "quality"
    for name in ("case_us", "corpus_ms", "server_ms", "client_wall_ms", "cold_first_request_ms", "startup_ms"):
        assert rules[name]["class"] == "noisy", name


# --------------------------------------------------------------------------
# Text pipeline: Python
# --------------------------------------------------------------------------


def test_render_request_matches_parity_protocol():
    case = {"input": "x", "expected": "x", "config_overrides": {"spoken_formatting": True, "style": "casual"},
            "phrases": [["a", "b"]], "replacements": [["c", "d"]]}
    assert dictation.render_request(case) == {
        "op": "render", "text": "x", "phrases": [["a", "b"]], "replacements": [["c", "d"]],
        "formatting": True, "style": "casual",
    }
    bare = dictation.render_request({"input": "y", "expected": "y", "config_overrides": {}})
    assert bare["phrases"] == [] and bare["replacements"] == [] and bare["formatting"] is False
    assert bare["style"] is None


def test_python_bench_in_process_matches_corpus():
    report = py_dictation_text.run(CORPUS_CASES, passes=2, warmup=1, repeat=1)
    assert len(report["pass_ns"]) == 2
    rate, mismatches = dictation.score_outputs(CORPUS_CASES, [c["outputs"] for c in report["cases"]])
    assert mismatches == []
    assert rate == 1.0


def test_python_text_pipeline_on_real_corpus():
    case = dictation.run_text(_ctx(text_passes=3, text_repeat=2))
    _valid([case])
    assert case["case"] == "text/python"
    assert case["status"] == "ok", case["error"]
    assert _value(case, "exact_match_rate") == 1.0
    assert case["metrics"]["exact_match_rate"]["unit"] == "ratio"
    us = case["metrics"]["case_us"]
    assert us["unit"] == "us" and us["better"] == "lower"
    assert len(us["samples"]) == 3 and all(x > 0 for x in us["samples"])
    assert case["metrics"]["corpus_ms"]["median"] > 0
    assert case["evidence"]["n_cases"] == len(CORPUS_CASES) == 41
    assert case["evidence"]["mismatches"] == 0


def test_python_text_pipeline_detects_wrong_expected(tmp_path):
    bad = [dict(c) for c in CORPUS_CASES[:4]]
    bad[1]["expected"] = "definitely not the output"
    corpus = tmp_path / "corpus.jsonl"
    corpus.write_text("\n".join(json.dumps(c) for c in bad) + "\n", encoding="utf-8")
    case = dictation.run_text(_ctx(corpus=str(corpus), text_passes=1, text_repeat=1))
    _valid([case])
    assert _value(case, "exact_match_rate") == 0.75
    assert case["evidence"]["mismatch_examples"][0]["name"] == bad[1]["name"]


def test_python_text_bench_unavailable_is_not_exercised(tmp_path):
    fake = tmp_path / "python"
    fake.write_text("#!/bin/sh\necho 'unavailable: no localflow' >&2\nexit 3\n")
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
    if sys.platform == "win32":
        pytest.skip("shebang stub")
    case = dictation.run_text(_ctx(python=str(fake)))
    _valid([case])
    assert case["status"] == "not_exercised"
    assert "no localflow" in case["error"]


def test_unreadable_corpus_fails(tmp_path):
    case = dictation.run_text(_ctx(corpus=str(tmp_path / "missing.jsonl")))
    _valid([case])
    assert case["status"] == "failed"


# --------------------------------------------------------------------------
# Text pipeline: Rust (stub parity)
# --------------------------------------------------------------------------

PARITY_STUB = """#!{python}
import json, os, sys
reqs = json.loads(sys.stdin.read())
mode = os.environ.get("STUB_MODE", "ok")
expected = json.loads(os.environ["STUB_EXPECTED"])  # [[input, expected]] in corpus order
if mode == "crash" and reqs:
    sys.stderr.write("boom\\n"); sys.exit(7)
if mode == "garbage" and reqs:
    print("not json"); sys.exit(0)
if mode == "short" and reqs:
    print(json.dumps([])); sys.exit(0)
out = []
for i, r in enumerate(reqs):
    assert r["op"] == "render", r
    text, exp = expected[i % len(expected)]
    assert r["text"] == text, (r, text)
    if mode == "mismatch" and i % len(expected) == 0:
        out.append({{"ok": exp + "!"}})
    elif mode == "error" and i % len(expected) == 0:
        out.append({{"error": "render failed"}})
    else:
        out.append({{"ok": exp}})
print(json.dumps(out))
"""


@pytest.fixture
def parity_stub(tmp_path, monkeypatch):
    path = tmp_path / "parity"
    path.write_text(PARITY_STUB.format(python=sys.executable), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setenv("STUB_EXPECTED", json.dumps([[c["input"], c["expected"]] for c in CORPUS_CASES]))
    return path


def _rust_text(parity_stub, monkeypatch, mode: str) -> dict:
    monkeypatch.setenv("STUB_MODE", mode)
    case = dictation.run_text(_ctx("rust", parity_bin=str(parity_stub), text_passes=3, text_repeat=2))
    _valid([case], "rust")
    return case


@POSIX_ONLY
def test_rust_text_pipeline_matching(parity_stub, monkeypatch):
    case = _rust_text(parity_stub, monkeypatch, "ok")
    assert case["case"] == "text/rust" and case["status"] == "ok", case["error"]
    assert _value(case, "exact_match_rate") == 1.0
    assert len(case["metrics"]["case_us"]["samples"]) == 3
    assert all(x >= 0 for x in case["metrics"]["case_us"]["samples"])
    assert case["evidence"]["spawn_ms"] > 0
    assert case["evidence"]["repeat"] == 2


@POSIX_ONLY
def test_rust_text_pipeline_mismatch(parity_stub, monkeypatch):
    case = _rust_text(parity_stub, monkeypatch, "mismatch")
    assert case["status"] == "ok"
    assert _value(case, "exact_match_rate") == pytest.approx(40 / 41)
    example = case["evidence"]["mismatch_examples"][0]
    assert example["actual"] == CORPUS_CASES[0]["expected"] + "!"


@POSIX_ONLY
def test_rust_text_pipeline_error_response_is_mismatch(parity_stub, monkeypatch):
    case = _rust_text(parity_stub, monkeypatch, "error")
    assert _value(case, "exact_match_rate") == pytest.approx(40 / 41)
    assert case["evidence"]["mismatch_examples"][0]["actual"] == {"error": "render failed"}


@POSIX_ONLY
@pytest.mark.parametrize(("mode", "needle"), [("crash", "exited 7"), ("garbage", "invalid JSON"),
                                              ("short", "0 responses")])
def test_rust_text_pipeline_broken_binary_fails(parity_stub, monkeypatch, mode, needle):
    case = _rust_text(parity_stub, monkeypatch, mode)
    assert case["status"] == "failed"
    assert needle in case["error"]


def test_rust_text_pipeline_missing_binary(tmp_path):
    ctx = RunContext(tier="smoke", runtime="rust", repo_root=tmp_path,
                     options={"corpus": str(CORPUS)})
    case = dictation.run_text(ctx)
    _valid([case], "rust")
    assert case["status"] == "not_exercised"
    assert "parity" in case["error"]


@POSIX_ONLY
def test_find_parity_bin_env_override(parity_stub, monkeypatch, tmp_path):
    monkeypatch.setenv("LF_PERF_PARITY_BIN", str(parity_stub))
    assert dictation.find_parity_bin(tmp_path) == parity_stub.resolve()
    monkeypatch.setenv("LF_PERF_PARITY_BIN", str(tmp_path / "nope"))
    assert dictation.find_parity_bin(tmp_path) is None


def test_find_parity_bin_build_dir(tmp_path):
    exe = "parity.exe" if sys.platform == "win32" else "parity"
    built = tmp_path / "rust" / "target" / "debug" / "examples" / exe
    built.parent.mkdir(parents=True)
    built.write_text("")
    assert dictation.find_parity_bin(tmp_path) == built


# --------------------------------------------------------------------------
# HTTP: helpers
# --------------------------------------------------------------------------


def test_encode_multipart_round_trips():
    from email.parser import BytesParser
    from email.policy import HTTP

    body, ctype = dictation.encode_multipart("file", "a.wav", b"RIFF\x00\x01")
    msg = BytesParser(policy=HTTP).parsebytes(f"Content-Type: {ctype}\r\n\r\n".encode() + body)
    parts = list(msg.iter_parts())
    assert len(parts) == 1
    assert parts[0].get_param("name", header="content-disposition") == "file"
    assert parts[0].get_filename() == "a.wav"
    assert parts[0].get_payload(decode=True) == b"RIFF\x00\x01"


def test_http_config_text_is_valid_toml():
    import tomllib

    data = tomllib.loads(dictation.http_config_text(1234, "tiny.en", "faster-whisper"))
    assert data["server"] == {"host": "127.0.0.1", "port": 1234}
    assert data["stt"] == {"model": "tiny.en", "provider": "faster-whisper"}
    assert data["llm"]["cleanup_enabled"] is False


def test_http_enabled_by_tier_and_flags(monkeypatch):
    assert dictation.http_enabled(_ctx(tier="smoke")) == dictation.HTTP_SMOKE_REASON
    assert dictation.http_enabled(_ctx(tier="smoke", include_http=True)) is None
    assert dictation.http_enabled(_ctx(tier="nightly")) is None
    assert dictation.http_enabled(_ctx(tier="nightly", skip_http="1")) == dictation.HTTP_SKIPPED_REASON
    monkeypatch.setenv("LF_PERF_DICTATION_HTTP", "1")
    assert dictation.http_enabled(_ctx(tier="smoke")) is None
    monkeypatch.setenv("LF_PERF_DICTATION_SKIP_HTTP", "1")
    assert dictation.http_enabled(_ctx(tier="smoke")) == dictation.HTTP_SKIPPED_REASON


def test_http_models_by_tier():
    assert dictation.http_models(_ctx(tier="smoke")) == ["tiny.en"]
    assert "base.en" in dictation.http_models(_ctx(tier="nightly"))
    assert dictation.http_models(_ctx(tier="nightly", http_models="tiny, small")) == ["tiny", "small"]


def test_read_token_prefers_newest(tmp_path):
    import os

    seeded = dictation.seed_token(tmp_path)
    assert dictation.read_token(tmp_path) == seeded
    rotated = tmp_path / "config" / "localflow" / "api-token"
    rotated.write_text("rotated\n", encoding="utf-8")
    future = rotated.stat().st_mtime_ns + 10**9
    os.utime(rotated, ns=(future, future))
    assert dictation.read_token(tmp_path) == "rotated"


# --------------------------------------------------------------------------
# HTTP: end to end against a stub server
# --------------------------------------------------------------------------

SERVER_STUB = r'''
import json, os, sys, time, secrets, http.server
from email.parser import BytesParser
from email.policy import HTTP

port = int(sys.argv[1])
mode = os.environ.get("STUB_MODE", "ok")
answers = json.loads(os.environ["STUB_ANSWERS"])
config = open(os.environ["LOCALFLOW_CONFIG"], encoding="utf-8").read()
assert "cleanup_enabled = false" in config and f"port = {port}" in config, config
token = secrets.token_hex(16)  # rotate like `lf serve`
path = os.path.join(os.environ["XDG_CONFIG_HOME"], "localflow", "api-token")
time.sleep(0.05)
with open(path, "w", encoding="utf-8") as fh:
    fh.write(token + "\n")
seen = {"n": 0}

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/healthz":
            return self._send(200, {"ok": True})
        self._send(404, {"detail": "nope"})

    def do_POST(self):
        if not self.path.startswith("/transcribe") or "clean=0" not in self.path:
            return self._send(404, {"detail": "nope"})
        if self.headers.get("X-Localflow-Token") != token:
            return self._send(401, {"detail": "invalid API token"})
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        msg = BytesParser(policy=HTTP).parsebytes(
            f"Content-Type: {self.headers['Content-Type']}\r\n\r\n".encode() + raw)
        parts = [p for p in msg.iter_parts() if p.get_param("name", header="content-disposition") == "file"]
        if len(parts) != 1 or not parts[0].get_payload(decode=True).startswith(b"RIFF"):
            return self._send(400, {"detail": "bad upload"})
        clip = parts[0].get_filename().rsplit(".", 1)[0]
        seen["n"] += 1
        if mode == "fail_second" and seen["n"] == 2:
            return self._send(500, {"detail": "transcription failed"})
        if mode == "badjson":
            return self._send(200, b"<html>", "text/html")
        if mode == "wrongshape":
            return self._send(200, {"transcript": "x"})
        self._send(200, {"text": answers[clip], "ms": 12 + seen["n"]})

http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
'''


def _write_wav(path: Path, seconds: float = 0.1) -> None:
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"\x00\x00" * int(16000 * seconds))


@pytest.fixture
def clips(tmp_path):
    out = []
    for cid, ref in (("c1", "HELLO WORLD"), ("c2", "THE QUICK BROWN FOX")):
        path = tmp_path / f"{cid}.wav"
        _write_wav(path)
        out.append({"id": cid, "path": str(path), "reference": ref})
    return out


@pytest.fixture
def server_stub(tmp_path, monkeypatch):
    path = tmp_path / "server_stub.py"
    path.write_text(SERVER_STUB, encoding="utf-8")
    # c1 is perfect; c2 drops one of four words -> corpus WER 1/6.
    monkeypatch.setenv("STUB_ANSWERS", json.dumps({"c1": "Hello, world.", "c2": "The quick fox."}))
    return lambda port: [sys.executable, str(path), str(port)]


def _http(server_stub, clips, monkeypatch, mode="ok", runtime="python", runs=2) -> dict:
    monkeypatch.setenv("STUB_MODE", mode)
    case = dictation.run_http_case(_ctx(runtime), "tiny.en", clips, command=server_stub, runs=runs,
                                   request_timeout=10, startup_timeout=20)
    _valid([case], runtime)
    return case


def test_http_case_ok(server_stub, clips, monkeypatch):
    case = _http(server_stub, clips, monkeypatch)
    assert case["status"] == "ok", case["error"]
    assert case["case"] == "http/python/tiny.en"
    assert case["dims"] == {"mode": "http", "runtime": "python", "model": "tiny.en"}
    m = case["metrics"]
    assert m["startup_ms"]["value"] > 0 and m["startup_ms"]["unit"] == "ms"
    assert m["cold_first_request_ms"]["value"] > 0
    # 2 runs x 2 clips = 4 requests; the first (cold) is excluded from warm samples.
    assert m["server_ms"]["samples"] == [14.0, 15.0, 16.0]
    assert len(m["client_wall_ms"]["samples"]) == 3
    assert m["error_rate"]["value"] == 0.0
    assert m["wer"]["value"] == pytest.approx(1 / 6)
    assert case["evidence"]["cold_server_ms"] == 13.0
    assert case["evidence"]["transcripts"] == {"c1": "Hello, world.", "c2": "The quick fox."}


def test_http_case_counts_http_500(server_stub, clips, monkeypatch):
    case = _http(server_stub, clips, monkeypatch, mode="fail_second")
    assert case["status"] == "ok"
    assert case["metrics"]["error_rate"]["value"] == 0.25
    assert len(case["metrics"]["server_ms"]["samples"]) == 2
    assert "HTTP 500" in case["evidence"]["error_examples"][0]["error"]


@pytest.mark.parametrize(("mode", "needle"), [("badjson", "invalid JSON"), ("wrongshape", "not {'text'")])
def test_http_case_bad_responses(server_stub, clips, monkeypatch, mode, needle):
    case = _http(server_stub, clips, monkeypatch, mode=mode, runs=1)
    assert case["metrics"]["error_rate"]["value"] == 1.0
    assert "server_ms" not in case["metrics"] and "wer" not in case["metrics"]
    assert needle in case["evidence"]["error_examples"][0]["error"]


def test_http_case_wrong_token_is_401(server_stub, clips, monkeypatch, tmp_path):
    monkeypatch.setattr(dictation, "read_token", lambda root: "wrong")
    case = _http(server_stub, clips, monkeypatch, runs=1)
    assert case["metrics"]["error_rate"]["value"] == 1.0
    assert "HTTP 401" in case["evidence"]["error_examples"][0]["error"]


def test_http_case_server_exits_early(clips, tmp_path):
    case = dictation.run_http_case(
        _ctx(), "tiny.en", clips,
        command=lambda port: [sys.executable, "-c", "import sys; print('dead'); sys.exit(4)"],
        startup_timeout=20,
    )
    _valid([case])
    assert case["status"] == "failed"
    assert "exited 4" in case["error"] and "dead" in case["error"]


def test_http_compare_flags_error_rate_regression(server_stub, clips, monkeypatch):
    good = _http(server_stub, clips, monkeypatch)
    bad = _http(server_stub, clips, monkeypatch, mode="fail_second")
    base, cand = _valid([good]), _valid([bad])
    report = compare_results(base, cand, load_policy(PERF_DIR / "policy.toml"))
    rows = {r.metric: r for r in report.rows}
    assert rows["error_rate"].verdict == "regressed"
    assert rows["error_rate"].blocking


def test_http_missing_rust_binary_not_exercised(tmp_path, clips):
    ctx = RunContext(tier="nightly", runtime="rust", repo_root=tmp_path,
                     options={"clips": clips, "http_models": "tiny.en"})
    cases = dictation.run_http(ctx)
    _valid(cases, "rust")
    assert [c["status"] for c in cases] == ["not_exercised"]
    assert "lf-rs" in cases[0]["error"]


def test_http_smoke_default_not_exercised():
    cases = dictation.run_http(_ctx(tier="smoke"))
    assert [c["case"] for c in cases] == ["http/python/tiny.en"]
    assert cases[0]["status"] == "not_exercised"
    assert cases[0]["error"] == dictation.HTTP_SMOKE_REASON


def test_resolve_clips_from_file(tmp_path, clips):
    manifest = tmp_path / "clips.json"
    manifest.write_text(json.dumps({"clips": clips}), encoding="utf-8")
    assert dictation.resolve_clips(_ctx(clips=str(manifest))) == clips
    with pytest.raises(ValueError):
        dictation.resolve_clips(_ctx(clips=[{"id": "x"}]))


def test_run_http_uses_injected_clips(server_stub, clips, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "ok")
    monkeypatch.setattr(dictation, "server_command", lambda ctx, port: server_stub(port))
    cases = dictation.run_http(_ctx(tier="nightly", clips=clips, http_models="tiny.en", http_runs=1))
    _valid(cases)
    assert [c["status"] for c in cases] == ["ok"]
    assert cases[0]["evidence"]["requests"] == 2


# --------------------------------------------------------------------------
# Whole suite
# --------------------------------------------------------------------------


def test_run_dictation_smoke_python():
    cases = dictation.run_dictation(_ctx(text_passes=2, text_repeat=1))
    _valid(cases)
    by = {c["case"]: c for c in cases}
    assert by["text/python"]["status"] == "ok"
    assert by["http/python/tiny.en"]["status"] == "not_exercised"


def test_run_dictation_rust_without_binaries(tmp_path):
    ctx = RunContext(tier="nightly", runtime="rust", repo_root=tmp_path,
                     options={"corpus": str(CORPUS), "http_models": "tiny.en"})
    cases = dictation.run_dictation(ctx)
    _valid(cases, "rust")
    assert {c["case"]: c["status"] for c in cases} == {
        "text/rust": "not_exercised", "http/rust/tiny.en": "not_exercised",
    }


def test_run_dictation_rejects_unknown_runtime():
    with pytest.raises(ValueError):
        dictation.run_dictation(_ctx("java"))
