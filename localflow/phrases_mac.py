"""Native AppKit hot-phrase editor (macOS only — PyObjC).

Opened by the dispatcher in phrases_window.py (which falls back to the Tk
window when PyObjC is absent). All logic lives in PhrasesModel; this file
is the AppKit layer only.

Selector hygiene: methods that PyObjC exposes to Objective-C follow Cocoa
naming (`tableView_viewForTableColumn_row_` = tableView:viewForTableColumn:row:).
Everything else is @objc.python_method so a stray underscore can't silently
export a malformed selector — tests/test_phrases_mac_static.py enforces it.
"""

import objc
import AppKit
from AppKit import (
    NSApplication,
    NSWindow,
    NSView,
    NSViewController,
    NSSplitViewController,
    NSSplitViewItem,
    NSTableView,
    NSTableColumn,
    NSScrollView,
    NSSearchField,
    NSTextField,
    NSTextView,
    NSSwitch,
    NSButton,
    NSImage,
    NSMenu,
    NSMenuItem,
    NSAlert,
    NSTimer,
    NSToolbar,
    NSToolbarItem,
    NSStackView,
    NSLayoutConstraint,
    NSBox,
    NSColor,
    NSFont,
    NSApp,
)
from Foundation import (
    NSObject, NSMakeRect, NSIndexSet, NSRunLoop, NSDate,
    NSAttributedString,
)

from localflow.hotphrases import HOT_PHRASES_PATH, HotPhraseStore
from localflow.phrases_model import PhrasesModel

_AUTOSAVE_DELAY = 0.6
_POLL_INTERVAL = 1.5
_UNDO_BAR_SECONDS = 6.0

_EXAMPLE_TRIGGER = "review checklist"
_EXAMPLE_TEXT = (
    "Review this PR:\n"
    "- correctness and edge cases\n"
    "- tests cover the change\n"
    "- no unrelated churn\n"
    "Report findings by severity."
)

_TOOLBAR_NEW = "newPhrase"


def _label(text, font=None, color=None, wrap=False):
    field = NSTextField.labelWithString_(text)
    if font is not None:
        field.setFont_(font)
    if color is not None:
        field.setTextColor_(color)
    if wrap:
        field.setMaximumNumberOfLines_(0)
        field.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
    return field


def _first_line(text):
    return next((ln.strip() for ln in text.splitlines() if ln.strip()), "")


