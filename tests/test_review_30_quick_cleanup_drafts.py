"""PR30 review finding 2: a quick cleanup-model save must reach the open editor.

`_quick_save_cleanup` saves the new model and writes it into the active
provider's draft (`provider_state[preset]["model"]`), then calls
`show_page("Models")`. The real `show_page` first runs `_models_capture()`,
which reads the *still-live* provider editor combo (showing the old model) and
overwrites that draft again. The rebuilt editor then shows the old model, and
the next "Test connection" tests the old model.

The real hub_mac methods (quick card, provider list, inline editor, capture,
Test connection action and worker) are AST-extracted and run against stub
controls (as in test_models_change); `show_page` is emulated exactly as the
real one handles the Models page (capture, drop controls, rebuild), and that
order is checked against the real source. No AppKit is imported.
"""

import ast
import sys
from dataclasses import replace
from types import SimpleNamespace

import pytest

# Sibling test module (pytest puts tests/ on sys.path): the same AST helper and
# stub controls the Change models card tests use.
from test_models_change import (
    CARD,
    HUB,
    VIEWPORT,
    Control,
    View,
    _methods,
    hub_class,
    namespace,
    popup_title,
)

from localflow import models_layout as ml
from localflow import provider_keys as keys
from localflow.config import Config
from localflow.ui_tokens import MODELS_ROW_HEIGHT, PILL_HEIGHT

FIREWORKS = keys.get_preset("fireworks")
OLLAMA = keys.get_preset("ollama")
CUSTOM_URL = "https://llm.example.test/v1"
TYPED_URL = "https://typed.example.test/v1"
WIDTH = 800
METHODS = CARD + (
    "_models_capture", "_models_state", "_models_provider_list", "_models_editor",
    "_models_apply_use", "_models_worker", "_models_action", "providerTest_",
)


class Layered(View):
    """A FlippedView/NSView stub whose `layer()` accepts styling messages."""

    def __init__(self, *args, rect=None, **kwargs):
        super().__init__(rect=rect)
        self._layer = Control()

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self.rect = tuple(frame)
        return self

    def layer(self):
        return self._layer


class Combo(Control):
    """The provider editor's editable NSComboBox (item list + typed text)."""

    def addItemsWithObjectValues_(self, items):
        self.items = list(items)

    def removeAllItems(self):
        self.items = []


class InlineThread:
    def __init__(self, target, **_kwargs):
        self.target = target

    def start(self):
        self.target()


@pytest.fixture
def env(monkeypatch):
    """In-memory secrets (every write recorded), fake provider network calls,
    a fake Foundation main queue (blocks are kept, not run)."""
    store, writes, tested, main_queue = {}, [], [], []
    monkeypatch.setattr(keys.secrets, "get_secret", lambda name: store.get(name, ""))
    monkeypatch.setattr(keys.secrets, "set_secret",
                        lambda name, value: writes.append(("set", name)))
    monkeypatch.setattr(keys.secrets, "delete_secret",
                        lambda name: writes.append(("delete", name)))
    monkeypatch.setattr(keys, "fetch_models", lambda provider_id, base_url=None: [])

    def test_llm(provider_id, *, base_url=None, model=""):
        tested.append((provider_id, base_url, model))
        return True, "Provider connection successful"

    monkeypatch.setattr(keys, "test_llm", test_llm)
    queue = SimpleNamespace(addOperationWithBlock_=main_queue.append)
    monkeypatch.setitem(sys.modules, "Foundation", SimpleNamespace(
        NSOperationQueue=SimpleNamespace(mainQueue=lambda: queue)))
    return SimpleNamespace(store=store, writes=writes, tested=tested)


