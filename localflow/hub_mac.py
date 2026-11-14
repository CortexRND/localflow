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
from Foundation import NSObject, NSTimer

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
from localflow.meeting_client import MeetingClient
from localflow.platform import current
from localflow.mac_style import (
    FlippedView,
    HalftoneView,
    color as _color,
    font as _font,
    segmented as _segmented,
    style_button,
    style_flat_button as _style_button,
    style_surface,
    tokens_for_window,
)
from localflow.ui_tokens import LIGHT, SUN_TEXT, TOKENS as TOKENS

PAGES = ("Home", "Meetings", "History", "Dictionary", "Style", "Models")


def _label(
    text, x, y, width=650, size=14, color="ink", *,
    palette=None, height=28, weight="regular",
):
    label = NSTextField.labelWithString_(text)
    label.setFrame_(NSMakeRect(x, y, width, height))
    label.setFont_(_font(size, weight))
    label.setTextColor_(_color(color, palette))
    label.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
    return label


class HubWindowController(NSObject):
    def init(self):
        config = load_config()
        return self.initWithHistory_dictionary_config_meetingClient_initialPage_(
            HistoryStore(enabled=config.history_enabled), DictionaryStore(), config,
            None, "Home",
        )

    def initWithHistory_dictionary_config_meetingClient_initialPage_(
        self, history, dictionary, config, meeting_client, initial_page
    ):
        self = super().init()  # noqa: PLW0642
        if self is None:
            return None
        self.history = history
        self.dictionary = dictionary
        self.config = config
        self.meeting_client = meeting_client or MeetingClient()
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
        self.meeting_busy = False
        self.meeting_error = ""
        self.meeting_next_poll = 0
        self.meeting_session_active = False
        self.tokens = LIGHT
        self.page = "Home"
        self.page_errors = {}
        self.page_notices = {}
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
            NSMakeRect(0, 0, 1040, 700),
            AppKit.NSTitledWindowMask | AppKit.NSClosableWindowMask |
            AppKit.NSMiniaturizableWindowMask | AppKit.NSResizableWindowMask,
            AppKit.NSBackingStoreBuffered,
            False,
        )
        self.window.setTitle_("LocalFlow Hub")
        self.window.center()
        self.root_view = FlippedView.alloc().initWithFrame_(NSMakeRect(0, 0, 1040, 700))
        self.root_view.setWantsLayer_(True)
        self.root_view.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        self.window.setContentView_(self.root_view)
        self.sidebar = FlippedView.alloc().initWithFrame_(NSMakeRect(0, 0, 200, 700))
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
        self.body = FlippedView.alloc().initWithFrame_(NSMakeRect(232, 24, 780, 652))
        self.root_view.addSubview_(self.body)
        self.window.setDelegate_(self)

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
                text, 0, self.cursor_y, 720, size, color,
                palette=self.tokens, height=height, weight=weight,
            )
        )
        self.cursor_y += height + gap

    @objc.python_method
    def _field(self, value, x, y, width=240):
        field = NSTextField.alloc().initWithFrame_(NSMakeRect(x, y, width, 32))
        field.setStringValue_(value or "")
        field.setFont_(_font(13))
        self.body.addSubview_(field)
        return field

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
        self.cursor_y = 0
        self.body.addSubview_(
            _label(page, 0, self.cursor_y, 700, 20, "ink", palette=self.tokens, height=28)
        )
        if page == "History":
            self._button(
                "Clear history", "clearHistory:", 610, 0, width=130,
                kind="destructive_ghost",
            )
        self.cursor_y = 48
        error = self.page_errors.get(page)
        if error:
            self._callout(error)
        getattr(self, f"_page_{page.lower()}")()

    @objc.python_method
    def _callout(self, message):
        card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, 720, 58)
        )
        style_surface(card, self.tokens)
        bar = NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 4, 58))
        bar.setWantsLayer_(True)
        bar.layer().setBackgroundColor_(_color("red", self.tokens).CGColor())
        card.addSubview_(bar)
        card.addSubview_(
            _label("Something went wrong", 16, 8, 660, 13, "red", palette=self.tokens, height=18)
        )
        card.addSubview_(
            _label(message, 16, 30, 660, 11, "muted", palette=self.tokens, height=18)
        )
        self.body.addSubview_(card)
        self.cursor_y += 70

    def windowDidChangeEffectiveAppearance_(self, _notification):
        state = self._preserve_page_state()
        self.show_page(self.page)
        self._restore_page_state(state)

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

    def showPage_(self, sender):
        self.show_page(PAGES[sender.tag()])

    @objc.python_method
    def _page_home(self):
        stats = self.history.stats()
        self.body.addSubview_(
            HalftoneView.alloc().initWithFrame_palette_(
                NSMakeRect(572, 0, 150, 116), self.tokens
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
        for index, (title, value) in enumerate(cards):
            card = FlippedView.alloc().initWithFrame_(
                NSMakeRect(index * 180, self.cursor_y, 168, 104)
            )
            style_surface(
                card, self.tokens,
                fill="sun" if title == "Streak" else "surface",
                border="ink" if title == "Streak" else "line",
                shadow=title == "Streak",
            )
            card.addSubview_(
                _label(
                    title.upper(), 16, 16, 140, 16,
                    SUN_TEXT if title == "Streak" else "muted",
                    palette=self.tokens, height=18, weight="semibold",
                )
            )
            card.addSubview_(
                _label(
                    str(value), 16, 42, 140, 32,
                    SUN_TEXT if title == "Streak" else "ink",
                    palette=self.tokens, height=34, weight="bold",
                )
            )
            self.body.addSubview_(card)
        self.cursor_y += 120
        self.body.addSubview_(
            _label(
                f'Today  {stats["words_today"]} words     Last 7 days  '
                f'{stats["words_7d"]} words     Longest streak  {longest_streak} day'
                f'{"s" if longest_streak != 1 else ""}',
                0, self.cursor_y, 740, 12, "muted", palette=self.tokens, height=20,
            )
        )
        self.cursor_y += 36
        self._add_text("Recent dictations", size=15, weight="semibold")
        rows = self.history.recent(limit=4)
        if not rows:
            self._empty_state(
                self.body, 0, self.cursor_y, 720, 220,
                "No dictations yet",
                "Your recent dictations will appear here.",
                "Open setup", "openSetup:",
            )
            return
        listing = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, 740, len(rows) * 56)
        )
        style_surface(listing, self.tokens)
        self.body.addSubview_(listing)
        for index, row in enumerate(rows):
            self._history_row(listing, row, index * 56, actions=False)
            if index < len(rows) - 1:
                divider = NSView.alloc().initWithFrame_(
                    NSMakeRect(12, (index + 1) * 56 - 1, 716, 1)
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
            _label(detail, 24, 96, width - 48, 13, "muted", palette=self.tokens, height=28)
        )
        self._button(
            action, selector, 24, 140, width=138, parent=state,
        )
        parent.addSubview_(state)

    @objc.python_method
    def _history_row(self, parent, row, y, actions):
        text = row.get("final_text") or row.get("raw_text") or "(no text)"
        timestamp = datetime.fromtimestamp(float(row.get("ts") or 0))
        if timestamp.date() == datetime.now().date():
            date_label = timestamp.strftime("%I:%M %p").lstrip("0")
        else:
            date_label = timestamp.strftime("%b %d").replace(" 0", " ")
        action_start = 740
        if actions:
            entries = [
                ("Copy", "copyEntry:", 56),
                ("Original", "rawEntry:", 76),
            ]
            if row["kind"] == "dictate" and row.get("audio_path"):
                entries.append(("Retry", "retryEntry:", 60))
            entries.append(("Delete", "deleteEntry:", 72))
            action_start = 740 - sum(
                width for _title, _action, width in entries
            ) - 8 * (len(entries) - 1)
            x = action_start
            for title, action, width in entries:
                self._button(
                    title, action, x, y + 10, row["id"], width,
                    parent=parent,
                    kind="destructive_ghost" if title == "Delete" else "ghost",
                )
                x += width + 8
        text_width = (action_start - 24) if actions else 700
        status = row.get("status", "ok")
        if status != "ok":
            chip_x = max(340, action_start - 82) if actions else 638
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
        self.search_field = self._field(self.history_query, 0, self.cursor_y, 600)
        self.search_field.setPlaceholderString_("Search transcripts")
        self._button("Search", "searchHistory:", 612, self.cursor_y, width=100)
        self.cursor_y += 48
        self.history_results_height = 520
        self.history_results = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, 760, self.history_results_height)
        )
        self.body.addSubview_(self.history_results)
        self._render_history()

    @objc.python_method
    def _render_history(self, query=None):
        if query is not None:
            self.history_query = query
            self.history_offset = 0
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
                self.history_results, 0, 110, 720, 220, *empty,
            )
        else:
            listing_height = len(rows) * 56
            listing = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, 0, 740, len(rows) * 56)
            )
            style_surface(listing, self.tokens)
            self.history_results.addSubview_(listing)
            for index, row in enumerate(rows):
                self._history_row(listing, row, index * 56, actions=True)
                if index < len(rows) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, (index + 1) * 56 - 1, 716, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    listing.addSubview_(divider)
        page_y = listing_height + 16
        self.history_results_height = page_y + 36
        self.history_results.setFrame_(
            NSMakeRect(0, self.cursor_y, 760, self.history_results_height)
        )
        if self.history_offset:
            self._button(
                "Newer", "newerHistory:", 270, page_y, width=90,
                parent=self.history_results,
            )
        self.history_results.addSubview_(
            _label(
                f"Page {self.history_offset // 8 + 1}",
                370, page_y + 8, 90, 13, "muted", palette=self.tokens, height=18, weight="semibold",
            )
        )
        if has_older:
            self._button(
                "Older", "olderHistory:", 470, page_y, width=90,
                parent=self.history_results,
            )

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
    def _page_dictionary(self):
        self.editing_word = None
        self.editing_source = None
        self.word_field = self._field("", 0, self.cursor_y, 160)
        self._button("Add / save word", "addWord:", 172, self.cursor_y, width=138)
        self.from_field = self._field("", 328, self.cursor_y, 130)
        self.to_field = self._field("", 472, self.cursor_y, 130)
        self._button(
            "Add / save replacement", "addReplacement:",
            614, self.cursor_y, width=160,
        )
        self.cursor_y += 44
        self._button(
            "Import CSV/JSON…", "importDictionary:",
            0, self.cursor_y, width=170,
        )
        self.cursor_y += 48
        notice = self.page_notices.get("Dictionary")
        if notice:
            banner = FlippedView.alloc().initWithFrame_(
                NSMakeRect(0, self.cursor_y, 720, 36)
            )
            banner.setWantsLayer_(True)
            banner.layer().setBackgroundColor_(_color("sun", self.tokens).CGColor())
            self.body.addSubview_(banner)
            self.body.addSubview_(
                _label(
                    notice, 10, self.cursor_y + 8, 700, 11,
                    SUN_TEXT, palette=self.tokens, height=20,
                )
            )
            self.cursor_y += 44
        self._render_dictionary()

    @objc.python_method
    def _render_dictionary(self):
        data = self.dictionary.list()
        entries = [(word, word, "word", "") for word in data["words"]]
        entries.extend(
            (f'{pair["from"]} → {pair["to"]}', pair["from"], "replacement", pair["to"])
            for pair in data["replacements"]
        )
        if hasattr(self, "dictionary_scroll"):
            self.dictionary_scroll.removeFromSuperview()
        self.dictionary_keys = {}
        listing_height = (
            220 if not entries else min(10, len(entries)) * 56
        )
        self.dictionary_results = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, 740, listing_height)
        )
        style_surface(self.dictionary_results, self.tokens)
        if entries:
            for index, (label, key, kind, target) in enumerate(entries[:10]):
                y = index * 56
                self.dictionary_results.addSubview_(
                    _label(
                        label, 14, y + 17, 460, 13, "ink",
                        palette=self.tokens, height=20,
                    )
                )
                self._button(
                    "Edit", "editDictionary:", 540, y + 10, index, 72,
                    parent=self.dictionary_results, kind="ghost",
                )
                self._button(
                    "Remove", "removeDictionary:", 628, y + 10, index, 88,
                    parent=self.dictionary_results, kind="destructive_ghost",
                )
                self.dictionary_keys[index] = (key, kind, target)
                if index < min(len(entries), 10) - 1:
                    divider = NSView.alloc().initWithFrame_(
                        NSMakeRect(12, (index + 1) * 56 - 1, 716, 1)
                    )
                    divider.setWantsLayer_(True)
                    divider.layer().setBackgroundColor_(
                        _color("line", self.tokens).CGColor()
                    )
                    self.dictionary_results.addSubview_(divider)
        else:
            self._empty_state(
                self.dictionary_results, 0, 0, 720, 200,
                "No vocabulary yet",
                "Add a word or replacement to personalize dictation.",
                "Add a word", "focusWord:",
            )
        scroll_height = min(
            500, listing_height,
            self.body.frame().size.height - self.cursor_y,
        )
        self.dictionary_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, self.cursor_y, 740, scroll_height)
        )
        self.dictionary_scroll.setDocumentView_(self.dictionary_results)
        self.dictionary_scroll.setHasVerticalScroller_(listing_height > scroll_height)
        self.dictionary_scroll.setAutohidesScrollers_(True)
        self.dictionary_scroll.setDrawsBackground_(False)
        self.body.addSubview_(self.dictionary_scroll)

    def addWord_(self, _sender):
        try:
            if self.editing_word:
                if not self.dictionary.update_word(
                    self.editing_word, self.word_field.stringValue()
                ):
                    raise DictionaryError("word already exists or was removed")
            else:
                self.dictionary.add_word(self.word_field.stringValue())
            self.editing_word = None
            self.show_page("Dictionary")
        except Exception as exc:  # noqa: BLE001
            self.page_errors["Dictionary"] = str(exc)
            self.show_page("Dictionary")

    def addReplacement_(self, _sender):
        try:
            if self.editing_source:
                if not self.dictionary.update_replacement(
                    self.editing_source,
                    self.from_field.stringValue(),
                    self.to_field.stringValue(),
                ):
                    raise DictionaryError(
                        "replacement source already exists or was removed"
                    )
            else:
                self.dictionary.add_replacement(
                    self.from_field.stringValue(), self.to_field.stringValue()
                )
            self.editing_source = None
            self.show_page("Dictionary")
        except Exception as exc:  # noqa: BLE001
            self.page_errors["Dictionary"] = str(exc)
            self.show_page("Dictionary")

    def removeDictionary_(self, sender):
        key, _kind, _target = self.dictionary_keys.get(
            sender.tag(), ("", "", "")
        )
        self.dictionary.remove(key)
        self.show_page("Dictionary")

    def editDictionary_(self, sender):
        key, kind, target = self.dictionary_keys.get(sender.tag(), ("", "", ""))
        if kind == "word":
            self.editing_word = key
            self.word_field.setStringValue_(key)
        else:
            self.editing_source = key
            self.from_field.setStringValue_(key)
            self.to_field.setStringValue_(target)

    def importDictionary_(self, _sender):
        panel = AppKit.NSOpenPanel.openPanel()
        panel.setAllowedFileTypes_(["csv", "json"])
        if panel.runModal() == AppKit.NSModalResponseOK:
            try:
                counts = self.dictionary.import_file(panel.URL().path())
                self.page_errors.pop("Dictionary", None)
                self.page_notices["Dictionary"] = (
                    f'Imported {counts["added"]}; duplicates {counts["skipped_duplicate"]}; '
                    f'invalid {counts["skipped_invalid"]}.'
                )
                self.show_page("Dictionary")
            except Exception as exc:  # noqa: BLE001
                self.page_errors["Dictionary"] = str(exc)
                self.show_page("Dictionary")

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
        cleanup_group = self._segment_group(
            self.cursor_y, 180, 330,
        )
        self.cleanup_group = cleanup_group
        for index, (value, title) in enumerate(
            zip(self.cleanup_options, ("None", "Light", "Medium"))
        ):
            button = self._segment_button(
                title, "selectCleanupLevel_", index,
                index * 110, 1, 110,
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
            group = self._segment_group(y, 180, 540)
            self.style_groups[category] = group
            for option_index, value in enumerate(self.style_options):
                tag = index * len(self.style_options) + option_index
                self.style_tag_map[tag] = (category, value)
                self.style_buttons[category].append(
                    self._segment_button(
                        value.replace("_", " ").title(),
                        "selectStyle_",
                        tag,
                        option_index * 135, 1, 135,
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
            self.page_errors["Style"] = str(exc)
            self.show_page("Style")

    @objc.python_method
    def _meeting_button(self, title, action, x, y, width, kind="secondary"):
        return self._button(
            title, action, x, y, width=width, kind=kind,
        )

    @objc.python_method
    def _page_meetings(self):
        self.meeting_message = _label(
            "Connecting to LocalFlow…", 0, 0, 600, 13,
            "muted", palette=self.tokens, height=28,
        )
        self.body.addSubview_(self.meeting_message)
        self.meeting_retry = self._button(
            "Retry", "meetingRetry:", 0, 36, width=100,
        )
        self.meeting_retry.setHidden_(True)
        self.meeting_watch = _label(
            "", 0, 0, 720, 11, "muted", palette=self.tokens, height=22,
        )
        self.body.addSubview_(self.meeting_watch)
        self.meeting_title_label = _label(
            "Meeting title", 0, 28, 150, 11,
            "ink", palette=self.tokens, height=18, weight="semibold",
        )
        self.body.addSubview_(self.meeting_title_label)
        self.meeting_title = self._field(self.meeting_title_value, 0, 48, 420)
        self.meeting_template_label = _label(
            "Notes template", 440, 28, 150, 11,
            "ink", palette=self.tokens, height=18, weight="semibold",
        )
        self.body.addSubview_(self.meeting_template_label)
        self.meeting_template = NSComboBox.alloc().initWithFrame_(
            NSMakeRect(440, 48, 300, 32)
        )
        self.meeting_template.setCompletes_(False)
        self.body.addSubview_(self.meeting_template)

        self.body.addSubview_(
            _label(
                "Your notes", 0, 92, 300, 13,
                "ink", palette=self.tokens, height=22, weight="semibold",
            )
        )
        self.meeting_notes_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, 118, 740, 146)
        )
        self.meeting_notes_scroll.setHasVerticalScroller_(True)
        self.meeting_notes_scroll.setAutohidesScrollers_(True)
        self.meeting_notes_scroll.setDrawsBackground_(True)
        self.meeting_notes = NSTextView.alloc().initWithFrame_(
            NSMakeRect(0, 0, 720, 142)
        )
        self.meeting_notes.setFont_(_font(13))
        self.meeting_notes.setBackgroundColor_(_color("surface", self.tokens))
        self.meeting_notes.setTextColor_(_color("ink", self.tokens))
        self.meeting_notes.setString_(self.meeting_notes_text)
        self.meeting_notes.setDelegate_(self)
        self.meeting_notes_scroll.setDocumentView_(self.meeting_notes)
        style_surface(self.meeting_notes_scroll, self.tokens)
        self.body.addSubview_(self.meeting_notes_scroll)
        self.meeting_notes_status = _label(
            "", 0, 268, 600, 10, "muted", palette=self.tokens, height=18,
        )
        self.body.addSubview_(self.meeting_notes_status)
        self.meeting_error_label = _label(
            "", 0, 290, 720, 11, "red", palette=self.tokens, height=22,
        )
        self.body.addSubview_(self.meeting_error_label)
        self.meeting_action = self._button(
            "Start meeting transcription", "meetingAction:", 0, 318,
            width=250, kind="primary",
        )
        self.meeting_live_label = _label(
            "", 8, 8, 724, 11, "ink", palette=self.tokens, height=82,
        )
        self.meeting_live_label.setLineBreakMode_(
            AppKit.NSLineBreakByWordWrapping
        )
        self.meeting_live_label.setMaximumNumberOfLines_(0)
        self.meeting_live_card = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 362, 740, 98)
        )
        style_surface(self.meeting_live_card, self.tokens)
        self.meeting_live_card.addSubview_(self.meeting_live_label)
        self.body.addSubview_(self.meeting_live_card)
        self.meeting_saved_label = _label(
            "", 0, 476, 565, 11, "ink", palette=self.tokens, height=32,
        )
        self.body.addSubview_(self.meeting_saved_label)
        self.meeting_open_notes = self._button(
            "Open notes", "meetingOpenNotes:", 584, 472, width=140,
        )
        self.meeting_chat_heading = _label(
            "Ask the transcript", 0, 516, 400, 13,
            "ink", palette=self.tokens, height=22, weight="semibold",
        )
        self.body.addSubview_(self.meeting_chat_heading)
        self.meeting_question = self._field("", 0, 542, 580)
        self.meeting_question.setPlaceholderString_("Ask about this meeting")
        self._button("Ask", "meetingAsk:", 596, 542, width=100, kind="primary")
        self.meeting_answer_scroll = NSScrollView.alloc().initWithFrame_(
            NSMakeRect(0, 584, 740, 64)
        )
        self.meeting_answer_scroll.setHasVerticalScroller_(True)
        self.meeting_answer_scroll.setAutohidesScrollers_(True)
        self.meeting_answer_content = FlippedView.alloc().initWithFrame_(
            NSMakeRect(0, 0, 740, 64)
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
    def _meeting_event(self, operation, result, error):
        if operation == "templates":
            if not error:
                self.meeting_templates = result.get("templates", [])
                self.meeting_template.removeAllItems()
                self.meeting_template.addItemsWithObjectValues_(
                    [item["name"] for item in self.meeting_templates]
                )
                default = result.get("default", "general")
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
            return
        if operation == "notes_autosave":
            if error:
                self.meeting_notes_dirty = True
                self.meeting_error = str(error)
                self._meeting_render()
                return
            self.meeting_notes_dirty = False
            self.meeting_notes_status.setStringValue_(
                "Saved " + datetime.now().strftime("%H:%M")
            )
            self.meeting_notes_status.setTextColor_(_color("success", self.tokens))
            return
        if operation == "notes_stop_flush":
            if error:
                self.meeting_busy = False
                self.meeting_notes_dirty = True
                self.meeting_error = (
                    "Notes could not be saved; the meeting is still running — "
                    + str(error)
                )
                self._meeting_render()
                return
            self._meeting_worker("stop")
            self.meeting_action.setTitle_("Enhancing your notes…")
            return
        if operation == "status":
            if error:
                self.meeting_status = {}
                self.meeting_error = ""
                self.meeting_message.setStringValue_(str(error))
                self.meeting_retry.setHidden_(False)
                for view in self._meeting_page_controls():
                    view.setHidden_(True)
            else:
                self.meeting_status = result
                self.meeting_message.setStringValue_("")
                self.meeting_retry.setHidden_(True)
                if result.get("last_saved"):
                    self.meeting_last_saved = result["last_saved"]
                session = result.get("session")
                if (
                    session and not self.meeting_session_active
                    and not self.meeting_notes_dirty
                ):
                    self._meeting_adopting_notes = True
                    self.meeting_notes_text = session.get("user_notes", "")
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
                        self.meeting_template.selectItemAtIndex_(selected)
                        self.meeting_template_value = self.meeting_templates[selected]["id"]
                self.meeting_session_active = bool(session)
                self._meeting_render()
            if self.page == "Meetings":
                self.meeting_next_poll = time.monotonic() + 2
            return
        if operation == "start":
            self.meeting_busy = False
            if error:
                self.meeting_error = str(error)
                self._meeting_render()
            else:
                self.meeting_notes_dirty = False
                self.meeting_last_saved = None
                self.meeting_answers.clear()
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
                self._meeting_adopting_notes = True
                self.meeting_notes.setString_("")
                self._meeting_adopting_notes = False
                self.meeting_notes_text = ""
                self.meeting_notes_dirty = False
                self.meeting_notes_status.setStringValue_("")
            self._meeting_render()
            self._meeting_poll()
            return
        if operation == "chat":
            self.meeting_busy = False
            answer = f"Couldn’t answer — {error}" if error else result.get("answer", "")
            self.meeting_answers[0]["answer"] = answer
            self.meeting_answers[0]["error"] = error is not None
            self._meeting_render_chat()

    @objc.python_method
    def _meeting_page_controls(self):
        return (
            self.meeting_watch, self.meeting_title_label, self.meeting_title,
            self.meeting_template_label, self.meeting_template,
            self.meeting_notes_scroll, self.meeting_notes_status,
            self.meeting_error_label, self.meeting_action, self.meeting_live_card,
            self.meeting_saved_label, self.meeting_open_notes,
            self.meeting_chat_heading,
            self.meeting_question, self.meeting_answer_scroll,
        )

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
            self.meeting_action.setTitle_(
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
            self.meeting_action.setTitle_("Start meeting transcription")
            self.meeting_live_label.setStringValue_("")
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
            self.meeting_status.get("chat_available") or self.meeting_last_saved
        )
        self.meeting_chat_heading.setHidden_(not chat_available)
        self.meeting_question.setHidden_(not chat_available)
        self.meeting_answer_scroll.setHidden_(
            not chat_available or not self.meeting_answers
        )
        self._meeting_render_chat()

    @objc.python_method
    def _meeting_render_chat(self):
        if not hasattr(self, "meeting_answer_content"):
            return
        for view in list(self.meeting_answer_content.subviews()):
            view.removeFromSuperview()
        height = max(64, len(self.meeting_answers) * 44)
        self.meeting_answer_content.setFrame_(NSMakeRect(0, 0, 740, height))
        for index, item in enumerate(self.meeting_answers):
            label = _label(
                f"You: {item['question']}\n{item['answer']}",
                0, index * 44, 720, 11,
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
            if self.page == "Meetings":
                self._meeting_event(operation, result, error)
        if self.page == "Meetings":
            if (
                self.meeting_notes_due is not None
                and time.monotonic() >= self.meeting_notes_due
            ):
                self.meeting_notes_due = None
                self._meeting_worker("notes_autosave", self.meeting_notes.string())
                self.meeting_notes_status.setStringValue_("Saving…")
            if time.monotonic() >= self.meeting_next_poll:
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
        self.meeting_notes_status.setStringValue_("Saving…")
        self._meeting_worker("notes_stop_flush", self.meeting_notes.string())

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
        if self.meeting_last_saved and self.meeting_last_saved.get("notes_path"):
            current().open_path(self.meeting_last_saved["notes_path"])

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
                0, self.cursor_y, 700, 13, "ink", palette=self.tokens, height=54,
            )
        )
        self._button(
            "Change models…", "openSetup:", 0, self.cursor_y + 72,
            width=180, kind="primary",
        )

    def focusWord_(self, _sender):
        self.window.makeFirstResponder_(self.word_field)

    def openSetup_(self, _sender):
        subprocess.Popen([sys.executable, "-m", "localflow.setup_window"])


def build_window(
    history=None, dictionary=None, config=None, meeting_client=None,
    initial_page="Home",
):
    config = config or load_config()
    return HubWindowController.alloc().initWithHistory_dictionary_config_meetingClient_initialPage_(
        history or HistoryStore(enabled=config.history_enabled),
        dictionary or DictionaryStore(),
        config,
        meeting_client,
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
