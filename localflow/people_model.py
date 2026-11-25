"""AppKit- and Tk-free logic behind the People block on the Meetings page.

Speaker recognition consent copy, the enrolled-voices list, Enroll my voice,
Rename / Forget / Forget all / Export, and the pure layout both Hubs draw. No
UI imports and no I/O, so every rule here is unit-tested on any platform.
"""

import ipaddress
import math
import re
from datetime import datetime
from pathlib import Path
from urllib.parse import urlsplit

from localflow.hub_model import (
    RECENT_ROW_H,
    SPEAKER_BUTTONS,
    SPEAKER_ERROR_MAX,
    safe_text,
    sanitize_speaker_label,
    speaker_name_error,
    speaker_row_frames,
)
from localflow.ui_tokens import SWITCH_WIDTH

# ------------------------------------------------------------------- copy ---

CONSENT_TITLE = "Turn on speaker recognition?"
CONSENT_DRAFT_NOTE = "Draft wording, pending legal review."
CONSENT_POINTS = (
    "Voiceprints are biometric data derived from audio.",
    (
        "They are stored only on this computer, in ~/.localflow/speakers/. They are never "
        "saved in your Obsidian vault and never sent to an LLM or any external service."
    ),
    (
        "Enrolling someone else's voice is your responsibility. Get their permission "
        "where the law requires it."
    ),
    (
        "Turning speaker recognition off stops reading voiceprints immediately but does "
        "not delete them. Use Forget or Forget all (or `lf speakers forget --all`) to "
        "permanently delete stored voiceprints and meeting voice data."
    ),
)
CONSENT_CONFIRM, CONSENT_CANCEL = "Turn On", "Cancel"
PEOPLE_HEADING = "People"
TOGGLE_TITLE = "Recognize enrolled voices"
TOGGLE_ON = "On. Voiceprints stay on this computer."
TOGGLE_OFF = "Off. Meetings use generic speaker labels."
TOGGLE_NEEDS_DIARIZATION = "Turn on speaker labels (speaker_diarization) first."
PEOPLE_LOADING = "Loading people…"
PEOPLE_EMPTY = "No one enrolled yet. Enroll your voice, or use Remember on a meeting's speakers."
STALE_BADGE = "Needs re-enrollment"
ENROLL_DEFAULT_NAME = "Me"
ENROLL_SECONDS, ENROLL_MIN_SECONDS = 30, 3
ENROLL_REMOTE = "Voice audio only goes to a LocalFlow server on this computer."
EXPORT_IN_VAULT = "Choose a folder outside your Obsidian vault. Vaults get synced."
EXPORT_NOT_EMPTY = "Choose a new or empty folder."
EXPORT_REMINDER = "These files are voiceprints. Keep them private."
PEOPLE_GENERIC_ERROR = "LocalFlow couldn't complete that."
MIC_ERROR = "Couldn't use the microphone. Check the input device and microphone permission."
SAVE_FAILED = "Couldn't save the setting. Check ~/.localflow and try again."
ENROLL_SAVED = "Voice enrolled. Later meetings will recognize it."


def consent_text() -> str:
    """The consent points and the draft note as one block, blank lines between."""
    return "\n\n".join((*CONSENT_POINTS, CONSENT_DRAFT_NOTE))


def forget_prompt(name: str) -> str:
    """The confirmation for Forget; `name` is already a sanitized display label."""
    return (
        f"Forget {name}? This permanently deletes the voiceprint and stored voice data "
        "from saved meetings. Names already written in transcripts stay."
    )


FORGET_ALL_PROMPT = (
    "Forget everyone? This permanently deletes every voiceprint and all stored voice "
    "data from saved meetings. Names already written in transcripts stay."
)


def recording_status(seconds_left: int) -> str:
    return f"Recording… 0:{max(0, int(seconds_left)):02d} left. Speak normally."


RECORD_STARTING = "Starting the microphone…"
RECORD_UPLOADING = "Saving the voice…"

