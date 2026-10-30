"""Meeting detection, long-form transcription, and Obsidian note output.

Detection: polls CoreAudio's kAudioDevicePropertyDeviceIsRunningSomewhere on the
default input device — true when ANY process holds the mic. Sustained use (beyond
push-to-talk blips) is treated as a probable meeting; the user confirms in the UI.
"""

import ctypes
import ctypes.util
import logging
import subprocess
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import numpy as np
import sounddevice as sd

from localflow.platform import current
from localflow.providers.base import LLMProvider

log = logging.getLogger("localflow.meetings")


# ---------------------------------------------------------------- CoreAudio ---

def _fourcc(code: str) -> int:
    return int.from_bytes(code.encode("ascii"), "big")


class _PropertyAddress(ctypes.Structure):
    _fields_ = [
        ("mSelector", ctypes.c_uint32),
        ("mScope", ctypes.c_uint32),
        ("mElement", ctypes.c_uint32),
    ]


_SYSTEM_OBJECT = 1
_DEFAULT_INPUT = _fourcc("dIn ")
_RUNNING_SOMEWHERE = _fourcc("goin")
_SCOPE_GLOBAL = _fourcc("glob")
_ELEMENT_MAIN = 0


class MicUsageProbe:
    """True/False: is the default input device in use by any process?"""

    def __init__(self):
        path = ctypes.util.find_library("CoreAudio")
        if path is None:
            raise OSError("CoreAudio not found")
        self._ca = ctypes.CDLL(path)
        self._ca.AudioObjectGetPropertyData.restype = ctypes.c_int32
        self._ca.AudioObjectGetPropertyData.argtypes = [
            ctypes.c_uint32,
            ctypes.POINTER(_PropertyAddress),
            ctypes.c_uint32,
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_uint32),
            ctypes.c_void_p,
        ]

    def _get_uint32(self, object_id: int, selector: int) -> int:
        addr = _PropertyAddress(selector, _SCOPE_GLOBAL, _ELEMENT_MAIN)
        value = ctypes.c_uint32(0)
        size = ctypes.c_uint32(ctypes.sizeof(value))
        status = self._ca.AudioObjectGetPropertyData(
            object_id, ctypes.byref(addr), 0, None,
            ctypes.byref(size), ctypes.byref(value),
        )
        if status != 0:
            raise OSError(f"AudioObjectGetPropertyData failed: {status}")
        return value.value

    def mic_in_use(self) -> bool:
        device = self._get_uint32(_SYSTEM_OBJECT, _DEFAULT_INPUT)
        if device == 0:
            return False
        return bool(self._get_uint32(device, _RUNNING_SOMEWHERE))


_PLATFORM_PROCESSES = [
    ("Zoom", ["pgrep", "-x", "zoom.us"]),
    ("Teams", ["pgrep", "-f", "Microsoft Teams"]),
    ("FaceTime", ["pgrep", "-x", "FaceTime"]),
    ("Webex", ["pgrep", "-f", "Webex"]),
    ("Slack", ["pgrep", "-x", "Slack"]),
    ("Discord", ["pgrep", "-x", "Discord"]),
]


def detect_platform() -> str:
    """Best-effort guess of the meeting app; browser meetings land in Other."""
    for name, cmd in _PLATFORM_PROCESSES:
        try:
            if subprocess.run(cmd, capture_output=True).returncode == 0:
                return name
        except Exception:
            continue
    return "Other"


def notify(title: str, message: str) -> None:
    current().notify(title, message)


