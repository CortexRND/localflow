"""Models & keys: status, active rules, import defaults, geometry and security."""

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
from localflow.ui_tokens import (
    HUB_CONTENT_GUTTER,
    HUB_CONTENT_X,
    HUB_HEADER_OFFSET,
    HUB_MIN_HEIGHT,
    HUB_MIN_WIDTH,
    HUB_PAGE_BOTTOM,
    MODELS_ROW_HEIGHT,
)

ROOT = Path(__file__).resolve().parents[1]
HUB = ROOT / "localflow/hub_mac.py"
BODY_MIN = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER
VIEWPORT = HUB_MIN_HEIGHT - 24

FIREWORKS = keys.get_preset("fireworks")
OPENAI = keys.get_preset("openai")
OLLAMA = keys.get_preset("ollama")
CUSTOM = keys.get_preset("custom")


def cloud(preset, model="m", url=None):
    return replace(
        Config(), llm_provider="openai-compatible",
        llm_base_url=preset.base_url if url is None else url, llm_model=model,
    )


# ------------------------------------------------------------ active rules ---

def test_default_config_is_the_local_ollama_provider():
    config = Config()
    assert [p.id for p in keys.PRESETS if ml.is_active(p, config)] == ["ollama"]
    assert ml.active_preset(config) is OLLAMA


def test_active_needs_both_provider_and_endpoint_to_match():
    config = cloud(FIREWORKS)
    assert [p.id for p in keys.PRESETS if ml.is_active(p, config)] == ["fireworks"]
    # Same provider kind, different endpoint: not the named preset, but custom.
    other = cloud(FIREWORKS, url="https://llm.internal.example/v1")
    assert [p.id for p in keys.PRESETS if ml.is_active(p, other)] == ["custom"]
    # Ollama provider on a non-default URL is not "Local (Ollama)".
    remote_ollama = replace(Config(), llm_base_url="http://other-host:11434")
    assert not ml.is_active(OLLAMA, remote_ollama)
    # A trailing slash does not change the endpoint.
    assert ml.is_active(FIREWORKS, cloud(FIREWORKS, url=FIREWORKS.base_url + "/"))


def test_custom_is_judged_against_the_typed_endpoint_in_the_editor():
    config = cloud(CUSTOM, url="https://llm.internal.example/v1")
    assert ml.is_active(CUSTOM, config, url="https://llm.internal.example/v1")
    assert not ml.is_active(CUSTOM, config, url="https://other.example/v1")
    assert not ml.is_active(CUSTOM, config, url="")


@pytest.mark.parametrize(
    ("preset", "config", "has_key", "label", "kind"),
    [
        (FIREWORKS, cloud(FIREWORKS), True, "Active", ("cobalt", "cobalt")),
        (FIREWORKS, cloud(OPENAI), True, "Key saved", ("success", "success")),
        (FIREWORKS, cloud(OPENAI), False, "No key", ("muted", "muted")),
        (OLLAMA, cloud(OPENAI), False, "Local", ("line", "ink")),
        (OLLAMA, Config(), False, "Active", ("cobalt", "cobalt")),
        # An active provider is Active even if its key was since removed.
        (FIREWORKS, cloud(FIREWORKS), False, "Active", ("cobalt", "cobalt")),
    ],
)
def test_status_pill(preset, config, has_key, label, kind):
    assert ml.provider_status(preset, config, has_key) == (label, kind)


def test_use_button_is_hidden_only_when_already_active_and_unchanged():
    config = cloud(FIREWORKS, model="model-a")
    assert not ml.show_use_button(
        FIREWORKS, config, url=FIREWORKS.base_url, model="model-a")
    # A different model on the active provider can still be applied.
    assert ml.show_use_button(FIREWORKS, config, url=FIREWORKS.base_url, model="model-b")
    assert ml.show_use_button(OPENAI, config, url=OPENAI.base_url, model="model-a")
    custom = cloud(CUSTOM, model="m", url="https://llm.internal.example/v1")
    assert not ml.show_use_button(CUSTOM, custom, url="https://llm.internal.example/v1", model="m")
    assert ml.show_use_button(CUSTOM, custom, url="https://typed.example/v1", model="m")