# --------------------------------------------------------------- helpers ---

PEOPLE_ROWS_MAX = 100
_VOICEPRINT_ID = re.compile(r"[0-9a-f]{32}")
_GENERIC_LABEL = re.compile(r"Speaker \d+|Others|Unnamed speaker")

_PEOPLE_ERRORS = {
    "speaker recognition is off": (
        "Speaker recognition is off on the LocalFlow server. Try again in a moment."
    ),
    "no speech detected": "No speech was heard. Speak closer to the microphone and try again.",
    "unknown speaker": "That person is no longer enrolled. The list will refresh.",
    "invalid speaker name": "That name can't be used. Use letters or numbers.",
    "could not decode the audio": "The recording couldn't be read. Try again.",
    "decoded audio is empty": "Nothing was recorded. Check the microphone and try again.",
}
_ACTION_VERBS = {
    "load": "load people",
    "rename": "rename",
    "forget": "forget",
    "forget_all": "forget everyone",
    "export": "export",
    "enroll": "enroll the voice",
    "record": "start recording",
}


def recognition_update(on) -> dict:
    """The config update that turns speaker recognition on or off."""
    return {"meetings": {"speaker_recognition": bool(on)}}


def local_base_url(base_url) -> str | None:
    """`base_url` with a literal loopback host, or None when it isn't on this computer.

    Only plain http to 127.0.0.0/8 or ::1 (``localhost`` rewritten to 127.0.0.1,
    so a hosts-file or resolver entry can't send voice data elsewhere), as
    `lf speakers enroll --record` and `lf speakers export` require.
    """
    try:
        parts = urlsplit(str(base_url or "").strip())
        host = parts.hostname
        port = parts.port
    except ValueError:
        return None
    if parts.scheme != "http" or not host or parts.username or parts.password:
        return None
    if host.lower() == "localhost":
        host = "127.0.0.1"
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        return None
    if not address.is_loopback:
        return None
    netloc = f"[{address}]" if address.version == 6 else str(address)
    if port:
        netloc += f":{port}"
    return f"http://{netloc}{parts.path.rstrip('/')}"


def is_local_url(base_url) -> bool:
    return local_base_url(base_url) is not None


def path_inside_vault(target, vault_path) -> bool:
    """True when `target` is the vault or inside it (absolute and resolved forms).

    When it can't be told, the answer is True: refuse rather than risk a synced folder.
    """
    if not vault_path:
        return False
    try:
        target = Path(target).expanduser()
        vault = Path(vault_path).expanduser()
        targets = {target.absolute(), target.resolve()}
        vaults = {vault.absolute(), vault.resolve()}
    except (OSError, RuntimeError, ValueError):
        return True
    return any(t == v or t.is_relative_to(v) for t in targets for v in vaults)


def needs_reenroll(vp, embedder) -> bool:
    """True when a voiceprint was made by another voice model than the server's."""
    if not isinstance(embedder, str) or not isinstance(vp, dict):
        return False
    return vp.get("model_id") != embedder


def _created_label(created) -> str:
    if not isinstance(created, str) or not created.strip():
        return ""
    try:
        when = datetime.fromisoformat(created.strip())  # 3.11+: a trailing Z is UTC
    except ValueError:
        return ""
    return f"{when.strftime('%b')} {when.day}, {when.year}"


def _person_meta(entry) -> str:
    parts = []
    samples = entry.get("samples")
    if isinstance(samples, int) and not isinstance(samples, bool) and samples > 0:
        parts.append(f"{samples} sample{'' if samples == 1 else 's'}")
    created = _created_label(entry.get("created"))
    if created:
        parts.append(created)
    return " · ".join(parts)


