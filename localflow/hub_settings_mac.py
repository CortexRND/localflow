"""Native Settings page of the Hub: plain-language tweaks via the local agent.

State and geometry live in hub_settings (AppKit-free); the rules live in
settings_agent. This controller only builds views and moves work off the
UI thread:

- Preview runs `SettingsAgent.propose` on a worker thread (a local model can
  take many seconds). The result comes back through
  performSelectorOnMainThread and is accepted only if its token is still
  current; it never touches a view directly, and the page is repainted only
  while the Hub is open and showing Settings.
- Apply / Undo are local file writes and run on the main thread.
- The typed request, preview, error and in-flight state live on this
  long-lived controller, so navigation, resizing and appearance changes
  (which rebuild every view) keep them.
"""

import logging
import queue
import threading

import AppKit
import objc
from Foundation import NSMakeRect, NSMakeSize, NSObject

from localflow.config import load_config
from localflow.hub_settings import (
    APPLIED_TITLE,
    BUSY_PREVIEW,
    CURRENT_TITLE,
    EMPTY_HINT,
    ERROR_TITLE,
    HEADING,
    INTRO,
    PLACEHOLDER,
    PREVIEW_TITLE,
    UNDO_TITLE,
    SettingsPageState,
    model_note,
    settings_layout,
)
from localflow.mac_style import (
    FlippedView,
    PlaceholderTextView,
    color,
    font,
    measure_text,
    style_button,
    style_input_box,
    style_surface,
)
from localflow.settings_agent import SettingsAgent, SettingsAgentError
from localflow.ui_tokens import CARD_BORDER_WIDTH

log = logging.getLogger("localflow.hub_settings")