def test_summary_values_name_the_speech_and_cleanup_models():
    config = replace(cloud(FIREWORKS, model=""), stt_backend="mlx-whisper", model_size="small")
    speech, cleanup = ml.summary_values(config)
    assert speech == "MLX Whisper / small"
    assert cleanup == "Fireworks / not selected"


def test_speech_summary_uses_the_provider_field_not_the_device():
    # Regression: an explicit provider with a different stt_device must name
    # the provider. The screenshot's "auto / base" came from the provider
    # field (default auto), never from stt_device.
    parakeet = replace(Config(), stt_backend="parakeet", model_size="large-v3", stt_device="cpu")
    assert ml.speech_summary(parakeet) == "Parakeet / TDT 0.6B v2"
    assert "cpu" not in ml.speech_summary(parakeet)
    assert "large-v3" not in ml.speech_summary(parakeet)  # ignored by Parakeet
    for backend, label in (
        ("faster-whisper", "Faster Whisper / base"),
        ("mlx", "MLX Whisper / base"),
        ("mlx-whisper", "MLX Whisper / base"),
        ("my-plugin", "my-plugin / base"),
    ):
        config = replace(Config(), stt_backend=backend, model_size="base", stt_device="mps")
        assert ml.speech_summary(config) == label
        assert "mps" not in ml.speech_summary(config)


def test_automatic_speech_is_not_resolved_or_loaded(monkeypatch):
    # "auto" stays "Automatic": no guessing the engine, no model load/download.
    from localflow import stt as stt_module
    from localflow.providers import registry

    def boom(*_args, **_kwargs):
        raise AssertionError("display must not touch the speech providers")

    monkeypatch.setattr(registry, "stt_provider_class", boom)
    monkeypatch.setattr(registry, "stt_available", boom)
    monkeypatch.setattr(stt_module, "Transcriber", boom)
    assert ml.speech_summary(Config()) == "Automatic / base"
    assert ml.speech_summary(replace(Config(), stt_device="cuda")) == "Automatic / base"
    assert ml.summary_values(Config())[0] == "Automatic / base"


def test_snapshots_prove_the_explicit_provider_label():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text(encoding="utf-8")
    assert 'stt_backend="parakeet"' in snapshots and 'stt_device="cpu"' in snapshots
    assert '"hub-models-parakeet"' in snapshots


# --------------------------------------------------------------- import ---

def candidate(provider_id, source="environment: KEY"):
    return keys.ImportCandidate(provider_id, source, "secret-value-1234")


def test_import_ticks_default_on_only_without_a_saved_key():
    found = [candidate("fireworks"), candidate("openai", "~/.zshrc: OPENAI_API_KEY")]
    states = ml.import_states(found, lambda pid: pid == "openai")
    assert states == {
        ("fireworks", "environment: KEY"): True,
        ("openai", "~/.zshrc: OPENAI_API_KEY"): False,
    }


def test_user_changed_ticks_survive_rebuilds():
    found = [candidate("fireworks"), candidate("openai", "file")]
    overrides = {("fireworks", "environment: KEY"): 0, ("openai", "file"): 1}
    states = ml.import_states(found, lambda pid: pid == "openai", overrides)
    assert states[("fireworks", "environment: KEY")] is False
    assert states[("openai", "file")] is True


# ------------------------------------------------------------- geometry ---

def overlap(a, b):
    return (
        a[0] < b[0] + b[2] and b[0] < a[0] + a[2]
        and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]
    )


@pytest.mark.parametrize("width", [BODY_MIN, 800, 1016])
def test_summary_card_buttons_clear_the_text(width):
    geo = ml.summary_layout(width)
    boxes = [geo["change"], geo["import"]]
    for row in geo["rows"]:
        boxes += [row["caption"], row["value"]]
        assert row["value"][0] + row["value"][2] <= geo["change"][0]
    for index, first in enumerate(boxes):
        assert first[0] >= 0 and first[0] + first[2] <= width
        assert first[1] >= 0 and first[1] + first[3] <= geo["height"]
        for second in boxes[index + 1:]:
            assert not overlap(first, second)
    assert geo["change"][2] == geo["import"][2]


