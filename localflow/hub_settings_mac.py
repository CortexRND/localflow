"""Native Settings page of the Hub: manual controls plus plain-language tweaks.

State and geometry live in hub_settings (AppKit-free); the rules live in
settings_agent. This controller only builds views and moves work off the
UI thread:

- Every ALLOWLIST setting has a native control (switch, popup or shortcut
  field), grouped as Dictation, Cleanup & style, Shortcuts and Meetings.
  A committed change (toggle, pick, Return/end of editing in a shortcut
  field) goes through `SettingsAgent.set_setting`, the same validation,
  conflict check, write and undo record as an agent Apply; it never calls
  save_config itself. An invalid typed shortcut stays in its field with a
  friendly inline error across resizes and navigation.
- Preview runs `SettingsAgent.propose` on a worker thread (a local model can
  take many seconds). The result comes back through
  performSelectorOnMainThread and is accepted only if its token is still
  current; it never touches a view directly, and the page is repainted only
  while the Hub is open and showing Settings. A manual change retires a
  pending or shown preview (see SettingsPageState.settings_changed), so a
  late answer can never overwrite what was just set by hand.
- Apply / Undo / manual changes are local file writes on the main thread.
- The typed request, preview, error, manual drafts and in-flight state live
  on this long-lived controller, so navigation, resizing and appearance
  changes (which rebuild every view) keep them. Controls are retained in
  `manual_controls` (AppKit targets are weak) and forgotten on capture, so a
  torn-down field's late action is ignored.
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
    EMPTY_HINT,
    ERROR_TITLE,
    HEADING,
    INTRO,
    MANUAL_INTRO,
    MANUAL_TITLE,
    PLACEHOLDER,
    PREVIEW_TITLE,
    UNDO_TITLE,
    WARNINGS_TITLE,
    SettingsPageState,
    manual_layout,
    manual_panel_width,
    model_note,
    settings_layout,
)
from localflow.mac_style import (
    CobaltSwitch,
    FlippedView,
    PlaceholderTextView,
    color,
    font,
    measure_text,
    style_button,
    style_input_box,
    style_surface,
)
from localflow.settings_agent import (
    SettingsAgent,
    SettingsAgentError,
    editable_shortcut,
    safe_text,
)
from localflow.ui_tokens import CARD_BORDER_WIDTH

log = logging.getLogger("localflow.hub_settings")

CHOOSE_TITLE = "Choose…"


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
        self.manual_rows = []
        # key -> (kind, control); strong references: AppKit targets are weak
        # and a control must stay alive (and wired) as long as it is shown.
        self.manual_controls = {}
        # Tests and snapshots inject a factory; the default talks to the
        # user's config, dictionary and a loopback Ollama.
        factory = getattr(hub, "settings_agent_factory", None)
        self.agent_factory = factory or (lambda: SettingsAgent(dictionary=hub.dictionary))
        return self

    # ----------------------------------------------------------- lifecycle ---

    @objc.python_method
    def capture(self):
        """Read the drafts before the Hub tears the views down, then forget them."""
        self._take_draft()
        self._take_manual_drafts()

    @objc.python_method
    def shutdown(self):
        """The Hub window closed: results may still arrive but touch nothing."""
        self.closed = True
        self.text_view = None
        self.manual_controls = {}

    @objc.python_method
    def _repaint(self):
        hub = self.hub
        if not self.closed and hub.page == "Settings":
            hub._rebuild_current_page()

    @objc.python_method
    def _refresh_local(self):
        """Manual settings and the undo summary (local reads only)."""
        agent = self.agent_factory()
        try:
            config = agent.load()
            self.manual_rows = list(agent.manual_settings(config))
        except Exception:
            log.warning("could not read settings for the Settings page", exc_info=True)
            self.manual_rows = []
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
        measure = lambda text: measure_text(text, font(13))
        panel_w = manual_panel_width(width)
        manual = manual_layout(
            panel_w, self.manual_rows, state, lambda text: measure_text(text, font(12)),
        )
        layout = settings_layout(
            width, hub.cursor_y, state, len(self.manual_rows), measure,
            lambda text: measure_text(text, font(13, "semibold")),
            manual_height=manual["height"],
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
        self._build_manual(layout["current"], manual)
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
                "busy": f"“{safe_text(state.pending or '')}”",
            }.get(state.status, state.message)
            self._label(card, text, layout["card_text"], 13,
                        "red" if state.status == "error" else "muted", wrap=True)
        rows = layout["rows"]
        for frame, line in zip(rows, state.lines):
            # Wrapped (frame sized by settings_layout): never cut off.
            self._label(card, line, frame, 13, "ink", weight="semibold", wrap=True)
        if "warnings_title" in layout:
            self._label(card, WARNINGS_TITLE, layout["warnings_title"], 12, "red",
                        weight="semibold")
            for frame, warning in zip(layout["warning_rows"], state.warnings):
                self._label(card, f"⚠ {warning}", frame, 12, "red", wrap=True)
        for frame, note in zip(rows[len(state.lines):], state.notes):
            self._label(card, f"Skipped: {note}", frame, 12, "muted", wrap=True)
        if state.status == "preview":
            self._button(card, "Apply", "apply:", layout["apply"], "primary")
            self._button(card, "Cancel", "cancel:", layout["cancel"], "secondary")
        self.hub.body.addSubview_(card)

    @objc.python_method
    def _build_manual(self, frame, manual):
        """Grouped native controls for every allowlisted setting."""
        tokens = self.hub.tokens
        card = FlippedView.alloc().initWithFrame_(NSMakeRect(*frame))
        style_surface(card, tokens, fill="paper", border="ink", width=CARD_BORDER_WIDTH)
        self._label(card, MANUAL_TITLE, manual["title"], 15, "ink", weight="semibold")
        self._label(card, MANUAL_INTRO, manual["intro"], 12, "muted", wrap=True)
        if "notice" in manual:
            self._label(card, self.state.manual_notice, manual["notice"], 12, "success",
                        wrap=True)
        for name, group_frame in manual["groups"]:
            self._label(card, name, group_frame, 12, "cobalt", weight="semibold")
        self.manual_controls = {}
        for row in self.manual_rows:
            frames = manual["rows"].get(row.key)
            if frames is None:
                continue
            # Label left (wrapping in its column), control right-aligned.
            self._label(card, row.label, frames["label"], 12, "ink", wrap=True)
            control = self._manual_control(card, row, frames["control"])
            self.manual_controls[row.key] = (row.kind, control)
            if "error" in frames:
                self._label(card, self.state.manual_errors.get(row.key, ""),
                            frames["error"], 12, "red", wrap=True)
        self.hub.body.addSubview_(card)

    @objc.python_method
    def _manual_control(self, parent, row, frame):
        """One explicit-target control, enabled and wired to its action."""
        tokens = self.hub.tokens
        if row.kind == "switch":
            # House switch: token colours in every appearance and window state.
            control = CobaltSwitch.alloc().initWithFrame_(NSMakeRect(*frame)).configure(
                tokens, bool(row.value), row.label,
            )
            action = "manualToggle:"
        elif row.kind == "choice":
            control = AppKit.NSPopUpButton.alloc().initWithFrame_pullsDown_(
                NSMakeRect(*frame), False
            )
            titles = [title for _value, title in row.choices]
            values = [value for value, _title in row.choices]
            if row.value not in values:
                titles.insert(0, CHOOSE_TITLE)  # stored value invalid: pick one
            control.addItemsWithTitles_(titles)
            control.selectItemAtIndex_(
                values.index(row.value) if row.value in values else 0
            )
            action = "manualChoose:"
        else:
            control = AppKit.NSTextField.alloc().initWithFrame_(NSMakeRect(*frame))
            draft = self.state.manual_drafts.get(row.key)
            control.setStringValue_(
                draft if draft is not None else editable_shortcut(row.value or "")
            )
            control.setPlaceholderString_("Off" if row.key != "hotkey" else "e.g. right_option")
            control.setFont_(font(12))
            control.setTextColor_(color("ink", tokens))
            control.setEditable_(True)
            control.setSelectable_(True)
            # Commit on Return and when editing ends, never per keystroke.
            control.cell().setSendsActionOnEndEditing_(True)
            action = "manualShortcut:"
        control.setTarget_(self)
        control.setAction_(action)
        control.setEnabled_(True)
        control.setAccessibilityLabel_(row.label)
        parent.addSubview_(control)
        return control

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

    @objc.python_method
    def _take_manual_drafts(self):
        """Keep typed-but-uncommitted shortcut text; forget every control."""
        rows = {row.key: row for row in self.manual_rows}
        for key, (kind, control) in self.manual_controls.items():
            if kind != "shortcut" or key not in rows:
                continue
            typed = str(control.stringValue())
            if typed != editable_shortcut(rows[key].value or ""):
                self.state.manual_draft(key, typed)
            elif key not in self.state.manual_errors:
                self.state.manual_drafts.pop(key, None)
        self.manual_controls = {}

    @objc.python_method
    def _manual_key(self, sender, kind):
        """The key of a live control, or None for a stale/unknown sender."""
        for key, (control_kind, control) in self.manual_controls.items():
            if control is sender and control_kind == kind:
                return key
        return None

    @objc.python_method
    def _manual_commit(self, key, value, typed=None):
        """Write one manual change through the agent; errors stay inline."""
        self._take_draft()
        self._take_manual_drafts()
        try:
            result = self.agent_factory().set_setting(key, value)
        except SettingsAgentError as exc:
            self.state.manual_failed(key, typed, str(exc))
        else:
            if result.applied:
                lines = "; ".join(change.line() for change in result.applied)
                self.state.manual_saved(key, f"Saved: {lines}")
            else:
                self.state.manual_drafts.pop(key, None)
                self.state.manual_errors.pop(key, None)
        self._after_write()

    def manualToggle_(self, sender):
        key = self._manual_key(sender, "switch")
        if key is None:
            return
        self._manual_commit(key, sender.state() == AppKit.NSControlStateValueOn)

    def manualChoose_(self, sender):
        key = self._manual_key(sender, "choice")
        if key is None:
            return
        row = next(row for row in self.manual_rows if row.key == key)
        values = [value for value, _title in row.choices]
        index = sender.indexOfSelectedItem()
        if row.value not in values:
            index -= 1  # skip the "Choose…" placeholder
        if not 0 <= index < len(values):
            return
        self._manual_commit(key, values[index])

    def manualShortcut_(self, sender):
        key = self._manual_key(sender, "shortcut")
        if key is None:
            return
        typed = str(sender.stringValue())
        row = next(row for row in self.manual_rows if row.key == key)
        if typed == editable_shortcut(row.value or "") and key not in self.state.manual_errors:
            return  # ended editing without a change
        self._manual_commit(key, typed.strip() or "off", typed)

    def apply_(self, _sender):
        state = self.state
        if state.status != "preview" or state.proposal is None:
            return
        self._take_draft()
        self._take_manual_drafts()
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
        self._take_manual_drafts()
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
