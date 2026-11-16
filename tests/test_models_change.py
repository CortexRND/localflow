"""Models & keys -> Change models: pick speech and cleanup models in place.

Pure rules from models_layout, plus the real hub_mac card and actions run
against stub controls (their source is extracted, as in test_models_layout),
so target/action/enabled wiring and the save path are executed, not just read.
"""

import ast
import itertools
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest

from localflow import models_layout as ml
from localflow import provider_keys as keys
from localflow.config import Config, ConfigError
from localflow.providers.base import ModelInfo
from localflow.text_wrap import wrap_lines
from localflow.ui_tokens import HUB_PAGE_BOTTOM

ROOT = Path(__file__).resolve().parents[1]
HUB = ROOT / "localflow/hub_mac.py"
FIREWORKS = keys.get_preset("fireworks")


def cloud(model="accounts/fireworks/models/a", key_ref="llm_activation_fireworks_" + "a" * 32):
    return replace(
        Config(), llm_provider="openai-compatible", llm_base_url=FIREWORKS.base_url,
        llm_model=model, llm_key_ref=key_ref,
    )


# ------------------------------------------------------------- cleanup ---


@pytest.mark.parametrize("model", [
    "gemma4:e4b", "llama3.2:3b", "accounts/fireworks/models/kimi-k2p6", "gpt-4o-mini",
    "meta-llama/Llama-3.3-70B-Instruct-Turbo", "qwen/qwen3-32b@latest", "  padded  ",
])
def test_model_ids_accept_real_provider_ids(model):
    assert ml.validate_model_id(model) == model.strip()


@pytest.mark.parametrize("model", ["", "   ", "gpt 4o", "x\ny", "a" * 201, "-flag", "m" + chr(0x202E), None])
def test_model_ids_refuse_junk(model):
    with pytest.raises(ml.ModelChoiceError):
        ml.validate_model_id(model)


def test_model_only_change_keeps_provider_endpoint_and_key_pairing():
    saved = []
    config = cloud()
    updated = ml.set_active_model(
        "accounts/fireworks/models/b", config,
        expected=(config.llm_provider, config.llm_base_url + "/"), save=saved.append,
    )
    assert saved == [updated]
    assert updated.llm_model == "accounts/fireworks/models/b"
    for name in ("llm_provider", "llm_base_url", "llm_key_ref"):
        assert getattr(updated, name) == getattr(config, name)  # no re-entering keys


def test_unchanged_invalid_or_moved_provider_writes_nothing():
    saved = []
    config = cloud()
    assert ml.set_active_model(config.llm_model, config, save=saved.append) is config
    with pytest.raises(ml.ModelChoiceError):
        ml.set_active_model("bad id", config, save=saved.append)
    # The control was drawn for Ollama; the saved config is Fireworks now:
    # a model-only pick never switches (or re-pairs) providers.
    with pytest.raises(ml.ModelChoiceError, match="provider changed"):
        ml.set_active_model("x", config, expected=("ollama", "http://localhost:11434"),
                            save=saved.append)
    assert saved == []


class FakeOllama:
    def __init__(self, names=(), error=None):
        self.names, self.error = names, error

    def list_models(self):
        if self.error:
            raise self.error
        return [ModelInfo(id=name) for name in self.names]


def test_ollama_listing_states():
    assert ml.ollama_models("u", client=FakeOllama(["gemma4:e4b"])) == (["gemma4:e4b"], "ready")
    assert ml.ollama_models("u", client=FakeOllama([])) == ([], "empty")
    assert ml.ollama_models("u", client=FakeOllama(error=OSError("refused"))) == (
        [], "unavailable")
    names, state = ml.ollama_models("u", client=FakeOllama(["ok:1", "bad\nname", "x y"]))
    assert names == ["ok:1"] and state == "ready"


def test_cleanup_picker_states_say_what_to_do():
    config = Config(llm_model="llama3.2:3b")
    items, selected, note = ml.cleanup_picker(config, "ready", ["gemma4:e4b"])
    assert items == ["llama3.2:3b", "gemma4:e4b"] and selected == "llama3.2:3b"
    assert "isn't installed" in note and "ollama pull llama3.2:3b" in note
    assert ml.cleanup_picker(config, "ready", ["llama3.2:3b"])[2] == ""
    assert "Start Ollama" in ml.cleanup_picker(config, "unavailable", [])[2]
    assert "ollama pull" in ml.cleanup_picker(config, "empty", [])[2]
    assert "Loading" in ml.cleanup_picker(config, "loading", [])[2]


