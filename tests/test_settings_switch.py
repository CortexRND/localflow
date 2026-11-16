"""The Settings page's house switch (mac_style.CobaltSwitch), without AppKit.

Pure geometry and palette come from ui_tokens; the real CobaltSwitch class
is then loaded against a fake AppKit that records what drawRect_ paints, so
the track fill, knob, outline and On/Off word are checked for both states,
both appearances and disabled, and its toggle semantics stay AppKit's own.
Native screenshots need macOS (scripts/mac_ui_snapshots.py: hub-settings-*).
"""

import ast
import importlib.util
import sys
import types
from pathlib import Path
from typing import ClassVar

import pytest

from localflow import ui_tokens
from localflow.ui_tokens import DARK, LIGHT

ROOT = Path(__file__).resolve().parents[1]
MAC_STYLE = ROOT / "localflow" / "mac_style.py"
SETTINGS_MAC = ROOT / "localflow" / "hub_settings_mac.py"
ON, OFF = 1, 0
PALETTES = {"light": LIGHT, "dark": DARK}


# ------------------------------------------------------------ geometry ---


@pytest.mark.parametrize("on", [True, False])
@pytest.mark.parametrize("size", [(ui_tokens.SWITCH_WIDTH, 22), (ui_tokens.SWITCH_WIDTH, 26),
                                  (120, 30)])
def test_track_knob_and_word_fit_without_overlapping(on, size):
    width, height = size
    parts = ui_tokens.switch_parts(width, height, on)
    tx, ty, tw, th = parts["track"]
    kx, ky, kw, kh = parts["knob"]
    sx, sy, sw, sh = parts["text"]
    assert tx + tw == width and 0 <= ty and ty + th <= height  # right-aligned, inside
    assert ty == (height - th) / 2  # centred: same in flipped and unflipped views
    assert tx < kx and kx + kw < tx + tw and ty < ky and ky + kh < ty + th
    assert kw == kh == th - 2 * ui_tokens.SWITCH_KNOB_INSET
    assert sx == 0 and sx + sw <= tx and sy + sh <= height
    assert parts["label"] == ("On" if on else "Off")
    # The knob says the state by position too: right when on, left when off.
    centre = kx + kw / 2
    assert (centre > tx + tw / 2) is on


def test_switch_width_holds_the_word_and_the_track():
    parts = ui_tokens.switch_parts(ui_tokens.SWITCH_WIDTH, ui_tokens.SWITCH_TRACK_H, True)
    assert parts["text"][2] == ui_tokens.SWITCH_STATE_W
    assert parts["track"][2] == ui_tokens.SWITCH_TRACK_W


def test_degenerate_sizes_never_go_negative():
    for width, height in ((0, 0), (10, 4), (ui_tokens.SWITCH_TRACK_W, 22)):
        parts = ui_tokens.switch_parts(width, height, True)
        assert all(value >= 0 for value in parts["track"][2:] + parts["knob"][2:])
        assert parts["text"][2] >= 0


# ------------------------------------------------------------- palette ---


@pytest.mark.parametrize("theme", ["light", "dark"])
def test_on_is_a_cobalt_fill_with_a_high_contrast_knob(theme):
    palette = PALETTES[theme]
    on = ui_tokens.switch_colors(palette, True)
    off = ui_tokens.switch_colors(palette, False)
    assert on["track"] == palette["cobalt"]  # the house token, never the system accent
    assert on["knob"] in (palette["paper"], palette["ink"])
    assert ui_tokens.contrast_ratio(on["knob"], on["track"]) >= 4.5
    assert on["outline"] == off["outline"] == palette["ink"]  # house ink outline
    # Off is a different, unfilled track with an ink knob.
    assert off["track"] == palette["paper"] != on["track"]
    assert off["knob"] == palette["ink"]
    assert ui_tokens.contrast_ratio(off["knob"], off["track"]) >= 4.5
    # The word is readable on the card's paper fill in both states.
    for colors in (on, off):
        assert ui_tokens.contrast_ratio(colors["text"], palette["paper"]) >= 4.5
    assert on["text"] != off["text"]
    assert on["alpha"] == off["alpha"] == 1.0


