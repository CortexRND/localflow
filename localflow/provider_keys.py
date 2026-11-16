"""Provider key discovery and Keychain-backed setup, independent of any UI.

Activation never rewrites a key some running reader may already use: it
writes a new endpoint-bound snapshot (see `llm_activation`) and then saves
the config that points at it in one atomic replace. A failed save deletes
only that new snapshot. The generic `llm_api_key` is left to legacy configs.
Activation never modifies an existing snapshot; only an explicit web Settings
key rotation rewrites the active one, for its same bound endpoint.
"""

import logging
import os
import re
import shlex
from collections.abc import Iterable, Mapping
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from urllib.parse import urlsplit

from localflow import llm_activation, secrets
from localflow.config import Config, load_config, save_config
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM

log = logging.getLogger("localflow.provider_keys")


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


def _write_index(refs: list[str]) -> None:
    if refs:
        secrets.set_secret(llm_activation.INDEX_NAME, llm_activation.encode_index(refs))
    elif secrets.get_secret(llm_activation.INDEX_NAME):
        secrets.delete_secret(llm_activation.INDEX_NAME)


def _revoke_activations(provider_id: str, config: Config | None) -> None:
    """Delete every activation snapshot taken from `provider_id`'s key, so a
    client built from any config (current or an older cached one) loses it."""
    index = llm_activation.read_index(secrets.get_secret)
    revoke = [ref for ref in index if llm_activation.ref_provider(ref) == provider_id]
    current = config.llm_key_ref if config is not None else ""
    if llm_activation.ref_provider(current) == provider_id and current not in revoke:
        revoke.append(current)
    for ref in revoke:
        if secrets.get_secret(ref):
            secrets.delete_secret(ref)
    remaining = [ref for ref in index if ref not in revoke]
    if remaining != index:
        _write_index(remaining)


def remove_key(provider_id: str, *, config: Config | None = None) -> None:
    """Remove the saved key and revoke its activations (the active key too).

    Snapshots are found through the activation index; `config`, when given,
    is also consulted in case its snapshot is missing from the index.
    """
    preset = get_preset(provider_id)
    if not preset.needs_key:
        return
    # Revoke first: if the Keychain refuses, the saved key is still there to
    # retry with, and the active key is never left behind silently.
    _revoke_activations(preset.id, config)
    secrets.delete_secret(f"provider_key_{preset.id}")
    if preset.id == "fireworks":
        secrets.delete_secret("fireworks_api_key")


def _active_key(config: Config) -> str:
    """The key the active config really sends (same rule as build_llm)."""
    if config.llm_key_ref:
        return llm_activation.resolve(
            config.llm_key_ref, config.llm_base_url, secrets.get_secret,
        )
    return secrets.get_secret("llm_api_key")


def key_needs_activation(provider_id: str, config: Config | None = None) -> bool:
    """True when a saved key for this provider differs from the key Ask and
    cleanup actually use for the active `config` (its activation snapshot, or
    the legacy `llm_api_key`), i.e. Save/Import happened after the last
    activation. Boolean only: the values stay in locals and are never
    returned, stored or logged. A missing saved key is never offered."""
    preset = get_preset(provider_id)
    if not preset.needs_key:
        return False
    saved = secrets.get_secret(f"provider_key_{preset.id}")
    if not saved:
        return False
    return saved != _active_key(config if config is not None else load_config())


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
    """Make `provider_id` the one Ask and cleanup use; atomic for readers.

    Keyed providers: (1) record a fresh snapshot name in the activation
    index, (2) write the immutable snapshot {endpoint, key}, (3) atomically
    save the config naming it. Until (3) every reader still sees the old
    config with its old key; after it, the new pair. Nothing shared is
    overwritten, so a config cached before activation stays correctly
    paired. On failure only what this call added is removed again.
    """
    preset = get_preset(provider_id)
    url = _url(preset, base_url)
    base = config or load_config()
    # Decided before any key or config write: never persist an unusable model.
    chosen = _choose_model(preset, model, base.llm_model)
    key = secrets.get_secret(f"provider_key_{preset.id}") if preset.needs_key else ""
    if preset.needs_key and not key:
        raise ValueError("Save a provider key before activating it")
    updated = replace(
        base, llm_provider=preset.llm_provider, llm_base_url=url, llm_key_ref="",
        llm_model=chosen,
    )
    if not preset.needs_key:
        save_config(updated)  # keyless (Ollama): nothing to pair
        return updated
    reused = _reusable_snapshot(base, preset.id, url, key)
    if reused:
        updated.llm_key_ref = reused
        save_config(updated)  # the existing snapshot is untouched either way
        return updated
    index = llm_activation.read_index(secrets.get_secret)
    ref = _unused_ref(preset.id)
    _write_index([*index, ref])  # raises before anything a reader could see
    try:
        secrets.set_secret(ref, llm_activation.encode(url, key))
        updated.llm_key_ref = ref
        save_config(updated)
    except BaseException:
        _discard_snapshot(ref, index)
        raise
    return updated


def _choose_model(preset: Preset, model: str | None, current: str) -> str:
    """The stripped model to activate.

    An explicit `model` wins; `None` keeps the configured model. Either one
    blank falls back to the preset's own default (only Ollama defines one);
    with no default the caller must choose, so nothing is written.
    """
    picked = model if model is not None else current
    picked = picked.strip() if isinstance(picked, str) else ""
    if not picked:
        picked = preset.default_model.strip()
    if not picked:
        raise ValueError("Choose a model before using this provider")
    return picked


def _reusable_snapshot(config: Config, provider_id: str, url: str, key: str) -> str:
    """Re-activating the same key for the same endpoint reuses its snapshot."""
    ref = config.llm_key_ref
    if llm_activation.ref_provider(ref) != provider_id:
        return ""
    if llm_activation.decode(secrets.get_secret(ref)) != (llm_activation.endpoint(url), key):
        return ""
    return ref


def _unused_ref(provider_id: str) -> str:
    for _attempt in range(3):
        ref = llm_activation.new_ref(provider_id)
        if not secrets.get_secret(ref):
            return ref
    raise RuntimeError("Could not allocate a provider activation")


def _discard_snapshot(ref: str, index: list[str]) -> None:
    """Undo a failed activation: delete only the snapshot it created.

    Never raises (the original error is what the caller reports) and never
    logs a key. If the delete fails, the unreferenced snapshot stays listed
    in the index so removing the provider key still revokes it later.
    """
    try:
        if secrets.get_secret(ref):
            secrets.delete_secret(ref)
    except Exception:  # noqa: BLE001
        log.warning(
            "activation failed and its unused key snapshot could not be removed; "
            "no config refers to it, and removing the provider key deletes it"
        )
        return
    try:
        _write_index(index)
    except Exception:  # noqa: BLE001
        log.warning("could not tidy the provider activation index")


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
