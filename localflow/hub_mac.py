"""Native AppKit history and settings hub for macOS."""

import subprocess
import sys
from datetime import datetime

import AppKit
import objc
from AppKit import (
    NSAlert,
    NSApplication,
    NSButton,
    NSMakeRect,
    NSScrollView,
    NSTextField,
    NSView,
    NSWindow,
)
from Foundation import NSObject

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
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

PAGES = ("Home", "History", "Dictionary", "Style", "Models")


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
        return self.initWithHistory_dictionary_config_(
            HistoryStore(enabled=config.history_enabled), DictionaryStore(), config
        )

    def initWithHistory_dictionary_config_(self, history, dictionary, config):
        self = super().init()  # noqa: PLW0642
        if self is None:
            return None
        self.history = history
        self.dictionary = dictionary
        self.config = config
        self.tokens = LIGHT
        self.page = "Home"
        self.page_errors = {}
        self.page_notices = {}
        self._build_window()
        self.show_page("Home")
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


def build_window(history=None, dictionary=None, config=None):
    config = config or load_config()
    return HubWindowController.alloc().initWithHistory_dictionary_config_(
        history or HistoryStore(enabled=config.history_enabled),
        dictionary or DictionaryStore(),
        config,
    )


def main():
    app = NSApplication.sharedApplication()
    controller = build_window()
    controller.window.makeKeyAndOrderFront_(None)
    app.activateIgnoringOtherApps_(True)
    from PyObjCTools import AppHelper
    AppHelper.runEventLoop()


if __name__ == "__main__":
    main()
