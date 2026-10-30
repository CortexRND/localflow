import tomllib
import json
import os
import stat
import tempfile
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import get_args, get_origin, get_type_hints

CONFIG_PATH = Path.home() / ".localflow.toml"

@dataclass
class Config:
    model_size: str = "base"           # whisper model size (ignored by parakeet: fixed model id)
    stt_backend: str = "auto"          # auto | mlx | faster-whisper | parakeet
    language: str | None = None        # None = autodetect (parakeet is English-only)
    ollama_url: str = "http://localhost:11434"
    ollama_model: str = "llama3.2:3b"
    cleanup_enabled: bool = True
    cleanup_timeout: int = 120         # seconds; long dictations need more time
    cleanup_num_ctx: int = 8192        # Ollama context window for cleanup
    # Bare pynput Key name ('alt_l' = left Option) = hold-to-talk;
    # GlobalHotKeys combo ('<cmd>+<shift>+<space>') = toggle.
    hotkey: str = "alt_l"
    sounds_enabled: bool = True        # audio cue on record start/stop
    spoken_symbols: bool = True        # dictated "slash"/"dash"/"underscore" -> / - _
    hot_phrases: bool = True           # spoken trigger phrases expand into saved prompts
    # Registered slash-command names: after a dictated "slash", an exact spoken
    # match is snapped to the registered spelling ("slash skill part" ->
    # "/skill_part"). Names listed here plus subdirectory names of skills_dirs.
    command_names: list[str] = field(default_factory=list)
    skills_dirs: list[str] = field(
        default_factory=lambda: ["~/.claude/skills", "~/.agents/skills"]
    )
    sample_rate: int = 16000
    # Bind loopback by default: the API (mic recording, transcription, prompt
    # approve/reject) has no auth, so 0.0.0.0 exposed it to the whole LAN.
    # Set to "0.0.0.0" explicitly in config to opt into network access.
    server_host: str = "127.0.0.1"
    server_port: int = 8756
    # Meeting transcription -> Obsidian. ~/projs is itself a vault (it has a
    # .obsidian/ at its root), which is where the work these notes describe
    # lives, so notes land beside the repos rather than in a separate vault.
    vault_path: str = "~/projs"
    notes_folder: str = "notes/meetings"
    logs_folder: str = "notes/transcripts"
    meeting_watch: bool = True         # detect mic-in-use and notify
    meeting_chunk_seconds: int = 30    # transcribe in chunks of this length
    meeting_min_busy_seconds: int = 12 # sustained mic use before "meeting detected"
    # Work-prompt extraction from meeting notes (open-weights model on Fireworks).
    # Key comes from FIREWORKS_API_KEY env var or fireworks_api_key here.
    work_prompts: bool = True
    fireworks_api_key: str = ""
    fireworks_model: str = "accounts/fireworks/models/kimi-k2p6"
    # Auto-start transcription on detection instead of only notifying.
    meeting_auto_start: bool = True
    # Where extracted work prompts are written, one file per prompt.
    prompts_dir: str = "~/projs/prompts/meetings"
    # Orca dispatch target, e.g. "id:<repoId>"; empty = dispatch not configured.
    orca_repo: str = ""
    orca_agent: str = "claude"
    input_device: str = ""
    setup_version: int = 0


def load_config(path: Path | None = None) -> Config:
    """Read the local TOML config if present; missing file is OK."""
    config = Config()
    path = path or CONFIG_PATH
    if not path.exists():
        return config

    with path.open("rb") as f:
        data = tomllib.load(f)

    known_keys = {f.name for f in fields(Config)}
    for key, value in data.items():
        if key in known_keys:
            setattr(config, key, value)

    return config


def _toml_key(key: str) -> str:
    if key and all(char.isalnum() or char in "_-" for char in key) and not key[0].isdigit():
        return key
    return json.dumps(key, ensure_ascii=False)


def _toml_value(value) -> str:
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(item) for item in value) + "]"
    raise ValueError(f"unsupported TOML value: {type(value).__name__}")


def _serialize_toml(data: dict) -> str:
    lines = []
    for key, value in data.items():
        if isinstance(value, dict):
            lines.append(f"[{_toml_key(key)}]")
            lines.extend(f"{_toml_key(k)} = {_toml_value(v)}" for k, v in value.items())
        else:
            lines.append(f"{_toml_key(key)} = {_toml_value(value)}")
    return "\n".join(lines) + ("\n" if lines else "")


def _validate_update(key: str, value) -> None:
    expected = get_type_hints(Config)[key]
    accepted = tuple(
        arg for arg in get_args(expected) if arg is not type(None)
    ) or (expected,)
    origin = get_origin(expected)
    if origin is list:
        if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
            raise ValueError(f"{key} must be a list of strings")
        return
    for candidate in accepted:
        if candidate is bool and isinstance(value, bool):
            return
        if candidate is int and isinstance(value, int) and not isinstance(value, bool):
            return
        if candidate is float and isinstance(value, (float, int)) and not isinstance(value, bool):
            return
        if candidate is str and isinstance(value, str):
            return
    names = " or ".join(item.__name__ for item in accepted)
    raise ValueError(f"{key} must be {names}")


def save_config(updates: dict, path: Path | None = None) -> Path:
    """Merge validated fields into TOML and atomically replace the config."""
    path = path or CONFIG_PATH
    known_keys = {item.name for item in fields(Config)}
    unknown = set(updates) - known_keys
    if unknown:
        raise ValueError(f"unknown config key(s): {', '.join(sorted(unknown))}")

    existing = {}
    previous = None
    if path.exists():
        previous = path.read_bytes()
        existing = tomllib.loads(previous.decode("utf-8"))
    for key, value in updates.items():
        if value is None:
            existing.pop(key, None)
            continue
        _validate_update(key, value)
        existing[key] = value

    serialized = _serialize_toml(existing)
    path.parent.mkdir(parents=True, exist_ok=True)
    if previous is not None:
        backup = path.with_name(path.name + ".bak")
        backup.write_bytes(previous)
        backup.chmod(stat.S_IMODE(path.stat().st_mode))
    mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o600
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as temp_file:
            temp_file.write(serialized)
            temp_file.flush()
            os.fsync(temp_file.fileno())
        os.replace(temp_name, path)
    finally:
        if os.path.exists(temp_name):
            os.unlink(temp_name)
    return path
