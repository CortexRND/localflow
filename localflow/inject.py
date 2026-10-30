import time
import uuid
from dataclasses import dataclass

import pyperclip

from localflow.platform import current


@dataclass(frozen=True)
class SelectionCapture:
    text: str
    clipboard_before: str | None


def capture_selection() -> SelectionCapture:
    try:
        clipboard_before = pyperclip.paste()
    except Exception:
        clipboard_before = None
    sentinel = "\u2063localflow-" + uuid.uuid4().hex
    try:
        pyperclip.copy(sentinel)
    except Exception:
        return SelectionCapture("", clipboard_before)
    try:
        current().copy_selection()
        time.sleep(0.15)
        copied = pyperclip.paste()
    except Exception:
        copied = None
    # Selection capture intentionally supports plain text only.
    if copied == sentinel or copied is None:
        restore_clipboard_if_matches(sentinel, clipboard_before)
        return SelectionCapture("", clipboard_before)
    return SelectionCapture(copied, clipboard_before)


def restore_clipboard_if_matches(
    expected: str, clipboard_before: str | None
) -> None:
    if clipboard_before is None:
        return
    try:
        if pyperclip.paste() == expected:
            pyperclip.copy(clipboard_before)
    except Exception:
        pass


def paste_text(
    text: str, method: str = "auto", clipboard_before: str | None = None
) -> None:
    if method == "clipboard":
        try:
            pyperclip.copy(text)
        except Exception:
            pass
        return

    old_clipboard = clipboard_before
    try:
        if old_clipboard is None:
            try:
                old_clipboard = pyperclip.paste()
            except Exception:
                old_clipboard = None

        pyperclip.copy(text)

        current().paste(method)

        time.sleep(0.3)
    except Exception:
        pass
    finally:
        if old_clipboard is not None:
            try:
                if pyperclip.paste() == text:
                    pyperclip.copy(old_clipboard)
            except Exception:
                pass
