"""Native Ask surface, kept separate from the shared Hub shell.

Layout (all geometry comes from ask_layout, measured with the real fonts):
one compact controls row, a conversation in its own scroll view, then the
composer and the privacy note pinned to the bottom of the Hub viewport.
"""

import queue
import threading

import AppKit
import objc
from Foundation import NSMakePoint, NSMakeRange, NSMakeRect, NSObject, NSTimer

from localflow.ask_layout import (
    at_bottom,
    clamp_selection,
    conversation_layout,
    page_layout,
    resolve_scroll,
)
from localflow.ask_model import (
    EXAMPLES,
    PRIVACY,
    SOURCES,
    TIME_FILTERS,
    AskModel,
    citation_label,
    citation_target,
)
from localflow.mac_style import (
    FlippedView,
    color,
    font,
    measure_text,
    style_bubble,
    style_button,
    style_chip,
)
from localflow.platform import current
from localflow.ui_tokens import ASK_BUBBLE_PADDING, ASK_CONTROL_FONT

INTRO = "Find decisions, ideas, and next steps across your local sources."
THINKING = "Thinking..."


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
        self.citations = []
        self.layout = None
        self.convo_layout = []
        self.convo_scroll = None
        self.clear_button = None
        # Conversation scroller memory: where the reader was (None before the
        # first paint), whether that was the bottom, and whether the next paint
        # should follow a just-sent or just-arrived turn.
        self.scroll_y = None
        self.was_at_bottom = True
        self.follow = False
        # Composer focus and caret survive a repaint (resize, appearance, answer).
        self.focus = True
        self.selection = None
        self.timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            0.2, self, objc.selector(self.tick_, signature=b"v@:@"), None, True,
        )
        return self

    # ------------------------------------------------------------- helpers ---

    @objc.python_method
    def _measure(self, text, size, weight="regular"):
        return measure_text(text, font(size, weight))

    @objc.python_method
    def _label(self, text, frame, size, token, *, wrap=False, weight="regular"):
        label = AppKit.NSTextField.labelWithString_(text)
        label.setFrame_(NSMakeRect(*frame))
        label.setFont_(font(size, weight))
        label.setTextColor_(color(token, self.hub.tokens))
        if wrap:
            label.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
            label.setMaximumNumberOfLines_(0)
        else:
            label.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
        return label

    @objc.python_method
    def _items(self):
        """What the conversation shows; also rebuilds the citation list."""
        self.citations = []
        items = []
        if not self.model.turns and not self.busy:
            items.append(("intro", INTRO))
            items.append(("examples", list(EXAMPLES)))
        for turn in self.model.turns:
            items.append(("user", turn.question))
            items.append(("assistant", turn.error or turn.answer, bool(turn.error)))
            if turn.citations:
                self.citations.extend(turn.citations)
                items.append(("citations", [citation_label(c) for c in turn.citations]))
        if self.busy:
            items.append(("user", self.pending))
            items.append(("assistant", THINKING, False))
        return items

    # ----------------------------------------------------- state capture ---

    @objc.python_method
    def capture(self):
        """Read the typed draft, caret and scroll offset before the hub tears views down.

        The field and scroller are dropped afterwards so a stale (removed) view
        is never read again. Everything else (turns, filters, sources,
        busy/pending) lives on this long-lived controller and needs no capture.
        """
        if self.field is not None:
            self.draft = self.field.stringValue()
            editor = self.field.currentEditor()  # only while it is being edited
            if editor is not None:
                selected = editor.selectedRange()
                self.selection = (int(selected.location), int(selected.length))
            if editor is not None or self.field.isEnabled():
                # A disabled (busy) field cannot hold focus; keep the earlier
                # intent so the caret returns when the answer lands.
                self.focus = editor is not None
            self.field = None
        if self.convo_scroll is not None:
            clip = self.convo_scroll.contentView().bounds()
            view_height = float(clip.size.height)
            content_height = float(self.convo_scroll.documentView().frame().size.height)
            self.scroll_y = float(clip.origin.y)
            self.was_at_bottom = at_bottom(self.scroll_y, content_height, view_height)
            self.convo_scroll = None
        self.clear_button = None

    @objc.python_method
    def shutdown(self):
        """The Hub window closed: stop the timer so no late result touches it."""
        self.closed = True
        self.field = None
        self.convo_scroll = None
        self.clear_button = None
        if self.timer is not None:
            self.timer.invalidate()
            self.timer = None

    # -------------------------------------------------------------- render ---

    @objc.python_method
    def render(self):
        hub = self.hub
        width = hub._content_width()
        layout = page_layout(
            width, hub._viewport_height(), hub.cursor_y, self._measure, PRIVACY,
        )
        self.layout = layout
        self._build_toolbar(layout)
        self._build_conversation(layout["conversation"])
        self._build_composer(layout)
        hub.body.addSubview_(
            self._label(PRIVACY, layout["privacy"], 11, "muted", wrap=True)
        )
        # The page fits the viewport, so the outer document never scrolls the
        # composer away.
        hub.cursor_y = layout["bottom"]
        self._restore_focus()

    @objc.python_method
    def _build_toolbar(self, layout):
        hub = self.hub
        toolbar, y = layout["toolbar"], layout["toolbar_y"]
        popup = AppKit.NSPopUpButton.alloc().initWithFrame_pullsDown_(
            NSMakeRect(0, y, toolbar["popup"][1], toolbar["height"]), False,
        )
        popup.setFont_(font(ASK_CONTROL_FONT))
        popup.addItemsWithTitles_(list(TIME_FILTERS))
        popup.selectItemWithTitle_(self.time_filter)
        popup.setTarget_(self)
        popup.setAction_("filterChanged:")
        popup.setEnabled_(not self.busy)
        hub.body.addSubview_(popup)
        self.time_popup = popup
        for index, (source, label) in enumerate(SOURCES.items()):
            spec = toolbar["toggles"][index]
            # Plain unbordered checkbox: the title carries an explicit colour so
            # it stays readable in dark mode.
            box = AppKit.NSButton.checkboxWithTitle_target_action_(
                label, self, "sourceChanged:",
            )
            box.setFrame_(NSMakeRect(spec["x"], y + 4, spec["w"], 20))
            box.setFont_(font(ASK_CONTROL_FONT))
            box.setAttributedTitle_(
                AppKit.NSAttributedString.alloc().initWithString_attributes_(
                    label,
                    {
                        AppKit.NSForegroundColorAttributeName: color("ink", hub.tokens),
                        AppKit.NSFontAttributeName: font(ASK_CONTROL_FONT),
                    },
                )
            )
            box.setTag_(index)
            box.setState_(int(source in self.sources))
            box.setEnabled_(not self.busy)
            hub.body.addSubview_(box)
        clear_x, clear_w = toolbar["clear"]
        clear = AppKit.NSButton.buttonWithTitle_target_action_(
            toolbar["clear_title"], self, "clear:",
        )
        clear.setFrame_(NSMakeRect(clear_x, y, clear_w, toolbar["height"]))
        style_button(clear, "secondary", hub.tokens)
        clear.setFont_(font(ASK_CONTROL_FONT, "semibold"))
        clear.setAutoresizingMask_(AppKit.NSViewMinXMargin)
        # Only meaningful with something to clear, and never mid-answer.
        clear.setEnabled_(bool(self.model.turns) and not self.busy)
        hub.body.addSubview_(clear)
        self.clear_button = clear

    @objc.python_method
    def _build_conversation(self, frame):
        hub = self.hub
        x, y, width, height = frame
        scroll = AppKit.NSScrollView.alloc().initWithFrame_(
            NSMakeRect(x, y, width, height)
        )
        scroll.setHasVerticalScroller_(True)
        scroll.setAutohidesScrollers_(True)
        scroll.setScrollerStyle_(AppKit.NSScrollerStyleOverlay)
        scroll.setBorderType_(AppKit.NSNoBorder)
        scroll.setDrawsBackground_(False)
        scroll.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        placements, content_height, anchor = conversation_layout(
            self._items(), width, self._measure,
        )
        self.convo_layout = placements
        document = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, max(content_height, height))
        )
        document.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        cite_index = 0
        for place in placements:
            if place["kind"] == "cite":
                self._add_chip(document, place, "openCitation:", cite_index)
                cite_index += 1
            else:
                self._add_placement(document, place)
        scroll.setDocumentView_(document)
        hub.body.addSubview_(scroll)
        self.convo_scroll = scroll
        target = resolve_scroll(
            self.scroll_y, self.was_at_bottom, self.follow, content_height,
            height, anchor,
        )
        self.follow = False
        document.scrollPoint_(NSMakePoint(0, target))

    @objc.python_method
    def _add_chip(self, document, place, action, tag):
        button = AppKit.NSButton.buttonWithTitle_target_action_(
            place["text"], self, action,
        )
        button.setFrame_(NSMakeRect(place["x"], place["y"], place["w"], place["h"]))
        button.setTag_(tag)
        style_chip(button, self.hub.tokens)
        button.setToolTip_(place["text"])
        document.addSubview_(button)

    @objc.python_method
    def _add_placement(self, document, place):
        kind = place["kind"]
        if kind == "intro":
            document.addSubview_(
                self._label(
                    place["text"], (place["x"], place["y"], place["w"], place["h"]),
                    13, "muted", wrap=True,
                )
            )
        elif kind == "chip":
            self._add_chip(document, place, "example:", place["tag"])
        else:
            bubble = FlippedView.alloc().initWithFrame_(
                NSMakeRect(place["x"], place["y"], place["w"], place["h"])
            )
            style_bubble(bubble, self.hub.tokens, role=kind)
            token = "red" if place["error"] else "ink"
            label = self._label(
                place["text"],
                (ASK_BUBBLE_PADDING, ASK_BUBBLE_PADDING, place["text_w"],
                 place["h"] - 2 * ASK_BUBBLE_PADDING),
                13, token, wrap=True,
            )
            label.setSelectable_(True)
            bubble.addSubview_(label)
            document.addSubview_(bubble)

    @objc.python_method
    def _build_composer(self, layout):
        hub = self.hub
        fx, fy, fw, _fh = layout["field"]
        self.field = hub._field(self.draft, fx, fy, fw)
        self.field.setPlaceholderString_("Ask a question about your notes or dictations")
        self.field.setTarget_(self)
        self.field.setAction_("send:")
        self.field.setEnabled_(not self.busy)
        self.field.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        ax, ay, aw, ah = layout["ask"]
        button = AppKit.NSButton.buttonWithTitle_target_action_("Ask", self, "send:")
        button.setFrame_(NSMakeRect(ax, ay, aw, ah))
        style_button(button, "primary", hub.tokens)
        button.setAutoresizingMask_(AppKit.NSViewMinXMargin)
        button.setEnabled_(not self.busy)
        hub.body.addSubview_(button)

    @objc.python_method
    def _restore_focus(self):
        """Put the caret back where it was; never steal focus while busy."""
        if self.busy or not self.focus or self.field is None:
            return
        self.hub.window.makeFirstResponder_(self.field)
        editor = self.field.currentEditor()
        if editor is not None:
            editor.setSelectedRange_(
                NSMakeRange(*clamp_selection(self.selection, self.draft))
            )
        self.selection = None

    # ------------------------------------------------------------- actions ---

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
        self.selection = None
        self.focus = True
        self.scroll_y = None
        self.was_at_bottom = True
        self.convo_scroll = None
        self.hub.show_page("Ask")

    def example_(self, sender):
        if self.field is None:
            return
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
        self.selection = None
        self.focus = True  # the caret returns to the composer with the answer
        self.follow = True
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
        self.follow = True
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
