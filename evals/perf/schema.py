"""``lf-perf-result/v1``: the one result format every perf suite emits.

Stdlib only. Never import ``localflow`` here: the harness must be usable
against either runtime without installing the Python product.
"""

from __future__ import annotations

import datetime as _dt
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import uuid
from pathlib import Path
from typing import Any

SCHEMA_VERSION = "lf-perf-result/v1"

VALID_TIERS = ("smoke", "nightly", "deep")
VALID_RUNTIMES = ("python", "rust")
VALID_OS = ("linux", "macos", "windows")
VALID_ARCH = ("x86_64", "arm64")
VALID_STATUSES = ("ok", "failed", "not_exercised")
VALID_BETTER = ("lower", "higher", "exact")

_OS_ALIASES = {
    "linux": "linux",
    "linux2": "linux",
    "darwin": "macos",
    "macos": "macos",
    "macosx": "macos",
    "osx": "macos",
    "win32": "windows",
    "windows": "windows",
    "cygwin": "windows",
}

_ARCH_ALIASES = {
    "x86_64": "x86_64",
    "amd64": "x86_64",
    "x64": "x86_64",
    "aarch64": "arm64",
    "arm64": "arm64",
    "armv8": "arm64",
}

_RUN_STR_FIELDS = ("id", "sha", "ref", "started_at", "harness")


# --------------------------------------------------------------------------
# Stats helpers
# --------------------------------------------------------------------------


def _as_floats(samples: list[float]) -> list[float]:
    if not samples:
        raise ValueError("samples must be a non-empty list")
    return [float(x) for x in samples]


def compute_median(samples: list[float]) -> float:
    """Median of ``samples`` (raises ``ValueError`` when empty)."""
    return float(statistics.median(_as_floats(samples)))


def compute_p95(samples: list[float]) -> float:
    """95th percentile with linear interpolation between closest ranks.

    Same definition as NumPy's default (``(n - 1) * q`` index), so a single
    sample returns itself and results are fully deterministic.
    """
    data = sorted(_as_floats(samples))
    if len(data) == 1:
        return data[0]
    pos = 0.95 * (len(data) - 1)
    lo = math.floor(pos)
    hi = min(lo + 1, len(data) - 1)
    frac = pos - lo
    return data[lo] + (data[hi] - data[lo]) * frac


def compute_mad(samples: list[float]) -> float:
    """Median absolute deviation around the median (0.0 for <= 1 samples)."""
    if not samples or len(samples) <= 1:
        return 0.0
    data = _as_floats(samples)
    med = statistics.median(data)
    return float(statistics.median(abs(x - med) for x in data))


# --------------------------------------------------------------------------
# Builders
# --------------------------------------------------------------------------


def _is_number(value: Any) -> bool:
    """True for int/float (finite) and bool."""
    if isinstance(value, bool):
        return True
    if isinstance(value, (int, float)):
        return math.isfinite(value)
    return False


def make_metric(
    *,
    unit: str,
    better: str,
    value: float | bool | None = None,
    samples: list[float] | None = None,
) -> dict:
    """Build one metric entry.

    With ``samples`` the entry carries ``samples``, ``median`` and ``p95``
    (plus ``value`` when explicitly given); otherwise just ``value``.
    """
    if not isinstance(unit, str) or not unit:
        raise ValueError("metric unit must be a non-empty string")
    if better not in VALID_BETTER:
        raise ValueError(f"metric better must be one of {VALID_BETTER}, got {better!r}")
    metric: dict[str, Any] = {"unit": unit, "better": better}
    if samples:
        if not all(_is_number(x) for x in samples):
            raise ValueError("metric samples must be finite numbers")
        metric["samples"] = list(samples)
        metric["median"] = compute_median(samples)
        metric["p95"] = compute_p95(samples)
        if value is not None:
            metric["value"] = value
    elif value is not None:
        if not _is_number(value):
            raise ValueError(f"metric value must be a finite number or bool, got {value!r}")
        metric["value"] = value
    else:
        raise ValueError("metric needs a value or non-empty samples")
    return metric


def make_case(
    *,
    suite: str,
    case: str,
    dims: dict,
    status: str = "ok",
    error: str | None = None,
    metrics: dict | None = None,
    evidence: dict | None = None,
) -> dict:
    """Build one case entry. Non-``ok`` cases must carry an ``error`` reason."""
    if status not in VALID_STATUSES:
        raise ValueError(f"case status must be one of {VALID_STATUSES}, got {status!r}")
    if status != "ok" and not (isinstance(error, str) and error.strip()):
        raise ValueError(f"case with status {status!r} needs a non-empty error reason")
    return {
        "suite": suite,
        "case": case,
        "dims": dict(dims),
        "status": status,
        "error": error,
        "metrics": dict(metrics or {}),
        "evidence": dict(evidence or {}),
    }


