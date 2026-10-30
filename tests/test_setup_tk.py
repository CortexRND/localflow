"""Smoke-test all seven setup screens with a local Tk display."""

import os
import sys

import pytest

tk = pytest.importorskip("tkinter")
if sys.platform.startswith("linux") and not os.environ.get("DISPLAY"):
    pytest.skip("no display for Tk", allow_module_level=True)

from localflow import setup_tk  # noqa: E402
from localflow.config import Config  # noqa: E402
from localflow.models import HardwareInfo  # noqa: E402


class FakeMonitor:
    def __init__(self, _device):
        self.stopped = False

    def start(self):
        return None

    def stop(self):
        self.stopped = True

    def level(self):
        return 0.2


def test_setup_window_renders_all_seven_steps(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", lambda: [])
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    window.downloaded = {model.id: True for model in window.models}
    window.ollama_checked = True
    rendered = []
    try:
        for step in range(7):
            window.step = step
            window._render()
            root.update_idletasks()
            assert window.title_label.cget("text")
            assert window.primary_button.winfo_exists()
            rendered.append(window.title_label.cget("text"))
        assert rendered == [
            "Let's get you talking.",
            "A quick setup check.",
            "Pick your speech model.",
            "Choose your cleanup model.",
            "Mic and hotkey.",
            "Give it a try.",
            "All set.",
        ]
        window.step = 2
        window.error_message = "network unavailable"
        window._render()
        assert any(
            isinstance(child, tk.Button) and child.cget("text") == "Retry"
            for child in window.body.winfo_children()
        )
    finally:
        root.destroy()
