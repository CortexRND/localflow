"""Shared helpers for the footprint adapters (stdlib only)."""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any

from ..schema import make_case, make_metric

SUITE = "footprint"
KIB = 1024


def is_count(value: Any) -> bool:
    """True for a finite, non-negative int/float that is not a bool."""
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(value)
        and value >= 0
    )


def bytes_metric(value: Any, *, better: str = "lower") -> dict:
    if not is_count(value):
        raise ValueError(f"expected a non-negative byte count, got {value!r}")
    return make_metric(unit="bytes", better=better, value=int(value))


def kib_metric(value: Any) -> dict:
    """KiB (as ``ps``/``pages.csv`` report it) converted to a bytes metric."""
    if not is_count(value):
        raise ValueError(f"expected a non-negative KiB count, got {value!r}")
    return make_metric(unit="bytes", better="lower", value=int(value) * KIB)


def ms_metric(value: Any) -> dict:
    if not is_count(value):
        raise ValueError(f"expected a non-negative millisecond value, got {value!r}")
    return make_metric(unit="ms", better="lower", value=float(value))


def failed(case: str, dims: dict, reason: str, *, status: str = "failed") -> dict:
    return make_case(suite=SUITE, case=case, dims=dims, status=status, error=reason)


def read_json(path: Path) -> Any:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def read_text(path: Path) -> str:
    # utf-8-sig: tolerate a BOM written by Windows tooling.
    return Path(path).read_text(encoding="utf-8-sig", errors="replace")
