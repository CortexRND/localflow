import json
import logging
import os
import re
import sys
import tempfile
import tomllib
from dataclasses import InitVar, dataclass, field, replace
from pathlib import Path

import platformdirs

from localflow import llm_activation
from localflow.platform.base import PASTE_METHODS

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
        "fast_model": "llm_fast_model",
        "api_key_ref": "llm_key_ref",
        "cleanup_enabled": "cleanup_enabled",
        "cleanup_timeout": "cleanup_timeout",
        "num_ctx": "cleanup_num_ctx",
    },
    "cleanup": {
        "level": "cleanup_level",
        "styles_enabled": "styles_enabled",
        "style_work": "style_work",
        "style_email": "style_email",
        "style_personal": "style_personal",
        "style_other": "style_other",
        "app_overrides": "app_category_overrides",
    },
    "hotkey": {
        "key": "hotkey",
        "sounds": "sounds_enabled",
        "paste_last": "paste_last_hotkey",
        "command": "command_hotkey",
        "polish": "polish_hotkey",
        "prompt": "prompt_hotkey",
        "dictation_toggle": "dictation_toggle_hotkey",
    },
    "paste": {"method": "paste_method"},
    "dictation": {
        "enabled": "dictation_enabled",
        "spoken_symbols": "spoken_symbols",
        "spoken_punctuation": "spoken_punctuation",
        "silence_threshold_dbfs": "silence_threshold_dbfs",
        "command_names": "command_names",
        "skills_dirs": "skills_dirs",
        "sample_rate": "sample_rate",
        "input_device": "input_device",
        "max_recording_seconds": "max_recording_seconds",
        "pause_media": "pause_media_while_dictating",
    },
    "server": {"host": "server_host", "port": "server_port"},
    "features": {
        "meetings": "meetings_enabled",
        "hot_phrases": "hot_phrases",
        "perf_metrics": "perf_metrics",
    },
    "history": {
        "enabled": "history_enabled",
        "keep_audio_clips": "keep_audio_clips",
    },
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
        "capture_system_audio": "capture_system_audio",
        "system_audio_device": "system_audio_device",
        "default_template": "meeting_default_template",
        "templates_dir": "meeting_templates_dir",
        "speaker_diarization": "speaker_diarization",
        "diarization_backend": "diarization_backend",
        "diarize_mic": "diarize_mic",
        "max_speakers": "diarization_max_speakers",
        "speaker_recognition": "speaker_recognition",
    },
    "setup": {"version": "setup_version"},
    "agents": {
        "enabled": "agents_enabled",
        "hot_commands": "agent_hot_commands",
        "default_provider": "agent_default_provider",
        "default_repo": "agent_default_repo",
        "repos": "agent_repos",
        "max_concurrent": "agent_max_concurrent",
        "max_queued": "agent_max_queued",
        "timeout_seconds": "agent_timeout_seconds",
        "max_output_kb": "agent_max_output_kb",
        "max_sessions": "agent_max_sessions",
        "poll_seconds": "agent_poll_seconds",
        "allow_remote": "agent_allow_remote",
        "sandbox": "agent_sandbox",
        "claude_bin": "agent_claude_bin",
        "claude_wrapper": "agent_claude_wrapper",
        "claude_permission_mode": "agent_claude_permission_mode",
        "claude_model": "agent_claude_model",
        "claude_stream": "agent_claude_stream",
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
    llm_fast_model: str = ""
    # Name (never the value) of the endpoint-bound key snapshot written by
    # provider activation (see llm_activation); "" = legacy `llm_api_key`.
    llm_key_ref: str = ""
    cleanup_enabled: bool = True
    cleanup_level: str = "light"
    styles_enabled: bool = True
    style_work: str = "formal"
    style_email: str = "formal"
    style_personal: str = "casual"
    style_other: str = "formal"
    app_category_overrides: list[str] = field(default_factory=list)
    cleanup_timeout: int = 120
    cleanup_num_ctx: int = 8192
    # Bare pynput Key name ('alt_l' = left Option) = hold-to-talk;
    # GlobalHotKeys combo ('<cmd>+<shift>+<space>') = toggle.
    hotkey: str = "alt_l"
    paste_last_hotkey: str = (
        "<ctrl>+<cmd>+v" if sys.platform == "darwin" else "<ctrl>+<shift>+v"
    )
    dictation_enabled: bool = True
    dictation_toggle_hotkey: str = ""
    command_hotkey: str = "alt_r"
    polish_hotkey: str = "<ctrl>+<alt>+1"
    prompt_hotkey: str = "<ctrl>+<alt>+2"
    sounds_enabled: bool = True        # audio cue on record start/stop
    paste_method: str = "auto"
    spoken_symbols: bool = True        # dictated "slash"/"dash"/"underscore" -> / - _
    spoken_punctuation: bool = True
    silence_threshold_dbfs: float = -50.0
    hot_phrases: bool = True           # spoken trigger phrases expand into saved prompts
    perf_metrics: bool = True          # local timings ledger (~/.localflow/perf), numbers only
    # Registered slash-command names: after a dictated "slash", an exact spoken
    # match is snapped to the registered spelling ("slash skill part" ->
    # "/skill_part"). Names listed here plus subdirectory names of skills_dirs.
    command_names: list[str] = field(default_factory=list)
    skills_dirs: list[str] = field(
        default_factory=lambda: ["~/.claude/skills", "~/.agents/skills"]
    )
    sample_rate: int = 16000
    max_recording_seconds: int = 1200
    pause_media_while_dictating: bool = False
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
    # Must be a model Fireworks serves on its shared serverless tier.
    fireworks_model: str = "accounts/fireworks/models/kimi-k3"
    prompts_dir: str = "~/projs/prompts/meetings"
    orca_repo: str = ""                # Orca dispatch target, e.g. "id:<repoId>"; empty = not configured
    orca_agent: str = "claude"
    # Also capture what the machine plays (remote participants) and tag
    # segments "me"/"others". macOS: ScreenCaptureKit, else a loopback device.
    capture_system_audio: bool = False
    system_audio_device: str = ""      # e.g. "BlackHole 2ch"; empty = auto
    meeting_default_template: str = "general"
    meeting_templates_dir: str = "~/.localflow/meeting_templates"
    # Speaker diarization, read when a meeting starts (applies to the next one).
    speaker_diarization: bool = False
    diarization_backend: str = "sherpa-onnx"   # sherpa-onnx | mock
    diarize_mic: bool = False
    diarization_max_speakers: int = 0          # 0 = auto; at most 10
    speaker_recognition: bool = False          # ignored unless speaker_diarization
    input_device: str = ""
    setup_version: int = 0
    history_enabled: bool = True
    keep_audio_clips: int = 5
    # Headless agent launcher ([agents]); off by default, nothing loads while off.
    agents_enabled: bool = False
    agent_hot_commands: bool = False
    agent_default_provider: str = "claude"
    agent_default_repo: str = ""
    agent_repos: list[str] = field(default_factory=list)   # "alias=/absolute/path"
    agent_max_concurrent: int = 2
    agent_max_queued: int = 8
    agent_timeout_seconds: int = 1800
    agent_max_output_kb: int = 512
    agent_max_sessions: int = 200
    agent_poll_seconds: int = 10
    agent_allow_remote: bool = False
    agent_sandbox: bool = False
    agent_claude_bin: str = "claude"
    agent_claude_wrapper: str = "none"
    agent_claude_permission_mode: str = "default"
    agent_claude_model: str = ""
    agent_claude_stream: bool = False
    # The file's meetings_enabled when load forced it off, saved in its place
    # until an update sets the field. An InitVar so it is not a config field
    # (fields(), ==, repr) yet survives replace().
    _file_meetings_enabled: InitVar[bool | None] = None

    def __post_init__(self, _file_meetings_enabled: bool | None) -> None:
        self._file_meetings_enabled = _file_meetings_enabled


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
    version = data.get("config_version")
    if version is None:
        log.warning("missing config_version; assuming %d", CONFIG_VERSION)
    elif version > CONFIG_VERSION:
        raise ConfigError(
            f"config_version {version} is newer than this localflow supports "
            f"({CONFIG_VERSION})"
        )
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
            if field_name in _ENUM_VALUES and (
                not isinstance(value, str) or value not in _ENUM_VALUES[field_name]
            ):
                log.warning(
                    "invalid config value for %s.%s: %r; using default",
                    section,
                    key,
                    value,
                )
                continue
            if field_name in _FIELD_TYPES and not _valid_field_value(field_name, value):
                if field_name == "llm_key_ref":
                    config.llm_key_ref = "llm_activation_invalid"
                    log.warning("invalid LLM credential reference; activate the provider again")
                    continue
                log.warning(
                    "invalid config value for %s.%s: %r; using default",
                    section,
                    key,
                    value,
                )
                continue
            setattr(config, field_name, value)
    if config.agent_default_repo and config.agent_default_repo not in agent_repo_aliases(
            config.agent_repos):
        log.warning("agents.default_repo is not one of agents.repos; using no default")
        config.agent_default_repo = ""
    if sys.platform != "darwin":
        if isinstance(config.meetings_enabled, bool):
            config._file_meetings_enabled = config.meetings_enabled
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
            if field_name == "meetings_enabled" and config._file_meetings_enabled is not None:
                value = config._file_meetings_enabled
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
            if not (
                os.environ.get("LOCALFLOW_FIREWORKS_API_KEY")
                or os.environ.get("FIREWORKS_API_KEY")
            ):
                os.environ.setdefault("FIREWORKS_API_KEY", fireworks_key)
                log.warning(
                    "not migrating ~/.localflow.toml: fireworks_api_key could not "
                    "be stored in the keyring; set FIREWORKS_API_KEY in the "
                    "environment (or fix the keyring) and rerun `lf config migrate`"
                )
                return config
            log.warning(
                "keyring unavailable; using the existing fireworks API key "
                "environment variable"
            )
    target = config_path()
    save_config(config, target)
    log.info(
        "migrated ~/.localflow.toml to %s; the old file was left in place",
        target,
    )
    return config


def read_config_file(path: Path) -> Config:
    """Read one v2 config file; no defaults, no migration. Raises FileNotFoundError."""
    return _config_from_v2(_parse(path))


def load_config() -> Config:
    """Load v2 config, migrating the legacy flat file when needed."""
    target = config_path()
    if target.exists():
        return read_config_file(target)
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


_ENUM_VALUES = {
    "cleanup_level": {"none", "light", "medium"},
    "style_work": {"formal", "casual", "very_casual", "excited"},
    "style_email": {"formal", "casual", "very_casual", "excited"},
    "style_personal": {"formal", "casual", "very_casual", "excited"},
    "style_other": {"formal", "casual", "very_casual", "excited"},
    "diarization_backend": {"sherpa-onnx", "mock"},
    "agent_claude_wrapper": {"none", "headroom"},
    # No bypass mode is settable.
    "agent_claude_permission_mode": {"default", "acceptEdits", "plan"},
}

_FIELD_TYPES = {
    "dictation_enabled": bool,
    "dictation_toggle_hotkey": str,
    "llm_key_ref": str,
    "llm_fast_model": str,
    "paste_last_hotkey": str,
    "command_hotkey": str,
    "polish_hotkey": str,
    "prompt_hotkey": str,
    "pause_media_while_dictating": bool,
    "max_recording_seconds": int,
    "history_enabled": bool,
    "perf_metrics": bool,
    "keep_audio_clips": int,
    "capture_system_audio": bool,
    "system_audio_device": str,
    "meeting_default_template": str,
    "meeting_templates_dir": str,
    "speaker_diarization": bool,
    "diarize_mic": bool,
    "diarization_max_speakers": int,
    "speaker_recognition": bool,
    "agents_enabled": bool,
    "agent_hot_commands": bool,
    "agent_default_provider": str,
    "agent_default_repo": str,
    "agent_repos": list,
    "agent_max_concurrent": int,
    "agent_max_queued": int,
    "agent_timeout_seconds": int,
    "agent_max_output_kb": int,
    "agent_max_sessions": int,
    "agent_poll_seconds": int,
    "agent_allow_remote": bool,
    "agent_sandbox": bool,
    "agent_claude_bin": str,
    "agent_claude_model": str,
    "agent_claude_stream": bool,
}

_AGENT_RANGES = {
    "agent_max_concurrent": (1, 8),
    "agent_max_queued": (0, 64),
    "agent_timeout_seconds": (10, 86400),
    "agent_max_output_kb": (16, 8192),
    "agent_max_sessions": (10, 2000),
    "agent_poll_seconds": (2, 300),
}
_AGENT_PROVIDER_RE = re.compile(r"^[a-z0-9][a-z0-9_.-]{0,39}$")
_AGENT_ALIAS_RE = re.compile(r"^[a-z0-9][a-z0-9 _-]{0,39}$")
_AGENT_MODEL_RE = re.compile(r"^[A-Za-z0-9._:\[\]-]{0,80}$")
_AGENT_BARE_BIN_RE = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9._+-]{0,63}$")
_MAX_AGENT_REPOS = 32