def test_fireworks_default_is_the_serverless_successor_and_explicit_ids_are_kept(
    tmp_path, monkeypatch,
):
    """kimi-k2p6 left Fireworks serverless; kimi-k3 replaces it as the
    built-in default only. A model someone configured is never migrated."""
    from localflow.config import _config_from_v2, _parse, save_config
    from localflow.providers import factory

    assert Config().fireworks_model == "accounts/fireworks/models/kimi-k3"
    monkeypatch.setattr(
        factory, "get_secret", lambda name: "test-key" if name == "fireworks_api_key" else "",
    )
    llm = factory.build_workprompts_llm(Config())  # builds a client; no request made
    assert llm.model == "accounts/fireworks/models/kimi-k3"
    assert llm.base_url == "https://api.fireworks.ai/inference/v1"

    path = tmp_path / "config.toml"
    old_id = "accounts/fireworks/models/kimi-k2p6"
    save_config(replace(Config(), fireworks_model=old_id), path)
    configured = _config_from_v2(_parse(path))
    assert configured.fireworks_model == old_id
    assert factory.build_workprompts_llm(configured).model == old_id


# -------------------------------------------------------------- speech ---


def available(*ids):
    return lambda provider: provider in ids


def test_speech_menu_offers_only_what_runs_here_plus_the_current_one():
    choices = ml.speech_provider_choices(Config(), available("faster-whisper"))
    assert [pid for pid, _label in choices] == ["auto", "faster-whisper"]
    parakeet = replace(Config(), stt_backend="parakeet")
    assert "parakeet" in [p for p, _l in ml.speech_provider_choices(parakeet, available())]
    mlx = replace(Config(), stt_backend="mlx")
    assert ml.current_speech_provider(mlx) == "mlx-whisper"


def test_parakeet_is_a_fixed_model_and_keeps_the_saved_size():
    assert ml.speech_model_choices("parakeet") == ()
    saved = []
    config = replace(Config(), stt_device="cpu", model_size="small")
    updated = ml.set_speech("parakeet", "large-v3", config,
                            available=available("parakeet"), save=saved.append)
    assert (updated.stt_backend, updated.model_size, updated.stt_device) == (
        "parakeet", "small", "auto")
    assert saved == [updated]


def test_whisper_sizes_and_refusals():
    saved = []
    config = Config()
    updated = ml.set_speech("faster-whisper", "small", config,
                            available=available("faster-whisper"), save=saved.append)
    assert (updated.stt_backend, updated.model_size) == ("faster-whisper", "small")
    assert "distil-large-v3" in [s for s, _l in ml.speech_model_choices("faster-whisper")]
    assert "distil-large-v3" not in [s for s, _l in ml.speech_model_choices("mlx-whisper")]
    odd = ml.set_speech("faster-whisper", "not-a-size", config,
                        available=available("faster-whisper"), save=saved.append)
    assert odd.model_size == "base"  # only catalog sizes are ever saved
    with pytest.raises(ml.ModelChoiceError):
        ml.set_speech("parakeet", None, config, available=available(), save=saved.append)
    with pytest.raises(ml.ModelChoiceError):
        ml.set_speech("whisper-cloud", None, config, available=lambda _p: True,
                      save=saved.append)
    same = ml.set_speech("auto", "base", config, available=available(), save=saved.append)
    # An unchanged choice is still saved: re-picking is the retry signal after a
    # failed load (the running app reloads nothing for a healthy model).
    assert same == config and len(saved) == 3 and saved[-1] == config


# -------------------------------------------------------------- layout ---


def overlap(a, b):
    return a[0] < b[0] + b[2] and b[0] < a[0] + a[2] and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]


