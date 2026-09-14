import secrets
from pathlib import Path

from localflow.config import config_dir


def token_path() -> Path:
    return config_dir() / "api-token"


def load_or_create() -> str:
    token = secrets.token_urlsafe(32)
    path = token_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(token + "\n", encoding="utf-8")
        try:
            path.chmod(0o600)
        except OSError:
            pass
    except OSError:
        pass
    return token
