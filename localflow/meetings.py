"""Meeting detection, long-form transcription, and Obsidian note output.

Detection: polls CoreAudio's kAudioDevicePropertyDeviceIsRunningSomewhere on the
default input device — true when ANY process holds the mic. Sustained use (beyond
push-to-talk blips) is treated as a probable meeting; the user confirms in the UI.
"""

import bisect
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

from localflow.meeting_templates import MeetingTemplate, resolve_template
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

SOURCE_ME = "me"
SOURCE_OTHERS = "others"
_SOURCE_LABELS = {SOURCE_ME: "Me", SOURCE_OTHERS: "Others"}


@dataclass
class Segment:
    offset_seconds: float
    text: str
    source: str = SOURCE_ME  # "me" = mic, "others" = system/loopback audio

    @property
    def stamp(self) -> str:
        m, s = divmod(int(self.offset_seconds), 60)
        return f"{m:02d}:{s:02d}"

    @property
    def speaker(self) -> str:
        return _SOURCE_LABELS.get(self.source, self.source)


def has_speakers(segments: list[Segment]) -> bool:
    """True when any segment came from system audio, i.e. labels mean something."""
    return any(s.source != SOURCE_ME for s in segments)


def format_transcript(segments: list[Segment]) -> str:
    """Transcript text for the LLM: speaker-labelled lines once system audio is on."""
    if not has_speakers(segments):
        return " ".join(s.text for s in segments)
    return "\n".join(f"[{s.stamp}] {s.speaker}: {s.text}" for s in segments)


class _SourceBuffer:
    """Audio from one source, drained into chunks at the session sample rate."""

    def __init__(self, source: str):
        self.source = source
        self.consumed_seconds = 0.0
        self._chunks: list[np.ndarray] = []
        self._lock = threading.Lock()

    def append(self, audio: np.ndarray) -> None:
        with self._lock:
            self._chunks.append(audio)

    def take(self) -> np.ndarray:
        with self._lock:
            chunks, self._chunks = self._chunks, []
        if not chunks:
            return np.array([], dtype=np.float32)
        return np.concatenate(chunks)


