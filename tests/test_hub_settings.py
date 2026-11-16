"""Hub Settings page: state, layout, wiring and worker lifecycle (no AppKit).

The real SettingsPageController (hub_settings_mac.py) runs against stub
AppKit/objc/Foundation/mac_style modules, so preview workers, stale results,
navigation, resize and window close are exercised for real; only view
construction is simulated.
"""

import ast
import contextlib
import importlib.util
import itertools
import sys
import threading
import types
from pathlib import Path
from typing import ClassVar

import pytest

from localflow import hub_route, hub_settings
from localflow.config import Config
from localflow.settings_agent import (
    ApplyResult,
    Proposal,
    SettingsAgentError,
    UndoResult,
    setting_change,
)
from localflow.ui_tokens import (
    HUB_CONTENT_GUTTER,
    HUB_CONTENT_X,
    HUB_HEADER_OFFSET,
    HUB_MIN_WIDTH,
)

ROOT = Path(__file__).parent.parent
SETTINGS_MAC = ROOT / "localflow" / "hub_settings_mac.py"
HUB_MAC = ROOT / "localflow" / "hub_mac.py"
MIN_CONTENT = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER
WIDE_CONTENT = 1280 - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER


def measure(text):
    return len(text) * 7


def proposal(model="gemma4:e4b"):
    return Proposal(
        "turn off sounds", model, (setting_change("sounds_enabled", True, False),),
        ("Server port is not a setting LocalFlow can change here",),
    )


# ------------------------------------------------------------------ state ---


def test_preview_tokens_drop_stale_results():
    state = hub_settings.SettingsPageState()
    assert state.begin_preview("   ") is None and state.status == "empty"
    first = state.begin_preview("turn off sounds")
    assert state.busy and state.pending == "turn off sounds"
    assert state.begin_preview("again") is None  # one request at a time

    state.cancel()
    assert state.finish_preview(first, proposal()) is False  # cancelled: ignored
    assert state.status == "empty"

    second = state.begin_preview("turn off sounds")
    assert second != first
    assert state.finish_preview(second, proposal()) is True
    assert state.status == "preview"
    assert state.lines == ["Sounds: On → Off"]
    assert state.notes == ["Server port is not a setting LocalFlow can change here"]
    assert state.model == "gemma4:e4b"
    assert hub_settings.model_note(state.model) == "Runs on your Mac with gemma4:e4b"
    assert state.finish_preview(second, proposal()) is False  # delivered once


def test_error_applied_and_undone_states():
    state = hub_settings.SettingsPageState()
    token = state.begin_preview("x")
    state.finish_preview(token, error="Couldn't reach Ollama", model=None)
    assert (state.status, state.message) == ("error", "Couldn't reach Ollama")
    assert state.draft == "x"  # kept so the user can retry

    token = state.begin_preview("x")
    state.finish_preview(token, proposal())
    change = state.proposal.changes[0]
    state.applied(ApplyResult((change,), "Applied 1 change."))
    assert (state.status, state.message, state.draft) == ("applied", "Applied 1 change.", "")
    assert state.lines == ["Sounds: On → Off"]

    state.undone(UndoResult(("Sounds: On",), ("word “x”",), "Undid the last change."))
    assert state.status == "applied" and state.notes == ["word “x”"]
    assert hub_settings.model_note(None) == hub_settings.NOTE_UNKNOWN


@pytest.mark.parametrize("width", [MIN_CONTENT, WIDE_CONTENT])
@pytest.mark.parametrize("status", ["empty", "busy", "preview", "applied", "error"])
def test_layout_fits_and_never_overlaps(width, status):
    state = hub_settings.SettingsPageState()
    if status != "empty":
        token = state.begin_preview("turn off sounds")
        if status == "preview":
            state.finish_preview(token, proposal())
        elif status == "error":
            state.finish_preview(token, error="A long error " * 12)
        elif status == "applied":
            state.finish_preview(token, proposal())
            state.applied(ApplyResult(state.proposal.changes, "Applied 1 change."))
    state.undo_lines = ["Sounds: Off → On"]
    layout = hub_settings.settings_layout(width, HUB_HEADER_OFFSET, state, 8, measure)

    assert layout["wide"] is (width >= hub_settings.WIDE_MIN)
    stack = [layout[k] for k in ("box_label", "intro", "box", "preview", "card", "undo")]
    for upper, lower in itertools.pairwise(stack):
        assert upper[1] + upper[3] <= lower[1], (upper, lower)
    for frame in stack + [layout["current"]]:
        x, y, w, h = frame
        assert x >= 0 and x + w <= width and y >= HUB_HEADER_OFFSET and h >= 0
    if layout["wide"]:
        assert layout["current"][0] >= layout["main_w"]  # its own column
    else:
        assert layout["current"][1] >= layout["undo"][1] + layout["undo"][3]
    assert layout["bottom"] >= layout["current"][1] + layout["current"][3]
    assert ("apply" in layout) is (status == "preview")
    card_h = layout["card"][3]
    for x, y, w, h in layout["rows"]:
        assert y + h <= card_h and x + w <= layout["main_w"]


