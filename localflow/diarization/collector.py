"""Meeting-wide speaker diarization collector (design §5.5).

The session feeds every transcribed chunk to ``feed``, which only enqueues it; one daemon
worker runs the diarizer and embedder off the transcription lock and keeps per-turn
embeddings in RAM (never the audio). ``finalize`` clusters all embeddings of a source once at
the end of the meeting and relabels copies of the live segments as ``Speaker 1..N``. Every
failure path returns the original segments; nothing here writes files or opens sockets.

Recognition (slice B9) happens only when the server attached a ``registry`` (both speaker
flags on): finalize then also computes one unit centroid per labelled speaker and asks the
registry's ``match`` for a name. The registry is the only I/O and runs inside the finalize
watchdog. With no registry no centroid is computed and the registry is never called.

This module must not import ``localflow.meetings``, ``localflow.server``, ``sounddevice`` or
``sherpa_onnx``: segments are duck-typed dataclasses copied with ``dataclasses.replace``.
"""

from __future__ import annotations

import dataclasses
import logging
import queue
import threading
import time
from collections.abc import Sequence
from dataclasses import dataclass

import numpy as np

from .align import assign_speakers, merge_runs
from .cluster import cluster
from .types import SOURCE_ME, SOURCE_OTHERS, MeetingSpeaker, SpeakerTurn, TimedText

log = logging.getLogger("localflow.diarization")

DIAR_FINALIZE_TIMEOUT_S = 20  # read at call time, so tests can monkeypatch it
QUEUE_MAX_CHUNKS = 4
MIN_EMBED_TURN_S = 1.0
MAX_CONSECUTIVE_FAILURES = 3
DEFAULT_CLUSTER_THRESHOLD = 0.5  # cosine distance; real backends override per model (B7/B10)
HARD_MAX_SPEAKERS = 10

STATUS_OK, STATUS_CHANNEL, STATUS_FAILED, STATUS_TIMEOUT = "ok", "channel", "failed", "timeout"

_POLL_S = 0.2  # worker wake-up period while waiting for chunks or a stop signal
_STOP = object()  # queue sentinel: finish what is queued, then exit
_DISABLED_ERROR = "speaker diarization stopped after 3 consecutive failures"


@dataclass(frozen=True)
class DiarizationResult:
    status: str  # "ok" | "channel" | "failed" | "timeout"
    segments: list  # meeting Segment objects (duck-typed), always a NEW list
    speakers: list[MeetingSpeaker]  # [] unless status == "ok" and labels were applied
    # speaker id -> unit centroid; only filled when a registry was attached (recognition on)
    centroids: dict[str, np.ndarray] = dataclasses.field(default_factory=dict)


@dataclass
class _Record:
    """One processed chunk: its texts, turns and per-turn vectors; never the audio."""

    source: str
    offset: float
    timed: tuple[TimedText, ...]
    turns: list[SpeakerTurn]
    vectors: dict[int, np.ndarray]


def _unit_vector(vec) -> np.ndarray | None:
    """1-D finite L2-normalised float32 copy, or None when unusable."""
    try:
        arr = np.asarray(vec, dtype=np.float32)
    except (TypeError, ValueError):  # ragged or non-numeric output
        return None
    if arr.ndim != 1 or arr.size == 0 or not np.isfinite(arr).all():
        return None
    norm = float(np.linalg.norm(arr))
    if not np.isfinite(norm) or norm <= 0.0:
        return None
    return (arr / norm).astype(np.float32, copy=False)


def _clamp_speakers(max_speakers: int | None) -> int:
    if max_speakers is None:
        return HARD_MAX_SPEAKERS
    return max(1, min(HARD_MAX_SPEAKERS, int(max_speakers)))