class MeetingSession:
    """Continuous mic capture, transcribed in chunks on a worker thread.

    With a ``system_audio`` source (see ``localflow.system_audio``) the remote
    side is captured too and transcribed separately, so segments carry
    ``source="me"`` (mic) or ``source="others"`` (what the machine played).
    A system source that fails to open never blocks the meeting: the session
    falls back to mic-only and reports why in ``system_audio_error``.
    """

    def __init__(self, transcriber, transcribe_lock: threading.Lock,
                 sample_rate: int = 16000, chunk_seconds: int = 30,
                 system_audio=None):
        self.transcriber = transcriber
        self.transcribe_lock = transcribe_lock
        self.sample_rate = sample_rate
        self.chunk_seconds = chunk_seconds
        self.system_audio = system_audio
        self.system_audio_error = ""
        self.segments: list[Segment] = []
        self.started_at: datetime | None = None
        self.active = False
        self._mic = _SourceBuffer(SOURCE_ME)
        self._others = _SourceBuffer(SOURCE_OTHERS)
        self._stream: sd.InputStream | None = None
        self._stop = threading.Event()
        self._worker: threading.Thread | None = None
        self._start_monotonic = 0.0

    @property
    def system_audio_status(self) -> str:
        """'on' / 'off' / 'error' — for the UI."""
        if self.system_audio is not None:
            return "on"
        return "error" if self.system_audio_error else "off"

    def start(self) -> None:
        self._stream = sd.InputStream(
            samplerate=self.sample_rate, channels=1, dtype="float32",
            callback=self._callback,
        )
        self._stream.start()
        if self.system_audio is not None:
            try:
                self.system_audio.start(self._others.append)
                log.info("system audio capture on (%s)", self.system_audio.label)
            except Exception as exc:  # noqa: BLE001 - any failure means mic-only
                self.system_audio_error = str(exc) or type(exc).__name__
                self.system_audio = None
                log.warning("system audio unavailable; mic only: %s", self.system_audio_error)
        self.started_at = datetime.now()
        self._start_monotonic = time.monotonic()
        self.active = True
        self._stop.clear()
        self._worker = threading.Thread(target=self._drain_loop, daemon=True)
        self._worker.start()
        log.info("meeting session started")

    def _callback(self, indata, frames, time_info, status) -> None:
        self._mic.append(indata[:, 0].copy())

    def _take_buffer(self) -> np.ndarray:
        return self._mic.take()

    def _transcribe_chunk(self, audio: np.ndarray, buffer: _SourceBuffer | None = None) -> None:
        buffer = buffer or self._mic
        offset = buffer.consumed_seconds
        buffer.consumed_seconds += len(audio) / self.sample_rate
        # Skip near-silent chunks — whisper hallucinates on silence.
        if len(audio) < self.sample_rate or float(np.abs(audio).max()) < 0.01:
            return
        with self.transcribe_lock:
            text = self.transcriber.transcribe(audio)
        text = (text or "").strip()
        if text:
            # insort, not append+sort: list.sort() empties the list while it
            # runs, and /meeting/status reads segments from another thread.
            bisect.insort(
                self.segments, Segment(offset, text, buffer.source),
                key=lambda seg: seg.offset_seconds,
            )
            log.info("segment @%.0fs (%s): %d chars", offset, buffer.source, len(text))

    def _drain_once(self) -> None:
        for buffer in (self._mic, self._others):
            try:
                self._transcribe_chunk(buffer.take(), buffer)
            except Exception:
                log.exception("chunk transcription failed (%s)", buffer.source)

    def _drain_loop(self) -> None:
        while not self._stop.wait(self.chunk_seconds):
            self._drain_once()

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
        if self.system_audio is not None:
            try:
                self.system_audio.stop()
            except Exception:
                log.exception("stopping system audio failed")
        self.active = False
        if self._worker is not None:
            self._worker.join(timeout=120)
        self._drain_once()
        log.info("meeting session stopped: %d segments", len(self.segments))
        return self.segments


# --------------------------------------------------------------- summarizer ---

_SPEAKER_NOTE = (
    'Transcript lines may be prefixed with a speaker: "Me" is the user (their '
    'microphone), "Others" is everyone else on the call (system audio).'
)

_NOTES_PROMPT = """You are a meeting-notes writer. From the transcript below, write concise meeting notes in Markdown with exactly these sections:

{sections}

{instructions}Only use information present in the transcript. No preamble, no commentary — output the Markdown sections only.
{speaker_note}

Transcript:
"""

_MERGE_PROMPT = """The following are meeting notes from consecutive parts of one meeting. Merge them into a single set of notes with sections: {section_list}. Deduplicate. Output Markdown only.

"""

# The first sentence is the product contract for AI-enhanced notes; keep it verbatim.
ENHANCE_INSTRUCTION = (
    "The user wrote these rough notes during the meeting — expand and enhance "
    "them using the transcript, keeping the user's structure and emphasis."
)

_ENHANCE_PROMPT = """You are a meeting-notes writer. {enhance}

- Keep every point the user wrote, in their order and under their headings. Flesh each one out with specifics from the transcript (names, numbers, owners, dates).
- What the user wrote down mattered to them: give those points the most space.
- Correct a detail only when the transcript clearly contradicts it.
- After the user's notes, add only those of these sections that the user's notes don't already cover:

{sections}

{instructions}Only use information present in the transcript or the user's notes. No preamble, no commentary — output Markdown only.
{speaker_note}

User's notes:
<<<
{user_notes}
>>>

Transcript:
"""

