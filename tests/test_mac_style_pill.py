"""Exercise mac_style.make_pill on Linux with a minimal fake AppKit."""

import importlib.util
import math
import sys
import types
from pathlib import Path

import pytest

from localflow import ui_tokens

SRC = Path(__file__).parent.parent / "localflow" / "mac_style.py"
GLYPH_WIDTH_PER_CHAR = 7
CELL_INSET = 4  # extra width NSTextField's cell adds around the glyphs


class Rect:
    def __init__(self, x, y, width, height):
        self.origin = types.SimpleNamespace(x=x, y=y)
        self.size = types.SimpleNamespace(width=width, height=height)


class Layer:
    def __init__(self):
        self.calls = {}

    def __getattr__(self, name):
        if name.startswith("set"):
            return lambda value: self.calls.__setitem__(name, value)
        raise AttributeError(name)


class View:
    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, frame):
        self._frame, self.subviews, self._layer = frame, [], Layer()
        return self

    def frame(self):
        return self._frame

    def setFrame_(self, frame):
        self._frame = frame

    def addSubview_(self, view):
        self.subviews.append(view)

    def setWantsLayer_(self, value):
        pass

    def layer(self):
        return self._layer


class Label(View):
    @classmethod
    def labelWithString_(cls, text):
        label = cls().initWithFrame_(Rect(0, 0, 0, 0))
        label.text, label.font_value = text, None
        return label

    def setFont_(self, value):
        self.font_value = value

    def setTextColor_(self, value):
        pass

    def setLineBreakMode_(self, value):
        pass

    def sizeToFit(self):
        width = GLYPH_WIDTH_PER_CHAR * len(self.text) + CELL_INSET
        self._frame = Rect(0, 0, width + 0.4, 14.2)


class TextView(View):
    """Base for mac_style.PlaceholderTextView; the pill never uses it."""


class Color:
    def CGColor(self):
        return "cgcolor"


@pytest.fixture
def mac_style(monkeypatch):
    fonts = []
    appkit = types.ModuleType("AppKit")
    appkit.NSView = View
    appkit.NSTextField = Label
    appkit.NSTextView = TextView
    appkit.NSLineBreakByClipping = 2
    appkit.NSFontWeightRegular = "regular"
    appkit.NSFontWeightSemibold = "semibold"
    appkit.NSFontWeightBold = "bold"
    appkit.NSFont = types.SimpleNamespace(
        systemFontOfSize_weight_=lambda size, weight: fonts.append((size, weight))
        or (size, weight)
    )
    appkit.NSColor = types.SimpleNamespace(
        colorWithCalibratedRed_green_blue_alpha_=lambda *args: Color(),
        clearColor=Color,
    )
    foundation = types.ModuleType("Foundation")
    foundation.NSMakeRect = Rect
    foundation.NSMakeSize = lambda w, h: (w, h)
    monkeypatch.setitem(sys.modules, "AppKit", appkit)
    monkeypatch.setitem(sys.modules, "Foundation", foundation)
    spec = importlib.util.spec_from_file_location("mac_style_under_test", SRC)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.recorded_fonts = fonts
    return module


@pytest.mark.parametrize("text", ["Ready", "Needs you", "Unknown", "Checking…"])
def test_pill_is_sized_from_the_text_cell_width_not_glyph_width(mac_style, text):
    pill = mac_style.make_pill(text, ("red", "red"))

    label = pill.subviews[0]
    glyph_only = GLYPH_WIDTH_PER_CHAR * len(text)
    cell_width = math.ceil(glyph_only + CELL_INSET + 0.4)
    assert label.frame().size.width == cell_width > glyph_only
    assert pill.frame().size.width == cell_width + 2 * ui_tokens.PILL_PADDING
    # The label ends a full padding before the pill's right edge: nothing clips.
    label_right = label.frame().origin.x + label.frame().size.width
    assert pill.frame().size.width - label_right == ui_tokens.PILL_PADDING
    assert label.frame().origin.x == ui_tokens.PILL_PADDING
    assert pill.frame().size.height == ui_tokens.PILL_HEIGHT


def test_pill_label_is_vertically_centered_and_uses_token_font_and_border(mac_style):
    pill = mac_style.make_pill("Ready", ("success", "success"))

    label = pill.subviews[0]
    height = label.frame().size.height
    assert label.frame().origin.y == (ui_tokens.PILL_HEIGHT - height) / 2
    assert mac_style.recorded_fonts[-1] == tuple(ui_tokens.TYPE["caption"])
    calls = pill.layer().calls
    assert calls["setBorderWidth_"] == ui_tokens.PILL_BORDER_WIDTH
    assert calls["setCornerRadius_"] == ui_tokens.PILL_HEIGHT / 2