@pytest.mark.parametrize("width", [596, 800, 1016])
@pytest.mark.parametrize("ollama", [True, False])
@pytest.mark.parametrize("fixed", [True, False])
def test_change_models_card_never_overlaps(width, ollama, fixed):
    frames = ml.quick_layout(width, ollama=ollama, fixed_speech=fixed, message_lines=2)
    height = frames.pop("height")
    for frame in frames.values():
        x, y, w, h = frame
        assert x >= 0 and x + w <= width and y >= 0 and y + h <= height
    for first, second in itertools.combinations(frames.values(), 2):
        assert not overlap(first, second)
    assert ("refresh" in frames) is ollama
    assert ("speech_fixed" in frames) is fixed and ("speech_model" in frames) is not fixed
    # One card: Import keys… shares the title row; the active provider has
    # its own status line under the cleanup control, not in the row label.
    assert frames["import"][1] == ml.CARD_PADDING and frames["title"][1] < frames["detail"][1]
    status, model = frames["provider_status"], frames["cleanup_model"]
    assert status[0] == model[0] and status[1] >= model[1] + model[3]
    assert status[1] + status[3] <= frames["message"][1]


@pytest.mark.parametrize("width", [596, 1016])
def test_no_message_reserves_no_blank_lines(width):
    """The card ends right under the provider status line: no reserved note."""
    bare = ml.quick_layout(width, ollama=True, fixed_speech=False)
    assert "message" not in bare
    status = bare["provider_status"]
    assert bare["height"] == status[1] + status[3] + ml.CARD_PADDING
    one = ml.quick_layout(width, ollama=True, fixed_speech=False, message_lines=1)
    assert one["height"] - bare["height"] == ml.MODELS_ROW_GAP + ml.MESSAGE_LINE


def char_measure(text):
    return len(text) * 7


def test_note_lines_are_the_wrapped_lines_and_zero_when_empty():
    config = Config(llm_model="a-rather-long-model-name:latest")
    missing = ml.cleanup_picker(config, "ready", ["other"])[2]
    assert ml.note_lines([missing], 200, char_measure) == len(
        wrap_lines(missing, 200, char_measure)) > 1
    assert ml.note_lines([missing], 2000, char_measure) == 1
    assert ml.note_lines([""], 200, char_measure) == ml.note_lines([], 200, char_measure) == 0
    assert not hasattr(ml, "cleanup_note_candidates")  # no worst-case reservation


def test_provider_status_names_the_active_provider_only():
    assert ml.provider_status_text(Config()) == "Active provider: Local (Ollama)"
    assert ml.provider_status_text(cloud()) == "Active provider: Fireworks"
    odd = replace(Config(), llm_provider="plugin-llm", llm_base_url="http://x")
    assert ml.provider_status_text(odd) == "Active provider: plugin-llm"
    assert ml.CLEANUP_LABEL == "Cleanup & Ask"


# ------------------------------------------- the real hub_mac code, run ---


def _methods():
    tree = ast.parse(HUB.read_text(encoding="utf-8"))
    controller = next(n for n in tree.body if isinstance(n, ast.ClassDef)
                      and n.name == "HubWindowController")
    return {n.name: n for n in controller.body if isinstance(n, ast.FunctionDef)}


def hub_class(names, namespace):
    methods = _methods()
    body = []
    for name in names:
        method = methods[name]
        method.decorator_list = []
        body.append(method)
    exec(compile(ast.Module(body=body, type_ignores=[]), str(HUB), "exec"), namespace)  # noqa: S102
    return type("Hub", (), {name: namespace[name] for name in names})


class Views(list):
    """`view.subviews` in tests, `view.subviews()` in the hub code."""

    def __call__(self):
        return list(self)


class Rect:
    def __init__(self, rect):
        x, y, w, h = rect
        self.origin = SimpleNamespace(x=x, y=y)
        self.size = SimpleNamespace(width=w, height=h)


class View:
    def __init__(self, *args, rect=None, **kwargs):
        self.subviews, self.text = Views(), kwargs.get("text", "")
        self.rect, self.parent = rect, None

    def addSubview_(self, view):
        self.subviews.append(view)
        view.parent = self

    def removeFromSuperview(self):
        self.parent.subviews.remove(self)
        self.parent = None

    def frame(self):
        return Rect(self.rect)

    def setFrame_(self, rect):
        self.rect = tuple(rect)

    def setFrameOrigin_(self, point):
        self.rect = (point[0], point[1], *self.rect[2:])

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return lambda *args, **kwargs: None


class Label(View):
    def setStringValue_(self, text):
        self.text = text


