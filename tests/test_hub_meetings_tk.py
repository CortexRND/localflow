import gc
import sys
import threading
import time
from types import SimpleNamespace

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


SPEAKER_MID = "2026-11-25-1430-planning"


def _default_speakers():
    return [
        {"id": "S1", "label": "Speaker 1", "seconds": 72.4, "source": "others"},
        {"id": "S2", "label": "Speaker 2", "seconds": 45, "source": "others"},
    ]


class FakeMeetingClient:
    def __init__(
        self, state="idle", *, save_error=None, chat_error=None,
        diarization: dict | None = None,
    ):
        self.state = state
        self.save_error = save_error
        self.chat_error = chat_error
        self.diarization = diarization  # merged into the running session
        self.calls = []
        self.auto_titles = []  # the optional auto_title flag of each start
        # "saved-speakers": the saved meeting's speakers, changed by rename/merge.
        self.speakers = _default_speakers()
        self.speaker_error = None

    def _speakers_payload(self):
        return {"meeting_id": SPEAKER_MID, "speakers": [dict(s) for s in self.speakers]}

    def rename_speaker(self, meeting_id, speaker_id, name):
        self.calls.append(("rename_speaker", meeting_id, speaker_id, name))
        self._speaker_wait()
        if self.speaker_error:
            raise self.speaker_error
        for speaker in self.speakers:
            if speaker["id"] == speaker_id:
                speaker["label"] = name
        return self._speakers_payload()

    def merge_speakers(self, meeting_id, from_id, into_id):
        self.calls.append(("merge_speakers", meeting_id, from_id, into_id))
        self._speaker_wait()
        if self.speaker_error:
            raise self.speaker_error
        source = next(s for s in self.speakers if s["id"] == from_id)
        self.speakers.remove(source)
        for speaker in self.speakers:
            if speaker["id"] == into_id:
                speaker["seconds"] += source["seconds"]
        return self._speakers_payload()

    def _speaker_wait(self):
        pass

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
                    **(self.diarization or {}),
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
        if self.state == "saved-speakers":
            return {
                "session": None,
                "last_saved": {
                    "notes_path": "/tmp/meeting.md", "enhanced": True,
                    **self._speakers_payload(),
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


class GatedSpeakerClient(FakeMeetingClient):
    """Holds rename/merge until `release_all()`, for in-flight tests."""

    def __init__(self, state="saved-speakers"):
        super().__init__(state)
        self.gate = threading.Event()

    def _speaker_wait(self):
        if not self.gate.wait(timeout=5):
            raise TimeoutError("speaker request was not released")

    def release_all(self):
        self.gate.set()


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


RLO = chr(0x202E)  # right-to-left override
LEGACY_WATCH = "Planning · mic only — system audio unavailable — device unavailable"


def _running(tmp_path, diarization):
    client = FakeMeetingClient("running", diarization=diarization)
    root, window = _window(tmp_path, client)
    _pump(root, lambda: bool(window._meeting_status_data.get("session")))
    return client, root, window


def _pady(widget):
    pady = widget.pack_info()["pady"]
    values = pady if isinstance(pady, tuple) else str(pady).split()
    return tuple(int(str(value)) for value in values)


@pytest.mark.parametrize("diarization", [None, {"diarization": "off"}])
def test_speaker_labels_off_or_absent_leave_the_page_as_before(tmp_path, diarization):
    _client, root, window = _running(tmp_path, diarization)
    try:
        assert window.meeting_watch.cget("text") == LEGACY_WATCH
        assert window.meeting_diarization.winfo_manager() == ""
        assert _pady(window.meeting_watch) == (0, 12)
    finally:
        root.destroy()


def test_speaker_labels_collecting_sits_under_the_watch_line(tmp_path):
    _client, root, window = _running(
        tmp_path,
        {"diarization": "collecting", "diarization_error": "ignored"},
    )
    try:
        assert window.meeting_watch.cget("text") == LEGACY_WATCH
        assert window.meeting_diarization.cget("text") == "Speaker labels: collecting"
        slaves = window.meeting_content.pack_slaves()
        index = slaves.index(window.meeting_watch)
        assert slaves[index + 1] is window.meeting_diarization
        assert _pady(window.meeting_watch) == (0, 2)
        assert _pady(window.meeting_diarization) == (0, 12)
        live = window.meeting_live.cget("text")
        assert "[00:04] Me: I can take this" in live
        assert "[01:12] Others: Thanks" in live
    finally:
        root.destroy()


def test_speaker_labels_unavailable_reason_is_escaped_and_wraps(tmp_path):
    reason = "Speaker models failed\nto load: x" + RLO + "y " + "word " * 40 + "."
    _client, root, window = _running(
        tmp_path,
        {"diarization": "unavailable", "diarization_error": reason},
    )
    try:
        window._meeting_wrap(SimpleNamespace(width=400))
        text = window.meeting_diarization.cget("text")
        assert text.startswith("Speaker labels: unavailable (")
        assert text.endswith(")")
        assert "\n" not in text and RLO not in text
        assert int(window.meeting_diarization.cget("wraplength")) == 400
        root.update_idletasks()
        long_h = window.meeting_diarization.winfo_reqheight()

        window._meeting_status_data["session"]["diarization"] = "collecting"
        window._meeting_render()
        root.update_idletasks()
        assert window.meeting_diarization.winfo_reqheight() < long_h
    finally:
        root.destroy()


def test_meeting_wrap_clamps_the_speaker_label_width(tmp_path):
    _client, root, window = _running(tmp_path, {"diarization": "collecting"})
    try:
        window._meeting_wrap(SimpleNamespace(width=1))  # not laid out yet
        assert int(window.meeting_diarization.cget("wraplength")) == 720
        window._meeting_wrap(SimpleNamespace(width=50))
        assert int(window.meeting_diarization.cget("wraplength")) == 200
        window._meeting_wrap(SimpleNamespace(width=2000))
        assert int(window.meeting_diarization.cget("wraplength")) == 720
    finally:
        root.destroy()


def test_speaker_labels_unpack_when_the_meeting_is_saved(tmp_path):
    client, root, window = _running(tmp_path, {"diarization": "collecting"})
    try:
        assert window.meeting_diarization.winfo_manager() == "pack"
        client.state = "saved"
        window._meeting_poll()
        _pump(root, lambda: window._meeting_status_data.get("last_saved"))
        assert window.meeting_diarization.winfo_manager() == ""
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


# ----------------------------------------------------------------- speakers ---

# meeting_content's children as _page_meetings builds them (includes U5's
# unpacked diarization status label; unchanged by U3).
MEETING_CONTENT_CLASSES = [
    "Label", "Label", "Label", "Frame", "Frame", "Text", "Button", "Label", "Frame", "Frame",
]


def _close(root, window, client=None):
    if client is not None and hasattr(client, "release_all"):
        client.release_all()  # unblock gated worker threads
    _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
    root.destroy()
    gc.collect()  # finalize Tk variables on this thread


def _speakers_window(tmp_path, client):
    root, window = _window(tmp_path, client)
    _pump(root, lambda: window._meeting_speakers_view is not None, timeout=5)
    _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
    return root, window, window._meeting_speakers_view


def _labels(view):
    return [label.cget("text") for label in view.name_labels]


@pytest.mark.parametrize("state", ["idle", "running", "saved"])
def test_no_speakers_adds_no_widget(tmp_path, state):
    client = FakeMeetingClient(state)
    root, window = _window(tmp_path, client)
    try:
        _pump(root, lambda: bool(window._meeting_status_data), timeout=5)
        _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
        assert window._meeting_speakers_view is None
        children = window.meeting_content.winfo_children()
        assert [child.winfo_class() for child in children] == MEETING_CONTENT_CLASSES
        packed = window.meeting_content.pack_slaves()
        assert all(child in children for child in packed)
        assert not window._meeting_speakers.visible
    finally:
        _close(root, window, client)


def test_saved_speakers_render_under_the_saved_row(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    client.speakers.append(
        {"id": "S3\u202e", "label": "\u202eEvil → 😀", "seconds": 3, "source": "others"}
    )
    root, window, view = _speakers_window(tmp_path, client)
    try:
        packed = window.meeting_content.pack_slaves()
        assert packed.index(view.frame) == packed.index(window.meeting_saved_frame) + 1
        assert view.heading.cget("text") == "Speakers"
        labels = _labels(view)
        assert labels[:2] == ["Speaker 1", "Speaker 2"]
        assert "\u202e" not in labels[2] and "\\u202e" in labels[2]
        assert [label.cget("text") for label in view.duration_labels] == ["1m 12s", "45s", "3s"]
        assert view.error.winfo_manager() == ""
    finally:
        _close(root, window, client)


def test_rename_speaker_updates_the_row(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("rename", "S1")].invoke()
        assert view.entry is not None and view.entry.get() == "Speaker 1"
        view.entry.delete(0, "end")
        view.entry.insert(0, "Sarah")
        view.buttons[("save", "S1")].invoke()
        _pump(root, lambda: window._meeting_speakers.busy == "", timeout=5)
        assert ("rename_speaker", SPEAKER_MID, "S1", "Sarah") in client.calls
        assert _labels(view)[0] == "Sarah"
        assert view.entry is None and window._meeting_speakers.mode == ""
        assert view.error.winfo_manager() == ""
    finally:
        _close(root, window, client)


def test_rename_in_flight_disables_and_ignores_stale_polls(tmp_path):
    client = GatedSpeakerClient()
    root, window, view = _speakers_window(tmp_path, client)
    try:
        panel = window._meeting_speakers
        stale = panel.ticket()
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Sarah")
        view._save()
        assert panel.busy == "rename"
        assert view.entry.cget("state") == "disabled"
        assert all(button.cget("state") == "disabled" for button in view.buttons.values())
        assert view.buttons[("save", "S1")].cget("text") == "Saving…"
        view._save()  # second save: nothing sent
        window._meeting_poll()  # answers with the old label while busy
        _pump(root, lambda: window._meeting_requests_in_flight == 1, timeout=5)
        assert panel.row("S1")["label"] == "Speaker 1"
        assert view.entry.get() == "Sarah"  # the poll left the editor alone
        old = client.status()
        client.release_all()
        _pump(root, lambda: panel.busy == "", timeout=5)
        assert [c for c in client.calls if c[0] == "rename_speaker"] == [
            ("rename_speaker", SPEAKER_MID, "S1", "Sarah"),
        ]
        assert _labels(view)[0] == "Sarah"
        # A poll sent before the rename finished answers late: ignored.
        window._meeting_status_received(old, None, stale)
        assert _labels(view)[0] == "Sarah"
        window._meeting_poll()
        _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
        assert _labels(view)[0] == "Sarah"
    finally:
        _close(root, window, client)


def test_rename_error_keeps_the_editor_then_success_clears_it(tmp_path):
    from localflow.hub_model import speaker_error_text

    client = FakeMeetingClient("saved-speakers")
    client.speaker_error = MeetingClientError("unknown speaker")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        panel = window._meeting_speakers
        view.buttons[("rename", "S2")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Bo")
        view._save()
        _pump(root, lambda: bool(panel.error), timeout=5)
        assert view.error.cget("text") == speaker_error_text(
            "rename", MeetingClientError("unknown speaker"),
        )
        assert view.error.winfo_manager() == "pack"
        assert panel.mode == "rename" and view.entry.get() == "Bo"
        client.speaker_error = None
        view._save()
        _pump(root, lambda: panel.mode == "", timeout=5)
        assert view.error.winfo_manager() == "" and panel.error == ""
        assert _labels(view)[1] == "Bo"
    finally:
        _close(root, window, client)


def test_invalid_name_shows_an_inline_error_without_a_request(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "!!!")
        view._save()
        assert view.error.cget("text") == "Use letters or numbers in the name."
        assert view.entry.get() == "!!!"
        assert not any(call[0] == "rename_speaker" for call in client.calls)
    finally:
        _close(root, window, client)


def test_typing_survives_polls(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("rename", "S1")].invoke()
        entry = view.entry
        entry.delete(0, "end")
        entry.insert(0, "Half typ")
        for _ in range(2):
            window._meeting_poll()
            _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
        assert view.entry is entry and entry.winfo_exists()
        assert entry.get() == "Half typ"
    finally:
        _close(root, window, client)


def test_merge_hidden_with_one_speaker(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    client.speakers = client.speakers[:1]
    root, window, view = _speakers_window(tmp_path, client)
    try:
        assert ("rename", "S1") in view.buttons
        assert ("merge", "S1") not in view.buttons
    finally:
        _close(root, window, client)


def test_merge_with_two_speakers_preselects_the_target(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("merge", "S2")].invoke()
        assert view.buttons[("confirm", "S2")].cget("state") == "normal"
        assert view.menubutton.cget("text") == "Speaker 1 · 1m 12s"
        view.buttons[("confirm", "S2")].invoke()
        _pump(root, lambda: len(window._meeting_speakers.rows) == 1, timeout=5)
        assert ("merge_speakers", SPEAKER_MID, "S2", "S1") in client.calls
        assert ("merge", "S1") not in view.buttons  # one speaker left
    finally:
        _close(root, window, client)


def test_merge_with_duplicate_labels_needs_an_explicit_target(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    client.speakers = [
        {"id": "S1", "label": "Sam", "seconds": 72.4},
        {"id": "S2", "label": "Sam", "seconds": 45},
        {"id": "S3", "label": "Ana", "seconds": 10},
    ]
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("merge", "S3")].invoke()
        confirm = view.buttons[("confirm", "S3")]
        assert confirm.cget("state") == "disabled"
        assert view.menubutton.cget("text") == "Choose a speaker…"
        titles = [view.menu.entrycget(i, "label") for i in range(view.menu.index("end") + 1)]
        assert titles == ["Sam · 1m 12s · S1", "Sam · 45s · S2"]
        view._confirm()
        assert not any(call[0] == "merge_speakers" for call in client.calls)
        view.menu.invoke(1)
        confirm = view.buttons[("confirm", "S3")]
        assert confirm.cget("state") == "normal"
        assert view.menubutton.cget("text") == "Sam · 45s · S2"
        confirm.invoke()
        _pump(root, lambda: len(window._meeting_speakers.rows) == 2, timeout=5)
        assert ("merge_speakers", SPEAKER_MID, "S3", "S2") in client.calls
        assert window._meeting_speakers.row("S3") is None
    finally:
        _close(root, window, client)


def test_new_meeting_clears_the_panel_and_drops_the_late_response(tmp_path):
    client = GatedSpeakerClient()
    root, window, view = _speakers_window(tmp_path, client)
    try:
        panel = window._meeting_speakers
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Sarah")
        view._save()
        window._meeting_toggle()  # Start: a new meeting
        _pump(root, lambda: bool(window._meeting_status_data.get("session")), timeout=5)
        assert window._meeting_speakers_view is None
        assert not panel.visible and panel.busy == ""
        client.release_all()
        _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
        assert not panel.visible and window._meeting_speakers_view is None
        assert panel.error == ""
    finally:
        _close(root, window, client)


def test_navigation_mid_edit_keeps_the_draft(tmp_path):
    client = FakeMeetingClient("saved-speakers")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Draft name")
        window.show_page("Home")
        assert window._meeting_speakers_view is None
        assert window._meeting_speakers.draft == "Draft name"
        window.show_page("Meetings")
        _pump(root, lambda: window._meeting_speakers_view is not None, timeout=5)
        assert window._meeting_speakers_view.entry.get() == "Draft name"
    finally:
        _close(root, window, client)


def test_navigation_during_a_failing_request_restores_the_edit(tmp_path):
    client = GatedSpeakerClient()
    client.speaker_error = MeetingClientError("transcript file not found")
    root, window, view = _speakers_window(tmp_path, client)
    try:
        panel = window._meeting_speakers
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Sarah")
        view._save()
        window.show_page("Home")
        client.release_all()
        _pump(root, lambda: panel.busy == "", timeout=5)
        _pump(root, lambda: window._meeting_requests_in_flight == 0, timeout=5)
        assert "moved or deleted" in panel.error
        window.show_page("Meetings")
        _pump(root, lambda: window._meeting_speakers_view is not None, timeout=5)
        view = window._meeting_speakers_view
        assert view.entry.get() == "Sarah"
        assert view.error.winfo_manager() == "pack"
        assert "moved or deleted" in view.error.cget("text")
    finally:
        _close(root, window, client)


def test_navigation_during_a_successful_request(tmp_path):
    client = GatedSpeakerClient()
    root, window, view = _speakers_window(tmp_path, client)
    try:
        view.buttons[("rename", "S1")].invoke()
        view.entry.delete(0, "end")
        view.entry.insert(0, "Sarah")
        view._save()
        window.show_page("Home")
        client.release_all()
        _pump(root, lambda: window._meeting_speakers.busy == "", timeout=5)
        window.show_page("Meetings")
        _pump(root, lambda: window._meeting_speakers_view is not None, timeout=5)
        view = window._meeting_speakers_view
        assert view.entry is None and _labels(view)[0] == "Sarah"
    finally:
        _close(root, window, client)


def test_speaker_panel_view_adds_no_tk_variables():
    import inspect

    from localflow.hub_tk import SpeakerPanelView

    source = inspect.getsource(SpeakerPanelView)
    for name in ("StringVar", "IntVar", "BooleanVar", "DoubleVar", "Variable("):
        assert name not in source
