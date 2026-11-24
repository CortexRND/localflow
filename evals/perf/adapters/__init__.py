"""Adapters: existing CI helper outputs → ``lf-perf-result/v1`` footprint cases.

The CI helpers (``scripts/ci_*.py`` / ``scripts/ci_*.sh``) stay unchanged;
these modules only read the files they already write
(``docs/plans/PLAN-perf-evals.md`` §5.5). Stdlib only; never imports
``localflow``.

Each adapter has a pure ``convert_*`` function over parsed content and a
``load_*`` / ``measure_*`` function over a path. Unreadable or malformed
input becomes ``failed`` cases with a reason, never an exception.
"""

from __future__ import annotations

from ._common import SUITE
from .hub_pages import convert_hub_pages, load_hub_pages
from .hub_rss import convert_hub_rss, load_hub_rss, parse_hub_rss
from .package_size import measure_package, measure_packages
from .parakeet_memory import convert_parakeet_memory, load_parakeet_memory
from .speech_helper import (
    convert_speech_helper,
    helper_pids_cleaned,
    load_speech_helper,
)

__all__ = [
    "SUITE",
    "convert_hub_pages",
    "convert_hub_rss",
    "convert_parakeet_memory",
    "convert_speech_helper",
    "helper_pids_cleaned",
    "load_hub_pages",
    "load_hub_rss",
    "load_parakeet_memory",
    "load_speech_helper",
    "measure_package",
    "measure_packages",
    "parse_hub_rss",
]
