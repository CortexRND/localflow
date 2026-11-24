"""Adapter: ``scripts/ci_parakeet_memory.py`` ``results.json`` → footprint cases.

The script (unchanged) runs ``parakeet_memory`` once per ORT configuration
and writes a JSON list; each entry is either a measurement (fields from
``rust/lf-stt/examples/parakeet_memory.rs`` plus ``name``, ``options``,
``clip_sha256`` and ``exact_match``) or ``{"name", "options", "error"}``.

One case per configuration: ``parakeet_memory/<name>``. Error entries become
``status="failed"`` with the script's error text. Footprint fields are
``null`` without ``LF_MEMORY_PROBE`` and are then omitted.

Stdlib only.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from ..schema import make_case, make_metric
from ._common import SUITE, bytes_metric, failed, is_count, ms_metric, read_json

RSS_FIELDS = (
    "rss_before",
    "rss_after_load",
    "rss_after_transcribe",
    "rss_after_unload",
    "rss_after_drop",
    "rss_after_relief",
)
FOOTPRINT_FIELDS = (
    "footprint_before",
    "footprint_load",
    "footprint_transcribe",
    "footprint_unload",
    "footprint_drop",
    "footprint_relief",
)
MS_FIELDS = ("load_ms", "latency_ms")
ERROR_TAIL = 800


def _case(name: str) -> str:
    return f"parakeet_memory/{name}"


def _dims(entry: dict, name: str) -> dict:
    dims: dict[str, Any] = {"component": "parakeet_memory", "config": name}
    if isinstance(entry.get("provider"), str):
        dims["provider"] = entry["provider"]
    if is_count(entry.get("threads")):
        dims["threads"] = int(entry["threads"])
    return dims


def _entry_case(entry: dict, name: str) -> dict:
    metrics: dict[str, dict] = {}
    for key in RSS_FIELDS:
        metrics[key] = bytes_metric(entry[key])
    for key in FOOTPRINT_FIELDS:
        if entry.get(key) is not None:
            metrics[key] = bytes_metric(entry[key])
    if entry.get("pressure_relief_bytes") is not None:
        # Bytes the allocator handed back under pressure: more is better.
        metrics["pressure_relief_bytes"] = bytes_metric(entry["pressure_relief_bytes"], better="higher")
    for key in MS_FIELDS:
        metrics[key] = ms_metric(entry[key])
    if isinstance(entry.get("exact_match"), bool):
        metrics["exact_match"] = make_metric(unit="bool", better="exact", value=entry["exact_match"])
    evidence: dict[str, Any] = {}
    for key in ("transcript", "clip", "clip_sha256", "options", "repo"):
        if entry.get(key) is not None:
            evidence[key] = entry[key]
    return make_case(suite=SUITE, case=_case(name), dims=_dims(entry, name), metrics=metrics, evidence=evidence)


def convert_parakeet_memory(data: Any) -> list[dict]:
    """Convert the parsed ``results.json`` list into one case per configuration."""
    if not isinstance(data, list):
        return [failed(_case("*"), {"component": "parakeet_memory"}, "results.json must be a JSON list")]
    cases: list[dict] = []
    seen: set[str] = set()
    for idx, entry in enumerate(data):
        if not isinstance(entry, dict):
            continue
        name = entry.get("name") if isinstance(entry.get("name"), str) and entry.get("name") else f"entry-{idx}"
        if name in seen:
            name = f"{name}#{idx}"
        seen.add(name)
        if entry.get("error") is not None:
            reason = str(entry["error"]).strip() or "configuration failed"
            reason = reason[-ERROR_TAIL:]
            case = failed(_case(name), _dims(entry, name), reason)
            if entry.get("options"):
                case["evidence"] = {"options": entry["options"]}
            cases.append(case)
            continue
        try:
            cases.append(_entry_case(entry, name))
        except (KeyError, TypeError, ValueError) as exc:
            cases.append(failed(_case(name), _dims(entry, name), f"malformed entry: {type(exc).__name__}: {exc}"))
    if not cases:
        return [failed(_case("*"), {"component": "parakeet_memory"}, "results.json holds no configurations")]
    return cases


def load_parakeet_memory(path: Path | str) -> list[dict]:
    """Cases from ``results.json`` (or a directory containing it)."""
    path = Path(path)
    if path.is_dir():
        path = path / "results.json"
    try:
        data = read_json(path)
    except (OSError, ValueError) as exc:
        return [failed(_case("*"), {"component": "parakeet_memory"}, f"cannot read {path.name}: {exc}")]
    return convert_parakeet_memory(data)
