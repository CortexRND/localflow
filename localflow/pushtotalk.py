"""Moves microphone and gesture work off pynput's event-tap thread."""

import logging
import queue
import threading
import time
from collections.abc import Callable

from localflow import media
from localflow import sounds as _sounds
from localflow.audio import Recorder
from localflow.context import AppInfo, frontmost_app
from localflow.inject import capture_selection, paste_text
from localflow.jobs import Job

log = logging.getLogger("localflow.pushtotalk")

MIN_CLIP_SECONDS = 0.3
TAP_MAX = MIN_CLIP_SECONDS
DOUBLE_TAP_WINDOW = 0.4
TRIPLE_CANCEL_WINDOW = 0.5

_SHUTDOWN = object()


class PushToTalkController:
    """Consume enqueue-only keyboard events and own recorder interactions."""

    def __init__(
        self,
        recorder: Recorder,
        config,
        work_queue: queue.Queue,
        play_sound: Callable[[str], None] = _sounds.play,
        min_clip_seconds: float = MIN_CLIP_SECONDS,
        clock: Callable[[], float] = time.monotonic,
        on_state: Callable[[str], None] | None = None,
    ):
        self._recorder = recorder
        self._config = config
        self._work_queue = work_queue
        self._recording_app_info: AppInfo | None = None
        self._play_sound = play_sound
        self._min_clip_seconds = min_clip_seconds
        self._tap_max = min_clip_seconds
        self._double_tap_window = DOUBLE_TAP_WINDOW
        self._triple_cancel_window = TRIPLE_CANCEL_WINDOW
        self._clock = clock
        self._on_state = on_state
        self._app_state = "idle"
        self._events: queue.Queue = queue.Queue()
        self._toggle_intent = False
        self._toggle_lock = threading.Lock()
        self._legacy_active = False
        self._gesture_state = "IDLE"
        self._key_held = False
        self._down_at: float | None = None
        self._tapped_at: float | None = None
        self._locked_at: float | None = None
        self._recording_started_at: float | None = None
        self._max_warned = False
        self._command_active = False
        self._command_selection = ""
        self._command_clipboard_before = None
        self._last_text = ""
        self._paused_media: set[str] = set()
        self._thread = threading.Thread(
            target=self._run, name="pushtotalk-consumer", daemon=True
        )
        self._thread.start()

    # The following methods are called by pynput callbacks. Keep them limited
    # to recording a timestamp and enqueueing an event.
    def note_start(self) -> None:
        self._events.put(("start", self._clock()))

    def note_stop(self) -> None:
        self._events.put(("stop", self._clock()))

    def note_down(self) -> None:
        self._events.put(("down", self._clock()))

    def note_up(self) -> None:
        self._events.put(("up", self._clock()))

    def note_cancel(self) -> None:
        self._events.put(("cancel", self._clock()))

    def note_command_down(self) -> None:
        self._events.put(("command_down", self._clock()))

    def note_command_up(self) -> None:
        self._events.put(("command_up", self._clock()))

    def note_transform(self, preset: str) -> None:
        self._events.put(("transform", preset))

    def note_paste_last(self) -> None:
        self._events.put(("paste_last", self._clock()))

    def note_job_result(
        self,
        text: str | None,
        clipboard_before: str | None = None,
        error: bool = False,
    ) -> None:
        self._events.put(("result", text, clipboard_before, error))

    def note_toggle(self) -> None:
        with self._toggle_lock:
            self._toggle_intent = not self._toggle_intent
            if self._toggle_intent:
                self.note_start()
            else:
                self.note_stop()

    def shutdown(self, timeout: float | None = 1.0) -> None:
        self._events.put(_SHUTDOWN)
        self._thread.join(timeout=timeout)

    def _publish_state(self, state: str) -> None:
        if state == self._app_state:
            return
        self._app_state = state
        if self._on_state is not None:
            try:
                self._on_state(state)
            except Exception:
                log.exception("status update failed")

    def _play(self, cue: str) -> None:
        if self._config.sounds_enabled:
            self._play_sound(cue)

    def _run(self) -> None:
        pending = None
        while True:
            try:
                event = pending if pending is not None else self._events.get(
                    timeout=self._timer_timeout()
                )
            except queue.Empty:
                self._check_timers(self._clock())
                continue
            pending = None
            if event is _SHUTDOWN:
                if self._recorder.recording:
                    log.warning("shutdown received mid-recording — stopping mic first")
                    self._do_stop()
                return
            if event[0] == "result":
                self._handle_result(event[1], event[2], event[3])
                continue
            if event[0] == "transform":
                self._handle_transform(event[1])
                continue
            kind, timestamp = event
            self._check_timers(timestamp)
            if kind == "start":
                if self._recorder.recording:
                    log.debug("stray start event (already recording), dropped")
                    continue
                try:
                    lookahead = self._events.get_nowait()
                except queue.Empty:
                    lookahead = None
                if lookahead is _SHUTDOWN:
                    pending = lookahead
                elif lookahead is not None:
                    next_kind, next_timestamp = lookahead[:2]
                    if (
                        next_kind == "stop"
                        and next_timestamp - timestamp < self._min_clip_seconds
                    ):
                        log.info(
                            "spam press coalesced (%.2fs), dropped",
                            next_timestamp - timestamp,
                        )
                        continue
                    pending = lookahead
                if self._do_start():
                    self._legacy_active = True
            elif kind == "stop":
                if not self._recorder.recording:
                    log.debug("stray stop event (not recording), dropped")
                    continue
                self._legacy_active = False
                self._do_stop()
            elif kind == "down":
                pending = self._handle_down(timestamp)
            elif kind == "up":
                self._handle_up(timestamp)
            elif kind == "cancel":
                self._handle_cancel()
            elif kind == "command_down":
                self._start_command()
            elif kind == "command_up":
                self._stop_command()
            elif kind == "paste_last":
                if self._last_text:
                    paste_text(self._last_text, self._config.paste_method)

    def _timer_timeout(self) -> float:
        now = self._clock()
        deadlines = []
        if self._gesture_state == "TAPPED" and self._tapped_at is not None:
            deadlines.append(self._tapped_at + self._double_tap_window)
        maximum = getattr(self._config, "max_recording_seconds", 1200)
        if self._recording_started_at is not None and maximum > 0:
            warning_at = max(0, maximum - 60)
            if not self._max_warned:
                deadlines.append(self._recording_started_at + warning_at)
            deadlines.append(self._recording_started_at + maximum)
        if not deadlines:
            return 0.05
        return max(0.001, min(0.05, min(deadlines) - now))

    def _check_timers(self, now: float) -> None:
        if (
            self._gesture_state == "TAPPED"
            and self._tapped_at is not None
            and now - self._tapped_at >= self._double_tap_window
        ):
            self._discard_recording()
            self._gesture_state = "IDLE"
            self._tapped_at = None
            self._key_held = False
            self._publish_state("idle")
        started_at = self._recording_started_at
        maximum = getattr(self._config, "max_recording_seconds", 1200)
        if started_at is None or maximum <= 0:
            return
        elapsed = max(0, now - started_at)
        warning_at = max(0, maximum - 60)
        if not self._max_warned and elapsed >= warning_at:
            self._max_warned = True
            self._play("warn")
        if elapsed >= maximum:
            log.info("max duration reached")
            self._legacy_active = False
            if self._command_active:
                self._stop_command()
            else:
                self._do_stop()
            self._gesture_state = "AWAIT_UP" if self._key_held else "IDLE"
            self._tapped_at = None
            self._locked_at = None

    def _handle_down(self, timestamp: float):
        if self._gesture_state == "IDLE":
            try:
                lookahead = self._events.get_nowait()
            except queue.Empty:
                lookahead = None
            if lookahead is _SHUTDOWN:
                return lookahead
            if lookahead is not None and lookahead is not _SHUTDOWN:
                kind = lookahead[0]
                if kind == "up":
                    up_at = lookahead[1]
                    if 0 <= up_at - timestamp <= self._tap_max:
                        self._gesture_state = "TAPPED"
                        self._tapped_at = up_at
                        self._key_held = False
                        return None
                self._events.put(lookahead)
            self._key_held = True
            self._down_at = timestamp
            if self._do_start():
                self._gesture_state = "HELD"
            else:
                self._gesture_state = "AWAIT_UP"
            return None

        if self._gesture_state == "HELD":
            return None

        if self._gesture_state == "TAPPED":
            tapped_at = self._tapped_at if self._tapped_at is not None else timestamp
            if timestamp - tapped_at > self._double_tap_window:
                self._discard_recording()
                self._gesture_state = "IDLE"
                self._tapped_at = None
                self._publish_state("idle")
                return self._handle_down(timestamp)
            self._key_held = True
            self._locked_at = timestamp
            if not self._recorder.recording and not self._do_start():
                self._gesture_state = "AWAIT_UP"
                return None
            self._gesture_state = "LOCKED"
            self._publish_state("locked")
            self._play("lock")
            print("🔒 locked")
            return None

        if self._gesture_state == "LOCKED":
            locked_at = self._locked_at if self._locked_at is not None else timestamp
            self._key_held = True
            if timestamp - locked_at <= self._triple_cancel_window:
                self._discard_recording()
                self._play("cancel")
                self._publish_state("idle")
                self._gesture_state = "AWAIT_UP"
                print("dictation cancelled")
            else:
                self._do_stop()
                self._gesture_state = "AWAIT_UP"
            self._locked_at = None
            return None

        return None

    def _handle_up(self, timestamp: float) -> None:
        if self._gesture_state == "AWAIT_UP":
            self._key_held = False
            self._gesture_state = "IDLE"
            return
        if self._gesture_state == "HELD":
            self._key_held = False
            down_at = self._down_at if self._down_at is not None else timestamp
            if timestamp - down_at >= self._tap_max:
                self._do_stop()
                self._gesture_state = "IDLE"
            else:
                self._gesture_state = "TAPPED"
                self._tapped_at = timestamp
            self._down_at = None
            return
        if self._gesture_state == "LOCKED":
            self._key_held = False

    def _handle_cancel(self) -> None:
        if self._gesture_state not in {"HELD", "TAPPED", "LOCKED", "COMMAND"} and not (
            self._legacy_active and self._recorder.recording
        ):
            return
        self._discard_recording()
        self._legacy_active = False
        self._play("cancel")
        self._gesture_state = "AWAIT_UP" if self._key_held else "IDLE"
        self._tapped_at = None
        self._locked_at = None
        with self._toggle_lock:
            self._toggle_intent = False
        self._publish_state("idle")
        self._command_active = False
        print("dictation cancelled")

    def _start_command(self) -> None:
        if self._gesture_state != "IDLE" or self._recorder.recording:
            return
        captured = capture_selection()
        app_info = frontmost_app()
        self._command_selection = captured.text
        self._command_clipboard_before = captured.clipboard_before
        if self._do_start(app_info=app_info, state="command"):
            self._command_active = True
            self._gesture_state = "COMMAND"

    def _stop_command(self) -> None:
        if not self._command_active:
            return
        self._command_active = False
        self._do_stop(
            kind="command",
            selection=self._command_selection,
            clipboard_before=self._command_clipboard_before,
            force=True,
        )
        self._command_selection = ""
        self._command_clipboard_before = None
        self._gesture_state = "IDLE"

    def _handle_transform(self, preset: str) -> None:
        captured = capture_selection()
        if not captured.text:
            log.error("%s needs selected text", preset)
            self._play("error")
            return
        self._work_queue.put(
            Job(
                "transform",
                app_info=frontmost_app(),
                selection=captured.text,
                preset=preset,
                clipboard_before=captured.clipboard_before,
            )
        )
        self._publish_state("processing")

    def _handle_result(
        self, text: str | None, clipboard_before: str | None, error: bool
    ) -> None:
        if not text:
            if error:
                log.error("text transform returned no output")
                print("text transform failed")
                self._play("error")
            self._publish_state("idle")
            return
        self._last_text = text
        paste_text(text, self._config.paste_method, clipboard_before)
        self._publish_state("idle")

    def _do_start(self, app_info: AppInfo | None = None, state: str = "recording") -> bool:
        try:
            self._recording_app_info = app_info if app_info is not None else frontmost_app()
            self._recorder.start()
        except Exception as exc:
            self._recording_app_info = None
            log.exception("mic error")
            print(f"mic error: {exc}")
            self._publish_state("idle")
            return False
        self._recording_started_at = self._clock()
        self._max_warned = False
        self._play("start")
        if getattr(self._config, "pause_media_while_dictating", False):
            self._paused_media = media.pause_playing()
        self._publish_state(state)
        print("● recording")
        return True

    def _do_stop(
        self,
        kind: str = "dictate",
        selection: str = "",
        clipboard_before: str | None = None,
        force: bool = False,
    ) -> None:
        if not self._recorder.recording:
            return
        audio = self._recorder.stop()
        app_info = self._recording_app_info
        self._recording_app_info = None
        self._recording_started_at = None
        self._max_warned = False
        self._play("stop")
        media.resume(self._paused_media)
        self._paused_media = set()
        print("■ transcribing…")
        duration = len(audio) / self._config.sample_rate if self._config.sample_rate else 0
        if duration < self._min_clip_seconds and not force:
            log.info("clip too short (%.2fs), dropped", duration)
            self._publish_state("idle")
            return
        self._work_queue.put(
            Job(
                kind,
                audio=audio,
                app_info=app_info,
                selection=selection,
                clipboard_before=clipboard_before,
            )
        )
        self._publish_state("processing")

    def _discard_recording(self) -> None:
        if self._recorder.recording:
            try:
                self._recorder.stop()
            except Exception:
                log.exception("failed to discard recording")
        self._recording_app_info = None
        self._recording_started_at = None
        self._max_warned = False
        media.resume(self._paused_media)
        self._paused_media = set()
