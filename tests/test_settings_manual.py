"""Manual Settings controls: every allowlisted key, wiring, shared apply/undo.

The real SettingsPageController runs against stub AppKit (as in
test_hub_settings) and a real SettingsAgent over temp files, so a toggle,
pick or typed shortcut goes through exactly the agent's validation, write
and undo record; the model is a gated fake Ollama.
"""

import contextlib
import importlib.util
import itertools
import sys
import threading
import types
from typing import ClassVar

import pytest

# Sibling test modules (pytest puts tests/ on sys.path).
from test_hub_settings import (
    MIN_CONTENT,
    SETTINGS_MAC,
    WIDE_CONTENT,
    FakeButton,
    FakeHub,
    Stub,
)
from test_settings_agent import FakeOllama, call

from localflow import hub_settings
from localflow import settings_agent as sa
from localflow.config import Config, _config_from_v2, _parse, save_config
from localflow.dictionary import DictionaryStore
from localflow.ui_tokens import HUB_HEADER_OFFSET

ON, OFF = 1, 0


def measure(text):
    return len(text) * 7


# ------------------------------------------------------------- mapping ---


def test_manual_groups_cover_every_allowlist_key_exactly_once():
    keys = [key for _group, group_keys in sa.MANUAL_GROUPS for key in group_keys]
    assert len(keys) == len(set(keys)) == 24
    assert set(keys) == set(sa.ALLOWLIST)
    assert [group for group, _keys in sa.MANUAL_GROUPS] == [
        "Dictation", "Cleanup & style", "Shortcuts", "Meetings",
    ]
    kinds = {key: sa.control_kind(key) for key in keys}
    assert {k for k, v in kinds.items() if v == "switch"} == set(sa._BOOL_KEYS)
    assert {k for k, v in kinds.items() if v == "shortcut"} == set(sa._SHORTCUT_KEYS)
    assert {k for k, v in kinds.items() if v == "choice"} == {
        "cleanup_level", "style_work", "style_email", "style_personal", "style_other",
        "paste_method", "meeting_default_template",
    }


@pytest.mark.parametrize("value", [
    "alt_l", "alt_r", "cmd_r", "ctrl_r", "f13", "<ctrl>+<alt>+1", "<ctrl>+<cmd>+v",
    "<cmd>+<shift>+<space>", "<ctrl>+<alt>+<f5>",
])
def test_editable_shortcut_round_trips_through_the_validator(value):
    validator = sa.Validator("darwin", templates=lambda _c: ("general",))
    text = sa.editable_shortcut(value)
    key = "hotkey" if "+" in value or value in sa.HOLD_KEYS else "polish_hotkey"
    assert validator.value(key, text, Config()) == value


# ------------------------------------------------------------ AppKit stub ---


class Control(Stub):
    made: ClassVar[list] = []

    def __init__(self, kind):
        super().__init__()
        self.kind = kind
        self.target = self.action = self.enabled = None
        self.value, self.state_, self.items, self.index = "", OFF, [], -1
        self.end_edit = False
        Control.made.append(self)

    def initWithFrame_pullsDown_(self, frame, _pulls):
        self.frame_ = frame
        return self

    def setTarget_(self, target):
        self.target = target

    def setAction_(self, action):
        self.action = action

    def setEnabled_(self, enabled):
        self.enabled = enabled

    def setState_(self, state):
        self.state_ = state

    def state(self):
        return self.state_

    def configure(self, palette, on, label):
        """mac_style.CobaltSwitch.configure: state and label of a new switch."""
        self.palette, self.label = palette, label
        self.state_ = ON if on else OFF
        return self

    def addItemsWithTitles_(self, titles):
        self.items = list(titles)

    def selectItemAtIndex_(self, index):
        self.index = index

    def indexOfSelectedItem(self):
        return self.index

    def setStringValue_(self, value):
        self.value = value

    def stringValue(self):
        return self.value

    def cell(self):
        return self

    def setSendsActionOnEndEditing_(self, flag):
        self.end_edit = flag


class Label(Stub):
    made: ClassVar[list] = []

    def __init__(self, text):
        super().__init__()
        self.text = text
        Label.made.append(self)


