import platform
import subprocess

_PLAYERS = ("Music", "Spotify")


def pause_playing() -> set[str]:
    if platform.system() != "Darwin":
        return set()
    paused = set()
    for app in _PLAYERS:
        script = (
            f'tell application "System Events" to set runningApp to exists '
            f'(processes where name is "{app}")\n'
            f'if runningApp then\n'
            f'  tell application "{app}"\n'
            f'    if player state is playing then\n'
            f'      pause\n'
            f'      return "paused"\n'
            f'    end if\n'
            f'  end tell\n'
            f'end if\n'
            f'return "unchanged"'
        )
        try:
            result = subprocess.run(
                ["osascript", "-e", script],
                check=False,
                capture_output=True,
                text=True,
                timeout=2,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if result.stdout.strip() == "paused":
            paused.add(app)
    return paused


def resume(paused: set[str]) -> None:
    if platform.system() != "Darwin":
        return
    for app in paused:
        try:
            subprocess.run(
                ["osascript", "-e", f'tell application "{app}" to play'],
                check=False,
                capture_output=True,
                timeout=2,
            )
        except (OSError, subprocess.SubprocessError):
            continue
