"""Multi-speaker mixes for the ``diarization`` suite (stdlib only).

``diarization.json`` pins LibriSpeech clips (same shape as ``stt.json``, so
:mod:`.download` fetches, verifies and caches them unchanged) plus ``mixes``:
ordered clip lists joined with ``gap_s`` of silence. Mixes are built per run in
a temp dir, never committed.

The reference turns of a mix are each clip's *speech regions* (a simple
energy VAD, constants below), shifted to the clip's start in the mix, not the
whole clip span: LibriSpeech clips have leading/trailing silence and pauses
that would otherwise count as missed speech for every backend.

``write_synthetic_mix`` writes the deterministic constant-tone mix the mock
backend decodes (``localflow.diarization.mock``), with exact turn times.
"""

from __future__ import annotations

import array
import itertools
import json
import math
import re
import sys
import wave
from pathlib import Path

from .download import load_stt_manifest

DEFAULT_DIARIZATION_MANIFEST_PATH = Path(__file__).resolve().parent / "diarization.json"
MIX_RATE = 16000
VAD_FRAME_S = 0.02
VAD_REL_DB = -30.0        # threshold relative to the clip's 95th-percentile frame RMS
VAD_ABS_FLOOR = 30.0      # int16 units
VAD_MIN_GAP_S = 0.5       # pauses shorter than this stay inside a turn (matches sherpa MIN_DURATION_OFF)
VAD_MIN_SPEECH_S = 0.1

_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")

#: Synthetic mix: (speaker, start s, end s); boundaries on the mock's 0.5 s grid.
#: The C 25-33 turn straddles the 30 s chunk boundary on purpose.
SYNTHETIC_TURNS = (
    ("A", 0.0, 4.0), ("B", 5.0, 9.0), ("C", 10.0, 14.0), ("A", 15.0, 19.0),
    ("B", 20.0, 24.0), ("C", 25.0, 33.0), ("A", 34.0, 38.0),
)
SYNTHETIC_SPEAKERS = ("A", "B", "C")


def vad_params() -> dict:
    return {
        "frame_s": VAD_FRAME_S,
        "rel_db": VAD_REL_DB,
        "abs_floor": VAD_ABS_FLOOR,
        "min_gap_s": VAD_MIN_GAP_S,
        "min_speech_s": VAD_MIN_SPEECH_S,
    }


# --------------------------------------------------------------------------
# Manifest
# --------------------------------------------------------------------------


def _is_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def load_diarization_manifest(path: Path = DEFAULT_DIARIZATION_MANIFEST_PATH) -> dict:
    """Read and check the diarization corpus manifest; violations raise ``ValueError``."""
    data = load_stt_manifest(path)
    speaker_of: dict[str, str] = {}
    smoke_of: dict[str, bool] = {}
    for clip in data["clips"]:
        speaker = clip.get("speaker")
        if not isinstance(speaker, str) or not speaker.strip():
            raise ValueError(f"{path}: clip {clip['id']}: speaker must be a non-empty string")
        speaker_of[clip["id"]] = speaker
        smoke_of[clip["id"]] = bool(clip.get("smoke"))
    mixes = data.get("mixes")
    if not isinstance(mixes, list) or not mixes:
        raise ValueError(f"{path}: manifest needs a non-empty 'mixes' list")
    seen: set[str] = set()
    for mix in mixes:
        mid = mix.get("id") if isinstance(mix, dict) else None
        if not isinstance(mid, str) or not _ID_RE.match(mid):
            raise ValueError(f"{path}: bad mix id {mid!r}")
        if mid in seen:
            raise ValueError(f"{path}: duplicate mix id {mid!r}")
        seen.add(mid)
        clips = mix.get("clips")
        if not isinstance(clips, list) or len(clips) < 2:
            raise ValueError(f"{path}: mix {mid}: clips must list at least 2 clip ids")
        for cid in clips:
            if not isinstance(cid, str) or cid not in speaker_of:
                raise ValueError(f"{path}: mix {mid}: unknown clip {cid!r}")
        if len(set(clips)) != len(clips):
            raise ValueError(f"{path}: mix {mid}: repeated clip id")
        for a, b in itertools.pairwise(clips):
            if speaker_of[a] == speaker_of[b]:
                raise ValueError(f"{path}: mix {mid}: consecutive clips {a} and {b} share a speaker")
        if len({speaker_of[c] for c in clips}) < 2:
            raise ValueError(f"{path}: mix {mid}: needs at least 2 distinct speakers")
        gap = mix.get("gap_s")
        if not _is_number(gap) or not 0 <= gap <= 5:
            raise ValueError(f"{path}: mix {mid}: gap_s must be a number in [0, 5]")
        if not isinstance(mix.get("smoke"), bool):
            raise ValueError(f"{path}: mix {mid}: smoke must be a bool")  # noqa: TRY004 - manifest errors are ValueError
        if mix["smoke"]:
            for cid in clips:
                if not smoke_of[cid]:
                    raise ValueError(f"{path}: mix {mid}: clip {cid} is in a smoke mix but not smoke")
    return data


# --------------------------------------------------------------------------
# PCM
# --------------------------------------------------------------------------


