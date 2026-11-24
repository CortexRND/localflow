"""``recall`` suite: local recall search speed, memory and cross-runtime citation parity.

Stdlib only; never imports ``localflow`` (``docs/plans/PLAN-perf-evals.md`` §5.4).
For every corpus size of the tier a deterministic corpus is generated (or
reused) by :mod:`evals.perf.fixtures.make_recall_corpus`, then benched in a
subprocess: Python ``benches/py_recall.py`` or Rust ``lf-ask`` ``bench_search
--queries`` (same flags, same JSON). One case per size, ``<runtime>/<size>``
(e.g. ``recall`` / ``python/1k``), reports:

* ``search_ms`` - every timed search over all queries and iterations;
* ``peak_rss_bytes`` - bench process peak RSS (omitted when unavailable);
* ``citation_parity`` - share of queries whose normalized citation lists
  equal the reference's (an exact gate: must be 1.0). The reference is
  ``options["reference_citations"]`` (a ``{query: [citation]}`` map, a bench
  report holding ``citations_by_query``, or a path to either), otherwise a
  one-iteration run of the *other* runtime's bench on the same corpus. With
  no reference available (or parity disabled) the metric is omitted and
  ``evidence.parity`` says why.

``evidence`` keeps ``citations_by_query`` plus a ``citations_sha256``
fingerprint so results can be gated against each other later.

``RunContext.options`` (all optional): ``sizes`` (list or comma string of
entry counts, ``1k``/``10k`` labels allowed), ``seed``, ``cache_dir``
(corpus cache; default ``$LF_PERF_CACHE_DIR/recall`` or a temp dir),
``recall_bin`` (Rust ``bench_search``; env ``LF_PERF_RECALL_BIN``), ``python``
(env ``LF_PERF_PYTHON``), ``iterations``, ``warmup``, ``now``, ``timeout_s``,
``reference_citations``, ``parity`` (``False``/``"off"`` disables the
cross-runtime reference run; env ``LF_PERF_RECALL_PARITY=0``).
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from ..fixtures.make_recall_corpus import DEFAULT_SEED, REFERENCE_NOW, ensure_corpus
from ..registry import RunContext, register_suite
from ..schema import make_case, make_metric

SUITE = "recall"
SMOKE_SIZES = (1000,)
NIGHTLY_SIZES = (1000, 10000)
TIER_SIZES = {"smoke": SMOKE_SIZES, "nightly": NIGHTLY_SIZES, "deep": NIGHTLY_SIZES}
DEFAULT_ITERATIONS = 5
DEFAULT_WARMUP = 1
DEFAULT_TIMEOUT_S = 1800
EXIT_UNAVAILABLE = 3  # py_recall.py: localflow could not be imported
ERROR_TAIL = 800
CITATION_FIELDS = ("source", "title", "app", "when", "snippet", "ref")
BUILD_HINT = "cargo build --release -p lf-ask --example bench_search"
MISSING_BIN_REASON = (
    f"Rust bench_search binary not found; build it with `{BUILD_HINT}` "
    "(in rust/) or set LF_PERF_RECALL_BIN"
)

PY_BENCH = Path(__file__).resolve().parents[1] / "benches" / "py_recall.py"


class BenchUnavailable(Exception):
    """The bench cannot run here (missing binary / dependency): ``not_exercised``."""


class BenchFailed(Exception):
    """The bench ran and failed (nonzero exit, timeout, bad JSON): ``failed``."""


# --------------------------------------------------------------------------
# Matrix
# --------------------------------------------------------------------------


def size_label(entries: int) -> str:
    return f"{entries // 1000}k" if entries >= 1000 and entries % 1000 == 0 else str(entries)


def _parse_size(value: object) -> int:
    text = str(value).strip().lower()
    n = int(text[:-1]) * 1000 if text.endswith("k") else int(text)
    if n <= 0:
        raise ValueError(f"recall size must be positive, got {value!r}")
    return n


def plan_sizes(tier: str, override: object = None) -> list[int]:
    """Corpus entry counts for ``tier`` (``override``: list or comma string)."""
    if tier not in TIER_SIZES:
        raise ValueError(f"unknown tier {tier!r}")
    if override is None or override == "" or override == []:
        return list(TIER_SIZES[tier])
    items = override.split(",") if isinstance(override, str) else list(override)  # type: ignore[arg-type]
    return list(dict.fromkeys(_parse_size(x) for x in items if str(x).strip()))


def case_name(runtime: str, entries: int) -> str:
    return f"{runtime}/{size_label(entries)}"


def _dims(runtime: str, entries: int) -> dict:
    return {"runtime": runtime, "size": size_label(entries), "entries": entries}


def _non_ok(runtime: str, entries: int, status: str, reason: str, evidence: dict | None = None) -> dict:
    return make_case(
        suite=SUITE, case=case_name(runtime, entries), dims=_dims(runtime, entries),
        status=status, error=reason, evidence=evidence,
    )


# --------------------------------------------------------------------------
# Citations
# --------------------------------------------------------------------------


def citation_fingerprint(citation: dict) -> tuple:
    """Order-sensitive identity of one citation (rank ``n`` is implied by list position)."""
    return tuple(str(citation.get(name) if citation.get(name) is not None else "") for name in CITATION_FIELDS)


def _fingerprints(citations: object) -> list[tuple] | None:
    if not isinstance(citations, list) or not all(isinstance(c, dict) for c in citations):
        return None
    return [citation_fingerprint(c) for c in citations]


def compare_citation_sets(candidate: dict, reference: dict) -> dict:
    """Compare two ``{query: [citation]}`` maps query by query.

    A query matches only when both lists have the same citations in the same
    order. Queries present on one side only count as ``missing``. Returns
    ``{"parity": matched / total, "matched", "total", "mismatched": [...],
    "missing": [...]}``; two empty maps have parity 1.0.
    """
    candidate = candidate if isinstance(candidate, dict) else {}
    reference = reference if isinstance(reference, dict) else {}
    keys = list(dict.fromkeys([*reference, *candidate]))
    matched, mismatched, missing = 0, [], []
    for key in keys:
        if key not in candidate or key not in reference:
            missing.append(key)
            continue
        a, b = _fingerprints(candidate[key]), _fingerprints(reference[key])
        if a is not None and a == b:
            matched += 1
        else:
            mismatched.append(key)
    total = len(keys)
    return {
        "parity": matched / total if total else 1.0,
        "matched": matched,
        "total": total,
        "mismatched": mismatched,
        "missing": missing,
    }


def citations_digest(citations_by_query: dict) -> str:
    """sha256 over the fingerprints of every query (key order independent)."""
    canonical = {k: [list(fp) for fp in _fingerprints(v) or []] for k, v in citations_by_query.items()}
    blob = json.dumps(canonical, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


def load_reference_citations(value: object) -> dict:
    """``{query: [citation]}`` from a map, a bench report, or a JSON file holding either."""
    if isinstance(value, (str, Path)):
        value = json.loads(Path(value).read_text(encoding="utf-8"))
    if isinstance(value, dict) and isinstance(value.get("citations_by_query"), dict):
        value = value["citations_by_query"]
    if not isinstance(value, dict):
        raise TypeError("reference_citations must be a {query: [citation]} object")
    return value


# --------------------------------------------------------------------------
# Bench discovery and invocation
# --------------------------------------------------------------------------


def find_recall_bench(repo_root: Path, override: str | None = None) -> Path | None:
    """``override`` / ``LF_PERF_RECALL_BIN``, else release then debug build."""
    explicit = override or os.environ.get("LF_PERF_RECALL_BIN")
    if explicit:
        path = Path(explicit)
        return path.resolve() if path.is_file() else None
    exe = "bench_search.exe" if sys.platform == "win32" else "bench_search"
    for profile in ("release", "debug"):
        path = repo_root / "rust" / "target" / profile / "examples" / exe
        if path.is_file():
            return path.resolve()
    return None


def bench_command(ctx: RunContext, runtime: str, corpus: dict, *, iterations: int, warmup: int,
                  now: str) -> list[str]:
    if runtime == "rust":
        binary = find_recall_bench(ctx.repo_root, ctx.options.get("recall_bin"))
        if binary is None:
            raise BenchUnavailable(MISSING_BIN_REASON)
        cmd = [str(binary), corpus["db_path"], corpus["vault_path"]]
    elif runtime == "python":
        python = ctx.options.get("python") or os.environ.get("LF_PERF_PYTHON") or sys.executable
        cmd = [str(python), str(PY_BENCH), "--db", corpus["db_path"], "--vault", corpus["vault_path"]]
    else:
        raise ValueError(f"unknown runtime {runtime!r}")
    return cmd + [
        "--queries", corpus["queries_path"],
        "--iterations", str(iterations),
        "--warmup", str(warmup),
        "--now", now,
    ]


def _bench_env(ctx: RunContext, runtime: str) -> dict:
    env = dict(os.environ)
    if runtime == "python":  # let the bench import localflow from this checkout
        root = str(ctx.repo_root)
        env["PYTHONPATH"] = os.pathsep.join(p for p in (root, env.get("PYTHONPATH")) if p)
    return env


def _tail(text: str | bytes | None) -> str:
    if isinstance(text, bytes):
        text = text.decode("utf-8", errors="replace")
    text = (text or "").strip()
    return text[-ERROR_TAIL:] if len(text) > ERROR_TAIL else text


def parse_report(stdout: str) -> dict:
    """Validate bench JSON (the whole stdout, or its last non-empty line)."""
    text = (stdout or "").strip()
    try:
        report = json.loads(text)
    except ValueError:
        lines = [line for line in text.splitlines() if line.strip()]
        if not lines:
            raise ValueError("bench printed no JSON") from None
        report = json.loads(lines[-1])
    if not isinstance(report, dict):
        raise TypeError("bench JSON must be an object")
    queries = report.get("queries")
    per_query = report.get("milliseconds")
    flat = report.get("all_milliseconds")
    citations = report.get("citations_by_query")
    if not isinstance(queries, list) or not queries or not all(isinstance(q, str) for q in queries):
        raise ValueError("bench JSON needs a non-empty 'queries' list")
    if not isinstance(per_query, dict) or not isinstance(citations, dict):
        raise TypeError("bench JSON needs 'milliseconds' and 'citations_by_query' objects")
    for key in queries:
        ms = per_query.get(key)
        if not isinstance(ms, list) or not ms:
            raise ValueError(f"bench JSON has no timings for query {key!r}")
        if _fingerprints(citations.get(key)) is None:
            raise ValueError(f"bench JSON has no citation list for query {key!r}")
    if not isinstance(flat, list) or not flat:
        raise ValueError("bench JSON needs a non-empty 'all_milliseconds' list")
    report["all_milliseconds"] = [float(x) for x in flat]
    if any(x < 0 for x in report["all_milliseconds"]):
        raise ValueError("negative search time")
    rss = report.get("peak_rss_bytes")
    if rss is not None and (isinstance(rss, bool) or not isinstance(rss, int) or rss < 0):
        raise ValueError(f"peak_rss_bytes must be a non-negative integer or null, got {rss!r}")
    return report


def run_bench(ctx: RunContext, runtime: str, corpus: dict, *, iterations: int, warmup: int,
              now: str) -> dict:
    """Run one bench subprocess; raise :class:`BenchUnavailable` / :class:`BenchFailed`."""
    cmd = bench_command(ctx, runtime, corpus, iterations=iterations, warmup=warmup, now=now)
    timeout = float(ctx.options.get("timeout_s", DEFAULT_TIMEOUT_S))
    try:
        proc = subprocess.run(
            cmd, cwd=str(ctx.repo_root), env=_bench_env(ctx, runtime), capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise BenchFailed(f"bench timed out after {timeout:g} s: {_tail(exc.stderr)}".strip()) from None
    except OSError as exc:
        raise BenchFailed(f"could not start bench: {exc}") from None
    if proc.returncode == EXIT_UNAVAILABLE and runtime == "python":
        raise BenchUnavailable(_tail(proc.stderr) or "localflow not importable")
    if proc.returncode != 0:
        raise BenchFailed(f"bench exited {proc.returncode}: {_tail(proc.stderr) or '(no stderr)'}")
    try:
        return parse_report(proc.stdout)
    except (TypeError, ValueError) as exc:
        raise BenchFailed(f"malformed bench JSON: {exc}") from None


# --------------------------------------------------------------------------
# Cases
# --------------------------------------------------------------------------


def _parity_enabled(opts: dict) -> bool:
    value = opts.get("parity", os.environ.get("LF_PERF_RECALL_PARITY", "1"))
    return str(value).strip().lower() not in ("0", "false", "off", "no", "")


def reference_for(ctx: RunContext, corpus: dict, now: str) -> tuple[dict | None, dict]:
    """(reference ``{query: [citation]}`` or ``None``, parity evidence)."""
    opts = ctx.options
    if opts.get("reference_citations") is not None:
        try:
            return load_reference_citations(opts["reference_citations"]), {"reference": "options"}
        except (OSError, TypeError, ValueError) as exc:
            return None, {"reference": "options", "status": "unavailable", "reason": f"bad reference: {exc}"}
    if not _parity_enabled(opts):
        return None, {"status": "disabled"}
    other = "rust" if ctx.runtime == "python" else "python"
    try:
        report = run_bench(ctx, other, corpus, iterations=1, warmup=0, now=now)
    except (BenchUnavailable, BenchFailed) as exc:
        return None, {"reference": other, "status": "unavailable", "reason": str(exc)}
    return report["citations_by_query"], {"reference": other}


def case_from_report(ctx: RunContext, entries: int, corpus: dict, report: dict,
                     reference: dict | None, parity_evidence: dict) -> dict:
    citations = {k: report["citations_by_query"][k] for k in report["queries"]}
    metrics = {
        "search_ms": make_metric(unit="ms", better="lower", samples=report["all_milliseconds"]),
    }
    if report.get("peak_rss_bytes") is not None:
        metrics["peak_rss_bytes"] = make_metric(unit="bytes", better="lower", value=int(report["peak_rss_bytes"]))
    parity = dict(parity_evidence)
    if reference is not None:
        summary = compare_citation_sets(citations, reference)
        metrics["citation_parity"] = make_metric(unit="ratio", better="higher", value=summary["parity"])
        parity.update(summary, status="compared", reference_sha256=citations_digest(reference))
    evidence = {
        "entries": entries,
        "seed": corpus.get("seed"),
        "corpus_digest": corpus.get("digest"),
        "now": report.get("now"),
        "iterations": report.get("iterations"),
        "warmup": report.get("warmup"),
        "queries": list(report["queries"]),
        "hits_by_query": {k: len(v) for k, v in citations.items()},
        "median_ms_by_query": {
            k: sorted(float(x) for x in report["milliseconds"][k])[(len(report["milliseconds"][k]) - 1) // 2]
            for k in report["queries"]
        },
        "citations_sha256": citations_digest(citations),
        "citations_by_query": citations,
        "parity": parity,
    }
    return make_case(
        suite=SUITE, case=case_name(ctx.runtime, entries), dims=_dims(ctx.runtime, entries),
        metrics=metrics, evidence=evidence,
    )


def run_size(ctx: RunContext, entries: int, cache_root: Path) -> dict:
    opts = ctx.options
    seed = int(opts.get("seed", DEFAULT_SEED))
    iterations = int(opts.get("iterations", DEFAULT_ITERATIONS))
    warmup = int(opts.get("warmup", DEFAULT_WARMUP))
    now = str(opts.get("now") or REFERENCE_NOW)
    if ctx.runtime == "rust" and find_recall_bench(ctx.repo_root, opts.get("recall_bin")) is None:
        return _non_ok(ctx.runtime, entries, "not_exercised", MISSING_BIN_REASON)
    try:
        corpus = ensure_corpus(cache_root / f"n{entries}-s{seed}", entries=entries, seed=seed)
    except Exception as exc:  # noqa: BLE001 - disk/sqlite: report, never crash
        return _non_ok(ctx.runtime, entries, "failed", f"could not generate corpus: {type(exc).__name__}: {exc}")
    try:
        report = run_bench(ctx, ctx.runtime, corpus, iterations=iterations, warmup=warmup, now=now)
    except BenchUnavailable as exc:
        return _non_ok(ctx.runtime, entries, "not_exercised", str(exc))
    except BenchFailed as exc:
        return _non_ok(ctx.runtime, entries, "failed", str(exc), {"corpus_digest": corpus.get("digest")})
    reference, parity_evidence = reference_for(ctx, corpus, now)
    return case_from_report(ctx, entries, corpus, report, reference, parity_evidence)


def _cache_root(opts: dict) -> Path | None:
    if opts.get("cache_dir"):
        return Path(opts["cache_dir"])
    root = os.environ.get("LF_PERF_CACHE_DIR")
    return Path(root) / "recall" if root else None


@register_suite(SUITE)
def run_recall(ctx: RunContext) -> list[dict]:
    if ctx.runtime not in ("python", "rust"):
        raise ValueError(f"unknown runtime {ctx.runtime!r}")
    sizes = plan_sizes(ctx.tier, ctx.options.get("sizes"))
    cache_root = _cache_root(ctx.options)
    if cache_root is not None:
        return [run_size(ctx, n, cache_root) for n in sizes]
    with tempfile.TemporaryDirectory(prefix="lf-perf-recall-") as tmp:
        return [run_size(ctx, n, Path(tmp)) for n in sizes]
