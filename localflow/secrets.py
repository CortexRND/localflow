import logging
import os

import keyring

log = logging.getLogger("localflow.secrets")

SERVICE = "localflow"
_LEGACY_ENV = {"fireworks_api_key": "FIREWORKS_API_KEY"}
_keyring_warned = False


class SecretsUnavailable(RuntimeError):
    """Raised when the configured keyring cannot store a secret."""


def _warn_keyring_once(exc: Exception) -> None:
    global _keyring_warned
    if not _keyring_warned:
        log.warning("keyring unavailable: %s", exc)
        _keyring_warned = True


def get_secret(name: str) -> str:
    env_name = f"LOCALFLOW_{name.upper()}"
    if env_name in os.environ:
        return os.environ[env_name]
    legacy_name = _LEGACY_ENV.get(name)
    if legacy_name and legacy_name in os.environ:
        return os.environ[legacy_name]
    try:
        return keyring.get_password(SERVICE, name) or ""
    except Exception as exc:
        _warn_keyring_once(exc)
        return ""


def set_secret(name: str, value: str) -> None:
    try:
        keyring.set_password(SERVICE, name, value)
    except Exception as exc:
        _warn_keyring_once(exc)
        raise SecretsUnavailable(str(exc)) from exc


def delete_secret(name: str) -> None:
    try:
        keyring.delete_password(SERVICE, name)
    except Exception as exc:
        _warn_keyring_once(exc)
        raise SecretsUnavailable(str(exc)) from exc