class MeetingWatcher:
    """Background poller. Sustained mic use by another app => probable meeting.

    Push-to-talk blips stay under min_busy_seconds and never trigger. While a
    MeetingSession is active we hold the mic ourselves, so polling pauses.
    """

    def __init__(self, min_busy_seconds: int = 12, poll_seconds: float = 3.0,
                 on_detect: Callable[[str], None] | None = None):
        self.min_busy_seconds = min_busy_seconds
        self.poll_seconds = poll_seconds
        self.on_detect = on_detect
        self.detected = False
        self.platform = ""
        self.mic_busy = False
        self.error = ""
        self._busy_since: float | None = None
        self._notified = False
        self._pause = threading.Event()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        try:
            self._probe = MicUsageProbe()
        except OSError as exc:
            self.error = f"mic watcher unavailable: {exc}"
            log.error(self.error)
            return
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        log.info("meeting watcher started")

    def pause(self) -> None:
        self._pause.set()

    def resume(self) -> None:
        self._busy_since = None
        self.detected = False
        self._notified = False
        self._pause.clear()

    def dismiss(self) -> None:
        """User declined this detection; stay quiet until the mic goes idle."""
        self.detected = False

    def _run(self) -> None:
        while not self._stop.is_set():
            time.sleep(self.poll_seconds)
            if self._pause.is_set():
                continue
            try:
                busy = self._probe.mic_in_use()
            except OSError as exc:
                log.warning("mic probe failed: %s", exc)
                continue
            self.mic_busy = busy
            now = time.monotonic()
            if not busy:
                self._busy_since = None
                self.detected = False
                self._notified = False
                continue
            if self._busy_since is None:
                self._busy_since = now
            if now - self._busy_since >= self.min_busy_seconds and not self.detected:
                self.detected = True
                self.platform = detect_platform()
                log.info("meeting detected (platform=%s)", self.platform)
                if not self._notified:
                    self._notified = True
                    notify(
                        "localflow: meeting detected",
                        f"Mic in use ({self.platform}). Open the localflow UI to transcribe.",
                    )
                if self.on_detect is not None:
                    # A bad callback must never kill the watcher thread.
                    try:
                        self.on_detect(self.platform)
                    except Exception:
                        log.exception("on_detect callback failed")


# ------------------------------------------------------------------ session ---

@dataclass
class Segment:
    offset_seconds: float
    text: str

    @property
    def stamp(self) -> str:
        m, s = divmod(int(self.offset_seconds), 60)
        return f"{m:02d}:{s:02d}"


class MeetingSession:
    """Continuous mic capture, transcribed in chunks on a worker thread."""

    def __init__(self, transcriber, transcribe_lock: threading.Lock,
                 sample_rate: int = 16000, chunk_seconds: int = 30):
        self.transcriber = transcriber
        self.transcribe_lock = transcribe_lock
        self.sample_rate = sample_rate
        self.chunk_seconds = chunk_seconds
        self.segments: list[Segment] = []
        self.started_at: datetime | None = None
        self.active = False
        self._buffer: list[np.ndarray] = []
        self._buf_lock = threading.Lock()
        self._stream: sd.InputStream | None = None
        self._stop = threading.Event()
        self._worker: threading.Thread | None = None
        self._start_monotonic = 0.0
        self._consumed_seconds = 0.0

    def start(self) -> None:
        self._stream = sd.InputStream(
            samplerate=self.sample_rate, channels=1, dtype="float32",
            callback=self._callback,
        )
        self._stream.start()
        self.started_at = datetime.now()
        self._start_monotonic = time.monotonic()
        self.active = True
        self._stop.clear()
        self._worker = threading.Thread(target=self._drain_loop, daemon=True)
        self._worker.start()
        log.info("meeting session started")

    def _callback(self, indata, frames, time_info, status) -> None:
        with self._buf_lock:
            self._buffer.append(indata[:, 0].copy())

    def _take_buffer(self) -> np.ndarray:
        with self._buf_lock:
            chunks, self._buffer = self._buffer, []
        if not chunks:
            return np.array([], dtype=np.float32)
        return np.concatenate(chunks)

    def _transcribe_chunk(self, audio: np.ndarray) -> None:
        offset = self._consumed_seconds
        self._consumed_seconds += len(audio) / self.sample_rate
        # Skip near-silent chunks — whisper hallucinates on silence.
        if len(audio) < self.sample_rate or float(np.abs(audio).max()) < 0.01:
            return
        with self.transcribe_lock:
            text = self.transcriber.transcribe(audio)
        text = (text or "").strip()
        if text:
            self.segments.append(Segment(offset, text))
            log.info("segment @%.0fs: %d chars", offset, len(text))

    def _drain_loop(self) -> None:
        while not self._stop.wait(self.chunk_seconds):
            try:
                self._transcribe_chunk(self._take_buffer())
            except Exception:
                log.exception("chunk transcription failed")

    def elapsed_seconds(self) -> float:
        if not self.started_at:
            return 0.0
        return time.monotonic() - self._start_monotonic

    def stop(self) -> list[Segment]:
        self._stop.set()
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None
        self.active = False
        if self._worker is not None:
            self._worker.join(timeout=120)
        try:
            self._transcribe_chunk(self._take_buffer())
        except Exception:
            log.exception("final chunk transcription failed")
        log.info("meeting session stopped: %d segments", len(self.segments))
        return self.segments