class Control(View):
    """A stub control that records every message it is sent (`calls`)."""

    def __init__(self):
        super().__init__()
        self.items, self.index, self.value = [], -1, ""
        self.target = self.action = self.enabled = None
        self.calls = []

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        calls = self.__dict__.setdefault("calls", [])
        return lambda *args, **kwargs: calls.append(name)

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self.calls.append("initWithFrame_")
        self.rect = frame
        return self

    def setStringValue_(self, value):
        self.calls.append("setStringValue_")
        self.value = value

    def stringValue(self):
        return self.value

    def setTarget_(self, target):
        self.target = target

    def setAction_(self, action):
        self.action = action

    def setEnabled_(self, enabled):
        self.calls.append("setEnabled_")
        self.enabled = enabled

    def cell(self):
        return self

    def setSendsActionOnEndEditing_(self, flag):
        self.end_edit = flag


class Popup(Control):
    """NSPopUpButton's item API only: a popup draws its own selected title."""

    def initWithFrame_pullsDown_(self, frame, pulls_down):
        self.calls.append("initWithFrame_pullsDown_")
        self.rect, self.pulls_down = frame, pulls_down
        return self

    def addItemsWithTitles_(self, items):
        self.calls.append("addItemsWithTitles_")
        self.items = list(items)

    def removeAllItems(self):
        self.calls.append("removeAllItems")
        self.items, self.index = [], -1

    def selectItemAtIndex_(self, index):
        self.calls.append("selectItemAtIndex_")
        self.index = index

    def indexOfSelectedItem(self):
        return self.index


# Anything that would put a second title on a popup (or restyle its text).
OVERLAY_CALLS = {
    "setStringValue_", "setAttributedTitle_", "setTitle_", "setTextColor_",
    "setBackgroundColor_", "setEditable_", "setSelectable_", "addItemsWithObjectValues_",
}


def popup_title(popup):
    return popup.items[popup.index] if 0 <= popup.index < len(popup.items) else ""


def namespace(saved, fresh):
    def load():
        return fresh[0]

    def save(config):
        saved.append(config)

    def label(text, x, y, w, *args, height=18, **kwargs):
        return Label(text=text, rect=(x, y, w, height))

    return {
        # A combo box here would be a regression: it is not provided.
        "NSPopUpButton": Popup, "NSMakeRect": lambda *a: a,
        "NSMakePoint": lambda x, y: (x, y), "HUB_PAGE_BOTTOM": HUB_PAGE_BOTTOM,
        "FlippedView": SimpleNamespace(alloc=lambda: SimpleNamespace(
            initWithFrame_=lambda frame: View(rect=frame))),
        "style_surface": lambda *a, **k: None,
        "_label": label,
        "_font": lambda *a: None, "_color": lambda *a: None,
        "measure_text": lambda text, _font: char_measure(text),
        "CARD_PADDING": ml.CARD_PADDING, "CLEANUP_LABEL": ml.CLEANUP_LABEL,
        "note_lines": ml.note_lines,
        "provider_status_text": ml.provider_status_text,
        "quick_layout": ml.quick_layout, "cleanup_picker": ml.cleanup_picker,
        "current_speech_provider": ml.current_speech_provider,
        "STT_FIXED_MODEL": ml.STT_FIXED_MODEL,
        "speech_provider_choices": lambda config: ml.speech_provider_choices(
            config, available("faster-whisper", "mlx-whisper")),
        "speech_model_choices": ml.speech_model_choices,
        "active_preset": ml.active_preset, "set_active_model": ml.set_active_model,
        "set_speech": lambda provider, model, config, save: ml.set_speech(
            provider, model, config, available=available("faster-whisper", "mlx-whisper"),
            save=save),
        "speech_summary": ml.speech_summary,
        "ModelChoiceError": ml.ModelChoiceError, "ConfigError": ConfigError,
        "load_config": load, "save_config": save,
    }


CARD = ("_quick_state", "_quick_fill_picker", "_quick_picker", "_models_quick_card",
        "_quick_note_lines", "_quick_reflow", "_fit_body_height",
        "_quick_live", "_quick_save_cleanup", "_quick_save_speech", "quickSpeechProvider_",
        "quickSpeechModel_", "quickCleanupModel_", "quickCleanupField_", "_quick_listed")


VIEWPORT = 300


