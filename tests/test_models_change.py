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
    assert same is config and len(saved) == 2


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


class View:
    def __init__(self, *args, **kwargs):
        self.subviews, self.text = [], kwargs.get("text", "")

    def addSubview_(self, view):
        self.subviews.append(view)

    def __getattr__(self, name):
        return lambda *args, **kwargs: None


class Picker(View):
    def __init__(self):
        super().__init__()
        self.items, self.index, self.value = [], -1, ""
        self.target = self.action = self.enabled = None
        self.editable = None

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self.frame = frame
        return self

    def addItemsWithObjectValues_(self, items):
        self.items = list(items)

    def removeAllItems(self):
        self.items = []

    def selectItemAtIndex_(self, index):
        self.index = index

    def indexOfSelectedItem(self):
        return self.index

    def setStringValue_(self, value):
        self.value = value

    def stringValue(self):
        return self.value

    def setTarget_(self, target):
        self.target = target

    def setAction_(self, action):
        self.action = action

    def setEnabled_(self, enabled):
        self.enabled = enabled

    def setEditable_(self, editable):
        self.editable = editable

    def cell(self):
        return self

    def setSendsActionOnEndEditing_(self, flag):
        self.end_edit = flag


def namespace(saved, fresh):
    def load():
        return fresh[0]

    def save(config):
        saved.append(config)

    return {
        "NSComboBox": Picker, "NSMakeRect": lambda *a: a,
        "FlippedView": SimpleNamespace(alloc=lambda: SimpleNamespace(
            initWithFrame_=lambda frame: View())),
        "style_surface": lambda *a, **k: None,
        "_label": lambda text, *a, **k: View(text=text),
        "_font": lambda *a: None, "_color": lambda *a: None,
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


CARD = ("_quick_state", "_quick_picker", "_models_quick_card", "_quick_live",
        "_quick_save_cleanup", "_quick_save_speech", "quickSpeechProvider_",
        "quickSpeechModel_", "quickCleanupModel_", "quickCleanupField_", "_quick_listed")


def make_hub(config, *, listing=("ready", ["gemma4:e4b", "qwen3:4b"])):
    saved, fresh = [], [config]
    cls = hub_class(CARD, namespace(saved, fresh))
    hub = cls()
    hub.config, hub.tokens, hub.cursor_y, hub.body = config, {}, 0, View()
    hub.page, hub._closed, hub.shown = "Models", False, []
    hub.quick_state = {"message": ("", True), "draft": None, "state": listing[0],
                       "models": list(listing[1]), "ollama_url": config.llm_base_url,
                       "token": 0, "drawn_for": None, "choices": {}}
    hub._quick_fetch = lambda force=False: None
    hub._button = lambda title, action, x, y, **k: _button(hub, title, action)
    hub._field = lambda value, x, y, w, parent=None, action=None: _field(hub, value, action)

    def show_page(page):
        hub.shown.append(page)
        hub.config = fresh[0] if not saved else saved[-1]
        hub._models_quick_card(800)

    hub.show_page = show_page
    hub._models_quick_card(800)
    return hub, saved, fresh


def _button(hub, title, action):
    button = Picker()
    button.title = title
    button.setTarget_(hub)
    button.setAction_(action)
    return button


def _field(hub, value, action):
    field = Picker()
    field.setStringValue_(value)
    field.setTarget_(hub)
    field.setAction_(action)
    return field


def test_card_controls_have_explicit_targets_actions_and_are_enabled():
    hub, _saved, _fresh = make_hub(Config(llm_model="gemma4:e4b"))
    controls = hub.quick_controls
    expected = {"speech_provider": "quickSpeechProvider:", "speech_model": "quickSpeechModel:",
                "cleanup_model": "quickCleanupModel:", "refresh": "quickRefresh:"}
    for key, action in expected.items():
        assert controls[key].target is hub and controls[key].action == action, key
        assert controls[key].enabled is not False, key
    picker = controls["cleanup_model"]
    assert picker.editable is False and picker.items == ["gemma4:e4b", "qwen3:4b"]
    assert picker.items[picker.index] == "gemma4:e4b"
    assert controls["speech_provider"].items[controls["speech_provider"].index] == "Automatic"


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
    hub.quick_state["token"] = 2
    hub._quick_listed(1, ["old"], "ready")  # an older request: ignored
    assert picker.items == ["llama3.2:3b"]
    hub._quick_listed(2, ["gemma4:e4b"], "ready")
    assert picker.items == ["llama3.2:3b", "gemma4:e4b"] and picker.enabled is True
    assert hub.shown == []  # filled in place, never a rebuild
    hub._closed = True
    hub.quick_state["token"] = 3
    hub._quick_listed(3, [], "empty")
    assert picker.items == ["llama3.2:3b", "gemma4:e4b"]


def test_legacy_change_speech_button_routes_to_the_direct_picker():
    source = HUB.read_text(encoding="utf-8")
    summary = ast.get_source_segment(source, _methods()["_models_summary"])
    assert '("Change speech model…", "focusSpeechModel:", "change")' in summary
    assert "openSetup" not in summary

    hub, _saved, _fresh = make_hub(Config())
    focus = hub_class(("focusSpeechModel_",), {})
    picker = hub.quick_controls["speech_provider"]
    card = SimpleNamespace(frame=lambda: ("card-frame",))
    picker.superview = lambda: card
    scrolled, focused = [], []
    target = focus()
    target.quick_controls = hub.quick_controls
    target.body = SimpleNamespace(scrollRectToVisible_=scrolled.append)
    target.window = SimpleNamespace(makeFirstResponder_=focused.append)
    target.focusSpeechModel_(None)
    assert scrolled == [("card-frame",)] and focused == [picker]
    target.quick_controls = {}  # page not built: nothing to do, no crash
    target.focusSpeechModel_(None)
    assert focused == [picker]


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
    assert "NSPopUpButton" not in source  # pickers are non-editable combo boxes
