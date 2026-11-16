"""PR29 review finding 3: an empty model must never be activated.

`provider_keys.activate(..., model="")` (or whitespace) currently saves
`llm_model = ""`, an unusable config for every provider, and the Hub's
"Use for cleanup & Ask" stays actionable while the model field is blank.

The rejection must happen before any key or config write. The Hub controller
methods are AST-extracted from hub_mac.py and run with fake controls, so no
AppKit is needed.
"""

import ast
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import llm_activation
from localflow import models_layout as ml
from localflow import provider_keys as keys
from localflow.config import Config
from localflow.ui_tokens import HUB_CONTENT_GUTTER, HUB_CONTENT_X, HUB_MIN_WIDTH

EDITOR_WIDTH = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER - 32
HUB = Path(__file__).resolve().parents[1] / "localflow" / "hub_mac.py"
OPENAI = keys.get_preset("openai")
OLLAMA = keys.get_preset("ollama")
BLANK = ["", " ", "   ", "\t", " \n "]


@pytest.fixture
def writes(monkeypatch):
    """In-memory secrets; every key and config write is recorded."""
    store = {"provider_key_openai": "fake-key-openai"}
    log = []
    monkeypatch.setattr(keys.secrets, "get_secret", lambda name: store.get(name, ""))

    def set_secret(name, value):
        log.append(("set_secret", name))
        store[name] = value

    def delete_secret(name):
        log.append(("delete_secret", name))
        store.pop(name, None)

    monkeypatch.setattr(keys.secrets, "set_secret", set_secret)
    monkeypatch.setattr(keys.secrets, "delete_secret", delete_secret)
    monkeypatch.setattr(
        keys, "save_config", lambda config, *a, **k: log.append(("save_config", config)),
    )
    monkeypatch.setattr(keys, "load_config", lambda: Config())
    return SimpleNamespace(store=store, log=log)


@pytest.mark.parametrize("model", BLANK)
def test_blank_model_is_rejected_for_a_cloud_provider_before_any_write(writes, model):
    with pytest.raises(ValueError):
        keys.activate("openai", model=model, config=Config())
    assert writes.log == []
    assert "llm_api_key" not in writes.store


@pytest.mark.parametrize("model", BLANK)
def test_blank_model_is_rejected_for_custom_before_any_write(writes, model):
    writes.store["provider_key_custom"] = "fake-key-custom"
    with pytest.raises(ValueError):
        keys.activate(
            "custom", base_url="https://llm.example.test/v1", model=model,
            config=Config(),
        )
    assert writes.log == []


@pytest.mark.parametrize("model", BLANK)
def test_blank_model_never_saves_an_empty_ollama_model(writes, model):
    """Ollama has a real default model, so falling back to it is acceptable;
    refusing is too. Persisting an empty model is not."""
    try:
        updated = keys.activate("ollama", model=model, config=Config())
    except ValueError:
        assert writes.log == []
        return
    assert updated.llm_model == OLLAMA.default_model
    saved = [entry[1] for entry in writes.log if entry[0] == "save_config"]
    assert [c.llm_model for c in saved] == [OLLAMA.default_model]


def test_existing_blank_model_is_not_kept_by_activation_without_a_model(writes):
    """`model=None` keeps the configured model, unless that model is blank."""
    with pytest.raises(ValueError):
        keys.activate("openai", config=replace(Config(), llm_model="  "))
    assert writes.log == []


def test_a_real_model_still_activates(writes):
    updated = keys.activate("openai", model="gpt-4o-mini", config=Config())
    assert (updated.llm_base_url, updated.llm_model) == (OPENAI.base_url, "gpt-4o-mini")
    # Order is finding 1's business; here only that the model was persisted.
    saved = [entry[1] for entry in writes.log if entry[0] == "save_config"]
    assert [c.llm_model for c in saved] == ["gpt-4o-mini"]
    # The key the factory would send comes from the activation snapshot the
    # saved config names (finding 1), not from a rewritten global key.
    assert saved[0].llm_key_ref == updated.llm_key_ref
    assert llm_activation.resolve(
        updated.llm_key_ref, updated.llm_base_url, lambda name: writes.store.get(name, ""),
    ) == "fake-key-openai"
    assert "llm_api_key" not in writes.store