# --------------------------------------------------------------- summarizer ---

_NOTES_PROMPT = """You are a meeting-notes writer. From the transcript below, write concise meeting notes in Markdown with exactly these sections:

## Summary
2-4 sentences on what the meeting was about.

## Key Points
Bulleted list of the substantive points discussed.

## Decisions
Bulleted list of decisions made. Write "None recorded." if none.

## Action Items
Bulleted list of tasks/follow-ups, with owner if mentioned. Write "None recorded." if none.

Only use information present in the transcript. No preamble, no commentary — output the Markdown sections only.

Transcript:
"""

_MERGE_PROMPT = """The following are meeting notes from consecutive parts of one meeting. Merge them into a single set of notes with sections: ## Summary, ## Key Points, ## Decisions, ## Action Items. Deduplicate. Output Markdown only.

"""


class MeetingSummarizer:
    def __init__(self, llm: LLMProvider, words_per_chunk: int = 2500):
        self.llm = llm
        self.words_per_chunk = words_per_chunk

    def _generate(self, prompt: str) -> str:
        return self.llm.complete("", prompt)

    def summarize(self, transcript: str) -> str:
        words = transcript.split()
        if not words:
            return "## Summary\n(No speech captured.)\n"
        chunks = [
            " ".join(words[i:i + self.words_per_chunk])
            for i in range(0, len(words), self.words_per_chunk)
        ]
        try:
            parts = [self._generate(_NOTES_PROMPT + chunk) for chunk in chunks]
            if len(parts) == 1:
                return parts[0]
            joined = "\n\n---\n\n".join(parts)
            return self._generate(_MERGE_PROMPT + joined)
        except Exception:
            log.exception("summarization failed; notes will hold raw transcript pointer")
            return "## Summary\n(LLM summarization failed — see the linked transcript log.)\n"


# ------------------------------------------------------------ obsidian sink ---

def _safe_title(title: str) -> str:
    cleaned = "".join(c for c in title if c.isalnum() or c in " -_").strip()
    return cleaned or "Meeting"


@dataclass
class SavedNote:
    notes_path: Path
    log_path: Path


class ObsidianWriter:
    """Writes notes to <vault>/<notes_folder>/<category>/ and the raw transcript
    to <vault>/<logs_folder>/<category>/, cross-linked with wikilinks."""

    def __init__(self, vault_path: str, notes_folder: str, logs_folder: str):
        self.vault = Path(vault_path).expanduser()
        self.notes_folder = notes_folder
        self.logs_folder = logs_folder

    def write(self, *, title: str, category: str, started_at: datetime,
              duration_seconds: float, notes_md: str,
              segments: list[Segment]) -> SavedNote:
        title = _safe_title(title)
        category = _safe_title(category)
        stamp = started_at.strftime("%Y-%m-%d %H%M")
        base = f"{stamp} {title}"
        notes_dir = self.vault / self.notes_folder / category
        logs_dir = self.vault / self.logs_folder / category
        notes_dir.mkdir(parents=True, exist_ok=True)
        logs_dir.mkdir(parents=True, exist_ok=True)

        notes_path = notes_dir / f"{base}.md"
        log_path = logs_dir / f"{base} (transcript).md"
        minutes = int(duration_seconds // 60)

        frontmatter = (
            "---\n"
            f"date: {started_at.strftime('%Y-%m-%dT%H:%M')}\n"
            f"platform: {category}\n"
            f"duration_minutes: {minutes}\n"
            "tags: [meeting, localflow]\n"
            "---\n\n"
        )
        notes_path.write_text(
            frontmatter
            + f"# {title}\n\n"
            + notes_md.strip() + "\n\n"
            + f"Raw transcript: [[{log_path.stem}]]\n"
        )
        transcript_body = "\n\n".join(f"**{s.stamp}** {s.text}" for s in segments)
        log_path.write_text(
            frontmatter
            + f"# {title} — transcript\n\n"
            + (transcript_body or "(no speech captured)") + "\n\n"
            + f"Notes: [[{notes_path.stem}]]\n"
        )
        log.info("wrote %s and %s", notes_path, log_path)
        return SavedNote(notes_path, log_path)
