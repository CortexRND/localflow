import logging
import threading
from collections.abc import Callable, Iterable

from pynput import keyboard

log = logging.getLogger("localflow.hotkey")


class KeyboardHub:
    """Route hold keys and global shortcut chords through one listener."""

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
            self._hold_key = self._key_from_name(hold_key)
        else:
            self._hold_key = None

        if reserve("dictation", toggle_combo):
            self._toggle_combo = self._parse_combo(toggle_combo, on_toggle)
        else:
            self._toggle_combo = None

        self._combos = []
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
                    self._combos.append(hotkey)

        if reserve("command mode", command_key):
            self._command_key = self._key_from_name(command_key)
        else:
            self._command_key = None

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

    def _release(self, key) -> None:
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
        self._listener = keyboard.Listener(on_press=self._press, on_release=self._release)
        if self._command_key is not None:
            self._command_combo_key = self._canonical(self._command_key)
        try:
            with self._listener as listener:
                listener.join()
        finally:
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
