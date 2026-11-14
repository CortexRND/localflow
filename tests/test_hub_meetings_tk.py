import time

import pytest

tk = pytest.importorskip("tkinter")

from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hub_tk import HubWindow
from localflow.meeting_client import MeetingClientError

TEMPLATES = {
    "default": "general",
    "templates": [
        {"id": "general", "name": "General"},
        {"id": "standup", "name": "Standup"},
    ],
}


class FakeMeetingClient:
    def __init__(self, state="idle", *, save_error=None, chat_error=None):
        self.state = state
        self.save_error = save_error
        self.chat_error = chat_error
        self.calls = []

    def templates(self):
        self.calls.append(("templates",))
        return TEMPLATES

    def status(self):
        self.calls.append(("status",))
        if self.state == "down":
            raise MeetingClientError("server unavailable")
        if self.state == "running":
            return {
                "session": {
                    "title": "Planning",
                    "template": "standup",
                    "user_notes": "Initial note",
                    "system_audio": "error",
                    "system_audio_error": "device unavailable",
                    "seconds": 185,
                    "segment_count": 2,
                    "segments": [
                        {"stamp": "00:04", "source": "me", "text": "I can take this"},
                        {"stamp": "01:12", "source": "others", "text": "Thanks"},
                    ],
                },
                "last_saved": None,
                "chat_available": False,
            }
        if self.state == "saved":
            return {
                "session": None,
                "last_saved": {
                    "notes_path": "/tmp/meeting.md", "enhanced": True,
                },
                "chat_available": True,
            }
        return {"session": None, "last_saved": None, "chat_available": False}

    def start(self, title, category, template, user_notes):
        self.calls.append(("start", title, category, template, user_notes))
        self.state = "running"
        return {"ok": True}

    def save_notes(self, notes):
        self.calls.append(("save_notes", notes))
        if self.save_error:
            raise self.save_error
        return {"ok": True}

    def stop(self):
        self.calls.append(("stop",))
        self.state = "saved"
        return {"notes_path": "/tmp/meeting.md", "enhanced": True}

    def chat(self, question):
        self.calls.append(("chat", question))
        if self.chat_error:
            raise self.chat_error
        return {"answer": "The launch is on Tuesday."}

    def dismiss(self):
        self.calls.append(("dismiss",))


def _window(tmp_path, client):
    try:
        root = tk.Tk()
    except tk.TclError as exc:
        pytest.skip(f"Tk unavailable: {exc}")
    root.withdraw()
    history = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    dictionary = DictionaryStore(tmp_path / "dictionary.json")
    window = HubWindow(
        root, history, dictionary, Config(), meeting_client=client,
        initial_page="Meetings",
    )
    return root, window


def _pump(root, condition=lambda: True, timeout=2):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        root.update()
        if condition():
            return
        time.sleep(0.01)
    root.update()


def test_meetings_idle_and_running_render(tmp_path):
    client = FakeMeetingClient("idle")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data))
        assert window.meeting_action.cget("text") == "Start meeting transcription"
        assert window.meeting_template_var.get() == "general"

        client.state = "running"
        window._meeting_poll()
        _pump(root, lambda: bool(window._meeting_status_data.get("session")))
        assert window.meeting_action.cget("text") == (
            "Stop & save notes — 3m05s · 2 segments"
        )
        assert "mic only — system audio unavailable" in window.meeting_watch.cget("text")
        assert "device unavailable" in window.meeting_watch.cget("text")
        assert "[00:04] Me: I can take this" in window.meeting_live.cget("text")
        assert "[01:12] Others: Thanks" in window.meeting_live.cget("text")
    finally:
        root.destroy()


def test_pre_start_notes_and_template_are_sent_when_starting(tmp_path):
    client = FakeMeetingClient("idle")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data))
        window.meeting_title_entry.delete(0, "end")
        window.meeting_title_entry.insert(0, "Planning")
        window.meeting_template_var.set("standup")
        window.meeting_notes.insert("1.0", "Bring the launch date")
        root.update()
        window._meeting_toggle()
        _pump(root, lambda: any(call[0] == "start" for call in client.calls))
        assert (
            "start", "Planning", "", "standup", "Bring the launch date"
        ) in client.calls
    finally:
        root.destroy()


def test_meetings_saved_chat_answer_and_error(tmp_path):
    client = FakeMeetingClient("saved")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data))
        assert "/tmp/meeting.md" in window.meeting_saved_label.cget("text")
        window.meeting_question.insert(0, "When is launch?")
        window._meeting_ask()
        _pump(root, lambda: "Tuesday" in window._meeting_chat[0]["answer"])
        assert "The launch is on Tuesday." in window.meeting_chat_answers.winfo_children()[0].cget("text")

        client.chat_error = MeetingClientError("LLM unavailable")
        window.meeting_question.insert(0, "Who owns it?")
        window._meeting_ask()
        _pump(root, lambda: bool(window._meeting_chat[0].get("error")))
        assert window._meeting_chat[0]["error"] is True
        assert "LLM unavailable" in window._meeting_chat[0]["answer"]
    finally:
        root.destroy()


def test_meetings_server_down_shows_retry(tmp_path):
    root, window = _window(tmp_path, FakeMeetingClient("down"))
    try:
        _pump(root, lambda: window.meeting_down.winfo_manager() == "pack")
        assert "server unavailable" in window.meeting_down_label.cget("text")
        assert any(
            child.cget("text") == "Retry"
            for child in window.meeting_down.winfo_children()
            if isinstance(child, tk.Button)
        )
    finally:
        root.destroy()


def test_stop_keeps_meeting_running_when_notes_cannot_be_saved(tmp_path):
    client = FakeMeetingClient(
        "running", save_error=MeetingClientError("disk full")
    )
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data.get("session")))
        window._meeting_toggle()
        _pump(root, lambda: any(call[0] == "save_notes" for call in client.calls))
        _pump(root, lambda: not window._meeting_busy)
        assert not any(call[0] == "stop" for call in client.calls)
        assert window._meeting_status_data.get("session")
        assert "disk full" in window.meeting_error.cget("text")
        assert window.meeting_notes.get("1.0", "end-1c") == "Initial note"
    finally:
        root.destroy()


def test_meeting_notes_autosave_uses_latest_text(tmp_path):
    client = FakeMeetingClient("running")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data.get("session")))
        window.meeting_notes.delete("1.0", "end")
        window.meeting_notes.insert("1.0", "Latest draft")
        root.update()
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and not any(
            call[0] == "save_notes" for call in client.calls
        ):
            root.update()
            time.sleep(0.02)
        assert ("save_notes", "Latest draft") in client.calls
    finally:
        root.destroy()
