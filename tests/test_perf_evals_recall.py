"""Unit tests for the perf-eval recall suite, corpus generator and benches.

Fast and deterministic: small generated corpora, the real Python recall code
(``localflow.recall``) for the Python bench, and a stub ``bench_search``
executable for the Rust side (no Rust build needed).
"""

from __future__ import annotations

import ast
import calendar
import json
import re
import stat
import sys
from datetime import datetime
from pathlib import Path

import pytest

from evals.perf import load_policy, make_result, make_run, registry, validate_result
from evals.perf.benches import py_recall
from evals.perf.fixtures import make_recall_corpus as mrc
from evals.perf.registry import RunContext
from evals.perf.suites import BUILTIN_SUITES, recall

REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_DIR = REPO_ROOT / "evals" / "perf"
STDLIB_OK = set(sys.stdlib_module_names) | {"__future__"}
POSIX_ONLY = pytest.mark.skipif(sys.platform == "win32", reason="uses shebang stub executables")
NOW = datetime(2026, 11, 14, 10, 0, 0)
SMALL = 50
# The stem pattern both runtimes parse (localflow/recall.py _FILE_STAMP_RE).
FILE_STAMP = re.compile(r"^\d{4}-\d{2}-\d{2} \d{4} \S.*$")


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch):
    for var in ("LF_PERF_RECALL_BIN", "LF_PERF_PYTHON", "LF_PERF_CACHE_DIR", "LF_PERF_RECALL_PARITY"):
        monkeypatch.delenv(var, raising=False)


@pytest.fixture(scope="module")
def corpus(tmp_path_factory) -> dict:
    return mrc.generate_corpus(tmp_path_factory.mktemp("recall") / "c", entries=SMALL, seed=7)


def _valid(cases: list[dict], runtime: str) -> dict:
    run = make_run(tier="smoke", runtime=runtime, sha="abc", ref="test", os_name="linux", arch="x86_64",
                   runner={"label": "t", "cpu": "c", "cores": 1, "ram_gb": 1})
    result = make_result(run, cases)
    errors = validate_result(result)
    assert errors == [], errors
    assert all(c["suite"] == "recall" for c in cases)
    return result


def _ctx(runtime: str = "python", tier: str = "smoke", **options) -> RunContext:
    return RunContext(tier=tier, runtime=runtime, repo_root=REPO_ROOT, options=options)


def _files(root: Path) -> dict[str, bytes]:
    return {p.relative_to(root).as_posix(): p.read_bytes() for p in sorted(root.rglob("*")) if p.is_file()}


# --------------------------------------------------------------------------
# Registration and import hygiene
# --------------------------------------------------------------------------


def test_recall_suite_is_registered():
    assert "recall" in BUILTIN_SUITES
    assert registry.get_suite("recall") is recall.run_recall


