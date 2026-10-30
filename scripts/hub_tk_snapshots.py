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
        history = HistoryStore(base / "history.sqlite", base / "clips")
        dictionary = DictionaryStore(base / "dictionary.json")
        root = tk.Tk()
        root.attributes("-topmost", True)
        window = HubWindow(root, history, dictionary, Config())

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
                output / f"{page.lower()}-{state}.png"
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
                audio_path=history.write_clip(np.zeros(1600, dtype=np.float32), 16000),
            )
            history.add(
                kind="command",
                status="ok",
                app_name="Browser",
                raw_text="Make this concise",
                final_text="A concise selection.",
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