class PhrasesWindowController(NSObject):
    """Owns the split-view window and drives PhrasesModel.

    Test-reachable surface: new_phrase, add_example, set_fields,
    flush_autosave, select_entry, toggle_enabled, delete_current,
    undo_delete, set_search, set_test_input, poll_store; visible_triggers,
    trigger_hint_text, status_text, preview_output, empty_state_visible,
    detail_editor_hidden, undo_bar_visible. `confirm_discard` is a callable
    returning True to discard pending edits (default shows an NSAlert) so
    tests can bypass the modal.
    """

    def initWithStore_config_(self, store, config=None):
        self = self.init()
        if self is None:
            return None
        self.model = PhrasesModel(store, config)
        self.confirm_discard = self._confirm_discard_alert
        self.current_id = None
        self.query = ""
        self._draft = False
        self._fields_dirty = False
        self._entries = []
        self._undo_token = None
        self._autosave_timer = None
        self._undo_timer = None

        self._build_window()
        self._build_sidebar()
        self._build_detail()
        self._build_empty_state()
        self._build_undo_bar()
        self.reload_sidebar(keep_selection=False)
        self._update_detail_visibility()
        if self._entries:
            self.select_entry(self._entries[0]["id"])
        # else: empty state shows, no draft in progress
        self._poll_timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            _POLL_INTERVAL, self, objc.selector(self.pollStore_, signature=b"v@:@"), None, True
        )
        return self

    # ------------------------------------------------------------ window ---

    @objc.python_method
    def _build_window(self):
        style = (
            AppKit.NSWindowStyleMaskTitled
            | AppKit.NSWindowStyleMaskClosable
            | AppKit.NSWindowStyleMaskMiniaturizable
            | AppKit.NSWindowStyleMaskResizable
            | AppKit.NSWindowStyleMaskFullSizeContentView
        )
        self.window = NSWindow.alloc().initWithContentRect_styleMask_backing_defer_(
            NSMakeRect(0, 0, 900, 600), style, AppKit.NSBackingStoreBuffered, False
        )
        self.window.setTitle_("Hot Phrases")
        self.window.setMinSize_(AppKit.NSMakeSize(680, 440))
        self.window.setFrameAutosaveName_("LocalFlowHotPhrases")
        self.window.setDelegate_(self)
        self.window.setReleasedWhenClosed_(False)

        toolbar = NSToolbar.alloc().initWithIdentifier_("HotPhrasesToolbar")
        toolbar.setDelegate_(self)
        toolbar.setDisplayMode_(AppKit.NSToolbarDisplayModeIconOnly)
        self.window.setToolbar_(toolbar)
        self.window.setToolbarStyle_(AppKit.NSWindowToolbarStyleUnified)

        self.split = NSSplitViewController.alloc().init()
        self.window.setContentViewController_(self.split)

    # ----------------------------------------------------------- sidebar ---

    @objc.python_method
    def _build_sidebar(self):
        self.sidebar_vc = NSViewController.alloc().init()
        stack = NSStackView.alloc().init()
        stack.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationVertical)
        stack.setEdgeInsets_(AppKit.NSEdgeInsets(0, 8, 8, 8))
        stack.setSpacing_(6)

        self.search_field = NSSearchField.alloc().init()
        self.search_field.setPlaceholderString_("Search phrases")
        self.search_field.setDelegate_(self)
        self.search_field.setTarget_(self)
        self.search_field.setAction_(objc.selector(self.searchChanged_, signature=b"v@:@"))
        stack.addView_inGravity_(self.search_field, AppKit.NSStackViewGravityTop)

        self.table = NSTableView.alloc().init()
        col = NSTableColumn.alloc().initWithIdentifier_("phrase")
        self.table.addTableColumn_(col)
        self.table.setHeaderView_(None)
        self.table.setStyle_(AppKit.NSTableViewStyleSourceList)
        self.table.setRowHeight_(44)
        self.table.setDataSource_(self)
        self.table.setDelegate_(self)
        self.table.setUsesAutomaticRowHeights_(False)

        scroll = NSScrollView.alloc().init()
        scroll.setDocumentView_(self.table)
        scroll.setHasVerticalScroller_(True)
        scroll.setAutohidesScrollers_(True)
        scroll.setDrawsBackground_(False)
        stack.addView_inGravity_(scroll, AppKit.NSStackViewGravityTop)
        scroll.setContentHuggingPriority_forOrientation_(1, AppKit.NSLayoutConstraintOrientationVertical)

        # Pin the stack's top to the safe area so the unified titlebar's
        # traffic-light buttons don't overlap the search field.
        container = NSView.alloc().init()
        stack.setTranslatesAutoresizingMaskIntoConstraints_(False)
        container.addSubview_(stack)
        for c in (
            stack.leadingAnchor().constraintEqualToAnchor_(container.leadingAnchor()),
            stack.trailingAnchor().constraintEqualToAnchor_(container.trailingAnchor()),
            stack.topAnchor().constraintEqualToAnchor_(
                container.safeAreaLayoutGuide().topAnchor()),
            stack.bottomAnchor().constraintEqualToAnchor_(container.bottomAnchor()),
        ):
            c.setActive_(True)
        self.sidebar_vc.setView_(container)
        item = NSSplitViewItem.sidebarWithViewController_(self.sidebar_vc)
        item.setMinimumThickness_(220)
        self.split.addSplitViewItem_(item)

    @objc.python_method
    def reload_sidebar(self, keep_selection=True):
        keep = self.current_id if keep_selection else None
        self._entries = self.model.entries(self.query)
        self.table.reloadData()
        if keep is not None:
            for row, e in enumerate(self._entries):
                if e["id"] == keep:
                    self.table.selectRowIndexes_byExtendingSelection_(
                        NSIndexSet.indexSetWithIndex_(row), False
                    )
                    self.table.scrollRowToVisible_(row)
                    break

    # -- table data source / delegate --

    def numberOfRowsInTableView_(self, _table):
        if self.query and not self._entries:
            return 1  # "No matches" placeholder row
        return len(self._entries)

    def tableView_viewForTableColumn_row_(self, _table, _col, row):
        if self.query and not self._entries:
            return _label("No matches", color=NSColor.secondaryLabelColor())
        e = self._entries[row]
        enabled = bool(e.get("enabled"))
        color = NSColor.labelColor() if enabled else NSColor.tertiaryLabelColor()

        cell = NSView.alloc().init()
        trigger = _label(
            e["trigger"],
            font=NSFont.systemFontOfSize_weight_(13, AppKit.NSFontWeightSemibold),
            color=color,
        )
        trigger.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
        sub = _label(
            _first_line(e["text"]),
            font=NSFont.systemFontOfSize_(11),
            color=color if not enabled else NSColor.secondaryLabelColor(),
        )
        sub.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)

        stack = NSStackView.alloc().init()
        stack.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationVertical)
        stack.setAlignment_(AppKit.NSLayoutAttributeLeading)
        stack.setSpacing_(1)
        stack.addArrangedSubview_(trigger)
        stack.addArrangedSubview_(sub)
        stack.setTranslatesAutoresizingMaskIntoConstraints_(False)
        cell.addSubview_(stack)
        stack.leadingAnchor().constraintEqualToAnchor_constant_(
            cell.leadingAnchor(), 4).setActive_(True)
        stack.trailingAnchor().constraintEqualToAnchor_constant_(
            cell.trailingAnchor(), -4).setActive_(True)
        stack.centerYAnchor().constraintEqualToAnchor_(cell.centerYAnchor()).setActive_(True)
        if not enabled:
            off = _label("Off", font=NSFont.systemFontOfSize_(10),
                         color=NSColor.tertiaryLabelColor())
            off.setTranslatesAutoresizingMaskIntoConstraints_(False)
            cell.addSubview_(off)
            off.trailingAnchor().constraintEqualToAnchor_constant_(
                cell.trailingAnchor(), -6).setActive_(True)
            off.centerYAnchor().constraintEqualToAnchor_(cell.centerYAnchor()).setActive_(True)
        return cell

    def tableView_shouldSelectRow_(self, _table, row):
        if self.query and not self._entries:
            return False
        target = self._entries[row]["id"]
        if target == self.current_id:
            return True
        return self._leave_editor()

    def tableViewSelectionDidChange_(self, _note):
        row = self.table.selectedRow()
        if row < 0 or row >= len(self._entries):
            return
        entry = self._entries[row]
        if entry["id"] != self.current_id:
            self._load_entry(entry)

    # ------------------------------------------------------------ detail ---

    @objc.python_method
    def _build_detail(self):
        self.detail_vc = NSViewController.alloc().init()
        root = NSStackView.alloc().init()
        root.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationVertical)
        root.setAlignment_(AppKit.NSLayoutAttributeLeading)
        root.setEdgeInsets_(AppKit.NSEdgeInsets(12, 16, 12, 16))
        root.setSpacing_(6)
        root.setDetachesHiddenViews_(True)
        self.editor_root = root

        # disabled banner: a plain view with a layer background and a pinned
        # wrapping label (an NSBox stretched to fill free space).
        self.banner = NSView.alloc().init()
        self.banner.setWantsLayer_(True)
        self.banner.layer().setBackgroundColor_(
            NSColor.systemYellowColor().colorWithAlphaComponent_(0.18).CGColor())
        self.banner.layer().setCornerRadius_(6)
        banner_label = _label(
            "Hot phrases are turned off in ~/.localflow.toml (hot_phrases = false)",
            wrap=True)
        banner_label.setTranslatesAutoresizingMaskIntoConstraints_(False)
        self.banner.addSubview_(banner_label)
        NSLayoutConstraint.activateConstraints_([
            banner_label.leadingAnchor().constraintEqualToAnchor_constant_(
                self.banner.leadingAnchor(), 10),
            banner_label.trailingAnchor().constraintEqualToAnchor_constant_(
                self.banner.trailingAnchor(), -10),
            banner_label.topAnchor().constraintEqualToAnchor_constant_(
                self.banner.topAnchor(), 6),
            banner_label.bottomAnchor().constraintEqualToAnchor_constant_(
                self.banner.bottomAnchor(), -6),
        ])
        self.banner.setContentCompressionResistancePriority_forOrientation_(
            751, AppKit.NSLayoutConstraintOrientationVertical)
        self.banner.setContentHuggingPriority_forOrientation_(
            751, AppKit.NSLayoutConstraintOrientationVertical)
        self.banner.setHidden_(self.model.hot_phrases_enabled)
        root.addArrangedSubview_(self.banner)

        # trigger
        root.addArrangedSubview_(_label("When I say", font=NSFont.boldSystemFontOfSize_(13)))
        self.trigger_field = NSTextField.alloc().init()
        self.trigger_field.setPlaceholderString_("e.g. review checklist")
        self.trigger_field.setDelegate_(self)
        root.addArrangedSubview_(self.trigger_field)
        self.trigger_field.setContentHuggingPriority_forOrientation_(1, AppKit.NSLayoutConstraintOrientationHorizontal)
        self.trigger_hint_label = _label("", font=NSFont.systemFontOfSize_(11),
                                         color=NSColor.secondaryLabelColor())
        root.addArrangedSubview_(self.trigger_hint_label)

        # expansion text
        root.addArrangedSubview_(_label("Paste this", font=NSFont.boldSystemFontOfSize_(13)))
        self.text_view = NSTextView.alloc().init()
        self.text_view.setRichText_(False)
        self.text_view.setFont_(NSFont.systemFontOfSize_(13))
        self.text_view.setAutomaticQuoteSubstitutionEnabled_(False)
        self.text_view.setAutomaticDashSubstitutionEnabled_(False)
        self.text_view.setAutomaticTextReplacementEnabled_(False)
        self.text_view.setDelegate_(self)
        self.text_view.setMinSize_(AppKit.NSMakeSize(0, 0))
        self.text_view.setMaxSize_(AppKit.NSMakeSize(1e7, 1e7))
        self.text_view.setVerticallyResizable_(True)
        self.text_view.setHorizontallyResizable_(False)
        self.text_view.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        text_scroll = NSScrollView.alloc().init()
        text_scroll.setDocumentView_(self.text_view)
        text_scroll.setHasVerticalScroller_(True)
        text_scroll.setAutohidesScrollers_(True)
        text_scroll.setBorderType_(AppKit.NSLineBorder)
        text_scroll.setTranslatesAutoresizingMaskIntoConstraints_(False)
        root.addArrangedSubview_(text_scroll)
        text_scroll.setContentHuggingPriority_forOrientation_(1, AppKit.NSLayoutConstraintOrientationVertical)
        # Let the paste field give up space first so the controls below it
        # never clip when the window is short.
        text_scroll.setContentCompressionResistancePriority_forOrientation_(
            50, AppKit.NSLayoutConstraintOrientationVertical)
        # Cap the paste field so a large fitting size can't push the rows
        # below it out of view; it grows up to 120pt and shrinks to 60pt
        # when space is tight (e.g. the test area is open).
        text_height = text_scroll.heightAnchor().constraintLessThanOrEqualToConstant_(120)
        text_height.setPriority_(900)
        text_height.setActive_(True)
        text_scroll.heightAnchor().constraintGreaterThanOrEqualToConstant_(
            60).setActive_(True)
        text_scroll.widthAnchor().constraintEqualToAnchor_constant_(
            root.widthAnchor(), -32).setActive_(True)
        self.text_scroll = text_scroll
        self.text_hint_label = _label("", font=NSFont.systemFontOfSize_(11),
                                      color=NSColor.secondaryLabelColor())
        root.addArrangedSubview_(self.text_hint_label)

        # enabled row: switch + "On" + spacer + Delete
        row = NSStackView.alloc().init()
        row.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationHorizontal)
        row.setSpacing_(8)
        self.enabled_switch = NSSwitch.alloc().init()
        self.enabled_switch.setTarget_(self)
        self.enabled_switch.setAction_(objc.selector(self.switchToggled_, signature=b"v@:@"))
        row.addArrangedSubview_(self.enabled_switch)
        row.addArrangedSubview_(_label("On"))
        spacer = NSView.alloc().init()
        spacer.setContentHuggingPriority_forOrientation_(1, AppKit.NSLayoutConstraintOrientationHorizontal)
        row.addArrangedSubview_(spacer)
        self.delete_button = NSButton.alloc().init()
        self.delete_button.setTitle_("Delete")
        self.delete_button.setBezelStyle_(AppKit.NSBezelStyleAccessoryBarAction)
        self.delete_button.setAttributedTitle_(
            NSAttributedString.alloc().initWithString_attributes_(
                "Delete", {AppKit.NSForegroundColorAttributeName: NSColor.systemRedColor()}))
        self.delete_button.setTarget_(self)
        self.delete_button.setAction_(objc.selector(self.deleteClicked_, signature=b"v@:@"))
        row.addArrangedSubview_(self.delete_button)
        row.setTranslatesAutoresizingMaskIntoConstraints_(False)
        root.addArrangedSubview_(row)
        row.widthAnchor().constraintEqualToAnchor_constant_(
            root.widthAnchor(), -32).setActive_(True)

        # status
        status_row = NSStackView.alloc().init()
        status_row.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationHorizontal)
        status_row.addArrangedSubview_(NSView.alloc().init())
        self.status_label = _label("", font=NSFont.systemFontOfSize_(11),
                                   color=NSColor.secondaryLabelColor())
        status_row.addArrangedSubview_(self.status_label)
        status_row.setTranslatesAutoresizingMaskIntoConstraints_(False)
        root.addArrangedSubview_(status_row)
        status_row.widthAnchor().constraintEqualToAnchor_constant_(
            root.widthAnchor(), -32).setActive_(True)

        # "Test a phrase" disclosure: the triangle bezel is only ~13pt tall
        # and clips its own title, so the label sits next to it.
        self.disclosure = NSButton.alloc().init()
        self.disclosure.setBezelStyle_(AppKit.NSBezelStyleDisclosure)
        self.disclosure.setButtonType_(AppKit.NSButtonTypePushOnPushOff)
        self.disclosure.setTitle_("")
        self.disclosure.setTarget_(self)
        self.disclosure.setAction_(objc.selector(self.toggleTestArea_, signature=b"v@:@"))
        disc_row = NSStackView.alloc().init()
        disc_row.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationHorizontal)
        disc_row.setAlignment_(AppKit.NSLayoutAttributeCenterY)
        disc_row.addArrangedSubview_(self.disclosure)
        disc_row.addArrangedSubview_(_label("Test a phrase"))
        root.addArrangedSubview_(disc_row)

        self.test_area = NSStackView.alloc().init()
        self.test_area.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationVertical)
        self.test_area.setAlignment_(AppKit.NSLayoutAttributeLeading)
        self.test_area.setSpacing_(6)
        self.test_field = NSTextField.alloc().init()
        self.test_field.setPlaceholderString_("Say something\u2026")
        # Delegate, not just target/action: the action only fires on Return;
        # the preview must update live as you type.
        self.test_field.setDelegate_(self)
        self.test_area.addArrangedSubview_(self.test_field)
        self.test_output = NSTextView.alloc().init()
        self.test_output.setEditable_(False)
        self.test_output.setRichText_(False)
        self.test_output.setDrawsBackground_(False)
        self.test_output.setMinSize_(AppKit.NSMakeSize(0, 0))
        self.test_output.setMaxSize_(AppKit.NSMakeSize(1e7, 1e7))
        self.test_output.setVerticallyResizable_(True)
        self.test_output.setHorizontallyResizable_(False)
        self.test_output.setAutoresizingMask_(AppKit.NSViewWidthSizable)
        test_scroll = NSScrollView.alloc().init()
        test_scroll.setDocumentView_(self.test_output)
        test_scroll.setHasVerticalScroller_(True)
        test_scroll.setAutohidesScrollers_(True)
        test_scroll.setBorderType_(AppKit.NSLineBorder)
        test_scroll.heightAnchor().constraintEqualToConstant_(60).setActive_(True)
        self.test_area.addArrangedSubview_(test_scroll)
        self.test_matched_label = _label("", font=NSFont.systemFontOfSize_(11),
                                         color=NSColor.secondaryLabelColor())
        self.test_area.addArrangedSubview_(self.test_matched_label)
        self.test_field.widthAnchor().constraintEqualToAnchor_(
            self.test_area.widthAnchor()).setActive_(True)
        test_scroll.widthAnchor().constraintEqualToAnchor_(
            self.test_area.widthAnchor()).setActive_(True)
        self.test_area.setHidden_(True)
        self.test_area.setTranslatesAutoresizingMaskIntoConstraints_(False)
        root.addArrangedSubview_(self.test_area)
        self.test_area.widthAnchor().constraintEqualToAnchor_constant_(
            root.widthAnchor(), -32).setActive_(True)

        # The split item's view is a plain container; hiding editor_root
        # must not collapse the split item, so the empty state and the undo
        # bar live as siblings of the editor, not inside it.
        container = NSView.alloc().init()
        root.setTranslatesAutoresizingMaskIntoConstraints_(False)
        container.addSubview_(root)
        for anchor, const in (
            (root.leadingAnchor().constraintEqualToAnchor_(container.leadingAnchor()), None),
            (root.trailingAnchor().constraintEqualToAnchor_(container.trailingAnchor()), None),
            # safe-area top keeps the editor below the unified toolbar.
            (root.topAnchor().constraintEqualToAnchor_(
                container.safeAreaLayoutGuide().topAnchor()), None),
            (root.bottomAnchor().constraintEqualToAnchor_(container.bottomAnchor()), None),
        ):
            anchor.setActive_(True)
        self.detail_container = container
        self.detail_vc.setView_(container)
        self.split.addSplitViewItem_(NSSplitViewItem.splitViewItemWithViewController_(self.detail_vc))

    @objc.python_method
    def _build_empty_state(self):
        self.empty_view = NSStackView.alloc().init()
        self.empty_view.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationVertical)
        self.empty_view.setAlignment_(AppKit.NSLayoutAttributeCenterX)
        self.empty_view.setSpacing_(8)
        title = _label("Say a few words, paste a whole prompt",
                       font=NSFont.boldSystemFontOfSize_(15))
        subtitle = _label(
            "Create a hot phrase, then say its trigger while dictating and "
            "LocalFlow pastes your saved text at the cursor.",
            color=NSColor.secondaryLabelColor(), wrap=True)
        subtitle.setAlignment_(AppKit.NSTextAlignmentCenter)
        subtitle.setMaximumNumberOfLines_(3)
        create = NSButton.buttonWithTitle_target_action_(
            "Create Hot Phrase", self, objc.selector(self.newPhraseClicked_, signature=b"v@:@"))
        create.setKeyEquivalent_("\r")
        create.setBezelStyle_(AppKit.NSBezelStyleRounded)
        example = NSButton.buttonWithTitle_target_action_(
            "Add an example", self, objc.selector(self.addExampleClicked_, signature=b"v@:@"))
        example.setBordered_(False)
        example.setContentTintColor_(NSColor.controlAccentColor())
        self.empty_view.addArrangedSubview_(title)
        self.empty_view.addArrangedSubview_(subtitle)
        self.empty_view.addArrangedSubview_(create)
        self.empty_view.addArrangedSubview_(example)
        self.empty_view.setTranslatesAutoresizingMaskIntoConstraints_(False)
        self.detail_container.addSubview_(self.empty_view)
        self.empty_view.centerXAnchor().constraintEqualToAnchor_(
            self.detail_vc.view().centerXAnchor()).setActive_(True)
        self.empty_view.centerYAnchor().constraintEqualToAnchor_(
            self.detail_vc.view().centerYAnchor()).setActive_(True)
        self.empty_view.widthAnchor().constraintLessThanOrEqualToAnchor_constant_(
            self.detail_vc.view().widthAnchor(), -80).setActive_(True)
        self.empty_view.setHidden_(True)

    @objc.python_method
    def _build_undo_bar(self):
        self.undo_bar = NSBox.alloc().init()
        self.undo_bar.setBoxType_(AppKit.NSBoxCustom)
        self.undo_bar.setFillColor_(NSColor.controlBackgroundColor())
        self.undo_bar.setCornerRadius_(6)
        inner = NSStackView.alloc().init()
        inner.setOrientation_(AppKit.NSUserInterfaceLayoutOrientationHorizontal)
        inner.setEdgeInsets_(AppKit.NSEdgeInsets(6, 10, 6, 10))
        inner.setSpacing_(10)
        self.undo_label = _label("")
        inner.addArrangedSubview_(self.undo_label)
        undo_btn = NSButton.buttonWithTitle_target_action_(
            "Undo", self, objc.selector(self.undoDeleteClicked_, signature=b"v@:@"))
        inner.addArrangedSubview_(undo_btn)
        self.undo_bar.setContentView_(inner)
        self.undo_bar.setTranslatesAutoresizingMaskIntoConstraints_(False)
        self.detail_container.addSubview_(self.undo_bar)
        self.undo_bar.centerXAnchor().constraintEqualToAnchor_(
            self.detail_vc.view().centerXAnchor()).setActive_(True)
        self.undo_bar.bottomAnchor().constraintEqualToAnchor_constant_(
            self.detail_vc.view().bottomAnchor(), -12).setActive_(True)
        self.undo_bar.setHidden_(True)

    @objc.python_method
    def _update_detail_visibility(self):
        """Empty state iff the store is empty AND no draft is being edited;
        the editor stack is fully hidden while it shows."""
        show_empty = not self.model.entries() and self.current_id is None and not self._draft
        self.empty_view.setHidden_(not show_empty)
        self.editor_root.setHidden_(show_empty)

    # ------------------------------------------------------------- editor ---

    @objc.python_method
    def _editor_values(self):
        return (
            str(self.trigger_field.stringValue()),
            str(self.text_view.string()),
            bool(self.enabled_switch.state() == AppKit.NSControlStateValueOn),
        )

    @objc.python_method
    def _fields_match_entry(self, entry):
        trigger, text, enabled = self._editor_values()
        return (
            trigger.strip() == entry["trigger"]
            and text == entry["text"]
            and enabled == bool(entry.get("enabled"))
        )

    @objc.python_method
    def _pending_edit(self):
        """An unsaved change worth confirming: a dirty stored entry, or a
        draft with anything typed that can't be committed."""
        if self.current_id is not None:
            entry = self.model.get(self.current_id)
            return entry is not None and not self._fields_match_entry(entry)
        return bool(self.trigger_field.stringValue().strip()
                    or self.text_view.string().strip())

    @objc.python_method
    def _update_hints(self):
        trigger, text, _ = self._editor_values()
        ok, msg = self.model.trigger_hint(trigger, exclude_id=self.current_id)
        self.trigger_hint_label.setStringValue_(msg)
        self.trigger_hint_label.setTextColor_(
            NSColor.secondaryLabelColor() if ok else NSColor.systemRedColor())
        ok, msg = self.model.text_hint(text)
        self.text_hint_label.setStringValue_(msg)
        self.text_hint_label.setTextColor_(
            NSColor.secondaryLabelColor() if ok else NSColor.systemRedColor())

    @objc.python_method
    def _schedule_autosave(self):
        if self._autosave_timer is not None:
            self._autosave_timer.invalidate()
        self._autosave_timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            _AUTOSAVE_DELAY, self, objc.selector(self.autosaveFired_, signature=b"v@:@"),
            None, False
        )

    # -- delegate callbacks (real selectors) --

    def controlTextDidChange_(self, note):
        obj = note.object()
        if obj is self.test_field:
            self._render_preview()
            return
        if obj is self.search_field:
            self.searchChanged_(self.search_field)
            return
        self._fields_dirty = True
        self._update_hints()
        self._schedule_autosave()

    def textDidChange_(self, _note):
        self._fields_dirty = True
        self._update_hints()
        self._schedule_autosave()

    def searchChanged_(self, _sender):
        self.set_search(str(self.search_field.stringValue()))

    def toggleTestArea_(self, _sender):
        self.open_test_area(bool(self.test_area.isHidden()))

    @objc.python_method
    def open_test_area(self, open_):
        self.test_area.setHidden_(not open_)
        self.disclosure.setState_(
            AppKit.NSControlStateValueOn if open_ else AppKit.NSControlStateValueOff)

    def switchToggled_(self, _sender):
        self.flush_autosave()

    def autosaveFired_(self, _timer):
        self.flush_autosave()

    def newPhraseClicked_(self, _sender):
        self.new_phrase()

    def addExampleClicked_(self, _sender):
        self.add_example()

    def deleteClicked_(self, _sender):
        self.delete_current()

    def undoDeleteClicked_(self, _sender):
        self.undo_delete()

    def pollStore_(self, _timer):
        self.poll_store()

    def hideUndoBar_(self, _timer):
        self._undo_timer = None
        self.undo_bar.setHidden_(True)
        self._undo_token = None

    def fadeStatus_(self, _timer):
        self.status_label.setStringValue_("")

    # ------------------------------------------------------------- actions ---

    @objc.python_method
    def new_phrase(self):
        if not self._leave_editor():
            return
        self.current_id = None
        self._draft = True
        self.table.deselectAll_(None)
        self._set_fields("", "", True)
        self._update_detail_visibility()
        self.window.makeFirstResponder_(self.trigger_field)

    @objc.python_method
    def add_example(self):
        entry, err = self.model.commit(None, _EXAMPLE_TRIGGER, _EXAMPLE_TEXT, True)
        if err is not None:
            self._flash_status(err, error=True)
            return
        self.reload_sidebar()
        self.select_entry(entry["id"])
        self._update_detail_visibility()

    @objc.python_method
    def _set_fields(self, trigger, text, enabled):
        self.trigger_field.setStringValue_(trigger)
        self.text_view.setString_(text)
        self.enabled_switch.setState_(
            AppKit.NSControlStateValueOn if enabled else AppKit.NSControlStateValueOff)
        self._fields_dirty = False
        self._update_hints()

    @objc.python_method
    def set_fields(self, trigger, text):
        """Test hook: simulate typing and run the same change handlers."""
        self.trigger_field.setStringValue_(trigger)
        self.text_view.setString_(text)
        self._fields_dirty = True
        self._update_hints()

    @objc.python_method
    def flush_autosave(self):
        """Commit the current editor contents immediately."""
        trigger, text, enabled = self._editor_values()
        if not trigger.strip() and not text.strip() and self.current_id is None:
            self._fields_dirty = False
            return  # untouched empty draft: nothing to write
        # Invalid fields are already explained by the hint labels; the status
        # label is only for errors that have no hint (race guard, undo).
        tok, _ = self.model.trigger_hint(trigger, self.current_id)
        xt, _ = self.model.text_hint(text)
        if not (tok and xt):
            return
        entry, err = self.model.commit(self.current_id, trigger, text, enabled)
        if err is not None:
            self._flash_status(err, error=True)
            return
        self.current_id = entry["id"]
        self._draft = False
        self._fields_dirty = False
        self.reload_sidebar()
        self._update_detail_visibility()
        self._flash_status("Saved \u2713")

    @objc.python_method
    def select_entry(self, entry_id):
        if entry_id == self.current_id:
            return
        for row, e in enumerate(self._entries):
            if e["id"] == entry_id:
                if self.tableView_shouldSelectRow_(self.table, row):
                    self.table.selectRowIndexes_byExtendingSelection_(
                        NSIndexSet.indexSetWithIndex_(row), False)
                    self.table.scrollRowToVisible_(row)
                    self._load_entry(self._entries[row])
                return

    @objc.python_method
    def _load_entry(self, entry):
        self.current_id = entry["id"]
        self._draft = False
        self._set_fields(entry["trigger"], entry["text"], bool(entry.get("enabled")))
        self._flash_status("")
        self._update_detail_visibility()

    @objc.python_method
    def toggle_enabled(self):
        if self.current_id is None:
            return
        _, _, enabled = self._editor_values()
        self.model.set_enabled(self.current_id, not enabled)
        self.enabled_switch.setState_(
            AppKit.NSControlStateValueOn if not enabled else AppKit.NSControlStateValueOff)
        self.reload_sidebar()

    @objc.python_method
    def delete_current(self):
        if self.current_id is None:
            return
        deleted_id = self.current_id
        old_index = next(
            (i for i, e in enumerate(self._entries) if e["id"] == deleted_id), 0
        )
        token = self.model.delete(deleted_id)
        if token is None:
            return
        self._undo_token = token
        self.undo_label.setStringValue_(f"Deleted \u201c{token['trigger']}\u201d")
        self.undo_bar.setHidden_(False)
        if self._undo_timer is not None:
            self._undo_timer.invalidate()
        self._undo_timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            _UNDO_BAR_SECONDS, self,
            objc.selector(self.hideUndoBar_, signature=b"v@:@"), None, False)
        self.current_id = None
        self._draft = False
        self.reload_sidebar(keep_selection=False)
        # Select the neighbour at the same index (clamped); only when the
        # store is empty does the empty state come back.
        if self._entries:
            neighbour = self._entries[min(old_index, len(self._entries) - 1)]
            self.select_entry(neighbour["id"])
        else:
            self._set_fields("", "", True)
        self._update_detail_visibility()

    @objc.python_method
    def undo_delete(self):
        if self._undo_token is None:
            return
        token = self._undo_token
        self._undo_token = None
        self.undo_bar.setHidden_(True)
        entry, err = self.model.undo_delete(token)
        if err is not None:
            self._flash_status(err, error=True)
            return
        self.reload_sidebar()
        self.select_entry(entry["id"])
        self._update_detail_visibility()

    @objc.python_method
    def set_search(self, q):
        self.query = q
        if str(self.search_field.stringValue()) != q:
            self.search_field.setStringValue_(q)
        self.reload_sidebar()

    @objc.python_method
    def set_test_input(self, s):
        self.test_field.setStringValue_(s)
        self._render_preview()

    @objc.python_method
    def _render_preview(self):
        said = str(self.test_field.stringValue())
        if not said.strip():
            self.test_output.setString_("")
            self.test_matched_label.setStringValue_("")
            return
        out, matched = self.model.preview(said)
        self.test_output.setString_(out)
        if matched:
            quoted = "\u201d, \u201c".join(matched)
            self.test_matched_label.setStringValue_(f"Matched: \u201c{quoted}\u201d")
        else:
            self.test_matched_label.setStringValue_("No hot phrase matched")

    @objc.python_method
    def poll_store(self):
        if not self.model.changed_on_disk():
            return
        if not self._pending_edit():
            current = self.model.get(self.current_id) if self.current_id else None
            self.reload_sidebar()
            if current is not None:
                self._load_entry(current)
            elif self.current_id is None and not self._entries:
                self._draft = False
                self._set_fields("", "", True)
        else:
            self.reload_sidebar()
        self._update_detail_visibility()

    # ------------------------------------------------- pending-change guard ---

    @objc.python_method
    def _leave_editor(self):
        """Commit pending edits, or ask what to do about a bad change.
        Returns False when the user chose 'Keep Editing'."""
        if self._autosave_timer is not None:
            self._autosave_timer.invalidate()
            self._autosave_timer = None
        if not self._pending_edit():
            return True
        trigger, text, enabled = self._editor_values()
        entry, err = self.model.commit(self.current_id, trigger, text, enabled)
        if err is None:
            self.current_id = entry["id"]
            self._fields_dirty = False
            self.reload_sidebar()
            self._update_detail_visibility()
            self._flash_status("Saved \u2713")
            return True
        # Non-empty invalid draft or bad edit: ask.
        if self.confirm_discard(err):
            self._fields_dirty = False
            return True
        return False

    @objc.python_method
    def _confirm_discard_alert(self, error):
        alert = NSAlert.alloc().init()
        alert.setMessageText_("This change can't be saved")
        alert.setInformativeText_(error)
        alert.addButtonWithTitle_("Keep Editing")
        alert.addButtonWithTitle_("Discard")
        return alert.runModal() == AppKit.NSAlertSecondButtonReturn

    # ------------------------------------------------------------- menus ---

    def validateMenuItem_(self, item):
        action = item.action()
        if action in (b"deletePhrase:", "deletePhrase:"):
            fr = self.window.firstResponder()
            # Cmd+Delete is also delete-to-line-start in text fields; never
            # steal it while a field or text view is editing.
            if isinstance(fr, (NSTextField, NSTextView, NSSearchField)):
                return False
            return self.current_id is not None
        return True

    def deletePhrase_(self, _sender):
        self.delete_current()

    def findPhrases_(self, _sender):
        self.window.makeFirstResponder_(self.search_field)

    def newPhraseMenu_(self, _sender):
        self.new_phrase()

    def windowShouldClose_(self, _sender):
        return self._leave_editor()

    def windowWillClose_(self, _note):
        NSApp.terminate_(self)

    # ------------------------------------------------------------ helpers ---

    @objc.python_method
    def _flash_status(self, msg, error=False):
        self.status_label.setStringValue_(msg)
        self.status_label.setTextColor_(
            NSColor.systemRedColor() if error else NSColor.secondaryLabelColor())
        if msg == "Saved \u2713":
            self.status_label.setAlphaValue_(1.0)
            NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
                1.2, self, objc.selector(self.fadeStatus_, signature=b"v@:@"), None, False)

    # ----------------------------------------------------------- test hooks ---

    @objc.python_method
    def visible_triggers(self):
        if self.query and not self._entries:
            return []
        return [e["trigger"] for e in self._entries]

    @objc.python_method
    def trigger_hint_text(self):
        return str(self.trigger_hint_label.stringValue())

    @objc.python_method
    def status_text(self):
        return str(self.status_label.stringValue())

    @objc.python_method
    def preview_output(self):
        return str(self.test_output.string())

    @objc.python_method
    def empty_state_visible(self):
        return not self.empty_view.isHidden()

    @objc.python_method
    def detail_editor_hidden(self):
        return bool(self.editor_root.isHidden())

    @objc.python_method
    def undo_bar_visible(self):
        return not self.undo_bar.isHidden()

    # ------------------------------------------------------------- toolbar ---

    def toolbarAllowedItemIdentifiers_(self, _toolbar):
        return [
            AppKit.NSToolbarToggleSidebarItemIdentifier,
            _TOOLBAR_NEW,
            AppKit.NSToolbarFlexibleSpaceItemIdentifier,
        ]

    def toolbarDefaultItemIdentifiers_(self, _toolbar):
        return [
            AppKit.NSToolbarToggleSidebarItemIdentifier,
            _TOOLBAR_NEW,
            AppKit.NSToolbarFlexibleSpaceItemIdentifier,
        ]

    def toolbar_itemForItemIdentifier_willBeInsertedIntoToolbar_(
            self, _toolbar, identifier, _flag):
        if identifier == _TOOLBAR_NEW:
            item = NSToolbarItem.alloc().initWithItemIdentifier_(identifier)
            item.setImage_(
                NSImage.imageWithSystemSymbolName_accessibilityDescription_("plus", None))
            item.setToolTip_("New phrase")
            item.setTarget_(self)
            item.setAction_(objc.selector(self.newPhraseMenu_, signature=b"v@:@"))
            return item
        return None