def models_page(config, *, expanded, drafts, listing=("", [])):
    """The Models page with `expanded` provider's inline editor open, built by
    the real methods; `show_page("Models")` rebuilds it like the real hub."""
    disk = SimpleNamespace(configs=[config], saved=[], fail=False)

    def load():
        return disk.configs[-1]

    def save(updated):
        if disk.fail:
            raise OSError("read-only settings")
        disk.saved.append(updated)
        disk.configs.append(updated)

    ns = namespace([], [config])
    ns.update({
        "load_config": load, "save_config": save,
        "FlippedView": Layered, "NSView": Layered, "NSComboBox": Combo,
        "AppKit": SimpleNamespace(NSSecureTextField=Control),
        "threading": SimpleNamespace(Thread=InlineThread),
        "_color": lambda *a, **k: Control(),
        "make_pill": lambda *a, **k: Layered(rect=(0, 0, 60, PILL_HEIGHT)),
        "MODELS_ROW_HEIGHT": MODELS_ROW_HEIGHT, "PILL_HEIGHT": PILL_HEIGHT,
        "row_columns": ml.row_columns, "list_layout": ml.list_layout,
        "provider_status": ml.provider_status, "editor_layout": ml.editor_layout,
        "action_frames": ml.action_frames, "use_button_visible": ml.use_button_visible,
        "is_active": ml.is_active,
    })
    hub = hub_class(METHODS, ns)()
    hub.config, hub.tokens, hub.page, hub._closed = config, {}, "Models", False
    hub.shown = []
    hub._viewport_height = lambda: VIEWPORT
    hub._quick_fetch = lambda force=False: None
    hub.quick_state = {"message": ("", True), "draft": None, "state": listing[0],
                       "models": list(listing[1]), "ollama_url": config.llm_base_url,
                       "token": 0, "drawn_for": None, "choices": {}}
    hub.provider_state = drafts
    hub.provider_expanded = expanded
    hub.provider_checks, hub.provider_candidates = [], []

    def button(title, action, x, y, width=150, parent=None, height=36, tag=None, **_kw):
        made = Control()
        made.title, made.rect = title, (x, y, width, height)
        made.tag = lambda value=tag: value
        made.setTarget_(hub)
        made.setAction_(action)
        if parent is not None:
            parent.addSubview_(made)
        return made

    def field(value, x, y, w, parent=None, action=None):
        made = Control()
        made.rect = (x, y, w, 22)
        made.setStringValue_(value)
        made.setTarget_(hub)
        made.setAction_(action)
        if parent is not None:
            parent.addSubview_(made)
        return made

    hub._button, hub._field = button, field

    def draw():
        # As _page_models: reload the config, then the card and provider list.
        hub.config = load()
        hub.body, hub.cursor_y = Layered(rect=(0, 0, WIDTH, VIEWPORT)), 0
        hub.provider_controls, hub.provider_action_views = {}, None
        statuses = {p.id: keys.key_status(p.id) for p in keys.PRESETS}
        hub._models_quick_card(WIDTH)
        hub._models_provider_list(WIDTH, statuses)

    def show_page(page):
        # The Models part of the real show_page (see
        # test_show_page_emulation_matches_the_real_capture_order).
        hub.shown.append(page)
        if hub.page == "Models" and hasattr(hub, "provider_controls"):
            hub._models_capture()
            hub.provider_controls = {}
            hub.provider_checks = []
            hub.provider_action_views = None
            hub.quick_controls = {}
            hub.quick_geometry = None
        hub.page = page
        draw()

    hub.show_page = show_page
    draw()
    return hub, disk


def editor(hub, provider_id):
    url, combo, secure = hub.provider_controls[provider_id]
    return SimpleNamespace(url=url, combo=combo, secure=secure)


def press_test_connection(hub):
    hub.providerTest_(hub.provider_action_views["test"])


def test_show_page_emulation_matches_the_real_capture_order():
    """The emulated show_page above captures, then drops the controls, then
    rebuilds - the same order as the real one."""
    source = ast.get_source_segment(HUB.read_text(encoding="utf-8"),
                                    _methods()["show_page"])
    block = source.split('if self.page == "Models" and hasattr(self, "provider_controls"):',
                         1)[1]
    assert block.index("self._models_capture()") < block.index("self.provider_controls = {}")
    assert block.index("self.provider_controls = {}") < block.index("self.quick_controls = {}")


