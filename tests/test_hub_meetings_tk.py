import gc
import sys
import threading
import time

import pytest

tk = pytest.importorskip("tkinter")
pytestmark = pytest.mark.skipif(
    sys.platform == "darwin",
    reason=(
        "macOS uses the AppKit Hub, covered by test_hub_mac_static and the "
        "mac UI snapshots."
    ),
)

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
        self.auto_titles = []  # the optional auto_title flag of each start

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

    def start(self, title, category, template, user_notes, auto_title=None):
        self.calls.append(("start", title, category, template, user_notes))
        self.auto_titles.append(auto_title)
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


class _ManualResult:
    def __init__(self):
        self.event = threading.Event()
        self.error = None
        self.completed = False

    def wait(self):
        if not self.event.wait(timeout=5):
            raise TimeoutError("manual meeting request was not completed")
        self.completed = True
        if self.error:
            raise self.error
        return {"ok": True}

    def complete(self, error=None):
        self.error = error
        self.event.set()


class ManualMeetingClient(FakeMeetingClient):
    def __init__(self, state="idle"):
        super().__init__(state)
        self.pending_saves = []
        self.pending_starts = []
        self.completed_saves = 0
        self._manual_lock = threading.Lock()
        self._available_saves = [_ManualResult() for _ in range(4)]
        self._available_starts = [_ManualResult() for _ in range(2)]

    def start(self, title, category, template, user_notes, auto_title=None):
        with self._manual_lock:
            pending = self._available_starts.pop(0)
            self.pending_starts.append(pending)
        self.calls.append(("start", title, category, template, user_notes))
        self.auto_titles.append(auto_title)
        result = pending.wait()
        self.state = "running"
        return result

    def save_notes(self, notes):
        with self._manual_lock:
            pending = self._available_saves.pop(0)
            self.pending_saves.append(pending)
        self.calls.append(("save_notes", notes))
        result = pending.wait()
        self.completed_saves += 1
        return result

    def complete_save(self, index, error=None):
        self.pending_saves[index].complete(error)

    def complete_start(self, index, error=None):
        self.pending_starts[index].complete(error)


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
        root.tk.dooneevent(tk._tkinter.DONT_WAIT)
        if condition():
            return
        time.sleep(0.01)


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


def test_meeting_result_dispatch_stops_when_requests_finish(tmp_path):
    client = FakeMeetingClient("idle")
    root, window = _window(tmp_path, client)
    try:
        _pump(
            root,
            lambda: bool(window._meeting_status_data)
            and bool(window._meeting_templates),
        )
        assert window._meeting_requests_in_flight == 0
        assert window._meeting_dispatch_after is None
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
        _pump(root, lambda: window._meeting_notes_text == "Bring the launch date")
        window._meeting_toggle()
        _pump(root, lambda: any(call[0] == "start" for call in client.calls))
        assert (
            "start", "Planning", "", "standup", "Bring the launch date"
        ) in client.calls
        assert client.auto_titles == [False]  # a typed title is the user's
    finally:
        root.destroy()


@pytest.mark.parametrize("typed,title,auto", [
    ("", "Meeting", True),           # left blank (the new default): named on save
    ("   ", "Meeting", True),
    ("Meeting", "Meeting", False),   # typed "Meeting" is kept, never replaced
    ("Zoom meeting", "Zoom meeting", False),
])
def test_tk_title_is_automatic_only_when_blank(tmp_path, typed, title, auto):
    client = FakeMeetingClient("idle")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data))
        assert window.meeting_title_entry.get() == ""  # no prefilled "Meeting"
        window.meeting_title_entry.insert(0, typed)
        window._meeting_toggle()
        _pump(root, lambda: any(call[0] == "start" for call in client.calls))
        start = next(call for call in client.calls if call[0] == "start")
        assert start[1] == title and client.auto_titles == [auto]
        _pump(root, lambda: window._meeting_requests_in_flight == 0)
    finally:
        root.destroy()
        gc.collect()


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