@pytest.mark.parametrize("pill_width", [70, 90, 120])
def test_row_columns_never_overlap(pill_width):
    cols = ml.row_columns(BODY_MIN, pill_width)
    text_x, text_w = cols["text"]
    button_x, button_w = cols["button"]
    assert text_x + text_w < cols["pill"]
    assert cols["pill"] + pill_width < button_x
    assert button_x + button_w <= BODY_MIN - 16
    assert text_w > 200  # room for the name and base URL


def inner_width(width=BODY_MIN):
    return width - 32


def test_collapsed_list_is_compact_and_the_page_fits_the_minimum_window():
    geo = ml.list_layout(BODY_MIN, len(keys.PRESETS))
    assert geo["height"] == len(keys.PRESETS) * MODELS_ROW_HEIGHT
    assert geo["editor_y"] is None
    page = HUB_HEADER_OFFSET + ml.SUMMARY_HEIGHT + 12 + geo["height"]
    assert page <= VIEWPORT - HUB_PAGE_BOTTOM  # no outer scrolling when collapsed


def test_only_the_expanded_row_adds_height():
    rows, editor_height = ml.editor_layout(
        inner_width(), custom=False, needs_key=True, show_use=True)
    collapsed = ml.list_layout(BODY_MIN, 7)
    expanded = ml.list_layout(BODY_MIN, 7, 2, editor_height)
    assert expanded["height"] == collapsed["height"] + editor_height
    assert expanded["rows"][:3] == collapsed["rows"][:3]
    assert expanded["editor_y"] == expanded["rows"][2] + MODELS_ROW_HEIGHT
    assert expanded["rows"][3] == expanded["editor_y"] + editor_height  # next row pushed
    assert rows  # sanity


@pytest.mark.parametrize(
    ("custom", "needs_key", "show_use", "lines"),
    [
        (False, True, True, 0), (False, True, False, 0), (False, False, True, 0),
        (True, True, True, 1), (True, True, False, 2), (False, False, False, 0),
    ],
)
def test_editor_has_only_needed_controls_and_never_overlaps(
    custom, needs_key, show_use, lines
):
    inner = inner_width()
    rows, height = ml.editor_layout(
        inner, custom=custom, needs_key=needs_key, show_use=show_use,
        message_lines=lines,
    )
    assert ("url_field" in rows) is custom
    assert ("key_field" in rows) is needs_key
    assert ("use" in rows) is show_use
    assert ("message" in rows) is bool(lines)
    assert "test" in rows and "model_field" in rows
    boxes = list(rows.values())
    for index, first in enumerate(boxes):
        assert first[0] >= 16 and first[0] + first[2] <= 16 + inner
        assert first[1] + first[3] <= height
        for second in boxes[index + 1:]:
            assert not overlap(first, second)
    if not show_use:  # no gap where the hidden primary would be
        assert rows["test"][0] == 16


def test_hiding_unused_rows_makes_the_editor_shorter():
    full = ml.editor_layout(inner_width(), custom=True, needs_key=True, show_use=True,
                            message_lines=2)[1]
    local = ml.editor_layout(inner_width(), custom=False, needs_key=False,
                             show_use=False)[1]
    assert local < full
    assert local < 150  # no large empty vertical slots


def test_import_card_height_grows_with_candidates_only():
    assert ml.import_height(2) - ml.import_height(1) == ml.IMPORT_ROW_H


# ---------------------------------------------------- hub behaviour/security ---

def controller_methods():
    tree = ast.parse(HUB.read_text(encoding="utf-8"))
    cls = next(
        n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "HubWindowController"
    )
    return {n.name: n for n in cls.body if isinstance(n, ast.FunctionDef)}


def extracted(name):
    method = controller_methods()[name]
    method.decorator_list = []
    namespace = {}
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[method], type_ignores=[]), str(HUB), "exec"), namespace,
    )
    return namespace[name]


def source_of(name):
    return ast.get_source_segment(HUB.read_text(encoding="utf-8"), controller_methods()[name])


def toggle_controller(expanded=None):
    secure = Mock()
    secure.stringValue.return_value = "typed-but-unsaved-key"
    controls = {"fireworks": (None, Mock(), secure)} if expanded == "fireworks" else {}
    return SimpleNamespace(
        provider_expanded=expanded, provider_controls=controls,
        _models_capture=Mock(), show_page=Mock(),
    ), secure


def tag_of(provider_id):
    return [p.id for p in keys.PRESETS].index(provider_id)