def _run_cmd(args: list[str], cwd: Path | None = None) -> str | None:
    try:
        proc = subprocess.run(
            args,
            cwd=str(cwd) if cwd else None,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    out = proc.stdout.strip()
    return out or None


def _detect_cpu() -> str:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text(errors="replace").splitlines():
                key, _, val = line.partition(":")
                if key.strip() in ("model name", "Hardware", "Processor") and val.strip():
                    return val.strip()
        except OSError:
            pass
    elif sys.platform == "darwin":
        out = _run_cmd(["sysctl", "-n", "machdep.cpu.brand_string"])
        if out:
            return out
    return platform.processor() or platform.machine() or "unknown"


def _detect_ram_bytes() -> int | None:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    parts = line.split()
                    return int(parts[1]) * 1024  # kB
        except (OSError, ValueError, IndexError):
            return None
    elif sys.platform == "darwin":
        out = _run_cmd(["sysctl", "-n", "hw.memsize"])
        if out:
            try:
                return int(out)
            except ValueError:
                return None
    elif sys.platform == "win32":
        try:
            import ctypes

            class _MemStatus(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat = _MemStatus()
            stat.dwLength = ctypes.sizeof(_MemStatus)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat)):  # type: ignore[attr-defined]
                return int(stat.ullTotalPhys)
        except Exception:  # noqa: BLE001 - best effort only
            return None
    return None


def detect_runner_info(label: str | None = None) -> dict:
    """Describe the machine: label, CPU model, logical cores, RAM in GB.

    The label never defaults to the hostname (results are uploaded as CI
    artifacts); it comes from ``label``, ``LF_PERF_RUNNER_LABEL``, the
    GitHub ``RUNNER_NAME``, or ``"local"``.
    """
    resolved = (
        label
        or os.environ.get("LF_PERF_RUNNER_LABEL")
        or os.environ.get("RUNNER_NAME")
        or "local"
    )
    ram = _detect_ram_bytes()
    return {
        "label": resolved,
        "cpu": _detect_cpu(),
        "cores": int(os.cpu_count() or 0),
        "ram_gb": round(ram / 1024**3) if ram else 0,
    }


