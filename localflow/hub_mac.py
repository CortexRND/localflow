"""Native AppKit history and settings hub for macOS."""

import logging
import queue
import subprocess
import sys
import threading
import time
from dataclasses import replace
from datetime import datetime

import AppKit
import objc
from AppKit import (
    NSAlert,
    NSApplication,
    NSButton,
    NSComboBox,
    NSMakeRect,
    NSPopUpButton,
    NSScrollView,
    NSSearchField,
    NSSwitch,
    NSTextField,
    NSTextView,
    NSView,
    NSWindow,
)
from Foundation import NSMakePoint, NSMakeSize, NSObject, NSTimer

from localflow.config import (
    ConfigError,
    apply_config_update,
    config_path,
    load_config,
    save_config,
)
from localflow.dictionary import DictionaryStore
from localflow.dispatch import PromptQueue
from localflow.history import HistoryStore
from localflow.hot_phrases_editor import (
    EMPTY_MESSAGE,
    ENABLED_ACCESSIBILITY,
    HotPhrasesEditor,
    empty_layout,
    enabled_label,
    hot_phrases_layout,
    preview_placeholder,
)
from localflow.hotphrases import HotPhraseStore
from localflow.hub_model import (
    MEETING_NOTES_PLACEHOLDER,
    MEETING_SUBTITLE,
    MEETING_TITLE_PLACEHOLDER,
    PROMPT_STATUS_PILLS,
    PROMPTS_EMPTY,
    PROMPTS_UNREADABLE,
    RECENT_EMPTY,
    RECENT_ROW_H,
    SPEED_TITLE,
    STYLE_AUTOSAVE_HINT,
    STYLE_SAVE_FAILED,
    dictation_state,
    dictionary_entries,
    home_decoration_frame,
    home_speed_card,
    load_perf_summary,
    meeting_capture_status,
    meeting_form_layout,
    meeting_notes_height,
    prompt_row_layout,
    prompt_rows,
    recent_notes,
    remove_entry,
    review_prompt,
    saved_notes_message,
    shortcut_rows,
    sidebar_label,
    sidebar_title,
    stat_layout,
    style_row_frames,
    style_update,
    submit_replacement,
    submit_word,
)
from localflow.live_config import LazyConfig
from localflow.mac_style import (
    FlippedView,
    HalftoneView,
    PlaceholderTextView,
    input_box,
    make_pill,
    measure_text,
    page_title,
    style_button,
    style_input_box,
    style_surface,
    tokens_for_window,
)
from localflow.mac_style import (
    color as _color,
)
from localflow.mac_style import (
    font as _font,
)
from localflow.mac_style import (
    segmented as _segmented,
)
from localflow.mac_style import (
    style_flat_button as _style_button,
)
from localflow.meeting_client import MeetingClient, vault_notes_path
from localflow.models_layout import (
    CLEANUP_LABEL,
    IMPORT_HEADER_H,
    IMPORT_ROW_H,
    STT_FIXED_MODEL,
    ModelChoiceError,
    action_frames,
    active_preset,
    cleanup_picker,
    current_speech_provider,
    editor_layout,
    import_height,
    import_states,
    is_active,
    list_layout,
    note_lines,
    ollama_models,
    provider_status,
    provider_status_text,
    quick_layout,
    row_columns,
    set_active_model,
    set_speech,
    speech_model_choices,
    speech_provider_choices,
    speech_summary,
    use_button_visible,
)
from localflow.phrases_model import PhrasesModel
from localflow.platform import current
from localflow.text_wrap import wrap_lines
from localflow.ui_tokens import (
    CARD_BORDER_WIDTH,
    CARD_PADDING,
    HUB_CONTENT_GUTTER,
    HUB_CONTENT_TOP,
    HUB_CONTENT_X,
    HUB_DEFAULT_HEIGHT,
    HUB_DEFAULT_WIDTH,
    HUB_HEADER_OFFSET,
    HUB_MIN_HEIGHT,
    HUB_MIN_WIDTH,
    HUB_PAGE_BOTTOM,
    HUB_SIDEBAR_WIDTH,
    LIGHT,
    MODELS_ROW_HEIGHT,
    PILL_HEIGHT,
    SUN_TEXT,
    surface_tint,
)
from localflow.ui_tokens import (
    TOKENS as TOKENS,  # noqa: PLC0414 - kept as a public re-export
)

PAGES = (
    "Home", "Ask", "Hot Phrases", "Dictionary", "Prompts",
    "Meetings", "History", "Style", "Models", "Settings",
)


def _label(
    text, x, y, width=650, size=14, color="ink", *,
    palette=None, height=28, weight="regular", wrap=False,
):
    label = NSTextField.labelWithString_(text)
    label.setFrame_(NSMakeRect(x, y, width, height))
    label.setFont_(_font(size, weight))
    label.setTextColor_(_color(color, palette))
    if wrap:
        # Multi-line: the frame height is the room for the wrapped lines.
        label.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
        label.setMaximumNumberOfLines_(0)
    else:
        label.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
    return label


class _MeetingNotesWriter:
    """Puts meeting notes on the server in the order the Hub asked for them.

    Every save takes a ticket on the main thread; writes are serialized and a
    write older than one already sent is dropped. So the flush a closing Hub
    sends (the newest ticket) is never overwritten by a save that was still
    in flight, whichever worker thread reaches the lock first. No AppKit.
    """

    def __init__(self):
        self._tickets = threading.Lock()
        self._writes = threading.Lock()
        self._issued = 0
        self._sent = 0

    def ticket(self):
        with self._tickets:
            self._issued += 1
            return self._issued

    def save(self, client, ticket, text):
        with self._writes:
            if ticket < self._sent:
                return {}  # superseded by newer notes already sent
            self._sent = ticket
            return client.save_notes(text)


# One per process: a Hub reopened after close orders its saves after the
# closed one's final flush.
_MEETING_NOTES_WRITER = _MeetingNotesWriter()