def test_clicking_a_row_expands_only_that_provider_and_clicking_again_collapses():
    controller, _ = toggle_controller()
    toggle = extracted("providerToggle_")
    toggle(controller, SimpleNamespace(tag=lambda: tag_of("openai")))
    assert controller.provider_expanded == "openai"
    toggle(controller, SimpleNamespace(tag=lambda: tag_of("groq")))
    assert controller.provider_expanded == "groq"  # one at a time
    toggle(controller, SimpleNamespace(tag=lambda: tag_of("groq")))
    assert controller.provider_expanded is None
    assert controller.show_page.call_count == 3
    controller.show_page.assert_called_with("Models")


def test_switching_captures_selections_and_drops_the_unsaved_key():
    controller, secure = toggle_controller("fireworks")
    extracted("providerToggle_")(controller, SimpleNamespace(tag=lambda: tag_of("openai")))
    controller._models_capture.assert_called_once()  # URL / model / ticks kept
    secure.setStringValue_.assert_called_once_with("")  # secret wiped, not retained
    secure.stringValue.assert_not_called()  # and never read into state
    assert controller.provider_expanded == "openai"


def test_expansion_state_lives_on_the_controller_across_rebuilds():
    page = source_of("_page_models")
    assert 'hasattr(self, "provider_expanded")' in page  # initialised once
    assert "self.provider_expanded = None" in page
    show = source_of("show_page")
    assert "provider_expanded" not in show  # navigation/resize never resets it
    assert "self.provider_controls = {}" in show and "self.provider_checks = []" in show


def test_models_are_fetched_only_for_the_expanded_provider():
    calls = [
        name for name, node in controller_methods().items()
        if '_models_worker(preset.id, "models")' in source_of(name)
        or '_models_worker(provider_id, "models")' in source_of(name)
    ]
    assert calls == ["_models_editor"]
    # The row loop builds no controls and starts no work.
    assert "_models_worker" not in source_of("_models_provider_list")
    assert "NSComboBox" not in source_of("_models_provider_list")
    assert "NSSecureTextField" not in source_of("_models_provider_list")


def test_single_primary_is_hidden_not_reserved_space_when_active():
    editor = source_of("_models_editor")
    # Always built, once, at the shared action-row slot; visibility is toggled.
    assert editor.count('kind="primary"') == 1
    assert editor.count('"Use for cleanup & Ask"') == 1
    assert 'if "use" in rows' not in editor
    assert "_models_apply_use(" in editor and "setDelegate_(self)" in editor
    listing = source_of("_models_provider_list")
    assert "show_use=True" in listing
    assert "is_active(" in listing and "provider_status(" in listing
    rows, _height = ml.editor_layout(inner_width(), custom=False, needs_key=True, show_use=True)
    use, test = ml.action_frames(rows, True)
    hidden_use, hidden_test = ml.action_frames(rows, False)
    assert hidden_use == use and test == rows["test"]
    assert hidden_test[0] == use[0] < test[0]  # Test slides left into the slot
    assert hidden_test[1:] == test[1:]  # same row, same size: no height change


def ns_method(name, **namespace):
    method = controller_methods()[name]
    method.decorator_list = []
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[method], type_ignores=[]), str(HUB), "exec"), namespace,
    )
    return namespace[name]


def live_controller(config, *, model, url=None, custom=False):
    preset = CUSTOM if custom else OLLAMA
    secure = Mock()
    combo = Mock()
    combo.stringValue.return_value = model
    url_field = Mock() if custom else None
    if url_field:
        url_field.stringValue.return_value = url
    rows, _ = ml.editor_layout(
        inner_width(), custom=custom, needs_key=custom, show_use=True)
    use, test = Mock(), Mock()
    controller = SimpleNamespace(
        page="Models", config=config,
        provider_state={preset.id: {"url": url or preset.base_url, "model": model, "models": []}},
        provider_controls={preset.id: (url_field, combo, secure)},
        provider_action_views={"use": use, "test": test, "rows": rows},
        show_page=Mock(), _rebuild_current_page=Mock(),
    )
    controller._models_apply_use = lambda show: ns_method(
        "_models_apply_use", action_frames=ml.action_frames, NSMakeRect=lambda *r: r,
    )(controller, show)
    controller._models_live_update = lambda source, picked=None: ns_method(
        "_models_live_update", use_button_visible=ml.use_button_visible,
    )(controller, source, picked)
    return controller, combo, url_field, secure, use, test, rows


