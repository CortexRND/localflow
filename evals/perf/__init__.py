"""LocalFlow performance evals (``lf-perf-result/v1``).

Stdlib only; see ``docs/plans/PLAN-perf-evals.md``. Entry point:
``python -m evals.perf {run,compare,validate}``.
"""

from .compare import (
    DEFAULT_POLICY_PATH,
    CompareReport,
    CompareRow,
    compare_results,
    load_policy,
    render_markdown,
)
from .registry import PLANNED_SUITES, SUITES, RunContext, register_suite
from .schema import (
    SCHEMA_VERSION,
    VALID_ARCH,
    VALID_BETTER,
    VALID_OS,
    VALID_RUNTIMES,
    VALID_STATUSES,
    VALID_TIERS,
    compute_mad,
    compute_median,
    compute_p95,
    detect_git_info,
    detect_runner_info,
    load_result,
    make_case,
    make_metric,
    make_result,
    make_run,
    validate_result,
    write_result,
)

__all__ = [
    "DEFAULT_POLICY_PATH",
    "PLANNED_SUITES",
    "SCHEMA_VERSION",
    "SUITES",
    "VALID_ARCH",
    "VALID_BETTER",
    "VALID_OS",
    "VALID_RUNTIMES",
    "VALID_STATUSES",
    "VALID_TIERS",
    "CompareReport",
    "CompareRow",
    "RunContext",
    "compare_results",
    "compute_mad",
    "compute_median",
    "compute_p95",
    "detect_git_info",
    "detect_runner_info",
    "load_policy",
    "load_result",
    "make_case",
    "make_metric",
    "make_result",
    "make_run",
    "register_suite",
    "render_markdown",
    "validate_result",
    "write_result",
]