def test_only_palette_tokens_are_used():
    for palette in PALETTES.values():
        for on in (True, False):
            for enabled in (True, False):
                colors = ui_tokens.switch_colors(palette, on, enabled)
                for key in ("track", "knob", "outline", "text"):
                    assert colors[key] in palette.values(), key
    assert ui_tokens.switch_colors(LIGHT, True, enabled=False)["alpha"] == (
        ui_tokens.SWITCH_DISABLED_ALPHA
    )


def test_contrast_ratio_matches_the_wcag_formula():
    assert ui_tokens.contrast_ratio("#000000", "#FFFFFF") == pytest.approx(21.0)
    assert ui_tokens.contrast_ratio("#777777", "#777777") == pytest.approx(1.0)


# ---------------------------------------------- the real class, faked AppKit ---


class Rect:
    def __init__(self, x, y, width, height):
        self.origin = types.SimpleNamespace(x=x, y=y)
        self.size = types.SimpleNamespace(width=width, height=height)

    def tuple(self):
        return (self.origin.x, self.origin.y, self.size.width, self.size.height)


class Canvas:
    """Everything one drawRect_ paints, in order."""

    ops: ClassVar[list] = []
    current = None
    depth = 0


class Color:
    def __init__(self, rgb, alpha=1.0):
        self.rgb, self.alpha = rgb, alpha

    def colorWithAlphaComponent_(self, alpha):
        return Color(self.rgb, alpha)

    def set(self):
        Canvas.current = self


def rgb(hex_color):
    value = hex_color.lstrip("#")
    return tuple(round(int(value[i:i + 2], 16) / 255, 6) for i in (0, 2, 4))


class FakePath:
    def __init__(self, kind, rect):
        self.kind, self.rect, self.width = kind, rect.tuple(), None

    def setLineWidth_(self, width):
        self.width = width

    def fill(self):
        Canvas.ops.append(("fill", self.kind, self.rect, Canvas.current))

    def stroke(self):
        Canvas.ops.append(("stroke", self.kind, self.rect, Canvas.current, self.width))


class BezierPath:
    @staticmethod
    def bezierPathWithRoundedRect_xRadius_yRadius_(rect, _rx, _ry):
        return FakePath("track", rect)

    @staticmethod
    def bezierPathWithOvalInRect_(rect):
        return FakePath("knob", rect)


class GraphicsContext:
    @staticmethod
    def saveGraphicsState():
        Canvas.depth += 1

    @staticmethod
    def restoreGraphicsState():
        Canvas.depth -= 1


class AttributedString:
    @classmethod
    def alloc(cls):
        return cls()

    def initWithString_attributes_(self, text, attributes):
        self.text, self.attributes = text, attributes
        return self

    def drawInRect_(self, rect):
        Canvas.ops.append(("text", self.text, rect.tuple(), self.attributes))


class Button:
    """Fake NSButton with AppKit's switch-type semantics: a click (or Space,
    which AppKit routes to performClick_) flips the state, then sends the
    action to the target. Its own drawing must never run under ours."""

    def __init__(self):
        self._state, self._enabled, self.calls = OFF, True, []
        self._target = self._action = None
        self._frame = Rect(0, 0, 0, 0)

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self._frame = frame
        return self

    def bounds(self):
        return Rect(0, 0, self._frame.size.width, self._frame.size.height)

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return lambda *args: self.__dict__["calls"].append((name, *args))

    def setState_(self, state):
        self._state = state

    def state(self):
        return self._state

    def setEnabled_(self, enabled):
        self._enabled = enabled

    def isEnabled(self):
        return self._enabled

    def setTarget_(self, target):
        self._target = target

    def setAction_(self, action):
        self._action = action

    def performClick_(self, _sender):
        if not self._enabled:
            return
        self._state = OFF if self._state == ON else ON
        getattr(self._target, self._action.replace(":", "_"))(self)

    def drawRect_(self, _rect):
        raise AssertionError("the button's own drawing ran: the switch would draw twice")


