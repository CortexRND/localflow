"""Python STT bench over ``localflow.stt.Transcriber``.

Mirror of ``rust/lf-stt/examples/stt_bench.rs``: same CLI flags, same JSON in
single-clip and ``--manifest`` mode, so the harness (``evals/perf/suites/stt.py``)
treats both runtimes identically. This is the only module under
``evals/perf`` allowed to import ``localflow`` and ``numpy``.

Exit codes: 0 ok, 1 benchmark error, 2 bad usage, 3 an engine dependency is
not installed (the harness reports that as ``not_exercised``).

Run: ``.venv-tk/bin/python evals/perf/benches/py_stt.py --manifest m.json --json out.json``
"""

from __future__ import annotations

import argparse
import gc
import json
import math
import platform
import sys
import time
import wave
from collections.abc import Callable
from pathlib import Path
from typing import Any

try:
    import numpy as np
except ImportError as _exc:  # pragma: no cover - depends on the interpreter
    if __name__ == "__main__":
        print(f"unavailable: numpy is not installed: {_exc}", file=sys.stderr)
        sys.exit(3)
    raise

SAMPLE_RATE = 16000
FIXED_CLIP = "test_wavs/0.wav"
FIXED_CLIP_REPO = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"
EXIT_UNAVAILABLE = 3

#: requested provider -> ``Transcriber(device=...)``. MLX/Metal pick the GPU
#: themselves and only accept ``auto``.
PROVIDER_DEVICE = {"auto": "auto", "cpu": "cpu", "cuda": "cuda", "mlx": "auto", "metal": "auto"}

#: bench engine id -> ``Transcriber(backend=...)``.
ENGINE_BACKEND = {
    "faster-whisper": "faster-whisper",
    "mlx-whisper": "mlx-whisper",
    "mlx": "mlx-whisper",
    "parakeet": "parakeet",
}

TranscriberFactory = Callable[..., Any]


class Unavailable(RuntimeError):
    """An optional engine dependency is missing (maps to exit code 3)."""


# --------------------------------------------------------------------------
# Audio
# --------------------------------------------------------------------------


def _pcm_to_float(raw: bytes, width: int) -> np.ndarray:
    if width == 1:  # unsigned 8-bit
        return (np.frombuffer(raw, dtype=np.uint8).astype(np.float32) - 128.0) / 128.0
    if width == 2:
        return np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0
    if width == 3:
        b = np.frombuffer(raw, dtype=np.uint8).reshape(-1, 3).astype(np.int32)
        ints = b[:, 0] | (b[:, 1] << 8) | (b[:, 2] << 16)
        ints = np.where(ints & 0x800000, ints - (1 << 24), ints)
        return ints.astype(np.float32) / float(1 << 23)
    if width == 4:
        return np.frombuffer(raw, dtype="<i4").astype(np.float32) / float(1 << 31)
    raise ValueError(f"unsupported WAV sample width: {width} bytes")


def resample_linear(pcm: np.ndarray, rate: int, target: int = SAMPLE_RATE) -> np.ndarray:
    """Linear-interpolation resample; output length ``round(n * target / rate)``."""
    if rate == target or pcm.size == 0:
        return pcm.astype(np.float32, copy=False)
    wanted = round(pcm.size * target / rate)
    positions = np.arange(wanted, dtype=np.float64) * (rate / target)
    return np.interp(positions, np.arange(pcm.size, dtype=np.float64), pcm).astype(np.float32)


def load_wav(path: Path) -> tuple[np.ndarray, int]:
    """Read a PCM WAV into 1-D float32 16 kHz mono; returns ``(pcm, original_rate)``.

    Multi-channel audio keeps the first channel, like the Rust bench's
    ``support::read_clip``, so both runtimes see the same samples.
    """
    with wave.open(str(path), "rb") as wav:
        channels, width, rate = wav.getnchannels(), wav.getsampwidth(), wav.getframerate()
        raw = wav.readframes(wav.getnframes())
    if channels == 0 or rate == 0:
        raise ValueError(f"{path}: invalid WAV rate/channels")
    samples = _pcm_to_float(raw, width)
    if samples.size == 0:
        raise ValueError(f"{path}: benchmark WAV is empty")
    mono = samples[: samples.size - samples.size % channels].reshape(-1, channels)[:, 0]
    pcm = resample_linear(np.ascontiguousarray(mono), rate)
    if not np.all(np.isfinite(pcm)):
        raise ValueError(f"{path}: benchmark WAV contains non-finite samples")
    return pcm, rate