def test_ollama_popup_pick_reaches_the_open_editor_and_test_connection(env):
    config = Config(llm_model="gemma4:e4b")
    assert ml.active_preset(config) is OLLAMA
    other = {"url": TYPED_URL, "model": "draft-model", "models": []}
    hub, disk = models_page(
        config, expanded="ollama", drafts={"custom": dict(other)},
        listing=("ready", ["gemma4:e4b", "qwen3:4b"]),
    )
    assert editor(hub, "ollama").combo.stringValue() == "gemma4:e4b"

    picker = hub.quick_controls["cleanup_model"]
    picker.selectItemAtIndex_(picker.items.index("qwen3:4b"))
    hub.quickCleanupModel_(picker)

    assert [c.llm_model for c in disk.saved] == ["qwen3:4b"]
    assert hub.shown == ["Models"]
    assert popup_title(hub.quick_controls["cleanup_model"]) == "qwen3:4b"
    # The quick card and the active provider's editor agree after the rebuild.
    assert hub.provider_state["ollama"]["model"] == "qwen3:4b"
    assert editor(hub, "ollama").combo.stringValue() == "qwen3:4b"
    assert hub.provider_state["custom"] == other  # another provider's draft untouched
    assert env.writes == []

    press_test_connection(hub)
    assert env.tested == [("ollama", OLLAMA.base_url, "qwen3:4b")]
    assert [c.llm_model for c in disk.saved] == ["qwen3:4b"]  # testing saves nothing


def custom_config(model="old-model"):
    return replace(Config(), llm_provider="openai-compatible", llm_base_url=CUSTOM_URL,
                   llm_model=model, llm_key_ref="")


def custom_page(env):
    env.store.update({"provider_key_custom": "fake-key-custom",
                      "llm_api_key": "fake-key-custom"})
    config = custom_config()
    assert ml.active_preset(config).id == "custom"
    fireworks = {"url": FIREWORKS.base_url, "model": "accounts/fireworks/models/draft",
                 "models": []}
    hub, disk = models_page(config, expanded="custom", drafts={"fireworks": dict(fireworks)})
    open_editor = editor(hub, "custom")
    assert open_editor.combo.stringValue() == "old-model"
    assert open_editor.url.stringValue() == CUSTOM_URL
    # Unsaved edits in the open editor: a new endpoint and a pasted key.
    open_editor.url.setStringValue_(TYPED_URL)
    open_editor.secure.setStringValue_("sk-typed-not-saved")
    return hub, disk, config, fireworks


def test_cloud_model_field_reaches_the_open_editor_and_keeps_its_drafts(env):
    hub, disk, config, fireworks = custom_page(env)
    field = hub.quick_controls["cleanup_field"]
    field.setStringValue_("new-model")
    hub.quickCleanupField_(field)

    assert [c.llm_model for c in disk.saved] == ["new-model"]
    saved = disk.saved[-1]
    # Only the model moved: endpoint and key pairing are the saved ones.
    assert (saved.llm_provider, saved.llm_base_url, saved.llm_key_ref) == (
        config.llm_provider, config.llm_base_url, config.llm_key_ref)
    assert hub.quick_controls["cleanup_field"].stringValue() == "new-model"
    state = hub.provider_state["custom"]
    assert state["model"] == "new-model"
    assert state["url"] == TYPED_URL  # the typed endpoint draft survives
    rebuilt = editor(hub, "custom")
    assert rebuilt.combo.stringValue() == "new-model"
    assert rebuilt.url.stringValue() == TYPED_URL
    assert hub.provider_state["fireworks"] == fireworks
    assert env.writes == []  # no key saved, removed or re-paired
    assert keys.key_status("custom")["has_key"] is True

    press_test_connection(hub)
    assert env.tested == [("custom", TYPED_URL, "new-model")]


def test_failed_or_stale_quick_save_leaves_provider_drafts_alone(env):
    hub, disk, _config, fireworks = custom_page(env)
    editor(hub, "custom").combo.setStringValue_("editor-draft")
    stale = hub.quick_controls["cleanup_field"]  # live now; removed by the rebuild

    disk.fail = True  # the settings file can't be written
    stale.setStringValue_("new-model")
    hub.quickCleanupField_(stale)
    assert disk.saved == [] and hub.quick_state["message"][1] is False
    assert hub.quick_controls["cleanup_field"].stringValue() == "new-model"  # kept to retry
    state = hub.provider_state["custom"]
    assert (state["url"], state["model"]) == (TYPED_URL, "editor-draft")
    assert editor(hub, "custom").combo.stringValue() == "editor-draft"

    # A late action from the removed field is ignored entirely.
    disk.fail = False
    stale.setStringValue_("other-model")
    hub.quickCleanupField_(stale)
    assert disk.saved == []
    assert (state["url"], state["model"]) == (TYPED_URL, "editor-draft")
    assert hub.provider_state["fireworks"] == fireworks
    assert env.writes == []