def _factory(kind):
    return types.SimpleNamespace(alloc=lambda: Control(kind))


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
    appkit.NSControlStateValueOn, appkit.NSControlStateValueOff = ON, OFF
    appkit.NSTextField = types.SimpleNamespace(
        labelWithString_=lambda text: Label(text), alloc=lambda: Control("field"),
    )
    appkit.NSButton = types.SimpleNamespace(
        buttonWithTitle_target_action_=lambda title, target, action: FakeButton(title, action)
    )
    # The house switch replaced NSSwitch: a stray NSSwitch would fail here.
    appkit.NSSwitch = None
    appkit.NSPopUpButton = _factory("popup")
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
    style.CobaltSwitch = _factory("switch")

    class _TextView(Stub):
        def __init__(self):
            super().__init__()
            self.text = ""

        def setString_(self, text):
            self.text = text

        def string(self):
            return self.text

    style.PlaceholderTextView = types.SimpleNamespace(alloc=lambda: types.SimpleNamespace(
        initWithFrame_=lambda frame: _TextView()))
    for name in ("color", "font", "style_button", "style_input_box", "style_surface"):
        setattr(style, name, lambda *args, **kwargs: None)
    style.measure_text = lambda text, _font: measure(text)
    for name, module in (("AppKit", appkit), ("Foundation", foundation), ("objc", objc),
                         ("localflow.mac_style", style)):
        monkeypatch.setitem(sys.modules, name, module)
    spec = importlib.util.spec_from_file_location("hub_settings_mac_manual", SETTINGS_MAC)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    monkeypatch.setattr(module, "load_config", lambda: "reloaded config")
    FakeButton.made, Control.made, Label.made = [], [], []
    return module


class GatedOllama(FakeOllama):
    """Answers /api/chat only once `gate` is set (a slow local model)."""

    def __init__(self, calls):
        super().__init__(calls)
        self.gate = threading.Event()

    def post(self, url, json=None, **kwargs):
        assert self.gate.wait(2)
        return super().post(url, json=json, **kwargs)


class SpyAgent(sa.SettingsAgent):
    """The real agent, recording which public entry points the page used."""

    used: ClassVar[list] = []

    def set_setting(self, key, value):
        SpyAgent.used.append(("set_setting", key, value))
        return super().set_setting(key, value)


@pytest.fixture
def env(tmp_path, monkeypatch, settings_mac):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    config_path = tmp_path / "config.toml"
    save_config(Config(), config_path)
    ollama = GatedOllama([call("set_setting", key="sounds_enabled", value="off")])
    SpyAgent.used = []

    def agent():
        return SpyAgent(
            config_path=config_path,
            dictionary=DictionaryStore(tmp_path / "dictionary.json"),
            undo_path=tmp_path / "settings-undo.json",
            session=ollama, platform="darwin",
            templates=lambda _c: ("general", "standup"),
        )

    hub = FakeHub(None)
    hub.settings_agent_factory = agent
    controller = settings_mac.SettingsPageController.alloc().initWithHub_(hub)
    controller.delivered = threading.Event()
    hub.settings_page = controller
    controller.render()
    return types.SimpleNamespace(
        controller=controller, hub=hub, ollama=ollama, module=settings_mac,
        config=lambda: _config_from_v2(_parse(config_path)),
        undo=tmp_path / "settings-undo.json",
    )


def control(env, key):
    return env.controller.manual_controls[key][1]


def label_texts():
    return [label.text for label in Label.made]


# ------------------------------------------------------------- wiring ---


def test_every_setting_has_an_enabled_explicitly_targeted_control(env):
    controls = env.controller.manual_controls
    assert set(controls) == set(sa.ALLOWLIST)
    actions = {"switch": "manualToggle:", "choice": "manualChoose:",
               "shortcut": "manualShortcut:"}
    for key, (kind, view) in controls.items():
        assert kind == sa.control_kind(key)
        assert view.target is env.controller, key   # explicit target, retained by us
        assert view.action == actions[kind], key
        assert view.enabled is True, key
        if kind == "shortcut":
            assert view.end_edit is True  # Return / end of editing, not keystrokes
    assert control(env, "sounds_enabled").state() == ON
    style = control(env, "style_work")
    assert style.items == ["Formal", "Casual", "Very casual", "Excited"]
    assert style.index == 0
    assert control(env, "paste_method").items == ["Automatic", "Clipboard", "AppleScript"]
    assert control(env, "meeting_default_template").items == ["General", "Standup"]
    assert control(env, "polish_hotkey").stringValue() == "ctrl+option+1"
    for group, _keys in sa.MANUAL_GROUPS:
        assert group in label_texts()


