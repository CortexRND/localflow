"""Smoke-test all seven setup screens with a local Tk display."""

import os
import sys
import threading
import time
from dataclasses import replace

import pytest

tk = pytest.importorskip("tkinter")
if sys.platform.startswith("linux") and not os.environ.get("DISPLAY"):
    pytest.skip("no display for Tk", allow_module_level=True)

from localflow import setup_tk
from localflow.config import Config
from localflow.models import HardwareInfo


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
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    window.downloaded = {model.id: True for model in window.models}
    window._ollama_checked = True
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
        strips = [child for child in window.body.winfo_children() if isinstance(child, tk.Frame)]
        assert any(
            isinstance(child, tk.Button) and child.cget("text") == "Retry"
            for strip in strips
            for child in strip.winfo_children()
        )
        window._ollama_checked = False
        window.choices = replace(window.choices, llm_provider="openai-compatible")
        window.step = 3
        window._render()
        root.update_idletasks()
        assert window._primary_label() == "Continue"
        assert any(
            isinstance(child, tk.Button) and child.cget("text") == "Change in Settings"
            for child in window.body.winfo_children()
        )
        assert "ollama-check" not in window._busy
        assert window._ollama_checked is False
    finally:
        root.destroy()


def test_stt_download_renders_once_and_advances_after_completion(
    monkeypatch, tmp_path
):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    completed = threading.Event()

    def download(_model, progress, _cancel):
        progress(10, 20)
        completed.set()
        return tmp_path / "fw-tiny"

    monkeypatch.setattr(setup_tk, "download_stt", download)
    try:
        window.step = 2
        window.downloaded = {model.id: False for model in window.models}
        window.stt_var.set("fw-tiny")
        window.error_message = "Earlier download error"
        window._render()
        window.primary_action()
        root.update_idletasks()

        assert window.error_message == ""
        primary_buttons = [
            child for child in window.footer.winfo_children()
            if isinstance(child, tk.Button)
            and child.cget("text") in {"Download & continue", "Downloading…"}
        ]
        assert len(primary_buttons) == 1
        assert primary_buttons[0].cget("text") == "Downloading…"
        assert primary_buttons[0].cget("state") == "disabled"
        assert any(
            isinstance(child, tk.Label)
            and str(child.cget("textvariable")) == str(window._progress_var)
            for child in _descendants(window.body)
        )
        assert any(
            isinstance(child, tk.Frame) and child.cget("bg") == setup_tk.TOKENS["sun"]
            for child in window.body.winfo_children()
        )
        window._select_stt("fw-base")
        assert window.stt_var.get() == "fw-tiny"
        assert not any(
            isinstance(child, tk.Button) and child.cget("text") == "Retry"
            for child in _descendants(window.body)
        )

        deadline = time.monotonic() + 2
        while window.step == 2 and time.monotonic() < deadline:
            root.update()
            time.sleep(0.01)

        assert completed.is_set()
        assert window.downloaded["fw-tiny"]
        assert not window.downloaded["fw-base"]
        assert window.step == 3
    finally:
        root.destroy()


def test_ollama_pull_advances_after_completion(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: True)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    completed = threading.Event()

    class FakeOllama:
        def __init__(self, _base_url):
            pass

        def pull(self, _model, progress, _cancel):
            progress(10, 20, "downloading")
            completed.set()

    monkeypatch.setattr(setup_tk, "OllamaClient", FakeOllama)
    try:
        window.step = 3
        window._ollama_checked = True
        window.ollama_running = True
        window.ollama_models = []
        window.llm_var.set("llama3.2:1b")
        window._render()
        window.primary_action()

        deadline = time.monotonic() + 2
        while window.step == 3 and time.monotonic() < deadline:
            root.update()
            time.sleep(0.01)

        assert completed.is_set()
        assert "llama3.2:1b" in window.ollama_models
        assert window.step == 4
    finally:
        root.destroy()