class HubWindowController(NSObject):
    def init(self):
        config = load_config()
        return self.initWithHistory_dictionary_config_meetingClient_promptQueue_initialPage_(
            HistoryStore(enabled=config.history_enabled), DictionaryStore(), config,
            None, None, "Home",
        )

    def initWithHistory_dictionary_config_meetingClient_initialPage_(
        self, history, dictionary, config, meeting_client, initial_page
    ):
        return self.initWithHistory_dictionary_config_meetingClient_promptQueue_initialPage_(
            history, dictionary, config, meeting_client, None, initial_page
        )

    def initWithHistory_dictionary_config_meetingClient_promptQueue_initialPage_(
        self, history, dictionary, config, meeting_client, prompt_queue, initial_page
    ):
        return self.initWithHistory_dictionary_config_meetingClient_promptQueue_phrasesModel_initialPage_(
            history, dictionary, config, meeting_client, prompt_queue, None,
            initial_page,
        )

    def initWithHistory_dictionary_config_meetingClient_promptQueue_phrasesModel_initialPage_(
        self, history, dictionary, config, meeting_client, prompt_queue,
        phrases_model, initial_page,
    ):
        self = super().init()  # noqa: PLW0642
        if self is None:
            return None
        # The Hot Phrases state is built on first use, so opening the Hub never
        # touches the hot-phrase store unless that page is visited. A model
        # may be injected (snapshots, tests); the default uses the user store.
        self._phrases_model = phrases_model
        self.phrases = None
        self._phrases_empty_shown = False
        self._closed = False
        self.history = history
        self.dictionary = dictionary
        self.config = config
        self._live_config = LazyConfig(config)
        self.meeting_client = meeting_client or MeetingClient()
        # Constructing a PromptQueue touches no disk; it is only read on Prompts.
        self.prompt_queue = prompt_queue if prompt_queue is not None else PromptQueue()
        self.meeting_events = queue.Queue()
        self.meeting_status = {}
        self.meeting_templates = []
        self.meeting_last_saved = None
        self.meeting_answers = []
        self.meeting_notes_text = ""
        # Blank: the placeholder says the notes are named when saved. Only a
        # title the user types (see controlTextDidChange_) is kept as is.
        self.meeting_title_value = ""
        self.meeting_title_edited = False  # see controlTextDidChange_
        self.meeting_template_value = ""
        self.meeting_notes_dirty = False
        self.meeting_notes_due = None
        self.meeting_notes_save_in_flight = False
        self.meeting_notes_save_pending = False
        self.meeting_stop_flush_pending = False
        self.meeting_active_notes_text = ""
        self.meeting_notes_save_operation = None  # the one in flight, if any
        self.meeting_notes_writer = _MEETING_NOTES_WRITER
        self.meeting_busy = False
        self.meeting_error = ""
        self.meeting_next_poll = 0
        self.meeting_session_active = False
        self.meeting_title_shows_session = False
        self.meeting_recent = []
        self.meeting_recent_error = ""
        self.tokens = LIGHT
        self.page = "Home"
        self.page_errors = {}
        self.page_notices = {}
        self.dictionary_error_card = None
        self.meeting_template_default = "general"
        self._build_window()
        self.show_page(initial_page)
        self.meeting_timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            0.2, self, objc.selector(self.meetingTimerTick_, signature=b"v@:@"),
            None, True,
        )
        return self

    @objc.python_method
    def _build_window(self):
        self.window = NSWindow.alloc().initWithContentRect_styleMask_backing_defer_(
            NSMakeRect(0, 0, HUB_DEFAULT_WIDTH, HUB_DEFAULT_HEIGHT),
            AppKit.NSTitledWindowMask | AppKit.NSClosableWindowMask |
            AppKit.NSMiniaturizableWindowMask | AppKit.NSResizableWindowMask,
            AppKit.NSBackingStoreBuffered,
            False,
        )
        self.window.setTitle_("LocalFlow Hub")
        self.window.setContentMinSize_(NSMakeSize(HUB_MIN_WIDTH, HUB_MIN_HEIGHT))
        self.window.center()
        self.root_view = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, HUB_DEFAULT_WIDTH, HUB_DEFAULT_HEIGHT)
        )
        self.root_view.setWantsLayer_(True)
        self.root_view.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        self.window.setContentView_(self.root_view)
        # The sidebar keeps its width and follows the window height.
        self.sidebar = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, HUB_SIDEBAR_WIDTH, HUB_DEFAULT_HEIGHT)
        )
        self.sidebar.setAutoresizingMask_(AppKit.NSViewHeightSizable)
        self.sidebar.setWantsLayer_(True)
        self.sidebar.layer().setBackgroundColor_(_color("sidebar", self.tokens).CGColor())
        self.root_view.addSubview_(self.sidebar)
        self.brand = _label(
            "localflow", 42, 25, 145, 17, "sidebar_text", palette=self.tokens, height=26
        )
        self.sidebar.addSubview_(self.brand)
        mark = NSView.alloc().initWithFrame_(NSMakeRect(22, 31, 10, 10))
        mark.setWantsLayer_(True)
        mark.layer().setBackgroundColor_(_color("sun", self.tokens).CGColor())
        mark.layer().setCornerRadius_(5)
        self.sidebar.addSubview_(mark)
        self.sidebar_buttons = []
        sidebar_font = _font(13, "semibold")  # style_flat_button's title font
        for index, page in enumerate(PAGES):
            button = NSButton.buttonWithTitle_target_action_(page, self, "showPage:")
            button.setFrame_(NSMakeRect(16, 76 + index * 44, 168, 36))
            # Display label only ("Models & keys"); the tag still routes by
            # PAGES, and a label that would not fit falls back to the page id.
            button.setTitle_(
                sidebar_title(page, lambda text: measure_text(text, sidebar_font))
            )
            button.setAccessibilityLabel_(sidebar_label(page))
            button.setAlignment_(AppKit.NSTextAlignmentLeft)
            button.setTag_(index)
            _style_button(button, selected=page == self.page, palette=self.tokens)
            self.sidebar.addSubview_(button)
            self.sidebar_buttons.append(button)
        # Pages are laid out in `body`, the document view of a scroll view that
        # tracks the window: pages use the current body width, and anything
        # taller than the window scrolls.
        self.body_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(
                HUB_CONTENT_X, HUB_CONTENT_TOP,
                HUB_DEFAULT_WIDTH - HUB_CONTENT_X - HUB_CONTENT_GUTTER,
                HUB_DEFAULT_HEIGHT - HUB_CONTENT_TOP,
            )
        )
        self.body_scroll.setAutoresizingMask_(
            AppKit.NSViewWidthSizable | AppKit.NSViewHeightSizable
        )
        self.body_scroll.setHasVerticalScroller_(True)
        self.body_scroll.setAutohidesScrollers_(True)
        self.body_scroll.setScrollerStyle_(AppKit.NSScrollerStyleOverlay)
        self.body_scroll.setDrawsBackground_(False)
        self.body = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, self._content_width(), self._viewport_height())
        )
        self.body.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        self.body_scroll.setDocumentView_(self.body)
        self.root_view.addSubview_(self.body_scroll)
        self.window.setDelegate_(self)

    @objc.python_method
    def _content_width(self):
        """Width pages lay out in: the scroll view minus the right gutter."""
        return max(
            self.body_scroll.frame().size.width - HUB_CONTENT_GUTTER,
            HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER,
        )

    @objc.python_method
    def _viewport_height(self):
        return self.body_scroll.frame().size.height

    @objc.python_method
    def _fit_body_height(self):
        """Grow the document to its content so tall pages scroll."""
        bottom = max(
            (view.frame().origin.y + view.frame().size.height
             for view in self.body.subviews()),
            default=0,
        )
        frame = self.body.frame()
        self.body.setFrame_(
            NSMakeRect(
                0, 0, frame.size.width,
                max(bottom + HUB_PAGE_BOTTOM, self._viewport_height()),
            )
        )

    @objc.python_method
    def _button(
        self, title, action, x, y, tag=0, width=150, parent=None,
        kind="secondary", height=36,
    ):
        button = NSButton.buttonWithTitle_target_action_(title, self, action)
        button.setFrame_(NSMakeRect(x, y, width, height))
        button.setTag_(tag)
        style_button(button, kind, self.tokens)
        container = parent if parent is not None else self.body
        container.addSubview_(button)
        return button

    @objc.python_method
    def _add_text(
        self, text, *, size=13, color="ink", height=24, gap=12, weight="regular",
    ):
        self.body.addSubview_(
            _label(
                text, 0, self.cursor_y, self.body.frame().size.width, size, color,
                palette=self.tokens, height=height, weight=weight,
            )
        )
        self.cursor_y += height + gap

    @objc.python_method
    def _field(self, value, x, y, width=240, *, parent=None, action=None):
        field = NSTextField.alloc().initWithFrame_(NSMakeRect(x, y, width, 32))
        field.setStringValue_(value or "")
        field.setFont_(_font(13))
        if action:
            # Return in the field sends the action, like clicking its button.
            field.setTarget_(self)
            field.setAction_(action)
        (parent if parent is not None else self.body).addSubview_(field)
        return field

    @objc.python_method
    def _rebuild_current_page(self):
        """Re-lay out the current page, keeping drafts and the scroll position."""
        state = self._preserve_page_state()
        scroll_y = self.body_scroll.contentView().bounds().origin.y
        self.show_page(self.page)
        self._restore_page_state(state)
        limit = max(0, self.body.frame().size.height - self._viewport_height())
        self.body.scrollPoint_(NSMakePoint(0, min(scroll_y, limit)))

    @objc.python_method
    def resize_content(self, width, height):
        """Resize the window's content (used by snapshots) and re-lay out."""
        self.window.setContentSize_(
            NSMakeSize(max(width, HUB_MIN_WIDTH), max(height, HUB_MIN_HEIGHT))
        )
        self._rebuild_current_page()

    @objc.python_method
    def show_page(self, page):
        # The one navigation entry point (sidebar, menu bar, CLI, page
        # requests). Leaving Hot Phrases commits a valid draft; an invalid one
        # keeps the user there with the error inline. Re-showing the current
        # page (resize, appearance) is a rebuild, never a navigation commit.
        if (
            self.page == "Hot Phrases"
            and page != "Hot Phrases"
            and not self._phrases_leave()
        ):
            page = "Hot Phrases"
        entering_phrases = page == "Hot Phrases" and self.page != "Hot Phrases"
        # Recent notes are read when Meetings is shown (and after a stop), never
        # by the 2s status poll, a resize rebuild or any other page.
        entering_meetings = page == "Meetings" and self.page != "Meetings"
        if self.page == "Meetings" and hasattr(self, "meeting_notes"):
            self._meeting_capture_state()
        # Every rebuild (resize, appearance, navigation) funnels through here,
        # so the Ask draft is read from its field before the views are torn
        # down. The controller and model themselves are never recreated.
        if self.page == "Ask" and hasattr(self, "ask_page"):
            self.ask_page.capture()
        # Settings keeps its typed request the same way (preview, errors and
        # an in-flight request live on its controller already).
        if self.page == "Settings" and hasattr(self, "settings_page"):
            self.settings_page.capture()
        # Models & keys: pending URL/model text and import ticks are read from
        # the live controls before they are removed. Secure key fields are never
        # captured, so an unsaved key is dropped with its view.
        if self.page == "Models" and hasattr(self, "provider_controls"):
            self._models_capture()
            # Drop every reference to the old controls (including an unsaved
            # secure field) before the views are torn down, so nothing hidden
            # holds a secret and a later capture cannot read stale views.
            self.provider_controls = {}
            self.provider_checks = []
            self.provider_action_views = None
            # The "Change models" controls go with their views; a late
            # action from a removed one is ignored (see _quick_live).
            self.quick_controls = {}
            self.quick_geometry = None
        self.tokens = tokens_for_window(self.window)
        self.page = page
        self.root_view.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        self.sidebar.layer().setBackgroundColor_(
            _color("sidebar", self.tokens).CGColor()
        )
        self.brand.setTextColor_(_color("sidebar_text", self.tokens))
        for button in self.sidebar_buttons:
            _style_button(
                button, selected=PAGES[button.tag()] == page,
                palette=self.tokens,
            )
        for view in self.body.subviews():
            view.removeFromSuperview()
        width = self._content_width()
        self.body.setFrame_(NSMakeRect(0, 0, width, self._viewport_height()))
        self.cursor_y = 0
        # Every page shares the setup display title; History leaves room for
        # its right-aligned action so the two frames never overlap.
        self.body.addSubview_(
            page_title(
                sidebar_label(page), width - 140 if page == "History" else width,
                self.tokens,
            )
        )
        if page == "History":
            self._button(
                "Clear history", "clearHistory:", width - 130, 0, width=130,
                kind="destructive_ghost",
            )
        self.cursor_y = HUB_HEADER_OFFSET
        error = self.page_errors.get(page)
        # Dictionary and Prompts show friendly errors inline, beside what failed.
        if error and page not in ("Dictionary", "Prompts"):
            self._callout(error)
        if entering_phrases:
            self._phrases().on_show()
        if entering_meetings:
            self._meeting_load_recent()
        getattr(self, f'_page_{page.lower().replace(" ", "_")}')()
        self._fit_body_height()

    @objc.python_method
    def _callout(self, message):
        width = self.body.frame().size.width
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, 58)
        )
        style_surface(card, self.tokens)
        bar = NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 4, 58))
        bar.setWantsLayer_(True)
        bar.layer().setBackgroundColor_(_color("red", self.tokens).CGColor())
        card.addSubview_(bar)
        card.addSubview_(
            _label(
                "Something went wrong", 16, 8, width - 40, 13, "red",
                palette=self.tokens, height=18,
            )
        )
        card.addSubview_(
            _label(message, 16, 30, width - 40, 11, "muted", palette=self.tokens, height=18)
        )
        self.body.addSubview_(card)
        self.cursor_y += 70

    def windowDidChangeEffectiveAppearance_(self, _notification):
        self._rebuild_current_page()

    def windowShouldClose_(self, _sender):
        # A valid draft is saved; an invalid one keeps the window open so the
        # user sees why instead of silently losing it.
        if self._phrases_leave():
            return True
        self._phrases_sync()
        return False

    def windowWillClose_(self, _notification):
        if self._closed:  # shut down once, however often AppKit tells us
            return
        self._closed = True
        try:
            self._meeting_flush_on_close()
        except Exception:
            logging.getLogger("localflow.hub").warning(
                "could not flush meeting notes on close", exc_info=True,
            )
        finally:
            # The run loop retains the timer and the timer retains us: stop it
            # so a closed Hub is released instead of ticking until quit.
            timer, self.meeting_timer = getattr(self, "meeting_timer", None), None
            if timer is not None:
                timer.invalidate()
        if hasattr(self, "ask_page"):
            self.ask_page.shutdown()
        if hasattr(self, "settings_page"):
            self.settings_page.shutdown()
        if self.phrases is not None:
            self.phrases.cancel()

    @objc.python_method
    def _meeting_flush_on_close(self):
        """Send the notes the stopped timer would have saved; the thread, or None.

        Unsaved means typed < 0.8 s ago (`meeting_notes_due`) or queued behind
        the save in flight; a Stop still waiting on its notes flush is sent
        too, after the notes, as `_meeting_notes_save_completed` would have.
        The text is read here on the main thread; the worker touches only the
        client, and the shared writer orders it after any save in flight.
        """
        stop = self.meeting_stop_flush_pending or (
            self.meeting_notes_save_in_flight
            and self.meeting_notes_save_operation == "notes_stop_flush"
        )
        unsaved = (
            stop or self.meeting_notes_due is not None or self.meeting_notes_save_pending
        )
        self.meeting_notes_due = None
        self.meeting_notes_save_pending = self.meeting_stop_flush_pending = False
        if not unsaved:
            return None
        text = self._meeting_current_notes_text()
        client, writer = self.meeting_client, self.meeting_notes_writer
        ticket = writer.ticket()

        def run():
            try:
                writer.save(client, ticket, text)
                if stop:
                    client.stop()  # only once the final notes are on the server
            except Exception:
                logging.getLogger("localflow.hub").warning(
                    "meeting notes flush on close failed", exc_info=True,
                )

        thread = threading.Thread(
            target=run, name="localflow-meeting-close-flush", daemon=True
        )
        thread.start()
        return thread

    def windowDidEndLiveResize_(self, _notification):
        self._rebuild_current_page()

    @objc.python_method
    def _preserve_page_state(self):
        if self.page == "Hot Phrases" and hasattr(self, "phrase_trigger"):
            self._phrases_pull()
            return (
                "hot_phrases", self._phrases_responder_key(),
                self._phrases_list_scroll_y(),
            )
        if self.page == "History" and hasattr(self, "search_field"):
            return ("history", self.search_field.stringValue(), self.history_offset)
        if self.page == "Style" and hasattr(self, "style_selections"):
            return ("style", self.cleanup_selection, dict(self.style_selections))
        if self.page == "Dictionary" and hasattr(self, "word_field"):
            return (
                "dictionary", self.word_field.stringValue(),
                self.from_field.stringValue(), self.to_field.stringValue(),
                self.editing_word, self.editing_source,
            )
        if self.page == "Meetings" and hasattr(self, "meeting_question"):
            return ("meetings", self.meeting_question.stringValue())
        return None

    @objc.python_method
    def _restore_page_state(self, state):
        if not state:
            return
        if state[0] == "hot_phrases":
            _kind, responder, scroll_y = state
            self._phrases_restore_view(responder, scroll_y)
        elif state[0] == "history":
            _kind, query, offset = state
            self.search_field.setStringValue_(query)
            self.history_query = query
            self.history_offset = offset
            self._render_history()
        elif state[0] == "style":
            _kind, cleanup, styles = state
            self.cleanup_selection = cleanup
            self.style_selections = styles
            if cleanup in self.cleanup_options:
                self._update_segment_buttons(
                    self.cleanup_group, self.cleanup_buttons,
                    self.cleanup_options.index(cleanup),
                )
            for category, value in styles.items():
                if value in self.style_options:
                    self._update_segment_buttons(
                        self.style_groups[category], self.style_buttons[category],
                        self.style_options.index(value),
                    )
        elif state[0] == "dictionary":
            _kind, word, source, target, editing_word, editing_source = state
            self.word_field.setStringValue_(word)
            self.from_field.setStringValue_(source)
            self.to_field.setStringValue_(target)
            self.editing_word = editing_word
            self.editing_source = editing_source
            self._sync_dictionary_buttons()
        elif state[0] == "meetings":
            self.meeting_question.setStringValue_(state[1])

    def showPage_(self, sender):
        page = PAGES[sender.tag()]
        # Navigating away and back starts clean: a stale error from an earlier
        # visit must not greet the user again.
        self.page_errors.pop(page, None)
        self.page_notices.pop(page, None)
        if page == "Dictionary":
            self.dictionary_error_card = None
        if page == "Models":
            self.storage_message = ("", True)
        self.show_page(page)

    def showHotPhrases_(self, _sender):
        self.show_page("Hot Phrases")

    @objc.python_method
    def _page_ask(self):
        from localflow.ask_mac import AskPageController
        if not hasattr(self, "ask_page"):
            self.ask_page = AskPageController.alloc().initWithHub_(self)
        self.ask_page.render()

    @objc.python_method
    def _page_settings(self):
        from localflow.hub_settings_mac import SettingsPageController
        if not hasattr(self, "settings_page"):
            self.settings_page = SettingsPageController.alloc().initWithHub_(self)
        self.settings_page.render()

    @objc.python_method
    def _page_home(self):
        self.config = replace(
            self.config,
            dictation_enabled=self._live_config.get().dictation_enabled,
        )
        state = dictation_state(self.config)
        width = self.body.frame().size.width
        self.dictation_switch = NSSwitch.alloc().initWithFrame_(
            NSMakeRect(width - 54, self.cursor_y + 4, 54, 32)
        )
        self.dictation_switch.setTarget_(self)
        self.dictation_switch.setAction_("toggleDictation:")
        self.dictation_switch.setAccessibilityLabel_("Dictation")
        self.dictation_switch.setState_(
            AppKit.NSControlStateValueOn
            if state["enabled"]
            else AppKit.NSControlStateValueOff
        )
        self.body.addSubview_(self.dictation_switch)
        self.body.addSubview_(
            _label(
                state["label"],
                0,
                self.cursor_y,
                width - 68,
                17,
                "ink",
                palette=self.tokens,
                height=26,
                weight="semibold",
            )
        )
        self.body.addSubview_(
            _label(
                state["detail"],
                0,
                self.cursor_y + 30,
                width,
                12,
                "muted",
                palette=self.tokens,
                height=22,
            )
        )
        self.cursor_y += 76
        stats = self.history.stats()
        width = self.body.frame().size.width
        # The decoration lives in the header band only (it ends above the stat
        # cards at HUB_HEADER_OFFSET) and the view clips its own drawing, so it
        # never paints over a card at any width.
        halftone = HalftoneView.alloc().initWithFrame_palette_(
            NSMakeRect(*home_decoration_frame(width)), self.tokens
        )
        halftone.anchor, halftone.step, halftone.max_radius, halftone.pad = (
            "top-right", 9, 3, 4,
        )
        self.body.addSubview_(halftone)
        streak = stats["current_streak_days"]
        longest_streak = stats["longest_streak_days"]
        cards = (
            ("Words", stats["total_words"]),
            ("Avg WPM", f'{stats["avg_wpm"]:.1f}'),
            ("Streak", f"{streak} day{'s' if streak != 1 else ''}"),
            ("Dictations", stats["total_dictations"]),
        )
        columns, card_width = stat_layout(width, len(cards))
        stat_rows = (len(cards) + columns - 1) // columns
        for index, (title, value) in enumerate(cards):
            row, column = divmod(index, columns)
            card = FlippedView.alloc().initWithFrame_(
                NSMakeRect(
                    column * (card_width + 12), self.cursor_y + row * 116,
                    card_width, 104,
                )
            )
            style_surface(
                card, self.tokens,
                fill="sun" if title == "Streak" else "surface",
                border="ink" if title == "Streak" else "line",
                shadow=title == "Streak",
            )
            card.addSubview_(
                _label(
                    title.upper(), 16, 16, card_width - 32, 16,
                    SUN_TEXT if title == "Streak" else "muted",
                    palette=self.tokens, height=18, weight="semibold",
                )
            )
            card.addSubview_(
                _label(
                    str(value), 16, 42, card_width - 32, 28,
                    SUN_TEXT if title == "Streak" else "ink",
                    palette=self.tokens, height=34, weight="bold",
                )
            )
            self.body.addSubview_(card)
        self.cursor_y += stat_rows * 116 + 4
        self.body.addSubview_(
            _label(
                f'Today  {stats["words_today"]} words     Last 7 days  '
                f'{stats["words_7d"]} words     Longest streak  {longest_streak} day'
                f'{"s" if longest_streak != 1 else ""}',
                0, self.cursor_y, width, 12, "muted", palette=self.tokens, height=20,
            )
        )
        self.cursor_y += 36
        self._speed_card(width)
        self._add_text("Recent dictations", size=15, weight="semibold")
        rows = self.history.recent(limit=4)
        if not rows:
            self._empty_state(
                self.body, 0, self.cursor_y, width, 220,
                "No dictations yet",
                "Your recent dictations will appear here.",
                "Open setup", "openSetup:",
            )
            return
        listing = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, len(rows) * 56)
        )
        style_surface(listing, self.tokens)
        self.body.addSubview_(listing)
        for index, row in enumerate(rows):
            self._history_row(listing, row, index * 56, actions=False, width=width)
            if index < len(rows) - 1:
                divider = NSView.alloc().initWithFrame_(
                    NSMakeRect(12, (index + 1) * 56 - 1, width - 24, 1)
                )
                divider.setWantsLayer_(True)
                divider.layer().setBackgroundColor_(_color("line", self.tokens).CGColor())
                listing.addSubview_(divider)

    @objc.python_method
    def _perf_config(self):
        try:
            return load_config() if config_path().exists() else self._live_config.get()
        except (ConfigError, OSError, ValueError):
            return self._live_config.get()

    @objc.python_method
    def _speed_card(self, width):
        """The Speed card under the stats line; nothing at all without data
        (`perf_summary` unset, perf metrics off or no dictations in 7 days)."""
        card = home_speed_card(
            getattr(self, "perf_summary", None), self._perf_config(),
        )
        self.speed_card = card
        if card is None:
            return
        view = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, card.height())
        )
        style_surface(view, self.tokens)
        view.addSubview_(
            _label(
                SPEED_TITLE.upper(), 16, 14, 160, 12, "muted",
                palette=self.tokens, height=16, weight="semibold",
            )
        )
        caption = _label(
            card.caption, width - 236, 14, 220, 12, "muted",
            palette=self.tokens, height=16,
        )
        caption.setAlignment_(AppKit.NSTextAlignmentRight)
        view.addSubview_(caption)
        column_w = (width - 32) / 3
        for index, (label, value) in enumerate(card.metrics()):
            x = 16 + index * column_w
            view.addSubview_(
                _label(label, x, 40, column_w - 12, 12, "muted", palette=self.tokens, height=16)
            )
            number = _label(
                value, x, 58, column_w - 12, 22, "ink", palette=self.tokens, height=30,
            )
            # Tabular digits: the numbers do not jiggle as they change.
            number.setFont_(
                AppKit.NSFont.monospacedDigitSystemFontOfSize_weight_(
                    22, AppKit.NSFontWeightSemibold,
                )
            )
            view.addSubview_(number)
        if card.hint:
            dot = NSView.alloc().initWithFrame_(NSMakeRect(16, 104, 8, 8))
            dot.setWantsLayer_(True)
            dot.layer().setBackgroundColor_(_color("sun", self.tokens).CGColor())
            dot.layer().setCornerRadius_(4)
            view.addSubview_(dot)
            view.addSubview_(
                _label(
                    card.hint, 32, 99, width - 48, 12, "ink",
                    palette=self.tokens, height=18,
                )
            )
        self.body.addSubview_(view)
        self.cursor_y += card.height() + 24

    def toggleDictation_(self, sender):
        try:
            updated = replace(
                load_config(),
                dictation_enabled=bool(sender.state()),
            )
            save_config(updated)
            self.config = updated
            self.page_errors.pop("Home", None)
        except Exception as exc:  # noqa: BLE001 - shown inline
            self.page_errors["Home"] = f"Couldn't change dictation: {exc}"
        self.show_page("Home")

    @objc.python_method
    def _empty_state(self, parent, x, y, width, height, title, detail, action, selector):
        state = FlippedView.alloc().initWithFrame_(
            NSMakeRect(x, y, width, height)
        )
        state.addSubview_(
            _label(title, 24, 58, width - 48, 15, "ink", palette=self.tokens, height=28, weight="semibold")
        )
        state.addSubview_(
            _label(
                detail, 24, 92, width - 48, 13, "muted", palette=self.tokens,
                height=44, wrap=True,
            )
        )
        self._button(
            action, selector, 24, 140, width=138, parent=state,
        )
        parent.addSubview_(state)

    @objc.python_method
    def _history_row(self, parent, row, y, actions, width):
        text = row.get("final_text") or row.get("raw_text") or "(no text)"
        timestamp = datetime.fromtimestamp(float(row.get("ts") or 0))
        if timestamp.date() == datetime.now().date():
            date_label = timestamp.strftime("%I:%M %p").lstrip("0")
        else:
            date_label = timestamp.strftime("%b %d").replace(" 0", " ")
        action_start = width
        if actions:
            entries = [
                ("Copy", "copyEntry:", 56),
                ("Original", "rawEntry:", 76),
            ]
            if row["kind"] == "dictate" and row.get("audio_path"):
                entries.append(("Retry", "retryEntry:", 60))
            entries.append(("Delete", "deleteEntry:", 72))
            action_start = width - 12 - sum(
                button_width for _title, _action, button_width in entries
            ) - 8 * (len(entries) - 1)
            x = action_start
            for title, action, button_width in entries:
                self._button(
                    title, action, x, y + 10, row["id"], button_width,
                    parent=parent,
                    kind="destructive_ghost" if title == "Delete" else "ghost",
                )
                x += button_width + 8
        text_width = (action_start - 24) if actions else width - 40
        status = row.get("status", "ok")
        if status != "ok":
            chip_x = action_start - 82 if actions else width - 102
            text_width = chip_x - 26
        else:
            chip_x = 0
        parent.addSubview_(
            _label(
                text, 14, y + 7, text_width, 13,
                "ink", palette=self.tokens, height=18,
            )
        )
        parent.addSubview_(
            _label(
                f'{row.get("app_name") or "Unknown"}  ·  {date_label}  ·  '
                f'{row["kind"].title()}',
                14, y + 30, text_width, 11,
                "muted", palette=self.tokens, height=16,
            )
        )
        if status != "ok":
            chip = FlippedView.alloc().initWithFrame_(
                NSMakeRect(chip_x, y + 16, 72, 22)
            )
            chip.setWantsLayer_(True)
            chip.layer().setCornerRadius_(11)
            chip.layer().setBackgroundColor_(
                _color("red" if status == "failed" else "line", self.tokens).CGColor()
            )
            chip.addSubview_(
                _label(
                    "Failed" if status == "failed" else "Silent",
                    8, 2, 58, 9, "paper" if status == "failed" else "muted", palette=self.tokens, height=18, weight="semibold",
                )
            )
            parent.addSubview_(chip)

    @objc.python_method
    def _page_history(self):
        self.history_offset = 0
        self.history_query = ""
        width = self.body.frame().size.width
        self.search_field = self._field(
            self.history_query, 0, self.cursor_y, width - 112,
            action="searchHistory:",
        )
        self.search_field.setPlaceholderString_("Search transcripts")
        self._button("Search", "searchHistory:", width - 100, self.cursor_y, width=100)
        self.cursor_y += 48
        self.history_results_height = 520
        self.history_results = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, self.history_results_height)
        )
        self.body.addSubview_(self.history_results)
        self._render_history()

    @objc.python_method
    def _render_history(self, query=None):
        if query is not None:
            self.history_query = query
            self.history_offset = 0
        width = self.body.frame().size.width
        for view in list(self.history_results.subviews()):
            view.removeFromSuperview()
        rows = self.history.recent(
            query=self.history_query or None,
            limit=9,
            offset=self.history_offset,
        )
        has_older = len(rows) > 8
        rows = rows[:8]
        listing_height = 330
        if not rows:
            empty = (
                (
                    "No matching entries",
                    "Try another search or start a new dictation.",
                    "Clear search",
                    "clearSearch:",
                )
                if self.history_query
                else (
                    "No dictations yet",
                    "Your dictations will appear here.",
                    "Open setup",
                    "openSetup:",
                )
            )
            self._empty_state(
                self.history_results, 0, 110, width, 220, *empty,
            )
        else:
            listing_height = len(rows) * 56
            listing = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, 0, width, len(rows) * 56)
            )
            style_surface(listing, self.tokens)
            self.history_results.addSubview_(listing)
            for index, row in enumerate(rows):
                self._history_row(listing, row, index * 56, actions=True, width=width)
                if index < len(rows) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, (index + 1) * 56 - 1, width - 24, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    listing.addSubview_(divider)
        page_y = listing_height + 16
        self.history_results_height = page_y + 36
        self.history_results.setFrame_(
            NSMakeRect(0, self.cursor_y, width, self.history_results_height)
        )
        middle = width // 2
        if self.history_offset:
            self._button(
                "Newer", "newerHistory:", middle - 145, page_y, width=90,
                parent=self.history_results,
            )
        self.history_results.addSubview_(
            _label(
                f"Page {self.history_offset // 8 + 1}",
                middle - 45, page_y + 8, 90, 13, "muted", palette=self.tokens, height=18, weight="semibold",
            )
        )
        if has_older:
            self._button(
                "Older", "olderHistory:", middle + 55, page_y, width=90,
                parent=self.history_results,
            )
        self._fit_body_height()

    def clearSearch_(self, _sender):
        self.search_field.setStringValue_("")
        self._render_history("")

    def searchHistory_(self, _sender):
        self._render_history(self.search_field.stringValue())

    def newerHistory_(self, _sender):
        self.history_offset = max(0, self.history_offset - 8)
        self._render_history()

    def olderHistory_(self, _sender):
        self.history_offset += 8
        self._render_history()

    def copyEntry_(self, sender):
        import pyperclip
        row = self.history.get(sender.tag())
        if row:
            pyperclip.copy(row["final_text"] or "")

    def rawEntry_(self, sender):
        import pyperclip
        row = self.history.get(sender.tag())
        if row:
            pyperclip.copy(row["raw_text"] or "")

    def retryEntry_(self, sender):
        from localflow.app import retry_history_entry
        try:
            retry_history_entry(sender.tag(), self.history, config=self.config)
            self.show_page("History")
        except Exception as exc:  # noqa: BLE001
            self.page_errors["History"] = str(exc)
            self.show_page("History")

    def deleteEntry_(self, sender):
        self.history.delete(sender.tag())
        self.show_page("History")

    def clearHistory_(self, _sender):
        alert = NSAlert.alloc().init()
        alert.setMessageText_("Clear all history and stored clips?")
        alert.addButtonWithTitle_("Clear")
        alert.addButtonWithTitle_("Cancel")
        if alert.runModal() == AppKit.NSAlertFirstButtonReturn:
            self.history.clear()
            self.show_page("History")

    @objc.python_method
    def _card_label(
        self, card, text, y, *, size=13, color="ink", weight="regular",
        height=22, wrap=False,
    ):
        card.addSubview_(
            _label(
                text, CARD_PADDING, y, card.frame().size.width - 2 * CARD_PADDING,
                size, color, palette=self.tokens, height=height, weight=weight,
                wrap=wrap,
            )
        )

    @objc.python_method
    def _dictionary_card(self, title, detail, height):
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, self.body.frame().size.width, height)
        )
        style_surface(card, self.tokens)
        self.body.addSubview_(card)
        self._card_label(card, title, CARD_PADDING, size=15, weight="semibold")
        self._card_label(card, detail, CARD_PADDING + 24, size=12, color="muted", height=18)
        self.cursor_y += height + 12
        return card

    @objc.python_method
    def _page_dictionary(self):
        self.editing_word = None
        self.editing_source = None
        width = self.body.frame().size.width
        inner = width - 2 * CARD_PADDING
        error = self.page_errors.get("Dictionary")
        error_card = self.dictionary_error_card if error else None
        if error and error_card not in ("word", "replacement"):
            # Not tied to a card (import, load): show it once, under the title.
            self.body.addSubview_(
                _label(
                    error, 0, self.cursor_y, width, 12, "red",
                    palette=self.tokens, height=22,
                )
            )
            self.cursor_y += 30

        word_card = self._dictionary_card(
            "Words LocalFlow should know",
            "Names, products and jargon that dictation should spell your way.",
            118 + (22 if error_card == "word" else 0),
        )
        add_width = 120
        self.word_field = self._field(
            "", CARD_PADDING, 70, inner - add_width - 12, parent=word_card,
            action="addWord:",
        )
        self.word_field.setPlaceholderString_("e.g. Kubernetes")
        self.add_word_button = self._button(
            "Add word", "addWord:", width - CARD_PADDING - add_width, 68,
            width=add_width, parent=word_card, kind="primary",
        )
        if error_card == "word":
            self._card_label(word_card, error, 108, size=12, color="red", height=20)

        replace_card = self._dictionary_card(
            "Auto-replace",
            "When you say one thing, LocalFlow types another.",
            138 + (22 if error_card == "replacement" else 0),
        )
        button_width = 160
        arrow_width = 28
        field_width = (inner - button_width - 12 - arrow_width - 16) // 2
        second_x = CARD_PADDING + field_width + 8 + arrow_width + 8
        replace_card.addSubview_(
            _label(
                "When I say", CARD_PADDING, 66, field_width, 11, "ink",
                palette=self.tokens, height=18, weight="semibold",
            )
        )
        replace_card.addSubview_(
            _label(
                "Type this", second_x, 66, field_width, 11, "ink",
                palette=self.tokens, height=18, weight="semibold",
            )
        )
        self.from_field = self._field(
            "", CARD_PADDING, 88, field_width, parent=replace_card,
            action="addReplacement:",
        )
        self.from_field.setPlaceholderString_("e.g. my email")
        replace_card.addSubview_(
            _label(
                "→", CARD_PADDING + field_width + 8, 92, arrow_width, 15, "muted",
                palette=self.tokens, height=24,
            )
        )
        self.to_field = self._field(
            "", second_x, 88, field_width, parent=replace_card,
            action="addReplacement:",
        )
        self.to_field.setPlaceholderString_("e.g. me@example.com")
        self.add_replacement_button = self._button(
            "Add replacement", "addReplacement:",
            width - CARD_PADDING - button_width, 86,
            width=button_width, parent=replace_card, kind="primary",
        )
        if error_card == "replacement":
            self._card_label(replace_card, error, 126, size=12, color="red", height=20)

        self._button(
            "Import CSV/JSON…", "importDictionary:",
            0, self.cursor_y, width=170,
        )
        self.cursor_y += 48
        notice = self.page_notices.get("Dictionary")
        if notice:
            banner = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, self.cursor_y, width, 36)
            )
            banner.setWantsLayer_(True)
            banner.layer().setBackgroundColor_(_color("sun", self.tokens).CGColor())
            self.body.addSubview_(banner)
            self.body.addSubview_(
                _label(
                    notice, 10, self.cursor_y + 8, width - 20, 11,
                    SUN_TEXT, palette=self.tokens, height=20,
                )
            )
            self.cursor_y += 44
        self._add_text("Your dictionary", size=15, weight="semibold", height=22, gap=8)
        self._render_dictionary()

    @objc.python_method
    def _sync_dictionary_buttons(self):
        self.add_word_button.setTitle_("Save word" if self.editing_word else "Add word")
        self.add_replacement_button.setTitle_(
            "Save replacement" if self.editing_source else "Add replacement"
        )

    @objc.python_method
    def _render_dictionary(self):
        width = self.body.frame().size.width
        entries = dictionary_entries(self.dictionary.list())
        if hasattr(self, "dictionary_scroll"):
            self.dictionary_scroll.removeFromSuperview()
        self.dictionary_keys = {}
        # Every entry is rendered; the list scrolls instead of being capped.
        listing_height = 220 if not entries else len(entries) * 56
        self.dictionary_results = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, listing_height)
        )
        self.dictionary_results.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        style_surface(self.dictionary_results, self.tokens)
        if entries:
            for index, (label, key, kind, target) in enumerate(entries):
                y = index * 56
                self.dictionary_results.addSubview_(
                    _label(
                        label, 14, y + 17, width - 214, 13, "ink",
                        palette=self.tokens, height=20,
                    )
                )
                self._button(
                    "Edit", "editDictionary:", width - 180, y + 10, index, 72,
                    parent=self.dictionary_results, kind="ghost",
                )
                self._button(
                    "Remove", "removeDictionary:", width - 100, y + 10, index, 88,
                    parent=self.dictionary_results, kind="destructive_ghost",
                )
                self.dictionary_keys[index] = (key, kind, target)
                if index < len(entries) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, (index + 1) * 56 - 1, width - 24, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    self.dictionary_results.addSubview_(divider)
        else:
            self._empty_state(
                self.dictionary_results, 0, 0, width, 200,
                "No vocabulary yet",
                "Add a word or replacement to personalize dictation.",
                "Add a word", "focusWord:",
            )
        available = self._viewport_height() - self.cursor_y - HUB_PAGE_BOTTOM
        scroll_height = min(listing_height, max(168, available))
        self.dictionary_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, scroll_height)
        )
        self.dictionary_scroll.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        self.dictionary_scroll.setDocumentView_(self.dictionary_results)
        self.dictionary_scroll.setHasVerticalScroller_(listing_height > scroll_height)
        self.dictionary_scroll.setAutohidesScrollers_(True)
        self.dictionary_scroll.setDrawsBackground_(False)
        self.body.addSubview_(self.dictionary_scroll)

    @objc.python_method
    def _reload_dictionary(
        self, error=None, card=None, clear=None, forget=None, notice=None,
        forget_kind=None,
    ):
        """Rebuild Dictionary keeping what was typed, the selection (editing ids)
        and, on failure, showing `error` beside the card it belongs to.

        Success clears the Dictionary error and the fields of the card that
        was submitted (`clear` is "word" or "replacement"); `forget` drops the
        editing id of an entry that no longer exists.
        """
        _kind, word, source, target, editing_word, editing_source = (
            self._preserve_page_state() or ("dictionary", "", "", "", None, None)
        )
        if error:
            self.page_errors["Dictionary"] = error
            self.dictionary_error_card = card
        else:
            self.page_errors.pop("Dictionary", None)
            self.page_notices.pop("Dictionary", None)
            self.dictionary_error_card = None
            if clear == "word":
                word, editing_word = "", None
            elif clear == "replacement":
                source, target, editing_source = "", "", None
            if forget:
                if (
                    forget_kind in (None, "word") and editing_word
                    and editing_word.casefold() == forget.casefold()
                ):
                    editing_word = None
                if (
                    forget_kind in (None, "replacement") and editing_source
                    and editing_source.casefold() == forget.casefold()
                ):
                    editing_source = None
            if notice:
                self.page_notices["Dictionary"] = notice
        self.show_page("Dictionary")
        self._restore_page_state(
            ("dictionary", word, source, target, editing_word, editing_source)
        )

    def addWord_(self, _sender):
        error = submit_word(
            self.dictionary, self.word_field.stringValue(), self.editing_word
        )
        self._reload_dictionary(error, "word", clear="word")

    def addReplacement_(self, _sender):
        error = submit_replacement(
            self.dictionary, self.from_field.stringValue(),
            self.to_field.stringValue(), self.editing_source,
        )
        self._reload_dictionary(error, "replacement", clear="replacement")

    def removeDictionary_(self, sender):
        key, kind, _target = self.dictionary_keys.get(
            sender.tag(), ("", "", "")
        )
        error = remove_entry(self.dictionary, key, kind or None)
        self._reload_dictionary(error, None, forget=key, forget_kind=kind)

    def editDictionary_(self, sender):
        key, kind, target = self.dictionary_keys.get(sender.tag(), ("", "", ""))
        if kind == "word":
            self.editing_word = key
            self.word_field.setStringValue_(key)
        else:
            self.editing_source = key
            self.from_field.setStringValue_(key)
            self.to_field.setStringValue_(target)
        self._sync_dictionary_buttons()

    def importDictionary_(self, _sender):
        panel = AppKit.NSOpenPanel.openPanel()
        panel.setAllowedFileTypes_(["csv", "json"])
        if panel.runModal() == AppKit.NSModalResponseOK:
            try:
                counts = self.dictionary.import_file(panel.URL().path())
            except Exception:  # noqa: BLE001
                self._reload_dictionary(
                    "Couldn't import that file. Choose a CSV or JSON file of "
                    "words and replacements.",
                    None,
                )
                return
            self._reload_dictionary(
                notice=(
                    f'Imported {counts["added"]}; duplicates {counts["skipped_duplicate"]}; '
                    f'invalid {counts["skipped_invalid"]}.'
                ),
            )

    @objc.python_method
    def _page_style(self):
        self.cleanup_options = ("none", "light", "medium")
        self.cleanup_selection = self.config.cleanup_level
        self.cleanup_buttons = []
        self.style_options = ("formal", "casual", "very_casual", "excited")
        self.style_selections = {}
        self.style_buttons = {}
        self.style_groups = {}
        self.style_tag_map = {}
        self.body.addSubview_(
            _label(
                "Cleanup level", 0, self.cursor_y + 8, 150, 15,
                "ink", palette=self.tokens, height=22, weight="semibold",
            )
        )
        # Segmented groups take the room left of the 180pt label column, capped
        # so wide windows do not stretch the controls.
        group_width = min(560, self.body.frame().size.width - 180)
        cleanup_width = min(330, group_width)
        cleanup_group = self._segment_group(
            self.cursor_y, 180, cleanup_width,
        )
        self.cleanup_group = cleanup_group
        for index, (value, title) in enumerate(
            zip(self.cleanup_options, ("None", "Light", "Medium"))
        ):
            button = self._segment_button(
                title, "selectCleanupLevel_", index,
                index * cleanup_width // 3, 1, cleanup_width // 3,
                parent=cleanup_group,
            )
            self.cleanup_buttons.append(button)
        _segmented(
            cleanup_group, self.cleanup_buttons,
            self.cleanup_options.index(self.cleanup_selection), self.tokens,
        )
        self.cursor_y += 58
        examples = {
            "work": "Please share the updated timeline.",
            "email": "Thanks for following up.",
            "personal": "Let's catch up soon!",
            "other": "The main point is clear.",
        }
        for index, (category, example) in enumerate(examples.items()):
            y = self.cursor_y
            frames = style_row_frames(y, group_width)
            label_x, label_y, label_w, label_h = frames["label"]
            self.body.addSubview_(
                _label(
                    f"{category.title()} style", label_x, label_y, label_w, 15,
                    "ink", palette=self.tokens, height=label_h, weight="semibold",
                )
            )
            # The example wraps onto a second line inside the row instead of
            # being cut off in the 160pt label column.
            example_x, example_y, example_w, example_h = frames["example"]
            self.body.addSubview_(
                _label(
                    example, example_x, example_y, example_w, 11, "muted",
                    palette=self.tokens, height=example_h, wrap=True,
                )
            )
            self.style_selections[category] = getattr(self.config, f"style_{category}")
            self.style_buttons[category] = []
            group = self._segment_group(y, 180, group_width)
            self.style_groups[category] = group
            option_width = group_width // len(self.style_options)
            for option_index, value in enumerate(self.style_options):
                tag = index * len(self.style_options) + option_index
                self.style_tag_map[tag] = (category, value)
                self.style_buttons[category].append(
                    self._segment_button(
                        value.replace("_", " ").title(),
                        "selectStyle_",
                        tag,
                        option_index * option_width, 1, option_width,
                        parent=group,
                    )
                )
            _segmented(
                group, self.style_buttons[category],
                self.style_options.index(self.style_selections[category]),
                self.tokens,
            )
            self.cursor_y += 86
        # No Save button: every click is saved right away (selectCleanupLevel_
        # / selectStyle_ -> _style_autosave); a failure is shown in the callout.
        self._add_text(STYLE_AUTOSAVE_HINT, size=12, color="muted", height=20, gap=8)

    @objc.python_method
    def _segment_group(self, y, x, width):
        group = FlippedView.alloc().initWithFrame_(
            NSMakeRect(x, y, width, 36)
        )
        self.body.addSubview_(group)
        return group

    @objc.python_method
    def _segment_button(self, title, action, tag, x, y, width, *, parent=None):
        button = NSButton.alloc().initWithFrame_(NSMakeRect(x, y, width, 34))
        button.setTitle_(title)
        button.setTarget_(self)
        button.setAction_(objc.selector(getattr(self, action), signature=b"v@:@"))
        button.setTag_(tag)
        button.setBordered_(False)
        (parent if parent is not None else self.body).addSubview_(button)
        return button

    @objc.python_method
    def _update_segment_buttons(self, group, buttons, selected):
        _segmented(group, buttons, selected, self.tokens)

    def selectCleanupLevel_(self, sender):
        self.cleanup_selection = self.cleanup_options[sender.tag()]
        # The clicked segment is restyled in place (no page rebuild), then the
        # choice is saved straight away.
        self._update_segment_buttons(
            self.cleanup_group, self.cleanup_buttons, sender.tag()
        )
        self._style_autosave()

    def selectStyle_(self, sender):
        category, value = self.style_tag_map[sender.tag()]
        self.style_selections[category] = value
        selected = self.style_options.index(value)
        self._update_segment_buttons(
            self.style_groups[category], self.style_buttons[category], selected
        )
        self._style_autosave()

    @objc.python_method
    def _style_autosave(self):
        """Persist the Style selections through the one config write path.

        The committed `self.config` only changes once save_config succeeded.
        On failure it is left as it was, the page shows the error inline and
        the clicked (pending) selections stay visible, so the next click saves
        them all again.
        """
        try:
            updated = apply_config_update(
                self.config, style_update(self.cleanup_selection, self.style_selections),
            )
            save_config(updated)
        except Exception as exc:  # noqa: BLE001
            message = str(exc) if isinstance(exc, ConfigError) else STYLE_SAVE_FAILED
            state = self._preserve_page_state()
            self.page_errors["Style"] = message
            self.show_page("Style")
            self._restore_page_state(state)
            return False
        self.config = updated
        if self.page_errors.pop("Style", None) is not None:
            # Only an earlier error forces a rebuild (to drop its callout);
            # a normal click keeps the in-place segment update above.
            self._rebuild_current_page()
        return True

    @objc.python_method
    def _meeting_button(self, title, action, x, y, width, kind="secondary"):
        return self._button(
            title, action, x, y, width=width, kind=kind,
        )

    @objc.python_method
    def _page_meetings(self):
        width = self.body.frame().size.width
        # Everything is placed relative to the standard header offset (plus any
        # error callout), never from y=0 where the page title sits.
        top = self.cursor_y
        notes_height = meeting_notes_height(self._viewport_height(), top)
        self.meeting_top, self.meeting_notes_h = top, notes_height
        frames = meeting_form_layout(width, top, notes_height)

        def rect(key):
            return NSMakeRect(*frames[key])

        # The subtitle and the connection message share the line under the
        # page title: the message replaces the subtitle while it has text.
        self.meeting_subtitle = _label(
            MEETING_SUBTITLE, 0, top, width, 13, "muted",
            palette=self.tokens, height=20,
        )
        self.body.addSubview_(self.meeting_subtitle)
        self.meeting_message = _label(
            "" if self.meeting_status else "Connecting to LocalFlow…",
            0, top, width, 13, "muted", palette=self.tokens, height=20,
        )
        self.body.addSubview_(self.meeting_message)
        self.meeting_retry = self._button(
            "Retry", "meetingRetry:", *frames["retry"][:2], width=100,
        )
        self.meeting_retry.setHidden_(True)
        self.meeting_watch = _label(
            "", 0, 0, width, 12, "muted", palette=self.tokens, height=20,
        )
        self.body.addSubview_(self.meeting_watch)
        self.meeting_title_label = _label(
            "Meeting title", *frames["title_label"][:3], 11,
            "ink", palette=self.tokens, height=frames["title_label"][3],
            weight="semibold",
        )
        self.body.addSubview_(self.meeting_title_label)
        # Title field and template picker: one shared input box each, the
        # same height and the same top edge (see mac_style.input_box).
        self.meeting_title = NSTextField.alloc().initWithFrame_(rect("title_field"))
        self.meeting_title.setStringValue_(self.meeting_title_value or "")
        self.meeting_title.setPlaceholderString_(MEETING_TITLE_PLACEHOLDER)
        self.meeting_title.setAccessibilityLabel_("Meeting title")
        # Only to notice a real edit (controlTextDidChange_): a typed title,
        # even "Meeting", is the user's and is never replaced on save.
        self.meeting_title.setDelegate_(self)
        self.meeting_title_box = input_box(
            self.meeting_title, frames["title_field"], self.tokens,
        )
        self.body.addSubview_(self.meeting_title_box)
        self.meeting_template_label = _label(
            "Notes template", *frames["template_label"][:3], 11,
            "ink", palette=self.tokens, height=frames["template_label"][3],
            weight="semibold",
        )
        self.body.addSubview_(self.meeting_template_label)
        self.meeting_template = NSComboBox.alloc().initWithFrame_(rect("template_field"))
        self.meeting_template.setCompletes_(False)
        self.meeting_template.setAccessibilityLabel_("Notes template")
        self.meeting_template_box = input_box(
            self.meeting_template, frames["template_field"], self.tokens,
        )
        self.body.addSubview_(self.meeting_template_box)
        if self.meeting_templates:
            self._meeting_fill_templates()

        self.body.addSubview_(
            _label(
                "Your notes", *frames["notes_label"][:3], 13,
                "ink", palette=self.tokens, height=frames["notes_label"][3],
                weight="semibold",
            )
        )
        # Autosave status rides on the heading row: no blank band is kept
        # for it under the card, and typing never moves the Start button.
        self.meeting_notes_status = _label(
            "", *frames["notes_status"][:3], 11, "muted",
            palette=self.tokens, height=frames["notes_status"][3],
        )
        self.meeting_notes_status.setAlignment_(AppKit.NSTextAlignmentRight)
        self.body.addSubview_(self.meeting_notes_status)
        _notes_x, _notes_y, notes_w, notes_h = frames["notes"]
        self.meeting_notes_card = FlippedView.alloc().initWithFrame_(rect("notes"))
        style_surface(
            self.meeting_notes_card, self.tokens, border="ink",
            width=CARD_BORDER_WIDTH,
        )
        self.meeting_notes_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(4, 4, notes_w - 8, notes_h - 8)
        )
        self.meeting_notes_scroll.setHasVerticalScroller_(True)
        self.meeting_notes_scroll.setAutohidesScrollers_(True)
        self.meeting_notes_scroll.setBorderType_(AppKit.NSNoBorder)
        self.meeting_notes_scroll.setDrawsBackground_(False)
        content = self.meeting_notes_scroll.contentSize()
        self.meeting_notes = PlaceholderTextView.alloc().initWithFrame_(
            NSMakeRect(0, 0, content.width, content.height)
        )
        self.meeting_notes.setMinSize_(NSMakeSize(0, content.height))
        self.meeting_notes.setMaxSize_(NSMakeSize(10000000, 10000000))
        self.meeting_notes.setVerticallyResizable_(True)
        self.meeting_notes.setHorizontallyResizable_(False)
        self.meeting_notes.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        self.meeting_notes.textContainer().setWidthTracksTextView_(True)
        self.meeting_notes.setTextContainerInset_(NSMakeSize(6, 8))
        self.meeting_notes.setFont_(_font(13))
        self.meeting_notes.setRichText_(False)
        self.meeting_notes.setDrawsBackground_(False)
        self.meeting_notes.setTextColor_(_color("ink", self.tokens))
        self.meeting_notes.setInsertionPointColor_(_color("ink", self.tokens))
        self.meeting_notes.setAccessibilityLabel_("Your notes")
        # Drawn only while the box is empty; never part of string(), so it is
        # never autosaved, sent with Start, or handed to the notes model.
        self.meeting_notes.setPlaceholder_color_(
            MEETING_NOTES_PLACEHOLDER, _color("muted", self.tokens),
        )
        self.meeting_notes.setString_(self.meeting_notes_text)
        self.meeting_notes.setDelegate_(self)
        self.meeting_notes_scroll.setDocumentView_(self.meeting_notes)
        self.meeting_notes_card.addSubview_(self.meeting_notes_scroll)
        self.body.addSubview_(self.meeting_notes_card)
        self.meeting_error_label = _label(
            "", 0, 0, width, 12, "red", palette=self.tokens, height=18, wrap=True,
        )
        self.body.addSubview_(self.meeting_error_label)
        self.meeting_action = self._button(
            "Start meeting transcription", "meetingAction:", 0, frames["action_y"],
            width=250, kind="primary",
        )
        self.meeting_live_label = _label(
            "", 8, 8, width - 16, 11, "ink", palette=self.tokens, height=82,
        )
        self.meeting_live_label.setLineBreakMode_(
            AppKit.NSLineBreakByWordWrapping
        )
        self.meeting_live_label.setMaximumNumberOfLines_(0)
        # Everything from here down is positioned by _meeting_layout, which
        # follows the action button; the frames below are placeholders.
        self.meeting_live_card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, 98)
        )
        style_surface(self.meeting_live_card, self.tokens)
        self.meeting_live_card.addSubview_(self.meeting_live_label)
        self.body.addSubview_(self.meeting_live_card)
        self.meeting_saved_label = _label(
            "", 0, 0, width - 160, 13, "ink",
            palette=self.tokens, height=22, weight="semibold",
        )
        self.body.addSubview_(self.meeting_saved_label)
        self.meeting_open_notes = self._button(
            "Open notes", "meetingOpenNotes:", width - 140, 0, width=140,
        )
        self.meeting_chat_heading = _label(
            "Ask the transcript", 0, 0, 400, 13,
            "ink", palette=self.tokens, height=22, weight="semibold",
        )
        self.body.addSubview_(self.meeting_chat_heading)
        self.meeting_question = self._field("", 0, 0, width - 120)
        self.meeting_question.setPlaceholderString_("Ask about this meeting")
        self.meeting_ask = self._button(
            "Ask", "meetingAsk:", width - 100, 0, width=100, kind="primary",
        )
        self.meeting_answer_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, 64)
        )
        self.meeting_answer_scroll.setHasVerticalScroller_(True)
        self.meeting_answer_scroll.setAutohidesScrollers_(True)
        self.meeting_answer_content = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, 64)
        )
        self.meeting_answer_scroll.setDocumentView_(self.meeting_answer_content)
        self.body.addSubview_(self.meeting_answer_scroll)
        self.meeting_recent_heading = _label(
            "Recent meetings", 0, 0, 400, 13,
            "ink", palette=self.tokens, height=22, weight="semibold",
        )
        self.body.addSubview_(self.meeting_recent_heading)
        self.meeting_recent_card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, RECENT_ROW_H)
        )
        style_surface(self.meeting_recent_card, self.tokens)
        self.body.addSubview_(self.meeting_recent_card)
        self._meeting_render_recent()
        self._meeting_render()
        self._meeting_worker("templates")
        self._meeting_poll()

    @objc.python_method
    def _meeting_load_recent(self):
        """Newest saved notes from the configured vault's notes folder.

        Called when Meetings is shown and after a stop, never from the status
        poll. Reads only <vault>/<notes_folder> (see hub_model.recent_notes).
        """
        self.meeting_recent, self.meeting_recent_error = recent_notes(
            getattr(self.config, "vault_path", ""),
            getattr(self.config, "notes_folder", ""),
        )

    @objc.python_method
    def _meeting_render_recent(self):
        """Rows of the Recent meetings card: title, date and Open."""
        card = self.meeting_recent_card
        for view in list(card.subviews()):
            view.removeFromSuperview()
        width = self.body.frame().size.width
        notes = self.meeting_recent
        if not notes:
            message = self.meeting_recent_error or RECENT_EMPTY
            card.setFrame_(NSMakeRect(0, card.frame().origin.y, width, RECENT_ROW_H))
            card.addSubview_(
                _label(
                    message, CARD_PADDING, 12, width - 2 * CARD_PADDING, 12,
                    "muted", palette=self.tokens, height=20,
                )
            )
            return
        card.setFrame_(
            NSMakeRect(0, card.frame().origin.y, width, len(notes) * RECENT_ROW_H)
        )
        date_w = 150
        open_w = 72
        for index, note in enumerate(notes):
            y = index * RECENT_ROW_H
            open_x = width - CARD_PADDING - open_w
            date_x = open_x - 12 - date_w
            card.addSubview_(
                _label(
                    note["title"], CARD_PADDING, y + 12, date_x - CARD_PADDING - 12,
                    13, "ink", palette=self.tokens, height=20, weight="semibold",
                )
            )
            date = _label(
                note["date_label"], date_x, y + 14, date_w, 11, "muted",
                palette=self.tokens, height=18,
            )
            date.setAlignment_(AppKit.NSTextAlignmentRight)
            card.addSubview_(date)
            button = self._button(
                "Open", "meetingOpenRecent:", open_x, y + 8, index, open_w,
                parent=card, kind="ghost", height=28,
            )
            button.setAccessibilityLabel_(f"Open notes: {note['title']}")
            if index < len(notes) - 1:
                divider = NSView.alloc().initWithFrame_(
                    NSMakeRect(12, y + RECENT_ROW_H - 1, width - 24, 1)
                )
                divider.setWantsLayer_(True)
                divider.layer().setBackgroundColor_(_color("line", self.tokens).CGColor())
                card.addSubview_(divider)

    @objc.python_method
    def _meeting_worker(self, operation, *args):
        if operation.startswith("notes_"):
            writer = self.meeting_notes_writer
            ticket = writer.ticket()  # main thread: the order saves were asked for

            def call():
                return writer.save(self.meeting_client, ticket, *args)
        else:
            def call():
                return getattr(self.meeting_client, operation)(*args)

        def run():
            try:
                result = call()
                error = None
            except Exception as exc:  # noqa: BLE001
                result, error = None, exc
            self.meeting_events.put((operation, result, error))

        threading.Thread(
            target=run, name=f"localflow-meeting-{operation}", daemon=True
        ).start()

    @objc.python_method
    def _meeting_poll(self):
        if self.page != "Meetings":
            return
        self.meeting_next_poll = time.monotonic() + 2
        self._meeting_worker("status")

    @objc.python_method
    def _meeting_current_notes_text(self):
        if self.page == "Meetings" and hasattr(self, "meeting_notes"):
            return self.meeting_notes.string()
        return self.meeting_notes_text

    @objc.python_method
    def _meeting_request_notes_save(self, stop_flush=False):
        if stop_flush:
            self.meeting_stop_flush_pending = True
            self.meeting_notes_save_pending = False
            if self.page == "Meetings":
                self.meeting_notes_status.setStringValue_("Saving…")
        else:
            self.meeting_notes_save_pending = True
        self._meeting_advance_notes_save()

    @objc.python_method
    def _meeting_advance_notes_save(self):
        if self.meeting_notes_save_in_flight:
            return
        if self.meeting_stop_flush_pending:
            operation = "notes_stop_flush"
            self.meeting_stop_flush_pending = False
        elif self.meeting_notes_save_pending:
            operation = "notes_autosave"
            self.meeting_notes_save_pending = False
        else:
            return
        text = self._meeting_current_notes_text()
        self.meeting_notes_save_in_flight = True
        self.meeting_notes_save_operation = operation
        self.meeting_active_notes_text = text
        self.meeting_notes_dirty = True
        if self.page == "Meetings":
            self.meeting_notes_status.setStringValue_("Saving…")
        self._meeting_worker(operation, text)

    @objc.python_method
    def _meeting_notes_save_completed(self, operation, error):
        self.meeting_notes_save_in_flight = False
        text = self.meeting_active_notes_text
        current_text = self._meeting_current_notes_text()
        is_stop_flush = operation == "notes_stop_flush"
        if error:
            self.meeting_notes_dirty = True
            self.meeting_error = str(error)
            if is_stop_flush:
                self.meeting_busy = False
                self.meeting_stop_flush_pending = False
                self.meeting_notes_save_pending = False
                self.meeting_error = (
                    "Notes could not be saved; the meeting is still running — "
                    + str(error)
                )
                if self.page == "Meetings":
                    self._meeting_render()
                return
            if self.meeting_stop_flush_pending or self.meeting_notes_save_pending:
                self._meeting_advance_notes_save()
            elif self.page == "Meetings":
                self._meeting_render()
            return

        self.meeting_error = ""
        self.meeting_notes_dirty = current_text != text
        if is_stop_flush:
            if self.meeting_notes_dirty:
                self.meeting_notes_save_pending = False
                self.meeting_stop_flush_pending = True
                self._meeting_advance_notes_save()
                return
            self._meeting_worker("stop")
            if self.page == "Meetings":
                self._meeting_set_action_title("Enhancing your notes…")
            return
        if self.meeting_stop_flush_pending or self.meeting_notes_save_pending:
            self._meeting_advance_notes_save()
        elif self.page == "Meetings" and not self.meeting_notes_dirty:
            self.meeting_notes_status.setStringValue_(
                "Saved " + datetime.now().strftime("%H:%M")
            )
            self.meeting_notes_status.setTextColor_(_color("success", self.tokens))

    @objc.python_method
    def _meeting_fill_templates(self):
        """Populate the template box from the loaded templates (also on rebuild)."""
        self.meeting_template.removeAllItems()
        self.meeting_template.addItemsWithObjectValues_(
            [item["name"] for item in self.meeting_templates]
        )
        default = self.meeting_template_default
        desired = self.meeting_template_value or default
        selected = next(
            (
                index for index, item in enumerate(self.meeting_templates)
                if item["id"] == desired
            ),
            next(
                (
                    index for index, item in enumerate(self.meeting_templates)
                    if item["id"] == default
                ),
                0,
            ),
        )
        if self.meeting_templates:
            self.meeting_template.selectItemAtIndex_(selected)
            self.meeting_template_value = self.meeting_templates[selected]["id"]

    @objc.python_method
    def _meeting_event(self, operation, result, error):
        if operation == "templates":
            if not error:
                self.meeting_templates = result.get("templates", [])
                self.meeting_template_default = result.get("default", "general")
                if self.page != "Meetings":
                    return
                self._meeting_fill_templates()
            return
        if operation in {"notes_autosave", "notes_stop_flush"}:
            self._meeting_notes_save_completed(operation, error)
            return
        if operation == "status":
            if error:
                self.meeting_status = {}
                self.meeting_error = ""
                self.meeting_session_active = False
                if self.page == "Meetings":
                    self.meeting_message.setStringValue_(str(error))
                    self.meeting_retry.setHidden_(False)
                    for view in self._meeting_page_controls():
                        view.setHidden_(True)
            else:
                self.meeting_status = result
                if result.get("last_saved"):
                    self.meeting_last_saved = result["last_saved"]
                session = result.get("session")
                if (
                    session and not self.meeting_session_active
                    and not self.meeting_notes_dirty
                ):
                    self.meeting_notes_text = session.get("user_notes", "")
                    if self.page == "Meetings":
                        self._meeting_adopting_notes = True
                        self.meeting_notes.setString_(self.meeting_notes_text)
                        self._meeting_adopting_notes = False
                if session and session.get("template"):
                    selected = next(
                        (
                            index for index, item in enumerate(self.meeting_templates)
                            if item["id"] == session["template"]
                        ),
                        -1,
                    )
                    if selected >= 0:
                        self.meeting_template_value = self.meeting_templates[selected]["id"]
                        if self.page == "Meetings":
                            self.meeting_template.selectItemAtIndex_(selected)
                self.meeting_session_active = bool(session)
                if self.page == "Meetings":
                    self.meeting_message.setStringValue_("")
                    self.meeting_retry.setHidden_(True)
                    self._meeting_render()
            if self.page == "Meetings":
                self.meeting_next_poll = time.monotonic() + 2
            return
        if operation == "start":
            self.meeting_busy = False
            if error:
                self.meeting_error = str(error)
                if self.page == "Meetings":
                    self._meeting_render()
            else:
                self.meeting_notes_dirty = False
                self.meeting_last_saved = None
                self.meeting_answers.clear()
                if self.page == "Meetings":
                    self.meeting_notes_status.setStringValue_(
                        "Saved" if self.meeting_notes.string() else ""
                    )
                self._meeting_poll()
            return
        if operation == "stop":
            self.meeting_busy = False
            if error:
                self.meeting_error = str(error)
            else:
                # The server's stop payload carries the saved title (the
                # generated one for an automatic session). Only an older
                # payload without it falls back to the session's title, and
                # never for an automatic session: its "Meeting" is a stand-in,
                # so the note's own name (from its path) is shown instead.
                previous = self.meeting_status.get("session") or {}
                result = dict(result or {})
                if previous.get("title") and not previous.get("title_auto"):
                    result.setdefault("title", previous["title"])
                self.meeting_last_saved = result
                # A new note exists: refresh Recent meetings (not on every poll).
                self._meeting_load_recent()
                if self.page == "Meetings" and hasattr(self, "meeting_recent_card"):
                    self._meeting_render_recent()
                self.meeting_notes_text = ""
                self.meeting_notes_dirty = False
                self.meeting_session_active = False
                self.meeting_status["session"] = None
                self.meeting_status["last_saved"] = result
                if self.page == "Meetings":
                    self._meeting_adopting_notes = True
                    self.meeting_notes.setString_("")
                    self._meeting_adopting_notes = False
                    self.meeting_notes_status.setStringValue_("")
            if self.page == "Meetings":
                self._meeting_render()
            self._meeting_poll()
            return
        if operation == "chat":
            self.meeting_busy = False
            answer = f"Couldn’t answer — {error}" if error else result.get("answer", "")
            self.meeting_answers[0]["answer"] = answer
            self.meeting_answers[0]["error"] = error is not None
            if self.page == "Meetings":
                self._meeting_render_chat()

    @objc.python_method
    def _meeting_page_controls(self):
        return (
            self.meeting_subtitle,
            self.meeting_watch, self.meeting_title_label, self.meeting_title_box,
            self.meeting_template_label, self.meeting_template_box,
            self.meeting_notes_card, self.meeting_notes_status,
            self.meeting_error_label, self.meeting_action, self.meeting_live_card,
            self.meeting_saved_label, self.meeting_open_notes,
            self.meeting_chat_heading, self.meeting_question,
            self.meeting_ask, self.meeting_answer_scroll,
            self.meeting_recent_heading, self.meeting_recent_card,
        )

    @objc.python_method
    def _meeting_set_action_title(self, title):
        self.meeting_action.setTitle_(title)
        self.meeting_action.sizeToFit()
        frame = self.meeting_action.frame()
        self.meeting_action.setFrame_(
            NSMakeRect(
                frame.origin.x, frame.origin.y, frame.size.width + 24, 36
            )
        )

    @objc.python_method
    def _meeting_show_title(self, session):
        """The title stays on screen in every state.

        Idle: the editable draft. Recording: the running session's own title,
        read-only but selectable, in a quiet box. The draft is set aside first
        and put back when the session ends, so showing the session title never
        overwrites what the user typed for the next meeting.
        """
        title = self.meeting_title
        # Every programmatic setStringValue_ below happens while
        # meeting_title_shows_session is True, so controlTextDidChange_ can
        # never take it for typing (AppKit doesn't post it for these anyway).
        if session:
            if not self.meeting_title_shows_session:
                self.meeting_title_value = title.stringValue()
                self.meeting_title_shows_session = True
            # An automatic session has no title yet: blank, so the
            # placeholder ("Named automatically when saved") says so instead
            # of the "Meeting" stand-in the server keeps until it names it.
            title.setStringValue_(
                "" if session.get("title_auto") else (session.get("title") or "Meeting")
            )
            title.setEditable_(False)
            title.setSelectable_(True)
        else:
            if self.meeting_title_shows_session:
                title.setStringValue_(self.meeting_title_value or "")
                self.meeting_title_shows_session = False
            title.setEditable_(True)
        title.setTextColor_(_color("ink", self.tokens))
        style_input_box(self.meeting_title_box, self.tokens, readonly=bool(session))
        style_input_box(self.meeting_template_box, self.tokens, readonly=bool(session))

    @objc.python_method
    def _meeting_error_height(self, width):
        """Wrapped height of the meeting error, 0 when there is none."""
        if not self.meeting_error:
            return 0
        error_font = _font(12)
        lines = wrap_lines(
            self.meeting_error, width - 8,
            lambda text: measure_text(text, error_font),
        )
        return max(1, len(lines)) * 16 + 2

    @objc.python_method
    def _meeting_layout(self, session, saved=None, chat_available=False):
        width = self.body.frame().size.width
        # The form part (error, action) comes from the pure layout: an error
        # takes room only while there is one, otherwise the Start button sits
        # a compact 12pt under the notes card.
        frames = meeting_form_layout(
            width, self.meeting_top, self.meeting_notes_h,
            error_height=self._meeting_error_height(width),
        )
        self.meeting_error_label.setHidden_("error" not in frames)
        if "error" in frames:
            self.meeting_error_label.setFrame_(NSMakeRect(*frames["error"]))
        current = self.meeting_action.frame()
        self.meeting_action.setFrame_(
            NSMakeRect(0, frames["action_y"], current.size.width, current.size.height)
        )
        action_frame = self.meeting_action.frame()
        watch_y = action_frame.origin.y + action_frame.size.height + 4
        watch_visible = bool(self.meeting_watch.stringValue())
        self.meeting_watch.setFrame_(
            NSMakeRect(action_frame.origin.x, watch_y, width, 20)
        )
        live_y = watch_y + (26 if watch_visible else 0)
        self.meeting_live_card.setFrame_(NSMakeRect(0, live_y, width, 98))
        self.meeting_live_label.setFrame_(NSMakeRect(8, 8, width - 16, 82))
        self.meeting_live_card.setHidden_(not bool(session))
        saved_y = live_y + (108 if session else 8)
        self.meeting_saved_label.setFrame_(
            NSMakeRect(0, saved_y + 4, width - 160, 22)
        )
        self.meeting_open_notes.setFrame_(
            NSMakeRect(width - 140, saved_y, 140, 36)
        )
        chat_y = saved_y + (48 if saved else 0)
        self.meeting_chat_heading.setFrame_(
            NSMakeRect(0, chat_y, 400, 22)
        )
        self.meeting_question.setFrame_(
            NSMakeRect(0, chat_y + 26, width - 120, 32)
        )
        self.meeting_ask.setFrame_(
            NSMakeRect(width - 100, chat_y + 26, 100, 36)
        )
        self.meeting_answer_scroll.setFrame_(
            NSMakeRect(0, chat_y + 68, width, 64)
        )
        recent_y = chat_y
        if chat_available:
            recent_y = chat_y + (140 if self.meeting_answers else 74)
        recent_y += 8
        self.meeting_recent_heading.setFrame_(NSMakeRect(0, recent_y, 400, 22))
        recent = self.meeting_recent_card.frame()
        self.meeting_recent_card.setFrame_(
            NSMakeRect(0, recent_y + 28, width, recent.size.height)
        )
        self._fit_body_height()

    @objc.python_method
    def _meeting_render(self):
        if not hasattr(self, "meeting_action"):
            return
        for view in self._meeting_page_controls():
            view.setHidden_(False)
        # The subtitle gives way to a connection message ("Connecting…").
        self.meeting_subtitle.setHidden_(bool(self.meeting_message.stringValue()))
        self.meeting_error_label.setStringValue_(self.meeting_error or "")
        session = self.meeting_status.get("session")
        self._meeting_show_title(session)
        if session:
            # One plain sentence: mic vs system audio, with the raw error only
            # choosing between "allow it" (permission) and "not available".
            self.meeting_watch.setStringValue_(meeting_capture_status(session))
            self.meeting_template.setEnabled_(False)
            self._meeting_set_action_title(
                f"Stop & save notes — {int(session.get('seconds', 0)) // 60}m"
                f"{int(session.get('seconds', 0)) % 60:02d}s · "
                f"{session.get('segment_count', 0)} segments"
            )
            lines = [
                f"[{segment.get('stamp', '00:00')}] "
                f"{'Me' if segment.get('source') == 'me' else 'Others'}: "
                f"{segment.get('text', '')}"
                for segment in session.get("segments", [])
            ]
            self.meeting_live_label.setStringValue_(
                "\n\n".join(lines) or "(listening — first transcript segment lands after ~30s)"
            )
        else:
            watch = (
                "mic in use" if self.meeting_status.get("watching")
                and self.meeting_status.get("mic_busy")
                else "watching" if self.meeting_status.get("watching")
                else "watcher off" if self.meeting_status.get("watch_error")
                else ""
            )
            self.meeting_watch.setStringValue_(watch)
            self.meeting_template.setEnabled_(True)
            self._meeting_set_action_title("Start meeting transcription")
            self.meeting_live_label.setStringValue_("")
        self.meeting_action.setEnabled_(not self.meeting_busy)
        saved = self.meeting_last_saved if not session else None
        self.meeting_saved_label.setHidden_(not bool(saved))
        self.meeting_open_notes.setHidden_(not bool(saved))
        if saved:
            # The note's title, never its raw path; Open notes still goes
            # through the vault check in meetingOpenNotes_.
            self.meeting_saved_label.setStringValue_(saved_notes_message(saved))
        chat_available = bool(
            not session
            and (self.meeting_status.get("chat_available") or self.meeting_last_saved)
        )
        self._meeting_layout(session, saved=saved, chat_available=chat_available)
        self.meeting_chat_heading.setHidden_(not chat_available)
        self.meeting_question.setHidden_(not chat_available)
        self.meeting_ask.setHidden_(not chat_available)
        self.meeting_answer_scroll.setHidden_(
            not chat_available or not self.meeting_answers
        )
        self._meeting_render_chat()

    @objc.python_method
    def _meeting_render_chat(self):
        if self.page != "Meetings" or not hasattr(self, "meeting_answer_content"):
            return
        for view in list(self.meeting_answer_content.subviews()):
            view.removeFromSuperview()
        height = max(64, len(self.meeting_answers) * 44)
        width = self.body.frame().size.width
        self.meeting_answer_content.setFrame_(NSMakeRect(0, 0, width, height))
        for index, item in enumerate(self.meeting_answers):
            label = _label(
                f"You: {item['question']}\n{item['answer']}",
                0, index * 44, width - 20, 11,
                "red" if item.get("error") else "ink",
                palette=self.tokens, height=42,
            )
            label.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
            label.setMaximumNumberOfLines_(2)
            self.meeting_answer_content.addSubview_(label)

    def meetingTimerTick_(self, _timer):
        # The Hub's one timer also drives Hot Phrases autosave, undo expiry and
        # the external-change poll; it does nothing off that page.
        if self._closed:  # a tick queued before close: leave the closed UI alone
            return
        self._phrases_tick()
        if self.page == "Home" and (
            self._live_config.get().dictation_enabled != self.config.dictation_enabled
        ):
            self.show_page("Home")
        while True:
            try:
                operation, result, error = self.meeting_events.get_nowait()
            except queue.Empty:
                break
            self._meeting_event(operation, result, error)
        if (
            self.meeting_notes_due is not None
            and time.monotonic() >= self.meeting_notes_due
        ):
            self.meeting_notes_due = None
            self._meeting_request_notes_save()
        if self.page == "Meetings" and time.monotonic() >= self.meeting_next_poll:
            self._meeting_poll()

    def textDidChange_(self, notification):
        # NSTextView delegate shared by pages: dispatch by page and by view so
        # a Hot Phrases edit is never taken for a meeting note (or the reverse).
        source = notification.object()
        if self.page == "Hot Phrases":
            if source is getattr(self, "phrase_text", None):
                self._phrases_edited()
            return
        if self.page != "Meetings" or getattr(self, "_meeting_adopting_notes", False):
            return
        notes = getattr(self, "meeting_notes", None)
        if notes is not None and source is not notes:
            return
        self.meeting_notes_dirty = True
        self.meeting_notes_text = self.meeting_notes.string()
        self.meeting_notes_status.setStringValue_("Editing…")
        self.meeting_notes_status.setTextColor_(_color("muted", self.tokens))
        if self.meeting_status.get("session"):
            self.meeting_notes_due = time.monotonic() + 0.8
        else:
            self.meeting_notes_status.setStringValue_("Kept for when you start")

    def controlTextDidChange_(self, notification):
        # NSTextField delegate (Models fields, the meeting title, Hot Phrases
        # search/trigger/test phrase): dispatch by page and by control.
        if self.page == "Models":
            self._models_live_update(notification.object())
            return
        if self.page == "Meetings":
            if (
                notification.object() is getattr(self, "meeting_title", None)
                and not getattr(self, "meeting_title_shows_session", False)
            ):
                self.meeting_title_edited = True  # typed by the user: explicit
            return
        if self.page != "Hot Phrases":
            return
        source = notification.object()
        if source is getattr(self, "phrase_search", None):
            self._phrases_search_changed()
        elif source is getattr(self, "phrase_trigger", None):
            self._phrases_edited()
        elif source is getattr(self, "phrase_test", None):
            self._phrases_test_changed()

    def comboBoxSelectionDidChange_(self, notification):
        # NSComboBox delegate: the picked item is not in stringValue yet.
        if self.page == "Models":
            combo = notification.object()
            self._models_live_update(combo, picked=combo.objectValueOfSelectedItem())

    def meetingAction_(self, _sender):
        if self.meeting_busy:
            return
        if not self.meeting_status.get("session"):
            # Read before anything re-renders the field. Name it
            # automatically on save when it is blank (the default: only the
            # placeholder shows), or the untouched "Meeting" an older Hub
            # prefilled. Anything else is kept: a typed title (even
            # "Meeting") or a restored non-default draft.
            raw = str(self.meeting_title.stringValue())
            typed = raw.strip()
            edited = getattr(self, "meeting_title_edited", False)
            auto_title = not typed or (not edited and typed == "Meeting")
            # "Meeting" on the wire is only the server's stand-in until it
            # names the notes (auto_title says so).
            title = typed or "Meeting"
            index = self.meeting_template.indexOfSelectedItem()
            template = (
                self.meeting_templates[index]["id"]
                if 0 <= index < len(self.meeting_templates) else ""
            )
            self.meeting_busy = True
            self.meeting_error = ""
            # The draft is what the field holds (blank stays blank), never the
            # stand-in: it is what the field shows again after the meeting.
            self.meeting_title_value = raw
            self.meeting_template_value = template
            self._meeting_render()
            self._meeting_worker(
                "start", title, "", template, self.meeting_notes.string(), auto_title,
            )
            return
        self.meeting_busy = True
        self.meeting_error = ""
        self.meeting_notes_due = None
        self.meeting_action.setEnabled_(False)
        self._meeting_request_notes_save(stop_flush=True)

    def meetingRetry_(self, _sender):
        self.meeting_message.setStringValue_("Connecting…")
        self._meeting_poll()

    @objc.python_method
    def _meeting_capture_state(self):
        self.meeting_notes_text = self.meeting_notes.string()
        # While recording the field shows the session's title; the draft was
        # already set aside by _meeting_show_title and must not be replaced.
        if not self.meeting_title_shows_session:
            self.meeting_title_value = self.meeting_title.stringValue()
        selected = self.meeting_template.indexOfSelectedItem()
        if 0 <= selected < len(self.meeting_templates):
            self.meeting_template_value = self.meeting_templates[selected]["id"]

    def meetingOpenNotes_(self, _sender):
        saved = self.meeting_last_saved or {}
        path = vault_notes_path(saved.get("notes_path"), load_config().vault_path)
        if path:
            current().open_path(path)

    def meetingOpenRecent_(self, sender):
        index = sender.tag()
        if not 0 <= index < len(self.meeting_recent):
            return
        # Same guard as Open notes: only an existing .md inside the vault.
        path = vault_notes_path(
            self.meeting_recent[index].get("path"), load_config().vault_path
        )
        if path:
            current().open_path(path)

    def meetingAsk_(self, _sender):
        question = self.meeting_question.stringValue().strip()
        if not question or self.meeting_busy:
            return
        self.meeting_pending_question = question
        self.meeting_question.setStringValue_("")
        self.meeting_busy = True
        self.meeting_answers.insert(
            0, {"question": question, "answer": "Reading the transcript…"}
        )
        self._meeting_render_chat()
        self._meeting_worker("chat", question)

    @objc.python_method
    def _page_models(self):
        from localflow import provider_keys

        self.config = load_config()
        self.body.subviews()[0].setStringValue_("Models & keys")
        if not hasattr(self, "provider_state"):
            self.provider_state = {}
            self.provider_message = ("", False)
            self.provider_candidates = []
        if not hasattr(self, "provider_expanded"):
            self.provider_expanded = None  # one provider's inline editor at a time
            self.provider_checked = {}
        self.provider_controls = {}
        self.provider_checks = []
        self.provider_action_views = None
        width = self.body.frame().size.width
        statuses = {p.id: provider_keys.key_status(p.id) for p in provider_keys.PRESETS}
        # One card: the models in use are the pickers' selections, the active
        # provider is its status line, and Import keys… sits in its title row.
        self._models_quick_card(width)
        message, ok = self.provider_message
        if message:
            self._add_text(message, color="success" if ok else "red", height=20, gap=8)
        if self.provider_candidates:
            self._models_import_card(width, statuses)
        self._models_provider_list(width, statuses)
        self._models_storage_card(width)

    @objc.python_method
    def _models_storage_card(self, width):
        """Free up space: downloaded speech models, sizes, the one in use.

        Only LocalFlow's own speech catalog in the Hugging Face cache is listed
        (model_storage); nothing is loaded or downloaded to draw it.
        """
        from localflow import model_storage

        try:
            models = model_storage.inventory(
                self.config, cache=getattr(self, "storage_cache", None)
            )
        except Exception:  # noqa: BLE001 - the card says so instead of failing the page
            models = []
            self.storage_message = ("Couldn't read the model cache.", False)
        self.storage_repos = [model.repo for model in models]
        message, ok = getattr(self, "storage_message", ("", True))
        row_h = 36
        top = CARD_PADDING + 22 + 20 + 8
        rows_h = max(len(models), 1) * row_h
        height = top + rows_h + (26 if message else 0) + CARD_PADDING
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, height)
        )
        style_surface(card, self.tokens, border="ink", width=CARD_BORDER_WIDTH)
        self.body.addSubview_(card)
        self._card_label(card, "Free up space", CARD_PADDING, size=15, weight="semibold")
        self._card_label(
            card,
            "Downloaded speech models. Models in use or selected are kept; "
            "others can be downloaded again any time.",
            CARD_PADDING + 24, size=12, color="muted", height=18,
        )
        inner = width - 2 * CARD_PADDING
        if not models:
            self._card_label(
                card, "No other speech models are taking up space.", top + 6,
                size=13, color="muted",
            )
        for index, model in enumerate(models):
            y = top + index * row_h
            card.addSubview_(_label(
                model.label, CARD_PADDING, y + 6, inner - 260, 13, "ink",
                palette=self.tokens, height=22, weight="semibold",
            ))
            card.addSubview_(_label(
                model_storage.format_bytes(model.size_bytes),
                CARD_PADDING + inner - 250, y + 7, 100, 12, "muted",
                palette=self.tokens, height=20,
            ))
            if model.protected:
                # "In use" = loaded by a running process; "Selected" = the
                # saved settings could load it. Either way it can't be deleted.
                kind = ("success", "success") if model.status == "In use" else ("line", "ink")
                pill = make_pill(model.status, kind, self.tokens)
                size = pill.frame().size
                pill.setFrameOrigin_(NSMakePoint(
                    CARD_PADDING + inner - size.width, y + (row_h - size.height) / 2,
                ))
                card.addSubview_(pill)
            else:
                button = self._button(
                    "Delete…", "deleteSpeechModel:", CARD_PADDING + inner - 120, y + 2,
                    tag=index, width=120, parent=card, kind="destructive_ghost",
                    height=32,
                )
                button.setAccessibilityLabel_(f"Delete {model.label}")
        if message:
            self._card_label(
                card, message, top + rows_h + 4, size=12,
                color="success" if ok else "red", height=20,
            )
        self.cursor_y += height + 12

    def deleteSpeechModel_(self, sender):
        from localflow import model_storage

        repos = getattr(self, "storage_repos", [])
        if not 0 <= sender.tag() < len(repos):
            return
        repo = repos[sender.tag()]
        label = model_storage.catalog().get(repo, repo)
        cache = getattr(self, "storage_cache", None)
        folder = (cache or model_storage.hf_hub_cache()) / model_storage.repo_folder_name(repo)
        size = model_storage.folder_size(folder) if folder.is_dir() else 0
        alert = NSAlert.alloc().init()
        alert.setMessageText_(f"Delete {label}?")
        alert.setInformativeText_(
            f"This frees {model_storage.format_bytes(size)}. LocalFlow can download "
            "it again later."
        )
        alert.addButtonWithTitle_("Delete")
        alert.addButtonWithTitle_("Cancel")
        if alert.runModal() != AppKit.NSAlertFirstButtonReturn:
            return
        try:
            # Re-read the config (and, inside delete_unused, the live STT
            # leases) now: what is selected or loaded may have changed since
            # the list was drawn, and it must never be deleted.
            freed = model_storage.delete_unused(repo, load_config(), cache=cache)
        except model_storage.ModelStorageError as exc:
            self.storage_message = (str(exc), False)
        else:
            self.storage_message = (
                f"Deleted {label} and freed {model_storage.format_bytes(freed)}.", True
            )
        self.show_page("Models")

    # ---------------------------------------------------------- Models card ---
    # The one card at the top of Models & keys: pick the speech engine/model
    # and the active provider's cleanup & Ask model in place; the active
    # provider is shown on its own status line and Import keys… sits in the
    # title row. Every control has an explicit target and action, is enabled
    # whenever it has something to pick, and is retained in `quick_controls`
    # while shown (AppKit targets and delegates are weak). A pick saves at
    # once (Return or end of editing for a typed model ID, never per
    # keystroke); the running app and server reload the clients from the
    # saved config without a restart. Ollama's installed models are listed on
    # a worker thread and filled in place.

    @objc.python_method
    def _quick_state(self):
        if not hasattr(self, "quick_state"):
            self.quick_state = {
                "message": ("", True), "draft": None, "state": "", "models": [],
                "ollama_url": None, "token": 0, "drawn_for": None, "choices": {},
            }
        return self.quick_state

    @objc.python_method
    def _quick_fill_picker(self, picker, titles, selected):
        """Items, selection and enabled state, via the popup item API only
        (no string value, attributed title or overlay label)."""
        picker.removeAllItems()
        picker.addItemsWithTitles_(titles)
        if 0 <= selected < len(titles):
            picker.selectItemAtIndex_(selected)
        picker.setEnabled_(bool(titles))

    @objc.python_method
    def _quick_picker(self, frame, parent, titles, selected, action, label):
        """A native NSPopUpButton for a choice-only list, wired to `action`.

        Choices are unique (engine labels, sizes, installed model names), so
        an item's index is its index in the card's `choices`."""
        picker = NSPopUpButton.alloc().initWithFrame_pullsDown_(
            NSMakeRect(*frame), False
        )
        picker.setFont_(_font(13))
        self._quick_fill_picker(picker, titles, selected)
        picker.setTarget_(self)
        picker.setAction_(action)
        picker.setAccessibilityLabel_(label)
        parent.addSubview_(picker)
        return picker

    @objc.python_method
    def _models_quick_card(self, width):
        state = self._quick_state()
        self.quick_controls = {}
        config = self.config
        ollama = config.llm_provider == "ollama"
        if ollama:
            self._quick_fetch()
        provider = current_speech_provider(config)
        fixed = provider in STT_FIXED_MODEL
        items, selected, note = (
            cleanup_picker(config, state["state"] or "loading", state["models"])
            if ollama else ([], config.llm_model, "")
        )
        message, ok = state["message"]
        text = message or note
        # Sized to the note shown now; a listing result reflows it in place
        # (_quick_reflow).
        lines = self._quick_note_lines(text, width)
        frames = quick_layout(width, ollama=ollama, fixed_speech=fixed, message_lines=lines)
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, frames["height"])
        )
        self.quick_geometry = {"card": card, "width": width, "ollama": ollama, "fixed": fixed}
        style_surface(card, self.tokens)
        self.body.addSubview_(card)

        def label(text, key, size=13, token="ink", weight="regular"):
            x, y, w, h = frames[key]
            card.addSubview_(_label(
                text, x, y, w, size, token, palette=self.tokens, height=h, weight=weight,
            ))

        label("Change models", "title", 15, weight="semibold")
        x, y, w, h = frames["import"]
        self.quick_controls["import"] = self._button(
            "Import keys…", "providerImport:", x, y, width=w, parent=card, height=h,
        )
        label(
            "Saved as soon as you pick. LocalFlow switches without a restart.",
            "detail", 12, "muted",
        )
        label("Speech model", "speech_label", 12, "muted", "semibold")
        speech = speech_provider_choices(config)
        ids = [pid for pid, _title in speech]
        self.quick_controls["speech_provider"] = self._quick_picker(
            frames["speech_provider"], card, [title for _pid, title in speech],
            ids.index(provider) if provider in ids else -1, "quickSpeechProvider:",
            "Speech engine",
        )
        sizes = list(speech_model_choices(provider))
        if fixed:
            label(f"{STT_FIXED_MODEL[provider]} (fixed model)", "speech_fixed", 13)
        else:
            if config.model_size not in [size for size, _title in sizes]:
                sizes.insert(0, (config.model_size, f"{config.model_size} (current)"))
            self.quick_controls["speech_model"] = self._quick_picker(
                frames["speech_model"], card, [title for _size, title in sizes],
                [size for size, _title in sizes].index(config.model_size),
                "quickSpeechModel:", "Speech model size",
            )
        label(CLEANUP_LABEL, "cleanup_label", 12, "muted", "semibold")
        label(provider_status_text(config), "provider_status", 11, "muted")
        state["drawn_for"] = (config.llm_provider, config.llm_base_url)
        if ollama:
            self.quick_controls["cleanup_model"] = self._quick_picker(
                frames["cleanup_model"], card, items,
                items.index(selected) if selected in items else -1,
                "quickCleanupModel:", "Cleanup and Ask model",
            )
            x, y, w, h = frames["refresh"]
            refresh = self._button(
                "Refresh", "quickRefresh:", x, y, width=w, parent=card, height=h,
            )
            refresh.setEnabled_(state["state"] != "loading")
            self.quick_controls["refresh"] = refresh
        else:
            x, y, w, h = frames["cleanup_model"]
            field = self._field(
                state["draft"] if state["draft"] is not None else config.llm_model,
                x, y, w, parent=card, action="quickCleanupField:",
            )
            field.setFrame_(NSMakeRect(x, y, w, h))
            field.setPlaceholderString_("The provider's model ID")
            field.setTextColor_(_color("ink", self.tokens))
            field.setBackgroundColor_(_color("paper", self.tokens))
            field.setEnabled_(True)
            # Return and leaving the field commit; typing alone never saves.
            field.cell().setSendsActionOnEndEditing_(True)
            field.setAccessibilityLabel_("Cleanup and Ask model ID")
            self.quick_controls["cleanup_field"] = field
        state["choices"] = {"speech": ids, "sizes": [size for size, _title in sizes],
                            "cleanup": items}
        if "message" in frames:
            x, y, w, h = frames["message"]
            tone = ("success" if ok else "red") if message else "muted"
            note_label = _label(text, x, y, w, 12, tone, palette=self.tokens, height=h,
                                wrap=True)
            card.addSubview_(note_label)
            self.quick_controls["note"] = note_label
        self.cursor_y += frames["height"] + 12

    @objc.python_method
    def _quick_fetch(self, force=False):
        """List Ollama's installed models on a worker thread (never the UI)."""
        state = self._quick_state()
        url = self.config.llm_base_url
        if not force and state["ollama_url"] == url and state["state"]:
            return
        state.update(ollama_url=url, state="loading", models=[])
        state["token"] += 1
        token = state["token"]
        if getattr(self, "provider_snapshot_mode", False):
            return

        def run():
            from Foundation import NSOperationQueue

            models, result = ollama_models(url)
            NSOperationQueue.mainQueue().addOperationWithBlock_(
                lambda: self._quick_listed(token, models, result)
            )

        threading.Thread(target=run, daemon=True, name="localflow-ollama-models").start()

    @objc.python_method
    def _quick_listed(self, token, models, result):
        """Main thread: fill the open picker in place (a rebuild would drop
        a key being typed in a provider editor)."""
        state = self._quick_state()
        if self._closed or state["token"] != token:
            return
        state.update(models=models, state=result)
        controls = getattr(self, "quick_controls", {})
        picker = controls.get("cleanup_model")
        if self.page != "Models" or picker is None:
            return
        items, selected, note = cleanup_picker(self.config, result, models)
        # Same popup, same target/action: only its items, selection and
        # enabled state change (the configured model stays selected).
        self._quick_fill_picker(
            picker, items, items.index(selected) if selected in items else -1,
        )
        state["choices"]["cleanup"] = items
        if controls.get("refresh") is not None:
            controls["refresh"].setEnabled_(True)
        if not state["message"][0]:  # a save message stays as shown
            self._quick_reflow(note)

    @objc.python_method
    def _quick_note_lines(self, text, width):
        """Wrapped lines of the card's note at card `width` (0 when empty)."""
        note_font = _font(12)
        return note_lines(
            [text], width - 2 * CARD_PADDING, lambda line: measure_text(line, note_font),
        )

    @objc.python_method
    def _quick_reflow(self, text):
        """Show `text` as the card's note and size the card to it, in place.

        Same card and controls; page views below the card move by the height
        change (no rebuild, so provider editors and typed keys are kept).
        """
        geometry = getattr(self, "quick_geometry", None)
        if geometry is None:
            return
        card, width = geometry["card"], geometry["width"]
        frames = quick_layout(
            width, ollama=geometry["ollama"], fixed_speech=geometry["fixed"],
            message_lines=self._quick_note_lines(text, width),
        )
        controls = self.quick_controls
        label = controls.get("note")
        if "message" in frames:
            x, y, w, h = frames["message"]
            if label is None:
                label = _label(text, x, y, w, 12, "muted", palette=self.tokens, height=h,
                               wrap=True)
                card.addSubview_(label)
                controls["note"] = label
            else:
                label.setFrame_(NSMakeRect(x, y, w, h))
                label.setStringValue_(text)
        elif label is not None:
            label.removeFromSuperview()
            del controls["note"]
        old = card.frame()
        delta = frames["height"] - old.size.height
        if not delta:
            return
        bottom = old.origin.y + old.size.height
        card.setFrame_(NSMakeRect(old.origin.x, old.origin.y, old.size.width, frames["height"]))
        for view in self.body.subviews():
            origin = view.frame().origin
            if view is not card and origin.y >= bottom:
                view.setFrameOrigin_(NSMakePoint(origin.x, origin.y + delta))
        self.cursor_y += delta
        self._fit_body_height()

    @objc.python_method
    def _quick_live(self, sender, key):
        """True when `sender` is the control currently shown for `key`."""
        return getattr(self, "quick_controls", {}).get(key) is sender

    @objc.python_method
    def _quick_save_cleanup(self, model):
        state = self._quick_state()
        try:
            self.config = set_active_model(
                model, load_config(), expected=state["drawn_for"], save=save_config,
            )
        except ModelChoiceError as exc:
            state["message"] = (str(exc), False)
            state["draft"] = model
        except (OSError, ConfigError):
            state["message"] = (
                "Couldn't save the model; check that LocalFlow's settings can be written.",
                False,
            )
            state["draft"] = model
        else:
            state["draft"] = None
            state["message"] = (
                f"Cleanup & Ask now use {self.config.llm_model}; no restart needed.", True,
            )
            preset = active_preset(self.config)
            drafts = getattr(self, "provider_state", {})
            if preset is not None and preset.id in drafts:
                drafts[preset.id]["model"] = self.config.llm_model
                # show_page captures the open editor before rebuilding, so its
                # combo must show the saved model too or the old one wins. The
                # URL and key fields are left alone.
                controls = getattr(self, "provider_controls", {}).get(preset.id)
                if controls is not None:
                    controls[1].setStringValue_(self.config.llm_model)
        self.show_page("Models")

    @objc.python_method
    def _quick_save_speech(self, provider, model):
        state = self._quick_state()
        try:
            self.config = set_speech(provider, model, load_config(), save=save_config)
        except ModelChoiceError as exc:
            state["message"] = (str(exc), False)
        except (OSError, ConfigError):
            state["message"] = (
                (
                    "Couldn't save the speech model; check that LocalFlow's settings "
                    "can be written."
                ),
                False,
            )
        else:
            state["message"] = (
                (
                    f"Speech now uses {speech_summary(self.config)}. LocalFlow loads it "
                    "in the background (it may download first) and keeps the current "
                    "model until it is ready."
                ),
                True,
            )
        self.show_page("Models")

    def quickSpeechProvider_(self, sender):
        if not self._quick_live(sender, "speech_provider"):
            return
        ids = self._quick_state()["choices"].get("speech", [])
        index = sender.indexOfSelectedItem()
        if 0 <= index < len(ids):
            self._quick_save_speech(ids[index], None)

    def quickSpeechModel_(self, sender):
        if not self._quick_live(sender, "speech_model"):
            return
        sizes = self._quick_state()["choices"].get("sizes", [])
        index = sender.indexOfSelectedItem()
        if 0 <= index < len(sizes):
            self._quick_save_speech(current_speech_provider(self.config), sizes[index])

    def quickCleanupModel_(self, sender):
        if not self._quick_live(sender, "cleanup_model"):
            return
        items = self._quick_state()["choices"].get("cleanup", [])
        index = sender.indexOfSelectedItem()
        if 0 <= index < len(items) and items[index] != self.config.llm_model:
            self._quick_save_cleanup(items[index])

    def quickCleanupField_(self, sender):
        if not self._quick_live(sender, "cleanup_field"):
            return
        typed = str(sender.stringValue())
        if typed.strip() == (self.config.llm_model or "") and (
            self._quick_state()["draft"] is None
        ):
            return  # editing ended without a change
        self._quick_save_cleanup(typed)

    def quickRefresh_(self, sender):
        if not self._quick_live(sender, "refresh"):
            return
        self._quick_state()["message"] = ("", True)
        self._quick_fetch(force=True)
        self.show_page("Models")

    @objc.python_method
    def _models_import_card(self, width, statuses):
        """Keys found by Import: ON by default only where no key is saved yet."""
        from localflow import provider_keys

        candidates = self.provider_candidates
        card = self._dictionary_card(
            "Keys found — select keys to import", "Only checked keys will be saved.",
            import_height(len(candidates)),
        )
        ticks = import_states(
            candidates, lambda pid: statuses[pid]["has_key"], self.provider_checked,
        )
        for index, candidate in enumerate(candidates):
            check = NSButton.checkboxWithTitle_target_action_(
                f"{provider_keys.get_preset(candidate.provider_id).label}: "
                f"{candidate.masked_value} — {candidate.source}", None, None,
            )
            check.setFrame_(NSMakeRect(
                CARD_PADDING, CARD_PADDING + IMPORT_HEADER_H + index * IMPORT_ROW_H,
                width - 2 * CARD_PADDING, 24,
            ))
            check.setContentTintColor_(_color("ink", self.tokens))
            check.setAttributedTitle_(AppKit.NSAttributedString.alloc().initWithString_attributes_(
                check.title(), {AppKit.NSForegroundColorAttributeName: _color("ink", self.tokens)},
            ))
            check.setState_(
                AppKit.NSControlStateValueOn
                if ticks[(candidate.provider_id, candidate.source)]
                else AppKit.NSControlStateValueOff
            )
            card.addSubview_(check)
            self.provider_checks.append(check)
        self._button(
            "Save selected keys", "providerConfirmImport:", CARD_PADDING,
            CARD_PADDING + IMPORT_HEADER_H + len(candidates) * IMPORT_ROW_H + 8,
            width=180, parent=card, height=28,
        )

    @objc.python_method
    def _models_state(self, preset):
        initial_url, initial_model = preset.base_url, preset.default_model
        if is_active(preset, self.config):
            # The configured model seeds a new state; later drafts are kept.
            initial_model = self.config.llm_model or initial_model
            if preset.id == "custom":
                initial_url = self.config.llm_base_url
        state = self.provider_state.setdefault(preset.id, {
            "url": initial_url, "model": initial_model, "models": [],
        })
        if is_active(preset, self.config) and not state["model"]:
            state["model"] = self.config.llm_model
        return state

    @objc.python_method
    def _models_live_update(self, source, picked=None):
        """Typing / picking in the open editor's URL or model control: update
        the state and the Use / Test buttons in place. Never rebuilds, never
        reads or clears the key field, never moves focus."""
        from localflow import provider_keys

        for provider_id, (url, combo, _secure) in getattr(
            self, "provider_controls", {}
        ).items():
            if source is not url and source is not combo:
                continue
            state = self.provider_state[provider_id]
            if url is not None:
                state["url"] = url.stringValue().strip()
            state["model"] = str(
                picked if picked is not None else combo.stringValue()
            ).strip()
            self._models_apply_use(use_button_visible(
                provider_keys.get_preset(provider_id), self.config,
                url=state["url"], model=state["model"],
            ))

    @objc.python_method
    def _models_apply_use(self, show):
        views = getattr(self, "provider_action_views", None)
        if not views:
            return
        _use_frame, test_frame = action_frames(views["rows"], show)
        views["use"].setHidden_(not show)
        views["test"].setFrame_(NSMakeRect(*test_frame))

    @objc.python_method
    def _models_provider_list(self, width, statuses):
        """One compact row per preset; only the clicked provider expands."""
        from localflow import provider_keys

        presets = provider_keys.PRESETS
        states = {p.id: self._models_state(p) for p in presets}
        expanded = self.provider_expanded
        if expanded not in states:
            expanded = self.provider_expanded = None
        inner = width - 2 * CARD_PADDING
        editor_rows, editor_height = {}, 0
        if expanded is not None:
            preset = provider_keys.get_preset(expanded)
            state = states[expanded]
            message = state.get("message", ("", False))[0]
            editor_rows, editor_height = editor_layout(
                inner, custom=preset.id == "custom", needs_key=preset.needs_key,
                # The Use slot always exists in the action row; the renderer
                # hides it (and slides Test left) while the editor is active.
                show_use=True,
                message_lines=-(-len(message) // 85),
            )
        geometry = list_layout(
            width, len(presets),
            [p.id for p in presets].index(expanded) if expanded else None,
            editor_height,
        )
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, geometry["height"])
        )
        style_surface(card, self.tokens)
        card.layer().setMasksToBounds_(True)
        self.body.addSubview_(card)
        for index, preset in enumerate(presets):
            top = geometry["rows"][index]
            status = statuses[preset.id]
            state = states[preset.id]
            label, kind = provider_status(preset, self.config, status["has_key"])
            pill = make_pill(label, kind, self.tokens)
            columns = row_columns(width, pill.frame().size.width)
            if index:
                rule = NSView.alloc().initWithFrame_(
                    NSMakeRect(CARD_PADDING, top, width - 2 * CARD_PADDING, 1)
                )
                rule.setWantsLayer_(True)
                rule.layer().setBackgroundColor_(_color("line", self.tokens).CGColor())
                card.addSubview_(rule)
            text_x, text_w = columns["text"]
            shown_url = (state["url"] if preset.id == "custom" else preset.base_url)
            card.addSubview_(_label(
                preset.label, text_x, top + 6, text_w, 13, "ink", palette=self.tokens,
                height=18, weight="semibold",
            ))
            card.addSubview_(_label(
                shown_url or "Your inference endpoint", text_x, top + 26, text_w, 11,
                "muted", palette=self.tokens, height=16,
            ))
            pill.setFrameOrigin_(NSMakePoint(
                columns["pill"], top + (MODELS_ROW_HEIGHT - PILL_HEIGHT) / 2,
            ))
            card.addSubview_(pill)
            configured = (
                status["has_key"] or not preset.needs_key
                or is_active(preset, self.config)
            )
            title = "Close" if preset.id == expanded else ("Edit" if configured else "Set up")
            button_x, button_w = columns["button"]
            self._button(
                title, "providerToggle:", button_x, top + 10, tag=index,
                width=button_w, parent=card, height=28,
            )
        if expanded is not None:
            self._models_editor(
                card, expanded, states[expanded], statuses[expanded],
                editor_rows, geometry["editor_y"], width, editor_height,
            )
        self.cursor_y += geometry["height"] + 12

    @objc.python_method
    def _models_editor(self, card, provider_id, state, status, rows, top, width, height):
        """The inline editor of the expanded provider; the only one with controls,
        and the only provider whose models are fetched."""
        from localflow import provider_keys

        preset = provider_keys.get_preset(provider_id)
        index = [p.id for p in provider_keys.PRESETS].index(provider_id)
        editor = FlippedView.alloc().initWithFrame_(NSMakeRect(0, top, width, height))
        editor.setWantsLayer_(True)
        editor.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        card.addSubview_(editor)

        def frame(key):
            x, y, w, h = rows[key]
            return NSMakeRect(x, y, w, h)

        def caption(key, text):
            x, y, w, h = rows[key]
            editor.addSubview_(_label(
                text, x, y, w, 11, "muted", palette=self.tokens, height=h,
                weight="semibold",
            ))

        def style_input(control):
            control.setTextColor_(_color("ink", self.tokens))
            control.setBackgroundColor_(_color("paper", self.tokens))

        url_field = secure = None
        if "url_label" in rows:
            caption("url_label", "Base URL")
            url_field = self._field(state["url"], 0, 0, 100, parent=editor)
            url_field.setFrame_(frame("url_field"))
            style_input(url_field)
            url_field.setPlaceholderString_("https://your-provider.example/v1")
        if "key_label" in rows:
            caption("key_label", "API key · " + (status["masked_value"] or "none saved"))
            secure = AppKit.NSSecureTextField.alloc().initWithFrame_(frame("key_field"))
            secure.setPlaceholderString_("Paste API key")
            style_input(secure)
            editor.addSubview_(secure)
            for title, action, key in (
                ("Save", "providerSave:", "key_save"),
                ("Remove", "providerRemove:", "key_remove"),
            ):
                x, y, w, h = rows[key]
                button = self._button(
                    title, action, x, y, tag=index, width=w, parent=editor, height=h,
                )
                if key == "key_remove":
                    button.setEnabled_(status["has_key"])
        caption("model_label", "Model")
        combo = NSComboBox.alloc().initWithFrame_(frame("model_field"))
        combo.setEditable_(True)
        combo.addItemsWithObjectValues_(state["models"])
        combo.setStringValue_(state["model"])
        combo.setPlaceholderString_("Choose or enter a model ID")
        style_input(combo)
        editor.addSubview_(combo)
        self.provider_controls[provider_id] = (url_field, combo, secure)
        # The one primary action, always built at the shared action-row slot and
        # toggled live (see `_models_live_update`).
        x, y, w, h = rows["use"]
        use = self._button(
            "Use for cleanup & Ask", "providerActivate:", x, y, tag=index,
            width=w, parent=editor, kind="primary", height=h,
        )
        # Activation needs a saved key: Save first, then Use.
        use.setEnabled_(status["has_key"] or not preset.needs_key)
        x, y, w, h = rows["test"]
        test = self._button(
            "Test connection", "providerTest:", x, y, tag=index, width=w,
            parent=editor, height=h,
        )
        self.provider_action_views = {"use": use, "test": test, "rows": rows}
        self._models_apply_use(use_button_visible(
            preset, self.config, url=state["url"], model=state["model"]))
        for control in (url_field, combo):
            if control is not None:
                control.setDelegate_(self)
        message, ok = state.get("message", ("", False))
        if "message" in rows:
            x, y, w, h = rows["message"]
            editor.addSubview_(_label(
                message, x, y, w, 12, "success" if ok else "red", palette=self.tokens,
                height=h, wrap=True,
            ))
        cache_key = (state["url"], status["masked_value"])
        if state.get("loaded") != cache_key and (status["has_key"] or not preset.needs_key):
            state["loaded"] = cache_key
            self._models_worker(preset.id, "models")

    @objc.python_method
    def _models_capture(self):
        # The ticks belong to the candidates they were rendered for, so they
        # are consumed once; a later capture of removed views is a no-op.
        if getattr(self, "provider_checks", None):
            self.provider_checked = {
                (candidate.provider_id, candidate.source): check.state()
                for candidate, check in zip(self.provider_candidates, self.provider_checks)
            }
            self.provider_checks = []
        controls = getattr(self, "provider_controls", {})
        for provider_id, (url, combo, _secure) in controls.items():
            state = self.provider_state[provider_id]
            if url is not None:
                state["url"] = url.stringValue().strip()
            state["model"] = combo.stringValue().strip()
        # A model ID typed (not yet committed) in Change models survives the
        # rebuild, like the provider editor's drafts.
        quick = getattr(self, "quick_controls", {}).get("cleanup_field")
        if quick is not None:
            typed = str(quick.stringValue())
            if typed.strip() != (self.config.llm_model or ""):
                self._quick_state()["draft"] = typed

    @objc.python_method
    def _models_worker(self, provider_id, operation):
        from Foundation import NSOperationQueue

        from localflow import provider_keys

        state = self.provider_state[provider_id]
        url, model = state["url"], state["model"]

        def finish(result):
            if self._closed:
                return
            if operation == "models":
                state["models"] = [item["id"] for item in result]
                # Fill the open picker in place: a rebuild would drop a key the
                # user is typing. A collapsed or off-page provider just keeps
                # the list for next time.
                controls = getattr(self, "provider_controls", {}).get(provider_id)
                if self.page == "Models" and controls is not None:
                    combo = controls[1]
                    typed = combo.stringValue()
                    combo.removeAllItems()
                    combo.addItemsWithObjectValues_(state["models"])
                    combo.setStringValue_(typed)
                return
            ok, message = result
            state["message"] = (message, ok)
            if self.page == "Models":
                self.show_page("Models")

        def run():
            result = (provider_keys.fetch_models(provider_id, base_url=url)
                      if operation == "models" else
                      provider_keys.test_llm(provider_id, base_url=url, model=model))
            NSOperationQueue.mainQueue().addOperationWithBlock_(lambda: finish(result))

        if not getattr(self, "provider_snapshot_mode", False):
            threading.Thread(target=run, daemon=True, name="localflow-provider").start()

    @objc.python_method
    def _models_action(self, sender, operation):
        from localflow import provider_keys

        self._models_capture()
        preset = provider_keys.PRESETS[sender.tag()]
        state = self.provider_state[preset.id]
        try:
            if operation == "save":
                secure = self.provider_controls[preset.id][2]
                try:
                    provider_keys.save_key(preset.id, secure.stringValue())
                finally:
                    secure.setStringValue_("")
            elif operation == "remove":
                # Also revokes its activation, so Ask/cleanup stop using it.
                provider_keys.remove_key(preset.id, config=self.config)
            elif operation == "activate":
                self.config = provider_keys.activate(preset.id, base_url=state["url"],
                                                     model=state["model"], config=self.config)
            else:
                state["message"] = ("Testing connection…", True)
                self._models_worker(preset.id, "test")
                self.show_page("Models")
                return
            state["message"] = ("Provider settings saved", True)
        except Exception:  # noqa: BLE001
            state["message"] = ("Unable to save; check the key, URL, and Keychain access", False)
        self.show_page("Models")

    def providerToggle_(self, sender):
        """Set up / Edit / Close on a row: expand only that provider inline."""
        from localflow import provider_keys

        self._models_capture()  # URL and model of the open editor are kept
        picked = provider_keys.PRESETS[sender.tag()].id
        # An unsaved key is dropped with its field, never held anywhere hidden.
        for _url, _combo, secure in getattr(self, "provider_controls", {}).values():
            if secure is not None:
                secure.setStringValue_("")
        self.provider_expanded = None if self.provider_expanded == picked else picked
        self.show_page("Models")

    def providerSave_(self, sender):
        self._models_action(sender, "save")

    def providerRemove_(self, sender):
        self._models_action(sender, "remove")

    def providerActivate_(self, sender):
        self._models_action(sender, "activate")

    def providerTest_(self, sender):
        self._models_action(sender, "test")

    def providerImport_(self, _sender):
        from localflow import provider_keys

        self._models_capture()
        found = provider_keys.detect_imports()
        self.provider_checked = {}
        panel = AppKit.NSOpenPanel.openPanel()
        panel.setTitle_("Import provider keys from a .env or shell profile (Cancel: environment only)")
        panel.setCanChooseDirectories_(False)
        panel.setAllowsMultipleSelection_(False)
        panel.setShowsHiddenFiles_(True)
        try:
            if panel.runModal() == AppKit.NSModalResponseOK:
                found += provider_keys.detect_imports(panel.URL().path(), environ={})
            self.provider_candidates = found
            self.provider_message = ("Select the keys to save" if found else "No keys found", bool(found))
        except (OSError, UnicodeError):
            self.provider_candidates = found
            self.provider_message = ("Unable to read that file; environment results shown", False)
        self.show_page("Models")

    def providerConfirmImport_(self, _sender):
        from localflow import provider_keys

        try:
            provider_keys.save_imports([
                candidate for candidate, check in zip(self.provider_candidates, self.provider_checks)
                if check.state() == AppKit.NSControlStateValueOn
            ])
            self.provider_candidates = []
            self.provider_message = ("Selected keys saved", True)
        except Exception:  # noqa: BLE001
            self.provider_message = ("Unable to save keys; check Keychain access", False)
        self.show_page("Models")

    # ------------------------------------------------------ Hot Phrases page ---
    # Rendering only: HotPhrasesEditor (hot_phrases_editor) owns every rule.

    @objc.python_method
    def _phrases(self):
        if self.phrases is None:
            model = self._phrases_model or PhrasesModel(HotPhraseStore(), self.config)
            self.phrases = HotPhrasesEditor(model)
        return self.phrases

    @objc.python_method
    def _phrases_active(self):
        # Page and model only: the empty surface has no editor controls, yet
        # it must still poll, expire Undo and show errors.
        return self.page == "Hot Phrases" and self.phrases is not None

    @objc.python_method
    def _phrases_leave(self):
        """Navigation guard: True when the editor may be left."""
        if self.phrases is None or self.page != "Hot Phrases":
            return True
        self._phrases_pull()
        return self.phrases.leave()

    @objc.python_method
    def _phrases_tick(self):
        if self._closed or not self._phrases_active():
            return
        if self.phrases.tick():
            self._phrases_sync()

    @objc.python_method
    def _phrases_pull(self):
        """Copy typed values from the controls into the editor state."""
        editor = self.phrases
        if (
            editor is None
            or editor.fields_stale
            or self._phrases_empty_shown
            or not hasattr(self, "phrase_trigger")
        ):
            return
        trigger = str(self.phrase_trigger.stringValue())
        text = str(self.phrase_text.string())
        enabled = self.phrase_switch.state() == AppKit.NSControlStateValueOn
        if (trigger, text, enabled) != (editor.trigger, editor.text, editor.enabled):
            editor.edit(trigger, text, enabled)

    @objc.python_method
    def _phrases_text_area(self, frame, *, editable):
        scroll = NSScrollView.alloc().initWithFrame_(frame)
        scroll.setHasVerticalScroller_(True)
        scroll.setAutohidesScrollers_(False)
        scroll.setBorderType_(AppKit.NSBezelBorder)
        scroll.setDrawsBackground_(True)
        scroll.setBackgroundColor_(_color("surface", self.tokens))
        size = scroll.contentSize()
        # PlaceholderTextView only adds a drawn (never inserted) placeholder;
        # with none set it behaves exactly like NSTextView.
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
        view.setFont_(_font(13))
        view.setTextColor_(_color("ink", self.tokens))
        view.setBackgroundColor_(_color("surface", self.tokens))
        view.setInsertionPointColor_(_color("ink", self.tokens))
        view.setRichText_(False)
        view.setEditable_(editable)
        view.setSelectable_(True)
        # Saved text is pasted verbatim: no smart quotes, dashes or rewrites.
        view.setAutomaticQuoteSubstitutionEnabled_(False)
        view.setAutomaticDashSubstitutionEnabled_(False)
        view.setAutomaticTextReplacementEnabled_(False)
        view.setAutomaticSpellingCorrectionEnabled_(False)
        if editable:
            view.setAllowsUndo_(True)
            view.setDelegate_(self)
        scroll.setDocumentView_(view)
        return scroll, view

    @objc.python_method
    def _phrases_build_empty(self, width):
        # Error and Undo take room only while visible, so the card stays
        # compact; `_phrases_relayout_empty` resizes it in place when they change.
        self._phrases_vis = self.phrases.notice_visibility
        rows, height = empty_layout(
            width, error_visible=self._phrases_vis[0], undo_visible=self._phrases_vis[1],
        )
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, height)
        )
        style_surface(card, self.tokens)
        self.body.addSubview_(card)
        self.phrase_empty_card = card
        inner = width - 2 * CARD_PADDING
        card.addSubview_(
            _label(
                EMPTY_MESSAGE, CARD_PADDING, CARD_PADDING + 8, inner, 15, "ink",
                palette=self.tokens, height=48, weight="semibold", wrap=True,
            )
        )
        card.addSubview_(
            _label(
                "Hot phrases swap a short spoken trigger for text you saved, "
                "right as you dictate.",
                CARD_PADDING, CARD_PADDING + 62, inner, 12, "muted",
                palette=self.tokens, height=36, wrap=True,
            )
        )
        self._button(
            "+ New phrase", "phrasesNew:", CARD_PADDING, 124, width=150,
            parent=card, kind="primary",
        )
        self._button(
            "Add an example", "phrasesAddExample:", CARD_PADDING + 162, 124,
            width=160, parent=card,
        )
        self.phrase_empty_error = _label(
            "", CARD_PADDING, 168, inner, 12, "red", palette=self.tokens,
            height=34, wrap=True,
        )
        card.addSubview_(self.phrase_empty_error)
        self.phrase_empty_undo_bar = FlippedView.alloc().initWithFrame_(
            NSMakeRect(CARD_PADDING, 208, inner, 36)
        )
        style_surface(self.phrase_empty_undo_bar, self.tokens, fill="paper")
        self.phrase_empty_undo_label = _label(
            "", 12, 9, max(inner - 96, 40), 12, "ink", palette=self.tokens, height=18,
        )
        self.phrase_empty_undo_bar.addSubview_(self.phrase_empty_undo_label)
        self._button(
            "Undo", "phrasesUndo:", max(inner - 84, 0), 0, width=76,
            parent=self.phrase_empty_undo_bar,
        )
        card.addSubview_(self.phrase_empty_undo_bar)
        self._phrases_apply_empty_layout(rows, height)
        self.cursor_y += height + 12
        self._phrases_empty_shown = True
        self._phrases_render_empty()

    @objc.python_method
    def _phrases_apply_empty_layout(self, rows, height):
        card = self.phrase_empty_card
        frame = card.frame()
        card.setFrame_(NSMakeRect(frame.origin.x, frame.origin.y, frame.size.width, height))
        for key, view in (
            ("error", self.phrase_empty_error), ("undo", self.phrase_empty_undo_bar),
        ):
            if key in rows:
                x, y, w, h = rows[key]
                view.setFrame_(NSMakeRect(x, y, w, h))
            view.setHidden_(key not in rows)

    @objc.python_method
    def _phrases_relayout_empty(self):
        """Resize the empty card only when its notices appeared or went away."""
        vis = self.phrases.notice_visibility
        if vis == self._phrases_vis:
            return
        self._phrases_vis = vis
        rows, height = empty_layout(
            self.phrase_empty_card.frame().size.width,
            error_visible=vis[0], undo_visible=vis[1],
        )
        self._phrases_apply_empty_layout(rows, height)
        self._fit_body_height()

    @objc.python_method
    def _page_hot_phrases(self):
        editor = self._phrases()
        width = self.body.frame().size.width
        if editor.banner:
            self.body.addSubview_(
                _label(
                    editor.banner, 0, self.cursor_y, width, 12, "muted",
                    palette=self.tokens, height=20,
                )
            )
            self.cursor_y += 28
        if editor.empty_state_visible:
            self._phrases_build_empty(width)
            return
        self._phrases_empty_shown = False
        top = self.cursor_y
        self._phrases_vis = editor.notice_visibility
        self.phrase_top, self.phrase_width = top, width
        frames = hot_phrases_layout(
            width, self._viewport_height() - top - HUB_PAGE_BOTTOM,
            error_visible=self._phrases_vis[0], undo_visible=self._phrases_vis[1],
        )

        def rect(frame, dx=0, dy=0):
            x, y, w, h = frame
            return NSMakeRect(x + dx, y + dy, w, h)

        # Left column: search, new phrase, scrollable list.
        self.phrase_search = NSSearchField.alloc().initWithFrame_(
            rect(frames["search"], 0, top)
        )
        self.phrase_search.setPlaceholderString_("Search phrases")
        self.phrase_search.setStringValue_(editor.query)
        self.phrase_search.setFont_(_font(13))
        self.phrase_search.setSendsSearchStringImmediately_(True)
        self.phrase_search.setTarget_(self)
        self.phrase_search.setAction_("phrasesSearch:")
        self.phrase_search.setDelegate_(self)
        self.phrase_search.setAccessibilityLabel_("Search hot phrases")
        self.body.addSubview_(self.phrase_search)
        new_frame = frames["new"]
        self._button(
            "+ New phrase", "phrasesNew:", new_frame[0], top + new_frame[1],
            width=new_frame[2], kind="primary",
        )
        self.phrase_list_scroll = NSScrollView.alloc().initWithFrame_(
            rect(frames["list"], 0, top)
        )
        self.phrase_list_scroll.setHasVerticalScroller_(True)
        self.phrase_list_scroll.setAutohidesScrollers_(False)
        self.phrase_list_scroll.setBorderType_(AppKit.NSNoBorder)
        self.phrase_list_scroll.setDrawsBackground_(False)
        self.phrase_list_scroll.setAccessibilityLabel_("Hot phrases")
        self.phrase_list = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, frames["list"][2], frames["list"][3])
        )
        self.phrase_list_scroll.setDocumentView_(self.phrase_list)
        self.body.addSubview_(self.phrase_list_scroll)

        # Right: the editor card.
        card_frame = frames["card"]
        card = FlippedView.alloc().initWithFrame_(rect(card_frame, 0, top))
        style_surface(card, self.tokens)
        self.body.addSubview_(card)
        rows = dict(frames["card_rows"])
        # A hidden notice has no slot in the layout; it still needs a frame so
        # it can be shown in place later (see `_phrases_relayout`).
        for key, height in (("error_label", 34), ("undo_bar", 36)):
            rows.setdefault(key, (CARD_PADDING, 0, rows["trigger_field"][2], height))

        def add_label(text, key, size=12, color="ink", weight="semibold", wrap=False):
            frame = rows[key]
            label = _label(
                text, frame[0], frame[1], frame[2], size, color,
                palette=self.tokens, height=frame[3], weight=weight, wrap=wrap,
            )
            card.addSubview_(label)
            self.phrase_views[key] = label
            return label

        self.phrase_views = {}
        self.phrase_card = card
        self.phrase_trigger_label = add_label("When I say", "trigger_label")
        self.phrase_trigger = self._field(
            "", rows["trigger_field"][0], rows["trigger_field"][1],
            rows["trigger_field"][2], parent=card,
        )
        self.phrase_views["trigger_field"] = self.phrase_trigger
        self.phrase_trigger.setDelegate_(self)
        self.phrase_trigger.setPlaceholderString_("e.g. review checklist")
        self.phrase_trigger_hint = add_label(
            "", "trigger_hint", 11, "muted", "regular"
        )
        add_label("Type this", "text_label")
        self.phrase_text_scroll, self.phrase_text = self._phrases_text_area(
            rect(rows["text_scroll"]), editable=True
        )
        self.phrase_text.setAccessibilityLabel_("Text to type")
        card.addSubview_(self.phrase_text_scroll)
        self.phrase_views["text_scroll"] = self.phrase_text_scroll
        self.phrase_text_hint = add_label("", "text_hint", 11, "muted", "regular")
        self.phrase_switch = NSSwitch.alloc().initWithFrame_(rect(rows["enabled_switch"]))
        self.phrase_views["enabled_switch"] = self.phrase_switch
        self.phrase_switch.setTarget_(self)
        self.phrase_switch.setAction_("phrasesToggle:")
        # The switch's own value says on/off; its name stays meaningful.
        self.phrase_switch.setAccessibilityLabel_(ENABLED_ACCESSIBILITY)
        card.addSubview_(self.phrase_switch)
        # "On" / "Off" follows the switch (set in _phrases_render_enabled).
        self.phrase_enabled_label = add_label(
            enabled_label(editor.enabled), "enabled_label", 13, "ink", "regular"
        )
        # Short status ("Saved ✓") sits inline here and never moves the page.
        self.phrase_notice = add_label("", "enabled_notice", 12, "success", "regular")
        self.phrase_delete = self._button(
            "Delete", "phrasesDelete:", rows["delete_button"][0],
            rows["delete_button"][1], width=rows["delete_button"][2],
            parent=card, kind="destructive_ghost",
        )
        self.phrase_views["delete_button"] = self.phrase_delete
        self.phrase_error_label = add_label(
            "", "error_label", 12, "red", "regular", wrap=True
        )
        undo_frame = rows["undo_bar"]
        self.phrase_undo_bar = FlippedView.alloc().initWithFrame_(rect(undo_frame))
        style_surface(self.phrase_undo_bar, self.tokens, fill="paper")
        self.phrase_undo_label = _label(
            "", 12, 9, undo_frame[2] - 96, 12, "ink",
            palette=self.tokens, height=18,
        )
        self.phrase_undo_bar.addSubview_(self.phrase_undo_label)
        self._button(
            "Undo", "phrasesUndo:", undo_frame[2] - 84, 0, width=76,
            parent=self.phrase_undo_bar,
        )
        card.addSubview_(self.phrase_undo_bar)
        self.phrase_views["undo_bar"] = self.phrase_undo_bar
        add_label("Try it: say a phrase", "preview_label")
        self.phrase_test = self._field(
            editor.test_input, rows["test_field"][0], rows["test_field"][1],
            rows["test_field"][2], parent=card,
        )
        self.phrase_views["test_field"] = self.phrase_test
        self.phrase_test.setDelegate_(self)
        self.phrase_test.setPlaceholderString_("e.g. please review checklist now")
        self.phrase_preview_scroll, self.phrase_preview = self._phrases_text_area(
            rect(rows["preview_output"]), editable=False
        )
        self.phrase_preview.setAccessibilityLabel_("Dictation preview")
        card.addSubview_(self.phrase_preview_scroll)
        self.phrase_views["preview_output"] = self.phrase_preview_scroll
        self.phrase_status_label = add_label(
            "", "preview_status", 11, "muted", "regular"
        )
        self.phrase_views["error_label"] = self.phrase_error_label
        self.phrase_error_label.setHidden_(not self._phrases_vis[0])
        self.phrase_undo_bar.setHidden_(not self._phrases_vis[1])
        self.cursor_y = top + frames["height"]
        self._phrases_push_fields()
        self._phrases_sync()

    @objc.python_method
    def _phrases_push_fields(self):
        """Programmatic load of the editor values into the controls."""
        editor = self.phrases
        self.phrase_trigger.setStringValue_(editor.trigger)
        self.phrase_text.setString_(editor.text)
        self.phrase_switch.setState_(
            AppKit.NSControlStateValueOn if editor.enabled
            else AppKit.NSControlStateValueOff
        )
        self._phrases_render_enabled()
        editor.ack_fields()

    @objc.python_method
    def _phrases_render_enabled(self):
        """The label beside the switch says what the switch is: On or Off."""
        self.phrase_enabled_label.setStringValue_(enabled_label(self.phrases.enabled))

    @objc.python_method
    def _phrases_sync(self):
        """Bring every control in line with the editor state."""
        if not self._phrases_active():
            return
        editor = self.phrases
        if editor.empty_state_visible != self._phrases_empty_shown:
            self._rebuild_current_page()
            return
        if editor.empty_state_visible:
            self._phrases_render_empty()
            return
        if editor.fields_stale:
            self._phrases_push_fields()
        self._phrases_render_list()
        self._phrases_render_status()

    @objc.python_method
    def _phrases_render_empty(self):
        """Error and Undo on the empty surface (e.g. after deleting the last
        phrase, or a failed load / add-example)."""
        editor = self.phrases
        self._phrases_relayout_empty()
        self.phrase_empty_error.setStringValue_(editor.error)
        self.phrase_empty_undo_bar.setHidden_(editor.undo_token is None)
        self.phrase_empty_undo_label.setStringValue_(editor.undo_label)

    @objc.python_method
    def _phrases_render_list(self):
        editor = self.phrases
        for view in list(self.phrase_list.subviews()):
            view.removeFromSuperview()
        width = self.phrase_list_scroll.contentSize().width
        rows = editor.rows()
        row_height, gap = 54, 4
        if not rows:
            message = "No matches" if editor.no_matches else "No phrases yet"
            self.phrase_list.addSubview_(
                _label(
                    message, 8, 8, max(width - 16, 40), 12, "muted",
                    palette=self.tokens, height=20,
                )
            )
        for index, row in enumerate(rows):
            holder = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, index * (row_height + gap), width, row_height)
            )
            holder.setWantsLayer_(True)
            holder.layer().setCornerRadius_(8)
            if row["selected"]:
                holder.layer().setBackgroundColor_(
                    _color(surface_tint("cobalt", self.tokens, 0.16)).CGColor()
                )
            dot = NSView.alloc().initWithFrame_(NSMakeRect(10, 21, 10, 10))
            dot.setWantsLayer_(True)
            dot.layer().setCornerRadius_(5)
            if row["enabled"]:
                dot.layer().setBackgroundColor_(_color("success", self.tokens).CGColor())
            else:
                dot.layer().setBorderWidth_(1.5)
                dot.layer().setBorderColor_(_color("muted", self.tokens).CGColor())
            holder.addSubview_(dot)
            text_width = max(width - 38, 40)
            holder.addSubview_(
                _label(
                    row["title"], 28, 8, text_width, 13, "ink",
                    palette=self.tokens, height=18, weight="semibold",
                )
            )
            holder.addSubview_(
                _label(
                    row["subtitle"], 28, 28, text_width, 11, "muted",
                    palette=self.tokens, height=16,
                )
            )
            hit = NSButton.alloc().initWithFrame_(NSMakeRect(0, 0, width, row_height))
            hit.setTitle_("")
            hit.setBordered_(False)
            hit.setTag_(index)
            hit.setTarget_(self)
            hit.setAction_("phrasesSelect:")
            hit.setAccessibilityLabel_(row["accessibility"])
            holder.addSubview_(hit)
            self.phrase_list.addSubview_(holder)
        total = max(
            len(rows) * (row_height + gap), self.phrase_list_scroll.contentSize().height
        )
        self.phrase_list.setFrame_(NSMakeRect(0, 0, width, total))

    @objc.python_method
    def _phrases_render_status(self):
        editor = self.phrases
        message, is_error = editor.trigger_hint()
        self.phrase_trigger_hint.setStringValue_(message)
        self.phrase_trigger_hint.setTextColor_(
            _color("red" if is_error else "muted", self.tokens)
        )
        message, is_error = editor.text_hint()
        self.phrase_text_hint.setStringValue_(message)
        self.phrase_text_hint.setTextColor_(
            _color("red" if is_error else "muted", self.tokens)
        )
        self._phrases_relayout()
        self.phrase_error_label.setStringValue_(editor.error)
        self.phrase_notice.setStringValue_("" if editor.error else editor.notice)
        self.phrase_undo_bar.setHidden_(editor.undo_token is None)
        self.phrase_undo_label.setStringValue_(editor.undo_label)
        self.phrase_delete.setEnabled_(
            editor.current_id is not None or not editor.blank()
        )
        self._phrases_render_enabled()
        self._phrases_render_preview()

    @objc.python_method
    def _phrases_relayout(self):
        """Move the editor's controls in place when the error or Undo notice
        appeared or went away. Nothing is rebuilt, so typing, the selection and
        the focus carry on untouched."""
        vis = self.phrases.notice_visibility
        if vis == self._phrases_vis:
            return
        self._phrases_vis = vis
        frames = hot_phrases_layout(
            self.phrase_width,
            self._viewport_height() - self.phrase_top - HUB_PAGE_BOTTOM,
            error_visible=vis[0], undo_visible=vis[1],
        )
        card_x, _card_y, card_w, card_h = frames["card"]
        self.phrase_card.setFrame_(NSMakeRect(card_x, self.phrase_top, card_w, card_h))
        list_x, list_y, list_w, list_h = frames["list"]
        self.phrase_list_scroll.setFrame_(
            NSMakeRect(list_x, self.phrase_top + list_y, list_w, list_h)
        )
        rows = frames["card_rows"]
        for key, view in self.phrase_views.items():
            if key in rows:
                x, y, w, h = rows[key]
                view.setFrame_(NSMakeRect(x, y, w, h))
            if key in ("error_label", "undo_bar"):
                view.setHidden_(key not in rows)
        for scroll, text_view in (
            (self.phrase_text_scroll, self.phrase_text),
            (self.phrase_preview_scroll, self.phrase_preview),
        ):
            # The text view must stay at least as tall as its scroll view.
            text_view.setMinSize_(NSMakeSize(0, scroll.contentSize().height))
        self._phrases_render_list()
        self._fit_body_height()
        self.cursor_y = self.phrase_top + frames["height"]

    @objc.python_method
    def _phrases_render_preview(self):
        output, status = self.phrases.preview()
        # Real output is shown unchanged; an empty box gets a drawn, muted
        # placeholder that is never part of the preview's string().
        self.phrase_preview.setPlaceholder_color_(
            preview_placeholder(self.phrases.test_input, output, status),
            _color("muted", self.tokens),
        )
        self.phrase_preview.setString_(output)
        self.phrase_status_label.setStringValue_(status)

    @objc.python_method
    def _phrases_edited(self):
        """The trigger field or the text view changed (user typing)."""
        if not self._phrases_active():
            return
        self.phrases.edit(
            trigger=str(self.phrase_trigger.stringValue()),
            text=str(self.phrase_text.string()),
        )
        self._phrases_render_status()

    @objc.python_method
    def _phrases_search_changed(self):
        if not self._phrases_active():
            return
        self.phrases.set_query(str(self.phrase_search.stringValue()))
        self._phrases_render_list()

    @objc.python_method
    def _phrases_test_changed(self):
        if not self._phrases_active():
            return
        self.phrases.set_test_input(str(self.phrase_test.stringValue()))
        self._phrases_render_preview()

    @objc.python_method
    def _phrases_responder_key(self):
        responder = self.window.firstResponder()
        if isinstance(responder, NSTextView) and responder.isFieldEditor():
            responder = responder.delegate()
        for key, name in (
            ("search", "phrase_search"), ("trigger", "phrase_trigger"),
            ("text", "phrase_text"), ("test", "phrase_test"),
        ):
            if responder is getattr(self, name, None):
                return key
        return None

    @objc.python_method
    def _phrases_list_scroll_y(self):
        scroll = getattr(self, "phrase_list_scroll", None)
        return scroll.contentView().bounds().origin.y if scroll is not None else 0

    @objc.python_method
    def _phrases_restore_view(self, responder, scroll_y):
        if not hasattr(self, "phrase_list_scroll"):
            return
        limit = max(
            0, self.phrase_list.frame().size.height
            - self.phrase_list_scroll.contentSize().height,
        )
        self.phrase_list.scrollPoint_(NSMakePoint(0, min(scroll_y, limit)))
        target = {
            "search": "phrase_search", "trigger": "phrase_trigger",
            "text": "phrase_text", "test": "phrase_test",
        }.get(responder)
        if target and hasattr(self, target):
            self.window.makeFirstResponder_(getattr(self, target))

    def phrasesSearch_(self, _sender):
        self._phrases_search_changed()

    def phrasesNew_(self, _sender):
        editor = self._phrases()
        self._phrases_pull()
        editor.new_phrase()
        self._phrases_sync()
        if hasattr(self, "phrase_trigger"):
            self.window.makeFirstResponder_(self.phrase_trigger)

    def phrasesAddExample_(self, _sender):
        self._phrases().add_example()
        self._phrases_sync()

    def phrasesSelect_(self, sender):
        rows = self.phrases.entries
        index = sender.tag()
        if 0 <= index < len(rows):
            self._phrases_pull()
            self.phrases.select(rows[index]["id"])
        self._phrases_sync()

    def phrasesToggle_(self, _sender):
        self._phrases_pull()
        self.phrases.set_enabled(
            self.phrase_switch.state() == AppKit.NSControlStateValueOn
        )
        self._phrases_sync()

    def phrasesDelete_(self, _sender):
        self.phrases.delete()
        self._phrases_sync()

    def phrasesUndo_(self, _sender):
        self.phrases.undo_delete()
        self._phrases_sync()

    @objc.python_method
    def _prompt_entries(self):
        """Queue entries, or (None, friendly error) when the file is unreadable."""
        try:
            return self.prompt_queue.list(), ""
        except Exception:  # noqa: BLE001
            return None, PROMPTS_UNREADABLE

    @objc.python_method
    def _page_prompts(self):
        width = self.body.frame().size.width
        inner = width - 2 * CARD_PADDING
        error = self.page_errors.get("Prompts")
        if error:
            self.body.addSubview_(
                _label(
                    error, 0, self.cursor_y, width, 12, "red",
                    palette=self.tokens, height=22,
                )
            )
            self.cursor_y += 30

        # Section 1: prompt shortcuts, read from the current config.
        shortcuts = shortcut_rows(self.config)
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, 76 + len(shortcuts) * 70)
        )
        style_surface(card, self.tokens)
        self.body.addSubview_(card)
        self._card_label(card, "Prompt shortcuts", CARD_PADDING, size=15, weight="semibold")
        self._card_label(
            card, "Select rough text in any app, then press a shortcut.",
            CARD_PADDING + 24, size=12, color="muted", height=18,
        )
        for index, row in enumerate(shortcuts):
            y = 64 + index * 70
            # The hotkey pill shares the name row; the explanation gets the
            # full card width below it and wraps instead of truncating.
            pill = make_pill(row["hotkey"], ("line", "ink"), self.tokens)
            pill_width = pill.frame().size.width
            pill.setFrameOrigin_(NSMakePoint(width - CARD_PADDING - pill_width, y))
            card.addSubview_(pill)
            card.addSubview_(
                _label(
                    row["name"], CARD_PADDING, y + 2, inner - pill_width - 12, 13,
                    "ink", palette=self.tokens, height=20, weight="semibold",
                )
            )
            card.addSubview_(
                _label(
                    row["detail"], CARD_PADDING, y + 28, inner, 12, "muted",
                    palette=self.tokens, height=34, wrap=True,
                )
            )
        self.cursor_y += 76 + len(shortcuts) * 70 + 12

        # Section 2: meeting work prompts awaiting approval.
        self._add_text("Meeting work prompts", size=15, weight="semibold", height=22, gap=8)
        entries, unreadable = self._prompt_entries()
        rows = prompt_rows(entries or [])
        self.prompt_ids = {}
        if unreadable:
            self._empty_state(
                self.body, 0, self.cursor_y, width, 190,
                "Can't read the queue", unreadable, "Try again", "reloadPrompts:",
            )
            self.cursor_y += 202
        elif not rows:
            self._empty_state(
                self.body, 0, self.cursor_y, width, 190,
                "No prompts to review yet", PROMPTS_EMPTY, "Open Meetings",
                "openMeetings:",
            )
            self.cursor_y += 202
        else:
            listing = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, self.cursor_y, width, len(rows) * 68)
            )
            style_surface(listing, self.tokens)
            self.body.addSubview_(listing)
            # Rows grow with their wrapped context line (a failure sentence is
            # never cut off), so each row's top is the sum of those above.
            y = 0
            for index, row in enumerate(rows):
                height = self._prompt_row(listing, row, index, width, y)
                y += height
                if index < len(rows) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, y - 1, width - 24, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    listing.addSubview_(divider)
            listing.setFrame_(NSMakeRect(0, self.cursor_y, width, y))
            self.cursor_y += y + 12

        # Section 3: teaser linking to the Hot Phrases page.
        teaser = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, 64)
        )
        style_surface(teaser, self.tokens)
        self.body.addSubview_(teaser)
        # The label stops before the button instead of running behind it.
        teaser.addSubview_(
            _label(
                "Say a word, get a saved prompt", CARD_PADDING, 18,
                width - 3 * CARD_PADDING - 160, 13, "ink",
                palette=self.tokens, height=28, weight="semibold",
            )
        )
        self._button(
            "See Hot Phrases", "showHotPhrases:", width - 160 - CARD_PADDING, 14,
            width=160, parent=teaser,
        )
        self.cursor_y += 76

    @objc.python_method
    def _prompt_row(self, listing, row, index, width, y):
        """One queue row at `y`; returns its height (see prompt_row_layout)."""
        self.prompt_ids[index] = row["id"]
        pill = make_pill(
            row["status_label"], PROMPT_STATUS_PILLS.get(row["status"], ("line", "muted")),
            self.tokens,
        )
        pill_width = pill.frame().size.width
        context_font = _font(11)
        geometry = prompt_row_layout(
            width, row, pill_width, lambda text: measure_text(text, context_font),
        )
        buttons = []
        if row["can_approve"]:
            buttons.append(("Approve", "approvePrompt:", 88, "primary"))
        if row["can_reject"]:
            buttons.append(("Reject", "rejectPrompt:", 80, "destructive_ghost"))
        x = geometry["action_x"]
        for title, action, button_width, kind in buttons:
            self._button(
                title, action, x, y + 16, index, button_width,
                parent=listing, kind=kind,
            )
            x += button_width + 8
        pill.setFrameOrigin_(NSMakePoint(geometry["pill_x"], y + 22))
        listing.addSubview_(pill)
        text_width = geometry["text_width"]
        listing.addSubview_(
            _label(
                row["title"], 14, y + 12, text_width, 13, "ink",
                palette=self.tokens, height=20, weight="semibold",
            )
        )
        listing.addSubview_(
            _label(
                row["context"], 14, y + 36, text_width, 11,
                "red" if row.get("error") else "muted",
                palette=self.tokens, height=geometry["context_h"], wrap=True,
            )
        )
        return geometry["height"]

    @objc.python_method
    def _review_prompt(self, sender, action):
        entry_id = self.prompt_ids.get(sender.tag(), "")
        # Same guarded compare-and-set the server API uses (localflow.dispatch).
        error = review_prompt(self.prompt_queue, entry_id, action)
        if error:
            self.page_errors["Prompts"] = error
        else:
            self.page_errors.pop("Prompts", None)
        self.show_page("Prompts")

    def approvePrompt_(self, sender):
        self._review_prompt(sender, "approve")

    def rejectPrompt_(self, sender):
        self._review_prompt(sender, "reject")

    def reloadPrompts_(self, _sender):
        self.page_errors.pop("Prompts", None)
        self.show_page("Prompts")

    def openMeetings_(self, _sender):
        self.show_page("Meetings")

    def focusWord_(self, _sender):
        self.window.makeFirstResponder_(self.word_field)

    def openSetup_(self, _sender):
        subprocess.Popen([sys.executable, "-m", "localflow.setup_window"])


