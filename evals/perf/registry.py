"""Suite registry.

Later PRs register suites here (``stt``, ``footprint``, ``dictation``,
``recall``, ``meetings``)::

    from evals.perf.registry import RunContext, register_suite

    @register_suite("stt")
    def run_stt(ctx: RunContext) -> list[dict]:
        ...  # return cases built with evals.perf.schema.make_case

A suite returns a list of case dicts. It reports anything it could not run
as ``not_exercised`` with a reason instead of leaving it out.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

#: Suites from the design spec, in report order. Any of these that is not
#: registered yet shows up as ``not_exercised`` rather than silently absent.
PLANNED_SUITES = ("stt", "dictation", "meetings", "recall", "footprint")

NOT_IMPLEMENTED_REASON = "suite not implemented yet"


@dataclass(frozen=True)
class RunContext:
    tier: str
    runtime: str
    repo_root: Path
    options: dict = field(default_factory=dict)


SuiteFn = Callable[[RunContext], list[dict]]

SUITES: dict[str, SuiteFn] = {}


def register_suite(name: str) -> Callable[[SuiteFn], SuiteFn]:
    """Decorator registering ``fn`` as suite ``name`` (names must be unique)."""

    def decorator(fn: SuiteFn) -> SuiteFn:
        if name in SUITES and SUITES[name] is not fn:
            raise ValueError(f"suite {name!r} is already registered")
        SUITES[name] = fn
        return fn

    return decorator


def get_suite(name: str) -> SuiteFn | None:
    return SUITES.get(name)