def test_continue_from_microphone_step_stops_and_clears_monitor(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    monitor = FakeMonitor("")
    try:
        window.step = 4
        window._monitor = monitor
        window._monitor_started = True

        window._continue()

        assert monitor.stopped
        assert window._monitor is None
        assert not window._monitor_started
        assert window.step == 5
    finally:
        root.destroy()


def test_cleanup_step_blocks_offline_ollama_selection(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    try:
        window.step = 3
        window.choices = replace(window.choices, llm_provider="ollama")
        window.llm_var.set("llama3.2:1b")
        window.ollama_running = False
        window._ollama_checked = True

        window.primary_action()

        assert window.step == 3
        assert window.error_message == (
            "Ollama isn't running — start it, or choose No cleanup."
        )
        assert window._progress_var.get() == window.error_message
    finally:
        root.destroy()


def test_back_is_disabled_while_stt_or_ollama_download_is_busy(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: True)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    try:
        window.downloaded = {"fw-tiny": False}
        window._ollama_checked = True
        window.ollama_running = True
        for step, job in ((2, "stt-download"), (3, "llm-pull")):
            window.step = step
            window._busy = {job}
            window._cancel = threading.Event()
            window._render()
            back = next(
                child for child in window.footer.winfo_children()
                if isinstance(child, tk.Button) and child.cget("text") == "Back"
            )

            assert back.cget("state") == "disabled"
            window.back()
            assert window.step == step
    finally:
        root.destroy()


def test_microphone_error_stays_inline_and_clears_on_navigation(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Linux", "x86_64", 8, False, 50),
    )
    monitor = FakeMonitor("")
    try:
        window.step = 4
        window._monitor = monitor
        window._monitor_started = True
        window._ollama_checked = True
        window._events.put(
            ("error", "monitor", RuntimeError("Error querying device -1"))
        )

        window._poll_events()

        assert window._mic_error == "Error querying device -1"
        assert window.error_message == ""
        assert window.status_message == ""
        assert any(
            isinstance(widget, tk.Label)
            and widget.cget("text") == "Error querying device -1"
            for widget in _descendants(window.body)
        )

        window.back()

        assert window.step == 3
        assert window._mic_error == ""
        assert window.error_message == ""
        assert window.status_message == ""
        assert "Error querying device -1" not in window._progress_var.get()
    finally:
        root.destroy()


def test_finish_reads_start_on_login_checkbox_before_apply(monkeypatch):
    monkeypatch.setattr(setup_tk.setup_flow, "run_checks", lambda _cfg: [])
    monkeypatch.setattr(setup_tk.setup_flow, "ollama_installed", lambda: False)
    monkeypatch.setattr(setup_tk, "list_input_devices", list)
    monkeypatch.setattr(setup_tk, "MicLevelMonitor", FakeMonitor)
    root = tk.Tk()
    root.withdraw()
    window = setup_tk.SetupWindow(
        root,
        Config(),
        HardwareInfo("Darwin", "arm64", 8, True, 50),
    )
    applied = []
    try:
        window.step = 6
        window.choices = replace(window.choices, start_on_login=False)
        window.start_login_var.set(True)
        monkeypatch.setattr(
            setup_tk.setup_flow, "apply", lambda choices: applied.append(choices)
        )
        monkeypatch.setattr(
            window, "_start_job", lambda _key, function: function()
        )

        window.primary_action()

        assert applied[0].start_on_login is True
    finally:
        root.destroy()


def _descendants(widget):
    for child in widget.winfo_children():
        yield child
        yield from _descendants(child)


def test_download_stall_message_after_timeout():
    window = setup_tk.SetupWindow.__new__(setup_tk.SetupWindow)
    now = [0.0]
    window._clock = lambda: now[0]
    window._stall_timeout = 30.0
    window._last_progress_at = 0.0
    window._progress_stalled = False
    window._busy = {"stt-download"}
    window.status_message = ""
    window.error_message = ""
    window._render = lambda: None

    now[0] = 30.0
    window._check_download_stall()

    assert window.status_message == "No progress for 30s — check your connection, or Cancel."
