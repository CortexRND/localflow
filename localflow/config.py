import tomllib
from dataclasses import dataclass, fields
from pathlib import Path


@dataclass
class Config:
    model_size: str = "base"           # whisper model size
    stt_backend: str = "auto"          # auto | mlx | faster-whisper
    language: str | None = None        # None = autodetect
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


def load_config() -> Config:
    """Read ~/.localflow.toml (tomllib) if present, override defaults; missing file OK."""
    config = Config()
    path = Path.home() / ".localflow.toml"
    if not path.exists():
        return config

    with path.open("rb") as f:
        data = tomllib.load(f)

    known_keys = {f.name for f in fields(Config)}
    for key, value in data.items():
        if key in known_keys:
            setattr(config, key, value)

    return config
