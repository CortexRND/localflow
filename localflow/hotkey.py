import ctypes
import logging
import sys
import threading
import time
from collections.abc import Callable, Iterable

from pynput import keyboard

log = logging.getLogger("localflow.hotkey")

# kIOHIDRequestTypeListenEvent — the Input Monitoring TCC service.
_LISTEN_EVENT = 1
# A live tap sees *something* (any key, any modifier) well within this window
# of normal use; silence this long almost always means TCC is dropping events.
_DEAD_TAP_SECONDS = 15.0
_INPUT_MONITORING_HELP = (
    "hold-to-talk and global hotkeys cannot see keystrokes until Input Monitoring "
    "is enabled for the process macOS blames — 'Python' when LocalFlow is started "
    "by the app or a script (the launcher execs the interpreter, so Settings lists "
    "Python, not 'LocalFlow'), or your terminal app when run by hand — in "
    "System Settings → Privacy & Security → Input Monitoring. Restart LocalFlow "
    "after granting; macOS does not re-arm a running event tap."
)


def _iokit():
    iokit = ctypes.CDLL("/System/Library/Frameworks/IOKit.framework/IOKit")
    iokit.IOHIDCheckAccess.argtypes = [ctypes.c_uint32]
    iokit.IOHIDCheckAccess.restype = ctypes.c_int
    iokit.IOHIDRequestAccess.argtypes = [ctypes.c_uint32]
    iokit.IOHIDRequestAccess.restype = ctypes.c_bool
    return iokit


def _input_monitoring_granted() -> bool | None:
    """True/False from IOHIDCheckAccess; None when it can't be determined."""
    try:
        return _iokit().IOHIDCheckAccess(_LISTEN_EVENT) == 0
    except (AttributeError, OSError):
        return None


def _request_input_monitoring() -> None:
    # Makes macOS show its grant prompt / list the responsible app in Settings.
    try:
        _iokit().IOHIDRequestAccess(_LISTEN_EVENT)
    except (AttributeError, OSError):
        log.debug("IOHIDRequestAccess unavailable", exc_info=True)


def _warn(message: str) -> None:
    # Both: log for the app log file, stderr for whoever launched us by hand.
    log.warning(message)
    print(f"warning: {message}", file=sys.stderr)


class _Bindings:
    """One parsed set of shortcuts; swapped as a whole by `reconfigure`."""

    __slots__ = ("combos", "command_key", "hold_key", "toggle_combo")

    def __init__(self, hold_key, toggle_combo, combos, command_key):
        self.hold_key = hold_key
        self.toggle_combo = toggle_combo
        self.combos = combos
        self.command_key = command_key