@pytest.mark.parametrize("width", [MIN_CONTENT, WIDE_CONTENT])
@pytest.mark.parametrize("status", ["preview", "applied"])
def test_long_change_lines_wrap_in_full_and_short_rows_keep_their_size(width, status):
    from localflow.settings_agent import replacement_change
    from localflow.text_wrap import wrap_lines

    long_change = replacement_change("ü" * 10, "Ünïcødé ẞtraße — " * 5 + "x" * 15)
    assert len(long_change.value[1]) == 100
    short = setting_change("sounds_enabled", True, False)
    state = hub_settings.SettingsPageState()
    token = state.begin_preview("x")
    state.finish_preview(token, Proposal("x", "m", (short, long_change)))
    if status == "applied":
        state.applied(ApplyResult((short, long_change), "Applied 2 changes."))
    layout = hub_settings.settings_layout(width, HUB_HEADER_OFFSET, state, 8, measure)

    (_sx, sy, sw, sh), (_lx, ly, lw, lh) = layout["rows"][:2]
    inner = layout["main_w"] - 2 * hub_settings.CARD_PAD
    assert sh == hub_settings.ROW_H and sw == lw == inner  # short row unchanged
    lines = wrap_lines(long_change.line(), inner, measure)
    assert len(lines) > 1 and "".join(lines).replace(" ", "") == (
        long_change.line().replace(" ", "")
    )  # every character is shown somewhere
    assert lh >= len(lines) * hub_settings.LINE_H
    assert ly == sy + sh  # stacked, no overlap
    card_h = layout["card"][3]
    assert ly + lh <= card_h
    if status == "preview":
        assert layout["apply"][1] >= ly + lh  # Apply sits below the full change


def test_native_change_lines_wrap_instead_of_truncating():
    source = SETTINGS_MAC.read_text(encoding="utf-8")
    assert 'self._label(card, line, frame, 13, "ink", weight="semibold", wrap=True)' in source
    assert 'font(13, "semibold")' in source  # rows measured with the font they draw in


def test_copy():
    assert hub_settings.HEADING == "Tell LocalFlow what to change"
    assert "Turn off sounds" in hub_settings.PLACEHOLDER


# ------------------------------------------------------------ page wiring ---


def test_settings_page_follows_models_everywhere():
    from localflow import hub_window

    assert hub_route.HUB_PAGES[-2:] == ("Models", "Settings")
    assert hub_route.validate_page("Settings") == "Settings"
    assert hub_route.validate_page("settings") is None  # requests stay strict
    assert hub_route.canonical_page("settings") == "Settings"
    assert hub_route.canonical_page(" hot  PHRASES ") == "Hot Phrases"
    assert hub_route.canonical_page("nope") is None
    assert "canonical_page" in Path(hub_window.__file__).read_text(encoding="utf-8")


def _hub_method(name):
    source = HUB_MAC.read_text(encoding="utf-8")
    cls = next(
        node for node in ast.parse(source).body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    node = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == name)
    return ast.get_source_segment(source, node)


def test_hub_captures_settings_before_teardown_and_shuts_it_down_on_close():
    show = _hub_method("show_page")
    capture = show.index("self.settings_page.capture()")
    assert capture < show.index("view.removeFromSuperview()")
    assert "self.settings_page.shutdown()" in _hub_method("windowWillClose_")
    page = _hub_method("_page_settings")
    assert 'hasattr(self, "settings_page")' in page and ".render()" in page


def test_native_controller_uses_objc_super_and_main_thread_delivery():
    source = SETTINGS_MAC.read_text(encoding="utf-8")
    tree = ast.parse(source)
    builtin_super = [
        node for node in ast.walk(tree)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
        and node.func.id == "super"
    ]
    assert builtin_super == []
    assert "objc.super(SettingsPageController, self).init()" in source
    assert "performSelectorOnMainThread_withObject_waitUntilDone_" in source
    assert "finish_preview(token" in source
    # The 28pt bold heading is the Hub's shared page title (show_page ->
    # page_title(sidebar_label(page))), so this module draws no title of its own.
    assert "page_title" not in source


def test_models_page_deletion_rechecks_the_config_and_confirms():
    delete = _hub_method("deleteSpeechModel_")
    assert "load_config()" in delete and "delete_unused(" in delete
    assert "runModal()" in delete and "NSAlertFirstButtonReturn" in delete
    assert "_models_storage_card(width)" in _hub_method("_page_models")


