import time

import pyperclip

from localflow.platform import current


def paste_text(text: str, method: str = "auto") -> None:
    if method == "clipboard":
        try:
            pyperclip.copy(text)
        except Exception:
            pass
        return

    old_clipboard = None
    try:
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
                pyperclip.copy(old_clipboard)
            except Exception:
                pass
