"""Built-in suites. Importing a suite module registers it (``@register_suite``)."""

from __future__ import annotations

import importlib

BUILTIN_SUITES = ("stt", "dictation", "footprint", "recall", "meetings")


def load_builtin_suites() -> None:
    """Import every built-in suite module so it registers itself (idempotent)."""
    for name in BUILTIN_SUITES:
        importlib.import_module(f"{__name__}.{name}")
