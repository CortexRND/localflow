import json
import os
import tempfile
import time
from pathlib import Path

STATUS_PATH = Path.home() / ".localflow" / "status.json"
LAST_TEXT_PATH = Path.home() / ".localflow" / "last.txt"
_PREFIXES = {
    "recording": "● ",
    "locked": "🔒 ",
    "command": "⌘ ",
    "processing": "… ",
}


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
        if pid <= 0:
            return "idle"
        os.kill(pid, 0)
        return data.get("state", "idle")
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError):
        return "idle"
