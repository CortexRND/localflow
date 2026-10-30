"""Render the AppKit hot-phrases window to PNGs (macOS only).

Used by the macOS UI workflow; writes artifacts/mac-ui/<state>-<light|dark>.png.
"""

import tempfile
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import numpy as np

from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore
from localflow.hub_mac import build_window as build_hub_window
from localflow.phrases_mac import build_window, snapshot_png
from localflow.setup_flow import Check
from localflow.setup_mac import SetupWindowController

OUT = Path("artifacts/mac-ui")


def _config(hot_phrases=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=True,
        spoken_punctuation=False,
        styles_enabled=False,
        style_other="formal",
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


def _hub_snapshots(base):
    for state in ("empty", "populated", "error"):
        history = HistoryStore(base / f"hub-{state}.sqlite", base / f"clips-{state}")
        dictionary = DictionaryStore(base / f"dictionary-{state}.json")
        config = Config()
        if state == "populated":
            history.add(
                kind="dictate", status="ok", app_name="Editor",
                raw_text="A raw transcript", final_text="A polished transcript.",
                duration_s=30,
                audio_path=history.write_clip(np.zeros(1600, dtype=np.float32), 16000),
            )
            history.add(
                kind="command", status="ok", app_name="Browser",
                raw_text="Make this concise", final_text="A concise selection.",
            )
            dictionary.add_word("LocalFlow")
            dictionary.add_replacement("teh", "the")
        controller = build_hub_window(history, dictionary, config)
        for page in ("Home", "History", "Dictionary", "Style", "Models"):
            controller.page_errors.clear()
            if state == "error":
                controller.page_errors[page] = "Unable to load local data."
            controller.show_page(page)
            _shot(controller, f"hub-{page.lower()}-{state}")


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

        setup.saved = False
        setup.step = 3
        setup.cleanup_level = "medium"
        setup._render()
        _shot(setup, "setup-cleanup-level-medium")

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
        _hub_snapshots(tmp)

    print(f"wrote snapshots to {OUT}")


if __name__ == "__main__":
    main()