def person_rows(payload) -> list[dict]:
    """[{id, name, meta, stale}] for a `GET /api/speakers` payload; [] for junk."""
    if not isinstance(payload, dict) or not isinstance(payload.get("speakers"), list):
        return []
    embedder = payload.get("embedder")
    rows, seen = [], set()
    for entry in payload["speakers"]:
        if len(rows) >= PEOPLE_ROWS_MAX:
            break
        if not isinstance(entry, dict):
            continue
        vid = entry.get("id")
        if not isinstance(vid, str) or not _VOICEPRINT_ID.fullmatch(vid) or vid in seen:
            continue
        seen.add(vid)
        rows.append({
            "id": vid,
            "name": sanitize_speaker_label(entry.get("name")),
            "meta": _person_meta(entry),
            "stale": needs_reenroll(entry, embedder),
        })
    counts: dict[str, int] = {}
    for row in rows:
        key = row["name"].casefold()
        counts[key] = counts.get(key, 0) + 1
    for row in rows:
        if counts[row["name"].casefold()] > 1:
            short_id = row["id"][:6]
            row["meta"] = f"{row['meta']} · {short_id}" if row["meta"] else short_id
    return rows


def is_generic_label(label) -> bool:
    """True for the labels a meeting gives unnamed speakers ("Speaker 2", "Others")."""
    return isinstance(label, str) and _GENERIC_LABEL.fullmatch(label.strip()) is not None


def can_remember(diarization, recognition) -> bool:
    """Remember is offered on meeting speakers only with both flags on."""
    return bool(diarization) and bool(recognition)


def people_error_text(action: str, error) -> str:
    """One plain line for a failed People action. No URL, token, path or vector.

    Only a client error (one carrying `status_code`) contributes its text,
    which is collapsed, escaped, bounded and mapped to plain copy for the
    server's fixed details. Anything else, and any text holding a URL, becomes
    a generic line (the microphone line for `"record"`).
    """
    fallback = MIC_ERROR if action == "record" else PEOPLE_GENERIC_ERROR
    text = ""
    if error is not None and hasattr(error, "status_code"):
        text = safe_text(" ".join(str(error).split()), SPEAKER_ERROR_MAX)
        text = _PEOPLE_ERRORS.get(text, text)
        if "://" in text:
            text = ""
    verb = _ACTION_VERBS.get(action, action)
    return f"Couldn't {verb}: {text or fallback}"


def export_notice(count, folder_name) -> str:
    try:
        count = int(count)
    except (TypeError, ValueError):
        count = 0
    noun = "voiceprint" if count == 1 else "voiceprints"
    return (
        f"Exported {count} {noun} to “{safe_text(str(folder_name or ''), 60)}”. "
        f"{EXPORT_REMINDER}"
    )


# ----------------------------------------------------------------- state ---