def test_active_ollama_with_a_non_default_model_seeds_that_model():
    config = replace(Config(), llm_model="qwen2.5:7b")
    controller = SimpleNamespace(config=config, provider_state={})
    state_for = ns_method("_models_state", is_active=ml.is_active)
    state = state_for(controller, OLLAMA)
    assert state["model"] == "qwen2.5:7b"  # not the preset default
    assert not ml.show_use_button(OLLAMA, config, url=state["url"], model=state["model"])
    state["model"] = "a later draft"
    assert state_for(controller, OLLAMA)["model"] == "a later draft"  # drafts kept
    # An inactive provider still starts from its own default.
    assert state_for(controller, OPENAI)["model"] == OPENAI.default_model


def test_typing_and_picking_a_model_toggles_use_and_test_in_place():
    config = replace(Config(), llm_model="qwen2.5:7b")
    controller, combo, _url, secure, use, test, rows = live_controller(
        config, model="qwen2.5:7b")
    notify = SimpleNamespace(object=lambda: combo)
    delegate = ns_method("controlTextDidChange_")
    pick = ns_method("comboBoxSelectionDidChange_")

    combo.stringValue.return_value = "llama3.2:3b"  # typed a different model
    delegate(controller, notify)
    use.setHidden_.assert_called_with(False)
    assert test.setFrame_.call_args.args[0][0] == rows["test"][0]  # Test on the right
    assert controller.provider_state["ollama"]["model"] == "llama3.2:3b"

    combo.stringValue.return_value = "qwen2.5:7b"  # reverted
    delegate(controller, notify)
    use.setHidden_.assert_called_with(True)
    assert test.setFrame_.call_args.args[0][0] == rows["use"][0]  # Test slides left

    combo.objectValueOfSelectedItem.return_value = "mistral:7b"  # picked from the list
    pick(controller, notify)
    use.setHidden_.assert_called_with(False)
    assert controller.provider_state["ollama"]["model"] == "mistral:7b"

    secure.stringValue.assert_not_called()  # the key field is never touched
    secure.setStringValue_.assert_not_called()
    controller.show_page.assert_not_called()  # and nothing rebuilds
    controller._rebuild_current_page.assert_not_called()