class KeyboardHub:
    """Route hold keys and global shortcut chords through one listener.

    `reconfigure` swaps every binding in place, without restarting the pynput
    listener (on macOS that must not be torn down off the main thread). The
    swap is deferred while any key is down or `can_reconfigure()` says the
    app is busy (recording, locked, command mode), so a recording started
    with the old shortcut is always ended by the same physical key.
    """

    # A press whose release pynput never reported must not block a pending
    # reconfiguration forever once nothing else is held.
    STALE_PRESS_SECONDS = 10.0

    def __init__(
        self,
        hold_key: str | None,
        toggle_combo: str | None,
        combos: Iterable[tuple[str, Callable[[], None]]],
        on_down: Callable[[], None],
        on_up: Callable[[], None],
        on_toggle: Callable[[], None],
        on_cancel: Callable[[], None],
        command_key: str | None,
        on_command_down: Callable[[], None],
        on_command_up: Callable[[], None],
        on_command_cancel: Callable[[], None] | None = None,
        can_reconfigure: Callable[[], bool] | None = None,
        clock: Callable[[], float] = time.monotonic,
    ):
        self._pressed: set[object] = set()
        self._pressed_raw: set[object] = set()
        self._held_key_down = False
        self._command_key_down = False
        self._command_timer = None
        self._command_combo_key = None
        self._saw_any_event = False
        self._listener = None
        self._on_down = on_down
        self._on_up = on_up
        self._on_toggle = on_toggle
        self._on_cancel = on_cancel
        self._on_command_down = on_command_down
        self._on_command_up = on_command_up
        self._on_command_cancel = on_command_cancel or (lambda: None)
        self._can_reconfigure = can_reconfigure or (lambda: True)
        self._clock = clock
        # Guards the bindings and key state: pynput's thread, the command
        # timer and a reconfiguring thread all go through it.
        self._lock = threading.RLock()
        self._pending: _Bindings | None = None
        self._pending_since: float | None = None
        self._last_event_at: float | None = None
        self._install(
            self._build(hold_key, toggle_combo, combos, command_key, on_toggle)
        )

    def _build(self, hold_key, toggle_combo, combos, command_key, on_toggle) -> _Bindings:
        """Parse shortcuts into fresh pynput objects (no shared state touched)."""
        seen: dict[str, str] = {}

        def reserve(name: str, shortcut: str | None) -> bool:
            if not shortcut:
                return False
            if shortcut in seen:
                print(
                    f"warning: shortcut {shortcut!r} is already assigned to "
                    f"{seen[shortcut]}; disabling {name}"
                )
                return False
            seen[shortcut] = name
            return True

        if reserve("dictation", hold_key):
            parsed_hold = self._key_from_name(hold_key)
        else:
            parsed_hold = None

        if reserve("dictation", toggle_combo):
            parsed_toggle = self._parse_combo(toggle_combo, on_toggle)
        else:
            parsed_toggle = None

        parsed_combos = []
        for shortcut, callback in combos:
            if reserve("shortcut", shortcut):
                hotkey_ref = []

                def fire(
                    callback=callback,
                    hotkey_ref=hotkey_ref,
                ):
                    hotkey = hotkey_ref[0]
                    if (
                        self._command_key_down
                        and self._command_combo_key is not None
                        and self._command_combo_key in hotkey._keys
                    ):
                        self._command_key_down = False
                        self._on_command_cancel()
                    callback()

                hotkey = self._parse_combo(shortcut, fire)
                if hotkey is not None:
                    hotkey_ref.append(hotkey)
                    parsed_combos.append(hotkey)

        if reserve("command mode", command_key):
            parsed_command = self._key_from_name(command_key)
        else:
            parsed_command = None
        return _Bindings(parsed_hold, parsed_toggle, parsed_combos, parsed_command)

    def _install(self, bindings: _Bindings) -> None:
        with self._lock:
            self._hold_key = bindings.hold_key
            self._toggle_combo = bindings.toggle_combo
            self._combos = bindings.combos
            self._command_key = bindings.command_key
            self._command_combo_key = (
                self._canonical(self._command_key)
                if self._listener is not None and self._command_key is not None
                else None
            )

    def reconfigure(
        self,
        hold_key: str | None,
        toggle_combo: str | None,
        combos: Iterable[tuple[str, Callable[[], None]]],
        command_key: str | None,
    ) -> bool:
        """Replace every binding; True when applied now, False when deferred.

        A deferred set is applied by the first safe key release or
        `apply_pending()`; a newer reconfigure replaces an older pending one.
        """
        bindings = self._build(hold_key, toggle_combo, combos, command_key, self._on_toggle)
        with self._lock:
            self._pending = bindings
            if self._pending_since is None:
                self._pending_since = self._clock()
        return self.apply_pending()

    def apply_pending(self) -> bool:
        """Install a pending binding set if it is safe now. True when installed."""
        with self._lock:
            if self._pending is None or not self._safe_to_swap():
                return False
            bindings, self._pending, self._pending_since = self._pending, None, None
            # Every key is up: no chord can be half pressed in the new set.
            self._pressed.clear()
            self._pressed_raw.clear()
            self._install(bindings)
        log.info("keyboard shortcuts updated")
        return True

    @property
    def reconfigure_pending(self) -> bool:
        return self._pending is not None

    def _safe_to_swap(self) -> bool:
        if self._held_key_down or self._command_key_down or self._command_timer is not None:
            return False
        try:
            if not self._can_reconfigure():
                return False
        except Exception:
            log.exception("reconfigure check failed; deferring")
            return False
        if not self._pressed_raw:
            return True
        # Keys still look pressed. Only trust that for a while: pynput can
        # miss a release, and that must not freeze the old shortcuts forever.
        last = self._last_event_at if self._last_event_at is not None else self._pending_since
        return last is not None and self._clock() - last >= self.STALE_PRESS_SECONDS

    @staticmethod
    def _key_from_name(name: str | None):
        if not name:
            return None
        key = getattr(keyboard.Key, name, None)
        if key is not None:
            return key
        if len(name) == 1:
            try:
                return keyboard.KeyCode.from_char(name)
            except (AttributeError, ValueError):
                pass
        print(f"warning: invalid key {name!r}; disabling shortcut")
        return None

    @staticmethod
    def _parse_combo(shortcut: str | None, callback: Callable[[], None]):
        if not shortcut:
            return None
        try:
            return keyboard.HotKey(keyboard.HotKey.parse(shortcut), callback)
        except ValueError:
            print(f"warning: invalid shortcut {shortcut!r}; disabling shortcut")
            return None

    def _note_event(self) -> None:
        if not self._saw_any_event:
            self._saw_any_event = True
            log.info("first keyboard event received — Input Monitoring OK")

    def _preflight_input_monitoring(self) -> bool:
        """Return True when the tap is known to be allowed to see keystrokes."""
        if sys.platform != "darwin":
            # No TCC gate elsewhere; nothing to preflight or watch for.
            return True
        granted = _input_monitoring_granted()
        if granted is False:
            _request_input_monitoring()
            _warn(f"Input Monitoring not granted: {_INPUT_MONITORING_HELP}")
        return granted is True

    def _check_tap_alive(self) -> None:
        # The tap is created even when TCC denies it; it just never delivers.
        if not self._saw_any_event:
            _warn(
                f"no keyboard events received {_DEAD_TAP_SECONDS:.0f}s after start; "
                f"if hold-to-talk does not respond, the event tap is dead: "
                f"{_INPUT_MONITORING_HELP}"
            )

    def _canonical(self, key):
        if self._listener is None:
            return key
        return self._listener.canonical(key)

    @staticmethod
    def _invoke(callback: Callable[[], None]) -> None:
        try:
            callback()
        except Exception:
            log.exception("keyboard shortcut callback failed")

    def _press(self, key) -> None:
        with self._lock:
            self._last_event_at = self._clock()
            self._press_locked(key)

    def _release(self, key) -> None:
        with self._lock:
            self._last_event_at = self._clock()
            self._release_locked(key)
            if self._pending is not None and not self._pressed_raw:
                self.apply_pending()

    def _press_locked(self, key) -> None:
        self._note_event()
        canonical = self._canonical(key)
        if key in self._pressed_raw:
            return
        self._pressed_raw.add(key)
        self._pressed.add(canonical)
        if self._command_timer is not None and self._is_combo_chord():
            self._command_timer.cancel()
            self._command_timer = None
        if canonical == self._canonical(keyboard.Key.esc):
            self._invoke(self._on_cancel)
        if key == self._hold_key and self._hold_key is not None:
            self._held_key_down = True
            self._invoke(self._on_down)
        if (
            key == self._command_key
            and self._command_key is not None
            and not self._is_combo_chord()
        ):
            self._command_timer = threading.Timer(0.12, self._command_down_fire)
            self._command_timer.daemon = True
            self._command_timer.start()
        if self._toggle_combo is not None:
            self._toggle_combo.press(canonical)
        for hotkey in self._combos:
            hotkey.press(canonical)

    def _release_locked(self, key) -> None:
        self._note_event()
        canonical = self._canonical(key)
        self._pressed_raw.discard(key)
        if not any(self._canonical(pressed) == canonical for pressed in self._pressed_raw):
            self._pressed.discard(canonical)
            if self._toggle_combo is not None:
                self._toggle_combo.release(canonical)
            for hotkey in self._combos:
                hotkey.release(canonical)
        if (
            key == self._hold_key
            and self._hold_key is not None
            and self._held_key_down
        ):
            self._held_key_down = False
            self._invoke(self._on_up)
        if (
            key == self._command_key
            and self._command_key is not None
        ):
            if self._command_timer is not None:
                self._command_timer.cancel()
                self._command_timer = None
            elif self._command_key_down:
                self._command_key_down = False
                self._invoke(self._on_command_up)

    def _is_combo_chord(self) -> bool:
        for hotkey in self._combos:
            chord_keys = hotkey._keys - {self._command_combo_key}
            if (
                self._command_combo_key in hotkey._keys
                and chord_keys & self._pressed
            ):
                return True
        return False

    def _command_down_fire(self) -> None:
        with self._lock:
            if (
                self._command_key_down
                or self._command_key not in self._pressed_raw
                or self._is_combo_chord()
            ):
                self._command_timer = None
                return
            self._command_timer = None
            self._command_key_down = True
            self._invoke(self._on_command_down)

    def run_forever(self) -> None:
        granted = self._preflight_input_monitoring()
        self._listener = keyboard.Listener(on_press=self._press, on_release=self._release)
        with self._lock:
            if self._command_key is not None:
                self._command_combo_key = self._canonical(self._command_key)
        # Only watch for a dead tap when access is denied or unknown: with a
        # confirmed grant, 15s of silence is just a user who hasn't typed yet.
        watchdog = None
        if not granted:
            watchdog = threading.Timer(_DEAD_TAP_SECONDS, self._check_tap_alive)
            watchdog.daemon = True
        try:
            with self._listener as listener:
                if watchdog is not None:
                    watchdog.start()
                listener.join()
        finally:
            if watchdog is not None:
                watchdog.cancel()
            if self._command_timer is not None:
                self._command_timer.cancel()
                self._command_timer = None
        log.warning("keyboard listener exited")


