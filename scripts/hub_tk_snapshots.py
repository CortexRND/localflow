"""Capture empty, populated, and error states for every Tk hub page."""

import os
import tempfile
import time
import tkinter as tk
from pathlib import Path

import numpy as np
from PIL import ImageGrab

from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hub_tk import PAGES, HubWindow


def main():
    output = Path(
        os.environ.get(
            "LOCALFLOW_HUB_SHOTS",
            str(Path.home() / "rea-runs" / "shots-D" / "hub-tk"),
        )
    )
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as directory:
        base = Path(directory)
        for theme in ("light", "dark"):
            history = HistoryStore(
                base / f"history-{theme}.sqlite", base / f"clips-{theme}"
            )
            dictionary = DictionaryStore(base / f"dictionary-{theme}.json")
            root = tk.Tk()
            root.attributes("-topmost", True)
            window = HubWindow(
                root, history, dictionary, Config(), theme=theme
            )

            def capture(page, state):
                window.show_page(page)
                root.update()
                time.sleep(0.1)
                bounds = (
                    root.winfo_rootx(),
                    root.winfo_rooty(),
                    root.winfo_rootx() + root.winfo_width(),
                    root.winfo_rooty() + root.winfo_height(),
                )
                ImageGrab.grab(bbox=bounds).save(
                    output / f"hub-{page.lower()}-{state}-{theme}.png"
                )

            try:
                for page in PAGES:
                    capture(page, "empty")
                history.add(
                    kind="dictate",
                    status="ok",
                    app_name="Editor",
                    raw_text="A raw transcript",
                    final_text="A polished transcript.",
                    duration_s=30,
                    audio_path=history.write_clip(
                        np.zeros(1600, dtype=np.float32), 16000
                    ),
                )
                history.add(
                    kind="command",
                    status="ok",
                    app_name="Browser",
                    raw_text="Make this concise",
                    final_text="A concise selection.",
                )
                for index in range(10):
                    history.add(
                        kind="dictate",
                        status="ok",
                        app_name="Editor",
                        raw_text=f"Raw transcript example {index + 1}",
                        final_text=f"Polished dictation example {index + 1}.",
                        duration_s=18,
                    )
                dictionary.add_word("LocalFlow")
                dictionary.add_replacement("teh", "the")
                for page in PAGES:
                    capture(page, "populated")
                for page in PAGES:
                    window.page_errors[page] = "Unable to load local data."
                    capture(page, "error")
            finally:
                root.destroy()
    print(f"wrote Tk hub screenshots to {output}")


if __name__ == "__main__":
    main()
