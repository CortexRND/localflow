import queue
import threading
import time

import numpy as np
import pytest

from localflow.pushtotalk import MIN_CLIP_SECONDS, PushToTalkController
from localflow.context import AppInfo


def _wait_until(predicate, timeout=2.0, interval=0.005):
    """Poll predicate() until it's truthy or timeout elapses. Returns the
    final truthiness so callers can assert on it with a clear failure."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


class FakeRecorder:
    """Stands in for localflow.audio.Recorder. Never touches sounddevice."""

    def __init__(self, clip_seconds: float = 1.0, sample_rate: int = 16000):
        self.recording = False
        self.start_calls = 0
        self.stop_calls = 0
        self._clip_seconds = clip_seconds
        self._sample_rate = sample_rate
        self.start_gate: threading.Event | None = None  # optional block point
        # Set the instant start() is entered, before it blocks on
        # start_gate — lets tests wait for "consumer is inside recorder.start()"
        # deterministically instead of guessing with a sleep.
        self.start_entered = threading.Event()

    def start(self) -> None:
        self.start_entered.set()
        if self.start_gate is not None:
            self.start_gate.wait(timeout=2.0)
        self.start_calls += 1
        self.recording = True

    def stop(self) -> np.ndarray:
        self.stop_calls += 1
        self.recording = False
        n = int(self._clip_seconds * self._sample_rate)
        return np.zeros(n, dtype=np.float32)


class FakeConfig:
    sounds_enabled = False
    sample_rate = 16000
    max_recording_seconds = 1200
    paste_method = "auto"


def make_controller(
    recorder=None,
    config=None,
    work_queue=None,
    min_clip_seconds=MIN_CLIP_SECONDS,
    clock=time.monotonic,
    on_state=None,
):
    recorder = recorder if recorder is not None else FakeRecorder()
    config = config if config is not None else FakeConfig()
    work_queue = work_queue if work_queue is not None else queue.Queue()
    sounds_played = []
    controller = PushToTalkController(
        recorder,
        config,
        work_queue,
        play_sound=lambda cue: sounds_played.append(cue),
        min_clip_seconds=min_clip_seconds,
        clock=clock,
        on_state=on_state,
    )
    return controller, recorder, config, work_queue, sounds_played


# --------------------------------------------------------------------- tests


def test_rapid_start_stop_pair_is_coalesced_without_touching_mic():
    controller, recorder, config, work_queue, _ = make_controller()
    try:
        # Both events land in the queue before the consumer thread has any
        # chance to act on them (plain back-to-back calls, no sleeps) —
        # this is exactly the "matching stop already queued" case.
        controller.note_start()
        controller.note_stop()

        # Give the consumer a moment to drain, then assert it never opened
        # the mic for this pair.
        time.sleep(0.05)
        assert recorder.start_calls == 0
        assert recorder.stop_calls == 0
        assert work_queue.empty()
    finally:
        controller.shutdown()


def test_normal_hold_above_threshold_records_and_queues_clip(monkeypatch):
    from localflow import pushtotalk

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    recorder = FakeRecorder(clip_seconds=1.0)
    controller, recorder, config, work_queue, sounds_played = make_controller(recorder=recorder)
    try:
        controller.note_start()
        assert _wait_until(lambda: recorder.start_calls == 1), "recorder.start() was never called"

        # Real gap well above MIN_CLIP_SECONDS before the stop arrives.
        time.sleep(MIN_CLIP_SECONDS + 0.05)
        controller.note_stop()

        assert _wait_until(lambda: recorder.stop_calls == 1), "recorder.stop() was never called"
        assert _wait_until(lambda: not work_queue.empty()), "clip never reached work_queue"

        assert recorder.start_calls == 1
        assert recorder.stop_calls == 1
        clip = work_queue.get_nowait()
        assert clip.kind == "dictate"
        audio, app_info = clip.audio, clip.app_info
        assert isinstance(audio, np.ndarray)
        assert len(audio) == int(1.0 * config.sample_rate)
        assert app_info is None
        assert sounds_played == []  # sounds_enabled is False in FakeConfig
    finally:
        controller.shutdown()


def test_spam_burst_touches_mic_zero_times():
    controller, recorder, config, work_queue, _ = make_controller()
    try:
        # 10 alternating events (5 press/release pairs), each pair well
        # under MIN_CLIP_SECONDS apart, fired back-to-back with no real
        # delay — mirrors a user mashing the hotkey faster than the
        # consumer can (or needs to) react.
        for _ in range(5):
            controller.note_start()
            controller.note_stop()

        time.sleep(0.1)
        assert recorder.start_calls == 0
        assert recorder.stop_calls == 0
        assert work_queue.empty()
    finally:
        controller.shutdown()


def test_stop_without_start_is_a_noop():
    controller, recorder, config, work_queue, _ = make_controller()
    try:
        controller.note_stop()
        time.sleep(0.05)
        assert recorder.start_calls == 0
        assert recorder.stop_calls == 0
        assert work_queue.empty()

        # Controller must still be alive and functional afterwards.
        controller.note_start()
        assert _wait_until(lambda: recorder.start_calls == 1)
    finally:
        controller.shutdown()


def test_note_start_returns_immediately_even_while_consumer_is_busy():
    recorder = FakeRecorder()
    recorder.start_gate = threading.Event()  # recorder.start() blocks until set
    controller, recorder, config, work_queue, _ = make_controller(recorder=recorder)
    try:
        t0 = time.monotonic()
        controller.note_start()
        elapsed = time.monotonic() - t0
        assert elapsed < 0.05, f"note_start() blocked the caller for {elapsed:.3f}s"

        # Consumer is now stuck inside recorder.start(); a second note_start
        # call must also return immediately (it only enqueues).
        t1 = time.monotonic()
        controller.note_start()
        elapsed2 = time.monotonic() - t1
        assert elapsed2 < 0.05

        assert recorder.start_calls == 0  # still blocked

        recorder.start_gate.set()
        assert _wait_until(lambda: recorder.start_calls == 1)
    finally:
        recorder.start_gate.set()
        controller.shutdown()


# ------------------------------------------------------- reviewer fix-ups ---
# Two defects a reviewer found in the merged pushtotalk.py: a caller-thread
# race in note_toggle(), and a mic left open if _SHUTDOWN is deferred while
# a "start" is in flight. Both fixed in pushtotalk.py; regression-tested here.


def test_note_toggle_resolves_correctly_under_rapid_double_press():
    """note_toggle() must NOT decide start-vs-stop from recorder.recording
    read on the caller thread: recording only flips True inside the
    consumer's recorder.start() call, which here is gated open for ~60-100ms
    of real work. A second toggle landing inside that window must still
    resolve to "stop" (not get silently dropped as a stray duplicate
    start), so the recording actually ends."""
    recorder = FakeRecorder()
    recorder.start_gate = threading.Event()  # block inside recorder.start()
    controller, recorder, config, work_queue, _ = make_controller(recorder=recorder)
    try:
        controller.note_toggle()  # intent: start
        assert _wait_until(lambda: recorder.start_entered.is_set()), (
            "consumer never reached recorder.start()"
        )
        # Still inside the blocking call — recorder.recording is the stale
        # False that a caller-thread read of it would race on.
        assert recorder.start_calls == 0
        assert recorder.recording is False

        controller.note_toggle()  # lands mid-block; must resolve to "stop"

        recorder.start_gate.set()  # release the block

        assert _wait_until(lambda: recorder.stop_calls == 1), (
            "recorder.stop() was never called — the second toggle was dropped"
        )
        assert recorder.start_calls == 1
        assert recorder.stop_calls == 1
        assert recorder.recording is False
    finally:
        recorder.start_gate.set()
        controller.shutdown()


def test_shutdown_mid_recording_stops_the_mic():
    """General case: shutdown() arrives while a recording is genuinely
    active. The mic stream must be closed before the consumer thread exits,
    not left open."""
    recorder = FakeRecorder()
    controller, recorder, config, work_queue, _ = make_controller(recorder=recorder)
    controller.note_start()
    assert _wait_until(lambda: recorder.start_calls == 1)
    assert recorder.recording is True

    controller.shutdown()

    assert recorder.stop_calls == 1
    assert recorder.recording is False


def test_shutdown_pulled_by_start_lookahead_still_stops_mic():
    """Precise regression for the reported race: shutdown()'s _SHUTDOWN
    sentinel lands in the queue immediately behind a "start" event, so the
    consumer's spam-coalescing lookahead (a get_nowait() run right after
    dequeuing "start") is the one that pulls it — not the top-level
    blocking get(). That deferred sentinel must not skip stopping the mic
    that _do_start() just opened.

    note_start() then shutdown() are called back-to-back with no sleep so
    both land in the queue before the daemon thread is scheduled (same
    reasoning as the spam-coalescing tests above), which reliably drives
    the lookahead-deferred path rather than the direct one.
    """
    recorder = FakeRecorder()
    controller, recorder, config, work_queue, _ = make_controller(recorder=recorder)
    controller.note_start()
    controller.shutdown()

    assert recorder.start_calls == 1
    assert recorder.stop_calls == 1
    assert recorder.recording is False


def test_frontmost_app_is_captured_on_consumer_thread(monkeypatch):
    from localflow import pushtotalk

    caller_thread = threading.get_ident()
    captured_threads = []
    expected = AppInfo("Editor", "com.example.editor")
    monkeypatch.setattr(
        pushtotalk,
        "frontmost_app",
        lambda: captured_threads.append(threading.get_ident()) or expected,
    )
    controller, recorder, _config, work_queue, _ = make_controller()
    try:
        controller.note_start()
        assert _wait_until(lambda: recorder.start_calls == 1)
        controller.note_stop()
        assert _wait_until(lambda: not work_queue.empty())
        clip = work_queue.get_nowait()
        assert clip.kind == "dictate"
        audio, app_info = clip.audio, clip.app_info
        assert isinstance(audio, np.ndarray)
        assert app_info == expected
        assert len(captured_threads) == 1
        assert captured_threads[0] != caller_thread
    finally:
        controller.shutdown()


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds


def test_queued_short_tap_pair_never_opens_microphone():
    clock = FakeClock()
    controller, recorder, _config, _work_queue, _sounds = make_controller(clock=clock)
    try:
        controller._events.put(("up", 0.1))
        controller._handle_down(0.0)

        assert controller._gesture_state == "TAPPED"
        assert recorder.start_calls == 0
    finally:
        controller.shutdown()


def test_short_tap_locks_on_second_down_and_triple_tap_cancels(monkeypatch):
    from localflow import pushtotalk

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    config = FakeConfig()
    config.sounds_enabled = True
    controller, recorder, _config, work_queue, sounds_played = make_controller(config=config)
    try:
        controller._handle_down(0.0)
        controller._handle_up(0.1)
        assert controller._gesture_state == "TAPPED"

        controller._handle_down(0.3)
        controller._handle_up(0.31)
        assert controller._gesture_state == "LOCKED"
        assert recorder.recording is True
        assert sounds_played == ["start", "lock"]

        controller._handle_down(0.6)
        assert recorder.recording is False
        assert controller._gesture_state == "AWAIT_UP"
        assert work_queue.empty()
        assert sounds_played[-1] == "cancel"
        controller._handle_up(0.61)
        assert controller._gesture_state == "IDLE"
    finally:
        controller.shutdown()


@pytest.mark.parametrize("state", ["HELD", "TAPPED", "LOCKED"])
def test_escape_cancels_active_gesture_states(monkeypatch, state):
    from localflow import pushtotalk

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    controller, recorder, _config, work_queue, _sounds = make_controller()
    try:
        controller._handle_down(0.0)
        if state in {"TAPPED", "LOCKED"}:
            controller._handle_up(0.1)
        if state == "LOCKED":
            controller._handle_down(0.2)
        assert controller._gesture_state == state

        controller._handle_cancel()

        assert recorder.recording is False
        assert controller._gesture_state == (
            "AWAIT_UP" if state in {"HELD", "LOCKED"} else "IDLE"
        )
        assert work_queue.empty()
    finally:
        controller.shutdown()


def test_escape_is_ignored_while_idle():
    controller, recorder, _config, work_queue, sounds_played = make_controller()
    try:
        controller._handle_cancel()

        assert recorder.start_calls == recorder.stop_calls == 0
        assert work_queue.empty()
        assert sounds_played == []
    finally:
        controller.shutdown()


def test_max_duration_warns_once_then_stops_and_queues(monkeypatch, caplog):
    from localflow import pushtotalk

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    clock = FakeClock()
    config = FakeConfig()
    config.sounds_enabled = True
    controller, recorder, _config, work_queue, sounds_played = make_controller(
        config=config, clock=clock
    )
    try:
        assert controller._do_start() is True
        clock.advance(1140)
        controller._check_timers(clock())
        controller._check_timers(clock())
        assert sounds_played.count("warn") == 1

        clock.advance(60)
        with caplog.at_level("INFO", logger="localflow.pushtotalk"):
            controller._check_timers(clock())

        assert recorder.recording is False
        assert not work_queue.empty()
        assert "max duration reached" in caplog.text
    finally:
        controller.shutdown()


def test_command_mode_creates_command_job_with_selection(monkeypatch):
    from localflow import pushtotalk
    from localflow.inject import SelectionCapture

    monkeypatch.setattr(
        pushtotalk, "capture_selection",
        lambda: SelectionCapture("selected text", "previous clipboard"),
    )
    app_info = AppInfo("Editor", "example.editor")
    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: app_info)
    controller, recorder, _config, work_queue, _sounds = make_controller()
    try:
        controller._start_command()
        assert controller._gesture_state == "COMMAND"
        assert recorder.recording is True
        controller._stop_command()

        job = work_queue.get_nowait()
        assert job.kind == "command"
        assert job.selection == "selected text"
        assert job.clipboard_before == "previous clipboard"
        assert job.app_info == app_info
    finally:
        controller.shutdown()


def test_transform_selection_is_enqueued_as_a_job(monkeypatch):
    from localflow import pushtotalk
    from localflow.inject import SelectionCapture

    monkeypatch.setattr(pushtotalk, "frontmost_app", lambda: None)
    monkeypatch.setattr(
        pushtotalk, "capture_selection",
        lambda: SelectionCapture("selected", "clipboard"),
    )
    controller, _recorder, _config, work_queue, _sounds = make_controller()
    try:
        controller._handle_transform("polish")
        job = work_queue.get_nowait()
        assert job.kind == "transform"
        assert job.preset == "polish"
        assert job.selection == "selected"
        assert job.clipboard_before == "clipboard"
    finally:
        controller.shutdown()


def test_job_result_pastes_on_consumer_thread(monkeypatch):
    from localflow import pushtotalk

    calls = []
    monkeypatch.setattr(
        pushtotalk, "paste_text",
        lambda *args: calls.append((args, threading.current_thread().name)),
    )
    controller, _recorder, _config, _queue, _sounds = make_controller()
    try:
        controller.note_job_result("result", "before")
        assert _wait_until(lambda: bool(calls))
        assert calls[0] == (("result", "auto", "before"), "pushtotalk-consumer")
        assert controller._last_text == "result"
    finally:
        controller.shutdown()


def test_status_transitions_cover_recording_processing_and_idle():
    states = []
    controller, _recorder, _config, _queue, _sounds = make_controller(
        on_state=states.append
    )
    try:
        controller._do_start()
        controller._do_stop()
        controller.note_job_result("result")

        assert _wait_until(lambda: states and states[-1] == "idle")
        assert states == ["recording", "processing", "idle"]
    finally:
        controller.shutdown()


def test_media_paused_after_start_and_resumed_after_stop(monkeypatch):
    from types import SimpleNamespace

    from localflow import media

    events = []
    monkeypatch.setattr(media, "pause_playing", lambda: events.append("pause") or {"Music"})
    monkeypatch.setattr(
        media, "resume", lambda paused: events.append(("resume", paused))
    )
    config = SimpleNamespace(
        sounds_enabled=False,
        sample_rate=16000,
        max_recording_seconds=1200,
        pause_media_while_dictating=True,
        paste_method="auto",
    )
    controller, _recorder, _config, work_queue, _sounds = make_controller(
        config=config
    )
    try:
        controller._do_start()
        controller._do_stop()

        assert events == ["pause", ("resume", {"Music"})]
        assert work_queue.get_nowait().kind == "dictate"
    finally:
        controller.shutdown()


def test_media_paused_after_start_and_resumed_on_cancel(monkeypatch):
    from types import SimpleNamespace

    from localflow import media

    resumed = []
    monkeypatch.setattr(media, "pause_playing", lambda: {"Spotify"})
    monkeypatch.setattr(media, "resume", lambda paused: resumed.append(paused))
    config = SimpleNamespace(
        sounds_enabled=False,
        sample_rate=16000,
        max_recording_seconds=1200,
        pause_media_while_dictating=True,
        paste_method="auto",
    )
    controller, _recorder, _config, _queue, _sounds = make_controller(config=config)
    try:
        controller._do_start()
        controller._discard_recording()

        assert resumed == [{"Spotify"}]
    finally:
        controller.shutdown()