def make_hub(config, *, listing=("ready", ["gemma4:e4b", "qwen3:4b"]), width=800):
    saved, fresh = [], [config]
    cls = hub_class(CARD, namespace(saved, fresh))
    hub = cls()
    hub.config, hub.tokens, hub.cursor_y = config, {}, 0
    hub.body = View(rect=(0, 0, width, VIEWPORT))
    hub._viewport_height = lambda: VIEWPORT
    hub.page, hub._closed, hub.shown = "Models", False, []
    hub.provider_state = {}
    hub.quick_state = {"message": ("", True), "draft": None, "state": listing[0],
                       "models": list(listing[1]), "ollama_url": config.llm_base_url,
                       "token": 0, "drawn_for": None, "choices": {}}
    hub._quick_fetch = lambda force=False: None

    def button(title, action, x, y, width=150, parent=None, height=36, **_kwargs):
        made = _button(hub, title, action)
        made.rect = (x, y, width, height)
        if parent is not None:
            parent.addSubview_(made)
        return made

    hub._button = button
    hub._field = lambda value, x, y, w, parent=None, action=None: _field(hub, value, action)

    def show_page(page):
        hub.shown.append(page)
        hub.config = fresh[0] if not saved else saved[-1]
        hub._models_quick_card(width)

    hub.show_page = show_page
    hub._models_quick_card(width)
    return hub, saved, fresh


def _button(hub, title, action):
    button = Control()
    button.title = title
    button.setTarget_(hub)
    button.setAction_(action)
    return button


def _field(hub, value, action):
    field = Control()
    field.setStringValue_(value)
    field.setTarget_(hub)
    field.setAction_(action)
    return field


def test_card_controls_have_explicit_targets_actions_and_are_enabled():
    hub, _saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"))
    controls = hub.quick_controls
    expected = {"speech_provider": "quickSpeechProvider:", "speech_model": "quickSpeechModel:",
                "cleanup_model": "quickCleanupModel:", "refresh": "quickRefresh:",
                "import": "providerImport:"}
    for key, action in expected.items():
        assert controls[key].target is hub and controls[key].action == action, key
        assert controls[key].enabled is not False, key
    picker = controls["cleanup_model"]
    assert picker.items == ["gemma4:e4b", "qwen3:4b"]
    assert popup_title(picker) == "gemma4:e4b"
    assert popup_title(controls["speech_provider"]) == "Automatic"
    assert popup_title(controls["speech_model"]) == "Base"


def card_views(hub):
    """(card, its subviews) of the one Models card the hub drew last."""
    card = hub.body.subviews[-1]
    return card, card.subviews


@pytest.mark.parametrize("config", [Config(llm_model="gemma4:e4b"), Config()])
def test_choice_pickers_are_native_popups_with_one_title_renderer(config):
    """The real factory, run: every choice control is an NSPopUpButton (not
    a non-editable NSComboBox), filled only through the popup's item API,
    explicitly targeted and enabled, with no label or text laid over it."""
    hub, _saved, _fresh = make_hub(config)
    _card, views = card_views(hub)
    popups = {key: hub.quick_controls[key]
              for key in ("speech_provider", "speech_model", "cleanup_model")}
    actions = {"speech_provider": "quickSpeechProvider:",
               "speech_model": "quickSpeechModel:", "cleanup_model": "quickCleanupModel:"}
    for key, popup in popups.items():
        assert isinstance(popup, Popup), key
        assert popup.calls[0] == "initWithFrame_pullsDown_" and popup.pulls_down is False
        assert popup.target is hub and popup.action == actions[key], key
        assert popup.enabled is True, key
        assert not OVERLAY_CALLS & set(popup.calls), (key, popup.calls)
        assert popup in views, key  # a direct child of the one card
        for view in views:
            if not isinstance(view, Control) and view.rect:
                assert not overlap(view.rect, popup.rect), (key, view.text)
    # The row label is exactly "Cleanup & Ask"; the provider is a separate
    # status line (never in the label, never over the picker).
    texts = [view.text for view in views if not isinstance(view, Control)]
    assert "Cleanup & Ask" in texts and not any("·" in text for text in texts)
    assert "Active provider: Local (Ollama)" in texts


