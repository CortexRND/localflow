"""Native AppKit history and settings hub for macOS."""

import queue
import subprocess
import sys
import threading
import time
from datetime import datetime

import AppKit
import objc
from AppKit import (
    NSAlert,
    NSApplication,
    NSButton,
    NSComboBox,
    NSMakeRect,
    NSScrollView,
    NSTextField,
    NSTextView,
    NSView,
    NSWindow,
)
from Foundation import NSMakePoint, NSMakeSize, NSObject, NSTimer

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryStore
from localflow.dispatch import PromptQueue
from localflow.history import HistoryStore
from localflow.hub_model import (
    PROMPT_STATUS_PILLS,
    PROMPTS_EMPTY,
    PROMPTS_UNREADABLE,
    dictionary_entries,
    meeting_notes_height,
    prompt_rows,
    remove_entry,
    review_prompt,
    shortcut_rows,
    stat_layout,
    submit_replacement,
    submit_word,
)
from localflow.mac_style import (
    FlippedView,
    HalftoneView,
    make_pill,
    style_button,
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
from localflow.platform import current
from localflow.ui_tokens import (
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
    SUN_TEXT,
)
from localflow.ui_tokens import (
    TOKENS as TOKENS,  # noqa: PLC0414 - kept as a public re-export
)

PAGES = (
    "Home", "Hot Phrases", "Dictionary", "Prompts",
    "Meetings", "History", "Style", "Models",
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
        self = super().init()  # noqa: PLW0642
        if self is None:
            return None
        self.history = history
        self.dictionary = dictionary
        self.config = config
        self.meeting_client = meeting_client or MeetingClient()
        # Constructing a PromptQueue touches no disk; it is only read on Prompts.
        self.prompt_queue = prompt_queue if prompt_queue is not None else PromptQueue()
        self.meeting_events = queue.Queue()
        self.meeting_status = {}
        self.meeting_templates = []
        self.meeting_last_saved = None
        self.meeting_answers = []
        self.meeting_notes_text = ""
        self.meeting_title_value = "Meeting"
        self.meeting_template_value = ""
        self.meeting_notes_dirty = False
        self.meeting_notes_due = None
        self.meeting_notes_save_in_flight = False
        self.meeting_notes_save_pending = False
        self.meeting_stop_flush_pending = False
        self.meeting_active_notes_text = ""
        self.meeting_busy = False
        self.meeting_error = ""
        self.meeting_next_poll = 0
        self.meeting_session_active = False
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
        for index, page in enumerate(PAGES):
            button = NSButton.buttonWithTitle_target_action_(page, self, "showPage:")
            button.setFrame_(NSMakeRect(16, 76 + index * 44, 168, 36))
            button.setTitle_(f"\u2002\u2002{page}")
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
        kind="secondary",
    ):
        button = NSButton.buttonWithTitle_target_action_(title, self, action)
        button.setFrame_(NSMakeRect(x, y, width, 36))
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
        if self.page == "Meetings" and hasattr(self, "meeting_notes"):
            self._meeting_capture_state()
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
        self.body.addSubview_(
            _label(page, 0, self.cursor_y, width, 20, "ink", palette=self.tokens, height=28)
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

    def windowDidEndLiveResize_(self, _notification):
        self._rebuild_current_page()

    @objc.python_method
    def _preserve_page_state(self):
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
        if state[0] == "history":
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
        self.show_page(page)

    def showHotPhrases_(self, _sender):
        self.show_page("Hot Phrases")

    @objc.python_method
    def _page_home(self):
        stats = self.history.stats()
        width = self.body.frame().size.width
        self.body.addSubview_(
            HalftoneView.alloc().initWithFrame_palette_(
                NSMakeRect(width - 150, 0, 150, 116), self.tokens
            )
        )
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
            self.body.addSubview_(
                _label(
                    f"{category.title()} style", 0, y + 4, 160, 15,
                    "ink", palette=self.tokens, height=22, weight="semibold",
                )
            )
            self.body.addSubview_(
                _label(example, 0, y + 29, 160, 11, "muted", palette=self.tokens, height=18)
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
        self._button(
            "Save style", "saveStyle:", 0, self.cursor_y,
            width=150, kind="primary",
        )

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
        self._update_segment_buttons(
            self.cleanup_group, self.cleanup_buttons, sender.tag()
        )

    def selectStyle_(self, sender):
        category, value = self.style_tag_map[sender.tag()]
        self.style_selections[category] = value
        selected = self.style_options.index(value)
        self._update_segment_buttons(
            self.style_groups[category], self.style_buttons[category], selected
        )

    def saveStyle_(self, _sender):
        update = {
            "cleanup": {
                "level": self.cleanup_selection,
                **{
                    f"style_{name}": value
                    for name, value in self.style_selections.items()
                },
            }
        }
        try:
            self.config = apply_config_update(self.config, update)
            save_config(self.config)
            self.page_errors.pop("Style", None)
            self.show_page("Style")
        except Exception as exc:  # noqa: BLE001
            # Keep the unsaved selections visible beside the error.
            state = self._preserve_page_state()
            self.page_errors["Style"] = str(exc)
            self.show_page("Style")
            self._restore_page_state(state)

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
        self.meeting_message = _label(
            "" if self.meeting_status else "Connecting to LocalFlow…",
            0, top, width, 13, "muted", palette=self.tokens, height=28,
        )
        self.body.addSubview_(self.meeting_message)
        self.meeting_retry = self._button(
            "Retry", "meetingRetry:", 0, top + 36, width=100,
        )
        self.meeting_retry.setHidden_(True)
        self.meeting_watch = _label(
            "", 0, 0, width, 11, "muted", palette=self.tokens, height=22,
        )
        self.body.addSubview_(self.meeting_watch)
        title_width = int((width - 20) * 0.58)
        template_x = title_width + 20
        self.meeting_title_label = _label(
            "Meeting title", 0, top + 28, 150, 11,
            "ink", palette=self.tokens, height=18, weight="semibold",
        )
        self.body.addSubview_(self.meeting_title_label)
        self.meeting_title = self._field(
            self.meeting_title_value, 0, top + 48, title_width
        )
        self.meeting_template_label = _label(
            "Notes template", template_x, top + 28, 150, 11,
            "ink", palette=self.tokens, height=18, weight="semibold",
        )
        self.body.addSubview_(self.meeting_template_label)
        self.meeting_template = NSComboBox.alloc().initWithFrame_(
            NSMakeRect(template_x, top + 48, width - template_x, 32)
        )
        self.meeting_template.setCompletes_(False)
        self.body.addSubview_(self.meeting_template)
        if self.meeting_templates:
            self._meeting_fill_templates()

        self.body.addSubview_(
            _label(
                "Your notes", 0, top + 92, 300, 13,
                "ink", palette=self.tokens, height=22, weight="semibold",
            )
        )
        notes_top = top + 118
        notes_bottom = notes_top + notes_height
        self.meeting_notes_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, notes_top, width, notes_height)
        )
        self.meeting_notes_scroll.setHasVerticalScroller_(True)
        self.meeting_notes_scroll.setAutohidesScrollers_(True)
        self.meeting_notes_scroll.setDrawsBackground_(True)
        self.meeting_notes = NSTextView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width - 20, notes_height - 4)
        )
        self.meeting_notes.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        self.meeting_notes.setFont_(_font(13))
        self.meeting_notes.setBackgroundColor_(_color("surface", self.tokens))
        self.meeting_notes.setTextColor_(_color("ink", self.tokens))
        self.meeting_notes.setString_(self.meeting_notes_text)
        self.meeting_notes.setDelegate_(self)
        self.meeting_notes_scroll.setDocumentView_(self.meeting_notes)
        style_surface(self.meeting_notes_scroll, self.tokens)
        self.body.addSubview_(self.meeting_notes_scroll)
        self.meeting_notes_status = _label(
            "", 0, notes_bottom + 4, width, 10, "muted",
            palette=self.tokens, height=18,
        )
        self.body.addSubview_(self.meeting_notes_status)
        self.meeting_error_label = _label(
            "", 0, notes_bottom + 26, width, 11, "red", palette=self.tokens, height=22,
        )
        self.body.addSubview_(self.meeting_error_label)
        self.meeting_action = self._button(
            "Start meeting transcription", "meetingAction:", 0, notes_bottom + 54,
            width=250, kind="primary",
        )
        self.meeting_live_label = _label(
            "", 8, 8, width - 16, 11, "ink", palette=self.tokens, height=82,
        )
        self.meeting_live_label.setLineBreakMode_(
            AppKit.NSLineBreakByWordWrapping
        )
        self.meeting_live_label.setMaximumNumberOfLines_(0)
        self.meeting_live_card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, notes_bottom + 98, width, 98)
        )
        style_surface(self.meeting_live_card, self.tokens)
        self.meeting_live_card.addSubview_(self.meeting_live_label)
        self.body.addSubview_(self.meeting_live_card)
        self.meeting_saved_label = _label(
            "", 0, notes_bottom + 212, width - 160, 11, "ink",
            palette=self.tokens, height=32,
        )
        self.body.addSubview_(self.meeting_saved_label)
        self.meeting_open_notes = self._button(
            "Open notes", "meetingOpenNotes:", width - 140, notes_bottom + 208,
            width=140,
        )
        self.meeting_chat_heading = _label(
            "Ask the transcript", 0, notes_bottom + 252, 400, 13,
            "ink", palette=self.tokens, height=22, weight="semibold",
        )
        self.body.addSubview_(self.meeting_chat_heading)
        self.meeting_question = self._field(
            "", 0, notes_bottom + 278, width - 120
        )
        self.meeting_question.setPlaceholderString_("Ask about this meeting")
        self.meeting_ask = self._button(
            "Ask", "meetingAsk:", width - 100, notes_bottom + 278,
            width=100, kind="primary",
        )
        self.meeting_answer_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, notes_bottom + 320, width, 64)
        )
        self.meeting_answer_scroll.setHasVerticalScroller_(True)
        self.meeting_answer_scroll.setAutohidesScrollers_(True)
        self.meeting_answer_content = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, width, 64)
        )
        self.meeting_answer_scroll.setDocumentView_(self.meeting_answer_content)
        self.body.addSubview_(self.meeting_answer_scroll)
        self._meeting_render()
        self._meeting_worker("templates")
        self._meeting_poll()

    @objc.python_method
    def _meeting_worker(self, operation, *args):
        method = "save_notes" if operation.startswith("notes_") else operation

        def run():
            try:
                result = getattr(self.meeting_client, method)(*args)
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
                self.meeting_last_saved = result
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
            self.meeting_watch, self.meeting_title_label, self.meeting_title,
            self.meeting_template_label, self.meeting_template,
            self.meeting_notes_scroll, self.meeting_notes_status,
            self.meeting_error_label, self.meeting_action, self.meeting_live_card,
            self.meeting_saved_label, self.meeting_open_notes,
            self.meeting_chat_heading, self.meeting_question,
            self.meeting_ask, self.meeting_answer_scroll,
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
    def _meeting_layout(self, session):
        width = self.body.frame().size.width
        action_frame = self.meeting_action.frame()
        watch_y = action_frame.origin.y + action_frame.size.height + 4
        self.meeting_watch.setFrame_(
            NSMakeRect(action_frame.origin.x, watch_y, width, 22)
        )
        live_y = watch_y + 30
        self.meeting_live_card.setFrame_(NSMakeRect(0, live_y, width, 98))
        self.meeting_live_label.setFrame_(NSMakeRect(8, 8, width - 16, 82))
        self.meeting_live_card.setHidden_(not bool(session))
        saved_y = live_y + (108 if session else 8)
        self.meeting_saved_label.setFrame_(
            NSMakeRect(0, saved_y, width - 160, 32)
        )
        self.meeting_open_notes.setFrame_(
            NSMakeRect(width - 140, saved_y - 4, 140, 36)
        )
        chat_y = saved_y + 44
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
        self._fit_body_height()

    @objc.python_method
    def _meeting_render(self):
        if not hasattr(self, "meeting_action"):
            return
        for view in self._meeting_page_controls():
            view.setHidden_(False)
        self.meeting_error_label.setStringValue_(self.meeting_error or "")
        session = self.meeting_status.get("session")
        if session:
            self.meeting_title_label.setHidden_(True)
            self.meeting_title.setHidden_(True)
            system_audio = session.get("system_audio")
            watch = (
                "mic + system audio" if system_audio == "on"
                else "mic only — system audio unavailable" if system_audio == "error"
                else "mic only"
            )
            if session.get("title"):
                watch = f"{session['title']} · {watch}"
            if system_audio == "error" and session.get("system_audio_error"):
                watch += " — " + session["system_audio_error"]
            self.meeting_watch.setStringValue_(watch)
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
            self.meeting_title_label.setHidden_(False)
            self.meeting_title.setHidden_(False)
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
        self._meeting_layout(session)
        self.meeting_action.setEnabled_(not self.meeting_busy)
        saved = self.meeting_last_saved if not session else None
        self.meeting_saved_label.setHidden_(not bool(saved))
        self.meeting_open_notes.setHidden_(not bool(saved))
        if saved:
            label = "Enhanced notes: " if saved.get("enhanced") else "Saved: "
            self.meeting_saved_label.setStringValue_(
                label + saved.get("notes_path", "")
            )
        chat_available = bool(
            not session
            and (self.meeting_status.get("chat_available") or self.meeting_last_saved)
        )
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

    def textDidChange_(self, _notification):
        if self.page != "Meetings" or getattr(self, "_meeting_adopting_notes", False):
            return
        self.meeting_notes_dirty = True
        self.meeting_notes_text = self.meeting_notes.string()
        self.meeting_notes_status.setStringValue_("Editing…")
        self.meeting_notes_status.setTextColor_(_color("muted", self.tokens))
        if self.meeting_status.get("session"):
            self.meeting_notes_due = time.monotonic() + 0.8
        else:
            self.meeting_notes_status.setStringValue_("Kept for when you start")

    def meetingAction_(self, _sender):
        if self.meeting_busy:
            return
        if not self.meeting_status.get("session"):
            title = self.meeting_title.stringValue() or "Meeting"
            index = self.meeting_template.indexOfSelectedItem()
            template = (
                self.meeting_templates[index]["id"]
                if 0 <= index < len(self.meeting_templates) else ""
            )
            self.meeting_busy = True
            self.meeting_error = ""
            self.meeting_title_value = title
            self.meeting_template_value = template
            self._meeting_render()
            self._meeting_worker(
                "start", title, "", template, self.meeting_notes.string()
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
        self.meeting_title_value = self.meeting_title.stringValue()
        selected = self.meeting_template.indexOfSelectedItem()
        if 0 <= selected < len(self.meeting_templates):
            self.meeting_template_value = self.meeting_templates[selected]["id"]

    def meetingOpenNotes_(self, _sender):
        saved = self.meeting_last_saved or {}
        path = vault_notes_path(saved.get("notes_path"), load_config().vault_path)
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
        self.config = load_config()
        self.body.addSubview_(
            _label(
                f"Speech model: {self.config.stt_backend} / {self.config.model_size}\n"
                f"Cleanup model: {self.config.llm_provider} / {self.config.llm_model or 'not selected'}",
                0, self.cursor_y, self.body.frame().size.width, 13, "ink",
                palette=self.tokens, height=54, wrap=True,
            )
        )
        self._button(
            "Change models…", "openSetup:", 0, self.cursor_y + 72,
            width=180, kind="primary",
        )

    @objc.python_method
    def _page_hot_phrases(self):
        # Temporary working placeholder until Hot Phrases moves into the Hub.
        width = self.body.frame().size.width
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, width, 150)
        )
        style_surface(card, self.tokens)
        self.body.addSubview_(card)
        self._card_label(
            card, "Say a word, get a saved prompt", CARD_PADDING + 8,
            size=15, weight="semibold",
        )
        self._card_label(
            card,
            "Hot phrases swap a short spoken trigger for text you saved, "
            "right as you dictate.",
            CARD_PADDING + 36, size=12, color="muted", height=36, wrap=True,
        )
        self._button(
            "Open Hot Phrases", "openHotPhrases:", CARD_PADDING, 92,
            width=180, parent=card, kind="primary",
        )
        self.cursor_y += 162

    def openHotPhrases_(self, _sender):
        subprocess.Popen([sys.executable, "-m", "localflow.phrases_window"])

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
            for index, row in enumerate(rows):
                self._prompt_row(listing, row, index, width)
                if index < len(rows) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, (index + 1) * 68 - 1, width - 24, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    listing.addSubview_(divider)
            self.cursor_y += len(rows) * 68 + 12

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
    def _prompt_row(self, listing, row, index, width):
        y = index * 68
        self.prompt_ids[index] = row["id"]
        buttons = []
        if row["can_approve"]:
            buttons.append(("Approve", "approvePrompt:", 88, "primary"))
        if row["can_reject"]:
            buttons.append(("Reject", "rejectPrompt:", 80, "destructive_ghost"))
        x = width - 12 - sum(w for _t, _a, w, _k in buttons) - 8 * max(0, len(buttons) - 1)
        action_start = x
        for title, action, button_width, kind in buttons:
            self._button(
                title, action, x, y + 16, index, button_width,
                parent=listing, kind=kind,
            )
            x += button_width + 8
        pill = make_pill(
            row["status_label"], PROMPT_STATUS_PILLS.get(row["status"], ("line", "muted")),
            self.tokens,
        )
        pill_width = pill.frame().size.width
        pill.setFrameOrigin_(NSMakePoint(action_start - pill_width - 12, y + 22))
        listing.addSubview_(pill)
        text_width = action_start - pill_width - 12 - 12 - 14
        listing.addSubview_(
            _label(
                row["title"], 14, y + 12, text_width, 13, "ink",
                palette=self.tokens, height=20, weight="semibold",
            )
        )
        listing.addSubview_(
            _label(
                row["context"], 14, y + 36, text_width, 11, "muted",
                palette=self.tokens, height=18,
            )
        )

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
    initial_page="Home", prompt_queue=None,
):
    config = config or load_config()
    return HubWindowController.alloc().initWithHistory_dictionary_config_meetingClient_promptQueue_initialPage_(
        history or HistoryStore(enabled=config.history_enabled),
        dictionary or DictionaryStore(),
        config,
        meeting_client,
        prompt_queue,
        initial_page,
    )


def main(initial_page="Home"):
    app = NSApplication.sharedApplication()
    controller = build_window(initial_page=initial_page)
    controller.window.makeKeyAndOrderFront_(None)
    app.activateIgnoringOtherApps_(True)
    from PyObjCTools import AppHelper
    AppHelper.runEventLoop()


if __name__ == "__main__":
    main()
