"""Python recall bench (counterpart to Rust ``lf-ask`` ``bench_search --queries``).

For every query in ``--queries`` (a JSON list of strings / ``{"query", "since"}``
objects, or newline-delimited plain queries) runs ``--warmup`` discarded and
``--iterations`` timed calls of::

    localflow.recall.search(q, store=HistoryStore(DB), vault_path=VAULT,
                            notes_folder="Notes", logs_folder="Transcripts",
                            since=since, now=NOW)

and prints one JSON object to stdout, the same shape as ``bench_search``::

    {"runtime": "python", "now": .., "iterations": N, "warmup": W,
     "queries": [key, ...],                       # in file order
     "milliseconds": {key: [ms, ...]},            # timed iterations per query
     "all_milliseconds": [ms, ...],               # flattened, query order
     "citations_by_query": {key: [citation, ...]},
     "peak_rss_bytes": int | null}

Citations are normalized like the Rust bench: ``ref`` paths inside the vault
become vault-relative with ``/`` separators so both runtimes (and machines)
produce comparable fingerprints. Each search rebuilds the corpus, exactly as
the app does (the index is not cached between calls).

Exit codes: 0 ok, 2 bad arguments / unreadable queries, 3 ``localflow`` could
not be imported.

Benches may import ``localflow``; this file itself only needs the stdlib.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from datetime import datetime
from pathlib import Path

EXIT_BAD_INPUT = 2
EXIT_UNAVAILABLE = 3
DEFAULT_NOW = "2026-11-14T10:00:00"
DEFAULT_ITERATIONS = 5
DEFAULT_WARMUP = 1
NOTES_FOLDER = "Notes"
LOGS_FOLDER = "Transcripts"
CITATION_FIELDS = ("n", "source", "title", "app", "when", "snippet", "ref")


# --------------------------------------------------------------------------
# Queries and citations (mirrors bench_search.rs)
# --------------------------------------------------------------------------


def query_key(query: str, since: str | None) -> str:
    return f"{query} (since {since})" if since else query


def load_queries(path: Path) -> list[tuple[str, str | None]]:
    """``[(query, since)]`` from a JSON list or a newline-delimited file."""
    text = Path(path).read_text(encoding="utf-8")
    stripped = text.lstrip("﻿").strip()
    if stripped.startswith("["):
        data = json.loads(stripped)
        out: list[tuple[str, str | None]] = []
        for item in data:
            if isinstance(item, str):
                out.append((item, None))
            elif isinstance(item, dict) and isinstance(item.get("query"), str):
                since = item.get("since")
                if since is not None and not isinstance(since, str):
                    raise ValueError(f"query {item['query']!r}: since must be a string")
                out.append((item["query"], since or None))
            else:
                raise TypeError(f"query entries must be strings or {{\"query\": str}} objects, got {item!r}")
    else:
        out = [(line.strip(), None) for line in stripped.splitlines() if line.strip()]
    if not out:
        raise ValueError(f"{path}: no queries")
    keys = [query_key(q, s) for q, s in out]
    if len(set(keys)) != len(keys):
        raise ValueError(f"{path}: duplicate queries")
    return out


def normalize_ref(ref: str, vault: str) -> str:
    prefix = vault.rstrip("/\\")
    if prefix and ref.startswith(prefix) and len(ref) > len(prefix) and ref[len(prefix)] in "/\\":
        return ref[len(prefix) + 1:].replace("\\", "/")
    return ref


def normalize_citation(citation: dict, vault: str) -> dict:
    out = {name: citation.get(name) for name in CITATION_FIELDS}
    out["ref"] = normalize_ref(str(out["ref"] or ""), vault)
    return out


# --------------------------------------------------------------------------
# Peak RSS (best effort)
# --------------------------------------------------------------------------


def _peak_rss_windows() -> int | None:  # pragma: no cover - Windows only
    import ctypes
    from ctypes import wintypes

    class _Counters(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    counters = _Counters()
    counters.cb = ctypes.sizeof(_Counters)
    kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
    psapi = ctypes.windll.psapi  # type: ignore[attr-defined]
    kernel32.GetCurrentProcess.argtypes = []
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(_Counters), wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    handle = kernel32.GetCurrentProcess()
    if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
        return int(counters.PeakWorkingSetSize)
    return None


def peak_rss_bytes() -> int | None:
    """Process peak resident set size in bytes, or ``None`` if unavailable."""
    try:
        with open("/proc/self/status", encoding="ascii", errors="replace") as fh:
            for line in fh:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass
    if sys.platform == "win32":  # pragma: no cover
        try:
            return _peak_rss_windows()
        except (OSError, AttributeError):
            return None
    try:
        import resource
    except ImportError:  # pragma: no cover
        return None
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if peak <= 0:
        return None
    return int(peak if sys.platform == "darwin" else peak * 1024)


# --------------------------------------------------------------------------
# Bench
# --------------------------------------------------------------------------


def run_bench(
    db: Path, vault: Path, queries: list[tuple[str, str | None]], *,
    iterations: int, warmup: int, now: datetime,
) -> dict:
    from localflow.history import HistoryStore
    from localflow.recall import search

    store = HistoryStore(Path(db))
    vault_str = str(vault)
    keys: list[str] = []
    milliseconds: dict[str, list[float]] = {}
    citations_by_query: dict[str, list[dict]] = {}
    for query, since in queries:
        key = query_key(query, since)
        keys.append(key)

        def once(query: str = query, since: str | None = since):
            return search(
                query, store=store, vault_path=vault_str, notes_folder=NOTES_FOLDER,
                logs_folder=LOGS_FOLDER, since=since, now=now,
            )

        result = None
        for _ in range(warmup):
            result = once()
        timed: list[float] = []
        for _ in range(iterations):
            start = time.perf_counter()
            result = once()
            timed.append((time.perf_counter() - start) * 1000.0)
        milliseconds[key] = timed
        citations_by_query[key] = [normalize_citation(c, vault_str) for c in (result.citations if result else [])]
    return {
        "runtime": "python",
        "now": now.isoformat(timespec="seconds"),
        "iterations": iterations,
        "warmup": warmup,
        "queries": keys,
        "milliseconds": milliseconds,
        "all_milliseconds": [ms for key in keys for ms in milliseconds[key]],
        "citations_by_query": citations_by_query,
        "peak_rss_bytes": peak_rss_bytes(),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--vault", type=Path, required=True)
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=DEFAULT_ITERATIONS)
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    parser.add_argument("--now", default=DEFAULT_NOW)
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return EXIT_BAD_INPUT if exc.code else 0
    if args.iterations <= 0 or args.warmup < 0:
        print("error: --iterations must be positive and --warmup non-negative", file=sys.stderr)
        return EXIT_BAD_INPUT
    try:
        now = datetime.fromisoformat(args.now)
        queries = load_queries(args.queries)
    except (OSError, TypeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EXIT_BAD_INPUT
    if not args.db.is_file():
        print(f"error: database not found: {args.db}", file=sys.stderr)
        return EXIT_BAD_INPUT
    try:
        import localflow.recall  # noqa: F401
    except ImportError as exc:
        print(f"localflow unavailable: {exc}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    report = run_bench(args.db, args.vault, queries, iterations=args.iterations, warmup=args.warmup, now=now)
    sys.stdout.write(json.dumps(report, ensure_ascii=False) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
