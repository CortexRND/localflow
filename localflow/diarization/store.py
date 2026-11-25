"""Per-meeting speaker sidecars (design §5.5).

One JSON file per saved diarized meeting under ``~/.localflow/meetings``:
speaker identities plus segment timings and ids, never transcript text.
Numpy-free and independent of the meetings/server modules.
"""

from __future__ import annotations

import copy
import json
import logging
import math
import os
import re
import tempfile
import threading
from collections.abc import Callable, Iterable, Mapping
from pathlib import Path

log = logging.getLogger("localflow.diarization")

SCHEMA_VERSION = 1
CHANNEL_LABELS = {"me": "Me", "others": "Others"}
_ID_RE = re.compile(r"[0-9a-f]{32}")
_PUBLIC_KEYS = ("id", "label", "name", "source", "seconds")
_LOCK = threading.RLock()
_warned: set[tuple[str, str]] = set()


def default_root() -> Path:
    """``~/.localflow/meetings``, resolved at call time."""
    return Path.home() / ".localflow" / "meetings"


def valid_meeting_id(meeting_id: object) -> bool:
    """True for a 32-char lowercase hex id (``uuid4().hex``)."""
    return isinstance(meeting_id, str) and _ID_RE.fullmatch(meeting_id) is not None


def stamp(seconds: float) -> str:
    """MM:SS exactly like ``Segment.stamp``."""
    m, s = divmod(int(seconds), 60)
    return f"{m:02d}:{s:02d}"


# ---------------------------------------------------------------- records ---

def build_record(*, meeting_id: str, log_path: str, notes_path: str, started_at: str,
                 embedder: str | None, status: str,
                 speakers: Iterable[Mapping], segments: Iterable) -> dict:
    """A sidecar record from stop-payload speakers and saved segments."""
    return {
        "schema_version": SCHEMA_VERSION,
        "meeting_id": meeting_id,
        "log_path": log_path,
        "notes_path": notes_path,
        "started_at": started_at,
        "embedder": embedder,
        "status": status,
        "speakers": [
            {
                "id": sp["id"],
                "source": sp["source"],
                "label": sp["label"],
                "name": sp.get("name"),
                "voiceprint_id": sp.get("voiceprint_id"),
                "seconds": float(sp["seconds"]),
                "centroid": None,
            }
            for sp in speakers
        ],
        "segments": [
            {
                "start": float(seg.offset_seconds),
                "end": None if seg.end_seconds is None else float(seg.end_seconds),
                "source": str(seg.source),
                "speaker_id": seg.speaker_id,
            }
            for seg in segments
        ],
    }


def public_speakers(record: Mapping) -> list[dict]:
    """Speakers without voiceprint data or any private field."""
    return [{k: sp.get(k) for k in _PUBLIC_KEYS} for sp in record["speakers"]]


def find_speaker(record: Mapping, speaker_id: object) -> dict | None:
    for sp in record["speakers"]:
        if sp["id"] == speaker_id:
            return sp
    return None


def _identity(labels: Mapping[str, str], seg: Mapping) -> tuple[str, str] | None:
    sid = seg.get("speaker_id")
    if isinstance(sid, str):
        if sid in labels:
            return f"id:{sid}", labels[sid]
        return None
    source = seg.get("source")
    if source == "me":
        return "me", CHANNEL_LABELS["me"]
    return f"ch:{source}", CHANNEL_LABELS.get(source, source)


def _labels(record: Mapping) -> dict[str, str]:
    return {sp["id"]: sp["label"] for sp in record["speakers"]}


def frontmatter_speakers(record: Mapping) -> list[str]:
    """The ``speakers:`` frontmatter list: one label per identity, first appearance."""
    known = _labels(record)
    labels: dict[str, str] = {}
    for seg in record["segments"]:
        identity = _identity(known, seg)
        if identity is None or identity[0].startswith("ch:"):
            continue
        labels.setdefault(identity[0], identity[1])
    return list(labels.values())


def speaker_lines(record: Mapping, speaker_id: str) -> dict[str, list[bool]]:
    """Per stamp, one flag per transcript line rendered with this speaker's
    current label, in segment order: True where the line is this speaker's.

    Only stamps with at least one line of this speaker are included.
    """
    speaker = find_speaker(record, speaker_id)
    if speaker is None:
        return {}
    known = _labels(record)
    label = speaker["label"]
    flags: dict[str, list[bool]] = {}
    for s in record["segments"]:
        identity = _identity(known, s)
        if identity is not None and identity[1] == label:
            flags.setdefault(stamp(s["start"]), []).append(s.get("speaker_id") == speaker_id)
    return {k: v for k, v in flags.items() if any(v)}


def rename_speaker(record: Mapping, speaker_id: str, name: str) -> dict:
    """A copy with that speaker's name and label set to ``name``."""
    new = copy.deepcopy(dict(record))
    speaker = find_speaker(new, speaker_id)
    if speaker is None:
        raise KeyError(speaker_id)
    speaker["name"] = name
    speaker["label"] = name
    return new