class PeopleState:
    """The People block: flags, consent, the list and the one open action. No UI, no I/O.

    `seq` tokens drop a list load that answers late; `epoch` tokens drop a
    rename/forget/enroll/export answer that belongs to an older state;
    `rec_seq` drops a microphone start that answers after Cancel. One message
    slot: setting `error` clears `notice` and the reverse.
    """

    def __init__(self):
        self.diarization = False
        self.recognition = False
        self.consent_open = False
        self.phase = ""         # "" | "loading" | "ready" | "error"
        self.rows = []
        self.embedder = None
        self.load_error = ""
        self.seq = 0
        self.grace = 0          # 403 retries left after our own consent write
        self.mode = ""          # "" | "rename" | "enroll"
        self.person_id = ""
        self.draft = ""
        self.rec = ""           # "" | "starting" | "recording" | "uploading"
        self.rec_seq = 0
        self.rec_started = 0.0
        self.rec_name = ""
        self.busy = ""          # "" | "rename" | "forget" | "forget_all" | "enroll" | "export"
        self.epoch = 0
        self.error = ""
        self.notice = ""
        self.export_name = ""
        self.reload = False
        self.revision = 0

    # ------------------------------------------------------------ derived ---

    @property
    def visible(self) -> bool:
        return self.diarization or self.recognition

    @property
    def active(self) -> bool:
        """The list, footer and actions exist only with both flags on."""
        return self.diarization and self.recognition

    @property
    def toggle_enabled(self) -> bool:
        return self.diarization and not self.busy and not self.rec

    @property
    def toggle_detail(self) -> str:
        if not self.diarization:
            return TOGGLE_NEEDS_DIARIZATION
        return TOGGLE_ON if self.recognition else TOGGLE_OFF

    @property
    def message(self) -> str:
        return self.error or self.notice

    def row(self, person_id):
        for row in self.rows:
            if row["id"] == person_id:
                return row
        return None

    def _say(self, error="", notice=""):
        self.error, self.notice = error, notice

    def _close_edit(self):
        self.mode = ""
        self.person_id = ""
        self.draft = ""
        self.rec = ""
        self.rec_name = ""

    # -------------------------------------------------------------- flags ---

    def set_flags(self, diarization, recognition):
        """Adopt the config flags; the list load request when the list just appeared."""
        diarization, recognition = bool(diarization), bool(recognition)
        was_active = self.active
        if (diarization, recognition) == (self.diarization, self.recognition):
            return None
        self.diarization, self.recognition = diarization, recognition
        self.revision += 1
        if self.active and not was_active:
            return self.load()
        if was_active and not self.active:
            self.rows = []
            self.embedder = None
            self.phase = ""
            self.load_error = ""
            self.grace = 0
            self._close_edit()
            self.busy = ""
            self._say()
            self.seq += 1
            self.epoch += 1
            self.rec_seq += 1
        return None

    def refresh(self):
        """Reload the list (Meetings shown again); None when there is nothing to do."""
        if not self.active or self.busy or self.rec or self.phase == "loading":
            return None
        return self.load()

    def request_recognition(self, on):
        """"consent" (show the sheet), "write" (save off directly) or None."""
        if not self.toggle_enabled:
            return None
        if on:
            if self.consent_open or self.recognition:
                return None
            self.consent_open = True
            return "consent"
        if not self.recognition:
            return None
        return "write"

    def consent_result(self, accepted) -> bool:
        """Close the consent sheet; True means write `speaker_recognition = true`."""
        was_open = self.consent_open
        self.consent_open = False
        return bool(accepted) and was_open

    def recognition_saved(self, on):
        """Our own write went through; the list load request when it's now on."""
        if on:
            self.grace = 3
        if self.error == SAVE_FAILED:
            self._say()
            self.revision += 1
        return self.set_flags(self.diarization, on)

    def recognition_save_failed(self, message=SAVE_FAILED):
        self._say(error=message or SAVE_FAILED)
        self.revision += 1

    # --------------------------------------------------------------- list ---

    def load(self):
        if not self.active:
            return None
        self.phase = "loading"
        self.load_error = ""
        self.seq += 1
        self.revision += 1
        return ("enrolled", (), self.seq)

    def retry(self):
        if self.phase != "error":
            return None
        return self.load()

    def loaded(self, token, result, error):
        """Apply a list load: True (changed), False (stale) or "retry" (403 after consent)."""
        if token != self.seq or self.phase != "loading":
            return False
        if error is not None:
            if getattr(error, "status_code", None) == 403 and self.grace > 0:
                self.grace -= 1
                return "retry"
            self.grace = 0
            self.phase = "error"
            self.load_error = people_error_text("load", error)
            self.revision += 1
            return True
        self.grace = 0
        if not isinstance(result, dict) or not isinstance(result.get("speakers"), list):
            self.phase = "error"
            self.load_error = people_error_text("load", None)
            self.revision += 1
            return True
        self.rows = person_rows(result)
        embedder = result.get("embedder")
        self.embedder = embedder if isinstance(embedder, str) else None
        self.phase = "ready"
        if self.mode == "rename" and self.row(self.person_id) is None and not self.busy:
            self._close_edit()
        self.revision += 1
        return True

    # ------------------------------------------------------------- rename ---

    def begin_rename(self, person_id) -> None:
        row = self.row(person_id)
        if row is None or self.busy or self.rec:
            return
        self.mode = "rename"
        self.person_id = person_id
        self.draft = row["name"]
        self._say()
        self.revision += 1

    def set_draft(self, text, key=None) -> None:
        """Keep typed text; no revision bump. `key` is the person id the field was
        drawn for ("" for the enroll name): a field from an older edit is ignored."""
        if self.mode not in ("rename", "enroll") or self.busy or self.rec:
            return
        if key is not None and key != self.person_id:
            return
        self.draft = str(text)

    def cancel(self) -> None:
        if self.busy or self.rec:
            return
        changed = bool(self.mode or self.draft or self.error)
        self._close_edit()
        self.error = ""
        if changed:
            self.revision += 1

    def submit_rename(self, name):
        """("rename_enrolled", (id, name), token) or None."""
        if self.mode != "rename" or self.busy:
            return None
        self.draft = str(name)
        problem = speaker_name_error(name)
        if problem:
            self._say(error=problem)
            self.revision += 1
            return None
        collapsed = " ".join(str(name).split())
        row = self.row(self.person_id)
        if row is None:
            self.cancel()
            return None
        if collapsed == row["name"]:
            self.cancel()
            return None
        self.busy = "rename"
        self._say()
        self.epoch += 1
        self.revision += 1
        return ("rename_enrolled", (self.person_id, collapsed), self.epoch)

    # ------------------------------------------------------------- forget ---

    def submit_forget(self, person_id):
        """("forget", (id,), token) or None."""
        if self.busy or self.rec or self.row(person_id) is None:
            return None
        self.busy = "forget"
        self._say()
        self.epoch += 1
        self.revision += 1
        return ("forget", (person_id,), self.epoch)

    def submit_forget_all(self):
        """("forget", (None,), token) or None."""
        if not self.active or self.busy or self.rec:
            return None
        self.busy = "forget_all"
        self._say()
        self.epoch += 1
        self.revision += 1
        return ("forget", (None,), self.epoch)

    # ------------------------------------------------------------- export ---

    def submit_export(self, target, vault_path, *, nonempty, base_url=None):
        """("export_enrolled", (target,), token), or None with the refusal in `error`."""
        if not self.active or self.busy or self.rec or not target:
            return None
        problem = ""
        if base_url is not None and not is_local_url(base_url):
            problem = ENROLL_REMOTE
        elif path_inside_vault(target, vault_path):
            problem = EXPORT_IN_VAULT
        elif nonempty:
            problem = EXPORT_NOT_EMPTY
        if problem:
            self._say(error=problem)
            self.revision += 1
            return None
        self.busy = "export"
        self.export_name = Path(str(target)).name or str(target)
        self._say()
        self.epoch += 1
        self.revision += 1
        return ("export_enrolled", (str(target),), self.epoch)

    # ------------------------------------------------------------- enroll ---

    def begin_enroll(self, base_url) -> None:
        if not self.active or self.busy or self.rec:
            return
        if not is_local_url(base_url):
            self._say(error=ENROLL_REMOTE)
            self.revision += 1
            return
        taken = any(row["name"].casefold() == ENROLL_DEFAULT_NAME.casefold() for row in self.rows)
        self.mode = "enroll"
        self.person_id = ""
        self.draft = "" if taken else ENROLL_DEFAULT_NAME
        self._say()
        self.revision += 1

    def start_recording(self, name):
        """("capture_start", (), rec_seq) or None (the name problem in `error`)."""
        if self.mode != "enroll" or self.busy or self.rec:
            return None
        self.draft = str(name)
        problem = speaker_name_error(name)
        if problem:
            self._say(error=problem)
            self.revision += 1
            return None
        self.rec = "starting"
        self.rec_name = " ".join(str(name).split())
        self.rec_seq += 1
        self._say()
        self.revision += 1
        return ("capture_start", (), self.rec_seq)

    def recording_started(self, token, error, now) -> bool:
        """The microphone opened (or failed); False when Cancel came first."""
        if token != self.rec_seq or self.rec != "starting":
            return False
        if error is not None:
            self.rec = ""
            self._say(error=people_error_text("record", error))
        else:
            self.rec = "recording"
            self.rec_started = float(now)
        self.revision += 1
        return True

    def elapsed(self, now) -> float:
        if self.rec != "recording":
            return 0.0
        return max(0.0, float(now) - self.rec_started)

    def remaining(self, now) -> int:
        if self.rec != "recording":
            return ENROLL_SECONDS
        return max(0, min(ENROLL_SECONDS, math.ceil(ENROLL_SECONDS - self.elapsed(now))))

    def can_stop(self, now) -> bool:
        return self.rec == "recording" and self.elapsed(now) >= ENROLL_MIN_SECONDS

    def due(self, now) -> bool:
        return self.rec == "recording" and self.elapsed(now) >= ENROLL_SECONDS

    def stop_recording(self, now=None):
        """("capture_finish", (name,), token) or None (also before ENROLL_MIN_SECONDS
        when `now` is given)."""
        if self.rec != "recording" or self.busy:
            return None
        if now is not None and not self.can_stop(now):
            return None
        self.rec = "uploading"
        self.busy = "enroll"
        self.epoch += 1
        self.revision += 1
        return ("capture_finish", (self.rec_name,), self.epoch)

    def cancel_recording(self) -> bool:
        """Cancel a recording (the enroll row closes); True when a capture must be
        discarded. An upload already under way can't be cancelled."""
        if self.rec not in ("starting", "recording"):
            return False
        self.rec_seq += 1
        self._close_edit()
        self.revision += 1
        return True

    # ------------------------------------------------------------- finish ---

    def finish(self, token, result, error) -> bool:
        """Apply a rename/forget/enroll/export answer; False when it is stale.

        After a change to the list `reload` is True: the caller sends `load()`.
        """
        if token != self.epoch or not self.busy:
            return False
        action = self.busy
        self.busy = ""
        self.epoch += 1
        self.revision += 1
        self.reload = False
        if error is not None:
            self._say(error=people_error_text(action, error))
            if action == "enroll":
                self.rec = ""  # back to naming, the draft kept
                self.draft = self.rec_name or self.draft
            if str(error).strip().lower() == "unknown speaker":
                self.reload = True
            return True
        if action == "export":
            self._say(notice=export_notice(result, self.export_name))
            return True
        self._close_edit()
        self._say(notice=ENROLL_SAVED if action == "enroll" else "")
        self.reload = True
        return True