class HotkeyListener:
    """Compatibility wrapper for a single toggle shortcut."""

    def __init__(self, combo: str, on_toggle: Callable[[], None]):
        self.combo = combo
        self.on_toggle = on_toggle

    def run_forever(self) -> None:
        hub = KeyboardHub(
            hold_key=None,
            toggle_combo=self.combo,
            combos=[],
            on_down=lambda: None,
            on_up=lambda: None,
            on_toggle=self.on_toggle,
            on_cancel=lambda: None,
            command_key=None,
            on_command_down=lambda: None,
            on_command_up=lambda: None,
        )
        hub.run_forever()


class HoldKeyListener:
    """Compatibility wrapper for a single push-to-talk key."""

    def __init__(
        self,
        key_name: str,
        on_start: Callable[[], None],
        on_stop: Callable[[], None],
    ):
        self.key_name = key_name
        self.on_start = on_start
        self.on_stop = on_stop

    def run_forever(self) -> None:
        hub = KeyboardHub(
            hold_key=self.key_name,
            toggle_combo=None,
            combos=[],
            on_down=self.on_start,
            on_up=self.on_stop,
            on_toggle=lambda: None,
            on_cancel=lambda: None,
            command_key=None,
            on_command_down=lambda: None,
            on_command_up=lambda: None,
        )
        hub.run_forever()


def is_hold_key(hotkey: str) -> bool:
    """A bare pynput Key name ('alt_l', 'f19') means hold mode; combos toggle."""
    return bool(hotkey) and "+" not in hotkey and hasattr(keyboard.Key, hotkey)