def test_controller_writes_only_through_the_agent():
    """No direct config writes: checked on imports and call targets (code),
    not text, so the docstring may still say what it never does."""
    import ast

    tree = ast.parse(SETTINGS_MAC.read_text(encoding="utf-8"))
    forbidden = {"save_config", "apply_config_update", "_apply_settings", "_save",
                 "write_text", "_write_private_json"}
    imported = {
        alias.asname or alias.name
        for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)
        for alias in node.names
    }
    assert imported.isdisjoint(forbidden), imported & forbidden
    called = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Call):
            target = node.func
            if isinstance(target, ast.Name):
                called.add(target.id)
            elif isinstance(target, ast.Attribute):
                called.add(target.attr)
    assert called.isdisjoint(forbidden), called & forbidden
    assert "set_setting" in called  # manual changes go through the agent's rules


def test_toggle_saves_through_set_setting_and_undo_reverts(env):
    switch = control(env, "sounds_enabled")
    switch.setState_(OFF)
    env.controller.manualToggle_(switch)
    assert SpyAgent.used == [("set_setting", "sounds_enabled", False)]
    assert env.config().sounds_enabled is False
    assert env.undo.exists()  # recorded like an agent Apply
    assert env.controller.state.manual_notice == "Saved: Sounds: On → Off"
    assert env.hub.config == "reloaded config"  # shared pages see the change
    assert control(env, "sounds_enabled").state() == OFF  # repainted from disk

    env.controller.undo_(None)
    assert env.config().sounds_enabled is True
    assert control(env, "sounds_enabled").state() == ON


def test_choice_picks_save_and_unchanged_picks_write_nothing(env):
    popup = control(env, "cleanup_level")
    assert popup.items == ["None", "Light", "Medium"] and popup.index == 1
    popup.selectItemAtIndex_(1)  # same value: nothing written, no undo record
    env.controller.manualChoose_(popup)
    assert not env.undo.exists()
    popup = control(env, "cleanup_level")
    popup.selectItemAtIndex_(2)
    env.controller.manualChoose_(popup)
    assert env.config().cleanup_level == "medium"


def test_invalid_shortcut_keeps_its_text_and_a_friendly_error(env):
    field = control(env, "hotkey")
    field.setStringValue_("a")
    env.controller.manualShortcut_(field)
    state = env.controller.state
    assert env.config().hotkey == "alt_l"
    assert "hotkey" in state.manual_errors and "not a key you can hold" in state.manual_errors[
        "hotkey"]
    assert control(env, "hotkey").stringValue() == "a"  # kept after the repaint
    assert any("not a key you can hold" in text for text in label_texts())

    for _ in range(2):  # resize / navigate away and back
        env.hub._rebuild_current_page()
        assert control(env, "hotkey").stringValue() == "a"
        assert "hotkey" in env.controller.state.manual_errors

    field = control(env, "hotkey")
    field.setStringValue_("right_command")
    env.controller.manualShortcut_(field)
    assert env.config().hotkey == "cmd_r"
    assert "hotkey" not in state.manual_errors and "hotkey" not in state.manual_drafts
    assert control(env, "hotkey").stringValue() == "right_command"


def test_conflicting_shortcut_is_refused_inline(env):
    field = control(env, "prompt_hotkey")
    field.setStringValue_("ctrl+option+1")  # the Polish shortcut
    env.controller.manualShortcut_(field)
    assert env.config().prompt_hotkey == "<ctrl>+<alt>+2"
    assert "already the Polish shortcut" in env.controller.state.manual_errors["prompt_hotkey"]
    assert control(env, "prompt_hotkey").stringValue() == "ctrl+option+1"
    assert not env.undo.exists()


def test_typed_but_uncommitted_text_survives_rebuilds_without_saving(env):
    control(env, "polish_hotkey").setStringValue_("ctrl+opt")
    env.hub._rebuild_current_page()
    assert control(env, "polish_hotkey").stringValue() == "ctrl+opt"
    assert env.config().polish_hotkey == "<ctrl>+<alt>+1"
    assert SpyAgent.used == []


def test_stale_controls_from_a_removed_page_do_nothing(env):
    old_switch = control(env, "sounds_enabled")
    old_switch.setState_(OFF)
    env.hub._rebuild_current_page()  # the views are replaced
    env.controller.manualToggle_(old_switch)
    env.controller.manualShortcut_(Control("field"))
    assert SpyAgent.used == [] and env.config().sounds_enabled is True
    env.controller.shutdown()
    assert env.controller.manual_controls == {}


