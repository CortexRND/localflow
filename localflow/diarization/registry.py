"""Local voiceprint registry for opt-in speaker recognition (design §4, slice B9).

``~/.localflow/speakers/`` (0700) holds ``registry.json`` (names, ids, timestamps, model id;
never a vector) and one ``<voiceprint_id>.npy`` float32 unit centroid per voiceprint (0600),
each written atomically. Nothing here is consulted unless both ``speaker_diarization`` and
``speaker_recognition`` are on; the server enforces that gate before creating a registry.

Voiceprints are only ever compared with vectors from the same embedder (``model_id``) and
dimension; a voiceprint from another model is inert. Names are cleaned with
``localflow.meetings.clean_speaker_name`` on every write and on every read that can become a
label (imported lazily so importing this module stays light). Logs carry counts only.

Single writer: the LocalFlow server process. The CLI goes through its HTTP API, so the
in-process lock and the forget epoch below are sufficient.
"""

from __future__ import annotations

import io
import json
import logging
import math
import os
import re
import tempfile
import threading
import uuid
import zipfile
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path

import numpy as np

from . import models, staging
from .collector import _unit_vector
from .staging import default_root
from .store import MeetingSpeakerStore
from .types import Voiceprint

log = logging.getLogger("localflow.diarization")

SCHEMA_VERSION = 1
REGISTRY_FILE = "registry.json"
MAX_DIM = 4096
MAX_VECTOR_FILE_BYTES = 1024 * 1024
MAX_REGISTRY_FILE_BYTES = 4 * 1024 * 1024
MAX_EXPORT_BYTES = 64 * 1024 * 1024

# Cosine-similarity floor of the CI-only mock embedder. Real embedders carry theirs in
# models.DIAR_MODELS (provisional until B10). Unknown models never match.
MOCK_MATCH_THRESHOLD = 0.9
# The one real embedder, resolved from the catalog exactly as sherpa.py does (no sherpa import).
EMBEDDING_MODEL_ID = models.DIAR_MODELS[1].id
# config.diarization_backend -> embedder id it produces (no model load needed).
_BACKEND_EMBEDDERS = {"mock": "mock", "sherpa-onnx": EMBEDDING_MODEL_ID}

_ID_RE = re.compile(r"[0-9a-f]{32}")
_NPY_RE = re.compile(r"[0-9a-f]{32}\.npy")
_MODEL_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
# Temp files this module creates: "." + owned name + "." + random + ".tmp".
_TMP_RE = re.compile(r"\.(?:registry\.json|[0-9a-f]{32}\.npy)\.[^/\\]*\.tmp")

_LOCK = threading.RLock()
_epoch = 0
_warned: set[str] = set()


# -------------------------------------------------------------- errors ---

class RegistryError(RuntimeError):
    """A registry operation failed; ``str(exc)`` is safe to show (no names, no vectors)."""


class InvalidName(RegistryError):
    pass


class UnknownVoiceprint(RegistryError):
    pass


class AmbiguousName(RegistryError):
    pass


class BadVector(RegistryError):
    pass


class UnsupportedModel(RegistryError):
    pass


class RegistryUnreadable(RegistryError):
    pass


class EnrollmentInterrupted(RegistryError):
    """Voiceprints were forgotten while this enrollment was being prepared."""


class ErasureIncomplete(RegistryError):
    """Forget removed what it could, but some files still hold voice data: ``failed``
    meeting speaker files and ``audio_failed`` staged enrollment audio (voice-data) files."""

    def __init__(self, forgotten: int, failed: int, audio_failed: int = 0) -> None:
        parts = [f"{failed} meeting speaker file(s)"] if failed else []
        if audio_failed:
            parts.append(f"{audio_failed} voice-data file(s)")
        super().__init__(f"{' and '.join(parts)} could not be erased")
        self.forgotten = forgotten
        self.failed = failed
        self.audio_failed = audio_failed


# ------------------------------------------------------------- helpers ---


def forget_epoch() -> int:
    """Incremented by every forget. Read without the lock (a plain int read), so checking it
    never waits for a match or a forget in progress."""
    return _epoch


def match_threshold(model_id: object) -> float | None:
    """Cosine-similarity floor for a match, or None when the model has none (never match)."""
    if not isinstance(model_id, str):
        return None
    if model_id == "mock":
        return MOCK_MATCH_THRESHOLD
    for model in models.DIAR_MODELS:
        if model.id == model_id:
            return model.match_threshold
    return None


def embedder_id_for_backend(backend: object) -> str | None:
    """The embedder id ``diarization_backend`` produces, without loading anything."""
    return _BACKEND_EMBEDDERS.get(backend) if isinstance(backend, str) else None