def read_pcm16(path: Path) -> tuple[array.array, int]:
    """Mono 16-bit PCM WAV samples as ``array('h')`` and the sample rate."""
    with wave.open(str(path), "rb") as wav:
        channels, width, rate = wav.getnchannels(), wav.getsampwidth(), wav.getframerate()
        raw = wav.readframes(wav.getnframes())
    if channels != 1 or width != 2:
        raise ValueError(f"{path}: expected mono 16-bit PCM, got {channels} channel(s), {8 * width}-bit")
    samples = array.array("h")
    samples.frombytes(raw[: len(raw) - len(raw) % 2])
    if sys.byteorder == "big":
        samples.byteswap()
    return samples, rate


def _write_pcm16(out_wav: Path, samples: array.array, rate: int = MIX_RATE) -> None:
    data = array.array("h", samples)
    if sys.byteorder == "big":
        data.byteswap()
    out_wav = Path(out_wav)
    out_wav.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(out_wav), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(rate)
        wav.writeframes(data.tobytes())


def speech_regions(samples: array.array, rate: int) -> list[tuple[float, float]]:
    """Energy-VAD speech regions ``[(start, end)]`` in seconds within the clip."""
    total_s = round(len(samples) / rate, 3) if rate > 0 else 0.0
    step = max(1, round(VAD_FRAME_S * rate))
    rms = []
    for begin in range(0, len(samples), step):
        frame = samples[begin:begin + step]
        rms.append(math.sqrt(sum(x * x for x in frame) / len(frame)))
    if not rms:
        return [(0.0, total_s)]
    ordered = sorted(rms)
    ref = ordered[int(0.95 * (len(ordered) - 1))]
    threshold = max(ref * 10 ** (VAD_REL_DB / 20), VAD_ABS_FLOOR)
    runs: list[list[int]] = []  # [first frame, end frame) of speech
    for i, value in enumerate(rms):
        if value < threshold:
            continue
        if runs and runs[-1][1] == i:
            runs[-1][1] = i + 1
        else:
            runs.append([i, i + 1])
    frame_s = step / rate
    min_gap = round(VAD_MIN_GAP_S / frame_s)
    merged: list[list[int]] = []
    for run in runs:
        if merged and run[0] - merged[-1][1] < min_gap:
            merged[-1][1] = run[1]
        else:
            merged.append(list(run))
    regions = []
    for a, b in merged:
        start = a * step / rate
        end = min(b * step, len(samples)) / rate
        if end - start >= VAD_MIN_SPEECH_S - 1e-9:
            regions.append((round(start, 3), round(end, 3)))
    return regions or [(0.0, total_s)]


# --------------------------------------------------------------------------
# Mixes
# --------------------------------------------------------------------------


def build_mix(mix: dict, clips: dict[str, dict], out_wav: Path) -> dict:
    """Concatenate ``mix["clips"]`` with ``gap_s`` silence into a 16 kHz mono WAV.

    ``clips`` maps clip id -> ``{"path", "speaker", ...}`` (an ``ensure_stt_clips``
    entry merged with its manifest entry).
    """
    gap = array.array("h", bytes(2 * round(float(mix["gap_s"]) * MIX_RATE)))
    out = array.array("h")
    reference: list[dict] = []
    for i, cid in enumerate(mix["clips"]):
        clip = clips[cid]
        samples, rate = read_pcm16(Path(clip["path"]))
        if rate != MIX_RATE:
            raise ValueError(f"clip {cid}: sample rate {rate} Hz, expected {MIX_RATE}")
        if i:
            out.extend(gap)
        offset = len(out) / MIX_RATE
        for start, end in speech_regions(samples, rate):
            reference.append({"start": round(offset + start, 3), "end": round(offset + end, 3),
                              "speaker": str(clip["speaker"])})
        out.extend(samples)
    _write_pcm16(Path(out_wav), out)
    return {
        "id": mix["id"],
        "path": str(out_wav),
        "duration_s": round(len(out) / MIX_RATE, 3),
        "speakers": sorted({str(clips[c]["speaker"]) for c in mix["clips"]}),
        "reference": reference,
        "vad": vad_params(),
    }


def write_synthetic_mix(out_wav: Path) -> dict:
    """The mock-backend mix: one constant tone per speaker, zeros for silence."""
    duration = max(end for _, _, end in SYNTHETIC_TURNS)
    samples = array.array("h", bytes(2 * round(duration * MIX_RATE)))
    for speaker, start, end in SYNTHETIC_TURNS:
        level = round((0.1 + SYNTHETIC_SPEAKERS.index(speaker) * 0.001) * 32768)
        a, b = round(start * MIX_RATE), round(end * MIX_RATE)
        samples[a:b] = array.array("h", [level]) * (b - a)
    _write_pcm16(Path(out_wav), samples)
    return {
        "id": "synthetic-tones",
        "path": str(out_wav),
        "duration_s": round(duration, 3),
        "speakers": list(SYNTHETIC_SPEAKERS),
        "reference": [{"start": s, "end": e, "speaker": spk} for spk, s, e in SYNTHETIC_TURNS],
        "vad": vad_params(),
    }


if __name__ == "__main__":  # pragma: no cover - manual inspection
    print(json.dumps(load_diarization_manifest()["mixes"], indent=2))