# ------------------------------------------------ manual vs agent preview ---


def _deliver(env):
    env.ollama.gate.set()
    assert env.controller.delivered.wait(2)
    env.controller.previewDone_(None)


def test_manual_change_retires_a_shown_preview(env):
    env.controller.text_view.setString_("turn off sounds")
    env.controller.preview_(None)
    _deliver(env)
    state = env.controller.state
    assert state.status == "preview" and state.warnings  # sounds off is flagged
    popup = control(env, "cleanup_level")
    popup.selectItemAtIndex_(2)
    env.controller.manualChoose_(popup)
    assert state.status == "error" and state.message == hub_settings.STALE_PREVIEW
    assert state.proposal is None
    env.controller.apply_(None)  # nothing left to apply
    assert env.config().sounds_enabled is True
    assert env.config().cleanup_level == "medium"
    assert env.controller.text_view.string() == "turn off sounds"  # draft kept


def test_late_preview_never_overwrites_a_manual_change(env):
    env.controller.text_view.setString_("turn off sounds")
    env.controller.preview_(None)
    assert env.controller.state.busy
    switch = control(env, "sounds_enabled")
    switch.setState_(OFF)
    env.controller.manualToggle_(switch)  # controls stay usable while busy
    state = env.controller.state
    assert state.status == "error" and state.message == hub_settings.STALE_PENDING
    _deliver(env)  # the model's answer lands after the manual change
    assert state.status == "error" and state.proposal is None
    assert env.config().sounds_enabled is False
    assert env.controller.text_view.string() == "turn off sounds"


def test_agent_apply_clears_manual_drafts(env):
    field = control(env, "hotkey")
    field.setStringValue_("a")
    env.controller.manualShortcut_(field)
    env.controller.text_view.setString_("turn off sounds")
    env.controller.preview_(None)
    _deliver(env)
    env.controller.apply_(None)
    state = env.controller.state
    assert state.status == "applied" and env.config().sounds_enabled is False
    assert state.manual_drafts == {} and state.manual_errors == {}
    assert control(env, "hotkey").stringValue() == "left_option"


# -------------------------------------------------------------- layout ---


def _rows(config=None):
    agent = sa.SettingsAgent(
        config_path="/nonexistent/config.toml", dictionary=DictionaryStore("/nonexistent/d.json"),
        platform="darwin", templates=lambda _c: ("general",),
    )
    return agent.manual_settings(config or Config())


def overlap(a, b):
    return a[0] < b[0] + b[2] and b[0] < a[0] + a[2] and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]


@pytest.mark.parametrize("content", [MIN_CONTENT, WIDE_CONTENT])
@pytest.mark.parametrize("with_error", [False, True])
def test_manual_panel_fits_its_column_and_nothing_overlaps(content, with_error):
    state = hub_settings.SettingsPageState()
    if with_error:
        state.manual_failed("prompt_hotkey", "ctrl+option+1",
                            "Nothing was changed: Prompt shortcut: Ctrl+Option+1 is "
                            "already the Polish shortcut." * 2)
        state.manual_notice = "Saved: Cleanup level: Light → Medium"
    rows = _rows()
    width = hub_settings.manual_panel_width(content)
    manual = hub_settings.manual_layout(width, rows, state, measure)
    assert set(manual["rows"]) == set(sa.ALLOWLIST)
    frames = [manual["title"], manual["intro"]] + [f for _n, f in manual["groups"]]
    for row in manual["rows"].values():
        frames += list(row.values())
    if "notice" in manual:
        frames.append(manual["notice"])
    for frame in frames:
        x, y, w, h = frame
        assert x >= 0 and x + w <= width and y >= 0 and y + h <= manual["height"]
    for first, second in itertools.combinations(frames, 2):
        assert not overlap(first, second), (first, second)
    inner = width - 2 * hub_settings.CARD_PAD
    right = hub_settings.CARD_PAD + inner
    for row in rows:
        frames = manual["rows"][row.key]
        lx, ly, lw, lh = frames["label"]
        cx, cy, cw, ch = frames["control"]
        # Label left, control right-aligned on the same line, every kind
        # (switches too), at the minimum and wide widths alike.
        assert lx == hub_settings.CARD_PAD and cx + cw == right, row.key
        assert lx + lw + hub_settings.LABEL_GAP <= cx, row.key
        assert ly < cy + ch and cy < ly + lh, row.key  # one row, not stacked
        if "error" in frames:  # wraps below the whole row, full width
            ex, ey, ew, _eh = frames["error"]
            assert (ex, ew) == (hub_settings.CARD_PAD, inner)
            assert ey >= max(ly + lh, cy + ch)
    switch = manual["rows"]["sounds_enabled"]["control"]
    assert switch[2] == hub_settings.SWITCH_WIDTH
    if with_error:
        assert "error" in manual["rows"]["prompt_hotkey"]
    assert "side_by_side" not in manual

    layout = hub_settings.settings_layout(
        content, HUB_HEADER_OFFSET, state, 22, measure, manual_height=manual["height"],
    )
    x, y, w, h = layout["current"]
    # The manual panel is full width under the agent and its Undo row.
    assert h == manual["height"] and (x, w) == (0, content) and width == content
    assert layout["bottom"] >= y + h
    assert y >= layout["undo"][1] + layout["undo"][3]
    assert layout["main_w"] == content