def valid_voiceprint_id(value: object) -> bool:
    return isinstance(value, str) and _ID_RE.fullmatch(value) is not None


def clean_name(name: object) -> str | None:
    """``clean_speaker_name`` (the meeting label sanitizer), imported on first use."""
    from localflow.meetings import clean_speaker_name

    return clean_speaker_name(name)


def _vector(value) -> np.ndarray | None:
    """A float 1-D finite non-zero vector as a float32 unit vector, else None."""
    try:
        arr = np.asarray(value)
    except (TypeError, ValueError):
        return None
    if arr.dtype.kind not in "fiu" or arr.ndim != 1 or not 0 < arr.size <= MAX_DIM:
        return None
    return _unit_vector(arr)


def _now() -> str:
    return datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _private_dir(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    if os.name != "nt":
        os.chmod(root, 0o700)


def _atomic_write(root: Path, name: str, data: bytes) -> Path:
    """Write ``root/name`` via a 0600 temp file in ``root`` + fsync + os.replace."""
    _private_dir(root)
    target = root / name
    fd, tmp = tempfile.mkstemp(dir=root, prefix=f".{name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as fh:
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


def _npy_bytes(vector: np.ndarray) -> bytes:
    buf = io.BytesIO()
    np.save(buf, np.asarray(vector, dtype=np.float32), allow_pickle=False)
    return buf.getvalue()


def _npy_vector(data: bytes) -> np.ndarray | None:
    if len(data) > MAX_VECTOR_FILE_BYTES:
        return None
    try:
        return _vector(np.load(io.BytesIO(data), allow_pickle=False))
    except (ValueError, OSError, EOFError, TypeError):
        return None


def _warn_once(reason: str) -> None:
    with _LOCK:
        if reason in _warned:
            return
        _warned.add(reason)
    log.warning("ignoring the speaker registry: %s", reason)


def _entry(raw: object) -> Voiceprint | None:
    """A usable registry entry with its name re-cleaned, else None."""
    if not isinstance(raw, dict):
        return None
    vid, model_id, created = raw.get("id"), raw.get("model_id"), raw.get("created")
    samples = raw.get("samples")
    name = clean_name(raw.get("name"))
    if not (valid_voiceprint_id(vid) and name and isinstance(model_id, str)
            and isinstance(created, str)
            and isinstance(samples, int) and not isinstance(samples, bool) and samples > 0):
        return None
    return Voiceprint(vid, name, model_id, created, samples)


def public(vp: Voiceprint) -> dict:
    """The API shape of a voiceprint (design §5.6); never a vector."""
    return {"id": vp.id, "name": vp.name, "created": vp.created, "samples": vp.samples,
            "model_id": vp.model_id}


# ------------------------------------------------------------ registry ---

class FileSpeakerRegistry:
    """Voiceprints under ``~/.localflow/speakers`` (design §4, §5.1)."""

    def __init__(self, root: Path | str | None = None,
                 meetings: MeetingSpeakerStore | None = None) -> None:
        self._root = root
        self._meetings = meetings

    @property
    def root(self) -> Path:
        return Path(self._root) if self._root is not None else default_root()

    @property
    def meetings(self) -> MeetingSpeakerStore:
        return self._meetings if self._meetings is not None else MeetingSpeakerStore()

    @property
    def lock(self) -> threading.RLock:
        """Serialises every registry read-modify-write (re-entrant: hold it around a
        lookup + write to make the pair atomic)."""
        return _LOCK

    # -------------------------------------------------------------- read ---

    def _load_raw(self) -> list | None:
        """Raw entries; [] when absent; None when unreadable or written by a newer version."""
        path = self.root / REGISTRY_FILE
        try:
            if path.stat().st_size > MAX_REGISTRY_FILE_BYTES:
                _warn_once("too large")
                return None
            data = json.loads(path.read_bytes().decode("utf-8"))
        except FileNotFoundError:
            return []
        except (OSError, ValueError, RecursionError):
            _warn_once("unreadable")
            return None
        if not isinstance(data, dict):
            _warn_once("shape")
            return None
        version = data.get("schema_version")
        if not isinstance(version, int) or isinstance(version, bool) or version < 1:
            _warn_once("shape")
            return None
        if version > SCHEMA_VERSION:
            _warn_once("newer schema")
            return None
        entries = data.get("voiceprints")
        if not isinstance(entries, list):
            _warn_once("shape")
            return None
        return entries

    def _writable_raw(self) -> list:
        raw = self._load_raw()
        if raw is None:
            raise RegistryUnreadable("the speaker registry can't be read by this version")
        return raw

    def _save_raw(self, entries: list) -> None:
        data = json.dumps({"schema_version": SCHEMA_VERSION, "voiceprints": entries},
                          ensure_ascii=False, indent=2, allow_nan=False) + "\n"
        _atomic_write(self.root, REGISTRY_FILE, data.encode("utf-8"))

    def _read_vector(self, voiceprint_id: str) -> np.ndarray | None:
        if not valid_voiceprint_id(voiceprint_id):
            return None
        try:
            return _npy_vector((self.root / f"{voiceprint_id}.npy").read_bytes())
        except OSError:
            return None

    def _enrolled(self) -> list[Voiceprint]:
        """Usable voiceprints in enrollment (file) order."""
        with _LOCK:
            raw = self._load_raw() or []
        return [vp for vp in map(_entry, raw) if vp is not None]

    def list(self) -> list[Voiceprint]:
        """Usable voiceprints sorted by name (casefold), then id."""
        return sorted(self._enrolled(), key=lambda vp: (vp.name.casefold(), vp.id))

    def find(self, ref: object) -> Voiceprint:
        """By exact id, else by cleaned name (casefold). Raises UnknownVoiceprint, or
        AmbiguousName when several voiceprints share the name."""
        voiceprints = self.list()
        for vp in voiceprints:
            if vp.id == ref:
                return vp
        name = clean_name(ref)
        hits = [vp for vp in voiceprints if name and vp.name.casefold() == name.casefold()]
        if not hits:
            raise UnknownVoiceprint("unknown speaker")
        if len(hits) > 1:
            raise AmbiguousName(
                "more than one enrolled speaker has this name; use its id "
                "(lf speakers list --json)")
        return hits[0]

    def match(self, vector: np.ndarray, model_id: str) -> tuple[Voiceprint, float] | None:
        """Best same-model, same-dimension voiceprint at or above the model's threshold;
        ties go to the earlier enrollment."""
        threshold = match_threshold(model_id)
        query = _vector(vector)
        if threshold is None or query is None:
            return None
        with _LOCK:
            candidates = [(vp, self._read_vector(vp.id)) for vp in self._enrolled()
                          if vp.model_id == model_id]
        best: tuple[float, Voiceprint] | None = None
        for vp, ref in candidates:
            if ref is None or ref.shape != query.shape:
                continue
            score = float(np.dot(ref, query))
            if not math.isfinite(score) or score < threshold:
                continue
            if best is None or score > best[0]:
                best = (score, vp)
        return None if best is None else (best[1], best[0])

    # ------------------------------------------------------------- write ---

    def enroll(self, name: str, vectors: list[np.ndarray], model_id: str, *,
               expected_epoch: int | None = None) -> Voiceprint:
        """Store the normalised mean of ``vectors`` as a new voiceprint.

        ``expected_epoch``: the ``forget_epoch()`` read before the vectors were computed;
        a forget since then raises EnrollmentInterrupted instead of recreating voice data.
        """
        clean = clean_name(name)
        if not clean:
            raise InvalidName("invalid speaker name")
        if not (isinstance(model_id, str) and _MODEL_RE.fullmatch(model_id)
                and match_threshold(model_id) is not None):
            raise UnsupportedModel("voice data from this model can't be enrolled")
        units = [_vector(v) for v in (vectors or [])]
        if not units or any(u is None for u in units):
            raise BadVector("no usable voice sample")
        if len({u.shape for u in units}) != 1:
            raise BadVector("voice samples have different sizes")
        centroid = _vector(np.mean(np.stack(units), axis=0))
        if centroid is None:
            raise BadVector("no usable voice sample")
        vp = Voiceprint(uuid.uuid4().hex, clean, model_id, _now(), len(units))
        with _LOCK:
            if expected_epoch is not None and expected_epoch != _epoch:
                raise EnrollmentInterrupted(
                    "enrolled speakers were forgotten during this enrollment; try again")
            raw = self._writable_raw()
            # Vector first: a crash leaves an orphan .npy that nothing reads and
            # forget --all removes, never an entry without its vector.
            _atomic_write(self.root, f"{vp.id}.npy", _npy_bytes(centroid))
            self._save_raw([*raw, {"id": vp.id, "name": vp.name, "model_id": vp.model_id,
                                   "created": vp.created, "samples": vp.samples}])
        log.info("speaker enrolled (%d samples)", vp.samples)
        return vp

    def rename(self, voiceprint_id: str, name: str) -> None:
        """Rename for later meetings; past transcripts and sidecars are not rewritten."""
        clean = clean_name(name)
        if not clean:
            raise InvalidName("invalid speaker name")
        if not valid_voiceprint_id(voiceprint_id):
            raise UnknownVoiceprint("unknown speaker")
        with _LOCK:
            raw = self._writable_raw()
            for item in raw:
                if isinstance(item, dict) and item.get("id") == voiceprint_id \
                        and _entry(item) is not None:
                    item["name"] = clean
                    self._save_raw(raw)
                    return
        raise UnknownVoiceprint("unknown speaker")

    def forget(self, voiceprint_id: str | None = None) -> int:
        """Forget one voiceprint (or all with None); returns how many were removed.

        Both modes first null every stored centroid in every meeting sidecar (centroids of
        anonymous speakers included) and the matching ``voiceprint_id`` links (all links for
        None), then delete the voiceprint files. Names already shown in transcripts and
        sidecar labels are user text and stay. Raises ErasureIncomplete (after deleting
        everything it could) when some sidecar could not be read or rewritten.
        """
        global _epoch
        single = voiceprint_id is not None
        if single and not valid_voiceprint_id(voiceprint_id):
            raise UnknownVoiceprint("unknown speaker")
        store = self.meetings
        with store.lock, _LOCK:
            # Bump first: an in-flight finalize or enrollment that read the epoch earlier
            # now discards its voice data even if something below fails.
            _epoch += 1
            if single:
                raw = self._writable_raw()
                keep = [item for item in raw
                        if not (isinstance(item, dict) and item.get("id") == voiceprint_id)]
                if len(keep) == len(raw):
                    raise UnknownVoiceprint("unknown speaker")
            else:
                raw = self._load_raw() or []
            changed, failed = store.strip_voiceprints(voiceprint_id)
            audio_failed = 0
            if single:
                self._unlink(f"{voiceprint_id}.npy")
                self._save_raw(keep)
                removed = 1
            else:
                removed = len(raw)
                # Staged enrollment audio first (copies a killed server left behind and one
                # in flight now, whose enrollment then fails on the epoch bumped above), so a
                # failure on the registry files below can't skip it.
                audio_failed = staging.sweep(self.root)[1]
                self._remove_owned_files()
        log.info("speakers forgotten: %d voiceprints, %d meeting entries", removed, changed)
        if failed or audio_failed:
            raise ErasureIncomplete(removed, failed, audio_failed)
        return removed

    @contextmanager
    def staged_audio(self, data: bytes, *,
                     expected_epoch: int | None = None) -> Iterator[str]:
        """A private seekable copy of ``data`` (``.enroll-<hex>.audio``, 0600, in this
        registry's 0700 folder) for the duration of the ``with`` block, then deleted.

        Created under the registry lock, refusing (EnrollmentInterrupted) when a forget ran
        since ``expected_epoch``: a forget --all either sees and deletes the copy or runs
        before it exists. The lock is not held while the caller decodes. A copy a crash
        leaves behind is removed by the next server start or forget --all.
        """
        name = staging.staged_name()
        path = self.root / name
        with _LOCK:
            if expected_epoch is not None and expected_epoch != _epoch:
                raise EnrollmentInterrupted(
                    "enrolled speakers were forgotten during this enrollment; try again")
            created = False
            try:
                if not staging.plain_dir_or_missing(self.root):  # symlink or not a folder
                    raise RegistryError("could not prepare the audio")
                _private_dir(self.root)
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                             | getattr(os, "O_BINARY", 0), 0o600)
                created = True  # from here on the file is ours to delete
                try:
                    with os.fdopen(fd, "wb") as fh:
                        fh.write(data)
                finally:
                    if os.name != "nt":
                        os.chmod(path, 0o600)
            except OSError:
                if created:
                    self._discard_staged(path)
                raise RegistryError("could not prepare the audio") from None
        try:
            yield str(path)
        finally:
            if not self._discard_staged(path):
                log.warning("could not delete staged enrollment audio")
                raise RegistryError("temporary audio could not be deleted")

    @staticmethod
    def _discard_staged(path: Path) -> bool:
        try:
            path.unlink()
        except FileNotFoundError:
            pass  # already swept by a forget --all
        except OSError:
            return False
        return True

    def _unlink(self, name: str) -> None:
        try:
            (self.root / name).unlink()
        except FileNotFoundError:
            pass

    def _remove_owned_files(self) -> None:
        """Delete registry.json, every <id>.npy (orphans too) and this module's temp files;
        anything else in the directory is left alone."""
        try:
            names = os.listdir(self.root)
        except FileNotFoundError:
            return
        for name in names:
            if name == REGISTRY_FILE or _NPY_RE.fullmatch(name) or _TMP_RE.fullmatch(name):
                path = self.root / name
                if path.is_symlink() or path.is_file():
                    self._unlink(name)

    # ------------------------------------------------------------ export ---

    def export_zip(self) -> bytes:
        """A ZIP of ``registry.json`` + ``<id>.npy`` for every usable voiceprint, in memory."""
        with _LOCK:
            items = [(vp, self._read_vector(vp.id)) for vp in self.list()]
        items = [(vp, vec) for vp, vec in items if vec is not None]
        meta = {"schema_version": SCHEMA_VERSION,
                "voiceprints": [public(vp) for vp, _ in items]}
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
            _zip_add(zf, REGISTRY_FILE, (json.dumps(meta, ensure_ascii=False, indent=2)
                                         + "\n").encode("utf-8"))
            for vp, vec in items:
                _zip_add(zf, f"{vp.id}.npy", _npy_bytes(vec))
        log.info("speaker registry exported: %d voiceprints", len(items))
        return buf.getvalue()

    def export(self, target: Path) -> Path:
        """Write the export into the explicit local folder ``target`` (new or empty)."""
        return write_bundle(self.export_zip(), target)[0]


def _zip_add(zf: zipfile.ZipFile, name: str, data: bytes) -> None:
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = 0o600 << 16
    zf.writestr(info, data)


def _read_bundle(data: bytes) -> tuple[dict, dict[str, bytes]]:
    """Validated (registry dict, {npy name: bytes}) from export ZIP bytes."""
    if not isinstance(data, (bytes, bytearray)) or len(data) > MAX_EXPORT_BYTES:
        raise RegistryError("not a speaker export")
    try:
        zf = zipfile.ZipFile(io.BytesIO(data))
        members = zf.infolist()
    except (zipfile.BadZipFile, ValueError, OSError):
        raise RegistryError("not a speaker export") from None
    files: dict[str, bytes] = {}
    meta = None
    total = 0
    with zf:
        for info in members:
            name = info.filename
            limit = MAX_REGISTRY_FILE_BYTES if name == REGISTRY_FILE else MAX_VECTOR_FILE_BYTES
            if info.is_dir() or not (name == REGISTRY_FILE or _NPY_RE.fullmatch(name)) \
                    or name in files or info.file_size > limit:
                raise RegistryError("the speaker export contains an unexpected file")
            total += info.file_size
            if total > MAX_EXPORT_BYTES:
                raise RegistryError("the speaker export is too large")
            try:
                with zf.open(info) as fh:
                    blob = fh.read(limit + 1)
            except (zipfile.BadZipFile, ValueError, OSError, EOFError):
                raise RegistryError("not a speaker export") from None
            if len(blob) > limit:
                raise RegistryError("the speaker export contains an unexpected file")
            if name == REGISTRY_FILE:
                try:
                    meta = json.loads(blob.decode("utf-8"))
                except (ValueError, RecursionError):
                    raise RegistryError("not a speaker export") from None
            else:
                if _npy_vector(blob) is None:
                    raise RegistryError("the speaker export contains an unusable vector")
                files[name] = blob
    if not (isinstance(meta, dict) and meta.get("schema_version") == SCHEMA_VERSION
            and isinstance(meta.get("voiceprints"), list)):
        raise RegistryError("not a speaker export")
    entries = []
    for raw in meta["voiceprints"]:
        vp = _entry(raw)
        if vp is None or f"{vp.id}.npy" not in files:
            raise RegistryError("the speaker export is incomplete")
        entries.append(public(vp))
    if len(files) != len(entries):
        raise RegistryError("the speaker export contains an unexpected file")
    return {"schema_version": SCHEMA_VERSION, "voiceprints": entries}, files


def write_bundle(data: bytes, target: Path | str) -> tuple[Path, int]:
    """Validate an export ZIP and write its ``registry.json`` + ``<id>.npy`` files into
    ``target`` (created 0700 if missing; must otherwise be an empty folder; files 0600,
    atomic). Nothing is written unless the whole bundle is valid. Returns (dir, count)."""
    meta, files = _read_bundle(data)
    target = Path(target).expanduser()
    if (target.exists() or target.is_symlink()) and (
            not target.is_dir() or any(target.iterdir())):
        raise RegistryError("the export folder must be new or empty")
    for name, blob in sorted(files.items()):
        _atomic_write(target, name, blob)
    _atomic_write(target, REGISTRY_FILE, (json.dumps(meta, ensure_ascii=False, indent=2)
                                          + "\n").encode("utf-8"))
    return target, len(meta["voiceprints"])