def test_model_none_keeps_the_configured_model(writes):
    config = replace(Config(), llm_model="kept-model")
    assert keys.activate("openai", config=config).llm_model == "kept-model"


@pytest.mark.parametrize("given,saved", [
    (" gpt-4o-mini ", "gpt-4o-mini"),
    ("\tgpt-4o-mini\n", "gpt-4o-mini"),
])
def test_an_explicit_model_is_stored_stripped(writes, given, saved):
    assert keys.activate("openai", model=given, config=Config()).llm_model == saved


def test_model_none_strips_the_configured_model(writes):
    config = replace(Config(), llm_model="  kept-model  ")
    assert keys.activate("openai", config=config).llm_model == "kept-model"


def test_ollama_with_a_blank_configured_model_uses_its_default(writes):
    updated = keys.activate("ollama", config=replace(Config(), llm_model=" "))
    assert updated.llm_model == OLLAMA.default_model
    assert updated.llm_key_ref == ""  # still keyless


def test_ollama_keeps_an_explicit_model(writes):
    assert keys.activate("ollama", model="qwen2.5:7b", config=Config()).llm_model == (
        "qwen2.5:7b"
    )


def test_blank_model_error_says_what_to_do(writes):
    with pytest.raises(ValueError, match="Choose a model"):
        keys.activate("openai", model=" ", config=Config())


def test_a_missing_key_is_still_refused_with_a_real_model(writes):
    writes.store.clear()
    with pytest.raises(ValueError, match="Save a provider key"):
        keys.activate("openai", model="gpt-4o-mini", config=Config())
    assert writes.log == []


# ------------------------------------------------------------ Hub UI state ---


def controller_methods():
    tree = ast.parse(HUB.read_text(encoding="utf-8"))
    cls = next(
        n for n in tree.body
        if isinstance(n, ast.ClassDef) and n.name == "HubWindowController"
    )
    return {n.name: n for n in cls.body if isinstance(n, ast.FunctionDef)}


def ns_method(name, **namespace):
    method = controller_methods()[name]
    method.decorator_list = []
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[method], type_ignores=[]), str(HUB), "exec"), namespace,
    )
    return namespace[name]


def live_controller(preset, config, model):
    combo = Mock()
    combo.stringValue.return_value = model
    rows, _ = ml.editor_layout(
        EDITOR_WIDTH, custom=False, needs_key=preset.needs_key, show_use=True)
    use, test = Mock(), Mock()
    controller = SimpleNamespace(
        page="Models", config=config,
        provider_state={preset.id: {"url": preset.base_url, "model": model, "models": []}},
        provider_controls={preset.id: (None, combo, Mock())},
        provider_action_views={"use": use, "test": test, "rows": rows},
        show_page=Mock(), _rebuild_current_page=Mock(),
    )
    namespace = dict(vars(ml))  # the helpers hub_mac imports from models_layout
    namespace["NSMakeRect"] = lambda *r: r
    controller._models_apply_use = lambda show: ns_method(
        "_models_apply_use", **namespace)(controller, show)
    controller._models_live_update = lambda source, picked=None: ns_method(
        "_models_live_update", **namespace)(controller, source, picked)
    return controller, combo, use


def last(mock_method, default):
    return mock_method.call_args.args[0] if mock_method.call_args else default


def actionable(use):
    """Visible and enabled, whichever of the two the fix chooses to toggle."""
    return last(use.setHidden_, False) is False and last(use.setEnabled_, True) is True


@pytest.mark.parametrize("preset", [OPENAI, OLLAMA], ids=lambda p: p.id)
def test_use_follows_the_model_field_between_blank_and_filled(preset, monkeypatch):
    monkeypatch.setattr(keys, "key_needs_activation", lambda *_a, **_k: False)
    config = replace(
        Config(), llm_provider="openai-compatible",
        llm_base_url="https://api.fireworks.ai/inference/v1", llm_model="other",
    )  # a different provider is active, so this editor offers Use
    controller, combo, use = live_controller(preset, config, "a-model")
    typed = SimpleNamespace(object=lambda: combo)
    on_text = ns_method("controlTextDidChange_")

    on_text(controller, typed)
    assert actionable(use)

    for blank in ("", "   "):
        combo.stringValue.return_value = blank
        on_text(controller, typed)
        assert not actionable(use), f"Use is still actionable for model {blank!r}"

    combo.stringValue.return_value = "a-model"
    on_text(controller, typed)
    assert actionable(use)