def fixed_clip_path() -> Path:
    """The Parakeet ``test_wavs/0.wav`` the Rust bench uses by default."""
    try:
        from huggingface_hub import hf_hub_download
    except ImportError as exc:  # pragma: no cover - depends on the environment
        raise Unavailable("huggingface_hub is needed to fetch the default clip; pass --clip") from exc
    return Path(hf_hub_download(FIXED_CLIP_REPO, FIXED_CLIP))


# --------------------------------------------------------------------------
# Process stats
# --------------------------------------------------------------------------


def _rss_windows() -> int:  # pragma: no cover - Windows only
    import ctypes
    from ctypes import wintypes

    class _Counters(ctypes.Structure):
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

    counters = _Counters()
    counters.cb = ctypes.sizeof(_Counters)
    kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
    psapi = ctypes.windll.psapi  # type: ignore[attr-defined]
    kernel32.GetCurrentProcess.argtypes = []
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(_Counters), wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    handle = kernel32.GetCurrentProcess()
    if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
        return int(counters.WorkingSetSize)
    return 0


def _rss_macos() -> int:  # pragma: no cover - macOS only
    import ctypes

    class _TimeValue(ctypes.Structure):
        _fields_ = [("seconds", ctypes.c_int32), ("microseconds", ctypes.c_int32)]

    class _MachTaskBasicInfo(ctypes.Structure):
        _fields_ = [
            ("virtual_size", ctypes.c_uint64),
            ("resident_size", ctypes.c_uint64),
            ("resident_size_max", ctypes.c_uint64),
            ("user_time", _TimeValue),
            ("system_time", _TimeValue),
            ("policy", ctypes.c_int32),
            ("suspend_count", ctypes.c_int32),
        ]

    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    libc.mach_task_self.restype = ctypes.c_uint32
    libc.task_info.argtypes = [
        ctypes.c_uint32,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_uint32),
    ]
    libc.task_info.restype = ctypes.c_int32
    mach_task_basic_info = 20
    info = _MachTaskBasicInfo()
    count = ctypes.c_uint32(ctypes.sizeof(info) // ctypes.sizeof(ctypes.c_uint32))
    if libc.task_info(libc.mach_task_self(), mach_task_basic_info, ctypes.byref(info), ctypes.byref(count)) == 0:
        return int(info.resident_size)
    return 0


def rss_bytes() -> int:
    """Current resident set size in bytes (peak RSS where current is unavailable)."""
    try:
        with open("/proc/self/status", encoding="ascii", errors="replace") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass
    if sys.platform == "win32":  # pragma: no cover
        return _rss_windows()
    if sys.platform == "darwin":  # pragma: no cover
        current = _rss_macos()
        if current > 0:
            return current
    try:
        import resource
    except ImportError:  # pragma: no cover
        return 0
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(peak if sys.platform == "darwin" else peak * 1024)


def _os_name() -> str:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "darwin":
        return "macos"
    if sys.platform in ("win32", "cygwin"):
        return "windows"
    return sys.platform


def _arch() -> str:
    machine = platform.machine().lower()
    return {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}.get(machine, machine)


# --------------------------------------------------------------------------
# Bench
# --------------------------------------------------------------------------


def stats(latencies: list[float]) -> tuple[float, float]:
    """``(median, p95)`` with the same nearest-rank p95 as the Rust bench."""
    data = sorted(latencies)
    n = len(data)
    median = (data[(n - 1) // 2] + data[n // 2]) / 2.0
    return median, data[math.ceil(n * 0.95) - 1]


def _default_factory(**kwargs: Any) -> Any:
    try:
        from localflow.stt import Transcriber
    except ImportError as exc:
        raise Unavailable(f"localflow.stt is not importable: {exc}") from exc
    return Transcriber(**kwargs)


def _model_label(engine: str, model: str) -> str:
    if engine == "parakeet":
        try:
            from localflow.stt import PARAKEET_REPO

            return str(PARAKEET_REPO)
        except ImportError:
            return model
    return model


def _effective_provider(transcriber: Any, device: str) -> str:
    backend = str(getattr(transcriber, "backend", ""))
    if backend in ("mlx", "mlx-whisper", "parakeet"):
        return "mlx"
    return "cuda" if device == "cuda" else "cpu"


def _load(args: argparse.Namespace, engine: str, factory: TranscriberFactory) -> dict:
    backend = ENGINE_BACKEND.get(engine)
    if backend is None:
        raise ValueError(f"unknown engine {engine!r}; expected one of {sorted(ENGINE_BACKEND)}")
    device = PROVIDER_DEVICE.get(args.provider)
    if device is None:
        raise ValueError(f"unknown provider {args.provider!r}; expected one of {sorted(PROVIDER_DEVICE)}")
    rss_before = rss_bytes()
    start = time.perf_counter()
    try:
        transcriber = factory(
            model_size=args.model, language=args.language, backend=backend, device=device
        )
    except ImportError as exc:
        raise Unavailable(f"{engine} is not installed: {exc}") from exc
    load_ms = (time.perf_counter() - start) * 1000.0
    actual = str(getattr(transcriber, "backend", backend))
    if backend == "mlx-whisper" and actual != "mlx":
        # Transcriber silently uses faster-whisper for sizes without an MLX build.
        raise ValueError(f"mlx-whisper has no build for model {args.model!r} (got {actual})")
    return {
        "transcriber": transcriber,
        "engine": str(getattr(transcriber, "backend", engine) or engine),
        "model": _model_label(engine, args.model),
        "requested_provider": args.provider,
        "effective_provider": _effective_provider(transcriber, device),
        # Transcriber downloads inside load; download time is part of load_ms.
        "download_ms": 0.0,
        "load_ms": load_ms,
        "rss_before_bytes": rss_before,
        "rss_after_bytes": rss_bytes(),
    }


def _unload(loaded: dict) -> int:
    transcriber = loaded.pop("transcriber")
    provider = getattr(transcriber, "provider", None)
    for name in ("unload", "close"):
        fn = getattr(provider, name, None)
        if callable(fn):
            fn()
            break
    del provider, transcriber
    gc.collect()
    return rss_bytes()


def time_clip(transcriber: Any, pcm: np.ndarray, args: argparse.Namespace) -> dict:
    for _ in range(args.warmup):
        transcriber.transcribe(pcm, prompt=args.prompt)
    latencies: list[float] = []
    transcript = ""
    for _ in range(args.runs):
        start = time.perf_counter()
        text = transcriber.transcribe(pcm, prompt=args.prompt)
        latencies.append((time.perf_counter() - start) * 1000.0)
        if not str(text).strip():
            raise RuntimeError(f"{getattr(transcriber, 'backend', 'engine')} returned an empty transcript")
        transcript = str(text)
    median, p95 = stats(latencies)
    return {"latencies_ms": latencies, "first_ms": latencies[0], "median_ms": median, "p95_ms": p95,
            "transcript": transcript}


def parse_manifest(text: str, base: Path) -> list[dict]:
    """``{"clips": [{"id","path"}]}`` or ``[{"id","path"}]``; relative paths resolve to ``base``."""
    data = json.loads(text)
    clips = data.get("clips") if isinstance(data, dict) else data
    if not isinstance(clips, list) or not clips:
        raise ValueError("manifest lists no clips")
    out: list[dict] = []
    seen: set[str] = set()
    for clip in clips:
        if not isinstance(clip, dict) or not isinstance(clip.get("path"), str):
            raise TypeError(f"manifest clip needs id and path: {clip!r}")
        cid = clip.get("id")
        if not isinstance(cid, str) or not cid.strip():
            raise ValueError("manifest clip id is empty")
        if cid in seen:
            raise ValueError(f"duplicate manifest clip id {cid!r}")
        seen.add(cid)
        path = Path(clip["path"])
        out.append({"id": cid, "path": path if path.is_absolute() else base / path})
    return out


def run_manifest(args: argparse.Namespace, factory: TranscriberFactory) -> dict:
    entries = parse_manifest(Path(args.manifest).read_text(encoding="utf-8"), Path(args.manifest).parent)
    clips = []
    for entry in entries:
        pcm, rate = load_wav(entry["path"])
        clips.append((entry["id"], str(entry["path"]), pcm, rate))
    results = []
    for engine in args.engines:
        loaded = _load(args, engine, factory)
        measured = []
        for cid, name, pcm, rate in clips:
            seconds = pcm.size / SAMPLE_RATE
            timed = time_clip(loaded["transcriber"], pcm, args)
            measured.append({"id": cid, "clip": name, "audio_seconds": seconds, "original_sample_rate": rate,
                             **timed, "real_time_factor": timed["median_ms"] / (seconds * 1000.0)})
        rss_after_unload = _unload(loaded)
        results.append({**loaded, "rss_after_unload_bytes": rss_after_unload, "clips": measured})
    return {"mode": "manifest", "runs": args.runs, "warmup": args.warmup, "os": _os_name(), "arch": _arch(),
            "results": results}


def run_single(args: argparse.Namespace, factory: TranscriberFactory) -> dict:
    path = Path(args.clip) if args.clip else fixed_clip_path()
    pcm, rate = load_wav(path)
    seconds = pcm.size / SAMPLE_RATE
    results = []
    for engine in args.engines:
        loaded = _load(args, engine, factory)
        timed = time_clip(loaded["transcriber"], pcm, args)
        rss_after_unload = _unload(loaded)
        results.append({**loaded, "rss_after_unload_bytes": rss_after_unload,
                        "latencies_ms": timed["latencies_ms"], "first_ms": timed["first_ms"],
                        "median_ms": timed["median_ms"], "p95_ms": timed["p95_ms"],
                        "real_time_factor": timed["median_ms"] / (seconds * 1000.0),
                        "transcript": timed["transcript"]})
    return {"clip": str(path), "fixed_clip": FIXED_CLIP, "audio_seconds": seconds, "original_sample_rate": rate,
            "sample_rate": SAMPLE_RATE, "runs": args.runs, "os": _os_name(), "arch": _arch(), "results": results}


def render_table(report: dict) -> str:
    lines = []
    if report.get("mode") == "manifest":
        lines.append(f"{report['runs']} runs after {report['warmup']} warm-up.\n")
        lines.append("| Engine / model | Clip | Requested → effective provider | Audio s | First ms | Median ms | p95 ms | RTF |")
        lines.append("|---|---|---|---:|---:|---:|---:|---:|")
        for r in report["results"]:
            for c in r["clips"]:
                lines.append(f"| {r['engine']} / {r['model']} | {c['id']} | {r['requested_provider']} → "
                             f"{r['effective_provider']} | {c['audio_seconds']:.3f} | {c['first_ms']:.1f} | "
                             f"{c['median_ms']:.1f} | {c['p95_ms']:.1f} | {c['real_time_factor']:.3f} |")
    else:
        lines.append(f"Clip: `{report['clip']}` ({report['audio_seconds']:.3f} s, {report['original_sample_rate']} Hz"
                     f" → 16000 Hz), {report['runs']} runs.\n")
        lines.append("| Engine / model | Requested → effective provider | Load ms | First ms | Median ms | p95 ms | RTF |")
        lines.append("|---|---|---:|---:|---:|---:|---:|")
        for r in report["results"]:
            lines.append(f"| {r['engine']} / {r['model']} | {r['requested_provider']} → {r['effective_provider']} | "
                         f"{r['load_ms']:.1f} | {r['first_ms']:.1f} | {r['median_ms']:.1f} | {r['p95_ms']:.1f} | "
                         f"{r['real_time_factor']:.3f} |")
    lines.append("\nRSS is process-wide (allocator caches may survive unload).")
    return "\n".join(lines) + "\n"


def _positive_int(text: str) -> int:
    value = int(text)
    if value < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return value


def _non_negative_int(text: str) -> int:
    value = int(text)
    if value < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return value


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Benchmark localflow.stt (mirror of the Rust stt_bench).")
    p.add_argument("--engines", default="faster-whisper",
                   type=lambda s: [e.strip() for e in s.split(",") if e.strip()])
    p.add_argument("--model", default="base.en")
    p.add_argument("--provider", default="auto", choices=sorted(PROVIDER_DEVICE))
    p.add_argument("--language")
    p.add_argument("--prompt")
    src = p.add_mutually_exclusive_group()
    src.add_argument("--clip", type=Path)
    src.add_argument("--manifest", type=Path)
    p.add_argument("--runs", type=_positive_int, default=10)
    p.add_argument("--warmup", type=_non_negative_int, default=0)
    p.add_argument("--json", type=Path)
    p.add_argument("--table", type=Path)
    return p


def main(argv: list[str] | None = None, *, factory: TranscriberFactory | None = None) -> int:
    args = build_parser().parse_args(argv)
    if not args.engines:
        print("error: --engines is empty", file=sys.stderr)
        return 2
    factory = factory or _default_factory
    try:
        report = run_manifest(args, factory) if args.manifest else run_single(args, factory)
    except Unavailable as exc:
        print(f"unavailable: {exc}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    encoded = json.dumps(report, indent=2)
    table = render_table(report)
    if args.json:
        Path(args.json).write_text(encoded, encoding="utf-8")
    if args.table:
        Path(args.table).write_text(table, encoding="utf-8")
    print(encoded)
    print(table, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
