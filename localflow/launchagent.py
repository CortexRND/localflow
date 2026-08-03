"""launchd LaunchAgent management — run localflow on login, always on.

Installs a per-user LaunchAgent that starts the `localflow` console script at
login and relaunches it on a crash (nonzero exit) but not after a clean exit
0 — see `plist_content` for why. Scoped to the Aqua session type because
dictation needs a GUI session for the microphone, Accessibility (paste
injection), and clipboard.
"""

import os
import plistlib
import subprocess
import sys
from pathlib import Path

LABEL = "com.cortexrnd.localflow"


def _default_program() -> str:
    """Path to the `localflow` console script next to the current Python.

    `sys.executable`'s directory is the venv's bin dir, where `pip install -e .`
    puts the `localflow` entry-point script alongside python/pip.
    """
    return str(Path(sys.executable).parent / "localflow")


def _log_dir() -> Path:
    return Path.home() / ".localflow"


def _plist_path() -> Path:
    return Path.home() / "Library" / "LaunchAgents" / f"{LABEL}.plist"


def _gui_target() -> str:
    return f"gui/{os.getuid()}/{LABEL}"


def _gui_domain() -> str:
    return f"gui/{os.getuid()}"


def plist_content(program: str | None = None) -> str:
    """XML plist text for the LaunchAgent.

    `program` defaults to the `localflow` console script next to
    `sys.executable` (see `_default_program`). Log paths are expanded to
    absolute paths under `~/.localflow/`.

    KeepAlive only restarts on a nonzero exit (crash), not a clean exit 0: a
    single-instance lock elsewhere means a manually-started localflow holding
    the lock makes the launchd copy exit 0, and KeepAlive=true would
    relaunch-loop it forever.
    """
    if program is None:
        program = _default_program()
    log_dir = _log_dir()
    plist = {
        "Label": LABEL,
        "ProgramArguments": [program],
        "RunAtLoad": True,
        "KeepAlive": {"SuccessfulExit": False},
        "LimitLoadToSessionType": "Aqua",
        "StandardOutPath": str(log_dir / "agent.out.log"),
        "StandardErrorPath": str(log_dir / "agent.err.log"),
    }
    return plistlib.dumps(plist).decode("utf-8")


def install(program: str | None = None) -> Path:
    """Write the plist and load it into launchd. Returns the plist path.

    If the agent is already loaded, it is booted out first so `bootstrap`
    doesn't fail with "already loaded". `bootstrap` failures are not
    diagnosed by matching stderr text — on real macOS, permission-denied,
    TCC/session issues, and "already bootstrapped" all produce a "Bootstrap
    failed" prefix too, so that string tells us nothing. Instead, ANY
    bootstrap failure falls back to `launchctl load -w`; if that also fails,
    both error outputs are raised together so the real cause isn't masked.
    """
    path = _plist_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(plist_content(program))
    _log_dir().mkdir(parents=True, exist_ok=True)

    # Unload any existing copy first; a clean "not loaded" failure here is
    # expected and ignored.
    subprocess.run(["launchctl", "bootout", _gui_target()], capture_output=True, text=True)

    result = subprocess.run(
        ["launchctl", "bootstrap", _gui_domain(), str(path)],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        bootstrap_err = (result.stderr or "").strip()
        fallback = subprocess.run(
            ["launchctl", "load", "-w", str(path)],
            capture_output=True, text=True,
        )
        if fallback.returncode != 0:
            load_err = (fallback.stderr or "").strip()
            raise RuntimeError(
                f"launchctl bootstrap failed: {bootstrap_err}; "
                f"launchctl load fallback also failed: {load_err}"
            )
    return path


def uninstall() -> None:
    """Boot the agent out of launchd and remove its plist."""
    subprocess.run(["launchctl", "bootout", _gui_target()], capture_output=True, text=True)
    path = _plist_path()
    if path.exists():
        path.unlink()


def status() -> str:
    """One-line running/not-loaded summary from `launchctl print`."""
    result = subprocess.run(
        ["launchctl", "print", _gui_target()],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return f"not loaded ({LABEL})"
    out = result.stdout or ""
    if "state = running" in out:
        return f"running ({LABEL})"
    return f"loaded, not running ({LABEL})"