def _valid_agent_repos(value: list) -> bool:
    if len(value) > _MAX_AGENT_REPOS:
        return False
    seen: set[str] = set()
    for entry in value:
        if not isinstance(entry, str) or "=" not in entry:
            return False
        alias, path = (part.strip() for part in entry.split("=", 1))
        if not _AGENT_ALIAS_RE.match(alias) or not path or any(c in path for c in "\0\n\r"):
            return False
        if not os.path.isabs(os.path.expanduser(path)):
            return False
        key = " ".join(re.findall(r"[^\W_]+", alias.lower()))
        if key in seen:
            return False
        seen.add(key)
    return True


def _valid_claude_bin(value: str) -> bool:
    if not value or any(c in value for c in "\0\n\r\t"):
        return False
    if os.path.isabs(value):
        return True
    return bool(_AGENT_BARE_BIN_RE.match(value))


def agent_repo_aliases(repos: list[str]) -> set[str]:
    return {entry.split("=", 1)[0].strip() for entry in repos if "=" in entry}


def _valid_field_value(field_name: str, value: object) -> bool:
    expected_type = _FIELD_TYPES.get(field_name)
    if expected_type is not None and type(value) is not expected_type:
        return False
    if field_name in _AGENT_RANGES:
        low, high = _AGENT_RANGES[field_name]
        return low <= value <= high
    if field_name == "agent_repos":
        return _valid_agent_repos(value)
    if field_name == "agent_default_provider":
        return bool(_AGENT_PROVIDER_RE.match(value))
    if field_name == "agent_default_repo":
        return value == "" or bool(_AGENT_ALIAS_RE.match(value))
    if field_name == "agent_claude_model":
        return bool(_AGENT_MODEL_RE.match(value))
    if field_name == "agent_claude_bin":
        return _valid_claude_bin(value)
    if field_name in {"max_recording_seconds", "keep_audio_clips"}:
        return value >= 0
    if field_name == "diarization_max_speakers":
        return 0 <= value <= 10
    if field_name == "llm_key_ref":
        # Only our own activation namespace: a config can never point the
        # LLM client at some other stored secret.
        return value == "" or llm_activation.is_ref(value)
    return True


