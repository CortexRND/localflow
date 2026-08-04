"""Moves mic start/stop work off the pynput event-tap thread.

HoldKeyListener's on_press/on_release callbacks (localflow/hotkey.py) run
*inside* the pynput CGEventTap callback on macOS. Anything blocking there is
dangerous: opening a CoreAudio input stream (sd.query_devices() +
sd.InputStream(...).start()) takes 60-100ms. Doing that on the tap thread
delays keyboard event delivery system-wide, and if it happens often enough
(e.g. a user spamming the hotkey) macOS disables the tap
(kCGEventTapDisabledByTimeout) — the hotkey goes silently dead.

PushToTalkController decouples the two. note_start()/note_stop() are
enqueue-only (a queue.Queue.put(), microseconds, no I/O) and are safe to call
directly from the tap thread. A single daemon consumer thread drains the
queue serially and performs the real recorder.start()/stop() pipeline, so
mic I/O never runs on the tap thread.

It also coalesces spam: if the consumer is still busy with a previous press
when a new press is released, both events are already sitting in the queue
by the time the consumer gets to the new "start" — in that case, if the
pair is shorter than MIN_CLIP_SECONDS, it's dropped without ever opening the
mic for it.
"""

import logging
import queue
import threading
import time
from collections.abc import Callable

import numpy as np

from localflow import sounds as _sounds
from localflow.audio import Recorder

log = logging.getLogger("localflow.pushtotalk")

# Below this, a press is either genuine noise (post-hoc: the captured clip
# is too short to be worth transcribing) or spam (pre-hoc: released again
# before the consumer even got around to opening the mic for it).
MIN_CLIP_SECONDS = 0.3

# Sentinel enqueued by shutdown() to stop the consumer thread. Not a
# ("start"|"stop", ts) tuple, so it's checked with `is` before unpacking.
_SHUTDOWN = object()


class PushToTalkController:
    """Owns the start/stop event queue and the consumer thread that turns
    those events into real mic I/O, off the caller's thread.

    Constructor takes plain collaborators (recorder, config, work_queue,
    play_sound) rather than importing them, so it's unit-testable with fakes
    and never needs a real mic or sounddevice.
    """

    def __init__(
        self,
        recorder: Recorder,
        config,
        work_queue: "queue.Queue[np.ndarray]",
        play_sound: Callable[[str], None] = _sounds.play,
        min_clip_seconds: float = MIN_CLIP_SECONDS,
    ):
        self._recorder = recorder
        self._config = config
        self._work_queue = work_queue
        self._play_sound = play_sound
        self._min_clip_seconds = min_clip_seconds
        self._events: "queue.Queue" = queue.Queue()
        # Tracks the logical state note_toggle() last drove toward. Guarded
        # by _toggle_lock because it's read-modify-written from the caller
        # thread; recorder.recording is NOT a substitute (see note_toggle).
        self._toggle_intent = False
        self._toggle_lock = threading.Lock()
        self._thread = threading.Thread(
            target=self._run, name="pushtotalk-consumer", daemon=True
        )
        self._thread.start()

    # -- called from the tap callback thread -----------------------------
    # Must stay microseconds: no I/O, no locks held beyond queue.Queue's own
    # brief internal one.

    def note_start(self) -> None:
        self._events.put(("start", time.monotonic()))

    def note_stop(self) -> None:
        self._events.put(("stop", time.monotonic()))

    def note_toggle(self) -> None:
        """Toggle-mode equivalent of note_start()/note_stop().

        Resolves start-vs-stop from _toggle_intent, tracked synchronously
        here under a lock — NOT from recorder.recording. recorder.recording
        only flips True inside the consumer thread's recorder.start() call,
        which runs async and takes ~60-100ms; reading it on the caller
        thread would race a rapid double-press landing inside that window
        (both presses would read False and both enqueue "start", and the
        consumer's already-recording guard would silently drop the second
        one — the user's intended stop is lost and the mic keeps
        recording). _toggle_intent flips synchronously on every call, so
        the second press always resolves to the opposite of the first
        regardless of how far the consumer has gotten.
        """
        with self._toggle_lock:
            self._toggle_intent = not self._toggle_intent
            if self._toggle_intent:
                self.note_start()
            else:
                self.note_stop()

    # -- lifecycle ---------------------------------------------------------

    def shutdown(self, timeout: float | None = 1.0) -> None:
        """Stop the consumer thread. For tests and clean process exit."""
        self._events.put(_SHUTDOWN)
        self._thread.join(timeout=timeout)

    # -- consumer thread: the only thread that touches the recorder --------

    def _run(self) -> None:
        pending = None  # an already-dequeued event carried into the next loop
        while True:
            event = pending if pending is not None else self._events.get()
            pending = None
            if event is _SHUTDOWN:
                # _SHUTDOWN can arrive either straight off self._events.get()
                # or deferred via `pending` (e.g. the spam-coalescing
                # lookahead pulled it right after a "start" was already
                # processed and the mic opened). Either way, if a recording
                # is live at this point, close it before exiting — otherwise
                # the stream is left open with nothing left to stop it.
                if self._recorder.recording:
                    log.warning("shutdown received mid-recording — stopping mic first")
                    self._do_stop()
                return
            kind, ts = event

            if kind == "stop":
                if not self._recorder.recording:
                    log.debug("stray stop event (not recording), dropped")
                    continue
                self._do_stop()
                continue

            # kind == "start"
            if self._recorder.recording:
                log.debug("stray start event (already recording), dropped")
                continue

            # Spam coalescing: peek at whatever is already queued. If it's
            # the matching stop and the pair is shorter than
            # min_clip_seconds, drop both without touching the mic.
            try:
                lookahead = self._events.get_nowait()
            except queue.Empty:
                lookahead = None

            if lookahead is not None:
                if lookahead is not _SHUTDOWN:
                    lkind, lts = lookahead
                    if lkind == "stop" and (lts - ts) < self._min_clip_seconds:
                        log.info("spam press coalesced (%.2fs), dropped", lts - ts)
                        continue
                pending = lookahead  # not a coalescing pair; handle next loop

            self._do_start()

    def _do_start(self) -> None:
        try:
            self._recorder.start()
        except Exception as exc:
            log.exception("mic error")
            print(f"mic error: {exc}")
            return
        if self._config.sounds_enabled:
            self._play_sound("start")
        print("● recording")

    def _do_stop(self) -> None:
        audio = self._recorder.stop()
        if self._config.sounds_enabled:
            self._play_sound("stop")
        print("■ transcribing…")
        duration = len(audio) / self._config.sample_rate if self._config.sample_rate else 0
        if duration < self._min_clip_seconds:
            log.info("clip too short (%.2fs), dropped", duration)
            return
        self._work_queue.put(audio)