@pytest.fixture
def vault(monkeypatch):
    """In-memory secrets: no Keychain, no config file."""
    store = {}
    monkeypatch.setattr(keys.secrets, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(
        keys.secrets, "set_secret", lambda name, value: store.__setitem__(name, value))
    monkeypatch.setattr(keys.secrets, "delete_secret", lambda name: store.pop(name, None))
    monkeypatch.setattr(keys, "save_config", lambda _config: None)
    return store


def test_replacing_the_key_of_the_active_provider_offers_use_until_activated(vault):
    config = cloud(FIREWORKS, model="m")

    def visible():
        return ml.use_button_visible(FIREWORKS, config, url=FIREWORKS.base_url, model="m")

    keys.save_key("fireworks", "fake-key-A")
    config = keys.activate("fireworks", model="m", config=config)
    assert visible() is False  # active, same model, same key
    keys.save_key("fireworks", "fake-key-B")  # Save a replacement
    assert visible() is True and keys.key_needs_activation("fireworks", config) is True
    assert "llm_api_key" not in vault  # Save alone never activates anything
    config = keys.activate("fireworks", model="m", config=config)
    assert visible() is False  # the new activation snapshot holds the new key
    active = llm_activation.resolve(config.llm_key_ref, config.llm_base_url, vault.get)
    assert bool(active == "fake-key-B") is True  # Ask now uses it
    keys.save_imports([keys.ImportCandidate("fireworks", "env: FIREWORKS_API_KEY", "fake-key-C")])
    assert visible() is True  # an imported replacement is activateable too
    config = keys.activate("fireworks", model="m", config=config)
    assert visible() is False


def test_missing_or_irrelevant_keys_never_offer_reactivation(vault):
    config = cloud(FIREWORKS, model="m")
    assert keys.key_needs_activation("fireworks") is False  # nothing saved
    keys.save_key("fireworks", "fake-key-A")
    config = keys.activate("fireworks", model="m", config=config)
    keys.remove_key("fireworks")
    assert keys.key_needs_activation("fireworks", config) is False
    assert not vault  # the active snapshot (and its index entry) went too
    assert ml.use_button_visible(FIREWORKS, config, url=FIREWORKS.base_url, model="m") is False
    assert keys.key_needs_activation("ollama") is False  # keyless provider


def test_secrets_are_only_compared_for_an_otherwise_active_provider(monkeypatch):
    def forbidden(_name):
        raise AssertionError("secret lookup for a non-active provider")

    monkeypatch.setattr(keys.secrets, "get_secret", forbidden)
    config = cloud(FIREWORKS, model="m")
    assert ml.use_button_visible(OPENAI, config, url=OPENAI.base_url, model="m") is True
    assert ml.use_button_visible(FIREWORKS, config, url=FIREWORKS.base_url, model="other") is True
    assert ml.show_use_button(
        FIREWORKS, config, url=FIREWORKS.base_url, model="m", key_changed=True) is True


def test_hub_routes_initial_and_live_use_through_the_key_aware_helper():
    for name in ("_models_editor", "_models_live_update"):
        assert "use_button_visible(" in source_of(name)
    assert "key_needs_activation" not in source_of("_models_editor")  # boolean helper only


def test_custom_url_edits_update_use_live_and_other_controls_are_ignored(vault):
    url = "https://llm.internal.example/v1"
    config = cloud(CUSTOM, model="m", url=url)
    controller, _combo, url_field, secure, use, _test, _rows = live_controller(
        config, model="m", url=url, custom=True)
    delegate = ns_method("controlTextDidChange_")
    url_field.stringValue.return_value = "https://typed.example/v1"
    delegate(controller, SimpleNamespace(object=lambda: url_field))
    use.setHidden_.assert_called_with(False)
    assert controller.provider_state["custom"]["url"] == "https://typed.example/v1"
    url_field.stringValue.return_value = url
    delegate(controller, SimpleNamespace(object=lambda: url_field))
    use.setHidden_.assert_called_with(True)
    before = use.setHidden_.call_count
    delegate(controller, SimpleNamespace(object=lambda: secure))  # not a routed control
    assert use.setHidden_.call_count == before
    secure.stringValue.assert_not_called()


def test_live_edits_survive_a_resize_rebuild():
    config = replace(Config(), llm_model="qwen2.5:7b")
    controller, combo, _url, _secure, _use, _test, _rows = live_controller(
        config, model="qwen2.5:7b")
    combo.stringValue.return_value = "edited-model"
    ns_method("controlTextDidChange_")(controller, SimpleNamespace(object=lambda: combo))
    controller.provider_checks = []
    ns_method("_models_capture")(controller)  # what show_page runs before teardown
    assert controller.provider_state["ollama"]["model"] == "edited-model"
    assert ml.show_use_button(OLLAMA, config, url=OLLAMA.base_url, model="edited-model")
    assert "_models_live_update" in source_of("controlTextDidChange_")
    live = source_of("_models_live_update")
    for forbidden in ("show_page", "_rebuild_current_page", "setStringValue_", "makeFirstResponder_"):
        assert forbidden not in live
    assert "secure" not in live.replace("_secure", "")


def test_page_summary_and_import_wiring():
    summary = source_of("_models_summary")
    assert "Change speech model…" in summary and '"openSetup:"' in summary
    assert "Import keys…" in summary and '"providerImport:"' in summary
    assert "Cleanup & Ask model" in summary and "Speech model" in summary
    assert 'kind="primary"' not in summary  # both are compact secondary actions
    card = source_of("_models_import_card")
    assert "import_states(" in card and "self.provider_checked" in card
    assert "get_secret" not in card and "candidate._value" not in card


def test_async_results_are_safe_when_closed_or_off_page():
    worker = source_of("_models_worker")
    assert "if self._closed:" in worker
    assert 'if self.page == "Models"' in worker
    # A models result fills the open picker in place instead of rebuilding.
    assert "removeAllItems()" in worker and "return" in worker


def test_capture_never_reads_secure_fields():
    capture = source_of("_models_capture")
    assert "_secure" in capture and "secure" not in capture.replace("_secure", "")
    assert "provider_controls = {}" not in capture
