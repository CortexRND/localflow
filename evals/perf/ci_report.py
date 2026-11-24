"""Combine per-runtime smoke compare reports into one CI summary / PR comment.

Used by ``.github/workflows/perf-evals.yml`` (``docs/plans/PLAN-perf-evals.md``
§7). Stdlib only; never imports ``localflow``.

Each *entry* describes one runtime (``python`` or ``rust``)::

    {"runtime": "rust",
     "compare_json": Path | None,   # from `python -m evals.perf compare --json`
     "report_md": Path | None,      # from `python -m evals.perf compare --output`
     "candidate_json": Path | None, # the PR head lf-perf-result/v1
     "note": str | None}            # e.g. why there is no baseline

Status per runtime:

* compare JSON given and readable: its ``status`` (``FAIL`` also when
  ``blocking > 0``).
* compare JSON given but missing/unreadable: ``FAIL`` (compare crashed or
  received invalid input).
* no compare JSON (bootstrap: merge-base predates ``evals/perf`` or
  ``stt_bench --manifest``, or a non-PR run): candidate-only summary.
  ``PASS`` when every candidate case is ``ok``/``not_exercised``, ``WARN`` when
  some failed (no baseline to tell a regression from a pre-existing failure),
  ``FAIL`` when no candidate result exists at all.

Overall: ``FAIL`` (exit 1) if any runtime fails, else ``WARN`` if any warns,
else ``PASS`` (exit 0).

CLI::

    python -m evals.perf.ci_report --output perf-results/report.md \\
        --sha HEAD_SHA --base-sha BASE_SHA \\
        --entry runtime=python,compare=c.json,report=r.md,candidate=p.json \\
        --entry runtime=rust,candidate=r.json,note=baseline has no --manifest \\
        --status-json perf-results/status.json --no-fail

In ``--entry`` values, ``note=`` must come last: everything after it
(commas included) is the note. ``note_file=PATH`` reads the note from a file.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

STICKY_MARKER = "<!-- lf-perf-smoke-report -->"

#: GitHub rejects issue comments longer than 65,536 characters.
GITHUB_COMMENT_LIMIT = 65536
#: Our own ceiling, leaving generous headroom below GitHub's limit.
MAX_REPORT_CHARS = 60000
ARTIFACT_HINT = "the `perf-smoke-*` workflow artifact"

STATUS_ORDER = ("PASS", "WARN", "FAIL")
STATUS_ICON = {"PASS": "✅", "WARN": "⚠️", "FAIL": "❌"}
EXIT_OK = 0
EXIT_FAIL = 1
EXIT_USAGE = 2

_ENTRY_KEYS = {
    "runtime": "runtime",
    "compare": "compare_json",
    "compare_json": "compare_json",
    "report": "report_md",
    "report_md": "report_md",
    "candidate": "candidate_json",
    "candidate_json": "candidate_json",
    "note": "note",
    "note_file": "note_file",
}
_PATH_FIELDS = ("compare_json", "report_md", "candidate_json")


# --------------------------------------------------------------------------
# Inputs
# --------------------------------------------------------------------------


def _read_json(path: Path | None) -> tuple[Any, str | None]:
    """``(data, error)``; error is ``None`` on success."""
    if path is None:
        return None, "not provided"
    try:
        return json.loads(Path(path).read_text(encoding="utf-8")), None
    except FileNotFoundError:
        return None, f"`{Path(path).name}` not found"
    except (OSError, ValueError) as exc:
        return None, f"`{Path(path).name}` unreadable: {type(exc).__name__}: {exc}"


def _read_text(path: Path | None) -> str | None:
    if path is None:
        return None
    try:
        return Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def _worst(statuses: list[str]) -> str:
    return max(statuses, key=STATUS_ORDER.index) if statuses else "PASS"


def _status_from_compare(data: Any) -> tuple[str, dict]:
    if not isinstance(data, dict):
        return "FAIL", {}
    counts = data.get("counts") if isinstance(data.get("counts"), dict) else {}
    blocking = counts.get("blocking", data.get("blocking", 0))
    status = str(data.get("status", "")).upper()
    if status not in STATUS_ORDER:
        status = "FAIL"  # unknown shape: fail closed
    if data.get("blocked") or (isinstance(blocking, int) and blocking > 0):
        status = "FAIL"
    return status, counts


def _candidate_summary(data: Any) -> dict:
    """Case counts by status for a candidate ``lf-perf-result/v1``."""
    counts = {"ok": 0, "failed": 0, "not_exercised": 0}
    failed: list[str] = []
    cases = data.get("cases") if isinstance(data, dict) else None
    for case in cases if isinstance(cases, list) else []:
        if not isinstance(case, dict):
            continue
        status = str(case.get("status", "failed"))
        counts[status] = counts.get(status, 0) + 1
        if status == "failed":
            failed.append(f"{case.get('suite', '?')}/{case.get('case', '?')}: {case.get('error') or ''}".strip())
    run = data.get("run") if isinstance(data, dict) and isinstance(data.get("run"), dict) else {}
    return {"counts": counts, "failed": failed, "run": run, "valid": isinstance(cases, list)}


def evaluate_entry(entry: dict) -> dict:
    """Status and render inputs for one runtime entry."""
    runtime = str(entry.get("runtime") or "?")
    note = (entry.get("note") or "").strip() or None
    result: dict[str, Any] = {"runtime": runtime, "note": note, "mode": "compare", "counts": {}, "problems": []}
    compare_path = entry.get("compare_json")
    cand, cand_err = _read_json(entry.get("candidate_json"))
    if compare_path is not None:
        data, err = _read_json(compare_path)
        if err:
            result["status"] = "FAIL"
            result["problems"].append(f"compare report {err} (compare crashed or input was invalid)")
        else:
            result["status"], result["counts"] = _status_from_compare(data)
        result["report_md"] = _read_text(entry.get("report_md"))
        if cand is not None:
            result["candidate"] = _candidate_summary(cand)
        return result

    result["mode"] = "candidate-only"
    if cand_err:
        result["status"] = "FAIL"
        result["problems"].append(f"candidate result {cand_err} (the PR-head run produced no result)")
        return result
    summary = _candidate_summary(cand)
    result["candidate"] = summary
    if not summary["valid"]:
        result["status"] = "FAIL"
        result["problems"].append("candidate result has no `cases` list")
    elif summary["counts"].get("failed"):
        result["status"] = "WARN"
    else:
        result["status"] = "PASS"
    return result


# --------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def _short(sha: str) -> str:
    return sha[:12] if sha else "?"


def truncate(text: str, limit: int, *, hint: str = ARTIFACT_HINT) -> str:
    """Cut ``text`` to at most ``limit`` chars on a line boundary, with a note."""
    if len(text) <= limit:
        return text
    note = f"\n\n_… truncated ({len(text):,} chars total); see {hint} for the full report._\n"
    keep = max(0, limit - len(note))
    cut = text[:keep]
    if "\n" in cut:
        cut = cut[: cut.rfind("\n")]
    return cut + note


def _counts_line(counts: dict) -> str:
    keys = ("blocking", "warnings", "improved", "pass", "regressed", "flaky", "not_exercised", "low_power")
    parts = [f"{counts[k]} {k}" for k in keys if isinstance(counts.get(k), int) and counts.get(k)]
    return ", ".join(parts) if parts else "no rows"


def _render_section(ev: dict, budget: int) -> str:
    status = ev["status"]
    head = f"### {STATUS_ICON[status]} `{ev['runtime']}` — {status}"
    lines = [head, ""]
    if ev["mode"] == "compare":
        if ev["counts"]:
            lines.append(f"Same-runner A/B: {_counts_line(ev['counts'])}.")
    else:
        lines.append("**No baseline compare** — candidate-only summary.")
    if ev.get("note"):
        lines.append("")
        lines.append(f"> {ev['note']}".replace("\n", "\n> "))
    for problem in ev["problems"]:
        lines.append("")
        lines.append(f"- ❗ {problem}")
    cand = ev.get("candidate")
    if ev["mode"] == "candidate-only" and cand:
        c = cand["counts"]
        lines.append("")
        lines.append(
            f"Candidate cases: {c.get('ok', 0)} ok, {c.get('failed', 0)} failed, "
            f"{c.get('not_exercised', 0)} not_exercised."
        )
        if cand["failed"]:
            lines.append("")
            lines.append("Failed candidate cases (no baseline to tell regression from pre-existing):")
            lines.extend(f"- `{f[:300]}`" for f in cand["failed"][:20])
            if len(cand["failed"]) > 20:
                lines.append(f"- … and {len(cand['failed']) - 20} more")
    header = "\n".join(lines) + "\n"
    detail = ev.get("report_md")
    if not detail:
        return truncate(header, budget)
    open_tag = "\n<details><summary>Compare table</summary>\n\n"
    close_tag = "\n</details>\n"
    room = budget - len(header) - len(open_tag) - len(close_tag)
    if room < 200:
        return truncate(header, budget - 120) + f"\n_Compare table omitted for size; see {ARTIFACT_HINT}._\n"
    return header + open_tag + truncate(detail.rstrip("\n") + "\n", room) + close_tag


def render_report(evaluations: list[dict], overall: str, *, sha: str = "", base_sha: str = "",
                  limit: int = MAX_REPORT_CHARS) -> str:
    head = [
        STICKY_MARKER,
        f"## {STATUS_ICON[overall]} Perf smoke: **{overall}**",
        "",
        f"Head `{_short(sha)}` vs merge-base `{_short(base_sha)}` (same-runner A/B, ubuntu-latest).",
        "",
    ]
    if not evaluations:
        head.append("_No runtimes were evaluated._")
    header = "\n".join(head) + "\n"
    footer = (
        f"\n---\n<sub>Smoke tier: noisy metrics warn, exact/quality metrics and ≥2× slowdowns block "
        f"(`evals/perf/policy.toml`). Full results: {ARTIFACT_HINT}.</sub>\n"
    )
    room = limit - len(header) - len(footer)
    sections: list[str] = []
    per = room // max(1, len(evaluations))
    for ev in evaluations:
        sections.append(_render_section(ev, max(per, 0)))
    body = header + "\n".join(sections) + footer
    return truncate(body, limit)  # belt and braces


def combine_smoke_reports(entries: list[dict], *, sha: str = "", base_sha: str = "",
                          limit: int = MAX_REPORT_CHARS) -> tuple[str, int]:
    """Markdown report (starting with :data:`STICKY_MARKER`) and exit code."""
    markdown, _status, code, _evs = _combine(entries, sha=sha, base_sha=base_sha, limit=limit)
    return markdown, code


def _combine(entries: list[dict], *, sha: str, base_sha: str, limit: int) -> tuple[str, str, int, list[dict]]:
    evaluations = [evaluate_entry(e) for e in entries]
    overall = _worst([ev["status"] for ev in evaluations])
    code = EXIT_FAIL if overall == "FAIL" else EXIT_OK
    markdown = render_report(evaluations, overall, sha=sha, base_sha=base_sha, limit=limit)
    return markdown, overall, code, evaluations


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def parse_entry(spec: str) -> dict:
    """Parse ``runtime=rust,compare=PATH,...,note=free text, commas ok``."""
    entry: dict[str, Any] = {"runtime": None, "compare_json": None, "report_md": None,
                             "candidate_json": None, "note": None}
    rest = spec
    while rest:
        item, sep, tail = rest.partition(",")
        key, eq, value = item.partition("=")
        key = key.strip()
        if not eq or key not in _ENTRY_KEYS:
            raise ValueError(f"bad --entry item {item!r} (expected key=value; keys: {sorted(_ENTRY_KEYS)})")
        field = _ENTRY_KEYS[key]
        if field == "note":
            entry["note"] = (value + sep + tail).strip() or None
            break
        value = value.strip()
        if field == "note_file":
            if value:
                entry["note"] = (_read_text(Path(value)) or "").strip() or None
        elif field in _PATH_FIELDS:
            entry[field] = Path(value) if value else None
        else:
            entry[field] = value or None
        rest = tail
    if entry["runtime"] not in ("python", "rust"):
        raise ValueError(f"--entry needs runtime=python|rust, got {entry['runtime']!r}")
    return entry


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="python -m evals.perf.ci_report",
                                description="Combine perf smoke compare reports into one PR comment.")
    p.add_argument("--entry", action="append", default=[], required=True,
                   help="runtime=python|rust[,compare=PATH][,report=PATH][,candidate=PATH][,note_file=PATH]"
                        "[,note=TEXT] (repeatable; note= must be last)")
    p.add_argument("--output", type=Path, required=True, help="combined markdown report")
    p.add_argument("--sha", default="", help="PR head SHA")
    p.add_argument("--base-sha", default="", help="merge-base SHA")
    p.add_argument("--status-json", type=Path, help="write {status, exit_code, runtimes} here")
    p.add_argument("--no-fail", action="store_true",
                   help="always exit 0; read the real verdict from --status-json")
    p.add_argument("--max-chars", type=int, default=MAX_REPORT_CHARS, help=argparse.SUPPRESS)
    return p


def main(argv: list[str] | None = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        try:
            sys.stdout.reconfigure(encoding="utf-8")
        except (OSError, ValueError):
            pass
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return int(exc.code or 0)
    try:
        entries = [parse_entry(spec) for spec in args.entry]
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EXIT_USAGE
    limit = min(args.max_chars, GITHUB_COMMENT_LIMIT - 1)
    markdown, overall, code, evaluations = _combine(entries, sha=args.sha, base_sha=args.base_sha, limit=limit)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(markdown, encoding="utf-8")
    if args.status_json:
        status = {
            "status": overall,
            "exit_code": code,
            "sha": args.sha,
            "base_sha": args.base_sha,
            "runtimes": {
                ev["runtime"]: {"status": ev["status"], "mode": ev["mode"], "note": ev["note"],
                                "problems": ev["problems"]}
                for ev in evaluations
            },
        }
        args.status_json.parent.mkdir(parents=True, exist_ok=True)
        args.status_json.write_text(json.dumps(status, indent=2) + "\n", encoding="utf-8")
    print(markdown, end="")
    return EXIT_OK if args.no_fail else code


if __name__ == "__main__":
    sys.exit(main())
