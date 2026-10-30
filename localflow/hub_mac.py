"""Native AppKit history and settings hub for macOS."""

import subprocess
import sys

import AppKit
import objc
from AppKit import (
    NSAlert,
    NSApplication,
    NSButton,
    NSColor,
    NSFont,
    NSMakeRect,
    NSTextField,
    NSView,
    NSWindow,
)
from Foundation import NSObject

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
from localflow.setup_mac import TOKENS

PAGES = ("Home", "History", "Dictionary", "Style", "Models")


def _color(token):
    value = TOKENS[token].lstrip("#")
    red, green, blue = (int(value[i:i + 2], 16) / 255 for i in (0, 2, 4))
    return NSColor.colorWithCalibratedRed_green_blue_alpha_(red, green, blue, 1)


def _label(text, x, y, width=650, size=14, color="ink"):
    label = NSTextField.labelWithString_(text)
    label.setFrame_(NSMakeRect(x, y, width, 34))
    label.setFont_(NSFont.systemFontOfSize_(size))
    label.setTextColor_(_color(color))
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
        self.root_view = NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 1040, 700))
        self.root_view.setWantsLayer_(True)
        self.root_view.layer().setBackgroundColor_(_color("paper").CGColor())
        self.window.setContentView_(self.root_view)
        self.sidebar = NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 210, 700))
        self.sidebar.setWantsLayer_(True)
        self.sidebar.layer().setBackgroundColor_(_color("ink").CGColor())
        self.root_view.addSubview_(self.sidebar)
        brand = _label("LOCALFLOW  /  HUB", 22, 635, 175, 15, "sun")
        self.sidebar.addSubview_(brand)
        for index, page in enumerate(PAGES):
            button = NSButton.buttonWithTitle_target_action_(page, self, "showPage:")
            button.setFrame_(NSMakeRect(16, 575 - index * 46, 175, 34))
            button.setTag_(index)
            self.sidebar.addSubview_(button)
        self.body = NSView.alloc().initWithFrame_(NSMakeRect(235, 20, 780, 660))
        self.root_view.addSubview_(self.body)

    @objc.python_method
    def _button(self, title, action, x, y, tag=0, width=150, parent=None):
        button = NSButton.buttonWithTitle_target_action_(title, self, action)
        button.setFrame_(NSMakeRect(x, y, width, 32))
        button.setTag_(tag)
        container = parent if parent is not None else self.body
        container.addSubview_(button)
        return button

    @objc.python_method
    def _field(self, value, x, y, width=240):
        field = NSTextField.alloc().initWithFrame_(NSMakeRect(x, y, width, 28))
        field.setStringValue_(value or "")
        self.body.addSubview_(field)
        return field

    @objc.python_method
    def show_page(self, page):
        self.page = page
        for view in self.body.subviews():
            view.removeFromSuperview()
        self.body.addSubview_(_label(page, 0, 610, size=24))
        error = self.page_errors.get(page)
        if error:
            self.body.addSubview_(_label(error, 0, 565, 710, 13, "red"))
        getattr(self, f"_page_{page.lower()}")()

    def showPage_(self, sender):
        self.show_page(PAGES[sender.tag()])

    @objc.python_method
    def _page_home(self):
        stats = self.history.stats()
        streak = stats["current_streak_days"]
        longest_streak = stats["longest_streak_days"]
        cards = (
            ("Words", stats["total_words"]),
            ("Avg WPM", f'{stats["avg_wpm"]:.1f}'),
            ("Streak", f"{streak} day{'s' if streak != 1 else ''}"),
            ("Dictations", stats["total_dictations"]),
        )
        for index, (title, value) in enumerate(cards):
            card = NSView.alloc().initWithFrame_(
                NSMakeRect(index * 180, 475, 168, 68)
            )
            card.setWantsLayer_(True)
            card.layer().setBackgroundColor_(
                _color("sun" if title == "Streak" else "card").CGColor()
            )
            card.layer().setBorderColor_(_color("ink").CGColor())
            card.layer().setBorderWidth_(1)
            card.addSubview_(_label(title.upper(), 10, 42, 148, 10, "muted"))
            card.addSubview_(_label(str(value), 10, 12, 148, size=18))
            self.body.addSubview_(card)
        self.body.addSubview_(
            _label(
                f'Today  {stats["words_today"]} words     Last 7 days  '
                f'{stats["words_7d"]} words     Longest streak  {longest_streak} day'
                f'{"s" if longest_streak != 1 else ""}',
                0, 450, 740, 12, "muted",
            )
        )
        for index, row in enumerate(self.history.recent(limit=10)):
            text = row["final_text"] or row["raw_text"] or "(no text)"
            self.body.addSubview_(
                _label(f'{row["kind"]} · {row["app_name"] or "Unknown"} · {text[:80]}',
                       0, 420 - index * 30, 730, 12)
            )
        if not self.history.recent(limit=1):
            self.body.addSubview_(_label("Your dictations will appear here.", 0, 420, color="muted"))

    @objc.python_method
    def _page_history(self):
        self.search_field = self._field("", 0, 565, 350)
        self._button("Search", "searchHistory:", 360, 565)
        self._button("Clear history", "clearHistory:", 530, 565)
        self.history_offset = 0
        self.history_query = ""
        self.history_results = NSView.alloc().initWithFrame_(
            NSMakeRect(0, 25, 760, 510)
        )
        self.body.addSubview_(self.history_results)
        self._render_history("")

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
        for index, row in enumerate(rows):
            y = 455 - index * 52
            self.history_results.addSubview_(
                _label(
                    f'{row["id"]} · {row["kind"]} · {row["status"]} · '
                    f'{(row["final_text"] or row["raw_text"])[:42]}',
                    0, y, 310, 12,
                )
            )
            self._button(
                "Copy", "copyEntry:", 320, y, row["id"], 66,
                parent=self.history_results,
            )
            self._button(
                "Copy original", "rawEntry:", 390, y, row["id"], 110,
                parent=self.history_results,
            )
            if row["kind"] == "dictate" and row["audio_path"]:
                self._button(
                    "Retry", "retryEntry:", 505, y, row["id"], 65,
                    parent=self.history_results,
                )
            self._button(
                "Delete", "deleteEntry:", 575, y, row["id"], 75,
                parent=self.history_results,
            )
        if not rows:
            self.history_results.addSubview_(
                _label("No matching history entries.", 0, 455, color="muted")
            )
        if self.history_offset:
            self._button(
                "Newer", "newerHistory:", 320, 8, width=80,
                parent=self.history_results,
            )
        if has_older:
            self._button(
                "Older", "olderHistory:", 410, 8, width=80,
                parent=self.history_results,
            )

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
        self.word_field = self._field("", 0, 560, 150)
        self._button("Add / save word", "addWord:", 160, 560, width=110)
        self.from_field = self._field("", 280, 560, 140)
        self.to_field = self._field("", 430, 560, 140)
        self._button("Add / save replacement", "addReplacement:", 580, 560, width=180)
        self._button("Import CSV/JSON…", "importDictionary:", 0, 515, width=170)
        notice = self.page_notices.get("Dictionary")
        if notice:
            banner = NSView.alloc().initWithFrame_(NSMakeRect(0, 475, 720, 30))
            banner.setWantsLayer_(True)
            banner.layer().setBackgroundColor_(_color("sun").CGColor())
            self.body.addSubview_(banner)
            self.body.addSubview_(_label(notice, 10, 475, 700, 11))
        self._render_dictionary()

    @objc.python_method
    def _render_dictionary(self):
        data = self.dictionary.list()
        entries = [(word, word, "word", "") for word in data["words"]]
        entries.extend(
            (f'{pair["from"]} → {pair["to"]}', pair["from"], "replacement", pair["to"])
            for pair in data["replacements"]
        )
        self.dictionary_keys = {}
        for index, (label, key, kind, target) in enumerate(entries[:10]):
            y = (430 if self.page_notices.get("Dictionary") else 465) - index * 38
            self.body.addSubview_(_label(label, 0, y, 450, 12))
            self._button("Edit", "editDictionary:", 470, y, index, 75)
            self._button("Remove", "removeDictionary:", 550, y, index, 90)
            self.dictionary_keys[index] = (key, kind, target)
        if not entries:
            self.body.addSubview_(_label("No vocabulary or replacements yet.", 0, 470, color="muted"))

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
        self.style_tag_map = {}
        self.body.addSubview_(_label("Cleanup level", 0, 560, 145, 12))
        for index, (value, title) in enumerate(
            zip(self.cleanup_options, ("None", "Light", "Medium"))
        ):
            self.cleanup_buttons.append(
                self._segment_button(
                    title, "selectCleanupLevel_", index,
                    155 + index * 105, 555, 95,
                    selected=value == self.cleanup_selection,
                )
            )
        examples = {
            "work": "Please share the updated timeline.",
            "email": "Thanks for following up.",
            "personal": "Let's catch up soon!",
            "other": "The main point is clear.",
        }
        for index, (category, example) in enumerate(examples.items()):
            y = 465 - index * 78
            self.body.addSubview_(
                _label(f"{category.title()} style", 0, y + 22, 180, 12)
            )
            self.body.addSubview_(
                _label(example, 0, y - 3, 180, 10, "muted")
            )
            self.style_selections[category] = getattr(
                self.config, f"style_{category}"
            )
            self.style_buttons[category] = []
            for option_index, value in enumerate(self.style_options):
                tag = index * len(self.style_options) + option_index
                self.style_tag_map[tag] = (category, value)
                self.style_buttons[category].append(
                    self._segment_button(
                        value.replace("_", " ").title(),
                        "selectStyle_",
                        tag,
                        190 + option_index * 132,
                        y + 12,
                        124,
                        selected=value == self.style_selections[category],
                    )
                )
        self._button("Save style", "saveStyle:", 0, 145, width=150)

    @objc.python_method
    def _segment_button(
        self, title, action, tag, x, y, width, *, selected, parent=None
    ):
        button = NSButton.alloc().initWithFrame_(NSMakeRect(x, y, width, 34))
        button.setTitle_(title)
        button.setTarget_(self)
        button.setAction_(objc.selector(getattr(self, action), signature=b"v@:@"))
        button.setTag_(tag)
        button.setBezelStyle_(AppKit.NSBezelStyleRounded)
        button.setFont_(NSFont.boldSystemFontOfSize_(12))
        button.setWantsLayer_(True)
        button.layer().setBackgroundColor_(
            _color("sun" if selected else "card").CGColor()
        )
        button.layer().setBorderColor_(_color("ink").CGColor())
        button.layer().setBorderWidth_(2)
        button.layer().setCornerRadius_(10)
        (parent if parent is not None else self.body).addSubview_(button)
        return button

    @objc.python_method
    def _update_segment_buttons(self, buttons, selected):
        for index, button in enumerate(buttons):
            button.layer().setBackgroundColor_(
                _color("sun" if index == selected else "card").CGColor()
            )

    def selectCleanupLevel_(self, sender):
        self.cleanup_selection = self.cleanup_options[sender.tag()]
        self._update_segment_buttons(self.cleanup_buttons, sender.tag())

    def selectStyle_(self, sender):
        category, value = self.style_tag_map[sender.tag()]
        self.style_selections[category] = value
        selected = self.style_options.index(value)
        self._update_segment_buttons(self.style_buttons[category], selected)

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
                0, 540, 700, 14,
            )
        )
        self._button("Change models…", "openSetup:", 0, 485, width=180)

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
