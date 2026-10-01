"""Hot-phrase editor entrypoint. On macOS the native AppKit window
(phrases_mac) is used when PyObjC is available; everywhere else — and as
the fallback there — the Tk window (phrases_tk) opens. `python -m
localflow.phrases_window` keeps working for the .app launcher, the menu
bar, and `lf phrases`.
"""

import sys


def main(path=None):
    if sys.platform == "darwin":
        try:
            from localflow.phrases_mac import main as mac_main
        except ImportError:
            pass  # no PyObjC -> Tk fallback
        else:
            return mac_main(path)
    from localflow.phrases_tk import main as tk_main

    return tk_main(path)


if __name__ == "__main__":
    main()
