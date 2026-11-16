"""PR29 review finding 5: closing the Hub must stop the controller's timers.

`HubWindowController.initWithConfig...` schedules a repeating NSTimer
(`self.meeting_timer`) that targets the controller. The run loop retains the
timer and the timer retains its target, so after `windowWillClose_` the
closed controller stays alive and keeps ticking every 0.2 s (draining
meeting results, Hot Phrases autosave, Meetings polling) until the app quits.

Stopping the timer must not lose notes: anything the next ticks would have
saved (a debounced edit, a save queued behind one in flight, a Stop waiting
on its notes flush) is sent on close, after the save in flight, so an older
save can never overwrite the final text.

The real methods (and `_MeetingNotesWriter`) are AST-extracted from
hub_mac.py and run against fakes, so no AppKit is imported. Worker threads
are real; every wait is gated by an Event the test controls.
"""

import ast
import logging
import queue
import threading
import time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

HUB = Path(__file__).resolve().parents[1] / "localflow" / "hub_mac.py"
SOURCE = HUB.read_text(encoding="utf-8")
WAIT = 5  # seconds; only reached if the code under test deadlocks
CLOSE_FLUSH = "localflow-meeting-close-flush"


def controller_class():
    tree = ast.parse(SOURCE)
    return next(
        n for n in tree.body
        if isinstance(n, ast.ClassDef) and n.name == "HubWindowController"
    )


def methods():
    return {n.name: n for n in controller_class().body if isinstance(n, ast.FunctionDef)}


def notes_writer():
    tree = ast.parse(SOURCE)
    cls = next(
        n for n in tree.body
        if isinstance(n, ast.ClassDef) and n.name == "_MeetingNotesWriter"
    )
    namespace = {"threading": threading}
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[cls], type_ignores=[]), str(HUB), "exec"), namespace,
    )
    return namespace["_MeetingNotesWriter"]()


def bind(controller, name, **namespace):
    method = methods()[name]
    method.decorator_list = []
    namespace.setdefault("logging", logging)
    namespace.setdefault("queue", queue)
    namespace.setdefault("time", time)
    namespace.setdefault("threading", threading)
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[method], type_ignores=[]), str(HUB), "exec"), namespace,
    )
    function = namespace[name]
    return lambda *args, **kwargs: function(controller, *args, **kwargs)


def scheduled_timer_attributes():
    """Every `self.<name> = NSTimer.scheduledTimer...(...)` in the controller."""
    names = set()
    for node in ast.walk(controller_class()):
        if not isinstance(node, ast.Assign) or not isinstance(node.value, ast.Call):
            continue
        func = node.value.func
        if not (
            isinstance(func, ast.Attribute) and func.attr.startswith("scheduledTimer")
        ):
            continue
        for target in node.targets:
            if (
                isinstance(target, ast.Attribute)
                and isinstance(target.value, ast.Name)
                and target.value.id == "self"
            ):
                names.add(target.attr)
    return names


class FakeTimer:
    def __init__(self):
        self.invalidations = 0

    def invalidate(self):
        self.invalidations += 1

    def isValid(self):
        return self.invalidations == 0


class FakeNotesView:
    def __init__(self, text=""):
        self.text = text

    def string(self):
        return self.text


class GatedClient:
    """Meeting client fake: `server_notes` is what the server last stored.

    A save of a text listed in `gates` signals `entered[text]` and then waits
    for the test to set `gates[text]`.
    """

    def __init__(self, fail_saves=False):
        self.server_notes = None
        self.calls = []
        self.gates = {}
        self.entered = {}
        self.fail_saves = fail_saves
        self._lock = threading.Lock()

    def gate(self, text):
        self.gates[text] = threading.Event()
        self.entered[text] = threading.Event()
        return self.gates[text]

    def save_notes(self, notes):
        if notes in self.gates:
            self.entered[notes].set()
            assert self.gates[notes].wait(WAIT), "test never released the save"
        with self._lock:
            self.calls.append(("save_notes", notes))
            if self.fail_saves:
                raise ConnectionError("server gone")
            self.server_notes = notes
        return {"ok": True}

    def stop(self):
        with self._lock:
            self.calls.append(("stop",))
        return {}