def detect_git_info(repo_root: Path | None = None) -> tuple[str, str]:
    """Return ``(sha, ref)`` of the checkout, falling back to ``"unknown"``."""
    root = repo_root or Path(__file__).resolve().parents[2]
    sha = _run_cmd(["git", "rev-parse", "HEAD"], cwd=root) or os.environ.get("GITHUB_SHA")
    ref = _run_cmd(["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=root)
    if not ref or ref == "HEAD":
        ref = os.environ.get("GITHUB_HEAD_REF") or os.environ.get("GITHUB_REF") or ref
    return (sha or "unknown", ref or "unknown")


def normalize_os(name: str) -> str:
    key = name.strip().lower()
    if key.startswith("linux"):
        return "linux"
    if key in _OS_ALIASES:
        return _OS_ALIASES[key]
    raise ValueError(f"unsupported OS {name!r}; expected one of {VALID_OS}")


def normalize_arch(name: str) -> str:
    key = name.strip().lower()
    if key in _ARCH_ALIASES:
        return _ARCH_ALIASES[key]
    raise ValueError(f"unsupported arch {name!r}; expected one of {VALID_ARCH}")


def _utc_now_iso() -> str:
    now = _dt.datetime.now(_dt.UTC).replace(microsecond=0)
    return now.isoformat().replace("+00:00", "Z")


def make_run(
    *,
    tier: str,
    runtime: str,
    run_id: str | None = None,
    sha: str | None = None,
    ref: str | None = None,
    os_name: str | None = None,
    arch: str | None = None,
    runner: dict | None = None,
    started_at: str | None = None,
    harness: str = "1",
) -> dict:
    """Build the ``run`` header, detecting anything not passed in."""
    if tier not in VALID_TIERS:
        raise ValueError(f"tier must be one of {VALID_TIERS}, got {tier!r}")
    if runtime not in VALID_RUNTIMES:
        raise ValueError(f"runtime must be one of {VALID_RUNTIMES}, got {runtime!r}")
    if sha is None or ref is None:
        det_sha, det_ref = detect_git_info()
        sha = sha if sha is not None else det_sha
        ref = ref if ref is not None else det_ref
    return {
        "id": run_id or uuid.uuid4().hex,
        "sha": sha,
        "ref": ref,
        "tier": tier,
        "runtime": runtime,
        "os": normalize_os(os_name if os_name is not None else sys.platform),
        "arch": normalize_arch(arch if arch is not None else platform.machine()),
        "runner": dict(runner) if runner is not None else detect_runner_info(),
        "started_at": started_at or _utc_now_iso(),
        "harness": str(harness),
    }


def make_result(run: dict, cases: list[dict]) -> dict:
    return {"schema": SCHEMA_VERSION, "run": dict(run), "cases": list(cases)}


# --------------------------------------------------------------------------
# Validation and IO
# --------------------------------------------------------------------------


def _validate_run(run: Any) -> list[str]:
    if not isinstance(run, dict):
        return ["run: must be an object"]
    errors: list[str] = []
    for key in _RUN_STR_FIELDS:
        val = run.get(key)
        if not isinstance(val, str) or not val:
            errors.append(f"run.{key}: required non-empty string")
    for key, allowed in (
        ("tier", VALID_TIERS),
        ("runtime", VALID_RUNTIMES),
        ("os", VALID_OS),
        ("arch", VALID_ARCH),
    ):
        if key not in run:
            errors.append(f"run.{key}: required")
        elif run[key] not in allowed:
            errors.append(f"run.{key}: {run[key]!r} not in {allowed}")
    started = run.get("started_at")
    if isinstance(started, str) and started:
        try:
            _dt.datetime.fromisoformat(started)
        except ValueError:
            errors.append(f"run.started_at: not ISO-8601: {started!r}")
    runner = run.get("runner")
    if not isinstance(runner, dict):
        errors.append("run.runner: required object")
    else:
        for key in ("label", "cpu"):
            if not isinstance(runner.get(key), str):
                errors.append(f"run.runner.{key}: required string")
        for key in ("cores", "ram_gb"):
            val = runner.get(key)
            if isinstance(val, bool) or not isinstance(val, int) or val < 0:
                errors.append(f"run.runner.{key}: required non-negative integer")
    return errors


def _validate_metric(where: str, metric: Any) -> list[str]:
    if not isinstance(metric, dict):
        return [f"{where}: must be an object"]
    errors: list[str] = []
    if not isinstance(metric.get("unit"), str) or not metric.get("unit"):
        errors.append(f"{where}.unit: required non-empty string")
    if metric.get("better") not in VALID_BETTER:
        errors.append(f"{where}.better: {metric.get('better')!r} not in {VALID_BETTER}")
    has_data = False
    if "samples" in metric:
        samples = metric["samples"]
        if not isinstance(samples, list) or not all(_is_number(x) for x in samples):
            errors.append(f"{where}.samples: must be a list of finite numbers")
        elif samples:
            has_data = True
    for key in ("value", "median", "p95"):
        if key in metric and metric[key] is not None:
            if not _is_number(metric[key]):
                errors.append(f"{where}.{key}: must be a finite number or bool")
            elif key != "p95":
                has_data = True
    if not has_data:
        errors.append(f"{where}: needs at least one of value, samples or median")
    return errors


def validate_result(data: Any) -> list[str]:
    """Return a list of problems; an empty list means ``data`` is valid."""
    if not isinstance(data, dict):
        return ["result: must be a JSON object"]
    errors: list[str] = []
    if data.get("schema") != SCHEMA_VERSION:
        errors.append(f"schema: expected {SCHEMA_VERSION!r}, got {data.get('schema')!r}")
    if "run" not in data:
        errors.append("run: required")
    else:
        errors.extend(_validate_run(data["run"]))
    cases = data.get("cases")
    if not isinstance(cases, list):
        errors.append("cases: must be a list")
        return errors
    seen: set[tuple[str, str]] = set()
    for idx, case in enumerate(cases):
        where = f"cases[{idx}]"
        if not isinstance(case, dict):
            errors.append(f"{where}: must be an object")
            continue
        suite, name = case.get("suite"), case.get("case")
        if not isinstance(suite, str) or not suite:
            errors.append(f"{where}.suite: required non-empty string")
        if not isinstance(name, str) or not name:
            errors.append(f"{where}.case: required non-empty string")
        if isinstance(suite, str) and isinstance(name, str) and suite and name:
            where = f"cases[{idx}] ({suite}/{name})"
            if (suite, name) in seen:
                errors.append(f"{where}: duplicate (suite, case)")
            seen.add((suite, name))
        if not isinstance(case.get("dims"), dict):
            errors.append(f"{where}.dims: required object")
        status = case.get("status")
        if status not in VALID_STATUSES:
            errors.append(f"{where}.status: {status!r} not in {VALID_STATUSES}")
        if "error" not in case:
            errors.append(f"{where}.error: required (null when ok)")
        error = case.get("error")
        if status in ("failed", "not_exercised"):
            if not isinstance(error, str) or not error.strip():
                errors.append(f"{where}.error: status {status!r} needs a non-empty reason")
        elif error is not None and not isinstance(error, str):
            errors.append(f"{where}.error: must be a string or null")
        metrics = case.get("metrics")
        if not isinstance(metrics, dict):
            errors.append(f"{where}.metrics: required object")
        else:
            for mname, metric in metrics.items():
                errors.extend(_validate_metric(f"{where}.metrics.{mname}", metric))
        if not isinstance(case.get("evidence"), dict):
            errors.append(f"{where}.evidence: required object")
    return errors


def load_result(path: Path) -> dict:
    """Read a result file (not validated; see ``validate_result``)."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise TypeError(f"{path}: top-level JSON must be an object")
    return data


def write_result(path: Path, data: dict) -> None:
    """Validate ``data`` and write it as pretty JSON, creating parent dirs."""
    errors = validate_result(data)
    if errors:
        raise ValueError("invalid lf-perf-result/v1:\n  " + "\n  ".join(errors))
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
