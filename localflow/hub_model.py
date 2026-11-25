"""AppKit-free logic behind the native hub pages.

Kept separate from hub_mac so the rules (dictionary validation, prompt review,
responsive sizing) can be unit-tested on any platform.
"""

import math
import os
import re
import unicodedata
from datetime import datetime
from pathlib import Path

from localflow.dictionary import DictionaryError
from localflow.dispatch import (
    APPROVE_FROM,
    REJECT_FROM,
    PromptTransitionError,
    approve_prompt,
    reject_prompt,
)
from localflow.settings_agent import safe_text
from localflow.text_wrap import wrap_lines
from localflow.ui_tokens import HUB_HEADER_OFFSET

# Width below which the Home stat cards wrap from four columns into two.
STAT_FOUR_COLUMN_MIN_CARD = 150
STAT_GAP = 12

# ------------------------------------------------------------------ sidebar ---

# Display labels only: routing (PAGES, hub_route, show_page) keeps the page ids.
SIDEBAR_LABELS = {"Models": "Models & keys"}
SIDEBAR_BUTTON_WIDTH = 168
# Room the borderless button cell keeps around its title (both sides).
SIDEBAR_TITLE_INSET = 16
SIDEBAR_TITLE_PREFIX = "  "


def sidebar_label(page: str) -> str:
    """What the sidebar button and the page title say for `page`."""
    return SIDEBAR_LABELS.get(page, page)


def sidebar_title(page: str, measure=None, width: int = SIDEBAR_BUTTON_WIDTH) -> str:
    """The sidebar button title; the plain page id if the label would not fit.

    `measure(text) -> points` is the real font measurement (13pt semibold) in
    the AppKit hub; without it the label is used as is.
    """
    title = SIDEBAR_TITLE_PREFIX + sidebar_label(page)
    if measure is not None and measure(title) > width - SIDEBAR_TITLE_INSET:
        return SIDEBAR_TITLE_PREFIX + page
    return title


# --------------------------------------------------------------------- home ---

HOME_DECORATION_WIDTH = 150
HOME_DECORATION_GAP = 8  # between the decoration and the stat cards below it


def dictation_state(config) -> dict:
    enabled = config.dictation_enabled
    return {
        "enabled": enabled,
        "label": f"Dictation: {'On' if enabled else 'Off'}",
        "action": "Turn Dictation Off" if enabled else "Turn Dictation On",
        "detail": (
            "Dictation is on — push-to-talk is ready"
            if enabled
            else "Dictation is off — push-to-talk is paused"
        ),
    }


def home_decoration_frame(width: int) -> tuple[int, int, int, int]:
    """The halftone's (x, y, w, h): the right end of the header band only,
    ending above the stat cards (which start at HUB_HEADER_OFFSET)."""
    return (
        width - HOME_DECORATION_WIDTH, 0, HOME_DECORATION_WIDTH,
        HUB_HEADER_OFFSET - HOME_DECORATION_GAP,
    )


def stat_layout(width: int, count: int = 4) -> tuple[int, int]:
    """(columns, card_width) for `count` stat cards across `width` points."""
    columns = count
    while columns > 1 and (width - STAT_GAP * (columns - 1)) // columns < STAT_FOUR_COLUMN_MIN_CARD:
        columns //= 2
    return columns, (width - STAT_GAP * (columns - 1)) // columns


def meeting_notes_height(viewport_height: int, top: int) -> int:
    """Height of the meeting notes box: roomy when tall, never below 110."""
    return int(max(110, min(220, viewport_height - top - 330)))


# ----------------------------------------------------------------- meetings ---

MEETING_SUBTITLE = (
    "Transcribe a meeting, jot notes alongside, and save both to your vault."
)
# The title field starts blank; this is drawn, never inserted as text.
MEETING_TITLE_PLACEHOLDER = "Named automatically when saved"
MEETING_NOTES_PLACEHOLDER = (
    "Type notes while you talk: headings and bullets work. When you stop, "
    "LocalFlow fills them in from the transcript."
)
MEETING_FIELD_LABEL_H = 18
MEETING_LABEL_GAP = 4
MEETING_FIELD_H = 32  # == ui_tokens.INPUT_HEIGHT, shared by field and picker
MEETING_SECTION_GAP = 14
MEETING_HEADING_H = 22
MEETING_NOTES_STATUS_W = 220
MEETING_ACTION_GAP = 12  # notes card -> error (when shown) -> Start button