class SettingsPageController(NSObject):
    def initWithHub_(self, hub):
        self = objc.super(SettingsPageController, self).init()  # noqa: PLW0642
        if self is None:
            return None
        self.hub = hub
        self.state = SettingsPageState()
        self.results = queue.Queue()
        self.closed = False
        self.text_view = None
        self.current = []
        # Tests and snapshots inject a factory; the default talks to the
        # user's config, dictionary and a loopback Ollama.
        factory = getattr(hub, "settings_agent_factory", None)
        self.agent_factory = factory or (lambda: SettingsAgent(dictionary=hub.dictionary))
        return self

    # ----------------------------------------------------------- lifecycle ---

    @objc.python_method
    def capture(self):
        """Read the draft before the Hub tears the views down, then forget them."""
        self._take_draft()

    @objc.python_method
    def shutdown(self):
        """The Hub window closed: results may still arrive but touch nothing."""
        self.closed = True
        self.text_view = None

    @objc.python_method
    def _repaint(self):
        hub = self.hub
        if not self.closed and hub.page == "Settings":
            hub._rebuild_current_page()

    @objc.python_method
    def _refresh_local(self):
        """Current settings and the undo summary (local reads only)."""
        agent = self.agent_factory()
        try:
            config = agent.load()
            self.current = agent.current_settings(config)
        except Exception:
            log.warning("could not read settings for the Settings page", exc_info=True)
            self.current = []
        self.state.undo_lines = agent.pending_undo()

    # -------------------------------------------------------------- render ---

    @objc.python_method
    def _label(self, parent, text, frame, size=13, token="ink", *, weight="regular",
               wrap=False):
        label = AppKit.NSTextField.labelWithString_(text)
        label.setFrame_(NSMakeRect(*frame))
        label.setFont_(font(size, weight))
        label.setTextColor_(color(token, self.hub.tokens))
        if wrap:
            label.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
            label.setMaximumNumberOfLines_(0)
        else:
            label.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
        parent.addSubview_(label)
        return label

    @objc.python_method
    def _button(self, parent, title, action, frame, kind):
        button = AppKit.NSButton.buttonWithTitle_target_action_(title, self, action)
        button.setFrame_(NSMakeRect(*frame))
        style_button(button, kind, self.hub.tokens)
        parent.addSubview_(button)
        return button

    @objc.python_method
    def render(self):
        hub = self.hub
        self._refresh_local()
        state = self.state
        width = hub._content_width()
        layout = settings_layout(
            width, hub.cursor_y, state, len(self.current),
            lambda text: measure_text(text, font(13)),
            lambda text: measure_text(text, font(13, "semibold")),
        )
        body = hub.body
        # The page heading is the Hub's shared 28pt bold title ("Settings").
        self._label(body, HEADING, layout["box_label"], 15, "ink", weight="semibold")
        self._label(body, INTRO, layout["intro"], 13, "muted", wrap=True)
        self._build_box(layout["box"])
        preview = self._button(body, "Preview", "preview:", layout["preview"], "primary")
        preview.setEnabled_(not state.busy)
        self._label(body, model_note(state.model), layout["note"], 12, "muted")
        self._build_card(layout)
        undo = self._button(body, UNDO_TITLE, "undo:", layout["undo"], "secondary")
        undo.setEnabled_(bool(state.undo_lines) and not state.busy)
        self._label(body, layout["undo_detail"], layout["undo_text"], 12, "muted", wrap=True)
        self._build_current(layout)
        hub.cursor_y = layout["bottom"]

    @objc.python_method
    def _build_box(self, frame):
        x, y, w, h = frame
        box = FlippedView.alloc().initWithFrame_(NSMakeRect(x, y, w, h))
        style_input_box(box, self.hub.tokens)
        scroll = AppKit.NSScrollView.alloc().initWithFrame_(NSMakeRect(6, 6, w - 12, h - 12))
        scroll.setHasVerticalScroller_(True)
        scroll.setAutohidesScrollers_(True)
        scroll.setBorderType_(AppKit.NSNoBorder)
        scroll.setDrawsBackground_(False)
        size = scroll.contentSize()
        view = PlaceholderTextView.alloc().initWithFrame_(
            NSMakeRect(0, 0, size.width, size.height)
        )
        view.setMinSize_(NSMakeSize(0, size.height))
        view.setMaxSize_(NSMakeSize(10000000, 10000000))
        view.setVerticallyResizable_(True)
        view.setHorizontallyResizable_(False)
        view.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        view.textContainer().setContainerSize_(NSMakeSize(size.width, 10000000))
        view.textContainer().setWidthTracksTextView_(True)
        view.setFont_(font(13))
        view.setTextColor_(color("ink", self.hub.tokens))
        view.setDrawsBackground_(False)
        view.setInsertionPointColor_(color("ink", self.hub.tokens))
        view.setRichText_(False)
        view.setAutomaticQuoteSubstitutionEnabled_(False)
        view.setAutomaticDashSubstitutionEnabled_(False)
        view.setAutomaticTextReplacementEnabled_(False)
        view.setAllowsUndo_(True)
        view.setEditable_(not self.state.busy)
        view.setSelectable_(True)
        view.setAccessibilityLabel_("Tell LocalFlow what to change")
        view.setString_(self.state.draft)
        view.setPlaceholder_color_(PLACEHOLDER, color("muted", self.hub.tokens))
        scroll.setDocumentView_(view)
        box.addSubview_(scroll)
        self.hub.body.addSubview_(box)
        self.text_view = view

    @objc.python_method
    def _build_card(self, layout):
        state, tokens = self.state, self.hub.tokens
        card = FlippedView.alloc().initWithFrame_(NSMakeRect(*layout["card"]))
        if state.status == "error":
            style_surface(card, tokens, border="red", width=CARD_BORDER_WIDTH)
        elif state.status == "preview":
            style_surface(card, tokens, border="ink", shadow=True, width=CARD_BORDER_WIDTH)
        else:
            style_surface(card, tokens, border="ink", width=CARD_BORDER_WIDTH)
        title, token = {
            "empty": ("Try asking", "ink"),
            "busy": (BUSY_PREVIEW, "cobalt"),
            "preview": (PREVIEW_TITLE, "ink"),
            "applied": (APPLIED_TITLE, "success"),
            "error": (ERROR_TITLE, "red"),
        }[state.status]
        self._label(card, title, layout["card_title"], 15, token, weight="semibold")
        if "card_text" in layout:
            text = {
                "empty": EMPTY_HINT,
                "busy": f"“{state.pending or ''}”",
            }.get(state.status, state.message)
            self._label(card, text, layout["card_text"], 13,
                        "red" if state.status == "error" else "muted", wrap=True)
        rows = layout["rows"]
        for frame, line in zip(rows, state.lines):
            # Wrapped (frame sized by settings_layout): never cut off.
            self._label(card, line, frame, 13, "ink", weight="semibold", wrap=True)
        for frame, note in zip(rows[len(state.lines):], state.notes):
            self._label(card, f"Skipped: {note}", frame, 12, "muted", wrap=True)
        if state.status == "preview":
            self._button(card, "Apply", "apply:", layout["apply"], "primary")
            self._button(card, "Cancel", "cancel:", layout["cancel"], "secondary")
        self.hub.body.addSubview_(card)

    @objc.python_method
    def _build_current(self, layout):
        tokens = self.hub.tokens
        card = FlippedView.alloc().initWithFrame_(NSMakeRect(*layout["current"]))
        style_surface(card, tokens, fill="paper", border="ink", width=CARD_BORDER_WIDTH)
        width = layout["current"][2]
        self._label(card, CURRENT_TITLE, (16, 16, width - 32, 22), 13, "ink", weight="semibold")
        for (x, y, w, h), (label, value) in zip(layout["current_rows"], self.current):
            self._label(card, label, (x, y, w * 0.55, h), 12, "muted")
            self._label(card, value, (x + w * 0.55, y, w * 0.45, h), 12, "ink",
                        weight="semibold")
        self.hub.body.addSubview_(card)

    # ------------------------------------------------------------- actions ---

    def preview_(self, _sender):
        if self.text_view is None or self.state.busy:
            return
        request = str(self.text_view.string())
        token = self.state.begin_preview(request)
        if token is None:
            return
        agent = self.agent_factory()

        def work():
            try:
                proposal = agent.propose(request)
                result = (token, proposal, None, agent.model)
            except SettingsAgentError as exc:
                result = (token, None, str(exc), agent.model)
            except Exception:
                log.exception("settings preview failed")
                result = (token, None, "Something went wrong asking the local model.",
                          agent.model)
            self.results.put(result)
            with objc.autorelease_pool():
                self.performSelectorOnMainThread_withObject_waitUntilDone_(
                    "previewDone:", None, False
                )

        threading.Thread(target=work, name="localflow-settings-preview", daemon=True).start()
        self.hub._rebuild_current_page()

    def previewDone_(self, _ignored):
        changed = False
        while True:
            try:
                token, proposal, error, model = self.results.get_nowait()
            except queue.Empty:
                break
            changed |= self.state.finish_preview(token, proposal, error, model)
        if changed:
            self._repaint()

    @objc.python_method
    def _take_draft(self):
        """Move the typed text into the state and forget the (soon stale) view,
        so the repaint after a write shows exactly what the state says."""
        if self.text_view is not None:
            self.state.draft = str(self.text_view.string())
            self.text_view = None

    def apply_(self, _sender):
        state = self.state
        if state.status != "preview" or state.proposal is None:
            return
        self._take_draft()
        try:
            result = self.agent_factory().apply(state.proposal.changes)
        except SettingsAgentError as exc:
            state.failed(str(exc))
        else:
            state.applied(result)
        self._after_write()

    def cancel_(self, _sender):
        self.state.cancel()
        self._repaint()

    def undo_(self, _sender):
        if self.state.busy:
            return
        self._take_draft()
        try:
            result = self.agent_factory().undo()
        except SettingsAgentError as exc:
            self.state.failed(str(exc))
        else:
            self.state.undone(result)
        self._after_write()

    @objc.python_method
    def _after_write(self):
        """Config and dictionary may have changed: refresh what the Hub shares."""
        hub = self.hub
        try:
            hub.config = load_config()
        except Exception:
            log.warning("could not reload the config after a settings change", exc_info=True)
        self._repaint()