def test_empty_listing_disables_the_popup_and_shows_what_to_do():
    hub, saved, _fresh = make_hub(Config(llm_model=""), listing=("empty", []))
    picker = hub.quick_controls["cleanup_model"]
    assert picker.items == [] and picker.enabled is False and picker.index == -1
    assert picker.target is hub and picker.action == "quickCleanupModel:"
    note = hub.quick_controls["note"]
    assert "No models installed" in note.text
    hub.quickCleanupModel_(picker)  # nothing to pick: nothing saved
    assert saved == []


def test_one_models_card_and_no_summary_or_duplicate_speech_button():
    hub, _saved, _fresh = make_hub(Config())
    assert len(hub.body.subviews) == 1  # the summary card is gone, not hidden
    _card, views = card_views(hub)
    titles = [getattr(view, "title", None) for view in views]
    assert "Import keys…" in titles and "Change speech model…" not in titles
    methods = _methods()
    assert "_models_summary" not in methods and "focusSpeechModel_" not in methods
    source = HUB.read_text(encoding="utf-8")
    assert "focusSpeechModel" not in source and "Change speech model" not in source
    assert "summary_layout" not in source and not hasattr(ml, "summary_layout")
    page = ast.get_source_segment(source, methods["_page_models"])
    assert page.count("self._models_quick_card(width)") == 1


def test_picking_an_installed_model_saves_only_the_model():
    hub, saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"))
    picker = hub.quick_controls["cleanup_model"]
    picker.selectItemAtIndex_(1)
    hub.quickCleanupModel_(picker)
    assert [c.llm_model for c in saved] == ["qwen3:4b"]
    assert saved[0].llm_provider == "ollama" and hub.shown == ["Models"]
    assert hub.quick_state["message"][1] is True
    stale = picker  # the card was rebuilt: the old control is no longer live
    stale.selectItemAtIndex_(0)
    hub.quickCleanupModel_(stale)
    assert len(saved) == 1


def test_cloud_model_id_commits_on_end_of_editing_and_keeps_bad_drafts():
    config = cloud()
    hub, saved, _fresh = make_hub(config, listing=("", []))
    field = hub.quick_controls["cleanup_field"]
    assert field.action == "quickCleanupField:" and field.end_edit is True
    hub.quickCleanupField_(field)  # editing ended without a change: nothing saved
    assert saved == [] and hub.shown == []
    field.setStringValue_("gpt 4o")
    hub.quickCleanupField_(field)
    assert saved == [] and hub.quick_state["message"][1] is False
    assert hub.quick_controls["cleanup_field"].stringValue() == "gpt 4o"  # kept to fix
    field = hub.quick_controls["cleanup_field"]
    field.setStringValue_("accounts/fireworks/models/b")
    hub.quickCleanupField_(field)
    assert saved[-1].llm_model == "accounts/fireworks/models/b"
    assert saved[-1].llm_key_ref == config.llm_key_ref  # no key re-entry, same pairing


def test_speech_pickers_save_provider_and_size_together():
    hub, saved, _fresh = make_hub(Config())
    provider = hub.quick_controls["speech_provider"]
    provider.selectItemAtIndex_(provider.items.index("MLX Whisper"))
    hub.quickSpeechProvider_(provider)
    assert (saved[-1].stt_backend, saved[-1].model_size) == ("mlx-whisper", "base")
    sizes = hub.quick_controls["speech_model"]
    sizes.selectItemAtIndex_(sizes.items.index("Small"))
    hub.quickSpeechModel_(sizes)
    assert (saved[-1].stt_backend, saved[-1].model_size) == ("mlx-whisper", "small")
    assert "loads it in the background" in hub.quick_state["message"][0]