def meeting_form_layout(
    width: int, top: int, notes_height: int, *, error_height: int = 0,
) -> dict:
    """Frames (x, y, w, h) of the Meetings form, from the subtitle down to the
    Start/Stop button.

    The title field and the template picker share one row: the same y and the
    same height, so they line up exactly. The autosave status sits on the
    "Your notes" heading row (no space is reserved for it under the card) and
    an error only takes room when there is one.
    """
    title_w = int((width - 20) * 0.58)
    template_x = title_w + 20
    form_top = top + 28
    field_y = form_top + MEETING_FIELD_LABEL_H + MEETING_LABEL_GAP
    heading_y = field_y + MEETING_FIELD_H + MEETING_SECTION_GAP
    notes_y = heading_y + MEETING_HEADING_H + MEETING_LABEL_GAP
    notes_bottom = notes_y + notes_height
    frames = {
        "subtitle": (0, top, width, 20),
        "title_label": (0, form_top, title_w, MEETING_FIELD_LABEL_H),
        "title_field": (0, field_y, title_w, MEETING_FIELD_H),
        "template_label": (template_x, form_top, width - template_x, MEETING_FIELD_LABEL_H),
        "template_field": (template_x, field_y, width - template_x, MEETING_FIELD_H),
        "notes_label": (0, heading_y, width - MEETING_NOTES_STATUS_W - 12, MEETING_HEADING_H),
        "notes_status": (
            width - MEETING_NOTES_STATUS_W, heading_y + 3, MEETING_NOTES_STATUS_W, 18,
        ),
        "notes": (0, notes_y, width, notes_height),
        "retry": (0, form_top, 100, 36),
    }
    y = notes_bottom + MEETING_ACTION_GAP
    if error_height:
        frames["error"] = (0, y, width, error_height)
        y += error_height + MEETING_ACTION_GAP
    frames["action_y"] = y
    return frames


SYSTEM_AUDIO_PERMISSION = (
    "Recording your mic only. Allow system audio to also capture others."
)
SYSTEM_AUDIO_UNAVAILABLE = (
    "Recording your mic only. System audio isn't available on this Mac right now."
)
_PERMISSION_HINTS = ("permission", "not authorized", "not permitted", "denied", "tcc")


def system_audio_is_permission_error(error: str) -> bool:
    """True when a system-audio failure reads as a missing OS permission.

    Only the ScreenCaptureKit path reports these; a missing loopback device,
    absent bindings or a timeout are not permission problems.
    """
    text = (error or "").lower()
    return any(hint in text for hint in _PERMISSION_HINTS)


def meeting_capture_status(session: dict) -> str:
    """One plain sentence about what a running meeting is recording.

    Replaces the old "mic only — system audio unavailable — <raw error>" line:
    the mic and system-audio failure are reported once, and the raw error only
    decides which sentence is honest.
    """
    state = session.get("system_audio")
    if state == "on":
        return "Recording your mic and system audio."
    if state == "error":
        if system_audio_is_permission_error(session.get("system_audio_error", "")):
            return SYSTEM_AUDIO_PERMISSION
        return SYSTEM_AUDIO_UNAVAILABLE
    return "Recording your mic only."


_STAMP_PREFIX = re.compile(r"^\d{4}-\d{2}-\d{2} \d{4}\s+")


def note_title_from_path(path: str) -> str:
    """'Planning' from '.../2026-11-15 1430 Planning.md' (date prefix dropped)."""
    stem = Path(str(path or "")).stem
    return _STAMP_PREFIX.sub("", stem).strip() or stem or "Meeting"


def saved_notes_message(saved: dict) -> str:
    """The saved-state line: the note's title, never its raw path."""
    title = (saved.get("title") or "").strip() or note_title_from_path(
        saved.get("notes_path", "")
    )
    return f"Notes saved: {title}"


# The newest saved notes shown on the Meetings page.
RECENT_NOTES_LIMIT = 5
RECENT_NOTES_SCAN_LIMIT = 2000  # files looked at, so a huge folder stays cheap
RECENT_NOTES_MAX_DEPTH = 3  # notes_folder/<category>/<file>.md, with slack
RECENT_NOTES_UNREADABLE = "Couldn't read your meeting notes folder."
RECENT_NOTES_OUTSIDE = (
    "Your meeting notes folder is outside your vault, so it isn't listed here."
)
_HEAD_BYTES = 4096
_FILE_STAMP = re.compile(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2})(\d{2})(?:\s+(.*))?$")


def _note_heading(path: Path) -> str:
    """The note's '# Title' line (after any frontmatter), or ''."""
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            head = handle.read(_HEAD_BYTES)
    except OSError:
        return ""
    lines = head.splitlines()
    if lines and lines[0].strip() == "---":
        for index in range(1, len(lines)):
            if lines[index].strip() == "---":
                lines = lines[index + 1:]
                break
    for line in lines:
        if line.startswith("# "):
            return line[2:].strip()
        if line.strip():
            return ""
    return ""


