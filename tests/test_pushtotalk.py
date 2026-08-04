import queue
import threading
import time

import numpy as np
import pytest

from localflow.pushtotalk import MIN_CLIP_SECONDS, PushToTalkController


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


def make_controller(recorder=None, config=None, work_queue=None, min_clip_seconds=MIN_CLIP_SECONDS):
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


def test_normal_hold_above_threshold_records_and_queues_clip():
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
        assert isinstance(clip, np.ndarray)
        assert len(clip) == int(1.0 * config.sample_rate)
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
