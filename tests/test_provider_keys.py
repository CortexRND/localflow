from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import api
from localflow import provider_keys as keys
from localflow.config import Config, load_config
from localflow.providers.base import Health, ModelInfo


@pytest.fixture
def vault(monkeypatch, tmp_path):
    store = {}
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setattr(keys.secrets, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(
        keys.secrets, "set_secret", lambda name, value: store.__setitem__(name, value)
    )
    monkeypatch.setattr(
        keys.secrets, "delete_secret", lambda name: store.pop(name, None)
    )
    return store


def test_presets():
    assert {p.id for p in keys.PRESETS} == {
        "ollama",
        "fireworks",
        "openai",
        "openrouter",
        "groq",
        "together",
        "custom",
    }
    assert not keys.get_preset("ollama").needs_key
    assert (
        keys.get_preset("fireworks").base_url == "https://api.fireworks.ai/inference/v1"
    )
    assert set(keys.PROVIDER_SECRET_NAMES) <= set(api.SECRET_NAMES)
    for p in keys.PRESETS[1:]:
        assert p.needs_key and p.env_vars and p.llm_provider == "openai-compatible"
    with pytest.raises(ValueError):
        keys.get_preset("missing")


@pytest.mark.parametrize(
    "value,masked",
    [
        ("", ""),
        ("a", "••••"),
        ("abcd", "••••"),
        ("secretabcd", "••••abcd"),
    ],
)
def test_mask(value, masked):
    assert keys.mask_key(value) == masked


def test_parse_env():
    assert keys.parse_env("""
# comment
export FIREWORKS_API_KEY="fire works" # comment
OPENAI_API_KEY='open#ai'
GROQ_API_KEY=groq # comment
LLM_API_KEY=$OTHER_KEY
OPENROUTER_API_KEY="${OTHER_KEY}"
TOGETHER_API_KEY=$(command)
BAD='unterminated
echo ignored
EMPTY=
""") == {
        "FIREWORKS_API_KEY": "fire works",
        "OPENAI_API_KEY": "open#ai",
        "GROQ_API_KEY": "groq",
    }
    assert keys.parse_env("") == {}


def test_discovery_and_confirmed_subset(vault, tmp_path, monkeypatch):
    path = tmp_path / ".zshrc"
    path.write_text("export OPENAI_API_KEY='file-secret-1234'\nGROQ_API_KEY=$OTHER\n")
    found = keys.detect_imports(path, environ={"FIREWORKS_API_KEY": "env-secret-5678"})
    assert [(c.provider_id, c.masked_value) for c in found] == [
        ("fireworks", "••••5678"),
        ("openai", "••••1234"),
    ]
    assert "env-secret" not in repr(found)
    assert found[0].source == "environment: FIREWORKS_API_KEY"
    assert str(path) in found[1].source
    assert vault == {}
    keys.save_imports(found[1:])
    assert vault == {"provider_key_openai": "file-secret-1234"}
    keys.save_imports([])
    assert keys.detect_imports(environ={}) == []
    monkeypatch.setenv("OPENAI_API_KEY", "from-environment")
    assert any(c.provider_id == "openai" for c in keys.detect_imports())
    with pytest.raises(FileNotFoundError):
        keys.detect_imports(tmp_path / "missing", environ={})


def test_storage_status_removal_and_fireworks_alias(vault):
    assert keys.key_status("fireworks") == {"has_key": False, "masked_value": ""}
    keys.save_key("fireworks", "secret-abcd")
    assert vault == {
        "provider_key_fireworks": "secret-abcd",
        "fireworks_api_key": "secret-abcd",
    }
    assert keys.key_status("fireworks") == {"has_key": True, "masked_value": "••••abcd"}
    keys.remove_key("fireworks")
    assert not vault
    keys.remove_key("ollama")
    assert keys.key_status("ollama")["has_key"] is False
    with pytest.raises(ValueError):
        keys.save_key("openai", " ")
    with pytest.raises(ValueError):
        keys.save_key("ollama", "secret")


def test_secrets_failures_propagate(vault, monkeypatch):
    def fail(*args):
        raise keys.secrets.SecretsUnavailable("unavailable")

    monkeypatch.setattr(keys.secrets, "set_secret", fail)
    with pytest.raises(keys.secrets.SecretsUnavailable):
        keys.save_key("openai", "secret")
    vault["provider_key_openai"] = "secret"
    with pytest.raises(keys.secrets.SecretsUnavailable):
        keys.activate("openai", config=Config())


def test_activation_never_writes_keys_to_config(vault, tmp_path):
    key = "private-key-not-in-config-1234"
    keys.save_key("fireworks", key)
    config = keys.activate("fireworks", model="chosen-model", config=Config())
    assert config.llm_provider == "openai-compatible"
    assert config.llm_base_url == keys.get_preset("fireworks").base_url
    assert config.llm_model == "chosen-model"
    assert vault["llm_api_key"] == key
    assert load_config().llm_model == "chosen-model"
    for path in tmp_path.rglob("*"):
        if path.is_file():
            assert key not in path.read_text()
    local = keys.activate("ollama", config=config)
    assert local.llm_provider == "ollama"
    assert local.llm_base_url == "http://localhost:11434"


def test_custom_activation_and_validation(vault):
    with pytest.raises(ValueError):
        keys.activate("openai", config=Config())
    keys.save_key("custom", "secret-custom")
    for url in (
        "",
        "file:///key",
        "https://user:secret@example.org",
        "https://host/?key=secret",
    ):
        with pytest.raises(ValueError):
            keys.activate("custom", base_url=url, config=Config())
    config = keys.activate(
        "custom", base_url="https://inference.example/v1/", config=Config()
    )
    assert config.llm_base_url == "https://inference.example/v1"
    with pytest.raises(ValueError):
        keys.activate("openai", base_url="https://other.example", config=Config())


def test_models_and_health_use_provider_key_not_active(vault, monkeypatch):
    vault.update(provider_key_openai="openai-secret", llm_api_key="another-provider")
    provider = SimpleNamespace(
        list_models=Mock(return_value=[ModelInfo("model-one")]),
        healthcheck=Mock(return_value=Health(True)),
    )
    constructor = Mock(return_value=provider)
    monkeypatch.setattr(keys, "OpenAICompatLLM", constructor)
    assert keys.fetch_models("openai") == [
        {"id": "model-one", "label": "", "installed": None}
    ]
    assert constructor.call_args.kwargs["api_key"] == "openai-secret"
    assert keys.test_llm("openai")[0] is True
    provider.healthcheck.return_value = Health(False, "may contain secret")
    assert keys.test_llm("openai") == (
        False,
        "Provider connection failed; check the key, base URL, and network",
    )
    provider.list_models.side_effect = RuntimeError("secret")
    assert keys.fetch_models("openai") == []
    provider.list_models.side_effect = None
    provider.list_models.return_value = []
    assert keys.fetch_models("openai") == []
    vault.clear()
    assert keys.fetch_models("openai") == []
    assert keys.test_llm("openai")[0] is False
    monkeypatch.setattr(keys, "OllamaLLM", constructor)
    assert keys.test_llm("ollama")[0] is False
    assert "api_key" not in constructor.call_args.kwargs