# ---------------------------------------------------------------- layout ---

PEOPLE_PAD = 16
PEOPLE_TOGGLE_H = 60
PEOPLE_ROW_H = RECENT_ROW_H
PEOPLE_CARD_Y = 28
PEOPLE_MESSAGE_GAP = 6
PEOPLE_GAP = 12
PEOPLE_FORGET_W = 72
PEOPLE_RENAME_W = SPEAKER_BUTTONS["rename"]
PEOPLE_META_W = 150
PEOPLE_ENROLL_W = 140
PEOPLE_EXPORT_W = 88
PEOPLE_FORGET_ALL_W = 104
PEOPLE_STOP_W = 108
PEOPLE_RETRY_W = 72
_GAP = 8


def people_list_height(state) -> int:
    if not state.active:
        return 0
    if state.phase == "ready" and state.rows:
        return len(state.rows) * PEOPLE_ROW_H
    return PEOPLE_ROW_H  # loading, error or empty: one line


def people_card_height(state) -> int:
    if not state.active:
        return PEOPLE_TOGGLE_H
    return PEOPLE_TOGGLE_H + people_list_height(state) + PEOPLE_ROW_H


def people_layout(width: int, y: int, state, *, message_height: int = 0) -> dict:
    """Heading, card and message frames of the People block (body coordinates
    for those three, card-local for the rest) and the y below it."""
    pad = PEOPLE_PAD
    card_y = y + PEOPLE_CARD_Y
    card_h = people_card_height(state)
    text_w = width - 2 * pad - SWITCH_WIDTH - pad
    frames = {
        "heading": (0, y, 400, 22),
        "card": (0, card_y, width, card_h),
        "toggle_title": (pad, 10, text_w, 20),
        "toggle_detail": (pad, 32, text_w, 16),
        "switch": (width - pad - SWITCH_WIDTH, 19, SWITCH_WIDTH, 22),
    }
    if state.active:
        list_h = people_list_height(state)
        frames["list"] = (0, PEOPLE_TOGGLE_H, width, list_h)
        frames["footer"] = (0, PEOPLE_TOGGLE_H + list_h, width, PEOPLE_ROW_H)
    bottom = card_y + card_h
    if message_height:
        frames["message"] = (0, bottom + PEOPLE_MESSAGE_GAP, width, message_height)
        bottom += PEOPLE_MESSAGE_GAP + message_height
    frames["bottom"] = bottom + PEOPLE_GAP
    return frames