def test_stop_waits_for_inflight_autosave_then_flushes_latest_notes(tmp_path):
    client = ManualMeetingClient("running")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data.get("session")))
        window.meeting_notes.delete("1.0", "end")
        window.meeting_notes.insert("1.0", "a")
        root.update()
        _pump(root, lambda: ("save_notes", "a") in client.calls, timeout=10)
        assert ("save_notes", "a") in client.calls

        window.meeting_notes.insert("end", "b")
        root.update()
        window._meeting_toggle()
        assert [
            call for call in client.calls if call[0] == "save_notes"
        ] == [("save_notes", "a")]
        assert not any(call[0] == "stop" for call in client.calls)

        client.complete_save(0)
        _pump(
            root,
            lambda: ("save_notes", "ab") in client.calls,
            timeout=10,
        )
        assert ("save_notes", "ab") in client.calls
        assert [
            call for call in client.calls if call[0] == "save_notes"
        ] == [("save_notes", "a"), ("save_notes", "ab")]
        assert not any(call[0] == "stop" for call in client.calls)

        client.complete_save(1)
        _pump(
            root,
            lambda: any(call[0] == "stop" for call in client.calls),
            timeout=10,
        )
        assert client.completed_saves == 2
        assert client.calls.index(("stop",)) > client.calls.index(
            ("save_notes", "ab")
        )
    finally:
        root.destroy()


def test_start_completion_after_navigating_away_restores_action(tmp_path):
    client = ManualMeetingClient()
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data))
        window._meeting_toggle()
        _pump(
            root,
            lambda: any(call[0] == "start" for call in client.calls),
            timeout=10,
        )
        assert any(call[0] == "start" for call in client.calls)
        window.show_page("Home")

        client.complete_start(0)
        _pump(root, lambda: not window._meeting_busy, timeout=10)
        assert not window._meeting_busy
        window.show_page("Meetings")
        _pump(
            root,
            lambda: bool(window._meeting_status_data.get("session")),
            timeout=10,
        )

        assert not window._meeting_busy
        assert window.meeting_action.cget("state") == "normal"
    finally:
        root.destroy()


def test_stop_flush_completion_after_navigating_away_still_stops(tmp_path):
    client = ManualMeetingClient("running")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data.get("session")))
        window._meeting_toggle()
        _pump(root, lambda: ("save_notes", "Initial note") in client.calls, timeout=10)
        assert ("save_notes", "Initial note") in client.calls
        window.show_page("Home")

        client.complete_save(0)
        _pump(
            root,
            lambda: any(call[0] == "stop" for call in client.calls),
            timeout=10,
        )
        assert ("stop",) in client.calls
    finally:
        root.destroy()


def _opened_paths(monkeypatch):
    opened = []

    class FakePlatform:
        def open_path(self, target):
            opened.append(target)

    monkeypatch.setattr("localflow.platform.current", lambda: FakePlatform())
    return opened


def _use_vault(monkeypatch, vault):
    monkeypatch.setattr(
        "localflow.hub_tk.load_config", lambda: Config(vault_path=str(vault))
    )


@pytest.mark.parametrize(
    "notes_path", ["/usr/bin/xterm", "C:\\Windows\\System32\\calc.exe"]
)
def test_open_notes_ignores_paths_outside_the_vault(tmp_path, monkeypatch, notes_path):
    opened = _opened_paths(monkeypatch)
    root, window = _window(tmp_path, FakeMeetingClient("idle"))
    try:
        _use_vault(monkeypatch, tmp_path)
        window._meeting_last_saved = {"notes_path": notes_path}
        window._meeting_open_notes()
        assert opened == []
    finally:
        root.destroy()


def test_open_notes_opens_markdown_inside_the_vault(tmp_path, monkeypatch):
    opened = _opened_paths(monkeypatch)
    note = tmp_path / "vault" / "Meetings" / "Daily.md"
    note.parent.mkdir(parents=True)
    note.write_text("# Daily\n")
    root, window = _window(tmp_path, FakeMeetingClient("idle"))
    try:
        _use_vault(monkeypatch, tmp_path / "vault")
        window._meeting_last_saved = {"notes_path": str(note)}
        window._meeting_open_notes()
        assert opened == [str(note.resolve())]
    finally:
        root.destroy()


# ------------------------------------------------------- speaker labels ---

READY = {"extra_installed": True, "models_ready": True, "reason": None}
NO_MODELS = {"extra_installed": True, "models_ready": False,
             "reason": "run `lf speakers download`"}


class DiarizationClient(FakeMeetingClient):
    def __init__(self, diarization):
        super().__init__("idle")
        self.diarization = diarization

    def diarization_status(self):
        self.calls.append(("diarization_status",))
        if isinstance(self.diarization, Exception):
            raise self.diarization
        return self.diarization


