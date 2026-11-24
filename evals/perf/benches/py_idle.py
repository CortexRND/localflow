"""Python idle footprint probe (counterpart to ``rust/lf-runtime/examples/idle.rs``).

Imports the Python runtime (``localflow.app`` by default), loads a fresh
config from a temp ``LOCALFLOW_CONFIG`` and starts the config watcher, like
``idle.rs`` wires its runtime with null audio/speech, then sits idle for
``--seconds`` and reports::

    {"seconds": 10, "rss_bytes": .., "hwm_bytes": .., "threads": ..,
     "cpu_seconds": .., "mode": "runtime" | "headless" | "bare",
     "imports": [...], "source": "proc" | "psapi" | "ps"}

Without a display ``localflow.app`` cannot import its pynput hotkey
listener; the probe then imports the same runtime modules minus that one
(``mode="headless"``).

``cpu_seconds`` is the process CPU time (user + system) spent *during* the
idle window, as ``idle.rs`` counts ticks across its sleep.

``--bare`` skips ``localflow`` entirely (interpreter-only baseline).

Exit codes: 0 ok, 2 bad arguments, 3 ``localflow`` could not be imported.

Benches may import ``localflow``; this file itself only needs the stdlib.
"""

from __future__ import annotations

import argparse
import importlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

EXIT_UNAVAILABLE = 3
DEFAULT_IMPORTS = ("localflow.app",)
#: ``localflow.app`` minus the pynput hotkey listener (needs a display).
HEADLESS_IMPORTS = (
    "localflow.config",
    "localflow.live_config",
    "localflow.jobs",
    "localflow.dispatch",
    "localflow.commands",
    "localflow.audio",
    "localflow.stt",
    "localflow.cleanup",
    "localflow.inject",
    "localflow.history",
)


# --------------------------------------------------------------------------
# Measurements
# --------------------------------------------------------------------------


def _proc_status() -> dict[str, str]:
    fields: dict[str, str] = {}
    try:
        text = Path("/proc/self/status").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return fields
    for line in text.splitlines():
        key, _, val = line.partition(":")
        fields[key.strip()] = val.strip()
    return fields


def _kb(value: str | None) -> int | None:
    if not value:
        return None
    parts = value.split()
    try:
        return int(parts[0]) * 1024
    except (ValueError, IndexError):
        return None


def _windows_memory() -> tuple[int | None, int | None]:
    try:
        import ctypes
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        counters = Counters()
        counters.cb = ctypes.sizeof(Counters)
        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
        psapi = ctypes.windll.psapi  # type: ignore[attr-defined]
        kernel32.GetCurrentProcess.restype = wintypes.HANDLE
        psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]
        psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        handle = kernel32.GetCurrentProcess()
        if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return int(counters.WorkingSetSize), int(counters.PeakWorkingSetSize)
    except Exception as exc:  # noqa: BLE001 - best effort
        sys.stderr.write(f"py_idle: GetProcessMemoryInfo failed: {exc}\n")
    return None, None


def _ps_rss_bytes() -> int | None:
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(os.getpid())],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5, check=False,
        ).stdout.strip()
        return int(out) * 1024 if out else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def _maxrss_bytes() -> int | None:
    try:
        import resource
    except ImportError:
        return None
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(peak) if sys.platform == "darwin" else int(peak) * 1024  # macOS: bytes; else KiB


def sample() -> dict:
    """Current ``rss_bytes``, ``hwm_bytes``, ``threads`` and their ``source``."""
    status = _proc_status()
    if status.get("VmRSS"):
        rss, hwm = _kb(status.get("VmRSS")), _kb(status.get("VmHWM"))
        try:
            threads = int(status.get("Threads", ""))
        except ValueError:
            threads = threading.active_count()
        return {"rss_bytes": rss, "hwm_bytes": hwm or rss, "threads": threads, "source": "proc"}
    if sys.platform == "win32":
        rss, hwm = _windows_memory()
        return {"rss_bytes": rss, "hwm_bytes": hwm, "threads": threading.active_count(), "source": "psapi"}
    rss = _ps_rss_bytes()
    hwm = _maxrss_bytes()
    if rss is not None and hwm is not None:
        hwm = max(hwm, rss)
    return {"rss_bytes": rss, "hwm_bytes": hwm, "threads": threading.active_count(), "source": "ps"}


# --------------------------------------------------------------------------
# Runtime wiring
# --------------------------------------------------------------------------


def import_runtime(imports: list[str] | None) -> tuple[list[str], str]:
    """Import ``imports``; with ``None`` try the full app, else the headless set.

    ``localflow.app`` pulls in the global hotkey listener (pynput), which
    raises ``ImportError`` without a display (headless CI); the headless set
    is the same runtime minus that listener. Returns ``(modules, mode)``.
    """
    if imports is not None:
        for name in imports:
            importlib.import_module(name)
        return imports, "runtime"
    try:
        for name in DEFAULT_IMPORTS:
            importlib.import_module(name)
        return list(DEFAULT_IMPORTS), "runtime"
    except ImportError as exc:
        if exc.name == "localflow" or not importlib.util.find_spec("localflow"):
            raise
    for name in HEADLESS_IMPORTS:
        importlib.import_module(name)
    return list(HEADLESS_IMPORTS), "headless"


def wire_runtime() -> list[object]:
    """Load config and start the config watcher (``idle.rs`` does the same).

    Returns objects to keep alive while idling.
    """
    keep: list[object] = []
    try:
        from localflow.config import load_config
        from localflow.live_config import ConfigWatcher
    except ImportError:
        return keep
    keep.append(load_config())
    watcher = ConfigWatcher(lambda _config: None)
    watcher.start()
    keep.append(watcher)
    return keep


def run(seconds: float, *, bare: bool, imports: list[str] | None = None) -> dict:
    keep: list[object] = []
    modules: list[str] = []
    mode = "bare"
    if not bare:
        modules, mode = import_runtime(imports)
        keep = wire_runtime()
    before = time.process_time()
    time.sleep(seconds)
    cpu = time.process_time() - before
    result: dict = {"seconds": seconds, "mode": mode, "imports": modules}
    result.update(sample())
    result["cpu_seconds"] = round(max(cpu, 0.0), 6)
    del keep
    return result


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seconds", type=float, default=10.0, help="idle window (default 10; nightly 30)")
    parser.add_argument("--json", type=Path, help="write the JSON here (default: stdout)")
    parser.add_argument("--bare", action="store_true", help="do not import localflow")
    parser.add_argument(
        "--imports",
        default=None,
        help=(
            "comma-separated modules to import before idling (default: "
            f"{','.join(DEFAULT_IMPORTS)}, or the headless set without a display)"
        ),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.seconds < 0:
        print("--seconds must be >= 0", file=sys.stderr)
        return 2
    imports = None if args.imports is None else [m.strip() for m in args.imports.split(",") if m.strip()]
    tmp = None
    if not args.bare and not os.environ.get("LOCALFLOW_CONFIG"):
        tmp = tempfile.TemporaryDirectory(prefix="lf-perf-idle-")
        os.environ["LOCALFLOW_CONFIG"] = str(Path(tmp.name) / "config.toml")
    try:
        result = run(args.seconds, bare=args.bare, imports=imports)
    except ImportError as exc:
        print(f"localflow runtime unavailable: {exc}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    finally:
        if tmp is not None:
            tmp.cleanup()
    text = json.dumps(result, indent=2) + "\n"
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