def _note_when(path: Path, mtime: float) -> datetime:
    match = _FILE_STAMP.match(path.stem)
    if match:
        try:
            return datetime(*(int(match.group(i)) for i in range(1, 6)))
        except ValueError:
            pass
    return datetime.fromtimestamp(mtime)


def note_date_label(when: datetime, now: datetime | None = None) -> str:
    now = now or datetime.now()
    clock = when.strftime("%I:%M %p").lstrip("0")
    if when.date() == now.date():
        return f"Today, {clock}"
    if when.year == now.year:
        return f"{when.strftime('%b')} {when.day}, {clock}"
    return f"{when.strftime('%b')} {when.day}, {when.year}"


def recent_notes(
    vault_path, notes_folder: str, *, limit: int = RECENT_NOTES_LIMIT,
    now: datetime | None = None,
) -> tuple[list[dict], str]:
    """The newest saved meeting notes: ([{title, date_label, path}], error).

    Same storage the CLI's `lf notes` and recall read: Markdown files under
    <vault>/<notes_folder>/ as ObsidianWriter writes them
    (<category>/YYYY-MM-DD HHMM Title.md, '# Title' after the frontmatter).
    Only that folder is walked (never the whole vault), symlinks are not
    followed, anything resolving outside it is skipped, transcripts are
    excluded, and only the `limit` newest files are opened for their heading.
    A missing folder is simply "no notes yet"; an unreadable one is a friendly
    error, never an exception.
    """
    if not vault_path or not notes_folder:
        return [], ""
    try:
        vault = Path(vault_path).expanduser().resolve()
        root = (vault / notes_folder).resolve()
        # An absolute or ../ notes_folder, or a notes folder that is itself a
        # symlink out of the vault, is never read.
        if not root.is_relative_to(vault):
            return [], RECENT_NOTES_OUTSIDE
        if not root.exists():
            return [], ""
        if not root.is_dir() or not os.access(root, os.R_OK | os.X_OK):
            return [], RECENT_NOTES_UNREADABLE
    except (OSError, RuntimeError, ValueError):
        return [], RECENT_NOTES_UNREADABLE
    try:
        return _recent_notes_in(root, limit, now)
    except (OSError, RuntimeError, ValueError):
        return [], RECENT_NOTES_UNREADABLE


def _recent_notes_in(root: Path, limit: int, now: datetime | None):
    failures = []
    found = []
    seen = 0
    for directory, subdirs, files in os.walk(
        root, followlinks=False, onerror=failures.append,
    ):
        depth = len(Path(directory).relative_to(root).parts)
        if depth >= RECENT_NOTES_MAX_DEPTH:
            subdirs[:] = []
        subdirs[:] = [name for name in subdirs if not name.startswith(".")]
        for name in files:
            if not name.lower().endswith(".md") or name.endswith(" (transcript).md"):
                continue
            seen += 1
            if seen > RECENT_NOTES_SCAN_LIMIT:
                break
            path = Path(directory) / name
            try:
                if path.is_symlink() or not path.is_file():
                    continue
                if not path.resolve().is_relative_to(root):
                    continue
                mtime = path.stat().st_mtime
            except OSError:
                continue
            found.append((_note_when(path, mtime), mtime, path))
        if seen > RECENT_NOTES_SCAN_LIMIT:
            break
    if not found and failures and any(
        Path(getattr(error, "filename", "") or "") == root for error in failures
    ):
        return [], RECENT_NOTES_UNREADABLE
    found.sort(key=lambda item: (item[0], item[1]), reverse=True)
    notes = []
    for when, _mtime, path in found[:limit]:
        match = _FILE_STAMP.match(path.stem)
        fallback = ((match.group(6) or "").strip() if match else "") or path.stem
        notes.append({
            "title": _note_heading(path) or fallback,
            "date_label": note_date_label(when, now),
            "path": str(path),
        })
    return notes, ""


RECENT_ROW_H = 44
RECENT_EMPTY = "No saved meeting notes yet. Notes you save appear here."


# ----------------------------------------------------------------- speakers ---

SPEAKER_LABEL_MAX = 60        # display cap; the server caps names at 40
SPEAKER_ID_MAX = 24
SPEAKER_NAME_MAX = 40         # == meetings.MAX_SPEAKER_NAME_CHARS
SPEAKER_ERROR_MAX = 200
SPEAKER_ROWS_MAX = 32
SPEAKERS_HEADING = "Speakers"
SPEAKER_FALLBACK_LABEL = "Unnamed speaker"
SPEAKER_TARGET_PLACEHOLDER = "Choose a speaker…"
SPEAKER_NO_UPDATE = "LocalFlow didn't return the updated speakers. The list will refresh."

