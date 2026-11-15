"""Ask page state across rebuild/navigation, without AppKit.

The real `AskPageController` (ask_mac.py) is loaded with stub AppKit/objc
modules, and the real `HubWindowController._page_ask` is AST-extracted, so the
lazy singleton, `capture`, `render`, `send_`, `tick_` and `shutdown` all run for
real. Only the Hub's view teardown is simulated by `FakeHub.show_page`, whose
contract (capture before teardown) is pinned against the real source below.
"""

import ast
import importlib.util
import sys
import textwrap
import threading
import types
from datetime import datetime, timedelta
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from localflow import ask_model as ask
from localflow.config import Config
from localflow.recall import Passage

ROOT = Path(__file__).parent.parent
HUB_MAC = ROOT / "localflow" / "hub_mac.py"


class FakeField:
    def __init__(self, value):
        self.value = value
        self.enabled = True

    def stringValue(self):
        return self.value

    def setStringValue_(self, value):
        self.value = value

    def __getattr__(self, name):  # setPlaceholderString_, setTarget_, ...
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


def _stub_modules():
    appkit = MagicMock(name="AppKit")
    objc = types.ModuleType("objc")
    objc.super = lambda cls, obj: super(cls, obj)
    objc.python_method = lambda fn: fn
    objc.selector = lambda fn, signature=None: fn
    foundation = types.ModuleType("Foundation")
    foundation.NSObject = FakeNSObject
    foundation.NSTimer = MagicMock(name="NSTimer")
    foundation.NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_ = (
        lambda *args: FakeTimer()
    )
    style = types.ModuleType("localflow.mac_style")
    style.FlippedView = MagicMock(name="FlippedView")
    style.style_button = lambda *args, **kwargs: None
    style.style_surface = lambda *args, **kwargs: None
    hub_mac = types.ModuleType("localflow.hub_mac")
    hub_mac._label = lambda *args, **kwargs: MagicMock(name="label")
    return {
        "AppKit": appkit, "objc": objc, "Foundation": foundation,
        "localflow.mac_style": style, "localflow.hub_mac": hub_mac,
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

    def _content_width(self):
        return 640

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
        self.cursor_y = 0
        self.shown.append(page)
        if page == "Ask":
            self._page_ask()

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
