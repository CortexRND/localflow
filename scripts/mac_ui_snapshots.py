"""Render the AppKit hot-phrases window to PNGs (macOS only).

Used by the macOS UI workflow; writes artifacts/mac-ui/<state>-<light|dark>.png.
"""

import tempfile
from pathlib import Path
from types import SimpleNamespace

from localflow.hotphrases import HotPhraseStore
from localflow.phrases_mac import build_window, snapshot_png

OUT = Path("artifacts/mac-ui")


def _config(hot_phrases=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=True,
        command_names=[],
        skills_dirs=[],
    )


def _controller(store_dir, hot_phrases=True):
    store = HotPhraseStore(store_dir / "hot_phrases.json")
    c = build_window(store, _config(hot_phrases))
    c.confirm_discard = lambda _err: True
    return c, store


def _shot(c, name):
    for dark in (False, True):
        snapshot_png(c, OUT / f"{name}-{'dark' if dark else 'light'}.png", dark=dark)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)

        c, _ = _controller(tmp / "empty")
        _shot(c, "empty")

        c, store = _controller(tmp / "list")
        store.add("review checklist", "Review this PR:\n- tests\n- types")
        store.add("sign off", "Thanks — talk tomorrow.", enabled=False)
        store.add("standup", "yesterday:\ntoday:\nblockers:")
        c.reload_sidebar()
        c.select_entry(store.list()[0]["id"])
        _shot(c, "list-editing")

        c, store = _controller(tmp / "validation")
        store.add("review checklist", "x")
        c.reload_sidebar()
        c.new_phrase()
        c.set_fields("Review, checklist", "y")
        c.flush_autosave()
        _shot(c, "validation-error")

        c, store = _controller(tmp / "test-drawer")
        store.add("review checklist", "CHECK: tests, lint, types")
        c.reload_sidebar()
        c.toggleTestArea_(c.disclosure)
        c.set_test_input("please review checklist now")
        _shot(c, "test-drawer-open")

        c, store = _controller(tmp / "undo")
        e = store.add("trig", "t")
        c.reload_sidebar()
        c.select_entry(e["id"])
        c.delete_current()
        _shot(c, "undo-bar")

        c, store = _controller(tmp / "search")
        store.add("alpha", "A")
        store.add("beta", "B")
        c.reload_sidebar()
        c.set_search("zzz")
        _shot(c, "search")

        c, _ = _controller(tmp / "off", hot_phrases=False)
        _shot(c, "hot-phrases-off-banner")

    print(f"wrote snapshots to {OUT}")


if __name__ == "__main__":
    main()