def build_window(store, config=None) -> PhrasesWindowController:
    # Tests and the snapshot script need a shared app before creating windows.
    NSApplication.sharedApplication()
    return PhrasesWindowController.alloc().initWithStore_config_(store, config)


def _build_main_menu(controller):
    main_menu = NSMenu.alloc().init()

    app_menu_item = NSMenuItem.alloc().init()
    main_menu.addItem_(app_menu_item)
    app_menu = NSMenu.alloc().init()
    app_menu.addItemWithTitle_action_keyEquivalent_("About Hot Phrases", "orderFrontStandardAboutPanel:", "")
    app_menu.addItemWithTitle_action_keyEquivalent_("Hide", "hide:", "h")
    app_menu.addItemWithTitle_action_keyEquivalent_("Quit", "terminate:", "q")
    app_menu_item.setSubmenu_(app_menu)

    file_item = NSMenuItem.alloc().init()
    file_item.setTitle_("File")
    main_menu.addItem_(file_item)
    file_menu = NSMenu.alloc().initWithTitle_("File")
    file_menu.addItemWithTitle_action_keyEquivalent_("New Phrase", "newPhraseMenu:", "n")
    file_menu.addItemWithTitle_action_keyEquivalent_("Close Window", "performClose:", "w")
    file_item.setSubmenu_(file_menu)

    edit_item = NSMenuItem.alloc().init()
    edit_item.setTitle_("Edit")
    main_menu.addItem_(edit_item)
    edit_menu = NSMenu.alloc().initWithTitle_("Edit")
    for title, action, key in [
        ("Undo", "undo:", "z"),
        ("Redo", "redo:", "Z"),
        ("Cut", "cut:", "x"),
        ("Copy", "copy:", "c"),
        ("Paste", "paste:", "v"),
        ("Select All", "selectAll:", "a"),
    ]:
        edit_menu.addItemWithTitle_action_keyEquivalent_(title, action, key)
    edit_menu.addItem_(NSMenuItem.separatorItem())
    delete_item = edit_menu.addItemWithTitle_action_keyEquivalent_(
        "Delete Phrase", "deletePhrase:", "\x7f")
    delete_item.setKeyEquivalentModifierMask_(AppKit.NSEventModifierFlagCommand)
    edit_menu.addItemWithTitle_action_keyEquivalent_(
        "Find", "findPhrases:", "f")
    edit_item.setSubmenu_(edit_menu)

    window_item = NSMenuItem.alloc().init()
    window_item.setTitle_("Window")
    main_menu.addItem_(window_item)
    window_menu = NSMenu.alloc().initWithTitle_("Window")
    window_menu.addItemWithTitle_action_keyEquivalent_("Minimize", "performMiniaturize:", "m")
    window_item.setSubmenu_(window_menu)

    NSApp.setMainMenu_(main_menu)