def hub(client=None, **extra):
    controller = SimpleNamespace(
        _closed=False,
        page="Meetings",
        phrases=Mock(),
        ask_page=Mock(),
        settings_page=Mock(),
        meeting_client=client or GatedClient(),
        meeting_notes_writer=notes_writer(),
        meeting_events=queue.Queue(),
        meeting_notes=FakeNotesView(),
        meeting_notes_status=Mock(),
        meeting_notes_text="",
        meeting_notes_due=None,
        meeting_notes_dirty=False,
        meeting_notes_save_in_flight=False,
        meeting_notes_save_pending=False,
        meeting_notes_save_operation=None,
        meeting_stop_flush_pending=False,
        meeting_active_notes_text="",
        meeting_next_poll=0,
        _phrases_tick=Mock(),
        _meeting_event=Mock(),
        _meeting_poll=Mock(),
        _meeting_render=Mock(),
        show_page=Mock(),
    )
    for name in scheduled_timer_attributes():
        setattr(controller, name, FakeTimer())
    for key, value in extra.items():
        setattr(controller, key, value)
    for name in (
        "windowWillClose_", "meetingTimerTick_", "_meeting_flush_on_close",
        "_meeting_request_notes_save", "_meeting_advance_notes_save",
        "_meeting_current_notes_text", "_meeting_worker",
    ):
        setattr(controller, name, bind(controller, name))
    return controller


def join_close_flush():
    for thread in threading.enumerate():
        if thread.name == CLOSE_FLUSH:
            thread.join(WAIT)
            assert not thread.is_alive()


def join_workers():
    for thread in threading.enumerate():
        if thread.name.startswith("localflow-meeting-"):
            thread.join(WAIT)
            assert not thread.is_alive()


@pytest.fixture(autouse=True)
def no_leftover_threads():
    yield
    join_workers()


def type_notes(controller, text):
    """What textDidChange_ does during a session: text kept, save debounced."""
    controller.meeting_notes.text = text
    controller.meeting_notes_text = text
    controller.meeting_notes_dirty = True
    controller.meeting_notes_due = time.monotonic() + 0.8


# ------------------------------------------------------------ the timer ---


def test_the_meeting_timer_is_the_controllers_scheduled_timer():
    assert "meeting_timer" in scheduled_timer_attributes()


def test_close_invalidates_every_controller_owned_timer():
    controller = hub()
    timers = {name: getattr(controller, name) for name in scheduled_timer_attributes()}

    controller.windowWillClose_(None)

    for name, timer in timers.items():
        assert timer.invalidations == 1, f"{name} still retains the closed controller"
        # The reference is dropped too (or at least points at a dead timer).
        current = getattr(controller, name, None)
        assert current is None or current is timer


def test_close_still_shuts_down_ask_settings_and_phrases_once():
    controller = hub()

    controller.windowWillClose_(None)

    assert controller._closed is True
    controller.ask_page.shutdown.assert_called_once_with()
    controller.settings_page.shutdown.assert_called_once_with()
    controller.phrases.cancel.assert_called_once_with()


def test_close_without_optional_pages_still_stops_the_timer():
    controller = hub()
    del controller.ask_page, controller.settings_page
    controller.phrases = None
    timer = controller.meeting_timer

    controller.windowWillClose_(None)

    assert timer.invalidations == 1


def test_closing_twice_shuts_down_once():
    controller = hub()
    timer = controller.meeting_timer

    controller.windowWillClose_(None)
    controller.windowWillClose_(None)

    assert timer.invalidations == 1
    assert controller._closed is True
    controller.ask_page.shutdown.assert_called_once_with()
    controller.settings_page.shutdown.assert_called_once_with()
    controller.phrases.cancel.assert_called_once_with()


def test_a_tick_already_queued_before_close_does_not_touch_the_closed_hub():
    """A worker result (e.g. meeting status) that lands after close is not
    applied: no repaint, no Hot Phrases tick, no new poll."""
    controller = hub()
    controller.windowWillClose_(None)
    controller.meeting_events.put(("status", {"session": None}, None))

    controller.meetingTimerTick_(None)

    controller._meeting_event.assert_not_called()
    controller._meeting_render.assert_not_called()
    controller._phrases_tick.assert_not_called()
    controller._meeting_poll.assert_not_called()
    controller.show_page.assert_not_called()


def test_an_open_hub_still_drains_meeting_results():
    """Guard (already green): the fix must not stop a live Hub's tick."""
    controller = hub()
    controller.meeting_next_poll = time.monotonic() + 60
    controller.meeting_events.put(("status", {"session": None}, None))

    controller.meetingTimerTick_(None)

    controller._phrases_tick.assert_called_once_with()
    controller._meeting_event.assert_called_once_with("status", {"session": None}, None)


# ------------------------------------------------- notes are not lost ---


def test_close_flushes_notes_that_were_waiting_for_the_timer():
    """Notes typed < 0.8 s before close were saved by the next tick; with the
    timer stopped, close itself sends them."""
    client = GatedClient()
    controller = hub(client)
    type_notes(controller, "typed just now")

    controller.windowWillClose_(None)
    join_close_flush()

    assert client.calls == [("save_notes", "typed just now")]
    assert controller.meeting_notes_due is None