def test_listing_result_fills_the_picker_in_place_and_ignores_stale_tokens():
    hub, _saved, _fresh = make_hub(Config(llm_model="llama3.2:3b"), listing=("loading", []))
    picker = hub.quick_controls["cleanup_model"]
    note = hub.quick_controls["note"]
    assert note.text == "Loading installed models…"
    hub.quick_state["token"] = 2
    hub._quick_listed(1, ["old"], "ready")  # an older request: ignored
    assert picker.items == ["llama3.2:3b"]
    picker.calls.clear()
    hub._quick_listed(2, ["gemma4:e4b"], "ready")
    assert picker.items == ["llama3.2:3b", "gemma4:e4b"] and picker.enabled is True
    # The configured model stays selected; same popup, same target/action;
    # only the popup's item API is used (no second title drawn over it).
    assert popup_title(picker) == "llama3.2:3b"
    assert hub.quick_controls["cleanup_model"] is picker
    assert picker.target is hub and picker.action == "quickCleanupModel:"
    assert not OVERLAY_CALLS & set(picker.calls), picker.calls
    assert hub.shown == []  # filled in place, never a rebuild
    # The longest result fits the room measured while loading (no clipping).
    lines = ml.note_lines([note.text], note.rect[2], char_measure)
    assert note.text.startswith("llama3.2:3b isn't installed")
    assert lines * ml.MESSAGE_LINE == note.rect[3]  # reflowed to the result exactly
    hub._closed = True
    hub.quick_state["token"] = 3
    hub._quick_listed(3, [], "empty")
    assert picker.items == ["llama3.2:3b", "gemma4:e4b"]


def test_listing_failure_keeps_the_selection_and_disables_nothing_it_can_pick():
    hub, _saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"), listing=("loading", []))
    picker = hub.quick_controls["cleanup_model"]
    hub.quick_state["token"] = 1
    hub._quick_listed(1, [], "unavailable")
    assert picker.items == ["gemma4:e4b"] and popup_title(picker) == "gemma4:e4b"
    assert picker.enabled is True and hub.quick_controls["refresh"].enabled is True
    assert "isn't answering" in hub.quick_controls["note"].text
    # A save message wins over a later listing note.
    hub.quick_state["message"] = ("Cleanup & Ask now use gemma4:e4b; no restart needed.", True)
    hub.quick_state["token"] = 2
    note_before = hub.quick_controls["note"].text
    hub._quick_listed(2, ["gemma4:e4b"], "ready")
    assert hub.quick_controls["note"].text == note_before


def test_settled_card_reserves_no_blank_note_lines():
    hub, _saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"))
    assert "note" not in hub.quick_controls  # nothing to say: no empty band
    loading, _saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"), listing=("loading", []))
    assert "note" in loading.quick_controls


def loading_page(config, width=800):
    """A cold open: the card is still listing, the rest of the page below it
    (an open provider editor with a typed, unsaved key, then storage)."""
    hub, saved, _fresh = make_hub(config, listing=("loading", []), width=width)
    card = hub.body.subviews[-1]
    top = card.rect[1] + card.rect[3] + 12
    assert hub.cursor_y == top
    editor = View(rect=(0, top, width, 400))
    key = Control()
    key.setStringValue_("sk-typed-not-saved")
    editor.addSubview_(key)
    storage = View(rect=(0, top + 412, width, 120))
    for view in (editor, storage):
        hub.body.addSubview_(view)
    hub.cursor_y = top + 412 + 120 + 12
    hub._fit_body_height()
    hub.quick_state["token"] = 1
    return SimpleNamespace(hub=hub, saved=saved, card=card, editor=editor, key=key,
                           storage=storage, width=width)


def card_label(card, prefix):
    return next(v for v in card.subviews if isinstance(v, Label) and v.text.startswith(prefix))


def assert_reflowed(page, before, delta):
    """Later views moved by `delta`; cursor and document height follow;
    the same editor (and its typed key) is still there; no rebuild."""
    hub = page.hub
    assert page.editor.rect[1] == before["editor"] + delta
    assert page.storage.rect[1] == before["storage"] + delta
    assert hub.cursor_y == before["cursor"] + delta
    bottom = page.storage.rect[1] + page.storage.rect[3]
    assert hub.body.rect[3] == max(bottom + HUB_PAGE_BOTTOM, VIEWPORT)
    assert hub.body.subviews == [page.card, page.editor, page.storage]  # same views
    assert page.editor.subviews == [page.key]
    assert page.key.stringValue() == "sk-typed-not-saved"
    assert hub.shown == [] and page.saved == []


def snapshot(page):
    return {"editor": page.editor.rect[1], "storage": page.storage.rect[1],
            "cursor": page.hub.cursor_y, "height": page.card.rect[3]}


