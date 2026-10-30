"""Render the AppKit hot-phrases window to PNGs (macOS only).

Used by the macOS UI workflow; writes artifacts/mac-ui/<state>-<light|dark>.png.
"""

import tempfile
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

from localflow.hotphrases import HotPhraseStore
from localflow.phrases_mac import build_window, snapshot_png
from localflow.setup_mac import SetupWindowController
from localflow.setup_flow import Check

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
        c.select_entry(store.list()[0]["id"])
        c.open_test_area(True)
        c.set_test_input("please review checklist now")
        _shot(c, "test-drawer-open")

        c, store = _controller(tmp / "undo")
        e = store.add("trig", "t")
        c.reload_sidebar()
        c.select_entry(e["id"])
        c.delete_current()
        _shot(c, "undo-bar")

        c, store = _controller(tmp / "search")
        store.add("review checklist", "Review this PR")
        store.add("sign off", "Thanks")
        store.add("standup", "yesterday:\ntoday:")
        c.reload_sidebar()
        c.set_search("stand")
        c.select_entry(store.list()[-1]["id"])
        _shot(c, "search")
        c.set_search("zzz")
        _shot(c, "search-no-matches")

        c, store = _controller(tmp / "off", hot_phrases=False)
        store.add("review checklist", "CHECK")  # banner needs the editor visible
        c.reload_sidebar()
        c.select_entry(store.list()[0]["id"])
        _shot(c, "hot-phrases-off-banner")

        setup = SetupWindowController.alloc().init()
        setup.snapshot_mode = True
        setup.checks = [
            Check("ffmpeg", "ffmpeg", True, "Installed", "", ""),
            Check("ollama-installed", "Ollama installed", True, "Installed", "", ""),
            Check("ollama-running", "Ollama running", True, "Ready", "", ""),
            Check("microphone-device", "Input device", True, "1 input device found", "", ""),
        ]
        setup.downloaded = {model.id: True for model in setup.models}
        setup.ollama_checked = True
        setup.ollama_running = True
        setup.ollama_models = ["llama3.2:1b"]
        setup.llm_id = "llama3.2:1b"
        setup.status_message = ""
        for step in range(7):
            setup.step = step
            setup._render()
            _shot(setup, f"setup-step-{step + 1}")
        setup.saved = True
        setup._render()
        _shot(setup, "setup-complete")

        setup.choices = replace(setup.choices, llm_provider="openai-compatible")
        setup.step = 3
        setup.saved = False
        setup._render()
        _shot(setup, "setup-non-ollama-provider")

        setup.choices = replace(setup.choices, llm_provider="ollama")
        setup.step = 2
        setup.busy.add("stt-download")
        setup.status_message = "43 / 78 MB"
        setup.error_message = ""
        setup._render()
        _shot(setup, "setup-download-progress")

        setup.busy.clear()
        setup.status_message = "Download failed."
        setup.error_message = "Network connection interrupted."
        setup._render()
        _shot(setup, "setup-download-error")

    print(f"wrote snapshots to {OUT}")


if __name__ == "__main__":
    main()