_ENHANCE_MERGE_PROMPT = """The following are enhanced meeting notes from consecutive parts of one meeting, each built on the same rough notes from the user. Merge them into one set of notes. {enhance} Keep the user's headings and order; then any of these sections that are not already covered: {section_list}. Deduplicate. Output Markdown only.

User's notes:
<<<
{user_notes}
>>>

Notes to merge:

"""


def _template_fields(template: MeetingTemplate) -> dict[str, str]:
    instructions = template.instructions.strip()
    return {
        "sections": template.sections_markdown(),
        "section_list": ", ".join(f"## {name}" for name in template.section_names()),
        "instructions": f"{instructions}\n\n" if instructions else "",
        "speaker_note": _SPEAKER_NOTE,
        "enhance": ENHANCE_INSTRUCTION,
    }


def build_notes_prompt(template: MeetingTemplate | None = None) -> str:
    """Prompt prefix for plain notes; the transcript chunk is appended."""
    return _NOTES_PROMPT.format(**_template_fields(template or resolve_template(None)))


def build_enhance_prompt(user_notes: str, template: MeetingTemplate | None = None) -> str:
    """Prompt prefix for AI-enhanced notes; the transcript chunk is appended."""
    fields = _template_fields(template or resolve_template(None))
    return _ENHANCE_PROMPT.format(user_notes=user_notes.strip(), **fields)


class MeetingSummarizer:
    def __init__(self, llm: LLMProvider, words_per_chunk: int = 2500):
        self.llm = llm
        self.words_per_chunk = words_per_chunk

    def _generate(self, prompt: str) -> str:
        return self.llm.complete("", prompt)

    def _chunks(self, text: str) -> list[str]:
        words = text.split()
        return [
            " ".join(words[i:i + self.words_per_chunk])
            for i in range(0, len(words), self.words_per_chunk)
        ]

    def summarize(self, transcript: str, user_notes: str = "",
                  template: MeetingTemplate | None = None) -> str:
        """Notes for one meeting.

        With ``user_notes`` (the in-meeting notepad) the LLM enhances the
        user's notes rather than writing its own; ``template`` picks the
        sections either way (default: the general template).
        """
        template = template or resolve_template(None)
        user_notes = (user_notes or "").strip()
        chunks = self._chunks(transcript)
        if not chunks:
            if user_notes:
                return user_notes + "\n"
            return "## Summary\n(No speech captured.)\n"
        fields = _template_fields(template)
        try:
            if user_notes:
                prefix = build_enhance_prompt(user_notes, template)
                merge = _ENHANCE_MERGE_PROMPT.format(user_notes=user_notes, **fields)
            else:
                prefix = build_notes_prompt(template)
                merge = _MERGE_PROMPT.format(**fields)
            parts = [self._generate(prefix + chunk) for chunk in chunks]
            if len(parts) == 1:
                return parts[0]
            return self._generate(merge + "\n\n---\n\n".join(parts))
        except Exception:
            log.exception("summarization failed; notes will hold raw transcript pointer")
            failed = "## Summary\n(LLM summarization failed — see the linked transcript log.)\n"
            if user_notes:
                failed += f"\n## My Notes\n\n{user_notes}\n"
            return failed


# ------------------------------------------------------------- transcript Q&A ---

_CHAT_PROMPT = """You answer questions about a meeting using only its transcript below. Be concise and specific. When it helps, cite the timestamp (e.g. [12:30]) where something was said. If the transcript does not contain the answer, say so plainly — never guess.
{speaker_note}

Transcript:
<<<
{transcript}
>>>

Question: {question}
"""

_CHAT_PART_PROMPT = """You answer questions about a meeting using only an excerpt of its transcript (part {index} of {total}). If this part contains nothing relevant to the question, reply exactly: {not_found}
Otherwise answer concisely from this part only, citing timestamps (e.g. [12:30]) when helpful.
{speaker_note}

Transcript excerpt:
<<<
{transcript}
>>>

Question: {question}
"""