@pytest.mark.parametrize("path", [
    PERF_DIR / "suites" / "recall.py",
    PERF_DIR / "fixtures" / "make_recall_corpus.py",
    PERF_DIR / "fixtures" / "__init__.py",
    PERF_DIR / "benches" / "py_recall.py",
])
def test_recall_modules_import_only_stdlib_at_module_level(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    for node in tree.body:  # module level; the bench imports localflow lazily
        if isinstance(node, ast.Import):
            mods = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            mods = [node.module or ""]
        else:
            continue
        for mod in mods:
            assert mod.split(".")[0] in STDLIB_OK, f"{path} imports non-stdlib {mod}"


def test_suite_never_imports_localflow():
    for path in (PERF_DIR / "suites" / "recall.py", PERF_DIR / "fixtures" / "make_recall_corpus.py"):
        assert "localflow." not in "".join(
            ast.unparse(n) for n in ast.walk(ast.parse(path.read_text(encoding="utf-8")))
            if isinstance(n, (ast.Import, ast.ImportFrom))
        )


def test_policy_classifies_recall_metrics():
    policy = load_policy()
    rules = policy["metrics"]
    assert rules["citation_parity"]["class"] == "exact"
    assert rules["citation_parity"]["on_pr"] == "block"
    assert rules["search_ms"]["class"] == "noisy"
    assert rules["search_ms"]["rel_tol"] == pytest.approx(0.15)
    assert rules["peak_rss_bytes"]["class"] == "noisy"
    assert rules["peak_rss_bytes"]["rel_tol"] == pytest.approx(0.10)


# --------------------------------------------------------------------------
# Corpus generator
# --------------------------------------------------------------------------


def test_generator_is_deterministic(tmp_path):
    a = mrc.generate_corpus(tmp_path / "a", entries=SMALL, seed=3)
    b = mrc.generate_corpus(tmp_path / "b", entries=SMALL, seed=3)
    c = mrc.generate_corpus(tmp_path / "c", entries=SMALL, seed=4)
    assert a["digest"] == b["digest"] != c["digest"]
    assert mrc.read_rows(a["db_path"]) == mrc.read_rows(b["db_path"])
    assert mrc.read_rows(a["db_path"]) != mrc.read_rows(c["db_path"])
    files_a, files_b = _files(Path(a["vault_path"])), _files(Path(b["vault_path"]))
    assert files_a == files_b and files_a
    assert Path(a["queries_path"]).read_bytes() == Path(b["queries_path"]).read_bytes()
    # Regenerating in place yields the same corpus.
    again = mrc.generate_corpus(tmp_path / "a", entries=SMALL, seed=3)
    assert again["digest"] == a["digest"]
    assert _files(Path(again["vault_path"])) == files_a


def test_generator_layout_and_rows(corpus):
    rows = mrc.read_rows(corpus["db_path"])
    assert [r[0] for r in rows] == list(range(1, SMALL + 1))
    # Whole seconds, >= 1 day before REFERENCE_NOW read as UTC (so before it in every zone).
    anchor = calendar.timegm((2026, 11, 14, 10, 0, 0))
    assert all(r[1] == int(r[1]) and r[1] <= anchor - 86400 for r in rows)
    assert [r[1] for r in rows] == sorted(r[1] for r in rows)
    for row in rows:
        record = dict(zip(mrc.COLUMNS, row))
        assert record["kind"] in ("dictate", "command", "transform")
        assert record["status"] in ("ok", "failed", "silent")
        assert record["words"] == len(record["final_text"].split())
    vault = Path(corpus["vault_path"])
    transcripts = sorted(vault.glob("Transcripts/**/* (transcript).md"))
    notes = sorted(vault.glob("Notes/*.md"))
    assert len(transcripts) == corpus["counts"]["transcripts"] >= 1
    assert len(notes) == corpus["counts"]["notes"] > len(transcripts)
    for path in transcripts:
        text = path.read_text(encoding="utf-8")
        assert text.startswith("---\n") and " — transcript\n" in text and "\nNotes: [[" in text
        assert FILE_STAMP.match(path.name.removesuffix(" (transcript).md")), path.name
    queries = json.loads(Path(corpus["queries_path"]).read_text(encoding="utf-8"))
    assert 10 <= len(queries) <= 25
    assert any(isinstance(q, dict) and q.get("since") for q in queries)


def test_generator_refuses_foreign_folder(tmp_path):
    (tmp_path / "keep.txt").write_text("mine", encoding="utf-8")
    with pytest.raises(ValueError, match="refusing"):
        mrc.generate_corpus(tmp_path, entries=5)
    assert (tmp_path / "keep.txt").read_text(encoding="utf-8") == "mine"
    with pytest.raises(ValueError):
        mrc.generate_corpus(tmp_path / "x", entries=0)


def test_ensure_corpus_reuses_matching_cache(tmp_path):
    first = mrc.ensure_corpus(tmp_path / "c", entries=20, seed=1)
    marker = Path(first["root"]) / "history.sqlite"
    mtime = marker.stat().st_mtime_ns
    assert mrc.ensure_corpus(tmp_path / "c", entries=20, seed=1)["digest"] == first["digest"]
    assert marker.stat().st_mtime_ns == mtime  # reused, not rewritten
    assert mrc.load_corpus(tmp_path / "c", entries=21, seed=1) is None
    assert mrc.ensure_corpus(tmp_path / "c", entries=21, seed=1)["entries"] == 21


def test_generator_cli(tmp_path, capsys):
    assert mrc.main(["--out", str(tmp_path / "cli"), "--entries", "12", "--seed", "5"]) == 0
    printed = json.loads(capsys.readouterr().out)
    assert printed["entries"] == 12 and printed["seed"] == 5
    assert (tmp_path / "cli" / "recall_corpus.json").is_file()


def test_corpus_is_readable_by_localflow(corpus):
    from localflow.history import HistoryStore
    from localflow.recall import build_corpus, search

    store = HistoryStore(Path(corpus["db_path"]))
    assert len(store.recent(limit=1000)) == SMALL
    passages = build_corpus(store=store, vault_path=corpus["vault_path"], notes_folder="Notes",
                            logs_folder="Transcripts")
    assert {p.source for p in passages} == {"dictation", "meeting", "notes"}
    # Vault stamps come from filenames, not mtimes.
    assert all(p.ts < NOW.timestamp() for p in passages)
    result = search("can everyone see my screen agenda", store=store, vault_path=corpus["vault_path"],
                    notes_folder="Notes", logs_folder="Transcripts", now=NOW)
    assert any(c["source"] == "meeting" for c in result.citations)
    assert search("xylophone quasar nebula", store=store, vault_path=corpus["vault_path"],
                  notes_folder="Notes", logs_folder="Transcripts", now=NOW).citations == []


# --------------------------------------------------------------------------
# Python bench
# --------------------------------------------------------------------------


def test_load_queries_formats(tmp_path):
    js = tmp_path / "q.json"
    js.write_text(json.dumps(["a b", {"query": "c", "since": "7d"}, {"query": "d", "since": None}]), encoding="utf-8")
    assert py_recall.load_queries(js) == [("a b", None), ("c", "7d"), ("d", None)]
    txt = tmp_path / "q.txt"
    txt.write_text("\ufeffbudget\n\n  zephyr \n", encoding="utf-8")
    assert py_recall.load_queries(txt) == [("budget", None), ("zephyr", None)]
    for bad in ("[]", "[1]", '["a", "a"]', '[{"query": "a", "since": 3}]', ""):
        txt.write_text(bad, encoding="utf-8")
        with pytest.raises((TypeError, ValueError)):
            py_recall.load_queries(txt)
    assert py_recall.query_key("c", "7d") == mrc.query_key({"query": "c", "since": "7d"}) == "c (since 7d)"


def test_normalize_ref_matches_rust_rules():
    assert py_recall.normalize_ref("/v/Transcripts/Zoom/x.md#01:02", "/v") == "Transcripts/Zoom/x.md#01:02"
    assert py_recall.normalize_ref("/v/Notes/a.md", "/v/") == "Notes/a.md"
    assert py_recall.normalize_ref(r"C:\c\vault\Notes\a.md", r"C:\c\vault") == "Notes/a.md"
    assert py_recall.normalize_ref("42", "/v") == "42"
    assert py_recall.normalize_ref("/v2/Notes/a.md", "/v") == "/v2/Notes/a.md"


def test_python_bench_in_process(corpus):
    queries = py_recall.load_queries(Path(corpus["queries_path"]))
    report = py_recall.run_bench(Path(corpus["db_path"]), Path(corpus["vault_path"]), queries,
                                 iterations=2, warmup=1, now=NOW)
    assert report["runtime"] == "python"
    assert report["queries"] == [py_recall.query_key(q, s) for q, s in queries]
    assert all(len(report["milliseconds"][k]) == 2 for k in report["queries"])
    assert len(report["all_milliseconds"]) == 2 * len(queries)
    assert recall.parse_report(json.dumps(report))["queries"] == report["queries"]
    assert report["citations_by_query"]["xylophone quasar nebula"] == []
    refs = [c["ref"] for cs in report["citations_by_query"].values() for c in cs]
    assert refs and not any(r.startswith(corpus["vault_path"]) for r in refs)
    if sys.platform != "win32":
        assert isinstance(report["peak_rss_bytes"], int) and report["peak_rss_bytes"] > 0


def test_python_bench_cli_bad_input(tmp_path, capsys):
    assert py_recall.main(["--db", str(tmp_path / "missing.sqlite"), "--vault", str(tmp_path),
                           "--queries", str(tmp_path / "nope.json")]) == py_recall.EXIT_BAD_INPUT
    assert py_recall.main(["--db", "x", "--vault", "y", "--queries", "z", "--iterations", "0"]) == 2


# --------------------------------------------------------------------------
# Citation comparison
# --------------------------------------------------------------------------


def _cit(snippet: str, ref: str = "1", source: str = "dictation", n: int = 1) -> dict:
    return {"n": n, "source": source, "title": "Slack", "app": "Slack", "when": "2026-11-01T09:00",
            "snippet": snippet, "ref": ref}


def test_compare_citation_sets_exact_match():
    a = {"q1": [_cit("a"), _cit("b", "2", n=2)], "q2": []}
    b = json.loads(json.dumps(a))
    assert recall.compare_citation_sets(a, b) == {
        "parity": 1.0, "matched": 2, "total": 2, "mismatched": [], "missing": []}
    assert recall.citations_digest(a) == recall.citations_digest(dict(reversed(list(b.items()))))
    assert recall.compare_citation_sets({}, {})["parity"] == 1.0


def test_compare_citation_sets_order_and_content_mismatch():
    ref = {"q1": [_cit("a"), _cit("b", "2", n=2)], "q2": [_cit("c")], "q3": [_cit("d")]}
    cand = {"q1": [_cit("b", "2", n=1), _cit("a", n=2)],  # same set, different order
            "q2": [_cit("c", "9")],                         # different ref
            "q3": [_cit("d")]}
    out = recall.compare_citation_sets(cand, ref)
    assert out["mismatched"] == ["q1", "q2"] and out["matched"] == 1
    assert out["parity"] == pytest.approx(1 / 3)
    assert recall.citations_digest(cand) != recall.citations_digest(ref)
    # Extra / dropped citations also mismatch.
    assert recall.compare_citation_sets({"q": [_cit("a")]}, {"q": [_cit("a"), _cit("b")]})["parity"] == 0.0


def test_compare_citation_sets_missing_query():
    out = recall.compare_citation_sets({"q1": []}, {"q1": [], "q2": []})
    assert out["missing"] == ["q2"] and out["parity"] == 0.5
    out = recall.compare_citation_sets({"q1": [], "extra": []}, {"q1": []})
    assert out["missing"] == ["extra"] and out["total"] == 2
    assert recall.compare_citation_sets({"q": "junk"}, {"q": "junk"})["mismatched"] == ["q"]


def test_load_reference_citations(tmp_path):
    cbq = {"q": [_cit("a")]}
    assert recall.load_reference_citations(cbq) == cbq
    assert recall.load_reference_citations({"citations_by_query": cbq, "queries": ["q"]}) == cbq
    path = tmp_path / "ref.json"
    path.write_text(json.dumps({"citations_by_query": cbq}), encoding="utf-8")
    assert recall.load_reference_citations(str(path)) == cbq
    with pytest.raises(TypeError):
        recall.load_reference_citations([1])


# --------------------------------------------------------------------------
# Matrix and discovery
# --------------------------------------------------------------------------


def test_plan_sizes():
    assert recall.plan_sizes("smoke") == [1000]
    assert recall.plan_sizes("nightly") == [1000, 10000]
    assert recall.plan_sizes("deep") == [1000, 10000]
    assert recall.plan_sizes("nightly", "1k, 50") == [1000, 50]
    assert recall.plan_sizes("smoke", [10000, "10k"]) == [10000]
    with pytest.raises(ValueError):
        recall.plan_sizes("weekly")
    with pytest.raises(ValueError):
        recall.plan_sizes("smoke", "0")
    assert [recall.size_label(n) for n in (1000, 10000, 50, 1500)] == ["1k", "10k", "50", "1500"]
    assert recall.case_name("rust", 10000) == "rust/10k"


def test_find_recall_bench(tmp_path, monkeypatch):
    assert recall.find_recall_bench(tmp_path) is None
    debug = tmp_path / "rust" / "target" / "debug" / "examples"
    debug.mkdir(parents=True)
    exe = "bench_search.exe" if sys.platform == "win32" else "bench_search"
    (debug / exe).write_text("", encoding="utf-8")
    assert recall.find_recall_bench(tmp_path) == (debug / exe).resolve()
    release = tmp_path / "rust" / "target" / "release" / "examples"
    release.mkdir(parents=True)
    (release / exe).write_text("", encoding="utf-8")
    assert recall.find_recall_bench(tmp_path) == (release / exe).resolve()
    other = tmp_path / "custom"
    other.write_text("", encoding="utf-8")
    monkeypatch.setenv("LF_PERF_RECALL_BIN", str(other))
    assert recall.find_recall_bench(tmp_path) == other.resolve()
    monkeypatch.setenv("LF_PERF_RECALL_BIN", str(tmp_path / "missing"))
    assert recall.find_recall_bench(tmp_path) is None


@pytest.mark.parametrize("stdout, needle", [
    ("", "no JSON"),
    ("[]", "object"),
    ('{"queries": []}', "queries"),
    ('{"queries": ["a"], "milliseconds": {"a": [1]}, "citations_by_query": {}, "all_milliseconds": [1]}', "citation"),
    ('{"queries": ["a"], "milliseconds": {"a": []}, "citations_by_query": {"a": []}, "all_milliseconds": [1]}',
     "timings"),
    (('{"queries": ["a"], "milliseconds": {"a": [1]}, "citations_by_query": {"a": []}, "all_milliseconds": [1],'
      ' "peak_rss_bytes": "big"}'), "peak_rss_bytes"),
])
def test_parse_report_rejects_malformed(stdout, needle):
    with pytest.raises((TypeError, ValueError), match=needle):
        recall.parse_report(stdout)


def test_parse_report_accepts_trailing_json_line():
    line = json.dumps({"queries": ["a"], "milliseconds": {"a": [1.5]}, "citations_by_query": {"a": []},
                       "all_milliseconds": [1.5], "peak_rss_bytes": None})
    assert recall.parse_report("log noise\n" + line)["all_milliseconds"] == [1.5]


# --------------------------------------------------------------------------
# Suite: Python end to end
# --------------------------------------------------------------------------


def test_python_suite_end_to_end(tmp_path):
    cases = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=2, warmup=0, parity="off",
                                   cache_dir=str(tmp_path)))
    _valid(cases, "python")
    (case,) = cases
    assert case["case"] == f"python/{SMALL}" and case["status"] == "ok", case["error"]
    assert case["dims"] == {"runtime": "python", "size": str(SMALL), "entries": SMALL}
    metrics = case["metrics"]
    n_queries = len(mrc.QUERIES)
    assert metrics["search_ms"]["unit"] == "ms" and metrics["search_ms"]["better"] == "lower"
    assert len(metrics["search_ms"]["samples"]) == 2 * n_queries
    if sys.platform != "win32":
        assert metrics["peak_rss_bytes"]["unit"] == "bytes" and metrics["peak_rss_bytes"]["value"] > 0
    assert "citation_parity" not in metrics
    ev = case["evidence"]
    assert ev["parity"] == {"status": "disabled"}
    assert len(ev["citations_by_query"]) == n_queries
    assert ev["citations_sha256"] == recall.citations_digest(ev["citations_by_query"])
    assert ev["hits_by_query"]["xylophone quasar nebula"] == 0
    # Corpus cached under cache_dir and reused on the next run.
    assert (tmp_path / f"n{SMALL}-s{mrc.DEFAULT_SEED}" / "recall_corpus.json").is_file()


