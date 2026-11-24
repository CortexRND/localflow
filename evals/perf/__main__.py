"""CLI: ``python -m evals.perf {run,compare,validate}``.

Exit codes:
  0  ok (compare: pass or warnings only)
  1  compare found a blocking regression
  2  invalid input (unreadable/invalid result or policy file, bad arguments)
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
    return parser


def main(argv: list[str] | None = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        try:
            sys.stdout.reconfigure(encoding="utf-8")
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
