import logging
import os
import shutil
import subprocess
import sys
from pathlib import Path

import platformdirs

log = logging.getLogger("localflow.platform.linux")
_wayland_warned = False


class LinuxPlatform:
    name = "linux"

    def paste(self, method: str = "auto") -> None:
        global _wayland_warned
        if method == "auto":
            if os.environ.get("WAYLAND_DISPLAY") and shutil.which("wtype"):
                method = "wtype"
            elif shutil.which("xdotool"):
                method = "xdotool"
            else:
                method = "pynput"
        if (
            method == "pynput"
            and os.environ.get("WAYLAND_DISPLAY")
            and not shutil.which("wtype")
            and not shutil.which("xdotool")
            and not _wayland_warned
        ):
            log.warning(
                "Wayland detected but neither wtype nor xdotool is installed; "
                "pynput paste may not work"
            )
            _wayland_warned = True
        if method == "wtype":
            subprocess.run(["wtype", "-M", "ctrl", "v", "-m", "ctrl"], check=False)
            return
        if method == "xdotool":
            subprocess.run(
                ["xdotool", "key", "--clearmodifiers", "ctrl+v"],
                check=False,
            )
            return
        if method == "pynput":
            from pynput import keyboard

            controller = keyboard.Controller()
            with controller.pressed(keyboard.Key.ctrl):
                controller.press("v")
                controller.release("v")
            return
        raise ValueError(f"unknown paste method: {method!r}")

    def notify(self, title: str, message: str) -> None:
        if shutil.which("notify-send"):
            subprocess.run(
                ["notify-send", "--app-name", "localflow", title, message],
                check=False,
            )
        else:
            log.info("%s: %s", title, message)

    def play_sound(self, cue: str) -> None:
        sound_name = {"start": "message.oga", "stop": "complete.oga"}.get(cue)
        if sound_name is None:
            return
        sound = Path("/usr/share/sounds/freedesktop/stereo") / sound_name
        if not sound.exists():
            return
        player = next(
            (candidate for candidate in ("paplay", "pw-play", "aplay") if shutil.which(candidate)),
            None,
        )
        if player is None:
            return
        try:
            subprocess.Popen(
                [player, str(sound)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, RuntimeError, ValueError):
            return

    def open_path(self, target: str) -> None:
        subprocess.run(["xdg-open", target], check=False)

    def autostart_install(self, program: str | None = None) -> Path:
        if program is None:
            program = str(Path(sys.executable).parent / "localflow")
        path = self._autostart_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "[Desktop Entry]\n"
            "Type=Application\n"
            "Name=localflow\n"
            f"Exec={program}\n"
            "X-GNOME-Autostart-enabled=true\n"
        )
        return path

    def autostart_uninstall(self) -> None:
        path = self._autostart_path()
        if path.exists():
            path.unlink()

    def autostart_status(self) -> str:
        path = self._autostart_path()
        if path.exists():
            return f"installed ({path})"
        return "not installed"

    @staticmethod
    def _autostart_path() -> Path:
        return Path(platformdirs.user_config_dir()) / "autostart" / "localflow.desktop"

    def try_lock(self, fd: int) -> bool:
        from localflow.platform._posix_lock import try_lock

        return try_lock(fd)

    def unlock(self, fd: int) -> None:
        from localflow.platform._posix_lock import unlock

        unlock(fd)
