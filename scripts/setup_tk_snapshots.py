"""Capture each Tk setup screen and representative download states."""

import os
import threading
import time
import tkinter as tk
from dataclasses import replace
from pathlib import Path

from PIL import ImageGrab

from localflow import setup_tk
from localflow.config import Config
from localflow.models import HardwareInfo
from localflow.setup_flow import Check


class _Monitor:
    def __init__(self, _device):
        pass

    def start(self):
        pass

    def stop(self):
        pass

    def level(self):
        return 0.25


def main():
    output = Path(
        os.environ.get("LOCALFLOW_SHOTS_DIR", str(Path.home() / "rea-runs" / "shots-A"))
    )
    output.mkdir(parents=True, exist_ok=True)
    setup_tk.MicLevelMonitor = _Monitor
    root = tk.Tk()
    root.attributes("-topmost", True)
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 16, False, 50),
    )
    window.checks = [
        Check("ffmpeg", "ffmpeg", True, "Installed", "", ""),
        Check("ollama-installed", "Ollama installed", True, "Installed", "", ""),
        Check("ollama-running", "Ollama running", True, "Ready", "", ""),
        Check("microphone-device", "Input device", True, "1 input device found", "", ""),
    ]
    window.downloaded = {model.id: False for model in window.models}
    window._ollama_checked = True
    window.ollama_running = True
    window.ollama_models = ["llama3.2:1b"]
    window.llm_var.set("llama3.2:1b")

    def capture(name):
        root.update()
        time.sleep(0.15)
        bounds = (
            root.winfo_rootx(),
            root.winfo_rooty(),
            root.winfo_rootx() + root.winfo_width(),
            root.winfo_rooty() + root.winfo_height(),
        )
        ImageGrab.grab(bbox=bounds).save(output / f"{name}.png")

    try:
        for step in range(7):
            window.step = step
            if step == 5:
                window.status_message = "Hello, this is a local dictation test."
            elif step == 6:
                window.saved = True
                window.status_message = "Setup saved."
            window._render()
            capture(f"step-{step + 1}")
            if step == 6:
                capture("finish")

        window.saved = False
        window.step = 2
        window._busy = {"stt-download"}
        window._cancel = threading.Event()
        window.status_message = "43 / 78 MB"
        window._render()
        capture("download-in-progress")

        window._busy.clear()
        window._cancel = None
        window.status_message = ""
        window.error_message = "Network connection interrupted."
        window._render()
        capture("download-error")

        window.choices = replace(window.choices, llm_provider="openai-compatible")
        window.step = 3
        window.error_message = ""
        window._render()
        capture("non-ollama-provider")

        window.choices = replace(window.choices, llm_provider="ollama")
        window.step = 3
        window.ollama_running = False
        window.llm_var.set("llama3.2:1b")
        window.error_message = "Ollama isn't running — start it, or choose No cleanup."
        window._render()
        capture("ollama-offline")
    finally:
        root.destroy()
    print(f"wrote setup screenshots to {output}")


if __name__ == "__main__":
    main()
