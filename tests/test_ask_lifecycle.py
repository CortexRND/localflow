"""Ask page state across rebuild/navigation, without AppKit.

The real `AskPageController` (ask_mac.py) is loaded with stub AppKit/objc
modules, and the real `HubWindowController._page_ask` is AST-extracted, so the
lazy singleton, `capture`, `render`, `send_`, `tick_` and `shutdown` all run for
real. Only the Hub's view teardown is simulated by `FakeHub.show_page`, whose
contract (capture before teardown) is pinned against the real source below.
"""

import ast
import importlib.util
import math
import sys
import textwrap
import threading
import types
from datetime import datetime, timedelta
from pathlib import Path
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from localflow import ask_model as ask
from localflow.config import Config
from localflow.recall import Passage
from localflow.ui_tokens import HUB_HEADER_OFFSET, HUB_PAGE_BOTTOM

ROOT = Path(__file__).parent.parent
HUB_MAC = ROOT / "localflow" / "hub_mac.py"


class FakeEditor:
    """The field editor of a field being edited: only the caret is modelled."""

    def __init__(self, selection=(0, 0)):
        self.selection = selection

    def selectedRange(self):
        return types.SimpleNamespace(location=self.selection[0], length=self.selection[1])

    def setSelectedRange_(self, rng):
        self.selection = tuple(rng)


class FakeField:
    def __init__(self, value):
        self.value = value
        self.enabled = True
        self.editor = None  # set while the field is the first responder

    def stringValue(self):
        return self.value

    def setStringValue_(self, value):
        self.value = value

    def setEnabled_(self, enabled):
        self.enabled = enabled

    def isEnabled(self):
        return self.enabled

    def currentEditor(self):
        return self.editor

    def __getattr__(self, name):  # setPlaceholderString_, setTarget_, ...
        return lambda *args, **kwargs: None


class FakeButton:
    def __init__(self, title):
        self.title = title
        self.enabled = True

    def setEnabled_(self, enabled):
        self.enabled = enabled

    def __getattr__(self, name):
        return lambda *args, **kwargs: None


class FakeDocument:
    """Conversation document view: remembers its frame and scrolls its scroller."""

    def __init__(self):
        self.scroll = None
        self.rect = (0, 0, 0, 0)

    @classmethod
    def alloc(cls):
        return cls()

    def initWithFrame_(self, rect):
        self.rect = rect
        return self

    def frame(self):
        return types.SimpleNamespace(size=types.SimpleNamespace(height=self.rect[3]))

    def scrollPoint_(self, point):
        self.scroll.y = point[1]

    def __getattr__(self, name):
        return lambda *args, **kwargs: None


class FakeScroll:
    instances: ClassVar[list] = []

    def __init__(self):
        self.y = 0
        self.rect = (0, 0, 0, 0)
        self.document = None

    @classmethod
    def alloc(cls):
        instance = cls()
        cls.instances.append(instance)
        return instance

    def initWithFrame_(self, rect):
        self.rect = rect
        return self

    def setDocumentView_(self, document):
        self.document = document
        document.scroll = self

    def documentView(self):
        return self.document

    def contentView(self):
        bounds = types.SimpleNamespace(
            origin=types.SimpleNamespace(y=self.y),
            size=types.SimpleNamespace(height=self.rect[3]),
        )
        return types.SimpleNamespace(bounds=lambda: bounds)

    @property
    def limit(self):
        return max(0, self.document.rect[3] - self.rect[3])

    def __getattr__(self, name):
        return lambda *args, **kwargs: None


class FakeTimer:
    def __init__(self):
        self.valid = True

    def invalidate(self):
        self.valid = False


class FakeNSObject:
    @classmethod
    def alloc(cls):
        return cls.__new__(cls)

    def init(self):
        return self


LABELS = []  # every text the page put into a label
BUTTONS = []  # every button / checkbox the page created