def test_cold_open_listing_shrinks_the_card_to_no_note():
    page = loading_page(Config(llm_model="gemma4:e4b"))
    hub, card = page.hub, page.card
    picker = hub.quick_controls["cleanup_model"]
    note = hub.quick_controls["note"]
    before = snapshot(page)
    hub._quick_listed(1, ["gemma4:e4b", "qwen3:4b"], "ready")
    bare = ml.quick_layout(page.width, ollama=True, fixed_speech=False)["height"]
    delta = bare - before["height"]
    assert delta == -(ml.MODELS_ROW_GAP + ml.MESSAGE_LINE)
    assert card.rect[3] == bare
    status = card_label(card, "Active provider")
    assert card.rect[3] == status.rect[1] + status.rect[3] + ml.CARD_PADDING  # exact padding
    assert "note" not in hub.quick_controls and note not in card.subviews
    assert hub.quick_controls["cleanup_model"] is picker and popup_title(picker) == "gemma4:e4b"
    assert picker.target is hub and picker.action == "quickCleanupModel:" and picker.enabled
    assert_reflowed(page, before, delta)


@pytest.mark.parametrize("result", ["unavailable", "empty"])
def test_cold_open_listing_failure_grows_the_card_to_the_wrapped_note(result):
    page = loading_page(Config(llm_model="gemma4:e4b"), width=300)
    hub, card = page.hub, page.card
    note = hub.quick_controls["note"]
    before = snapshot(page)
    hub._quick_listed(1, [], result)
    text = ml.cleanup_picker(hub.config, result, [])[2]
    inner = page.width - 2 * ml.CARD_PADDING
    lines = len(wrap_lines(text, inner, char_measure))
    assert lines > 1  # wrapped
    frames = ml.quick_layout(page.width, ollama=True, fixed_speech=False, message_lines=lines)
    assert hub.quick_controls["note"] is note and note.text == text  # same label
    assert note.rect == frames["message"] and note.rect[3] == lines * ml.MESSAGE_LINE
    assert card.rect[3] == frames["height"] == note.rect[1] + note.rect[3] + ml.CARD_PADDING
    assert_reflowed(page, before, frames["height"] - before["height"])
    assert frames["height"] > before["height"]

    # Refresh, then Ollama answers: the same card shrinks back to no note.
    hub.quick_state["token"] = 2
    before = snapshot(page)
    hub._quick_listed(2, ["gemma4:e4b"], "ready")
    bare = ml.quick_layout(page.width, ollama=True, fixed_speech=False)["height"]
    assert card.rect[3] == bare and "note" not in hub.quick_controls
    assert_reflowed(page, before, bare - before["height"])


def test_reflow_after_the_page_is_gone_does_nothing():
    page = loading_page(Config(llm_model="gemma4:e4b"))
    before = snapshot(page)
    page.hub.quick_geometry = None  # torn down (show_page)
    page.hub._quick_listed(1, ["gemma4:e4b"], "ready")
    assert page.card.rect[3] == before["height"]
    assert_reflowed(page, before, 0)


def test_typed_cloud_model_id_survives_rebuild_without_saving():
    capture = ast.get_source_segment(HUB.read_text(encoding="utf-8"),
                                     _methods()["_models_capture"])
    assert 'get("cleanup_field")' in capture and '["draft"] = typed' in capture
    assert "set_active_model" not in capture  # captured, never saved per keystroke


def test_listing_runs_off_the_main_thread_and_nothing_else_blocks():
    source = HUB.read_text(encoding="utf-8")
    methods = _methods()
    fetch = ast.get_source_segment(source, methods["_quick_fetch"])
    assert "threading.Thread" in fetch and "ollama_models(url)" in fetch
    assert "NSOperationQueue.mainQueue().addOperationWithBlock_" in fetch
    card = ast.get_source_segment(source, methods["_models_quick_card"])
    assert "ollama_models" not in card and "build_stt" not in card
    assert "self._models_quick_card(width)" in ast.get_source_segment(
        source, methods["_page_models"])
    # Choice-only pickers are native popups: no combo box, no string value.
    for name in ("_quick_picker", "_quick_fill_picker", "_quick_listed"):
        method = ast.get_source_segment(source, methods[name])
        assert "NSComboBox" not in method and "setStringValue_" not in method, name
        assert "addItemsWithObjectValues_" not in method, name
    assert "NSPopUpButton.alloc().initWithFrame_pullsDown_(" in ast.get_source_segment(
        source, methods["_quick_picker"])
