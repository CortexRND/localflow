from dataclasses import asdict, replace

from localflow.config import Config, ConfigError, config_path
from localflow.providers import registry
from localflow.providers.factory import build_llm
from localflow.secrets import (
    SecretsUnavailable,
    delete_secret,
    get_secret,
    set_secret,
)

SECRET_NAMES = ("llm_api_key", "fireworks_api_key")
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
                section == "dictation" and key == "sample_rate"
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


def models(
    config: Config, kind: str, provider: str, base_url: str | None = None
) -> dict:
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
            values = build_llm(provider_config, timeout=10).list_models()
        else:
            raise ValueError(f"unknown model kind: {kind!r}")
        return {"models": [asdict(value) for value in values], "error": None}
    except Exception as exc:  # noqa: BLE001
        return {"models": [], "error": str(exc)}


def test_llm(
    config: Config,
    provider: str | None = None,
    base_url: str | None = None,
    model: str | None = None,
) -> dict:
    try:
        provider_config = replace(
            config,
            llm_provider=provider or config.llm_provider,
            llm_base_url=base_url or config.llm_base_url,
            llm_model=model or config.llm_model,
        )
        result = build_llm(provider_config, timeout=10).healthcheck()
        return {"ok": result.ok, "detail": result.detail}
    except Exception as exc:  # noqa: BLE001
        return {"ok": False, "detail": str(exc)}


def secrets_status() -> dict[str, bool]:
    return {name: bool(get_secret(name)) for name in SECRET_NAMES}


def update_secret(name: str, value: str) -> None:
    if name not in SECRET_NAMES:
        raise KeyError(name)
    if value:
        set_secret(name, value)
    else:
        try:
            delete_secret(name)
        except SecretsUnavailable:
            if get_secret(name):
                raise
