import time
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
    try:
        current().copy_selection()
        time.sleep(0.15)
        copied = pyperclip.paste()
    except Exception:
        copied = None
    return SelectionCapture(
        copied if copied is not None and copied != clipboard_before else "",
        clipboard_before,
    )


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
