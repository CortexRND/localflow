"""Native Ask surface, kept separate from the shared Hub shell."""

import math
import queue
import threading

import AppKit
import objc
from Foundation import NSObject, NSTimer

from localflow.ask_model import (
    EXAMPLES,
    PRIVACY,
    SOURCES,
    TIME_FILTERS,
    AskModel,
    citation_label,
    citation_target,
)
from localflow.mac_style import FlippedView, style_button, style_surface
from localflow.platform import current


class AskPageController(NSObject):
    def initWithHub_(self, hub):
        self = objc.super(AskPageController, self).init()  # noqa: PLW0642
        if self is None:
            return None
        self.hub = hub
        self.model = AskModel(store=hub.history)
        self.events = queue.Queue()
        self.busy = False
        self.pending = ""
        self.draft = ""
        self.time_filter = "Any time"
        self.sources = set(SOURCES)
        self.field = None
        self.closed = False
        self.timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            0.2, self, objc.selector(self.tick_, signature=b"v@:@"), None, True,
        )
        return self

    @objc.python_method
    def button(self, title, action, x, y, width, *, tag=0, kind="secondary"):
        button = AppKit.NSButton.buttonWithTitle_target_action_(title, self, action)
        button.setFrame_(AppKit.NSMakeRect(x, y, width, 36))
        button.setTag_(tag)
        style_button(button, kind, self.hub.tokens)
        self.hub.body.addSubview_(button)
        return button

    @objc.python_method
    def text(self, text, *, color="ink", size=13, card=False):
        from localflow.hub_mac import _label

        hub = self.hub
        width = hub._content_width()
        inset = 16 if card else 0
        columns = max(20, int((width - inset * 2) / (size * 0.56)))
        lines = sum(max(1, math.ceil(len(line) / columns)) for line in text.split("\n"))
        height = max(24, lines * (size + 6)) + inset * 2
        label = _label(
            text, inset, inset if card else hub.cursor_y, width - inset * 2,
            size, color, palette=hub.tokens, height=height - inset * 2, wrap=True,
        )
        if card:
            surface = FlippedView.alloc().initWithFrame_(
                AppKit.NSMakeRect(0, hub.cursor_y, width, height)
            )
            style_surface(surface, hub.tokens)
            surface.addSubview_(label)
            hub.body.addSubview_(surface)
        else:
            hub.body.addSubview_(label)
        hub.cursor_y += height + 12

    @objc.python_method
    def capture(self):
        """Read the typed draft; the hub calls this before tearing views down.

        The field is dropped afterwards so a stale (removed) view is never read
        again. Everything else (turns, filters, sources, busy/pending) lives on
        this long-lived controller and needs no capture.
        """
        if self.field is not None:
            self.draft = self.field.stringValue()
            self.field = None

    @objc.python_method
    def shutdown(self):
        """The Hub window closed: stop the timer so no late result touches it."""
        self.closed = True
        self.field = None
        if self.timer is not None:
            self.timer.invalidate()
            self.timer = None

    @objc.python_method
    def render(self):
        hub = self.hub
        width = hub._content_width()
        self.text("Ask your notes & history", size=17)
        self.text(PRIVACY, color="muted", size=11)
        self.button("Clear conversation", "clear:", 0, hub.cursor_y, 170).setEnabled_(not self.busy)
        hub.cursor_y += 46
        self.time_popup = AppKit.NSPopUpButton.alloc().initWithFrame_pullsDown_(
            AppKit.NSMakeRect(0, hub.cursor_y, 160, 32), False,
        )
        self.time_popup.addItemsWithTitles_(list(TIME_FILTERS))
        self.time_popup.selectItemWithTitle_(self.time_filter)
        self.time_popup.setTarget_(self)
        self.time_popup.setAction_("filterChanged:")
        self.time_popup.setEnabled_(not self.busy)
        hub.body.addSubview_(self.time_popup)
        hub.cursor_y += 42
        for index, (source, label) in enumerate(SOURCES.items()):
            button = self.button(label, "sourceChanged:", index * 110, hub.cursor_y, 105, tag=index)
            button.setButtonType_(AppKit.NSSwitchButton)
            button.setState_(int(source in self.sources))
            button.setEnabled_(not self.busy)
        hub.cursor_y += 48
        if not self.model.turns and not self.busy:
            self.text("Find decisions, ideas, and next steps across your local sources.", color="muted")
            for index, question in enumerate(EXAMPLES):
                self.button(question, "example:", 0, hub.cursor_y, width, tag=index)
                hub.cursor_y += 44
        self.citations = []
        for turn in self.model.turns:
            self.text("You · " + turn.question, size=14)
            self.text(turn.error or turn.answer, color="red" if turn.error else "ink", card=True)
            for citation in turn.citations:
                index = len(self.citations)
                self.citations.append(citation)
                self.button(citation_label(citation), "openCitation:", 0, hub.cursor_y, width, tag=index)
                hub.cursor_y += 40
        if self.busy:
            self.text("You · " + self.pending, size=14)
            self.text("Thinking...", color="muted", card=True)
        self.field = hub._field(self.draft, 0, hub.cursor_y, width - 100)
        self.field.setPlaceholderString_("Ask a question about your notes or dictations")
        self.field.setTarget_(self)
        self.field.setAction_("send:")
        self.field.setEnabled_(not self.busy)
        self.button("Ask", "send:", width - 90, hub.cursor_y, 90, kind="primary").setEnabled_(not self.busy)
        hub.cursor_y += 48

    def filterChanged_(self, sender):
        self.time_filter = sender.titleOfSelectedItem()

    def sourceChanged_(self, sender):
        source = list(SOURCES)[sender.tag()]
        if sender.state():
            self.sources.add(source)
        else:
            self.sources.discard(source)

    def clear_(self, _sender):
        if self.busy:
            return
        self.model.clear()
        self.field = None
        self.draft = ""
        self.hub.show_page("Ask")

    def example_(self, sender):
        self.field.setStringValue_(EXAMPLES[sender.tag()])
        self.send_(None)

    def send_(self, _sender):
        if self.busy or self.field is None:
            return
        question = self.field.stringValue().strip()
        if not question:
            return
        self.busy = True
        self.pending = question
        self.draft = ""
        self.field = None
        time_filter, sources = self.time_filter, set(self.sources)
        worker = AskModel(llm=self.model.llm, config=self.model.config, store=self.model.store)
        worker.turns = list(self.model.turns)

        def run():
            worker.ask(question, time_filter=time_filter, sources=sources)
            self.events.put(worker.turns)

        threading.Thread(target=run, name="localflow-ask", daemon=True).start()
        self.hub.show_page("Ask")

    def tick_(self, _timer):
        try:
            self.model.turns = self.events.get_nowait()
        except queue.Empty:
            return
        self.busy = False
        self.pending = ""
        # The answer is stored above regardless; only repaint when the user is
        # still looking at Ask (never navigate, never touch a closed Hub).
        if not self.closed and self.hub.page == "Ask":
            self.hub.show_page("Ask")

    def openCitation_(self, sender):
        citation = self.citations[sender.tag()]
        kind, target = citation_target(citation)
        if kind == "history":
            row = self.hub.history.get(target)
            alert = AppKit.NSAlert.alloc().init()
            alert.setMessageText_(citation_label(citation))
            alert.setInformativeText_(
                (row.get("final_text") or row.get("raw_text") or "(no text)")
                if row else "This history entry is no longer available."
            )
            alert.runModal()
        else:
            try:
                current().open_path(target)
            except OSError:
                alert = AppKit.NSAlert.alloc().init()
                alert.setMessageText_("Couldn't open that source file.")
                alert.setInformativeText_("It may have been moved or deleted.")
                alert.runModal()