def test_python_suite_with_reference_citations(tmp_path):
    first = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0, parity="off",
                                   cache_dir=str(tmp_path)))[0]
    ref = first["evidence"]["citations_by_query"]
    again = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0, cache_dir=str(tmp_path),
                                   reference_citations=ref))[0]
    assert again["metrics"]["citation_parity"]["value"] == 1.0
    assert again["metrics"]["citation_parity"]["better"] == "higher"
    assert again["evidence"]["citations_sha256"] == first["evidence"]["citations_sha256"]
    broken = json.loads(json.dumps(ref))
    key = next(k for k, v in broken.items() if v)
    broken[key][0]["snippet"] += " (changed)"
    bad = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0, cache_dir=str(tmp_path),
                                 reference_citations=broken))[0]
    assert bad["metrics"]["citation_parity"]["value"] < 1.0
    assert bad["evidence"]["parity"]["mismatched"] == [key]


@POSIX_ONLY
def test_python_bench_unavailable_is_not_exercised(tmp_path):
    fake = tmp_path / "python-without-localflow"
    fake.write_text(f"#!{sys.executable}\nimport sys\nsys.stderr.write('no localflow')\nsys.exit(3)\n",
                    encoding="utf-8")
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
    (case,) = recall.run_recall(_ctx("python", sizes=[10], python=str(fake), parity="off", cache_dir=str(tmp_path)))
    assert case["status"] == "not_exercised" and "no localflow" in case["error"]