def test_snapshots_cover_settings_and_storage_offline_and_isolated():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text(encoding="utf-8")
    for state in ("empty", "preview", "applied", "error"):
        assert f'"hub-settings-{state}"' in snapshots
    assert '"hub-models-storage"' in snapshots
    assert "_hub_sized_shots(controller, \"hub-settings-preview\")" in snapshots
    assert "session=_OfflineSession()" in snapshots  # never reaches Ollama
    assert 'config_path = folder / "config.toml"' in snapshots  # temp config only
    assert 'patch(\n            "localflow.model_storage.hf_hub_cache"' in snapshots
    assert "handle.truncate(size)" in snapshots  # sparse fake models
    # The request is typed into the live box and captured, never set on the
    # state behind a stale field, and the box is checked in each state.
    settings = snapshots.split("def _hub_settings_snapshots", 1)[1].split("\ndef ", 1)[0]
    assert "page.state.draft = " not in settings  # no direct assignment
    assert "page.text_view.setString_(text)" in settings and "page.capture()" in settings
    assert 'assert shown_request() == request, "preview must show the typed request"' in settings
    assert 'assert shown_request() == "", "applied state starts with an empty box"' in settings
    assert 'assert shown_request() == "Make it purple"' in settings


def test_snapshot_typing_sequence_keeps_the_box_honest(settings_mac):
    """The fixture's exact sequence on the real controller (stub AppKit)."""
    controller, hub, _agent = _page(settings_mac)
    page = controller
    request = "Turn off sounds, learn Kubernetes"
    page.text_view.setString_(request)
    page.capture()
    token = page.state.begin_preview(page.state.draft)
    page.state.finish_preview(token, proposal())
    hub._rebuild_current_page()
    assert page.text_view.string() == request

    page.capture()
    page.state.applied(ApplyResult(page.state.proposal.changes, "Applied 1 change."))
    hub._rebuild_current_page()
    assert page.text_view.string() == ""
    hub._rebuild_current_page()  # a resize
    assert page.text_view.string() == ""


# --------------------------------------------------- controller lifecycle ---


class Stub:
    """Any AppKit object: remembers subviews, accepts any other call."""

    def __init__(self, *args, **kwargs):
        self.subviews = []

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self.frame_ = frame
        return self

    def addSubview_(self, view):
        self.subviews.append(view)

    def __getattr__(self, name):
        return lambda *args, **kwargs: Stub()


class FakeTextView(Stub):
    def __init__(self):
        super().__init__()
        self.text = ""
        self.editable = True

    def setString_(self, text):
        self.text = text

    def string(self):
        return self.text

    def setEditable_(self, editable):
        self.editable = editable


class FakeButton(Stub):
    made: ClassVar[list] = []

    def __init__(self, title, action):
        super().__init__()
        self.title, self.action, self.enabled = title, action, True
        FakeButton.made.append(self)

    def setEnabled_(self, enabled):
        self.enabled = enabled


class FakeNSObject:
    @classmethod
    def alloc(cls):
        return cls.__new__(cls)

    def init(self):
        return self

    def performSelectorOnMainThread_withObject_waitUntilDone_(self, selector, obj, wait):
        self.delivered.set()


@pytest.fixture
def settings_mac(monkeypatch):
    appkit = types.ModuleType("AppKit")
    appkit.__getattr__ = lambda name: 0  # constants
    appkit.NSTextField = types.SimpleNamespace(labelWithString_=lambda text: Stub())
    appkit.NSButton = types.SimpleNamespace(
        buttonWithTitle_target_action_=lambda title, target, action: FakeButton(title, action)
    )
    appkit.NSScrollView = Stub
    foundation = types.ModuleType("Foundation")
    foundation.NSMakeRect = lambda *args: args
    foundation.NSMakeSize = lambda *args: args
    foundation.NSObject = FakeNSObject

    class Super:
        def __init__(self, _cls, obj):
            self.obj = obj

        def init(self):
            return self.obj

    objc = types.ModuleType("objc")
    objc.super = Super
    objc.python_method = lambda function: function
    objc.autorelease_pool = contextlib.nullcontext
    style = types.ModuleType("localflow.mac_style")
    style.FlippedView = Stub
    style.PlaceholderTextView = types.SimpleNamespace(alloc=lambda: types.SimpleNamespace(
        initWithFrame_=lambda frame: FakeTextView()))
    for name in ("color", "font", "style_button", "style_input_box", "style_surface"):
        setattr(style, name, lambda *args, **kwargs: None)
    style.measure_text = lambda text, _font: measure(text)
    for name, module in (("AppKit", appkit), ("Foundation", foundation), ("objc", objc),
                         ("localflow.mac_style", style)):
        monkeypatch.setitem(sys.modules, name, module)
    spec = importlib.util.spec_from_file_location("hub_settings_mac_under_test", SETTINGS_MAC)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    monkeypatch.setattr(module, "load_config", lambda: "reloaded config")
    FakeButton.made = []
    return module


