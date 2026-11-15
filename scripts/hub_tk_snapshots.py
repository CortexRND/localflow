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


class SnapshotMeetingClient:
    def __init__(self):
        self.state = "idle"
        self.status_calls = 0

    def templates(self):
        return {
            "default": "general",
            "templates": [
                {"id": "general", "name": "General"},
                {"id": "standup", "name": "Standup"},
            ],
        }

    def status(self):
        self.status_calls += 1
        if self.state == "running":
            return {
                "session": {
                    "title": "Planning",
                    "template": "standup",
                    "user_notes": "Check launch date",
                    "system_audio": "error",
                    "system_audio_error": "System audio permission unavailable",
                    "seconds": 185,
                    "segment_count": 2,
                    "segments": [
                        {"stamp": "00:04", "source": "me", "text": "I can take this"},
                        {"stamp": "01:12", "source": "others", "text": "Sounds good"},
                    ],
                },
                "last_saved": None,
                "chat_available": False,
            }
        if self.state == "saved":
            return {
                "session": None,
                "last_saved": {
                    "notes_path": "/home/ubuntu/Meeting notes/Planning.md",
                    "enhanced": True,
                },
                "chat_available": True,
            }
        return {"session": None, "last_saved": None, "chat_available": False}

    def start(self, *_args):
        return {"ok": True}

    def save_notes(self, _notes):
        return {"ok": True}

    def stop(self):
        return {"notes_path": "/home/ubuntu/Meeting notes/Planning.md"}

    def chat(self, _question):
        return {"answer": "The launch date is March 3."}

    def dismiss(self):
        return None


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
            meeting_client = SnapshotMeetingClient()
            window = HubWindow(
                root, history, dictionary, Config(), theme=theme,
                meeting_client=meeting_client,
            )

            def capture(page, state):
                if page == "Meetings":
                    meeting_client.state = "idle"
                    window._meeting_status_data = {}
                window.show_page(page)
                if page == "Meetings":
                    deadline = time.monotonic() + 2
                    while (
                        time.monotonic() < deadline
                        and not window._meeting_status_data
                    ):
                        root.update()
                        time.sleep(0.01)
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
                for state in ("idle", "running", "saved"):
                    meeting_client.state = state
                    meeting_client.status_calls = 0
                    if state == "saved":
                        window._meeting_notes_text = ""
                        window._meeting_notes_dirty = False
                        if hasattr(window, "meeting_notes"):
                            window.meeting_notes.delete("1.0", "end")
                    window._meeting_status_data = {}
                    window.show_page("Meetings")
                    deadline = time.monotonic() + 2
                    while (
                        time.monotonic() < deadline
                        and not window._meeting_status_data
                    ):
                        root.update()
                        time.sleep(0.01)
                    root.update()
                    time.sleep(0.1)
                    bounds = (
                        root.winfo_rootx(),
                        root.winfo_rooty(),
                        root.winfo_rootx() + root.winfo_width(),
                        root.winfo_rooty() + root.winfo_height(),
                    )
                    ImageGrab.grab(bbox=bounds).save(
                        output / f"hub-meetings-{state}-{theme}.png"
                    )
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