# --------------------------------------------------------------------------
# Suite: Rust (stub bench_search)
# --------------------------------------------------------------------------

BENCH_STUB = """#!{python}
import json, os, sys
args = sys.argv[1:]
mode = os.environ.get("STUB_MODE", "ok")
if mode == "crash":
    sys.stderr.write("boom\\n"); sys.exit(7)
if mode == "garbage":
    print("not json"); sys.exit(0)
assert args[0].endswith("history.sqlite") and args[1].endswith("vault"), args
flags = dict(zip(args[2::2], args[3::2]))
assert flags["--now"] == "2026-11-14T10:00:00", flags
iterations, warmup = int(flags["--iterations"]), int(flags["--warmup"])
log = os.environ.get("STUB_LOG")
if log:
    with open(log, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({{"iterations": iterations, "warmup": warmup}}) + "\\n")
keys = []
for q in json.load(open(flags["--queries"], encoding="utf-8")):
    keys.append(q if isinstance(q, str) else (q["query"] + (" (since %s)" % q["since"] if q.get("since") else "")))
cites = {{}}
if os.environ.get("STUB_CITATIONS"):
    cites = json.load(open(os.environ["STUB_CITATIONS"], encoding="utf-8"))
def fake(k):
    return [{{"n": 1, "source": "dictation", "title": "Slack", "app": "Slack", "when": "2026-11-01T09:00",
             "snippet": k, "ref": "1"}}]
out = {{
    "runtime": "rust", "now": flags["--now"], "iterations": iterations, "warmup": warmup, "queries": keys,
    "milliseconds": {{k: [1.0 + i for i in range(iterations)] for k in keys}},
    "all_milliseconds": [1.0 + i for k in keys for i in range(iterations)],
    "citations_by_query": {{k: cites.get(k, fake(k)) for k in keys}},
    "peak_rss_bytes": None if mode == "no_rss" else 12345678,
}}
print(json.dumps(out))
"""