def build_window(
    history=None, dictionary=None, config=None, meeting_client=None,
    initial_page="Home", prompt_queue=None, phrase_store=None,
    phrases_model=None, perf_summary=None,
):
    """`perf_summary()` feeds the Home Speed card. Injected stores (snapshots,
    tests) show no card unless one is passed; the real Hub reads the ledger."""
    config = config or load_config()
    if phrases_model is None and phrase_store is not None:
        phrases_model = PhrasesModel(phrase_store, config)
    if perf_summary is None and history is None:
        # `perf_metrics` is checked against the live config on every render.
        perf_summary = load_perf_summary
    controller = HubWindowController.alloc().initWithHistory_dictionary_config_meetingClient_promptQueue_phrasesModel_initialPage_(
        history or HistoryStore(enabled=config.history_enabled),
        dictionary or DictionaryStore(),
        config,
        meeting_client,
        prompt_queue,
        phrases_model,
        initial_page,
    )
    if perf_summary is not None:
        controller.perf_summary = perf_summary
        if controller.page == "Home":
            controller.show_page("Home")
    return controller


def main(initial_page="Home"):
    app = NSApplication.sharedApplication()
    controller = build_window(initial_page=initial_page)
    controller.window.makeKeyAndOrderFront_(None)
    app.activateIgnoringOtherApps_(True)
    from PyObjCTools import AppHelper
    AppHelper.runEventLoop()


if __name__ == "__main__":
    main()