def people_status_frames(width: int) -> dict:
    """List-local frames of the one-line list states: loading/empty label, or error + Retry."""
    pad = PEOPLE_PAD
    retry_x = width - pad - PEOPLE_RETRY_W
    return {
        "label": (pad, 12, width - 2 * pad, 20),
        "error": (pad, 13, retry_x - 12 - pad, 18),
        "retry": (retry_x, 8, PEOPLE_RETRY_W, 28),
    }


def person_row_frames(width: int, index: int, *, pill_width: int = 0) -> dict:
    """List-local frames of one person row: name, badge, meta, Rename, Forget."""
    pad = PEOPLE_PAD
    y0 = index * PEOPLE_ROW_H
    forget_x = width - pad - PEOPLE_FORGET_W
    rename_x = forget_x - _GAP - PEOPLE_RENAME_W
    meta_x = rename_x - 12 - PEOPLE_META_W
    frames = {
        "divider": (12, y0 + PEOPLE_ROW_H - 1, width - 24, 1),
        "forget": (forget_x, y0 + 8, PEOPLE_FORGET_W, 28),
        "rename": (rename_x, y0 + 8, PEOPLE_RENAME_W, 28),
        "meta": (meta_x, y0 + 14, PEOPLE_META_W, 18),
    }
    right = meta_x
    if pill_width:
        pill_x = meta_x - 12 - pill_width
        frames["pill"] = (pill_x, y0 + 10, pill_width, 24)
        right = pill_x
    frames["name"] = (pad, y0 + 12, max(0, right - 12 - pad), 20)
    return frames