@pytest.fixture
def mac_style(monkeypatch):
    appkit = types.ModuleType("AppKit")
    appkit.__getattr__ = lambda name: name  # other constants: their own names
    appkit.NSView = appkit.NSTextView = object
    appkit.NSButton = Button
    appkit.NSControlStateValueOn, appkit.NSControlStateValueOff = ON, OFF
    appkit.NSColor = types.SimpleNamespace(
        colorWithCalibratedRed_green_blue_alpha_=lambda r, g, b, a: Color(
            (round(r, 6), round(g, 6), round(b, 6)), a),
    )
    appkit.NSBezierPath = BezierPath
    appkit.NSGraphicsContext = GraphicsContext
    appkit.NSAttributedString = AttributedString
    appkit.NSMutableParagraphStyle = types.SimpleNamespace(
        alloc=lambda: types.SimpleNamespace(init=lambda: types.SimpleNamespace(
            setAlignment_=lambda alignment: None)))
    appkit.NSFont = types.SimpleNamespace(
        systemFontOfSize_weight_=lambda size, weight: (size, weight))
    foundation = types.ModuleType("Foundation")
    foundation.NSMakeRect = Rect
    foundation.NSMakeSize = lambda w, h: (w, h)
    fake_objc = types.ModuleType("objc")
    fake_objc.super = lambda *_args: pytest.fail("the switch never calls up to draw")
    fake_objc.python_method = lambda function: function
    for name, module in (("AppKit", appkit), ("Foundation", foundation),
                         ("objc", fake_objc)):
        monkeypatch.setitem(sys.modules, name, module)
    spec = importlib.util.spec_from_file_location("mac_style_switch_under_test", MAC_STYLE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    Canvas.ops, Canvas.current, Canvas.depth = [], None, 0
    return module


def make_switch(module, palette, on, width=ui_tokens.SWITCH_WIDTH, height=22):
    return module.CobaltSwitch.alloc().initWithFrame_(Rect(0, 0, width, height)).configure(
        palette, on, "Sounds",
    )


def painted(switch):
    Canvas.ops = []
    switch.drawRect_(switch.bounds())
    assert Canvas.depth == 0  # graphics state restored
    return Canvas.ops


@pytest.mark.parametrize("theme", ["light", "dark"])
@pytest.mark.parametrize("on", [True, False])
def test_draw_paints_track_knob_outline_and_word_from_tokens(mac_style, theme, on):
    palette = PALETTES[theme]
    switch = make_switch(mac_style, palette, on)
    ops = painted(switch)
    colors = ui_tokens.switch_colors(palette, on)
    parts = ui_tokens.switch_parts(ui_tokens.SWITCH_WIDTH, 22, on)
    fills = [op for op in ops if op[0] == "fill"]
    strokes = [op for op in ops if op[0] == "stroke"]
    texts = [op for op in ops if op[0] == "text"]
    # Exactly one track, one knob and one word: nothing drawn twice.
    assert [op[1] for op in fills] == ["track", "knob"]
    assert [op[1] for op in strokes] == ["track", "knob"] and len(texts) == 1
    track_fill, knob_fill = fills
    assert track_fill[3].rgb == rgb(colors["track"])
    assert knob_fill[3].rgb == rgb(colors["knob"]) and knob_fill[2] == parts["knob"]
    if on:
        assert track_fill[3].rgb == rgb(palette["cobalt"])
    for stroke in strokes:
        assert stroke[3].rgb == rgb(palette["ink"])
    assert strokes[0][4] == ui_tokens.SWITCH_BORDER_WIDTH
    # The stroke stays inside the bounds (inset by half its width).
    x, y, w, h = track_fill[2]
    assert x >= parts["track"][0] and x + w <= ui_tokens.SWITCH_WIDTH
    assert y >= parts["track"][1] and y + h <= parts["track"][1] + parts["track"][3]
    _kind, word, rect, attributes = texts[0]
    assert word == ("On" if on else "Off") and rect == parts["text"]
    assert attributes["NSForegroundColorAttributeName"].rgb == rgb(colors["text"])
    assert all(op[3].alpha == 1.0 for op in fills)


def test_disabled_switch_is_dimmed_not_recoloured(mac_style):
    switch = make_switch(mac_style, LIGHT, True)
    switch.setEnabled_(False)
    fills = [op for op in painted(switch) if op[0] == "fill"]
    assert fills[0][3].rgb == rgb(LIGHT["cobalt"])
    assert {op[3].alpha for op in fills} == {ui_tokens.SWITCH_DISABLED_ALPHA}


def test_configure_keeps_native_toggle_semantics_and_accessibility(mac_style):
    switch = make_switch(mac_style, DARK, False)
    calls = {name: args for name, *args in switch.calls}
    assert calls["setButtonType_"] == ["NSButtonTypeSwitch"]  # AppKit's toggle type
    assert calls["setTitle_"] == [""] and calls["setBordered_"] == [False]
    assert calls["setAccessibilityLabel_"] == ["Sounds"]
    assert switch.state() == OFF and switch.accessibilityValueDescription() == "Off"
    assert switch.wantsUpdateLayer() is False

    received = []
    target = types.SimpleNamespace(manualToggle_=lambda sender: received.append(
        sender.state()))
    switch.setTarget_(target)
    switch.setAction_("manualToggle:")
    switch.performClick_(None)  # click / Space: AppKit flips, then sends
    assert received == [ON] and switch.is_on()
    assert switch.accessibilityValueDescription() == "On"
    assert [op[1] for op in painted(switch) if op[0] == "text"] == ["On"]
    switch.performClick_(None)
    assert received == [ON, OFF] and not switch.is_on()
    switch.setEnabled_(False)
    switch.performClick_(None)
    assert received == [ON, OFF]  # disabled: no toggle, no action


def test_focus_ring_follows_the_track(mac_style):
    switch = make_switch(mac_style, LIGHT, True)
    assert switch.focusRingMaskBounds().tuple() == ui_tokens.switch_parts(
        ui_tokens.SWITCH_WIDTH, 22, False)["track"]
    Canvas.ops = []
    switch.drawFocusRingMask()
    assert [op[1] for op in Canvas.ops] == ["track"]


def test_switch_overrides_only_drawing_and_never_uses_builtin_super():
    source = MAC_STYLE.read_text(encoding="utf-8")
    cls = next(node for node in ast.parse(source).body
               if isinstance(node, ast.ClassDef) and node.name == "CobaltSwitch")
    methods = {node.name for node in cls.body if isinstance(node, ast.FunctionDef)}
    # Toggling, keyboard and state stay NSButton's: none of them is replaced.
    for native in ("mouseDown_", "keyDown_", "performClick_", "setState_", "state",
                   "acceptsFirstResponder", "accessibilityRole"):
        assert native not in methods, native
    body = ast.get_source_segment(source, cls)
    assert "super()" not in body and "objc.super" not in body  # no super drawing at all
    assert "controlAccentColor" not in body and "#" not in body.replace("# ", "")


def test_settings_uses_the_switch_and_hot_phrases_keeps_nsswitch():
    settings = SETTINGS_MAC.read_text(encoding="utf-8")
    assert "CobaltSwitch.alloc().initWithFrame_(" in settings
    assert "NSSwitch" not in settings
    hub = (ROOT / "localflow" / "hub_mac.py").read_text(encoding="utf-8")
    phrases = (ROOT / "localflow" / "phrases_mac.py").read_text(encoding="utf-8")
    assert "CobaltSwitch" not in hub and "CobaltSwitch" not in phrases
    assert "NSSwitch.alloc()" in hub and "NSSwitch.alloc()" in phrases