def config_to_dict(config: Config) -> dict:
    result: dict = {"config_version": CONFIG_VERSION}
    for section, mapping in _SECTIONS.items():
        result[section] = {
            file_key: getattr(config, field_name)
            for file_key, field_name in mapping.items()
        }
    return result


def apply_config_update(config: Config, update: dict) -> Config:
    changes: dict[str, object] = {}
    for section, values in update.items():
        mapping = _SECTIONS.get(section)
        if mapping is None or not isinstance(values, dict):
            raise ConfigError(f"unknown config key '{section}'")
        for file_key, value in values.items():
            field_name = mapping.get(file_key)
            if field_name is None:
                raise ConfigError(f"unknown config key '{section}.{file_key}'")
            current = getattr(config, field_name)
            if field_name == "language":
                valid_type = value is None or isinstance(value, str)
            elif isinstance(current, bool):
                valid_type = type(value) is bool
            elif isinstance(current, int):
                valid_type = type(value) is int
            elif isinstance(current, float):
                valid_type = type(value) is float
            elif isinstance(current, list):
                valid_type = (
                    isinstance(value, list)
                    and all(isinstance(item, str) for item in value)
                )
            else:
                valid_type = isinstance(value, str)
            if not valid_type:
                raise ConfigError(
                    f"invalid type for config key '{section}.{file_key}'"
                )
            if field_name in _FIELD_TYPES and not _valid_field_value(field_name, value):
                raise ConfigError(
                    f"invalid value for config key '{section}.{file_key}'"
                )
            if section == "paste" and file_key == "method" and value not in PASTE_METHODS:
                raise ConfigError(f"invalid paste method: {value!r}")
            if field_name in _ENUM_VALUES and (
                not isinstance(value, str) or value not in _ENUM_VALUES[field_name]
            ):
                choices = ", ".join(sorted(_ENUM_VALUES[field_name]))
                raise ConfigError(
                    f"invalid value for config key '{section}.{file_key}'; "
                    f"choose from: {choices}"
                )
            if field_name == "llm_key_ref" and value not in ("", current):
                raise ConfigError(
                    "llm.api_key_ref is set by activating a provider in Models & "
                    "keys; it can only be cleared here"
                )
            changes[field_name] = value
    # A hand-picked endpoint (`lf llm use`, the web settings) is not the one a
    # snapshot was activated for: drop the binding so the legacy llm_api_key
    # flow applies, exactly as before activation snapshots existed.
    endpoint_changed = (
        changes.get("llm_provider", config.llm_provider) != config.llm_provider
        or llm_activation.endpoint(changes.get("llm_base_url", config.llm_base_url))
        != llm_activation.endpoint(config.llm_base_url)
    )
    default_repo = changes.get("agent_default_repo", config.agent_default_repo)
    if default_repo and default_repo not in agent_repo_aliases(
            changes.get("agent_repos", config.agent_repos)):
        raise ConfigError("agents.default_repo must be one of the aliases in agents.repos")
    if endpoint_changed:
        changes["llm_key_ref"] = ""
        changes.setdefault("llm_fast_model", "")
    if "meetings_enabled" in changes:
        changes["_file_meetings_enabled"] = None
    return replace(config, **changes)
