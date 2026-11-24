"""Adapter: ``scripts/ci_speech_helper.py`` ``result.json`` → footprint cases.

The script (unchanged) runs the real ``lf-hub`` with
``LF_HUB_SPEECH_PROFILE`` and writes one ``result.json`` per mode
(``<output>/helper``, ``<output>/inprocess`` or ``<output>/whisper``),
produced by ``rust/lf-ui/src/speech_profile.rs``::

    {"rows": [{"stage": "idle", "main": {"rss": B, "footprint": B},
               "helper_pid": 123 | null, "helper": {...} | null,
               "total_rss": B, "total_footprint": B}, ...],
     "transcript": "...", "cold_load_ms": .., "cold_inference_ms": ..,
     "warm_inference_ms": .., "reload_ms": .., "reload_inference_ms": ..}

Cases (``suite="footprint"``):

* ``speech_helper/<mode>/<stage>``: ``main_rss_bytes``,
  ``main_footprint_bytes``, ``helper_rss_bytes`` / ``helper_footprint_bytes``
  (only while a helper runs), ``total_rss_bytes``, ``total_footprint_bytes``.
* ``speech_helper/<mode>/timings``: ``cold_ms`` (spawn + load),
  ``cold_inference_ms``, ``warm_ms`` (warm inference), ``reload_ms``,
  ``reload_inference_ms``.
* ``speech_helper/<mode>/lifecycle``: exact gate ``helper_pid_cleaned``
  (``unit="bool"``, ``better="exact"``): no helper process outside the
  loaded stages (always none in-process), and, for helper modes, a helper in
  every loaded stage; plus ``helper_respawned`` (reload spawned a new PID).

Stdlib only.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from ..schema import make_case, make_metric
from ._common import SUITE, bytes_metric, failed, ms_metric, read_json

STAGES = ("idle", "loaded", "transcribed", "unloaded", "reloaded", "dictation_off")
#: Stages in which a separate helper process is expected (not in-process).
HELPER_STAGES = frozenset({"loaded", "transcribed", "reloaded"})
MODES = ("helper", "inprocess", "whisper")
DEFAULT_MODE = "helper"

#: result.json key → metric name (``policy.toml`` has cold_ms/warm_ms/reload_ms).
TIMINGS = (
    ("cold_load_ms", "cold_ms"),
    ("cold_inference_ms", "cold_inference_ms"),
    ("warm_inference_ms", "warm_ms"),
    ("reload_ms", "reload_ms"),
    ("reload_inference_ms", "reload_inference_ms"),
)


def _dims(mode: str, stage: str) -> dict:
    return {"component": "speech_helper", "mode": mode, "stage": stage}


def _case(mode: str, stage: str) -> str:
    return f"speech_helper/{mode}/{stage}"


def _stage_case(mode: str, row: dict) -> dict:
    stage = row["stage"]
    main = row.get("main")
    if not isinstance(main, dict):
        raise TypeError("row has no main process metrics")
    metrics = {
        "main_rss_bytes": bytes_metric(main.get("rss")),
        "main_footprint_bytes": bytes_metric(main.get("footprint")),
    }
    helper = row.get("helper")
    if isinstance(helper, dict):
        metrics["helper_rss_bytes"] = bytes_metric(helper.get("rss"))
        metrics["helper_footprint_bytes"] = bytes_metric(helper.get("footprint"))
    metrics["total_rss_bytes"] = bytes_metric(row.get("total_rss"))
    metrics["total_footprint_bytes"] = bytes_metric(row.get("total_footprint"))
    return make_case(
        suite=SUITE,
        case=_case(mode, stage),
        dims=_dims(mode, stage),
        metrics=metrics,
        evidence={"helper_running": bool(row.get("helper_pid"))},
    )


def _timings_case(mode: str, data: dict) -> dict:
    metrics = {}
    missing = []
    for key, name in TIMINGS:
        if key in data:
            metrics[name] = ms_metric(data[key])
        else:
            missing.append(key)
    if not metrics:
        return failed(_case(mode, "timings"), _dims(mode, "timings"), "result.json has no timings")
    evidence = {"missing": missing} if missing else {}
    return make_case(
        suite=SUITE, case=_case(mode, "timings"), dims=_dims(mode, "timings"), metrics=metrics, evidence=evidence
    )


def helper_pids_cleaned(rows_by_stage: dict[str, dict], mode: str) -> tuple[bool, list[str]]:
    """``(ok, problems)`` for the helper PID lifecycle across all stages."""
    problems: list[str] = []
    for stage in STAGES:
        row = rows_by_stage.get(stage)
        if row is None:
            problems.append(f"{stage}: stage missing")
            continue
        running = bool(row.get("helper_pid"))
        expected = mode != "inprocess" and stage in HELPER_STAGES
        if running and not expected:
            problems.append(f"{stage}: helper process still running")
        elif expected and not running:
            problems.append(f"{stage}: no helper process")
    return (not problems, problems)


def _lifecycle_case(mode: str, rows_by_stage: dict[str, dict], transcript: Any) -> dict:
    ok, problems = helper_pids_cleaned(rows_by_stage, mode)
    metrics = {"helper_pid_cleaned": make_metric(unit="bool", better="exact", value=ok)}
    if mode != "inprocess":
        loaded = (rows_by_stage.get("loaded") or {}).get("helper_pid")
        reloaded = (rows_by_stage.get("reloaded") or {}).get("helper_pid")
        respawned = bool(loaded) and bool(reloaded) and loaded != reloaded
        metrics["helper_respawned"] = make_metric(unit="bool", better="exact", value=respawned)
    evidence: dict[str, Any] = {"problems": problems}
    if isinstance(transcript, str):
        evidence["transcript"] = transcript  # fixed CI fixture clip, never user audio
    return make_case(
        suite=SUITE,
        case=_case(mode, "lifecycle"),
        dims=_dims(mode, "lifecycle"),
        metrics=metrics,
        evidence=evidence,
    )


def convert_speech_helper(data: Any, *, mode: str = DEFAULT_MODE) -> list[dict]:
    """Convert one parsed ``result.json`` into footprint cases."""
    if not isinstance(data, dict) or not isinstance(data.get("rows"), list):
        return [failed(_case(mode, "*"), _dims(mode, "*"), "speech helper result.json has no rows list")]
    rows_by_stage: dict[str, dict] = {}
    cases: list[dict] = []
    for row in data["rows"]:
        if not isinstance(row, dict) or not isinstance(row.get("stage"), str) or not row["stage"]:
            continue
        stage = row["stage"]
        if stage in rows_by_stage:
            continue  # first sample of a stage wins; duplicates would break case uniqueness
        rows_by_stage[stage] = row
        try:
            cases.append(_stage_case(mode, row))
        except (KeyError, TypeError, ValueError) as exc:
            cases.append(failed(_case(mode, stage), _dims(mode, stage), f"malformed stage row: {exc}"))
    for stage in STAGES:
        if stage not in rows_by_stage:
            cases.append(failed(_case(mode, stage), _dims(mode, stage), "stage missing from result.json"))
    try:
        cases.append(_timings_case(mode, data))
    except ValueError as exc:
        cases.append(failed(_case(mode, "timings"), _dims(mode, "timings"), f"malformed timings: {exc}"))
    cases.append(_lifecycle_case(mode, rows_by_stage, data.get("transcript")))
    return cases


def _mode_from_path(path: Path) -> str:
    parent = path.parent.name
    return parent if parent in MODES else DEFAULT_MODE


def load_speech_helper(path: Path | str, *, mode: str | None = None) -> list[dict]:
    """Cases from a ``result.json`` or a ``ci_speech_helper.py`` output dir.

    For a directory, every ``<mode>/result.json`` (helper, inprocess,
    whisper) found is converted, plus a top-level ``result.json``. The mode
    comes from ``mode``, else the parent directory name, else ``helper``.
    """
    path = Path(path)
    if path.is_dir():
        files = [path / m / "result.json" for m in MODES if (path / m / "result.json").is_file()]
        if (path / "result.json").is_file():
            files.insert(0, path / "result.json")
        if not files:
            m = mode or DEFAULT_MODE
            return [failed(_case(m, "*"), _dims(m, "*"), f"no result.json under {path}")]
        cases: list[dict] = []
        for file in files:
            cases += load_speech_helper(file, mode=None if file.parent != path else mode)
        return cases
    resolved = mode or _mode_from_path(path)
    try:
        data = read_json(path)
    except (OSError, ValueError) as exc:
        return [failed(_case(resolved, "*"), _dims(resolved, "*"), f"cannot read {path.name}: {exc}")]
    return convert_speech_helper(data, mode=resolved)