def test_close_with_nothing_pending_saves_nothing():
    client = GatedClient()
    controller = hub(client)
    controller.meeting_notes.text = "already saved"

    controller.windowWillClose_(None)

    assert not [t for t in threading.enumerate() if t.name == CLOSE_FLUSH]
    assert client.calls == []
    assert controller.meeting_timer is None


def test_final_notes_land_after_a_save_that_was_in_flight_at_close():
    client = GatedClient()
    controller = hub(client)
    timer = controller.meeting_timer
    release_old = client.gate("old")
    type_notes(controller, "old")
    controller.meeting_notes_due = None
    controller._meeting_request_notes_save()  # what the tick does once due
    assert client.entered["old"].wait(WAIT)  # the old save is on the wire
    type_notes(controller, "latest")  # a newer edit, waiting on the debounce
    controller.meeting_notes_status.reset_mock()

    controller.windowWillClose_(None)
    assert timer.invalidations == 1
    assert client.calls == []  # the final flush waits behind the old save
    release_old.set()
    join_workers()

    assert client.calls == [("save_notes", "old"), ("save_notes", "latest")]
    assert client.server_notes == "latest"
    # The old save's completion is never applied to the closed window.
    controller.meetingTimerTick_(None)
    controller._meeting_event.assert_not_called()
    controller._meeting_render.assert_not_called()
    assert controller.meeting_notes_status.method_calls == []
    # Closing again sends nothing more and stops nothing more.
    controller.windowWillClose_(None)
    join_workers()
    assert timer.invalidations == 1
    assert client.calls == [("save_notes", "old"), ("save_notes", "latest")]


def test_an_older_save_that_reaches_the_server_late_is_dropped():
    """The in-flight worker had not sent yet when close flushed: it must not
    overwrite the final text when it finally runs."""
    client = GatedClient()
    controller = hub(client)
    started = []

    class DeferredThread(threading.Thread):
        def start(self):
            started.append(self)  # the autosave worker, held back by the test

    deferred = SimpleNamespace(Thread=DeferredThread, Lock=threading.Lock)
    controller._meeting_worker = bind(controller, "_meeting_worker", threading=deferred)
    type_notes(controller, "old")
    controller.meeting_notes_due = None
    controller._meeting_request_notes_save()
    type_notes(controller, "latest")

    controller.windowWillClose_(None)
    join_close_flush()
    assert client.server_notes == "latest"
    threading.Thread.start(started[0])  # the old worker finally runs
    started[0].join(WAIT)

    assert client.server_notes == "latest"
    assert client.calls == [("save_notes", "latest")]


def test_a_save_that_completed_before_close_is_still_followed_by_the_latest():
    """The old save finished but its completion was never ticked in: the
    queued edit is still sent."""
    client = GatedClient()
    controller = hub(client)
    type_notes(controller, "old")
    controller.meeting_notes_due = None
    controller._meeting_request_notes_save()
    join_workers()  # done; its event sits in the queue, in_flight still True
    type_notes(controller, "latest")
    controller._meeting_request_notes_save()  # queued: pending behind in-flight

    controller.windowWillClose_(None)
    join_close_flush()

    assert client.calls == [("save_notes", "old"), ("save_notes", "latest")]
    assert client.server_notes == "latest"


def test_stop_waiting_on_its_notes_flush_is_sent_after_the_final_notes():
    client = GatedClient()
    controller = hub(client)
    release = client.gate("draft")
    type_notes(controller, "draft")
    controller.meeting_notes_due = None  # what meetingAction_ does on Stop
    controller._meeting_request_notes_save(stop_flush=True)
    assert client.entered["draft"].wait(WAIT)
    type_notes(controller, "draft + last line")

    controller.windowWillClose_(None)
    release.set()
    join_workers()

    assert client.calls == [
        ("save_notes", "draft"), ("save_notes", "draft + last line"), ("stop",),
    ]
    assert client.server_notes == "draft + last line"


def test_failed_close_flush_does_not_stop_the_meeting_or_raise():
    """As in the live Hub: notes that could not be saved keep the meeting running."""
    client = GatedClient(fail_saves=True)
    controller = hub(client, meeting_stop_flush_pending=True)
    controller.meeting_notes_save_in_flight = True
    controller.meeting_notes.text = "unsaved"

    controller.windowWillClose_(None)
    join_close_flush()

    assert client.calls == [("save_notes", "unsaved")]
    assert controller.meeting_timer is None


def test_close_cleans_up_even_when_the_notes_flush_raises():
    controller = hub()
    timer = controller.meeting_timer
    controller._meeting_flush_on_close = Mock(side_effect=RuntimeError("boom"))

    controller.windowWillClose_(None)

    assert timer.invalidations == 1 and controller.meeting_timer is None
    controller.ask_page.shutdown.assert_called_once_with()
    controller.settings_page.shutdown.assert_called_once_with()
    controller.phrases.cancel.assert_called_once_with()
