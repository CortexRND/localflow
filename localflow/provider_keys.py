"""Provider key discovery and Keychain-backed setup, independent of any UI."""

import os
import re
import shlex
from collections.abc import Iterable, Mapping
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from urllib.parse import urlsplit

from localflow import secrets
from localflow.config import Config, load_config, save_config
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM


@dataclass(frozen=True)
class Preset:
    id: str
    label: str
    base_url: str
    env_vars: tuple[str, ...]
    llm_provider: str = "openai-compatible"
    needs_key: bool = True
    default_model: str = ""


PRESETS = (
    Preset(
        "ollama",
        "Local (Ollama)",
        "http://localhost:11434",
        (),
        "ollama",
        False,
        "llama3.2:3b",
    ),
    Preset(
        "fireworks",
        "Fireworks",
        "https://api.fireworks.ai/inference/v1",
        ("FIREWORKS_API_KEY",),
    ),
    Preset("openai", "OpenAI", "https://api.openai.com/v1", ("OPENAI_API_KEY",)),
    Preset(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        ("OPENROUTER_API_KEY",),
    ),
    Preset("groq", "Groq", "https://api.groq.com/openai/v1", ("GROQ_API_KEY",)),
    Preset(
        "together", "Together", "https://api.together.xyz/v1", ("TOGETHER_API_KEY",)
    ),
    Preset(
        "custom",
        "Custom OpenAI-compatible",
        "",
        ("OPENAI_COMPAT_API_KEY", "LLM_API_KEY"),
    ),
)
PROVIDER_SECRET_NAMES = tuple(f"provider_key_{p.id}" for p in PRESETS if p.needs_key)


def get_preset(provider_id: str) -> Preset:
    for preset in PRESETS:
        if preset.id == provider_id:
            return preset
    raise ValueError("Unknown inference provider")


def mask_key(value: str) -> str:
    return "••••" + (value[-4:] if len(value) > 4 else "") if value else ""


@dataclass(frozen=True)
class ImportCandidate:
    provider_id: str
    source: str
    _value: str = field(repr=False)

    @property
    def masked_value(self) -> str:
        return mask_key(self._value)


def parse_env(text: str) -> dict[str, str]:
    """Read literal assignments only; never execute shell code or expand variables."""
    values = {}
    for line in text.splitlines():
        match = re.match(
            r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$", line
        )
        if not match:
            continue
        name, raw = match.groups()
        try:
            lexer = shlex.shlex(raw, posix=True)
            lexer.whitespace_split = True
            parts = list(lexer)
        except ValueError:
            continue
        if len(parts) == 1 and parts[0] and not any(c in parts[0] for c in "$`;"):
            values[name] = parts[0]
    return values


def detect_imports(
    file: str | Path | None = None, *, environ: Mapping[str, str] | None = None
) -> list[ImportCandidate]:
    sources = [("environment", os.environ if environ is None else environ)]
    if file is not None:
        path = Path(file).expanduser()
        sources.append((str(path), parse_env(path.read_text(encoding="utf-8"))))
    return [
        ImportCandidate(p.id, f"{source}: {name}", values[name].strip())
        for source, values in sources
        for p in PRESETS
        for name in p.env_vars
        if values.get(name, "").strip() and "$" not in values[name]
    ]


def save_key(provider_id: str, value: str) -> None:
    preset = get_preset(provider_id)
    if not preset.needs_key or not value.strip():
        raise ValueError("A non-empty key and a keyed provider are required")
    secrets.set_secret(f"provider_key_{preset.id}", value.strip())
    if preset.id == "fireworks":
        secrets.set_secret("fireworks_api_key", value.strip())


def save_imports(confirmed: Iterable[ImportCandidate]) -> None:
    for candidate in confirmed:
        save_key(candidate.provider_id, candidate._value)


def remove_key(provider_id: str) -> None:
    preset = get_preset(provider_id)
    if not preset.needs_key:
        return
    secrets.delete_secret(f"provider_key_{preset.id}")
    if preset.id == "fireworks":
        secrets.delete_secret("fireworks_api_key")


def key_needs_activation(provider_id: str) -> bool:
    """True when a saved key for this provider differs from the generic key
    Ask and cleanup actually use (`llm_api_key`), i.e. Save/Import happened
    after the last activation. Boolean only: the values stay in locals and are
    never returned, stored or logged. A missing saved key is never offered."""
    preset = get_preset(provider_id)
    if not preset.needs_key:
        return False
    saved = secrets.get_secret(f"provider_key_{preset.id}")
    if not saved:
        return False
    return saved != secrets.get_secret("llm_api_key")


def key_status(provider_id: str) -> dict:
    preset = get_preset(provider_id)
    value = secrets.get_secret(f"provider_key_{preset.id}") if preset.needs_key else ""
    return {"has_key": bool(value), "masked_value": mask_key(value)}


def _url(preset: Preset, base_url: str | None) -> str:
    url = (base_url if base_url is not None else preset.base_url).strip().rstrip("/")
    parsed = urlsplit(url)
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.hostname
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
    ):
        raise ValueError(
            "Use an HTTP(S) base URL without credentials, query, or fragment"
        )
    if preset.id != "custom" and url != preset.base_url:
        raise ValueError("Use the custom preset for a different base URL")
    return url


def activate(
    provider_id: str,
    *,
    base_url: str | None = None,
    model: str | None = None,
    config: Config | None = None,
) -> Config:
    preset = get_preset(provider_id)
    url = _url(preset, base_url)
    key = secrets.get_secret(f"provider_key_{preset.id}") if preset.needs_key else ""
    if preset.needs_key and not key:
        raise ValueError("Save a provider key before activating it")
    updated = replace(
        config or load_config(), llm_provider=preset.llm_provider, llm_base_url=url
    )
    if model is not None:
        updated.llm_model = model
    if preset.needs_key:
        secrets.set_secret("llm_api_key", key)
    save_config(updated)
    return updated


def _provider(provider_id: str, base_url: str | None, model: str):
    preset = get_preset(provider_id)
    url = _url(preset, base_url)
    if not preset.needs_key:
        return OllamaLLM(url, model, timeout=10)
    key = secrets.get_secret(f"provider_key_{preset.id}")
    if not key:
        raise ValueError("Save a provider key first")
    return OpenAICompatLLM(url, model, api_key=key, timeout=10)


def fetch_models(provider_id: str, *, base_url: str | None = None) -> list[dict]:
    try:
        return [asdict(m) for m in _provider(provider_id, base_url, "").list_models()]
    except Exception:  # noqa: BLE001
        return []


def test_llm(
    provider_id: str, *, base_url: str | None = None, model: str = ""
) -> tuple[bool, str]:
    try:
        result = _provider(provider_id, base_url, model).healthcheck()
        return (
            (True, "Provider connection successful")
            if result.ok
            else (
                False,
                "Provider connection failed; check the key, base URL, and network",
            )
        )
    except Exception:  # noqa: BLE001
        return False, "Unable to test provider; check the saved key and base URL"
