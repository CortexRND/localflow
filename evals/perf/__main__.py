"""CLI: ``python -m evals.perf {run,compare,validate,trend,trend-append,regressions}``.

Exit codes:
  0  ok (compare: pass or warnings only; regressions: report written)
  1  compare found a blocking regression
  2  invalid input (unreadable/invalid result, index or policy file, bad arguments)
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from .compare import DEFAULT_POLICY_PATH, compare_results, load_policy, render_markdown
from .registry import NOT_IMPLEMENTED_REASON, PLANNED_SUITES, SUITES, RunContext
from .schema import (
    VALID_RUNTIMES,
    VALID_TIERS,
    detect_runner_info,
    load_result,
    make_case,
    make_result,
    make_run,
    validate_result,
    write_result,
)
from .suites import load_builtin_suites
from .trend import (
    DEFAULT_CONSECUTIVE,
    DEFAULT_LIMIT,
    append_run,
    detect_consecutive_regressions,
    render_trend,
)

load_builtin_suites()

EXIT_OK = 0
EXIT_BLOCKED = 1
EXIT_INVALID = 2

REPO_ROOT = Path(__file__).resolve().parents[2]
SUITE_CASE = "*"


def _err(msg: str) -> None:
    print(msg, file=sys.stderr)


def _load_valid(path: Path, label: str) -> dict:
    try:
        data = load_result(path)
    except (OSError, TypeError, ValueError) as exc:  # JSONDecodeError is a ValueError
        raise ValueError(f"{label}: cannot read {path}: {exc}") from exc
    errors = validate_result(data)
    if errors:
        raise ValueError(f"{label}: {path} is not a valid lf-perf-result/v1:\n  " + "\n  ".join(errors))
    return data


def cmd_compare(args: argparse.Namespace) -> int:
    try:
        baseline = _load_valid(args.baseline, "baseline")
        candidate = _load_valid(args.candidate, "candidate")
        policy = load_policy(args.policy)
    except (OSError, ValueError) as exc:
        _err(f"error: {exc}")
        return EXIT_INVALID
    report = compare_results(baseline, candidate, policy, strict=args.strict)
    markdown = render_markdown(report)
    print(markdown, end="")
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(markdown, encoding="utf-8")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report.to_dict(), indent=2) + "\n", encoding="utf-8")
    return report.exit_code


def cmd_validate(args: argparse.Namespace) -> int:
    try:
        data = _load_valid(args.path, "result")
    except ValueError as exc:
        _err(f"invalid: {exc}")
        return EXIT_INVALID
    print(f"ok: {args.path} ({len(data['cases'])} cases)")
    return EXIT_OK


def _suite_placeholder(suite: str, status: str, reason: str, runtime: str) -> dict:
    return make_case(
        suite=suite,
        case=SUITE_CASE,
        dims={"runtime": runtime},
        status=status,
        error=reason,
    )


def run_suites(names: list[str], ctx: RunContext) -> list[dict]:
    """Run each named suite; unknown or crashing suites become placeholder cases."""
    cases: list[dict] = []
    for name in names:
        fn = SUITES.get(name)
        if fn is None:
            cases.append(_suite_placeholder(name, "not_exercised", NOT_IMPLEMENTED_REASON, ctx.runtime))
            continue
        try:
            cases.extend(fn(ctx))
        except Exception as exc:  # noqa: BLE001 - report, never crash the run
            cases.append(
                _suite_placeholder(name, "failed", f"{type(exc).__name__}: {exc}", ctx.runtime)
            )
    return cases


def cmd_run(args: argparse.Namespace) -> int:
    if args.suite:
        names = list(dict.fromkeys(s.strip() for chunk in args.suite for s in chunk.split(",") if s.strip()))
    else:
        names = list(PLANNED_SUITES) + [s for s in SUITES if s not in PLANNED_SUITES]
    try:
        run = make_run(
            tier=args.tier,
            runtime=args.runtime,
            runner=detect_runner_info(args.runner_label),
        )
    except ValueError as exc:
        _err(f"error: {exc}")
        return EXIT_INVALID
    ctx = RunContext(tier=args.tier, runtime=args.runtime, repo_root=REPO_ROOT)
    result = make_result(run, run_suites(names, ctx))
    errors = validate_result(result)
    if errors:
        _err("error: suites produced an invalid result:\n  " + "\n  ".join(errors))
        return EXIT_INVALID
    if args.output:
        write_result(args.output, result)
        _err(f"wrote {args.output} ({len(result['cases'])} cases)")
    else:
        print(json.dumps(result, indent=2))
    return EXIT_OK


def _write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _append(store: Path, results: list[Path]) -> int:
    for path in results:
        try:
            info = append_run(store, path)
        except (OSError, TypeError, ValueError) as exc:
            _err(f"error: {path}: {exc}")
            return EXIT_INVALID
        _err(
            f"stored {path} -> {info['run_file']}"
            f"{' (replaced)' if info['replaced'] else ''}: "
            f"{info['appended']} index lines appended, {info['skipped']} already present"
        )
    return EXIT_OK


def cmd_trend_append(args: argparse.Namespace) -> int:
    return _append(args.store, args.result)


def cmd_trend(args: argparse.Namespace) -> int:
    if args.append:
        if not args.store:
            _err("error: --append needs --store")
            return EXIT_INVALID
        code = _append(args.store, args.append)
        if code != EXIT_OK:
            return code
    index = args.index or (args.store / "index.jsonl" if args.store else None)
    if index is None:
        _err("error: pass --index (or --store)")
        return EXIT_INVALID
    try:
        markdown = render_trend(index, limit=args.limit, tier=args.tier)
    except (OSError, TypeError, ValueError) as exc:
        _err(f"error: {exc}")
        return EXIT_INVALID
    if args.output:
        _write_text(args.output, markdown)
        _err(f"wrote {args.output}")
    else:
        print(markdown, end="")
    return EXIT_OK


def cmd_regressions(args: argparse.Namespace) -> int:
    try:
        report = detect_consecutive_regressions(
            args.index, args.policy, consecutive=args.consecutive, tier=args.tier or None
        )
    except (OSError, TypeError, ValueError) as exc:  # tomllib.TOMLDecodeError is a ValueError
        _err(f"error: {exc}")
        return EXIT_INVALID
    text = json.dumps(report, indent=2) + "\n"
    if args.output:
        _write_text(args.output, text)
        _err(
            f"wrote {args.output}: {len(report['active_regressions'])} active, "
            f"{len(report['recovered'])} recovered, {len(report['pending'])} pending"
        )
    else:
        print(text, end="")
    return EXIT_OK


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m evals.perf",
        description="LocalFlow performance evals (lf-perf-result/v1).",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_cmp = sub.add_parser("compare", help="compare a candidate result against a baseline")
    p_cmp.add_argument("--baseline", type=Path, required=True, help="baseline result.json")
    p_cmp.add_argument("--candidate", type=Path, required=True, help="candidate result.json")
    p_cmp.add_argument("--policy", type=Path, default=DEFAULT_POLICY_PATH, help="policy.toml")
    p_cmp.add_argument("--output", type=Path, help="also write the markdown report here")
    p_cmp.add_argument("--json", type=Path, help="write the machine-readable report here")
    p_cmp.add_argument(
        "--strict", action="store_true", help="noisy regressions block instead of warning"
    )
    p_cmp.set_defaults(func=cmd_compare)

    p_val = sub.add_parser("validate", help="validate a result file")
    p_val.add_argument("path", type=Path)
    p_val.set_defaults(func=cmd_validate)

    p_run = sub.add_parser("run", help="run registered suites and emit a result file")
    p_run.add_argument("--tier", choices=VALID_TIERS, required=True)
    p_run.add_argument("--runtime", choices=VALID_RUNTIMES, required=True)
    p_run.add_argument(
        "--suite",
        action="append",
        help="suite to run (repeatable or comma-separated); default: all planned suites",
    )
    p_run.add_argument("--output", type=Path, help="result path (default: print to stdout)")
    p_run.add_argument("--runner-label", help="runner label recorded in run.runner.label")
    p_run.set_defaults(func=cmd_run)

    p_tr = sub.add_parser("trend", help="render trend.md from a trend store index.jsonl")
    p_tr.add_argument("--index", type=Path, help="index.jsonl (default: <store>/index.jsonl)")
    p_tr.add_argument("--store", type=Path, help="trend store directory (needed with --append)")
    p_tr.add_argument(
        "--append", type=Path, action="append", default=[],
        help="result.json to append to --store before rendering (repeatable)",
    )
    p_tr.add_argument("--output", type=Path, help="write trend.md here (default: stdout)")
    p_tr.add_argument("--limit", type=int, default=DEFAULT_LIMIT, help="runs per series window")
    p_tr.add_argument("--tier", choices=VALID_TIERS, help="only this tier (default: all)")
    p_tr.set_defaults(func=cmd_trend)

    p_ta = sub.add_parser("trend-append", help="append result files to a trend store")
    p_ta.add_argument("--store", type=Path, required=True, help="trend store directory")
    p_ta.add_argument(
        "--result", type=Path, action="append", required=True, help="result.json (repeatable)"
    )
    p_ta.set_defaults(func=cmd_trend_append)

    p_rg = sub.add_parser("regressions", help="find regressions persisting N consecutive runs")
    p_rg.add_argument("--index", type=Path, required=True, help="index.jsonl")
    p_rg.add_argument("--policy", type=Path, default=DEFAULT_POLICY_PATH, help="policy.toml")
    p_rg.add_argument("--consecutive", type=int, default=DEFAULT_CONSECUTIVE)
    p_rg.add_argument(
        "--tier", default="nightly", help="only this tier (default: nightly; '' for all)"
    )
    p_rg.add_argument("--output", type=Path, help="write the JSON report here (default: stdout)")
    p_rg.set_defaults(func=cmd_regressions)
    return parser


def main(argv: list[str] | None = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        try:
            sys.stdout.reconfigure(
                encoding="utf-8",
                errors="surrogateescape" if sys.platform != "win32" else "replace",
            )
        except (OSError, ValueError):
            sys.stderr.write("warning: could not reconfigure stdout to utf-8\n")
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:  # argparse exits 2 on bad usage, 0 on --help
        return int(exc.code or 0)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
