from dataclasses import asdict, replace

from localflow import llm_activation
from localflow.config import Config, ConfigError, config_path
from localflow.provider_keys import PROVIDER_SECRET_NAMES
from localflow.providers import registry
from localflow.providers.factory import build_llm, llm_api_key
from localflow.secrets import (
    SecretsUnavailable,
    delete_secret,
    get_secret,
    set_secret,
)

SECRET_NAMES = ("llm_api_key", "fireworks_api_key", *PROVIDER_SECRET_NAMES)
_STT_ALIASES = {"auto": "faster-whisper", "mlx": "mlx-whisper"}


def resolve_stt_provider(provider: str) -> str:
    return _STT_ALIASES.get(provider, provider)


def validate_runtime(config: Config):
    stt_provider = resolve_stt_provider(config.stt_backend)
    if stt_provider not in registry.list_stt_ids():
        raise ConfigError(f"unknown stt provider {config.stt_backend!r}")
    if config.llm_provider not in registry.list_llm_ids():
        raise ConfigError(f"unknown llm provider {config.llm_provider!r}")
    try:
        return build_llm(config)
    except Exception as exc:
        raise ConfigError(str(exc)) from exc


def restart_required(update: dict) -> list[str]:
    restart_sections = {"meetings", "server", "hotkey", "features"}
    required: list[str] = []
    for section, values in update.items():
        if not isinstance(values, dict):
            continue
        for key in values:
            if section in restart_sections or (
                section == "dictation"
                and key in {"sample_rate", "max_recording_seconds", "pause_media"}
            ):
                required.append(f"{section}.{key}")
    return required


def status(config: Config, platform: object, loaded: bool) -> dict:
    try:
        autostart = platform.autostart_status()
    except (RuntimeError, OSError):
        autostart = "unavailable"
    return {
        "platform": platform.name,
        "config_path": str(config_path()),
        "stt": {
            "provider": config.stt_backend,
            "model": config.model_size,
            "device": config.stt_device,
            "language": config.language,
            "loaded": loaded,
        },
        "llm": {
            "provider": config.llm_provider,
            "base_url": config.llm_base_url,
            "model": config.llm_model,
            "cleanup_enabled": config.cleanup_enabled,
        },
        "hotkey": config.hotkey,
        "paste_method": config.paste_method,
        "sounds_enabled": config.sounds_enabled,
        "dictation_enabled": config.dictation_enabled,
        "server": {"host": config.server_host, "port": config.server_port},
        "meetings_enabled": config.meetings_enabled,
        "autostart": autostart,
    }


def stt_providers() -> list[dict]:
    return [
        {
            "id": provider_id,
            "offline": registry.stt_provider_class(provider_id).offline,
            "available": registry.stt_available(provider_id),
        }
        for provider_id in registry.list_stt_ids()
    ]


def llm_providers() -> list[dict]:
    return [
        {
            "id": provider_id,
            "offline": registry.llm_provider_class(provider_id).offline,
        }
        for provider_id in registry.list_llm_ids()
    ]


_UNSAVED_URL_HINT = (
    " (the stored API key is only sent to the saved base URL; save it first)"
)


def _is_saved_url(config: Config, base_url: str | None) -> bool:
    """The stored llm_api_key may only go to the URL the user saved."""
    if not base_url:
        return True
    return base_url.strip().rstrip("/") == (config.llm_base_url or "").strip().rstrip("/")


def models(
    config: Config, kind: str, provider: str, base_url: str | None = None
) -> dict:
    with_key = _is_saved_url(config, base_url)
    try:
        if kind == "stt":
            values = registry.stt_provider_class(
                resolve_stt_provider(provider)
            )().list_models()
        elif kind == "llm":
            provider_config = replace(
                config,
                llm_provider=provider,
                llm_base_url=base_url or config.llm_base_url,
            )
            values = build_llm(
                provider_config, timeout=10, with_api_key=with_key
            ).list_models()
        else:
            raise ValueError(f"unknown model kind: {kind!r}")
        return {"models": [asdict(value) for value in values], "error": None}
    except Exception as exc:  # noqa: BLE001
        hint = "" if with_key or kind != "llm" else _UNSAVED_URL_HINT
        return {"models": [], "error": f"{exc}{hint}"}


def test_llm(
    config: Config,
    provider: str | None = None,
    base_url: str | None = None,
    model: str | None = None,
) -> dict:
    with_key = _is_saved_url(config, base_url)
    try:
        provider_config = replace(
            config,
            llm_provider=provider or config.llm_provider,
            llm_base_url=base_url or config.llm_base_url,
            llm_model=model or config.llm_model,
        )
        result = build_llm(
            provider_config, timeout=10, with_api_key=with_key
        ).healthcheck()
        detail = result.detail
        if not result.ok and not with_key:
            detail = f"{detail or ''}{_UNSAVED_URL_HINT}"
        return {"ok": result.ok, "detail": detail}
    except Exception as exc:  # noqa: BLE001
        hint = "" if with_key else _UNSAVED_URL_HINT
        return {"ok": False, "detail": f"{exc}{hint}"}


class InvalidKeyRef(ValueError):
    """The active config names an LLM key outside the activation namespace."""


def _bound_ref(name: str, config: Config | None) -> str:
    """The activation slot `llm_api_key` means for `config`, or "" (legacy).

    Only the LLM key is ever redirected, and only into our own namespace: a
    bad `llm_key_ref` is refused before anything is written, so it can never
    make the web overwrite or delete some other stored secret.
    """
    if name != "llm_api_key" or config is None:
        return ""
    ref = getattr(config, "llm_key_ref", "")
    if not ref:
        return ""
    if not llm_activation.is_ref(ref):
        raise InvalidKeyRef(
            "The active LLM key reference is invalid; activate the provider "
            "again in Models & keys"
        )
    return ref


def secrets_status(config: Config | None = None) -> dict[str, bool]:
    """Which secrets are set. With `config`, `llm_api_key` reports the key its
    LLM client really sends (its activation slot when bound), never an unused
    generic key."""
    status = {name: bool(get_secret(name)) for name in SECRET_NAMES}
    if config is not None and getattr(config, "llm_key_ref", ""):
        status["llm_api_key"] = bool(llm_api_key(config))
    return status


def update_secret(name: str, value: str, config: Config | None = None) -> None:
    """Set (or clear, with "") a secret.

    With an activated `config`, `llm_api_key` rotates that config's own
    endpoint-bound activation slot in place, for the same endpoint, and
    leaves the generic `llm_api_key` to legacy configs (a config cached
    before activation still pairs it with its own endpoint). No config is
    written and no new slot name is created; a slot recreated after Remove
    is listed in the activation index so removing the key revokes it again.
    Clearing deletes the slot (never touching the index), so the client
    sends no key rather than falling back to the generic one.
    """
    if name not in SECRET_NAMES:
        raise KeyError(name)
    ref = _bound_ref(name, config)
    target = ref or name
    if value:
        if ref:
            # A slot recreated after Remove must stay revocable by name: list
            # it first (names only), so a failed write leaves at most a
            # harmless index entry, never an unlisted credential.
            index = llm_activation.read_index(get_secret)
            if ref not in index:
                set_secret(
                    llm_activation.INDEX_NAME,
                    llm_activation.encode_index([*index, ref]),
                )
        stored = llm_activation.encode(config.llm_base_url, value) if ref else value
        set_secret(target, stored)
    else:
        try:
            delete_secret(target)
        except SecretsUnavailable:
            if get_secret(target):
                raise