@pytest.fixture
def bench_stub(tmp_path, monkeypatch):
    path = tmp_path / "bench_search"
    path.write_text(BENCH_STUB.format(python=sys.executable), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setenv("STUB_LOG", str(tmp_path / "stub.log"))
    return path


def _stub_calls(bench_stub: Path) -> list[dict]:
    log = bench_stub.parent / "stub.log"
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()] if log.exists() else []


def _rust(bench_stub, tmp_path, **options) -> dict:
    opts = {"sizes": [SMALL], "recall_bin": str(bench_stub), "cache_dir": str(tmp_path / "cache"), **options}
    (case,) = recall.run_recall(_ctx("rust", **opts))
    _valid([case], "rust")
    return case


@POSIX_ONLY
def test_rust_suite_valid_output(bench_stub, tmp_path, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "ok")
    case = _rust(bench_stub, tmp_path, parity="off")
    assert case["case"] == f"rust/{SMALL}" and case["status"] == "ok", case["error"]
    assert case["metrics"]["search_ms"]["samples"] == [1.0, 2.0, 3.0, 4.0, 5.0] * len(mrc.QUERIES)
    assert case["metrics"]["peak_rss_bytes"]["value"] == 12345678
    assert "citation_parity" not in case["metrics"]
    assert _stub_calls(bench_stub) == [{"iterations": 5, "warmup": 1}]