def _speaker_window(tmp_path, client):
    root, window = _window(tmp_path, client)
    _pump(root, lambda: ("diarization_status",) in client.calls
          and window._meeting_requests_in_flight == 0)
    return root, window


def _saved_configs(monkeypatch, *, error=None):
    from localflow import hub_tk

    saved = []

    def save(config):
        if error:
            raise error
        saved.append(config)

    monkeypatch.setattr(hub_tk, "load_config", Config)
    monkeypatch.setattr(hub_tk, "save_config", save)
    return saved


def test_speaker_toggles_are_enabled_and_save_when_ready(tmp_path, monkeypatch):
    saved = _saved_configs(monkeypatch)
    root, window = _speaker_window(tmp_path, DiarizationClient(READY))
    try:
        checks = window.meeting_speaker_checks
        assert set(checks) == {"speaker_diarization", "diarize_mic"}
        assert checks["speaker_diarization"].cget("text") == "Speaker labels"
        assert checks["diarize_mic"].cget("text") == "Label speakers on my microphone"
        assert all(check.cget("state") == "normal" for check in checks.values())
        assert not window.meeting_speaker_reason.winfo_manager()
        checks["speaker_diarization"].invoke()
        assert saved and saved[-1].speaker_diarization is True
        assert saved[-1].diarize_mic is False
        assert window.config.speaker_diarization is True
    finally:
        root.destroy()


def test_speaker_toggles_are_disabled_with_a_reason_when_not_ready(tmp_path, monkeypatch):
    saved = _saved_configs(monkeypatch)
    root, window = _speaker_window(tmp_path, DiarizationClient(NO_MODELS))
    try:
        checks = window.meeting_speaker_checks
        assert all(check.cget("state") == "disabled" for check in checks.values())
        assert window.meeting_speaker_reason.winfo_manager() == "pack"
        assert window.meeting_speaker_reason.cget("text") == "run `lf speakers download`"
        checks["speaker_diarization"].invoke()  # disabled: does nothing
        assert saved == []
    finally:
        root.destroy()


def test_speaker_toggle_already_on_stays_usable_when_not_ready(tmp_path, monkeypatch):
    _saved_configs(monkeypatch)
    try:
        root = tk.Tk()
    except tk.TclError as exc:
        pytest.skip(f"Tk unavailable: {exc}")
    root.withdraw()
    client = DiarizationClient({"extra_installed": False, "models_ready": False})
    window = HubWindow(
        root, HistoryStore(tmp_path / "h.sqlite", tmp_path / "clips"),
        DictionaryStore(tmp_path / "d.json"), Config(speaker_diarization=True),
        meeting_client=client, initial_page="Meetings",
    )
    try:
        _pump(root, lambda: ("diarization_status",) in client.calls
              and window._meeting_requests_in_flight == 0)
        checks = window.meeting_speaker_checks
        assert checks["speaker_diarization"].cget("state") == "normal"
        assert checks["diarize_mic"].cget("state") == "disabled"
        assert window.meeting_speaker_reason.cget("text") == "Install localflow[diarization]"
    finally:
        root.destroy()


def test_speaker_toggles_stay_enabled_when_the_status_is_unavailable(tmp_path):
    client = DiarizationClient(MeetingClientError("server unavailable"))
    root, window = _speaker_window(tmp_path, client)
    try:
        checks = window.meeting_speaker_checks
        assert all(check.cget("state") == "normal" for check in checks.values())
        assert not window.meeting_speaker_reason.winfo_manager()
        assert window.meeting_error.cget("text") == ""  # no error surfaced
    finally:
        root.destroy()


def test_plain_client_without_diarization_status_is_not_asked(tmp_path):
    client = FakeMeetingClient("idle")
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data)
              and window._meeting_requests_in_flight == 0)
        assert ("diarization_status",) not in client.calls
        checks = window.meeting_speaker_checks
        assert all(check.cget("state") == "normal" for check in checks.values())
    finally:
        root.destroy()


def test_speaker_toggle_save_failure_reverts_the_switch(tmp_path, monkeypatch):
    _saved_configs(monkeypatch, error=OSError("disk full"))
    root, window = _speaker_window(tmp_path, DiarizationClient(READY))
    try:
        window.meeting_speaker_checks["diarize_mic"].invoke()
        assert window.meeting_speaker_vars["diarize_mic"].get() is False
        assert "disk full" in window._meeting_error_message
    finally:
        root.destroy()
