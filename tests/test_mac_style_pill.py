"""Exercise mac_style (make_pill, PlaceholderTextView) on Linux with a minimal
fake AppKit and objc."""

import ast
import importlib.util
import math
import sys
import types
from pathlib import Path
from typing import ClassVar

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
    """Fake NSTextView. Like a PyObjC class, its overridden selectors are only
    reachable through objc.super: a builtin super() call raises the same
    AttributeError the native snapshot run hit in drawRect_."""

    dispatching = False

    def _objc_only(self, name):
        if not TextView.dispatching:
            raise AttributeError(f"'super' object has no attribute '{name}'")
        self.super_calls.append(name)

    def initWithFrame_(self, frame):
        View.initWithFrame_(self, frame)
        self.text, self.super_calls, self.needs_display = "", [], False
        return self

    def string(self):
        return self.text

    def setString_(self, text):
        self._objc_only("setString_")
        self.text = text

    def didChangeText(self):
        self._objc_only("didChangeText")

    def drawRect_(self, _rect):
        self._objc_only("drawRect_")

    def setNeedsDisplay_(self, value):
        self.needs_display = value

    def bounds(self):
        return self._frame

    def textContainerInset(self):
        return types.SimpleNamespace(width=6, height=8)

    def textContainer(self):
        return types.SimpleNamespace(lineFragmentPadding=lambda: 5)

    def font(self):
        return "font-13"


def objc_super(cls, obj):
    """Fake objc.super: dispatches to the next class's selector."""
    base = cls.__mro__[1]

    class Proxy:
        def __getattr__(self, name):
            method = getattr(base, name)

            def call(*args):
                TextView.dispatching = True
                try:
                    return method(obj, *args)
                finally:
                    TextView.dispatching = False

            return call

    return Proxy()


class AttributedString:
    drawn: ClassVar[list] = []

    @classmethod
    def alloc(cls):
        return cls()

    def initWithString_attributes_(self, text, attributes):
        self.text, self.attributes = text, attributes
        return self

    def drawInRect_(self, rect):
        AttributedString.drawn.append((self.text, self.attributes, rect))


class Color:
    def CGColor(self):
        return "cgcolor"


@pytest.fixture
def mac_style(monkeypatch):
    fonts = []
    appkit = types.ModuleType("AppKit")
    appkit.NSView = View
    appkit.NSButton = View  # base of mac_style.CobaltSwitch
    appkit.NSTextField = Label
    appkit.NSTextView = TextView
    appkit.NSAttributedString = AttributedString
    appkit.NSFontAttributeName = "font"
    appkit.NSForegroundColorAttributeName = "color"
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
    fake_objc = types.ModuleType("objc")
    fake_objc.super = objc_super
    fake_objc.python_method = lambda function: function
    AttributedString.drawn = []
    monkeypatch.setitem(sys.modules, "AppKit", appkit)
    monkeypatch.setitem(sys.modules, "Foundation", foundation)
    monkeypatch.setitem(sys.modules, "objc", fake_objc)
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


def _placeholder_view(mac_style):
    view = mac_style.PlaceholderTextView.alloc().initWithFrame_(Rect(0, 0, 300, 120))
    view.setPlaceholder_color_("Type notes", "muted-color")
    return view


def test_placeholder_view_draws_through_objc_super_and_only_when_empty(mac_style):
    # Regression: builtin super() raised AttributeError in drawRect_ and
    # crashed the native snapshot run on the Meetings page.
    view = _placeholder_view(mac_style)
    view.drawRect_(Rect(0, 0, 300, 120))
    assert view.super_calls == ["drawRect_"]
    [(text, attributes, rect)] = AttributedString.drawn
    assert text == "Type notes" and attributes["color"] == "muted-color"
    assert attributes["font"] == "font-13"
    # Inside the text inset and line padding, never stored as text.
    assert (rect.origin.x, rect.origin.y) == (6 + 5, 8)
    assert rect.size.width == 300 - 2 * (6 + 5) and rect.size.height == 120 - 2 * 8
    assert view.string() == ""


def test_placeholder_view_set_string_and_edits_dispatch_to_nstextview(mac_style):
    view = _placeholder_view(mac_style)
    view.setString_("Real notes")
    assert view.string() == "Real notes"  # the real text, unchanged
    assert view.needs_display is True
    view.needs_display = False
    view.didChangeText()
    assert view.needs_display is True
    view.drawRect_(Rect(0, 0, 300, 120))
    assert view.super_calls == ["setString_", "didChangeText", "drawRect_"]
    assert AttributedString.drawn == []  # real text: no placeholder drawn
    # Emptying the box brings the placeholder back; the text stays empty.
    view.setString_("")
    view.drawRect_(Rect(0, 0, 300, 120))
    assert [entry[0] for entry in AttributedString.drawn] == ["Type notes"]
    assert view.string() == ""


def test_placeholder_overrides_never_use_builtin_super():
    source = SRC.read_text(encoding="utf-8")
    cls = next(
        node for node in ast.parse(source).body
        if isinstance(node, ast.ClassDef) and node.name == "PlaceholderTextView"
    )
    for method in cls.body:
        if not isinstance(method, ast.FunctionDef):
            continue
        body = ast.get_source_segment(source, method)
        assert "super()" not in body, method.name
        if method.name in {"drawRect_", "didChangeText", "setString_"}:
            assert f"objc.super(PlaceholderTextView, self).{method.name}(" in body
