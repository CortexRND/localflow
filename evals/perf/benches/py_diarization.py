"""Python speaker-diarization bench over ``localflow.diarization``.

Runs a diarizer (+ embedder) over the mixes listed in ``--manifest`` and
emits speaker turns, timings and RSS as JSON; the harness
(``evals/perf/suites/diarization.py``) scores the turns with
:mod:`evals.perf.der`. Besides ``py_stt.py`` this is the only module under
``evals/perf`` allowed to import ``localflow`` and ``numpy``. ``sherpa_onnx`` is
never imported here directly; it loads through ``localflow.diarization.sherpa``.

Pipelines:

* ``whole``: one ``diarize`` call over the full mix.
* ``chunked``: the meeting product path (fixed chunks, per-chunk diarize and
  embed, one clustering pass over every embedding at the end).

Exit codes: 0 ok, 1 benchmark error, 2 bad usage, 3 the backend (runtime or
models) is not available (the harness reports that as ``not_exercised``).

Run: ``python evals/perf/benches/py_diarization.py --manifest m.json --json out.json``
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import time
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

try:
    from evals.perf.benches.py_stt import _arch, _os_name, load_wav, rss_bytes, stats
except ImportError:  # run as a script without the repo root on sys.path
    sys.path.insert(0, str(Path(__file__).resolve().parents[3]))
    from evals.perf.benches.py_stt import _arch, _os_name, load_wav, rss_bytes, stats

SAMPLE_RATE = 16000
EXIT_UNAVAILABLE = 3
MIN_CHUNK_S = 1.0        # MeetingSession skips shorter chunks
SILENT_PEAK = 0.01       # ... and chunks whose peak is below this
#: Mirrors of localflow defaults, used only when localflow.diarization is not importable
#: (argparse defaults must not import it before rss_before is taken).
_FALLBACK_THRESHOLD = 0.5
_FALLBACK_CLUSTER_THRESHOLD = 0.5
_FALLBACK_HARD_MAX = 10
_INSTALL_HINT = (
    "Speaker labels need the sherpa-onnx diarization extra: "
    "pip install 'localflow[diarization]'."
)

Loader = Callable[[argparse.Namespace], tuple[Any, Any]]


class Unavailable(RuntimeError):
    """The diarization backend cannot be used here (maps to exit code 3)."""


# --------------------------------------------------------------------------
# Loading
# --------------------------------------------------------------------------


def _default_loader(args: argparse.Namespace) -> tuple[Any, Any]:
    try:
        if args.backend == "mock":
            from localflow.diarization.mock import MockDiarizer, MockEmbedder

            diarizer, embedder = MockDiarizer(), MockEmbedder()
            diarizer.load()
            embedder.load()
            return diarizer, embedder
        from localflow.diarization import models
    except ImportError as exc:
        raise Unavailable(f"localflow.diarization is not importable: {exc}") from exc
    if not models.runtime_installed():
        raise Unavailable(_INSTALL_HINT)
    ok, reason = models.models_ready()
    if not ok:
        raise Unavailable(reason or "speaker models are not ready")
    # Both checks passed: any DiarizationUnavailable from here on is a real failure.
    from localflow.diarization.sherpa import SherpaDiarizer, SherpaEmbedder

    diarizer = SherpaDiarizer(num_threads=args.num_threads, threshold=args.threshold)
    embedder = SherpaEmbedder(num_threads=args.num_threads)
    diarizer.load()
    embedder.load()
    return diarizer, embedder


def _resolve_defaults(args: argparse.Namespace) -> None:
    """Fill ``None`` thresholds / max speakers from localflow (after rss_before)."""
    try:
        from localflow.diarization import collector
    except ImportError:
        collector = None
    if args.threshold is None:
        threshold = _FALLBACK_THRESHOLD
        if args.backend == "sherpa-onnx":
            try:
                from localflow.diarization.sherpa import DEFAULT_THRESHOLD as threshold
            except ImportError:
                pass
        args.threshold = float(threshold)
    if args.cluster_threshold is None:
        args.cluster_threshold = float(
            getattr(collector, "DEFAULT_CLUSTER_THRESHOLD", _FALLBACK_CLUSTER_THRESHOLD))
    hard_max = int(getattr(collector, "HARD_MAX_SPEAKERS", _FALLBACK_HARD_MAX))
    args.max_speakers = hard_max if args.max_speakers == 0 else min(args.max_speakers, hard_max)


# --------------------------------------------------------------------------
# Pipelines
# --------------------------------------------------------------------------


def run_whole(diarizer: Any, embedder: Any, audio: np.ndarray, args: argparse.Namespace) -> tuple[list, int]:
    turns = [
        {"start": float(t.start), "end": float(t.end), "speaker": f"S{int(t.local_speaker) + 1}"}
        for t in diarizer.diarize(audio, SAMPLE_RATE)
    ]
    return turns, 0


def run_chunked(diarizer: Any, embedder: Any, audio: np.ndarray, args: argparse.Namespace) -> tuple[list, int]:
    """The meeting path: mirrors ``MeetingSession._transcribe_chunk`` (fixed chunks, skip
    short/silent ones) plus ``DiarizationCollector._process`` (per-chunk diarize, embed turns
    >= ``MIN_EMBED_TURN_S``) and ``DiarizationCollector._label_source`` (one clustering pass,
    fallback to the local speaker's heaviest cluster)."""
    from localflow.diarization import cluster
    from localflow.diarization.collector import MIN_EMBED_TURN_S, _unit_vector

    sr = SAMPLE_RATE
    step = max(1, round(args.chunk_seconds * sr))
    chunks: list[tuple[float, list, dict[int, int]]] = []  # (offset, turns, {turn idx: vector idx})
    vectors: list[np.ndarray] = []
    for begin in range(0, audio.size, step):
        chunk = audio[begin:begin + step]
        if chunk.size < MIN_CHUNK_S * sr or float(np.max(np.abs(chunk))) < SILENT_PEAK:
            continue
        turns = list(diarizer.diarize(chunk, sr))
        embedded: dict[int, int] = {}
        for i, turn in enumerate(turns):
            if turn.end - turn.start < MIN_EMBED_TURN_S:
                continue
            vec = _unit_vector(embedder.embed(chunk[int(turn.start * sr):int(turn.end * sr)], sr))
            if vec is not None:
                embedded[i] = len(vectors)
                vectors.append(vec)
        chunks.append((begin / sr, turns, embedded))
    labels = cluster(vectors, threshold=args.cluster_threshold, max_speakers=args.max_speakers) if vectors else []
    out: list[dict] = []
    unlabelled = 0
    for offset, turns, embedded in chunks:
        weight: dict[int, dict[int, float]] = {}
        for i, turn in enumerate(turns):
            if i in embedded:
                per = weight.setdefault(turn.local_speaker, {})
                lab = labels[embedded[i]]
                per[lab] = per.get(lab, 0.0) + (turn.end - turn.start)
        fallback = {
            local: min(per, key=lambda lab, per=per: (-per[lab], lab)) for local, per in weight.items()
        }
        for i, turn in enumerate(turns):
            label = labels[embedded[i]] if i in embedded else fallback.get(turn.local_speaker)
            if label is None:
                unlabelled += 1  # the product leaves this text as "Others": scored as missed speech
                continue
            out.append({"start": offset + float(turn.start), "end": offset + float(turn.end),
                        "speaker": f"S{label + 1}"})
    return out, unlabelled


PIPELINES = {"whole": run_whole, "chunked": run_chunked}


# --------------------------------------------------------------------------
# Bench
# --------------------------------------------------------------------------


def _vm_hwm() -> int:
    try:
        with open("/proc/self/status", encoding="ascii", errors="replace") as fh:
            for line in fh:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass
    return 0


def _peak_sample() -> int:
    return max(rss_bytes(), _vm_hwm() if sys.platform.startswith("linux") else 0)


def parse_manifest(text: str, base: Path) -> list[dict]:
    """``{"mixes": [{"id","path"}]}``; relative paths resolve against ``base``."""
    data = json.loads(text)
    mixes = data.get("mixes") if isinstance(data, dict) else None
    if not isinstance(mixes, list) or not mixes:
        raise ValueError("manifest lists no mixes")
    out: list[dict] = []
    seen: set[str] = set()
    for mix in mixes:
        if not isinstance(mix, dict) or not isinstance(mix.get("path"), str):
            raise ValueError(f"manifest mix needs id and path: {mix!r}")  # noqa: TRY004 - bad manifest = exit 1
        mid = mix.get("id")
        if not isinstance(mid, str) or not mid.strip():
            raise ValueError("manifest mix id is empty")
        if mid in seen:
            raise ValueError(f"duplicate manifest mix id {mid!r}")
        seen.add(mid)
        path = Path(mix["path"])
        out.append({"id": mid, "path": path if path.is_absolute() else base / path})
    return out


def _model_id(obj: Any, backend: str) -> str:
    return "mock" if backend == "mock" else str(getattr(obj, "id", type(obj).__name__))


def run_manifest(args: argparse.Namespace, loader: Loader, rss_before: int) -> dict:
    entries = parse_manifest(Path(args.manifest).read_text(encoding="utf-8"), Path(args.manifest).parent)
    mixes = [(e["id"], load_wav(e["path"])[0]) for e in entries]
    start = time.perf_counter()
    diarizer, embedder = loader(args)
    load_ms = (time.perf_counter() - start) * 1000.0
    rss_loaded = rss_bytes()
    peak = max(rss_loaded, _peak_sample())
    pipeline = PIPELINES[args.pipeline]
    measured = []
    for mid, audio in mixes:
        seconds = audio.size / SAMPLE_RATE
        for _ in range(args.warmup):
            pipeline(diarizer, embedder, audio, args)
            peak = max(peak, _peak_sample())
        latencies: list[float] = []
        turns: list[dict] = []
        unlabelled = 0
        for _ in range(args.runs):
            t0 = time.perf_counter()
            turns, unlabelled = pipeline(diarizer, embedder, audio, args)
            latencies.append((time.perf_counter() - t0) * 1000.0)
            peak = max(peak, _peak_sample())
        median, p95 = stats(latencies)
        measured.append({
            "id": mid, "audio_seconds": seconds, "latencies_ms": latencies, "first_ms": latencies[0],
            "median_ms": median, "p95_ms": p95, "real_time_factor": median / (seconds * 1000.0),
            "turns": [{"start": round(t["start"], 3), "end": round(t["end"], 3), "speaker": t["speaker"]}
                      for t in turns],
            "unlabelled_turns": unlabelled,
        })
    return {
        "mode": "manifest", "backend": args.backend, "pipeline": args.pipeline,
        "runs": args.runs, "warmup": args.warmup, "os": _os_name(), "arch": _arch(),
        "chunk_seconds": args.chunk_seconds, "threshold": args.threshold,
        "cluster_threshold": args.cluster_threshold, "max_speakers": args.max_speakers,
        "num_threads": args.num_threads,
        "segmentation_model": _model_id(diarizer, args.backend),
        "embedding_model": _model_id(embedder, args.backend),
        "load_ms": load_ms, "rss_before_bytes": rss_before, "rss_loaded_bytes": rss_loaded,
        "rss_peak_bytes": peak, "rss_delta_bytes": max(0, peak - rss_before),
        "mixes": measured,
    }


def render_table(report: dict) -> str:
    lines = [
        (f"{report['backend']} / {report['pipeline']}: {report['runs']} runs after {report['warmup']} warm-up; "
         f"load {report['load_ms']:.1f} ms, RSS delta {report['rss_delta_bytes'] / 2**20:.1f} MiB.\n"),
        "| Mix | Audio s | Turns | Speakers | Unlabelled | First ms | Median ms | p95 ms | RTF |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for m in report["mixes"]:
        speakers = len({t["speaker"] for t in m["turns"]})
        lines.append(f"| {m['id']} | {m['audio_seconds']:.3f} | {len(m['turns'])} | {speakers} | "
                     f"{m['unlabelled_turns']} | {m['first_ms']:.1f} | {m['median_ms']:.1f} | "
                     f"{m['p95_ms']:.1f} | {m['real_time_factor']:.4f} |")
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


def _positive_float(text: str) -> float:
    value = float(text)
    if not (math.isfinite(value) and value > 0):
        raise argparse.ArgumentTypeError("must be a finite number > 0")
    return value


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Benchmark speaker diarization (localflow.diarization).")
    p.add_argument("--manifest", type=Path, help='{"mixes": [{"id", "path"}]}; relative paths resolve '
                                                 "against the manifest dir")
    p.add_argument("--backend", choices=("sherpa-onnx", "mock"), default="sherpa-onnx")
    p.add_argument("--pipeline", choices=tuple(PIPELINES), default="chunked")
    p.add_argument("--chunk-seconds", type=_positive_float, default=30.0,
                   help="chunk length for --pipeline chunked (Config.meeting_chunk_seconds default)")
    p.add_argument("--threshold", type=float, default=None,
                   help="sherpa in-chunk clustering threshold (default sherpa.DEFAULT_THRESHOLD)")
    p.add_argument("--cluster-threshold", type=float, default=None,
                   help="cross-chunk clustering threshold (default collector.DEFAULT_CLUSTER_THRESHOLD)")
    p.add_argument("--max-speakers", type=_non_negative_int, default=0,
                   help="0 = collector.HARD_MAX_SPEAKERS")
    p.add_argument("--num-threads", type=_positive_int, default=1)
    p.add_argument("--runs", type=_positive_int, default=3)
    p.add_argument("--warmup", type=_non_negative_int, default=1)
    p.add_argument("--json", type=Path)
    p.add_argument("--table", type=Path)
    p.add_argument("--probe", action="store_true", help="load the backend only; exit 0 or 3")
    return p


def main(argv: list[str] | None = None, *, loader: Loader | None = None) -> int:
    args = build_parser().parse_args(argv)
    # Taken before the loader and before anything from localflow.diarization is imported,
    # so the delta includes the runtime import.
    rss_before = rss_bytes()
    loader = loader or _default_loader
    _resolve_defaults(args)  # the loader needs --threshold; never imports sherpa_onnx
    try:
        if args.probe:
            loader(args)
            print(json.dumps({"available": True, "backend": args.backend}))
            return 0
        if args.manifest is None:
            print("error: --manifest is required unless --probe is given", file=sys.stderr)
            return 2
        report = run_manifest(args, loader, rss_before)
    except Unavailable as exc:
        print(f"unavailable: {exc}", file=sys.stderr)
        return EXIT_UNAVAILABLE
    except (OSError, ValueError, TypeError, EOFError) as exc:  # bad manifest / unreadable WAV
        print(f"error: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
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
