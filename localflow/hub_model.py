"""AppKit-free logic behind the native hub pages.

Kept separate from hub_mac so the rules (dictionary validation, prompt review,
responsive sizing) can be unit-tested on any platform.
"""

from localflow.dictionary import DictionaryError
from localflow.dispatch import (
    APPROVE_FROM,
    REJECT_FROM,
    PromptTransitionError,
    approve_prompt,
    reject_prompt,
)

# Width below which the Home stat cards wrap from four columns into two.
STAT_FOUR_COLUMN_MIN_CARD = 150
STAT_GAP = 12


def stat_layout(width: int, count: int = 4) -> tuple[int, int]:
    """(columns, card_width) for `count` stat cards across `width` points."""
    columns = count
    while columns > 1 and (width - STAT_GAP * (columns - 1)) // columns < STAT_FOUR_COLUMN_MIN_CARD:
        columns //= 2
    return columns, (width - STAT_GAP * (columns - 1)) // columns


def meeting_notes_height(viewport_height: int, top: int) -> int:
    """Height of the meeting notes box: roomy when tall, never below 110."""
    return int(max(110, min(220, viewport_height - top - 330)))


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
    """The three prompt shortcuts, read from the config's current hotkeys.

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


def prompt_rows(entries: list[dict]) -> list[dict]:
    """Newest first. Buttons follow the same guards the server API enforces."""
    rows = []
    for entry in reversed(entries):
        status = entry.get("status") or "pending"
        context = prompt_context(entry)
        if status == "failed" and entry.get("error"):
            context = f'{context} — {entry["error"]}'
        rows.append(
            {
                "id": entry.get("id", ""),
                "title": entry.get("title") or "Untitled prompt",
                "context": context,
                "status": status,
                "status_label": status.title(),
                "can_approve": status in APPROVE_FROM,
                "can_reject": status in REJECT_FROM,
            }
        )
    return rows


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