@pytest.mark.parametrize("content", [MIN_CONTENT, WIDE_CONTENT])
def test_long_labels_wrap_in_their_column_and_keep_the_control_right(content):
    long_row = types.SimpleNamespace(
        key="sounds_enabled", label="Play a sound when dictation starts and stops " * 4,
        kind="switch", group="Dictation",
    )
    state = hub_settings.SettingsPageState()
    manual = hub_settings.manual_layout(content, [long_row], state, measure)
    lx, ly, lw, lh = manual["rows"]["sounds_enabled"]["label"]
    cx, _cy, cw, _ch = manual["rows"]["sounds_enabled"]["control"]
    assert lh > hub_settings.LINE_H  # wrapped, not truncated or overlapping
    assert lx + lw < cx and cx + cw == content - hub_settings.CARD_PAD
    assert manual["height"] >= ly + lh


def test_shortcut_draft_and_error_survive_resizing_between_widths(env):
    """A failed shortcut keeps its typed text and inline error when the page
    is rebuilt at another width (the frames move, the draft does not)."""
    control(env, "prompt_hotkey").setStringValue_("ctrl+option+1")  # the Polish shortcut
    env.controller.manualShortcut_(control(env, "prompt_hotkey"))
    assert "prompt_hotkey" in env.controller.state.manual_errors
    for width in (WIDE_CONTENT, MIN_CONTENT, WIDE_CONTENT):
        env.hub._content_width = lambda width=width: width
        env.hub._rebuild_current_page()  # capture, tear down, render
        field = control(env, "prompt_hotkey")
        assert field.stringValue() == "ctrl+option+1"
        x, _y, w, _h = field.frame_
        assert w <= hub_settings.CONTROL_W
        assert x + w == width - hub_settings.CARD_PAD  # right-aligned at this width
        assert "prompt_hotkey" in env.controller.state.manual_errors
    assert env.controller.state.manual_drafts["prompt_hotkey"] == "ctrl+option+1"
    assert env.config().prompt_hotkey == "<ctrl>+<alt>+2"  # nothing was saved


def test_switches_are_the_house_switch_with_tokens_label_and_state(env):
    switches = {key: view for key, (kind, view) in env.controller.manual_controls.items()
                if kind == "switch"}
    assert switches and set(switches) == set(sa._BOOL_KEYS)
    labels = {row.key: row.label for row in env.controller.manual_rows}
    for key, view in switches.items():
        assert view.kind == "switch" and view.palette is env.hub.tokens, key
        assert view.label == labels[key], key
        assert view.target is env.controller and view.action == "manualToggle:", key
    assert switches["sounds_enabled"].state() == ON


def test_preview_warnings_get_their_own_rows_inside_the_card():
    state = hub_settings.SettingsPageState()
    token = state.begin_preview("change all hotkeys")
    state.finish_preview(token, sa.Proposal("change all hotkeys", "m", (
        sa.setting_change("polish_hotkey", "<ctrl>+<alt>+1", "<ctrl>+<alt>+7"),
        sa.setting_change("meeting_watch", True, False),
    )))
    assert len(state.warnings) == 2
    layout = hub_settings.settings_layout(MIN_CONTENT, HUB_HEADER_OFFSET, state, 22, measure)
    card_h = layout["card"][3]
    warning_rows = layout["warning_rows"]
    assert len(warning_rows) == 2
    assert warning_rows[0][1] >= layout["rows"][1][1] + layout["rows"][1][3]
    assert all(y + h <= layout["apply"][1] <= card_h for _x, y, _w, h in warning_rows)