@POSIX_ONLY
def test_rust_suite_without_rss(bench_stub, tmp_path, monkeypatch):
    monkeypatch.setenv("STUB_MODE", "no_rss")
    case = _rust(bench_stub, tmp_path, parity="off", iterations=2)
    assert case["status"] == "ok" and "peak_rss_bytes" not in case["metrics"]


@POSIX_ONLY
def test_rust_suite_parity_against_python(bench_stub, tmp_path, monkeypatch):
    """Default parity: the Python bench is the reference; a stub echoing it matches, a fake one does not."""
    py = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0, parity="off",
                                cache_dir=str(tmp_path / "cache")))[0]
    echoed = tmp_path / "cites.json"
    echoed.write_text(json.dumps(py["evidence"]["citations_by_query"]), encoding="utf-8")
    monkeypatch.setenv("STUB_CITATIONS", str(echoed))
    case = _rust(bench_stub, tmp_path, iterations=1, warmup=0)
    assert case["metrics"]["citation_parity"]["value"] == 1.0, case["evidence"]["parity"]
    assert case["evidence"]["parity"]["reference"] == "python"
    assert case["evidence"]["citations_sha256"] == py["evidence"]["citations_sha256"]

    monkeypatch.delenv("STUB_CITATIONS")
    case = _rust(bench_stub, tmp_path, iterations=1, warmup=0)
    parity = case["evidence"]["parity"]
    # The stub's single fake citation per query matches none of Python's lists (empty ones included).
    assert case["metrics"]["citation_parity"]["value"] == 0.0
    assert parity["status"] == "compared" and len(parity["mismatched"]) == len(mrc.QUERIES)