class FakeAgent:
    def __init__(self):
        self.gate = threading.Event()
        self.model = None
        self.applied = []
        self.fail = None

    def load(self):
        return Config()

    def current_settings(self, _config):
        return [("Sounds", "On"), ("Dictation shortcut", "Hold Left Option")]

    def pending_undo(self):
        return ["Sounds: Off → On"] if self.applied else None

    def propose(self, request):
        assert self.gate.wait(2)
        self.model = "gemma4:e4b"
        if self.fail:
            raise SettingsAgentError(self.fail)
        return proposal()

    def apply(self, changes):
        self.applied.append(changes)
        return ApplyResult(tuple(changes), "Applied 1 change.")

    def undo(self):
        raise SettingsAgentError("There's no change to undo.")


class FakeHub:
    def __init__(self, agent):
        self.page = "Settings"
        self.tokens = {}
        self.body = Stub()
        self.cursor_y = HUB_HEADER_OFFSET
        self.dictionary = None
        self.config = None
        self.settings_agent_factory = lambda: agent
        self.rebuilds = 0
        self.settings_page = None

    def _content_width(self):
        return MIN_CONTENT

    def _rebuild_current_page(self):
        # The real show_page contract: capture, tear down, render.
        self.rebuilds += 1
        self.settings_page.capture()
        self.body = Stub()
        self.cursor_y = HUB_HEADER_OFFSET
        if self.page == "Settings":
            self.settings_page.render()


def _page(settings_mac):
    agent = FakeAgent()
    hub = FakeHub(agent)
    controller = settings_mac.SettingsPageController.alloc().initWithHub_(hub)
    controller.delivered = threading.Event()
    hub.settings_page = controller
    controller.render()
    return controller, hub, agent


def _button(title):
    return [b for b in FakeButton.made if b.title == title][-1]


def _preview(controller, agent, text="turn off sounds"):
    controller.text_view.setString_(text)
    controller.preview_(None)
    return controller


def _deliver(controller, agent):
    agent.gate.set()
    assert controller.delivered.wait(2)
    controller.previewDone_(None)


def test_preview_runs_off_the_ui_thread_and_survives_a_resize(settings_mac):
    controller, hub, agent = _page(settings_mac)
    _preview(controller, agent)
    assert controller.state.busy
    assert _button("Preview").enabled is False
    assert controller.text_view.editable is False
    hub._rebuild_current_page()  # live resize while the model thinks
    assert controller.text_view.string() == "turn off sounds"  # draft kept
    assert controller.state.busy

    _deliver(controller, agent)
    assert controller.state.status == "preview"
    assert controller.state.model == "gemma4:e4b"
    assert _button("Apply").action == "apply:"

    controller.apply_(None)
    assert agent.applied and controller.state.status == "applied"
    assert hub.config == "reloaded config"  # shared pages see the new config
    assert controller.text_view.string() == ""  # draft cleared after apply
    assert _button(hub_settings.UNDO_TITLE).enabled is True


def test_result_after_navigating_away_is_kept_but_not_painted(settings_mac):
    controller, hub, agent = _page(settings_mac)
    _preview(controller, agent)
    hub.page = "Home"
    hub._rebuild_current_page()
    assert controller.text_view is None  # views gone with the page
    before = hub.rebuilds

    _deliver(controller, agent)
    assert controller.state.status == "preview"  # waiting when the user returns
    assert hub.rebuilds == before  # never repainted another page

    hub.page = "Settings"
    hub._rebuild_current_page()
    assert controller.text_view.string() == "turn off sounds"


def test_result_after_window_close_touches_nothing(settings_mac):
    controller, hub, agent = _page(settings_mac)
    _preview(controller, agent)
    controller.shutdown()
    assert controller.text_view is None
    before = hub.rebuilds
    _deliver(controller, agent)
    assert hub.rebuilds == before


def test_cancel_while_busy_ignores_the_late_answer(settings_mac):
    controller, _hub, agent = _page(settings_mac)
    _preview(controller, agent)
    controller.cancel_(None)
    assert controller.state.status == "empty"
    _deliver(controller, agent)
    assert controller.state.status == "empty"


def test_errors_are_friendly_and_keep_the_draft(settings_mac):
    controller, _hub, agent = _page(settings_mac)
    agent.fail = "Couldn't reach Ollama at http://localhost:11434."
    _preview(controller, agent)
    _deliver(controller, agent)
    assert controller.state.status == "error"
    assert controller.text_view.string() == "turn off sounds"

    controller.undo_(None)
    assert controller.state.message == "There's no change to undo."