class DiarizationCollector:
    """Collects chunk embeddings during a meeting; labels speakers at the end."""

    def __init__(self, diarizer, embedder, *, sample_rate: int = 16000,
                 diarize_mic: bool = False, max_speakers: int | None = None,
                 threshold: float = DEFAULT_CLUSTER_THRESHOLD, registry=None) -> None:
        self.diarizer = diarizer
        self.embedder = embedder
        self.registry = registry  # voiceprint matcher; None = recognition off
        self.sample_rate = int(sample_rate)
        self.diarize_mic = bool(diarize_mic)
        self.max_speakers = _clamp_speakers(max_speakers)
        self.threshold = float(threshold)
        self.disabled = False
        self.error = ""
        self._lock = threading.Lock()
        self._queue: queue.Queue = queue.Queue(maxsize=QUEUE_MAX_CHUNKS)
        self._worker: threading.Thread | None = None
        self._closed = False  # no more feeds (finalize or close began)
        self._abandon = threading.Event()  # timeout/close: skip queued work and exit
        self._stopping = threading.Event()  # finalize: exit once the queue is empty
        self._records: list[_Record] = []
        self._consecutive_failures = 0
        self._counts = dict.fromkeys(("offered", "dropped", "failed", "skipped", "processed"), 0)
        self._final_lock = threading.Lock()
        self._result: DiarizationResult | None = None

    # ------------------------------------------------------------------ feed ---

    def feed(self, source: str, offset: float, audio: np.ndarray,
             timed: Sequence[TimedText]) -> bool:
        """Enqueue one transcribed chunk; True only when enqueued. Never blocks or raises."""
        try:
            return self._feed(source, offset, audio, timed)
        except Exception:
            log.exception("diarization feed failed (%s)", source)
            return False

    def _feed(self, source, offset, audio, timed) -> bool:
        if self._closed:
            return False
        if source == SOURCE_ME:
            if not self.diarize_mic:
                return False
        elif source != SOURCE_OTHERS:
            return False
        timed = tuple(timed or ())
        if not any((t.text or "").strip() for t in timed):
            return False
        with self._lock:
            if self._closed:
                return False
            self._counts["offered"] += 1
            if self.disabled:
                self._counts["skipped"] += 1
                return False
            try:
                self._queue.put_nowait((source, float(offset), audio, timed))
            except queue.Full:
                self._counts["dropped"] += 1
                return False
            if self._worker is None:
                self._worker = threading.Thread(
                    target=self._run, name="localflow-diarization", daemon=True
                )
                self._worker.start()
        return True

    def stats(self) -> dict[str, int]:
        with self._lock:
            return dict(self._counts)

    # ---------------------------------------------------------------- worker ---

    def _run(self) -> None:
        while True:
            if self._abandon.is_set():
                return
            try:
                item = self._queue.get(timeout=_POLL_S)
            except queue.Empty:
                if self._stopping.is_set() or self._abandon.is_set():
                    return
                continue
            if item is _STOP:
                return
            if self._abandon.is_set():
                return
            self._process(*item)
            del item

    def _process(self, source: str, offset: float, audio, timed) -> None:
        with self._lock:
            if self.disabled:
                self._counts["skipped"] += 1
                return
        try:
            sr = self.sample_rate
            audio = np.asarray(audio)
            turns = list(self.diarizer.diarize(audio, sr))
            vectors: dict[int, np.ndarray] = {}
            for i, turn in enumerate(turns):
                if turn.end - turn.start < MIN_EMBED_TURN_S:
                    continue
                clip = audio[int(turn.start * sr):int(turn.end * sr)]
                vec = _unit_vector(self.embedder.embed(clip, sr))
                if vec is not None:
                    vectors[i] = vec
        except Exception:
            log.exception("diarization failed on a chunk (%s)", source)
            with self._lock:
                self._counts["failed"] += 1
                self._consecutive_failures += 1
                if self._consecutive_failures >= MAX_CONSECUTIVE_FAILURES and not self.disabled:
                    self.disabled = True
                    self.error = _DISABLED_ERROR
                    log.warning("speaker diarization disabled after %d consecutive failures",
                                self._consecutive_failures)
            return
        record = _Record(source, offset, tuple(timed), turns, vectors)
        with self._lock:
            if self._abandon.is_set():
                return
            self._records.append(record)
            self._counts["processed"] += 1
            self._consecutive_failures = 0

    # -------------------------------------------------------------- finalize ---

    def finalize(self, segments: list, *, timeout: float | None = None) -> DiarizationResult:
        """Relabelled copy of ``segments``; bounded by ``timeout`` and never raises."""
        try:
            with self._final_lock:
                if self._result is not None:
                    return self._result
                original = list(segments)
                if timeout is None:
                    timeout = DIAR_FINALIZE_TIMEOUT_S
                if self._abandon.is_set():  # closed before finalize
                    self._result = DiarizationResult(STATUS_FAILED, original, [])
                    return self._result
                box: dict = {}
                done = threading.Event()

                def work() -> None:
                    try:
                        box["result"] = self._finalize_work(original)
                    except Exception:
                        log.exception("speaker diarization finalize failed")
                        box["error"] = True
                    finally:
                        done.set()

                threading.Thread(
                    target=work, name="localflow-diarization-finalize", daemon=True
                ).start()
                if not done.wait(timeout):
                    self._abandon.set()
                    self._drain_queue()
                    log.warning("speaker diarization timed out after %.1fs; keeping channel labels",
                                float(timeout))
                    result = DiarizationResult(STATUS_TIMEOUT, list(original), [])
                elif "error" in box:
                    result = DiarizationResult(STATUS_FAILED, list(original), [])
                else:
                    result = box["result"]
                self._result = result
                self._release()
                return result
        except Exception:
            log.exception("speaker diarization finalize failed")
            # e.g. the finalize thread could not start: stop the worker, stay idempotent.
            self.close()
            if self._result is None:
                self._result = DiarizationResult(STATUS_FAILED, list(segments), [])
            return self._result

    def _release(self) -> None:
        """Drop embeddings once a result exists; the worker is told to stop."""
        with self._lock:
            self._closed = True
            self._records = []

    def _drain_queue(self) -> None:
        """Drop queued chunks, then queue _STOP so an idle worker wakes and exits now
        instead of after its next ``get`` timeout."""
        try:
            while True:
                self._queue.get_nowait()
        except queue.Empty:
            pass
        try:
            self._queue.put_nowait(_STOP)
        except queue.Full:
            pass  # the worker still sees _abandon within _POLL_S

    def _finalize_work(self, segments: list) -> DiarizationResult:
        started = time.monotonic()
        registry = self.registry
        with self._lock:
            self._closed = True
            worker = self._worker
        if worker is not None:
            self._stopping.set()
            try:
                self._queue.put_nowait(_STOP)
            except queue.Full:
                pass  # the worker sees _stopping once the queue is empty
            worker.join()
        if self._abandon.is_set():
            return DiarizationResult(STATUS_TIMEOUT, list(segments), [])
        with self._lock:
            counts = dict(self._counts)
            records = list(self._records)
        offered = counts["offered"]
        lost = counts["dropped"] + counts["failed"] + counts["skipped"]
        if offered == 0:
            return DiarizationResult(STATUS_OK, list(segments), [])
        if lost * 2 > offered:
            log.warning("speaker diarization lost %d of %d chunks; keeping channel labels",
                        lost, offered)
            return DiarizationResult(STATUS_CHANNEL, list(segments), [])

        sources = ([SOURCE_ME] if self.diarize_mic else []) + [SOURCE_OTHERS]
        # (source, offset) -> runs; seconds per (source, cluster)
        runs_by_chunk: dict[tuple[str, float], list] = {}
        seconds: dict[tuple[str, int], float] = {}
        centroid_of: dict[tuple[str, int], np.ndarray] = {}
        for source in sources:
            labelled = self._label_source([r for r in records if r.source == source],
                                          centroids=registry is not None)
            if labelled is None:
                continue
            chunk_runs, source_seconds, source_centroids = labelled
            for offset, runs in chunk_runs.items():
                runs_by_chunk[(source, offset)] = runs
            for label, secs in source_seconds.items():
                seconds[(source, label)] = secs
            for label, vec in source_centroids.items():
                centroid_of[(source, label)] = vec

        output: list = []
        replaced: list[tuple[object, str, int | None]] = []  # (segment, source, cluster)
        for seg in segments:
            runs = runs_by_chunk.get((seg.source, seg.offset_seconds))
            if runs is None or seg.speaker_id is not None:
                output.append(seg)
                replaced.append((seg, seg.source, None))
                continue
            for run in runs:
                new = dataclasses.replace(
                    seg, offset_seconds=run.start, text=run.text, end_seconds=run.end,
                    speaker_id=None, speaker_label=None,
                )
                output.append(new)
                replaced.append((new, seg.source, run.speaker))
        order = sorted(range(len(output)), key=lambda i: output[i].offset_seconds)

        ids: dict[tuple[str, int], str] = {}
        final: list = []
        for i in order:
            seg, source, label = replaced[i]
            if label is None:
                final.append(seg)
                continue
            key = (source, label)
            if key not in ids:
                ids[key] = f"S{len(ids) + 1}"
            n = ids[key][1:]
            final.append(dataclasses.replace(seg, speaker_id=f"S{n}", speaker_label=f"Speaker {n}"))
        speakers = [
            MeetingSpeaker(sid, source, f"Speaker {sid[1:]}", None, None,
                           float(seconds.get((source, label), 0.0)))
            for (source, label), sid in ids.items()
        ]
        centroids = {sid: centroid_of[key] for key, sid in ids.items() if key in centroid_of}
        recognised = 0
        if registry is not None and centroids:
            speakers, final, recognised = self._recognise(registry, speakers, final, centroids)
        log.info("speaker diarization: %d chunks offered, %d lost, %d speakers, "
                 "%d recognised (%.1fs)",
                 offered, lost, len(speakers), recognised, time.monotonic() - started)
        return DiarizationResult(STATUS_OK, final, speakers, centroids)

    def _recognise(self, registry, speakers: list[MeetingSpeaker], segments: list,
                   centroids: dict[str, np.ndarray]):
        """Name speakers whose centroid matches an enrolled voiceprint of this embedder.
        Any registry error stops matching; the rest stay anonymous."""
        model_id = getattr(self.embedder, "id", None)
        if not isinstance(model_id, str):
            return speakers, segments, 0
        names: dict[str, tuple[str, str]] = {}  # sid -> (name, voiceprint id)
        for sp in speakers:
            vec = centroids.get(sp.id)
            if vec is None:
                continue
            try:
                hit = registry.match(vec, model_id)
            except Exception as exc:  # noqa: BLE001 - recognition is optional
                log.warning("speaker recognition failed (%s); speakers stay anonymous",
                            type(exc).__name__)
                break
            if hit is None:
                continue
            vp = hit[0]
            name, vid = getattr(vp, "name", None), getattr(vp, "id", None)
            if isinstance(name, str) and name and isinstance(vid, str):
                names[sp.id] = (name, vid)
        if not names:
            return speakers, segments, 0
        speakers = [
            dataclasses.replace(sp, label=names[sp.id][0], name=names[sp.id][0],
                                voiceprint_id=names[sp.id][1]) if sp.id in names else sp
            for sp in speakers
        ]
        segments = [
            dataclasses.replace(seg, speaker_label=names[seg.speaker_id][0])
            if seg.speaker_id in names else seg
            for seg in segments
        ]
        return speakers, segments, len(names)

    def _label_source(self, records: list[_Record], *, centroids: bool = False):
        """({offset: runs}, {cluster: seconds}, {cluster: unit centroid}) when ≥2 speakers
        appear, else None. Centroids are only computed when asked (recognition on)."""
        keys = [(ri, ti) for ri, r in enumerate(records) for ti in sorted(r.vectors)]
        if not keys:
            return None
        labels = cluster([records[ri].vectors[ti] for ri, ti in keys],
                         threshold=self.threshold, max_speakers=self.max_speakers)
        label_of = dict(zip(keys, labels))
        chunk_runs: dict[float, list] = {}
        seconds: dict[int, float] = {}
        found: set[int] = set()
        for ri, record in enumerate(records):
            # Fallback label per local speaker: largest embedded duration, tie → smallest.
            weight: dict[int, dict[int, float]] = {}
            for ti, turn in enumerate(record.turns):
                if (ri, ti) in label_of:
                    per = weight.setdefault(turn.local_speaker, {})
                    lab = label_of[(ri, ti)]
                    per[lab] = per.get(lab, 0.0) + (turn.end - turn.start)
            fallback = {
                local: min(per, key=lambda lab, per=per: (-per[lab], lab))
                for local, per in weight.items()
            }
            global_turns: list[SpeakerTurn] = []
            for ti, turn in enumerate(record.turns):
                label = label_of.get((ri, ti), fallback.get(turn.local_speaker))
                if label is None:
                    # Unknown voice: a negative sentinel keeps the turn occupying its time,
                    # so a neighbour within GAP_TOLERANCE_S can't claim its text.
                    global_turns.append(
                        SpeakerTurn(turn.start, turn.end, -(turn.local_speaker + 1)))
                    continue
                global_turns.append(SpeakerTurn(turn.start, turn.end, label))
                seconds[label] = seconds.get(label, 0.0) + (turn.end - turn.start)
            assigned = [
                (t, None if speaker is None or speaker < 0 else speaker)
                for t, speaker in assign_speakers(list(record.timed), global_turns)
            ]
            runs = merge_runs(assigned, chunk_offset=record.offset)
            runs = [run for run in runs if run.text]
            found.update(run.speaker for run in runs if run.speaker is not None)
            chunk_runs[record.offset] = runs
        if len(found) < 2:
            return None
        means: dict[int, np.ndarray] = {}
        if centroids:
            groups: dict[int, list[np.ndarray]] = {}
            for key, lab in label_of.items():
                groups.setdefault(lab, []).append(records[key[0]].vectors[key[1]])
            for lab in found:
                vec = _unit_vector(np.mean(np.stack(groups[lab]), axis=0)) \
                    if groups.get(lab) else None
                if vec is not None:
                    means[lab] = vec
        return chunk_runs, seconds, means

    # ----------------------------------------------------------------- close ---

    def close(self) -> None:
        """Stop accepting and abandon queued work. Idempotent; never blocks or raises."""
        try:
            with self._lock:
                self._closed = True
            self._abandon.set()
            self._drain_queue()
            with self._lock:
                self._records = []
        except Exception:
            log.exception("closing speaker diarization failed")