def test_use_is_not_actionable_when_the_editor_opens_with_a_blank_model():
    """OpenAI has no preset default, so a fresh editor starts blank."""
    assert OPENAI.default_model == ""
    source = ast.get_source_segment(
        HUB.read_text(encoding="utf-8"), controller_methods()["_models_editor"],
    )
    # The initial state comes from the same rule as live typing...
    assert "_models_apply_use(" in source
    # ...and that rule refuses a blank model, whatever the active config.
    config = replace(Config(), llm_provider="openai-compatible",
                     llm_base_url="https://api.groq.com/openai/v1", llm_model="x")
    rule_ok = ml.use_button_visible(OPENAI, config, url=OPENAI.base_url, model="")
    enabled_rule = getattr(ml, "use_button_enabled", None)
    if enabled_rule is not None:
        rule_ok = rule_ok and enabled_rule(OPENAI, config, url=OPENAI.base_url, model="")
    assert not rule_ok


def test_initial_build_keeps_the_key_rule_and_uses_the_model_rule():
    source = ast.get_source_segment(
        HUB.read_text(encoding="utf-8"), controller_methods()["_models_editor"],
    )
    # Activation still needs a saved key (keyless Ollama excepted)...
    assert 'use.setEnabled_(status["has_key"] or not preset.needs_key)' in source
    # ...and visibility is decided from the editor's current model.
    assert 'url=state["url"], model=state["model"]' in source


@pytest.mark.parametrize("model", ["", " ", "\t"])
def test_blank_model_hides_use_even_when_a_replacement_key_waits(monkeypatch, model):
    monkeypatch.setattr(
        keys, "key_needs_activation",
        lambda *_a, **_k: pytest.fail("no secret lookup for a blank model"),
    )
    config = replace(Config(), llm_provider="openai-compatible",
                     llm_base_url=OPENAI.base_url, llm_model="gpt-4o-mini")
    assert ml.use_button_visible(OPENAI, config, url=OPENAI.base_url, model=model) is False


def test_active_provider_with_its_model_stays_hidden_and_a_new_model_shows(monkeypatch):
    monkeypatch.setattr(keys, "key_needs_activation", lambda *_a, **_k: False)
    config = replace(Config(), llm_model="qwen2.5:7b")  # Ollama active
    controller, combo, use = live_controller(OLLAMA, config, "qwen2.5:7b")
    typed = SimpleNamespace(object=lambda: combo)
    on_text = ns_method("controlTextDidChange_")

    on_text(controller, typed)
    assert not actionable(use)  # already active: hidden, as before
    combo.stringValue.return_value = " "
    on_text(controller, typed)
    assert not actionable(use)
    combo.stringValue.return_value = "llama3.2:3b"
    on_text(controller, typed)
    assert actionable(use)


def test_replacement_key_still_offers_use_for_the_active_model(monkeypatch):
    monkeypatch.setattr(keys, "key_needs_activation", lambda *_a, **_k: True)
    config = replace(Config(), llm_provider="openai-compatible",
                     llm_base_url=OPENAI.base_url, llm_model="gpt-4o-mini")
    assert ml.use_button_visible(
        OPENAI, config, url=OPENAI.base_url, model="gpt-4o-mini") is True


def test_picking_from_the_model_list_follows_the_same_rule(monkeypatch):
    monkeypatch.setattr(keys, "key_needs_activation", lambda *_a, **_k: False)
    config = replace(Config(), llm_provider="openai-compatible",
                     llm_base_url="https://api.groq.com/openai/v1", llm_model="x")
    controller, combo, use = live_controller(OPENAI, config, "")
    picked = SimpleNamespace(object=lambda: combo)
    on_pick = ns_method("comboBoxSelectionDidChange_")

    combo.objectValueOfSelectedItem.return_value = "gpt-4o-mini"
    on_pick(controller, picked)
    assert actionable(use)
    assert controller.provider_state["openai"]["model"] == "gpt-4o-mini"

    combo.objectValueOfSelectedItem.return_value = "  "
    on_pick(controller, picked)
    assert not actionable(use)