def _stub_modules():
    LABELS.clear()
    BUTTONS.clear()
    FakeScroll.instances.clear()
    appkit = MagicMock(name="AppKit")
    appkit.NSScrollView = FakeScroll
    appkit.NSTextField.labelWithString_ = lambda text: LABELS.append(text) or MagicMock()

    def make_button(title, _target, _action):
        button = FakeButton(title)
        BUTTONS.append(button)
        return button

    appkit.NSButton.buttonWithTitle_target_action_ = make_button
    appkit.NSButton.checkboxWithTitle_target_action_ = make_button
    objc = types.ModuleType("objc")
    objc.super = lambda cls, obj: super(cls, obj)
    objc.python_method = lambda fn: fn
    objc.selector = lambda fn, signature=None: fn
    foundation = types.ModuleType("Foundation")
    foundation.NSObject = FakeNSObject
    foundation.NSMakeRect = lambda *rect: tuple(rect)
    foundation.NSMakePoint = lambda *point: tuple(point)
    foundation.NSMakeRange = lambda *rng: tuple(rng)
    foundation.NSTimer = MagicMock(name="NSTimer")
    foundation.NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_ = (
        lambda *args: FakeTimer()
    )
    style = types.ModuleType("localflow.mac_style")
    style.FlippedView = FakeDocument
    style.style_button = lambda *args, **kwargs: None
    style.style_chip = lambda *args, **kwargs: None
    style.style_bubble = lambda *args, **kwargs: None
    style.color = lambda token, palette=None: token
    style.font = lambda size, weight="regular": (size, weight)
    # A fixed-pitch stand-in for AppKit text measurement.
    style.measure_text = lambda text, f: math.ceil(len(text) * f[0] * 0.56)
    return {
        "AppKit": appkit, "objc": objc, "Foundation": foundation,
        "localflow.mac_style": style,
    }


@pytest.fixture
def ask_mac(monkeypatch):
    for name, module in _stub_modules().items():
        monkeypatch.setitem(sys.modules, name, module)
    spec = importlib.util.spec_from_file_location(
        "localflow.ask_mac", ROOT / "localflow" / "ask_mac.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    monkeypatch.setitem(sys.modules, "localflow.ask_mac", module)
    return module


def _extract_hub_method(name, namespace):
    source = HUB_MAC.read_text(encoding="utf-8")
    cls = next(
        n for n in ast.parse(source).body
        if isinstance(n, ast.ClassDef) and n.name == "HubWindowController"
    )
    node = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == name)
    text = " " * node.col_offset + ast.get_source_segment(source, node)
    exec(textwrap.dedent(text), namespace)  # noqa: S102 - our own source
    return namespace[name]


class FakeWindow:
    def __init__(self, hub):
        self.hub = hub

    def makeFirstResponder_(self, field):
        field.editor = FakeEditor()  # becoming first responder starts an edit
        return True


class FakeHub:
    """Hub teardown/rebuild stand-in: views are discarded, only page state stays."""

    _page_ask = None  # bound below from the real hub_mac source

    def __init__(self):
        self.page = "Home"
        self.history = object()
        self.tokens = object()
        self.body = MagicMock(name="body")
        self.cursor_y = 0
        self._closed = False
        self.fields = []
        self.shown = []
        self.width = 640
        self.viewport = 576
        self.window = FakeWindow(self)

    def _content_width(self):
        return self.width

    def _viewport_height(self):
        return self.viewport

    def _field(self, value, *args, **kwargs):
        field = FakeField(value)
        self.fields.append(field)
        return field

    def show_page(self, page):
        # Mirrors HubWindowController.show_page (pinned by the AST test below):
        # capture the leaving Ask page, tear views down, then build the page.
        if self.page == "Ask" and hasattr(self, "ask_page"):
            self.ask_page.capture()
        self.page = page
        self.fields = []  # the old views are gone
        self.cursor_y = HUB_HEADER_OFFSET
        self.shown.append(page)
        if page == "Ask":
            self._page_ask()

    @property
    def scroll(self):
        """The conversation scroller of the newest Ask paint."""
        return FakeScroll.instances[-1]

    def rebuild(self):  # resize / appearance
        self.show_page(self.page)

    @property
    def live_field(self):
        return self.fields[-1] if self.fields else None


@pytest.fixture
def hub(ask_mac):
    FakeHub._page_ask = _extract_hub_method("_page_ask", {})
    return FakeHub()


class GatedLLM:
    def __init__(self):
        self.release = threading.Event()
        self.calls = 0

    def complete(self, system, user):
        self.calls += 1
        assert self.release.wait(5)
        return "Launch is Friday [1]."


