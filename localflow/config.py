import json
import logging
import os
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

import platformdirs

log = logging.getLogger("localflow.config")

CONFIG_VERSION = 2

_SECTIONS: dict[str, dict[str, str]] = {
    "stt": {
        "provider": "stt_backend",
        "model": "model_size",
        "language": "language",
        "device": "stt_device",
    },
    "llm": {
        "provider": "llm_provider",
        "base_url": "llm_base_url",
        "model": "llm_model",
        "cleanup_enabled": "cleanup_enabled",
        "cleanup_timeout": "cleanup_timeout",
        "num_ctx": "cleanup_num_ctx",
    },
    "hotkey": {"key": "hotkey", "sounds": "sounds_enabled"},
    "paste": {"method": "paste_method"},
    "dictation": {
        "spoken_symbols": "spoken_symbols",
        "command_names": "command_names",
        "skills_dirs": "skills_dirs",
        "sample_rate": "sample_rate",
    },
    "server": {"host": "server_host", "port": "server_port"},
    "features": {"meetings": "meetings_enabled"},
    "meetings": {
        "vault_path": "vault_path",
        "notes_folder": "notes_folder",
        "logs_folder": "logs_folder",
        "watch": "meeting_watch",
        "chunk_seconds": "meeting_chunk_seconds",
        "min_busy_seconds": "meeting_min_busy_seconds",
        "auto_start": "meeting_auto_start",
        "work_prompts": "work_prompts",
        "fireworks_model": "fireworks_model",
        "prompts_dir": "prompts_dir",
        "orca_repo": "orca_repo",
        "orca_agent": "orca_agent",
    },
}


class ConfigError(RuntimeError):
    """Raised when a configuration file cannot be parsed."""


@dataclass
class Config:
    model_size: str = "base"           # whisper model size (ignored by parakeet: fixed model id)
    stt_backend: str = "auto"          # auto | mlx-whisper | faster-whisper | parakeet | entry-point id
    language: str | None = None        # None = autodetect (parakeet is English-only)
    stt_device: str = "auto"
    llm_provider: str = "ollama"       # ollama | openai-compatible | entry-point id
    llm_base_url: str = "http://localhost:11434"
    llm_model: str = "llama3.2:3b"
    cleanup_enabled: bool = True
    cleanup_timeout: int = 120
    cleanup_num_ctx: int = 8192
    # Bare pynput Key name ('alt_l' = left Option) = hold-to-talk;
    # GlobalHotKeys combo ('<cmd>+<shift>+<space>') = toggle.
    hotkey: str = "alt_l"
    sounds_enabled: bool = True        # audio cue on record start/stop
    paste_method: str = "auto"
    spoken_symbols: bool = True        # dictated "slash"/"dash"/"underscore" -> / - _
    command_names: list[str] = field(default_factory=list)
    skills_dirs: list[str] = field(
        default_factory=lambda: ["~/.claude/skills", "~/.agents/skills"]
    )
    sample_rate: int = 16000
    # Bind loopback by default: the API has no auth, so 0.0.0.0 exposes it to
    # the whole LAN. Set "0.0.0.0" explicitly to opt into network access.
    server_host: str = "127.0.0.1"
    server_port: int = 8756
    meetings_enabled: bool = sys.platform == "darwin"  # meeting watcher needs CoreAudio; forced off elsewhere
    vault_path: str = "~/projs"
    notes_folder: str = "notes/meetings"
    logs_folder: str = "notes/transcripts"
    meeting_watch: bool = True
    meeting_chunk_seconds: int = 30
    meeting_min_busy_seconds: int = 12
    meeting_auto_start: bool = True
    work_prompts: bool = True
    fireworks_model: str = "accounts/fireworks/models/kimi-k2p6"
    prompts_dir: str = "~/projs/prompts/meetings"
    orca_repo: str = ""                # Orca dispatch target, e.g. "id:<repoId>"; empty = not configured
    orca_agent: str = "claude"


def config_dir() -> Path:
    return Path(platformdirs.user_config_dir("localflow", appauthor=False))


def config_path() -> Path:
    override = os.environ.get("LOCALFLOW_CONFIG")
    if override:
        return Path(override)
    return config_dir() / "config.toml"


def legacy_config_path() -> Path:
    return Path.home() / ".localflow.toml"


def _parse(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{path}: {exc}") from exc


def _config_from_v2(data: dict) -> Config:
    config = Config()
    for section, values in data.items():
        if section == "config_version":
            continue
        mapping = _SECTIONS.get(section)
        if mapping is None:
            log.warning("unknown config section %r", section)
            continue
        if not isinstance(values, dict):
            log.warning("config section %r is not a table", section)
            continue
        for key, value in values.items():
            field_name = mapping.get(key)
            if field_name is None:
                log.warning("unknown config key %s.%s", section, key)
                continue
            if field_name == "language" and value == "":
                value = None
            setattr(config, field_name, value)
    if sys.platform != "darwin":
        config.meetings_enabled = False
    return config


def _toml_value(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (str, int, float)):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return json.dumps(value, ensure_ascii=False)
    raise TypeError(f"unsupported config value: {value!r}")


def config_toml(config: Config) -> str:
    lines = [f"config_version = {CONFIG_VERSION}", ""]
    for section, mapping in _SECTIONS.items():
        lines.append(f"[{section}]")
        for file_key, field_name in mapping.items():
            value = getattr(config, field_name)
            if value is None:
                continue
            lines.append(f"{file_key} = {_toml_value(value)}")
        lines.append("")
    return "\n".join(lines)


def save_config(config: Config, path: Path | None = None) -> Path:
    target = Path(path) if path is not None else config_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            dir=target.parent,
            prefix=f".{target.name}.",
            suffix=".tmp",
            delete=False,
        ) as handle:
            temporary = Path(handle.name)
            handle.write(config_toml(config))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, target)
    finally:
        if temporary is not None and temporary.exists():
            temporary.unlink()
    return target


def migrate_legacy(path: Path | None = None) -> Config | None:
    source = Path(path) if path is not None else legacy_config_path()
    if not source.exists():
        return None
    from localflow.config_migrate import migrate_v1

    data, fireworks_key = migrate_v1(_parse(source))
    config = _config_from_v2(data)
    if fireworks_key:
        from localflow.secrets import SecretsUnavailable, set_secret

        try:
            set_secret("fireworks_api_key", fireworks_key)
        except SecretsUnavailable:
            log.warning(
                "could not store fireworks_api_key in the keyring; "
                "set FIREWORKS_API_KEY in the environment instead"
            )
    target = config_path()
    save_config(config, target)
    log.info(
        "migrated ~/.localflow.toml to %s; the old file was left in place",
        target,
    )
    return config


def load_config() -> Config:
    """Load v2 config, migrating the legacy flat file when needed."""
    target = config_path()
    if target.exists():
        return _config_from_v2(_parse(target))
    legacy = legacy_config_path()
    if legacy.exists():
        migrated = migrate_legacy(legacy)
        if migrated is not None:
            return migrated
    config = Config()
    if sys.platform != "darwin":
        config.meetings_enabled = False
    return config


def config_field(section_key: str) -> tuple[str, str]:
    """Return (section, field_name) for a dotted section.key."""
    try:
        section, key = section_key.split(".", 1)
    except ValueError as exc:
        raise KeyError(section_key) from exc
    field_name = _SECTIONS.get(section, {}).get(key)
    if field_name is None:
        raise KeyError(section_key)
    return section, field_name


def valid_config_keys() -> list[str]:
    return [
        f"{section}.{key}"
        for section, mapping in _SECTIONS.items()
        for key in mapping
    ]
