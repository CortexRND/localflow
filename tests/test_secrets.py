import logging

import pytest

from localflow import secrets
from localflow.secrets import SecretsUnavailable


class MemoryKeyring:
    def __init__(self, values=None):
        self.values = dict(values or {})

    def get_password(self, service, name):
        return self.values.get((service, name))

    def set_password(self, service, name, value):
        self.values[(service, name)] = value

    def delete_password(self, service, name):
        self.values.pop((service, name), None)


class RaisingKeyring:
    def get_password(self, service, name):
        raise RuntimeError("no backend")

    def set_password(self, service, name, value):
        raise RuntimeError("no backend")

    def delete_password(self, service, name):
        raise RuntimeError("no backend")


def test_secret_environment_precedence(monkeypatch):
    monkeypatch.setattr(secrets, "keyring", MemoryKeyring({("localflow", "llm_api_key"): "keyring"}))
    monkeypatch.setenv("FIREWORKS_API_KEY", "legacy")
    monkeypatch.setenv("LOCALFLOW_FIREWORKS_API_KEY", "local")

    assert secrets.get_secret("fireworks_api_key") == "local"
    monkeypatch.delenv("LOCALFLOW_FIREWORKS_API_KEY")
    assert secrets.get_secret("fireworks_api_key") == "legacy"


def test_secret_keyring_fallback(monkeypatch):
    monkeypatch.delenv("LOCALFLOW_LLM_API_KEY", raising=False)
    monkeypatch.delenv("FIREWORKS_API_KEY", raising=False)
    monkeypatch.setattr(
        secrets,
        "keyring",
        MemoryKeyring({("localflow", "llm_api_key"): "keyring"}),
    )

    assert secrets.get_secret("llm_api_key") == "keyring"


def test_secret_unavailable(monkeypatch, caplog):
    monkeypatch.setattr(secrets, "keyring", RaisingKeyring())
    secrets._keyring_warned = False

    with caplog.at_level(logging.WARNING):
        assert secrets.get_secret("llm_api_key") == ""
    with pytest.raises(SecretsUnavailable):
        secrets.set_secret("llm_api_key", "value")
    with pytest.raises(SecretsUnavailable):
        secrets.delete_secret("llm_api_key")
    assert caplog.text.count("keyring unavailable") == 1