@POSIX_ONLY
def test_python_suite_parity_against_rust_stub(bench_stub, tmp_path, monkeypatch):
    case = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0, recall_bin=str(bench_stub),
                                  cache_dir=str(tmp_path / "cache")))[0]
    _valid([case], "python")
    assert case["evidence"]["parity"]["reference"] == "rust"
    assert case["metrics"]["citation_parity"]["value"] == 0.0  # stub citations are fake
    assert _stub_calls(bench_stub) == [{"iterations": 1, "warmup": 0}]  # one cheap reference run


@POSIX_ONLY
def test_rust_suite_reference_citations_option(bench_stub, tmp_path, monkeypatch):
    keys = [mrc.query_key(q) for q in mrc.QUERIES]
    ref = {k: [{"n": 1, "source": "dictation", "title": "Slack", "app": "Slack", "when": "2026-11-01T09:00",
                "snippet": k, "ref": "1"}] for k in keys}
    case = _rust(bench_stub, tmp_path, iterations=1, reference_citations=ref)
    assert case["metrics"]["citation_parity"]["value"] == 1.0
    assert case["evidence"]["parity"]["reference"] == "options"
    ref[keys[0]] = []
    case = _rust(bench_stub, tmp_path, iterations=1, reference_citations=ref)
    assert case["metrics"]["citation_parity"]["value"] == pytest.approx(1 - 1 / len(keys))
    assert case["evidence"]["parity"]["mismatched"] == [keys[0]]


@POSIX_ONLY
@pytest.mark.parametrize("mode, needle", [("crash", "exited 7"), ("garbage", "malformed bench JSON")])
def test_rust_suite_broken_bench_fails(bench_stub, tmp_path, monkeypatch, mode, needle):
    monkeypatch.setenv("STUB_MODE", mode)
    case = _rust(bench_stub, tmp_path, parity="off")
    assert case["status"] == "failed" and needle in case["error"]


def test_rust_suite_missing_binary_not_exercised(tmp_path, monkeypatch):
    monkeypatch.setenv("LF_PERF_RECALL_BIN", str(tmp_path / "missing"))
    cases = recall.run_recall(_ctx("rust", tier="nightly"))
    _valid(cases, "rust")
    assert [c["case"] for c in cases] == ["rust/1k", "rust/10k"]
    assert all(c["status"] == "not_exercised" and "bench_search" in c["error"] for c in cases)


@POSIX_ONLY
def test_python_parity_reference_missing_rust_binary(tmp_path, monkeypatch):
    monkeypatch.setenv("LF_PERF_RECALL_BIN", str(tmp_path / "missing"))
    case = recall.run_recall(_ctx("python", sizes=[SMALL], iterations=1, warmup=0,
                                  cache_dir=str(tmp_path / "cache")))[0]
    assert case["status"] == "ok" and "citation_parity" not in case["metrics"]
    assert case["evidence"]["parity"]["status"] == "unavailable"


def test_cache_dir_from_env(tmp_path, monkeypatch):
    monkeypatch.setenv("LF_PERF_CACHE_DIR", str(tmp_path))
    case = recall.run_recall(_ctx("python", sizes=[10], iterations=1, warmup=0, parity="off"))[0]
    assert case["status"] == "ok", case["error"]
    assert (tmp_path / "recall" / f"n10-s{mrc.DEFAULT_SEED}" / "history.sqlite").is_file()


def test_run_recall_rejects_unknown_runtime():
    with pytest.raises(ValueError):
        recall.run_recall(_ctx("go"))