def merge_speakers(record: Mapping, from_id: str, into_id: str) -> dict:
    """A copy with ``from_id``'s segments and seconds moved to ``into_id``."""
    if from_id == into_id:
        raise ValueError("cannot merge a speaker into itself")
    new = copy.deepcopy(dict(record))
    frm, into = find_speaker(new, from_id), find_speaker(new, into_id)
    if frm is None or into is None:
        raise KeyError(from_id if frm is None else into_id)
    for seg in new["segments"]:
        if seg.get("speaker_id") == from_id:
            seg["speaker_id"] = into_id
    into["seconds"] = round(float(into["seconds"]) + float(frm["seconds"]), 1)
    new["speakers"] = [sp for sp in new["speakers"] if sp["id"] != from_id]
    return new


# ------------------------------------------------------------- validation ---

def _real(value: object) -> bool:
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value))


def _problem(record: object, meeting_id: str) -> str | None:
    if not isinstance(record, dict):
        return "shape"
    version = record.get("schema_version")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        return "shape"
    if version > SCHEMA_VERSION:
        return "newer schema"
    if record.get("meeting_id") != meeting_id or not isinstance(record.get("log_path"), str):
        return "shape"
    speakers = record.get("speakers")
    if not isinstance(speakers, list):
        return "shape"
    ids = set()
    for sp in speakers:
        if not isinstance(sp, dict):
            return "shape"
        if not all(isinstance(sp.get(k), str) for k in ("id", "label", "source")):
            return "shape"
        if not (sp.get("name") is None or isinstance(sp.get("name"), str)):
            return "shape"
        if not _real(sp.get("seconds")) or sp["id"] in ids:
            return "shape"
        ids.add(sp["id"])
    segments = record.get("segments")
    if not isinstance(segments, list):
        return "shape"
    for seg in segments:
        if not isinstance(seg, dict) or not _real(seg.get("start")):
            return "shape"
        if not (seg.get("end") is None or _real(seg.get("end"))):
            return "shape"
        if not isinstance(seg.get("source"), str):
            return "shape"
        if not (seg.get("speaker_id") is None or isinstance(seg.get("speaker_id"), str)):
            return "shape"
    return None


def _warn_once(meeting_id: str, reason: str) -> None:
    with _LOCK:
        if (meeting_id, reason) in _warned:
            return
        _warned.add((meeting_id, reason))
    log.warning("ignoring meeting speaker data %s: %s", meeting_id, reason)


# ------------------------------------------------------------------ store ---

class MeetingSpeakerStore:
    """Per-meeting speaker sidecars under ~/.localflow/meetings (design §5.5)."""

    def __init__(self, root: Path | str | None = None) -> None:
        self._root = root

    @property
    def root(self) -> Path:
        return Path(self._root) if self._root is not None else default_root()

    @property
    def lock(self) -> threading.RLock:
        """Serialises every read-modify-write of any sidecar."""
        return _LOCK

    def path(self, meeting_id: object) -> Path | None:
        if not valid_meeting_id(meeting_id):
            return None
        return self.root / f"{meeting_id}.json"

    def load(self, meeting_id: object) -> dict | None:
        """The stored record, or None when absent or unusable. Never raises."""
        target = self.path(meeting_id)
        if target is None:
            return None
        try:
            record = json.loads(target.read_bytes().decode("utf-8"))
        except FileNotFoundError:
            return None
        except (OSError, ValueError, RecursionError):
            _warn_once(meeting_id, "unreadable")
            return None
        reason = _problem(record, meeting_id)
        if reason is not None:
            _warn_once(meeting_id, reason)
            return None
        return record

    def save(self, record: Mapping) -> Path:
        """Write the record atomically (0600 file in a 0700 directory)."""
        meeting_id = record.get("meeting_id")
        if not valid_meeting_id(meeting_id):
            raise ValueError("invalid meeting id")
        with _LOCK:
            root = self.root
            target = root / f"{meeting_id}.json"
            root.mkdir(parents=True, exist_ok=True)
            if os.name != "nt":
                os.chmod(root, 0o700)
            data = json.dumps(record, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
            fd, tmp = tempfile.mkstemp(dir=root, prefix=f".{meeting_id}.", suffix=".tmp")
            try:
                with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as fh:
                    fh.write(data)
                    fh.flush()
                    os.fsync(fh.fileno())
                if os.name != "nt":
                    os.chmod(tmp, 0o600)
                os.replace(tmp, target)
            finally:
                if os.path.exists(tmp):
                    try:
                        os.unlink(tmp)
                    except OSError:
                        pass
        return target

    def update(self, meeting_id: str, change: Callable[[dict], dict]) -> dict | None:
        """Locked load → change → save; None (and no call) when absent."""
        with _LOCK:
            record = self.load(meeting_id)
            if record is None:
                return None
            new = change(record)
            self.save(new)
            return new