@pytest.fixture
def corpus(monkeypatch):
    # Relative to the real clock: the worker ranks with the current time, so a
    # fixed date would fall outside the "Past month" filter.
    now = (datetime.now() - timedelta(days=1)).timestamp()
    passages = [
        Passage("Launch Friday", "meeting", "Planning", now, "/vault/m.md#01:20"),
        Passage("Launch note", "notes", "Checklist", now, "/vault/n.md"),
    ]
    monkeypatch.setattr(ask.recall, "build_corpus", lambda **kwargs: passages)


def _attach_llm(controller, llm):
    controller.model.llm = llm
    controller.model.config = Config()


def _wait_result(controller, timeout=5):
    deadline = threading.Event()
    for _ in range(int(timeout / 0.01)):
        if not controller.events.empty():
            return
        deadline.wait(0.01)
    raise AssertionError("worker never answered")


def test_controller_and_model_are_created_once(hub):
    hub.show_page("Ask")
    first = hub.ask_page
    hub.show_page("Home")
    hub.show_page("Ask")
    hub.rebuild()
    assert hub.ask_page is first


def test_draft_filters_and_sources_survive_rebuild_and_navigation(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    page.model.turns.append(ask.Turn("Earlier?", "Earlier answer."))
    hub.live_field.setStringValue_("half typed question")
    page.time_filter = "Past week"
    page.sources.discard("meeting")

    hub.rebuild()  # resize or appearance change
    assert hub.live_field.stringValue() == "half typed question"
    assert page.time_filter == "Past week"
    assert page.sources == {"dictation", "notes"}

    hub.live_field.setStringValue_("typed after resize")
    hub.show_page("Models")  # navigate away: views gone
    assert hub.live_field is None and hub.page == "Models"
    hub.show_page("Ask")  # and back
    assert hub.live_field.stringValue() == "typed after resize"
    assert [t.question for t in page.model.turns] == ["Earlier?"]
    assert page.time_filter == "Past week"
    assert page.sources == {"dictation", "notes"}


def test_busy_response_survives_rebuild_and_lands_while_elsewhere(hub, corpus):
    hub.show_page("Ask")
    page = hub.ask_page
    llm = GatedLLM()
    _attach_llm(page, llm)
    page.time_filter = "Past month"
    hub.live_field.setStringValue_("When is launch?")
    page.send_(None)

    assert page.busy and page.pending == "When is launch?"
    hub.rebuild()
    assert page.busy and page.pending == "When is launch?"
    assert page.draft == ""  # the sent question is not resurrected as a draft

    hub.show_page("Style")
    hub.show_page("Ask")
    assert page.busy and page.pending == "When is launch?"
    hub.show_page("Dictionary")

    llm.release.set()
    _wait_result(page)
    page.tick_(None)  # the run-loop timer fires while another page is showing
    assert hub.page == "Dictionary"  # a background answer never switches pages
    assert hub.shown[-1] == "Dictionary"
    assert not page.busy and page.pending == ""
    assert [t.question for t in page.model.turns] == ["When is launch?"]
    assert page.model.turns[0].answer == "Launch is Friday [1]."

    hub.show_page("Ask")
    assert [t.question for t in page.model.turns] == ["When is launch?"]
    assert page.time_filter == "Past month"
    assert llm.calls == 1


def test_answer_arriving_on_ask_repaints_in_place(hub, corpus):
    hub.show_page("Ask")
    page = hub.ask_page
    _attach_llm(page, GatedLLM())
    page.model.llm.release.set()
    hub.live_field.setStringValue_("When is launch?")
    page.send_(None)
    _wait_result(page)
    hub.shown.clear()
    page.tick_(None)
    assert hub.shown == ["Ask"]
    assert hub.live_field.stringValue() == ""
    assert not page.busy


def test_empty_send_and_clear_do_not_leak_draft(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    hub.live_field.setStringValue_("   ")
    page.send_(None)
    assert not page.busy
    page.model.turns.append(ask.Turn("q", "a"))
    hub.live_field.setStringValue_("draft before clear")
    page.clear_(None)
    assert page.model.turns == [] and hub.live_field.stringValue() == ""


def test_closed_hub_is_never_touched_by_a_late_result(hub, corpus):
    hub.show_page("Ask")
    page = hub.ask_page
    llm = GatedLLM()
    _attach_llm(page, llm)
    hub.live_field.setStringValue_("When is launch?")
    page.send_(None)
    timer = page.timer

    hub._closed = True
    page.shutdown()  # what windowWillClose_ does
    assert timer.valid is False and page.timer is None

    llm.release.set()
    _wait_result(page)
    hub.shown.clear()
    hub.page = "Ask"
    page.tick_(None)  # even if a stray tick arrives, no repaint
    assert hub.shown == []
    assert [t.question for t in page.model.turns] == ["When is launch?"]
    page.send_(None)  # a closed page has no field; must not raise


def _controller_calls(name):
    source = HUB_MAC.read_text(encoding="utf-8")
    cls = next(
        n for n in ast.parse(source).body
        if isinstance(n, ast.ClassDef) and n.name == "HubWindowController"
    )
    node = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == name)
    return node


def test_real_hub_captures_before_teardown_and_shuts_down_on_close():
    show_page = _controller_calls("show_page")
    capture = remove = None
    for call in ast.walk(show_page):
        if not isinstance(call, ast.Call) or not isinstance(call.func, ast.Attribute):
            continue
        if call.func.attr == "capture":
            capture = call.lineno
        if call.func.attr == "removeFromSuperview":
            remove = call.lineno
    assert capture is not None and remove is not None and capture < remove
    assert "self.ask_page.capture()" in ast.get_source_segment(
        HUB_MAC.read_text(encoding="utf-8"), show_page
    )
    closing = ast.get_source_segment(
        HUB_MAC.read_text(encoding="utf-8"), _controller_calls("windowWillClose_")
    )
    assert "self.ask_page.shutdown()" in closing
    # Resize and appearance both rebuild through show_page.
    rebuild = ast.get_source_segment(
        HUB_MAC.read_text(encoding="utf-8"), _controller_calls("_rebuild_current_page")
    )
    assert "self.show_page(self.page)" in rebuild


# ------------------------------------------------------- redesigned layout ---

def _turn(index, *, long=False):
    text = f"Answer sentence number {index}. " * (30 if long else 1)
    return ask.Turn(f"Question {index}?", text.strip())


def test_no_duplicate_heading_and_privacy_sits_small_below_the_composer(hub):
    hub.show_page("Ask")
    assert "Ask your notes & history" not in LABELS
    assert LABELS.count(ask.PRIVACY) == 1
    layout = hub.ask_page.layout
    assert layout["privacy"][1] > layout["field"][1] + layout["field"][3]


def test_empty_page_has_compact_example_chips_intro_and_a_disabled_clear(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    assert [p["kind"] for p in page.convo_layout] == ["intro", "chip", "chip", "chip"]
    chips = [p for p in page.convo_layout if p["kind"] == "chip"]
    assert [c["text"] for c in chips] == list(ask.EXAMPLES)
    assert all(c["w"] < hub.width for c in chips)  # measured, never full width
    assert page.clear_button.enabled is False


def test_clear_needs_a_conversation_and_no_running_answer(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    page.model.turns.append(ask.Turn("q", "a"))
    hub.rebuild()
    assert page.clear_button.enabled is True
    page.busy, page.pending = True, "next question"
    hub.rebuild()
    assert page.clear_button.enabled is False
    texts = [p["text"] for p in page.convo_layout]
    assert "next question" in texts and "Thinking..." in texts
    assert not any(p["kind"] == "chip" for p in page.convo_layout)  # examples gone
    assert hub.live_field.enabled is False


def test_turns_render_as_aligned_bubbles_with_citation_chips(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    citation = {"n": 1, "source": "meeting", "when": "2026-04-15T12:00",
                "title": "Launch planning", "ref": "/vault/m.md#01:20"}
    page.model.turns.append(ask.Turn("Launch?", "Friday [1].", [citation]))
    page.model.turns.append(ask.Turn("Again?", error="Couldn't reach the model."))
    hub.rebuild()
    layout = page.convo_layout
    users = [p for p in layout if p["kind"] == "user"]
    cards = [p for p in layout if p["kind"] == "assistant"]
    assert [u["x"] + u["w"] for u in users] == [hub.width, hub.width]  # right aligned
    assert [c["x"] for c in cards] == [0, 0]  # left aligned
    assert [c["error"] for c in cards] == [False, True]
    cites = [p for p in layout if p["kind"] == "cite"]
    assert [c["text"] for c in cites] == [ask.citation_label(citation)]
    assert cites[0]["w"] < hub.width and page.citations == [citation]


@pytest.mark.parametrize(("width", "viewport"), [(596, 576), (1016, 772), (700, 600)])
def test_page_fits_the_viewport_so_the_composer_stays_pinned(hub, width, viewport):
    hub.width, hub.viewport = width, viewport
    hub.show_page("Ask")
    page = hub.ask_page
    page.model.turns.extend(_turn(n, long=True) for n in range(12))
    hub.rebuild()
    assert hub.cursor_y == page.layout["bottom"] <= viewport - HUB_PAGE_BOTTOM
    # The long conversation scrolls inside its own view, not the outer page.
    assert hub.scroll.document.rect[3] > hub.scroll.rect[3]
    assert hub.scroll.rect[3] == page.layout["conversation"][3]
    assert page.layout["conversation"][2] == width  # tracks the width on rebuild


def test_conversation_keeps_its_place_on_resize_and_follows_new_answers(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    page.model.turns.extend(_turn(n, long=True) for n in range(8))
    hub.rebuild()
    assert hub.scroll.y == hub.scroll.limit > 0  # first paint rests at the bottom

    hub.scroll.y = 120  # the reader scrolled up
    hub.rebuild()  # appearance change
    assert hub.scroll.y == 120
    hub.width = 900  # resize
    hub.rebuild()
    assert hub.scroll.y == 120

    hub.scroll.y = hub.scroll.limit  # reader is at the bottom
    hub.width = 700
    hub.rebuild()
    assert hub.scroll.y == hub.scroll.limit

    hub.scroll.y = 50  # a new answer lands: follow it
    page.events.put(list(page.model.turns) + [ask.Turn("New?", "Fresh answer.")])
    page.tick_(None)
    assert hub.scroll.y == hub.scroll.limit and not page.follow


def test_answer_landing_elsewhere_is_followed_when_the_page_is_next_shown(hub):
    hub.show_page("Ask")
    page = hub.ask_page
    page.model.turns.extend(_turn(n, long=True) for n in range(8))
    hub.rebuild()
    hub.scroll.y = 40
    hub.show_page("Home")
    page.events.put(list(page.model.turns) + [ask.Turn("New?", "Fresh answer.")])
    page.tick_(None)
    assert page.follow and hub.shown[-1] == "Home"
    hub.show_page("Ask")
    assert hub.scroll.y == hub.scroll.limit and not page.follow


def test_caret_and_focus_survive_a_repaint_without_stealing_focus(hub):
    hub.show_page("Ask")
    field = hub.live_field
    field.setStringValue_("hello world")
    field.editor = FakeEditor((3, 2))
    hub.rebuild()
    assert hub.live_field.stringValue() == "hello world"
    assert hub.live_field.editor.selection == (3, 2)

    hub.live_field.editor = None  # the user clicked elsewhere
    hub.rebuild()
    assert hub.live_field.editor is None  # never steals focus back


def test_caret_after_an_emoji_is_restored_in_utf16_units(hub):
    hub.show_page("Ask")
    hub.live_field.setStringValue_("hi 😀")  # 4 characters, 5 UTF-16 units
    hub.live_field.editor = FakeEditor((5, 0))  # caret at the very end
    hub.rebuild()
    assert hub.live_field.editor.selection == (5, 0)


def test_focus_returns_to_the_composer_after_an_answer(hub, corpus):
    hub.show_page("Ask")
    page = hub.ask_page
    _attach_llm(page, GatedLLM())
    page.model.llm.release.set()
    hub.live_field.setStringValue_("When is launch?")
    page.send_(None)
    assert hub.live_field.editor is None  # disabled while thinking
    _wait_result(page)
    page.tick_(None)
    assert hub.live_field.editor is not None and hub.live_field.enabled


def test_ask_source_has_no_standalone_heading_and_uses_the_pure_layout():
    source = (ROOT / "localflow" / "ask_mac.py").read_text(encoding="utf-8")
    assert "Ask your notes" not in source
    for helper in ("page_layout(", "conversation_layout(", "resolve_scroll("):
        assert helper in source