_CHAT_COMBINE_PROMPT = """Partial answers to one question were drawn from consecutive parts of a meeting transcript. Combine them into a single concise answer. Keep timestamps. Do not add anything they don't say.

Question: {question}

Partial answers:

{answers}
"""

CHAT_NOT_FOUND = "NOT IN THIS PART"
CHAT_NO_ANSWER = "The transcript doesn't mention that."


def answer_question(llm: LLMProvider, transcript: str, question: str,
                    words_per_chunk: int = 4000) -> str:
    """Answer ``question`` from ``transcript``; long transcripts are map-reduced."""
    words = transcript.split()
    if not words:
        return CHAT_NO_ANSWER
    if len(words) <= words_per_chunk:
        return llm.complete("", _CHAT_PROMPT.format(
            transcript=transcript.strip(), question=question, speaker_note=_SPEAKER_NOTE,
        )).strip()
    # Split on line boundaries so a "[mm:ss] Speaker:" prefix stays with its text.
    parts: list[str] = []
    current: list[str] = []
    count = 0
    for line in transcript.splitlines():
        n = len(line.split())
        if current and count + n > words_per_chunk:
            parts.append("\n".join(current))
            current, count = [], 0
        current.append(line)
        count += n
    if current:
        parts.append("\n".join(current))
    answers = []
    for index, part in enumerate(parts, start=1):
        reply = llm.complete("", _CHAT_PART_PROMPT.format(
            index=index, total=len(parts), not_found=CHAT_NOT_FOUND,
            transcript=part, question=question, speaker_note=_SPEAKER_NOTE,
        )).strip()
        if reply and CHAT_NOT_FOUND not in reply.upper():
            answers.append(reply)
    if not answers:
        return CHAT_NO_ANSWER
    if len(answers) == 1:
        return answers[0]
    return llm.complete("", _CHAT_COMBINE_PROMPT.format(
        question=question, answers="\n\n---\n\n".join(answers),
    )).strip()


def read_transcript_log(path: Path) -> str:
    """The transcript body of a saved log, without frontmatter or the notes link."""
    text = Path(path).read_text(encoding="utf-8")
    if text.startswith("---\n"):
        end = text.find("\n---\n", 4)
        if end != -1:
            text = text[end + 5:]
    lines = [line for line in text.strip().splitlines() if not line.startswith("Notes: [[")]
    return "\n".join(lines).strip()


def latest_transcript_log(logs_root: Path) -> Path | None:
    """Most recently written ``* (transcript).md`` under ``logs_root``."""
    root = Path(logs_root).expanduser()
    if not root.is_dir():
        return None
    candidates = [p for p in root.rglob("* (transcript).md") if p.is_file()]
    return max(candidates, key=lambda p: p.stat().st_mtime, default=None)


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
              segments: list[Segment], user_notes: str = "",
              template: str = "") -> SavedNote:
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
            + (f"template: {_safe_title(template)}\n" if template else "")
            + "tags: [meeting, localflow]\n"
            "---\n\n"
        )
        notes_path.write_text(
            frontmatter
            + f"# {title}\n\n"
            + notes_md.strip() + "\n\n"
            + f"Raw transcript: [[{log_path.stem}]]\n"
        )
        if has_speakers(segments):
            lines = (f"**{s.stamp}** **{s.speaker}:** {s.text}" for s in segments)
        else:
            lines = (f"**{s.stamp}** {s.text}" for s in segments)
        transcript_body = "\n\n".join(lines)
        raw_notes = (
            f"## My raw notes\n\n{user_notes.strip()}\n\n" if user_notes.strip() else ""
        )
        log_path.write_text(
            frontmatter
            + f"# {title} — transcript\n\n"
            + (transcript_body or "(no speech captured)") + "\n\n"
            + raw_notes
            + f"Notes: [[{notes_path.stem}]]\n"
        )
        log.info("wrote %s and %s", notes_path, log_path)
        return SavedNote(notes_path, log_path)