_SPEAKER_ERRORS = {
    "invalid speaker name": "That name can't be used. Use letters or numbers.",
    "unknown speaker": "That speaker is no longer in this meeting. The list will refresh.",
    "no speaker data for this meeting": "This meeting's speaker data is no longer available.",
    "transcript file not found": (
        "The transcript file was moved or deleted, so speakers can't be changed."
    ),
}


def format_speaker_duration(seconds) -> str:
    """'45s', '1m 12s' or '1h 2m' for a speaker's talk time; '0s' for junk."""
    if isinstance(seconds, bool) or not isinstance(seconds, (int, float)):
        return "0s"
    if math.isnan(seconds) or math.isinf(seconds) or seconds <= 0:
        return "0s"
    total = int(seconds + 0.5)
    if total == 0:
        return "<1s"
    if total < 60:
        return f"{total}s"
    if total < 3600:
        return f"{total // 60}m {total % 60}s"
    return f"{total // 3600}h {total % 3600 // 60}m"


def sanitize_speaker_label(label, fallback: str = SPEAKER_FALLBACK_LABEL) -> str:
    """One escaped, bounded display line for a speaker label from the server."""
    if not isinstance(label, str):
        return fallback
    return safe_text(" ".join(label.split()), SPEAKER_LABEL_MAX) or fallback


def _speaker_seconds(value) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0.0
    value = float(value)
    return value if math.isfinite(value) and value > 0 else 0.0


def speaker_rows(saved) -> list[dict]:
    """Display rows for a saved meeting's `speakers`; [] when there are none."""
    if not isinstance(saved, dict):
        return []
    meeting_id = saved.get("meeting_id")
    speakers = saved.get("speakers")
    if not isinstance(meeting_id, str) or not meeting_id or not isinstance(speakers, list):
        return []
    rows = []
    seen = set()
    for entry in speakers:
        if len(rows) >= SPEAKER_ROWS_MAX:
            break
        if not isinstance(entry, dict):
            continue
        raw_id = entry.get("id")
        if not isinstance(raw_id, str) or not raw_id or raw_id in seen:
            continue
        seen.add(raw_id)
        source = entry.get("source")
        rows.append({
            "id": raw_id,
            "id_text": safe_text(raw_id, SPEAKER_ID_MAX),
            "label": sanitize_speaker_label(entry.get("label")),
            "seconds": _speaker_seconds(entry.get("seconds")),
            "duration": format_speaker_duration(entry.get("seconds")),
            "source": source if isinstance(source, str) else "",
        })
    return rows


def speaker_meeting_id(saved) -> str:
    """The saved meeting's id, only when it has speaker rows to show."""
    return saved["meeting_id"] if speaker_rows(saved) else ""


def merge_target_choices(rows, source_id) -> list[dict]:
    """[{id, title}] for every row but `source_id`; titles are always unique.

    NSPopUpButton drops an earlier item with a repeated title, so labels shared
    by several speakers get the speaker id, and any title still repeated a
    counter.
    """
    counts = {}
    for row in rows:
        key = row["label"].casefold()
        counts[key] = counts.get(key, 0) + 1
    choices = []
    used = set()
    for row in rows:
        if row["id"] == source_id:
            continue
        title = f"{row['label']} · {row['duration']}"
        if counts[row["label"].casefold()] > 1:
            title = f"{title} · {row['id_text']}"
        unique, n = title, 2
        while unique in used:
            unique = f"{title} ({n})"
            n += 1
        used.add(unique)
        choices.append({"id": row["id"], "title": unique})
    return choices


def speaker_name_error(name, current_label: str = "") -> str | None:
    """Why `name` can't be a speaker name, or None. Mirrors the server's rules."""
    text = " ".join(str(name or "").split())
    if not text:
        return "Type a name first."
    normalized = unicodedata.normalize("NFKC", text)
    if not any(
        unicodedata.category(ch)[0] == "L" or unicodedata.category(ch) == "Nd"
        for ch in normalized
    ):
        return "Use letters or numbers in the name."
    if len(text) > SPEAKER_NAME_MAX:
        return f"Keep the name to {SPEAKER_NAME_MAX} characters or fewer."
    return None


def speaker_error_text(action: str, error) -> str:
    """The line shown under the Speakers card when a rename or merge failed."""
    text = safe_text(" ".join(str(error).split()), SPEAKER_ERROR_MAX)
    text = _SPEAKER_ERRORS.get(text, text)
    return f"Couldn't {action} the speaker: {text}"


class PolledStatus(dict):
    """A /meeting/status payload that remembers the speaker ticket it was requested under."""

    speaker_ticket = None

    def __init__(self, data, ticket):
        super().__init__(data)
        self.speaker_ticket = ticket


