"""Ask follows the provider/model/key chosen on Models & keys (no AppKit).

Uses the real provider_keys.activate, the real config round-trip shape and the
real build_llm; only the secret store, config file and provider HTTP clients are
faked. Production Ask passes llm=None/config=None, so every question reloads
config and builds a fresh client; these tests pin that contract.
"""

from datetime import datetime, timedelta
from pathlib import Path
from typing import ClassVar

import pytest

from localflow import ask_model as ask
from localflow import provider_keys as keys
from localflow import secrets
from localflow.config import Config
from localflow.providers import factory
from localflow.recall import Passage

ROOT = Path(__file__).parent.parent


class FakeClient:
    instances: ClassVar[list] = []

    def __init__(self, base_url, model, api_key="", **kwargs):
        self.base_url, self.model, self.api_key = base_url, model, api_key
        self.questions = []
        FakeClient.instances.append(self)

    def complete(self, system, user, **kwargs):
        self.questions.append(user)
        return f"Answer from {self.model} [1]."


@pytest.fixture
def env(monkeypatch):
    FakeClient.instances = []
    store, saved = {}, {"config": Config()}
    monkeypatch.setattr(secrets, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(secrets, "set_secret", store.__setitem__)
    monkeypatch.setattr(secrets, "delete_secret", lambda name: store.pop(name, None))
    monkeypatch.setattr(factory, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(keys, "load_config", lambda: saved["config"])
    monkeypatch.setattr(keys, "save_config", lambda cfg: saved.update(config=cfg))
    monkeypatch.setattr(ask, "load_config", lambda: saved["config"])
    monkeypatch.setattr(factory, "OpenAICompatLLM", FakeClient)
    monkeypatch.setattr(factory, "OllamaLLM", FakeClient)
    when = (datetime.now() - timedelta(days=1)).timestamp()
    monkeypatch.setattr(
        ask.recall, "build_corpus",
        lambda **kwargs: [Passage("Launch Friday", "notes", "Plan", when, "/vault/n.md")],
    )
    return store, saved


def activate(saved, provider, model, key=None, url=None):
    if key is not None:
        keys.save_key(provider, key)
    saved["config"] = keys.activate(provider, base_url=url, model=model)


def test_ask_uses_new_provider_model_url_and_key_per_question(env):
    _store, saved = env
    chat = ask.AskModel(store=object())  # production shape: no llm, no config
    activate(saved, "fireworks", "model-a", key="key-a")
    first = chat.ask("When is launch?")
    activate(saved, "openrouter", "model-b", key="key-b")
    second = chat.ask("And the launch checklist?")

    a, b = FakeClient.instances
    assert a is not b
    assert (a.base_url, a.model, a.api_key) == (
        keys.get_preset("fireworks").base_url, "model-a", "key-a",
    )
    assert (b.base_url, b.model, b.api_key) == (
        keys.get_preset("openrouter").base_url, "model-b", "key-b",
    )
    assert [t.answer for t in chat.turns] == [first.answer, second.answer]
    assert first.answer == "Answer from model-a [1]."
    assert second.answer == "Answer from model-b [1]."
    assert "User: When is launch?\nAssistant: Answer from model-a" in b.questions[0]


def test_model_change_on_same_provider_and_local_provider_switch(env):
    _store, saved = env
    chat = ask.AskModel(store=object())
    activate(saved, "fireworks", "model-a", key="key-a")
    chat.ask("launch one?")
    activate(saved, "fireworks", "model-a2")
    chat.ask("launch two?")
    activate(saved, "ollama", "llama3.2:3b")
    chat.ask("launch three?")
    models = [(c.base_url, c.model) for c in FakeClient.instances]
    assert models == [
        (keys.get_preset("fireworks").base_url, "model-a"),
        (keys.get_preset("fireworks").base_url, "model-a2"),
        (keys.get_preset("ollama").base_url, "llama3.2:3b"),
    ]
    assert len(chat.turns) == 3 and not any(t.error for t in chat.turns)


def test_rotated_key_is_used_after_reactivating_the_same_provider(env):
    _store, saved = env
    chat = ask.AskModel(store=object())
    activate(saved, "openai", "gpt-x", key="old-key")
    chat.ask("launch one?")
    activate(saved, "openai", "gpt-x", key="rotated-key")  # save + activate again
    chat.ask("launch two?")
    assert [c.api_key for c in FakeClient.instances] == ["old-key", "rotated-key"]
    assert FakeClient.instances[0] is not FakeClient.instances[1]


def test_custom_endpoint_url_is_honoured(env):
    _store, saved = env
    chat = ask.AskModel(store=object())
    activate(saved, "custom", "m", key="k", url="https://llm.example/v1/")
    chat.ask("launch one?")
    assert FakeClient.instances[0].base_url == "https://llm.example/v1"


def test_injected_llm_and_config_are_preserved(env, monkeypatch):
    _store, saved = env
    injected = FakeClient("http://injected", "snap", "snap-key")
    FakeClient.instances.clear()
    cfg = Config()
    chat = ask.AskModel(llm=injected, config=cfg, store=object())
    activate(saved, "fireworks", "model-a", key="key-a")
    chat.ask("launch one?")
    assert injected.questions and FakeClient.instances == []  # nothing rebuilt


def test_ask_controller_never_caches_llm_or_config():
    source = (ROOT / "localflow" / "ask_mac.py").read_text()
    # Workers copy whatever the model was given (None in production), so each
    # question rebuilds from the saved provider settings.
    assert "AskModel(store=hub.history)" in source
    assert (
        "AskModel(llm=self.model.llm, config=self.model.config, store=self.model.store)"
        in source
    )
    assert "build_llm" not in source and "load_config" not in source