def main(path=None):
    store = HotPhraseStore(path or HOT_PHRASES_PATH)
    app = NSApplication.sharedApplication()
    app.setActivationPolicy_(AppKit.NSApplicationActivationPolicyRegular)
    controller = build_window(store)
    _build_main_menu(controller)
    app.activateIgnoringOtherApps_(True)
    controller.window.makeKeyAndOrderFront_(None)
    app.run()


def snapshot_png(controller, path, dark=False):
    """Render the window to PNG without screen-recording permission."""
    name = AppKit.NSAppearanceNameDarkAqua if dark else AppKit.NSAppearanceNameAqua
    controller.window.setAppearance_(AppKit.NSAppearance.appearanceNamed_(name))
    controller.window.orderFrontRegardless()
    controller.window.displayIfNeeded()
    # Let layout and the appearance change settle before reading pixels.
    NSRunLoop.currentRunLoop().runUntilDate_(
        NSDate.dateWithTimeIntervalSinceNow_(0.2))
    view = controller.window.contentView().superview()
    if view is None:
        view = controller.window.contentView()
    view.layoutSubtreeIfNeeded()
    view.displayIfNeeded()
    bounds = view.bounds()
    rep = view.bitmapImageRepForCachingDisplayInRect_(bounds)
    view.cacheDisplayInRect_toBitmapImageRep_(bounds, rep)
    data = rep.representationUsingType_properties_(AppKit.NSBitmapImageFileTypePNG, None)
    data.writeToFile_atomically_(str(path), True)


if __name__ == "__main__":
    main()