class SpeakerPanel:
    """One meeting's Speakers editor: rows, the open edit, busy, error. No UI, no I/O."""

    def __init__(self):
        self.meeting_id = ""
        self.rows = []
        self.mode = ""          # "" | "rename" | "merge"
        self.speaker_id = ""    # the row being renamed / merged away
        self.into_id = ""       # chosen merge target
        self.draft = ""         # rename text, survives rebuilds
        self.busy = ""          # "" | "rename" | "merge"
        self.error = ""
        self.epoch = 0          # request/poll freshness
        self.revision = 0       # bumps on every visible change

    @property
    def visible(self) -> bool:
        return bool(self.rows)

    @property
    def can_merge(self) -> bool:
        return len(self.rows) >= 2

    def row(self, speaker_id):
        for row in self.rows:
            if row["id"] == speaker_id:
                return row
        return None

    def choices(self) -> list[dict]:
        if self.mode != "merge":
            return []
        return merge_target_choices(self.rows, self.speaker_id)

    def ticket(self) -> int:
        """Taken on the UI thread when a status request is sent."""
        return self.epoch

    def stamp(self, result, ticket):
        return PolledStatus(result, ticket) if isinstance(result, dict) else result

    def _close_edit(self):
        self.mode = ""
        self.speaker_id = ""
        self.into_id = ""
        self.draft = ""

    def sync(self, saved, ticket=None) -> bool:
        """Adopt the speakers of `saved`; True when anything visible changed."""
        if ticket is not None and ticket != self.epoch:
            return False
        meeting_id = speaker_meeting_id(saved)
        rows = speaker_rows(saved)
        if meeting_id != self.meeting_id:
            self.meeting_id = meeting_id
            self.rows = rows
            self._close_edit()
            self.busy = ""
            self.error = ""
            self.epoch += 1
            self.revision += 1
            return True
        if not meeting_id:
            return False
        if self.busy or (ticket is not None and ticket != self.epoch):
            return False
        if rows == self.rows:
            return False
        self.rows = rows
        if self.mode:
            if self.row(self.speaker_id) is None or (
                self.mode == "merge" and not self.can_merge
            ):
                self._close_edit()
            elif self.into_id and self.row(self.into_id) is None:
                self.into_id = ""
        self.revision += 1
        return True

    def begin_rename(self, speaker_id) -> None:
        row = self.row(speaker_id)
        if self.busy or row is None:
            return
        self.mode = "rename"
        self.speaker_id = speaker_id
        self.into_id = ""
        self.draft = row["label"]
        self.error = ""
        self.revision += 1

    def begin_merge(self, speaker_id) -> None:
        if self.busy or self.row(speaker_id) is None or not self.can_merge:
            return
        self.mode = "merge"
        self.speaker_id = speaker_id
        self.draft = ""
        self.error = ""
        choices = self.choices()
        self.into_id = choices[0]["id"] if len(choices) == 1 else ""
        self.revision += 1

    def choose_target(self, speaker_id) -> None:
        if self.mode != "merge" or self.busy:
            return
        if speaker_id not in [choice["id"] for choice in self.choices()]:
            return
        self.into_id = speaker_id
        self.revision += 1

    def set_draft(self, text, speaker_id=None) -> None:
        """Keep the rename text; no revision bump (called before a rebuild).

        Ignored outside rename mode, and when `speaker_id` (the row the field
        was drawn for) is not the row being renamed: a field left over from an
        older edit never overwrites the current draft.
        """
        if self.mode != "rename" or self.busy:
            return
        if speaker_id is not None and speaker_id != self.speaker_id:
            return
        self.draft = str(text)

    def cancel(self) -> None:
        if self.busy:
            return
        changed = bool(self.mode or self.speaker_id or self.into_id or self.draft or self.error)
        self._close_edit()
        self.error = ""
        if changed:
            self.revision += 1

    def submit_rename(self, name):
        """("rename_speaker", (meeting_id, speaker_id, name), token) or None."""
        if self.busy or self.mode != "rename":
            return None
        self.draft = str(name)
        problem = speaker_name_error(name)
        if problem:
            self.error = problem
            self.revision += 1
            return None
        collapsed = " ".join(str(name).split())
        row = self.row(self.speaker_id)
        if row is not None and collapsed == row["label"]:
            self.cancel()
            return None
        self.busy = "rename"
        self.error = ""
        self.epoch += 1
        self.revision += 1
        return ("rename_speaker", (self.meeting_id, self.speaker_id, collapsed), self.epoch)

    def submit_merge(self):
        """("merge_speakers", (meeting_id, from_id, into_id), token) or None."""
        if self.busy or self.mode != "merge":
            return None
        if not self.into_id or self.into_id == self.speaker_id:
            return None
        self.busy = "merge"
        self.error = ""
        self.epoch += 1
        self.revision += 1
        return ("merge_speakers", (self.meeting_id, self.speaker_id, self.into_id), self.epoch)

    def finish(self, token, result, error) -> bool:
        """Apply a rename/merge response; False when it belongs to an older state."""
        if token != self.epoch:
            return False
        action = self.busy or "update"
        self.busy = ""
        self.epoch += 1
        self.revision += 1
        if error:
            self.error = speaker_error_text(action, error)
            return True
        rows = []
        if isinstance(result, dict) and result.get("meeting_id") in (None, self.meeting_id):
            rows = speaker_rows({"meeting_id": self.meeting_id, "speakers": result.get("speakers")})
        if rows:
            self.rows = rows
            self._close_edit()
            self.error = ""
        else:
            self.error = SPEAKER_NO_UPDATE
        return True