def person_rename_frames(width: int, index: int) -> dict:
    """A person row in rename mode: the speaker rename row (field, Save, Cancel)."""
    return speaker_row_frames(width, index, "rename")


def people_footer_frames(width: int, state) -> dict:
    """Footer-local frames: the idle buttons, the enroll name row, or the recording row."""
    pad = PEOPLE_PAD
    if state.rec:
        cancel_x = width - pad - PEOPLE_FORGET_W
        stop_x = cancel_x - _GAP - PEOPLE_STOP_W
        return {
            "cancel": (cancel_x, 8, PEOPLE_FORGET_W, 28),
            "stop": (stop_x, 8, PEOPLE_STOP_W, 28),
            "status": (pad, 12, max(0, stop_x - 12 - pad), 20),
        }
    if state.mode == "enroll":
        frames = speaker_row_frames(width, 0, "rename")
        frames.pop("divider", None)
        frames["record"] = frames.pop("save")
        return frames
    forget_all_x = width - pad - PEOPLE_FORGET_ALL_W
    return {
        "enroll": (pad, 8, PEOPLE_ENROLL_W, 28),
        "forget_all": (forget_all_x, 8, PEOPLE_FORGET_ALL_W, 28),
        "export": (forget_all_x - _GAP - PEOPLE_EXPORT_W, 8, PEOPLE_EXPORT_W, 28),
    }
