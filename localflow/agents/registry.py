"""Agent provider registry: built-ins as dotted paths plus entry points.

Listing ids imports no provider module; classes resolve on first use.
"""

from __future__ import annotations

import importlib
import importlib.metadata
import logging

log = logging.getLogger("localflow.agents.registry")

GROUP = "localflow.agent_providers"
AGENT_BUILTINS = {
    "claude": "localflow.agents.providers.claude:ClaudeProvider",
    "devin": "localflow.agents.providers.devin:DevinProvider",
    "http": "localflow.agents.providers.http:HttpProvider",
    "mock": "localflow.agents.providers.mock:MockProvider",
}


def _entry_points():
    try:
        return list(importlib.metadata.entry_points(group=GROUP))
    except Exception:  # noqa: BLE001 - broken metadata never breaks a route
        return []


def list_agent_ids() -> list[str]:
    return sorted({*AGENT_BUILTINS, *(ep.name for ep in _entry_points())})


def _load_dotted(path: str):
    module, _, name = path.partition(":")
    return getattr(importlib.import_module(module), name)


def agent_provider_class(id: str):
    if id in AGENT_BUILTINS:
        return _load_dotted(AGENT_BUILTINS[id])
    for entry_point in _entry_points():
        if entry_point.name == id:
            return entry_point.load()
    raise KeyError(f"unknown agent provider {id!r}; valid ids: {', '.join(list_agent_ids())}")


def is_builtin(id: str) -> bool:
    return id in AGENT_BUILTINS