SPEAKER_ROW_H = RECENT_ROW_H
SPEAKERS_CARD_Y = 28          # heading 22 + 6, as Recent meetings
SPEAKERS_ERROR_GAP = 6
SPEAKERS_GAP = 12             # panel -> next section
SPEAKER_BUTTONS = {"rename": 84, "merge": 72, "save": 84, "confirm": 92, "cancel": 72}
_SPEAKER_PAD = 16             # == ui_tokens.CARD_PADDING
_SPEAKER_BUTTON_GAP = 8


def meeting_speakers_layout(width: int, y: int, count: int, *, error_height: int = 0) -> dict:
    """Heading, card and error frames of the Speakers block, and the y below it."""
    if count <= 0:
        return {"bottom": y}
    bottom = y + SPEAKERS_CARD_Y + count * SPEAKER_ROW_H
    frames = {
        "heading": (0, y, 400, 22),
        "card": (0, y + SPEAKERS_CARD_Y, width, count * SPEAKER_ROW_H),
    }
    if error_height:
        frames["error"] = (0, bottom + SPEAKERS_ERROR_GAP, width, error_height)
        bottom += SPEAKERS_ERROR_GAP + error_height
    frames["bottom"] = bottom + SPEAKERS_GAP
    return frames


def speaker_row_frames(width: int, index: int, mode: str = "", *, can_merge: bool = True) -> dict:
    """Card-local frames of one speaker row in its normal, rename or merge state."""
    y0 = index * SPEAKER_ROW_H
    pad = _SPEAKER_PAD
    gap = _SPEAKER_BUTTON_GAP
    by = y0 + 8
    frames = {"divider": (12, y0 + SPEAKER_ROW_H - 1, width - 24, 1)}
    right = width - pad
    if mode == "":
        if can_merge:
            merge_w = SPEAKER_BUTTONS["merge"]
            frames["merge"] = (right - merge_w, by, merge_w, 28)
            right = right - merge_w - gap
        rename_w = SPEAKER_BUTTONS["rename"]
        rename_x = right - rename_w
        frames["rename"] = (rename_x, by, rename_w, 28)
        duration_x = rename_x - 12 - 80
        frames["duration"] = (duration_x, y0 + 14, 80, 18)
        frames["name"] = (pad, y0 + 12, duration_x - 12 - pad, 20)
        return frames
    cancel_w = SPEAKER_BUTTONS["cancel"]
    cancel_x = right - cancel_w
    frames["cancel"] = (cancel_x, by, cancel_w, 28)
    if mode == "rename":
        save_w = SPEAKER_BUTTONS["save"]
        save_x = cancel_x - gap - save_w
        frames["save"] = (save_x, by, save_w, 28)
        frames["field"] = (pad, y0 + 6, save_x - 12 - pad, 32)
        return frames
    confirm_w = SPEAKER_BUTTONS["confirm"]
    confirm_x = cancel_x - gap - confirm_w
    frames["confirm"] = (confirm_x, by, confirm_w, 28)
    picker_w = max(160, min(280, (width - 216) // 2))
    picker_x = confirm_x - 12 - picker_w
    frames["picker"] = (picker_x, by, picker_w, 28)
    frames["prompt"] = (pad, y0 + 12, picker_x - 12 - pad, 20)
    return frames


# -------------------------------------------------------------------- style ---

STYLE_LABEL_W = 160
STYLE_GROUP_X = 180
STYLE_ROW_H = 86
STYLE_EXAMPLE_Y = 28
STYLE_EXAMPLE_H = 34  # two 11pt lines: examples wrap instead of truncating
STYLE_SAVE_FAILED = (
    "Couldn't save your style settings. Check that ~/.localflow can be "
    "written to and try again."
)
STYLE_AUTOSAVE_HINT = "Changes save automatically."


def style_row_frames(y: int, group_width: int) -> dict:
    """Label, wrapped example and segmented group of one style row.

    The label and its example share the left column (STYLE_LABEL_W wide); the
    example wraps over two lines inside the row, and the group starts right of
    the column, so nothing overlaps at any width.
    """
    return {
        "label": (0, y + 4, STYLE_LABEL_W, 22),
        "example": (0, y + STYLE_EXAMPLE_Y, STYLE_LABEL_W, STYLE_EXAMPLE_H),
        "group": (STYLE_GROUP_X, y, group_width, 36),
    }


def style_update(cleanup: str, styles: dict) -> dict:
    """The config update for the Style page's current selections."""
    return {
        "cleanup": {
            "level": cleanup,
            **{f"style_{name}": value for name, value in styles.items()},
        }
    }


# ------------------------------------------------------------------ hotkeys ---

_KEY_NAMES = {
    "alt_r": "Right Option", "alt_l": "Left Option", "alt": "Option",
    "ctrl_r": "Right Control", "ctrl_l": "Left Control", "ctrl": "Control",
    "shift_r": "Right Shift", "shift_l": "Left Shift", "shift": "Shift",
    "cmd_r": "Right Command", "cmd_l": "Left Command", "cmd": "Command",
}


def format_hotkey(value: str | None) -> str:
    """Human label for a config hotkey; an empty value means disabled."""
    value = (value or "").strip()
    if not value:
        return "Off"
    parts = []
    for part in value.split("+"):
        part = part.strip().strip("<>").lower()
        parts.append(_KEY_NAMES.get(part) or (part.upper() if len(part) == 1 else part.title()))
    return " + ".join(parts)


def shortcut_rows(config) -> list[dict]:
    """The dictation shortcuts, read from the config's current hotkeys.

    None of them has an enable flag of its own: an empty hotkey disables it.
    """
    return [
        {
            "name": "Prompt Engineer",
            "detail": "Select rough text, press the shortcut, get an AI-ready prompt.",
            "hotkey": format_hotkey(getattr(config, "prompt_hotkey", "")),
        },
        {
            "name": "Polish",
            "detail": "Select text and clean up its wording and grammar in place.",
            "hotkey": format_hotkey(getattr(config, "polish_hotkey", "")),
        },
        {
            "name": "Command Mode",
            "detail": "Hold the key and speak an instruction to rewrite your selection.",
            "hotkey": format_hotkey(getattr(config, "command_hotkey", "")),
        },
        {
            "name": "Toggle Dictation",
            "detail": "Turn push-to-talk on or off without quitting LocalFlow.",
            "hotkey": format_hotkey(getattr(config, "dictation_toggle_hotkey", "")),
        },
    ]


# --------------------------------------------------------------- dictionary ---

DICTIONARY_SAVE_FAILED = (
    "Couldn't save to your dictionary. Check that ~/.localflow can be written to "
    "and try again."
)


def dictionary_entries(data: dict) -> list[tuple[str, str, str, str]]:
    """(label, key, kind, target) for every word and replacement; nothing capped."""
    entries = [(word, word, "word", "") for word in data["words"]]
    entries.extend(
        (f'{pair["from"]} → {pair["to"]}', pair["from"], "replacement", pair["to"])
        for pair in data["replacements"]
    )
    return entries


def submit_word(store, text: str, editing: str | None = None) -> str | None:
    """Add (or, when `editing`, rename) a word. Returns a friendly error or None."""
    if not (text or "").strip():
        return "Type a word first."
    try:
        if editing:
            if not store.update_word(editing, text):
                return "That word was removed or is already in your dictionary."
        elif not store.add_word(text):
            return f"“{text.strip()}” is already in your dictionary."
    except DictionaryError:
        return "Type a word first."
    except Exception:  # noqa: BLE001
        return DICTIONARY_SAVE_FAILED
    return None


def submit_replacement(
    store, source: str, target: str, editing: str | None = None
) -> str | None:
    """Add (or edit) an auto-replace pair. Returns a friendly error or None."""
    if not (source or "").strip():
        return "Type what you say first."
    if not (target or "").strip():
        return "Type what it should become."
    try:
        if editing:
            if not store.update_replacement(editing, source, target):
                return "That replacement was removed or already exists."
        elif not store.add_replacement(source, target):
            return f"You already have a replacement for “{source.strip()}”."
    except DictionaryError:
        return "Fill in both “When I say” and “Type this”."
    except Exception:  # noqa: BLE001
        return DICTIONARY_SAVE_FAILED
    return None


def remove_entry(store, key: str, kind: str | None = None) -> str | None:
    """Remove one dictionary entry (of `kind`). Returns a friendly error or None."""
    try:
        store.remove(key, kind)
    except Exception:  # noqa: BLE001
        return DICTIONARY_SAVE_FAILED
    return None


# ------------------------------------------------------------------ prompts ---

PROMPTS_EMPTY = "Prompts generated from meetings show up here for approval."
PROMPTS_UNREADABLE = "Couldn't read your prompt queue. Check ~/.localflow and try again."
PROMPT_STATUS_PILLS = {
    "pending": ("sun", "ink"),
    "approved": ("success", "success"),
    "rejected": ("line", "muted"),
    "dispatched": ("cobalt", "cobalt"),
    "failed": ("red", "red"),
}


def prompt_context(entry: dict) -> str:
    """Where a prompt came from. The queue stores `repo` and `meeting`, not a project."""
    return entry.get("repo") or entry.get("meeting") or "Unknown meeting"


ORCA_NOT_OPEN = "Couldn't send — the Orca agent app isn't open"
# What `orca_available` records ("Orca app is not running — run `orca open`…")
# and older queues stored ("Orca is not running"). Other failures (a missing
# CLI, an unreachable runtime, a bad repo, a timeout) keep their own text.
_ORCA_NOT_RUNNING = re.compile(r"\borca(?: app)? is not running\b", re.IGNORECASE)


def prompt_error_text(error) -> str:
    """The failure shown on a prompt row: shared copy for a known cause only."""
    text = " ".join(str(error or "").split())
    if _ORCA_NOT_RUNNING.search(text):
        return ORCA_NOT_OPEN
    return text


def prompt_rows(entries: list[dict]) -> list[dict]:
    """Newest first. Buttons follow the same guards the server API enforces."""
    rows = []
    for entry in reversed(entries):
        status = entry.get("status") or "pending"
        context = prompt_context(entry)
        error = prompt_error_text(entry.get("error")) if status == "failed" else ""
        if error:
            context = f"{context} · {error}"
        rows.append(
            {
                "id": entry.get("id", ""),
                "title": entry.get("title") or "Untitled prompt",
                "context": context,
                "error": error,
                "status": status,
                "status_label": status.title(),
                "can_approve": status in APPROVE_FROM,
                "can_reject": status in REJECT_FROM,
            }
        )
    return rows


PROMPT_ROW_MIN_H = 68
PROMPT_TITLE_Y = 12
PROMPT_CONTEXT_Y = 36
PROMPT_CONTEXT_LINE = 16
PROMPT_ROW_BOTTOM = 14
PROMPT_BUTTONS = {"approve": 88, "reject": 80}
PROMPT_BUTTON_GAP = 8
PROMPT_EDGE = 12


def prompt_row_layout(width: int, row: dict, pill_width: int, measure=None) -> dict:
    """Geometry of one prompt row: buttons, pill, text column and height.

    The context line (source plus any failure) wraps inside the text column and
    the row grows to fit it, so a friendly failure sentence is never truncated
    even at the minimum window width with both actions shown. `measure(text)`
    is the real 11pt measurement in AppKit; without it a conservative estimate
    is used.
    """
    widths = []
    if row.get("can_approve"):
        widths.append(PROMPT_BUTTONS["approve"])
    if row.get("can_reject"):
        widths.append(PROMPT_BUTTONS["reject"])
    action_x = width - PROMPT_EDGE - sum(widths) - PROMPT_BUTTON_GAP * max(0, len(widths) - 1)
    pill_x = action_x - pill_width - 12
    text_width = max(pill_x - 12 - 14, 40)
    measure = measure or (lambda text: len(text) * 6.5)
    lines = wrap_lines(row.get("context", ""), text_width, measure)
    context_h = max(1, len(lines)) * PROMPT_CONTEXT_LINE + 2
    height = max(PROMPT_ROW_MIN_H, PROMPT_CONTEXT_Y + context_h + PROMPT_ROW_BOTTOM)
    return {
        "action_x": action_x,
        "pill_x": pill_x,
        "text_width": text_width,
        "context_h": context_h,
        "height": height,
        "lines": len(lines),
    }


def review_prompt(queue, entry_id: str, action: str) -> str | None:
    """Approve or reject through the same guarded helpers the server uses.

    Returns a friendly, actionable message when it could not be done.
    """
    apply = {"approve": approve_prompt, "reject": reject_prompt}[action]
    try:
        apply(queue, entry_id)
    except PromptTransitionError as exc:
        if exc.status_code == 404:
            return "That prompt is no longer in the queue. The list was refreshed."
        past = "approved" if action == "approve" else "rejected"
        return (
            f"That prompt changed before it could be {past}, so nothing was "
            "changed. The list was refreshed."
        )
    except Exception:  # noqa: BLE001
        return "Couldn't update the prompt queue. Check ~/.localflow and try again."
    return None
