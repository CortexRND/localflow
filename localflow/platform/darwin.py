import subprocess
from pathlib import Path

_SOUNDS = {
    "start": "/System/Library/Sounds/Pop.aiff",
    "stop": "/System/Library/Sounds/Bottle.aiff",
}


def _escape_applescript(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


class DarwinPlatform:
    name = "darwin"

    def paste(self, method: str = "auto") -> None:
        del method
        subprocess.run(
            [
                "osascript",
                "-e",
                'tell application "System Events" to keystroke "v" using command down',
            ],
            check=False,
        )

    def notify(self, title: str, message: str) -> None:
        script = (
            f'display notification "{_escape_applescript(message)}" '
            f'with title "{_escape_applescript(title)}"'
        )
        subprocess.run(["osascript", "-e", script], check=False, capture_output=True)

    def play_sound(self, cue: str) -> None:
        path = _SOUNDS.get(cue)
        if not path:
            return
        try:
            subprocess.Popen(
                ["afplay", path],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, RuntimeError, ValueError):
            return

    def open_path(self, target: str) -> None:
        subprocess.run(["open", target], check=False)

    def autostart_install(self, program: str | None = None) -> Path:
        from localflow.launchagent import install

        return install(program)

    def autostart_uninstall(self) -> None:
        from localflow.launchagent import uninstall

        uninstall()

    def autostart_status(self) -> str:
        from localflow.launchagent import status

        return status()

    def try_lock(self, fd: int) -> bool:
        from localflow.platform._posix_lock import try_lock

        return try_lock(fd)

    def unlock(self, fd: int) -> None:
        from localflow.platform._posix_lock import unlock

        unlock(fd)
