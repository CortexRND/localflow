import ctypes
import json
import os
import subprocess
import tempfile
import time
from ctypes import wintypes
from pathlib import Path

STATUS_PATH = Path.home() / ".localflow" / "status.json"
LAST_TEXT_PATH = Path.home() / ".localflow" / "last.txt"
_IS_WINDOWS = os.name == "nt"
_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_ERROR_ACCESS_DENIED = 5
_PREFIXES = {
    "recording": "● ",
    "locked": "🔒 ",
    "command": "⌘ ",
    "processing": "… ",
}


def _windows_pid_is_alive(pid: int) -> bool:
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = (
        wintypes.DWORD,
        wintypes.BOOL,
        wintypes.DWORD,
    )
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    kernel32.CloseHandle.restype = wintypes.BOOL
    handle = kernel32.OpenProcess(
        _PROCESS_QUERY_LIMITED_INFORMATION,
        False,
        pid,
    )
    if not handle:
        return ctypes.get_last_error() == _ERROR_ACCESS_DENIED
    try:
        return True
    finally:
        kernel32.CloseHandle(handle)


def _pid_is_alive(pid: int) -> bool:
    if _IS_WINDOWS:
        return _windows_pid_is_alive(pid)
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "pid="],
            capture_output=True,
            text=True,
            timeout=1,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return bool(result.stdout.strip())


def title_prefix(state: str) -> str:
    return _PREFIXES.get(state, "")


def _write_atomic(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", dir=path.parent, prefix=f".{path.name}.",
            delete=False,
        ) as handle:
            temporary = Path(handle.name)
            os.chmod(temporary, 0o600)
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None and temporary.exists():
            temporary.unlink()


def write_state(state: str, path: Path = STATUS_PATH) -> None:
    _write_atomic(
        path,
        json.dumps({"state": state, "since": time.time(), "pid": os.getpid()}),
    )


def write_last_text(text: str, path: Path = LAST_TEXT_PATH) -> None:
    _write_atomic(path, text)


def read_last_text(path: Path = LAST_TEXT_PATH) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return ""


def live_state(path: Path = STATUS_PATH) -> str:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        pid = int(data["pid"])
        if pid <= 0 or not _pid_is_alive(pid):
            return "idle"
        return data.get("state", "idle")
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError):
        return "idle"
