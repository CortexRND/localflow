"""AppKit setup wizard for macOS."""

import objc
import AppKit
from AppKit import NSApplication, NSButton, NSColor, NSFont, NSProgressIndicator, NSWindow
from Foundation import NSMakePoint, NSMakeRect, NSObject, NSTimer, NSURL

import queue
import threading
import time
import webbrowser
from dataclasses import replace

from localflow import setup_flow
from localflow.config import load_config
from localflow.models import (
    LLM_MODELS,
    DownloadCancelled,
    OllamaClient,
    available_stt_models,
    detect_hardware,
    download_stt,
    friendly_download_error,
    is_stt_downloaded,
    recommend,
)
from localflow.setup_flow import HOTKEY_OPTIONS, MicLevelMonitor, SetupChoices, initial_choices
from localflow.stt import Transcriber
from localflow.mac_style import (
    FlippedView,
    HalftoneView,
    color as _style_color,
    font as _font,
    style_button,
    style_surface,
    tokens_for_window,
)
from localflow.ui_tokens import (
    LIGHT,
    SUN_TEXT,
    TOKENS as TOKENS,
    surface_tint,
)

_STEP_TITLES = (
    "Let's get you talking.",
    "A quick setup check.",
    "Pick your speech model.",
    "Choose your cleanup model.",
    "Mic and hotkey.",
    "Give it a try.",
    "All set.",
)


def _color(name, palette=None):
    return _style_color(name, palette)


class SetupWindowController(NSObject):
    def init(self):
        self = super().init()
        if self is None:
            return None
        self.config = load_config()
        self.tokens = LIGHT
        self.hardware = detect_hardware()
        self.choices = initial_choices(self.config, self.hardware)
        self.models = available_stt_models(self.hardware)
        self.stt_id = self.choices.stt_id
        self.llm_id = self.choices.llm_id or "none"
        self.cleanup_level = self.choices.cleanup_level
        self.device = self.choices.input_device or "System default"
        self.hotkey = self.choices.hotkey
        self.pause_media = self.choices.pause_media_while_dictating
        self.language = self.choices.language
        self.start_on_login = self.choices.start_on_login
        self.step = 0
        self.saved = False
        self.snapshot_mode = False
        self.checks = None
        self.downloaded = {}
        self.ollama_running = False
        self.ollama_models = []
        self.ollama_checked = False
        self.status_message = ""
        self.error_message = ""
        self.busy = set()
        self.cancel_event = None
        self.job_token = None
        self.events = queue.Queue()
        self._clock = time.monotonic
        self._stall_timeout = 30.0
        self._last_progress_at = None
        self._last_progress_bytes = None
        self._progress_stalled = False
        self.monitor = None
        self._build_window()
        self._render()
        self.timer = NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            0.15, self, objc.selector(self.timerTick_, signature=b"v@:@"), None, True
        )
        return self

    @objc.python_method
    def _build_window(self):
        style = (
            AppKit.NSWindowStyleMaskTitled
            | AppKit.NSWindowStyleMaskClosable
            | AppKit.NSWindowStyleMaskMiniaturizable
        )
        self.window = NSWindow.alloc().initWithContentRect_styleMask_backing_defer_(
            NSMakeRect(0, 0, 800, 700), style, AppKit.NSBackingStoreBuffered, False
        )
        self.window.setTitle_("LocalFlow Setup")
        self.window.setReleasedWhenClosed_(False)
        self.window.setDelegate_(self)
        self.root = FlippedView.alloc().initWithFrame_(NSMakeRect(0, 0, 800, 670))
        self.root.setWantsLayer_(True)
        self.root.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        self.window.setContentView_(self.root)
        self.brand = self._label("localflow", 56, 22, 180, 24, 17, True)
        self.step_label = self._label("", 660, 27, 108, 16, 11, True)
        self.progress_segments = []
        for index in range(7):
            segment = AppKit.NSView.alloc().initWithFrame_(
                NSMakeRect(32 + index * 106, 62, 102, 4)
            )
            segment.setWantsLayer_(True)
            self.root.addSubview_(segment)
            self.progress_segments.append(segment)
        self.title = self._label("", 32, 86, 736, 38, 28, True)
        self.subtitle = self._label("", 32, 130, 736, 24, 13)
        self.page = FlippedView.alloc().initWithFrame_(NSMakeRect(32, 174, 736, 420))
        self.root.addSubview_(self.brand)
        mark = AppKit.NSView.alloc().initWithFrame_(NSMakeRect(32, 25, 14, 14))
        mark.setWantsLayer_(True)
        mark.layer().setBackgroundColor_(_color("sun", self.tokens).CGColor())
        mark.layer().setCornerRadius_(7)
        self.root.addSubview_(mark)
        self.root.addSubview_(self.title)
        self.root.addSubview_(self.subtitle)
        self.root.addSubview_(self.page)
        divider = AppKit.NSView.alloc().initWithFrame_(NSMakeRect(32, 612, 736, 1))
        divider.setWantsLayer_(True)
        divider.layer().setBackgroundColor_(_color("line", self.tokens).CGColor())
        self.root.addSubview_(divider)
        self.back_button = self._button("Back", 32, 628, 100, "backAction_")
        self.primary_button = self._button("Get started", 636, 624, 132, "stepAction_", True)
        self.status = self._label("", 152, 629, 450, 20, 11)
        self.root.addSubview_(self.back_button)
        self.root.addSubview_(self.primary_button)
        self.root.addSubview_(self.status)
        self.cancel_button = self._button("Cancel", 520, 624, 100, "cancelAction_")
        self.root.addSubview_(self.cancel_button)

    @objc.python_method
    def _label(self, text, x, y, width, height, size=13, bold=False):
        label = AppKit.NSTextField.labelWithString_(text)
        label.setFrame_(NSMakeRect(x, y, width, height))
        label.setFont_(_font(size, "bold" if bold else "regular"))
        label.setTextColor_(_color("ink" if bold else "muted", self.tokens))
        label.setLineBreakMode_(AppKit.NSLineBreakByWordWrapping)
        label.setMaximumNumberOfLines_(0)
        return label

    @objc.python_method
    def _button(self, title, x, y, width, action, primary=False):
        button = NSButton.alloc().initWithFrame_(NSMakeRect(x, y, width, 34))
        button.setTitle_(title)
        button.setTarget_(self)
        button.setAction_(objc.selector(getattr(self, action), signature=b"v@:@"))
        style_button(button, "primary" if primary else "secondary", self.tokens)
        return button

    @objc.python_method
    def _clear_page(self):
        for view in self.page.subviews():
            view.removeFromSuperview()

    @objc.python_method
    def _add_text(self, text, y=None, size=13, bold=False, height=26):
        if y is None:
            y = self.cursor_y
        view = self._label(text, 8, y, 710, height, size, bold)
        self.page.addSubview_(view)
        if y == self.cursor_y:
            self.cursor_y += height + 12
        return view

    @objc.python_method
    def _add_card_button(
        self, title, detail, index, y, selected=False, action="selectStt_",
        *, parent=None, tags=(), meters=None,
    ):
        card = FlippedView.alloc().initWithFrame_(NSMakeRect(0, y, 710, 76))
        style_surface(
            card, self.tokens, border="ink" if selected else "line",
            shadow=selected,
        )
        radio = AppKit.NSView.alloc().initWithFrame_(NSMakeRect(17, 28, 16, 16))
        radio.setWantsLayer_(True)
        radio.layer().setCornerRadius_(8)
        radio.layer().setBorderWidth_(2)
        radio.layer().setBorderColor_(
            _color(SUN_TEXT if selected else "ink", self.tokens).CGColor()
        )
        radio.layer().setBackgroundColor_(
            _color("sun" if selected else "surface", self.tokens).CGColor()
        )
        card.addSubview_(radio)
        if selected:
            dot = AppKit.NSView.alloc().initWithFrame_(NSMakeRect(5, 5, 6, 6))
            dot.setWantsLayer_(True)
            dot.layer().setCornerRadius_(3)
            dot.layer().setBackgroundColor_(_color(SUN_TEXT, self.tokens).CGColor())
            radio.addSubview_(dot)
        card.addSubview_(self._label(title, 46, 10, 390, 22, 15, True))
        pill_widths = [max(70, len(label) * 6 + 16) for label in tags]
        pill_x = 702 - sum(pill_widths) - 4 * max(0, len(tags) - 1)
        for label, pill_width in zip(tags, pill_widths):
            if label == "Downloaded":
                fill, text_color = (
                    surface_tint("success", self.tokens), "success",
                )
            elif label == "English only":
                fill, text_color = (
                    surface_tint("muted", self.tokens), "muted",
                )
            else:
                fill, text_color = self.tokens["sun"], SUN_TEXT
            pill = FlippedView.alloc().initWithFrame_(
                NSMakeRect(pill_x, 10, pill_width, 20)
            )
            pill.setWantsLayer_(True)
            pill.layer().setBackgroundColor_(_color(fill, self.tokens).CGColor())
            pill.layer().setCornerRadius_(10)
            pill_label = self._label(
                label, 8, 2, max(54, pill_width - 16), 16, 10, True
            )
            pill_label.setTextColor_(_color(text_color, self.tokens))
            pill.addSubview_(pill_label)
            card.addSubview_(pill)
            pill_x += pill_width + 4
        if meters:
            card.addSubview_(self._label(detail, 46, 43, 70, 18, 11))
            meter_x = 130
            for meter_name, score in meters:
                card.addSubview_(self._label(meter_name, meter_x, 43, 52, 18, 11))
                meter_x += 54
                for dot_index in range(5):
                    dot_view = AppKit.NSView.alloc().initWithFrame_(
                        NSMakeRect(meter_x + dot_index * 11, 48, 7, 7)
                    )
                    dot_view.setWantsLayer_(True)
                    dot_view.layer().setCornerRadius_(3.5)
                    dot_view.layer().setBackgroundColor_(
                        _color(
                            "ink" if dot_index < score else "line",
                            self.tokens,
                        ).CGColor()
                    )
                    card.addSubview_(dot_view)
                meter_x += 67
        else:
            card.addSubview_(self._label(detail, 46, 43, 620, 18, 11))
        button = NSButton.alloc().initWithFrame_(card.bounds())
        button.setTitle_("")
        button.setBordered_(False)
        button.setTag_(index)
        button.setTarget_(self)
        button.setAction_(objc.selector(getattr(self, action), signature=b"v@:@"))
        card.addSubview_(button)
        (parent if parent is not None else self.page).addSubview_(card)
        return button

    @objc.python_method
    def _make_scroll(self, height, frame_height=310, y=None):
        if y is None:
            y = self.cursor_y
        scroll = AppKit.NSScrollView.alloc().initWithFrame_(
            NSMakeRect(8, y, 720, frame_height)
        )
        document = FlippedView.alloc().initWithFrame_(NSMakeRect(0, 0, 700, height))
        scroll.setDocumentView_(document)
        scroll.setHasVerticalScroller_(True)
        scroll.setBorderType_(AppKit.NSNoBorder)
        self.page.addSubview_(scroll)
        if y == self.cursor_y:
            self.cursor_y += frame_height + 12
        return document

    @objc.python_method
    def _add_cleanup_level_control(self):
        self._add_text("Cleanup level", size=13, bold=True)
        self.cleanup_level_buttons = []
        levels = (("none", "None"), ("light", "Light"), ("medium", "Medium"))
        group = FlippedView.alloc().initWithFrame_(
            NSMakeRect(8, self.cursor_y, 306, 36)
        )
        group.setWantsLayer_(True)
        group.layer().setBorderColor_(_color("line", self.tokens).CGColor())
        group.layer().setBorderWidth_(1)
        group.layer().setCornerRadius_(10)
        for index, (level, title) in enumerate(levels):
            button = self._button(
                title, index * 102, 1, 102, "cleanupLevelAction_"
            )
            button.setTag_(index)
            style_button(
                button,
                "selected" if self.cleanup_level == level else "secondary",
                self.tokens,
            )
            group.addSubview_(button)
            self.cleanup_level_buttons.append(button)
        self.page.addSubview_(group)
        self.cursor_y += 48

    @objc.python_method
    def _render_legacy(self):
        self._clear_page()
        self.title.setStringValue_(_STEP_TITLES[self.step])
        self.subtitle.setStringValue_(f"Step {self.step + 1} of 7")
        self.back_button.setHidden_(self.step == 0 or self.saved)
        model_busy = bool(self.busy & {"stt-download", "llm-pull"})
        self.back_button.setEnabled_(not model_busy)
        self.primary_button.setHidden_(False)
        self.status.setStringValue_(self.error_message or self.status_message)
        self.status.setTextColor_(_color("red" if self.error_message else "muted"))
        self.primary_button.setEnabled_(not bool(self.busy))
        self.cancel_button.setHidden_(
            self.cancel_event is None or self.step not in (2, 3)
        )

        if self.step == 0:
            stt_id, llm_id = recommend(self.hardware)
            self._add_text(
                f"{self.hardware.ram_gb:.0f} GB memory  ·  {self.hardware.free_disk_gb:.0f} GB free disk"
                f"  ·  {'Apple Silicon' if self.hardware.apple_silicon else 'CPU'}",
                240, 15, True,
            )
            self._add_text("A local voice setup, ready in a few minutes.", 190, 18, True)
            self._add_text(f"Suggested speech model: {stt_id}   ·   Cleanup: {llm_id or 'off'}", 155)
            self._add_text("Your audio and models stay on this machine.", 120)
        elif self.step == 1:
            self._add_text("Nothing here blocks setup. Fix what you can; you can revisit any time.", 270)
            if self.checks is None:
                self._add_text("Checking this machine…", 220)
                if not self.snapshot_mode:
                    self._start_checks()
            else:
                for index, check in enumerate(self.checks):
                    badge = "✓" if check.ok else ("?" if check.ok is None else "✗")
                    self._add_text(f"{badge}  {check.label}  ·  {check.detail}", 230 - index * 34, 12)
                    if check.fix_label:
                        button = self._button(check.fix_label, 480, 226 - index * 34, 150, "fixAction_")
                        button.setTag_(index)
                        self.page.addSubview_(button)
                self.page.addSubview_(self._button("Recheck", 8, 16, 120, "recheckAction_"))
        elif self.step == 2:
            self._add_text("Choose the speech model that turns your voice into text.", 278)
            rec, _ = recommend(self.hardware)
            document = self._make_scroll(max(310, len(self.models) * 52))
            for index, model in enumerate(self.models):
                title = model.label
                if model.id == rec:
                    title += "  ·  RECOMMENDED"
                if self.downloaded.get(model.id):
                    title += "  ·  DOWNLOADED"
                if model.english_only:
                    title += "  ·  ENGLISH ONLY"
                detail = (
                    f"{model.size_mb:,} MB  ·  speed {model.speed}/5  ·  accuracy {model.accuracy}/5"
                )
                button = self._add_card_button(
                    title, detail, index, max(0, max(310, len(self.models) * 52) - (index + 1) * 52),
                    self.stt_id == model.id,
                )
                button.removeFromSuperview()
                document.addSubview_(button)
                button.setFrameOrigin_(NSMakePoint(8, max(0, max(310, len(self.models) * 52) - (index + 1) * 52)))
            if not self.snapshot_mode and not self.downloaded and "stt-checks" not in self.busy:
                self._start_download_checks()
        elif self.step == 3 and self.choices.llm_provider != "ollama":
            self._add_text(
                f"Configured provider: {self.choices.llm_provider}. Manage models with this provider in Settings.",
                278,
            )
            self._add_cleanup_level_control()
            self.page.addSubview_(
                self._button("Change in Settings", 8, 152, 190, "settingsAction_")
            )
        elif self.step == 3:
            self._add_text("Cleanup fixes punctuation and filler words; raw transcript skips the extra model.", 278)
            self._add_cleanup_level_control()
            count = len(LLM_MODELS) + 1
            document = self._make_scroll(max(310, count * 52), frame_height=160)
            options = [("none", "No cleanup (raw transcript)", "Fastest; no Ollama required.")]
            options.extend((model.id, model.label, f"{model.size_mb:,} MB  ·  {model.min_ram_gb} GB RAM recommended")
                           for model in LLM_MODELS)
            for index, (model_id, label, detail) in enumerate(options):
                if model_id in self.ollama_models:
                    label += "  ·  DOWNLOADED"
                button = self._add_card_button(
                    label, detail, index, max(0, count * 52 - (index + 1) * 52),
                    self.llm_id == model_id, "selectLlm_",
                )
                button.removeFromSuperview()
                document.addSubview_(button)
                button.setFrameOrigin_(NSMakePoint(8, max(0, count * 52 - (index + 1) * 52)))
            if not self.snapshot_mode and not self.ollama_checked:
                self._start_ollama_check()
            if not self.ollama_running:
                self._add_text("Ollama is not running. Cleanup is optional.", 8)
                installed = setup_flow.ollama_installed()
                action = "recheckAction_" if installed else "ollamaDownloadAction_"
                title = "Recheck Ollama" if installed else "Get Ollama"
                button = self._button(title, 8, 8, 150, action)
                self.page.addSubview_(button)
            else:
                button = self._button("Refresh models", 250, 198, 150, "recheckAction_")
                self.page.addSubview_(button)
        elif self.step == 4:
            self._add_text("Pick a microphone and the key you'll hold to talk.", 278)
            self.device_popup = AppKit.NSPopUpButton.alloc().initWithFrame_(NSMakeRect(8, 208, 620, 30))
            self.device_popup.addItemWithTitle_("System default")
            self.device_popup.lastItem().setRepresentedObject_("")
            for device in setup_flow.list_input_devices():
                self.device_popup.addItemWithTitle_(device)
                self.device_popup.lastItem().setRepresentedObject_(device)
            for index in range(self.device_popup.numberOfItems()):
                represented = self.device_popup.itemAtIndex_(index).representedObject()
                if represented == ("" if self.device == "System default" else self.device):
                    self.device_popup.selectItemAtIndex_(index)
            self.device_popup.setTarget_(self)
            self.device_popup.setAction_(objc.selector(self.deviceChanged_, signature=b"v@:@"))
            self.page.addSubview_(self.device_popup)
            self.hotkey_popup = AppKit.NSPopUpButton.alloc().initWithFrame_(NSMakeRect(8, 152, 620, 30))
            for value, label in HOTKEY_OPTIONS:
                self.hotkey_popup.addItemWithTitle_(label)
                self.hotkey_popup.lastItem().setRepresentedObject_(value)
            for index in range(self.hotkey_popup.numberOfItems()):
                if self.hotkey_popup.itemAtIndex_(index).representedObject() == self.hotkey:
                    self.hotkey_popup.selectItemAtIndex_(index)
            self.hotkey_popup.setTarget_(self)
            self.hotkey_popup.setAction_(objc.selector(self.hotkeyChanged_, signature=b"v@:@"))
            self.page.addSubview_(self.hotkey_popup)
            shortcut_text = (
                f"Hold {self.hotkey} · Double-tap locks · Esc cancels\n"
                f"Paste last {self.choices.paste_last_hotkey} · Command {self.choices.command_hotkey}"
                f" · Polish {self.choices.polish_hotkey} · Prompt {self.choices.prompt_hotkey}"
            )
            shortcut_view = self._label(shortcut_text, 8, 44, 632, 38, 9)
            self.page.addSubview_(shortcut_view)
            self.pause_switch = NSButton.alloc().initWithFrame_(NSMakeRect(8, 8, 300, 24))
            self.pause_switch.setButtonType_(AppKit.NSSwitchButton)
            self.pause_switch.setTitle_("Pause music while I talk")
            self.pause_switch.setState_(AppKit.NSOnState if self.pause_media else AppKit.NSOffState)
            self.pause_switch.setTarget_(self)
            self.pause_switch.setAction_(
                objc.selector(self.pauseMediaChanged_, signature=b"v@:@")
            )
            self.page.addSubview_(self.pause_switch)
            self._add_text("Live mic level", 116, 12, True)
            self.meter = NSProgressIndicator.alloc().initWithFrame_(NSMakeRect(8, 88, 620, 18))
            self.meter.setIndeterminate_(False)
            self.meter.setMinValue_(0)
            self.meter.setMaxValue_(1)
            self.page.addSubview_(self.meter)
            if not self.snapshot_mode:
                self._start_monitor()
        elif self.step == 5:
            self._add_text("Record five seconds of speech. Your selected speech model will transcribe it.", 240)
            self._add_text(self.status_message or "Try a short phrase near your microphone.", 184, 14)
            if self.error_message:
                self._add_text(self.error_message, 135, 12)
            skip = self._button("Skip", 12, 16, 110, "skipAction_")
            self.page.addSubview_(skip)
        elif self.step == 6:
            self._add_text(f"Speech: {self.stt_id}  ·  Cleanup: {self.llm_id if self.llm_id != 'none' else 'off'}", 240, 14, True)
            self._add_text(f"Microphone: {self.device}  ·  Hotkey: {self.hotkey}", 204)
            if self.hardware.system == "Darwin":
                self.login_switch = NSButton.alloc().initWithFrame_(NSMakeRect(8, 152, 300, 28))
                self.login_switch.setButtonType_(AppKit.NSSwitchButton)
                self.login_switch.setTitle_("Start LocalFlow on login")
                self.login_switch.setState_(AppKit.NSOnState if self.start_on_login else AppKit.NSOffState)
                self.login_switch.setTarget_(self)
                self.login_switch.setAction_(objc.selector(self.loginChanged_, signature=b"v@:@"))
                self.page.addSubview_(self.login_switch)
            if self.saved:
                self._add_text("You're all set. Run `localflow` whenever you're ready.", 110, 14, True)
                self.page.addSubview_(self._button("Launch localflow now", 8, 64, 190, "launchAction_"))

        if self.step == 2 and self.status_message:
            self.status.setStringValue_(self.error_message or self.status_message)
        if self.saved:
            self.primary_button.setTitle_("Close")
        elif self.step == 2:
            self.primary_button.setTitle_("Continue" if self.downloaded.get(self.stt_id) else "Download & continue")
        elif self.step == 3:
            if self.choices.llm_provider != "ollama":
                self.primary_button.setTitle_("Continue")
            else:
                self.primary_button.setTitle_("Continue" if self.llm_id == "none" or not self.ollama_running or self.llm_id in self.ollama_models else "Download & continue")
        elif self.step == 5:
            self.primary_button.setTitle_("Record 5 seconds")
        elif self.step == 6:
            self.primary_button.setTitle_("Finish")
        else:
            self.primary_button.setTitle_("Get started" if self.step == 0 else "Continue")
        self.window.center()
        self.window.makeKeyAndOrderFront_(None)

    @objc.python_method
    def _render(self):
        self.tokens = tokens_for_window(self.window)
        self.root.layer().setBackgroundColor_(_color("paper", self.tokens).CGColor())
        style_button(self.back_button, "secondary", self.tokens)
        style_button(self.primary_button, "primary", self.tokens)
        style_button(self.cancel_button, "secondary", self.tokens)
        subtitles = (
            "Choose a local voice setup that fits your machine.",
            "Check that audio and model services are ready.",
            "Pick the speech model that transcribes your voice.",
            "Optional cleanup keeps your transcript clear and readable.",
            "Choose how LocalFlow listens and responds.",
            "Try a short dictation before you finish.",
            "Review your choices and start dictating.",
        )
        self.title.setStringValue_(_STEP_TITLES[self.step])
        self.subtitle.setStringValue_(subtitles[self.step])
        self.step_label.setStringValue_(f"STEP {self.step + 1} OF 7")
        for index, segment in enumerate(self.progress_segments):
            segment.layer().setBackgroundColor_(
                _color("cobalt" if index <= self.step else "line", self.tokens).CGColor()
            )
        self.back_button.setHidden_(self.step == 0 or self.saved)
        self.back_button.setEnabled_(not bool(self.busy & {"stt-download", "llm-pull"}))
        self.primary_button.setEnabled_(not bool(self.busy))
        self.cancel_button.setHidden_(
            self.cancel_event is None or self.step not in (2, 3)
        )
        self.status.setStringValue_("")
        self._clear_page()
        self.cursor_y = 8

        if self.step == 0:
            stt_id, llm_id = recommend(self.hardware)
            self.page.addSubview_(
                HalftoneView.alloc().initWithFrame_palette_(
                    NSMakeRect(540, 0, 180, 160), self.tokens
                )
            )
            self._add_text(
                "A local voice setup, ready in a few minutes.",
                size=20, bold=True, height=34,
            )
            self._add_text(
                f"{self.hardware.ram_gb:.0f} GB memory  ·  "
                f"{self.hardware.free_disk_gb:.0f} GB free disk  ·  "
                f"{'Apple Silicon' if self.hardware.apple_silicon else 'CPU'}",
            )
            self._add_text(f"Suggested speech model: {stt_id}  ·  Cleanup: {llm_id or 'off'}")
            self._add_text("Your audio and models stay on this machine.")
        elif self.step == 1:
            self._add_text("Nothing here blocks setup. Fix what you can; revisit any time.")
            if self.checks is None:
                self._add_text("Checking this machine…")
                if not self.snapshot_mode:
                    self._start_checks()
            else:
                for index, check in enumerate(self.checks):
                    color_name = "success" if check.ok else ("muted" if check.ok is None else "red")
                    self.page.addSubview_(
                        self._label(
                            f"{check.label}  ·  {check.detail}",
                            8, self.cursor_y, 510, 28, 13,
                        )
                    )
                    status = self._label(
                        "Ready" if check.ok else ("Check" if check.ok is None else "Needs attention"),
                        526, self.cursor_y, 150, 28, 11, True,
                    )
                    status.setTextColor_(_color(color_name, self.tokens))
                    self.page.addSubview_(status)
                    if check.fix_label:
                        button = self._button(
                            check.fix_label, 560, self.cursor_y, 150, "fixAction_"
                        )
                        button.setTag_(index)
                        self.page.addSubview_(button)
                    self.cursor_y += 38
                self.page.addSubview_(
                    self._button("Recheck", 8, self.cursor_y, 120, "recheckAction_")
                )
        elif self.step == 2:
            self._add_text("Choose the speech model that turns your voice into text.")
            self._add_text("Models stay on this machine. Size is shown before download.", size=11)
            if self.status_message or self.error_message:
                self._add_callout()
            rec, _ = recommend(self.hardware)
            document = self._make_scroll(
                max(240, len(self.models) * 84), frame_height=272,
            )
            for index, model in enumerate(self.models):
                tags = []
                if model.id == rec:
                    tags.append("Recommended")
                if self.downloaded.get(model.id):
                    tags.append("Downloaded")
                if model.english_only:
                    tags.append("English only")
                size = (
                    f"{model.size_mb / 1000:.1f} GB"
                    if model.size_mb >= 1000 else f"{model.size_mb:,} MB"
                )
                self._add_card_button(
                    model.label, size, index, index * 84,
                    self.stt_id == model.id, parent=document, tags=tags,
                    meters=(("Speed", model.speed), ("Accuracy", model.accuracy)),
                )
            if not self.snapshot_mode and not self.downloaded and "stt-checks" not in self.busy:
                self._start_download_checks()
        elif self.step == 3:
            self._add_text(
                "Cleanup fixes punctuation and filler words; raw transcript skips the extra model."
            )
            self._add_cleanup_level_control()
            if self.choices.llm_provider != "ollama":
                self._add_text(
                    f"Configured provider: {self.choices.llm_provider}. "
                    "Manage models with this provider in Settings."
                )
                self.page.addSubview_(
                    self._button(
                        "Change in Settings", 8, self.cursor_y, 190, "settingsAction_"
                    )
                )
            else:
                options = [
                    ("none", "No cleanup (raw transcript)", "Fastest; no Ollama required.")
                ]
                options.extend(
                    (
                        model.id,
                        model.label,
                        f"{model.size_mb:,} MB  ·  {model.min_ram_gb} GB RAM recommended",
                    )
                    for model in LLM_MODELS
                )
                document = self._make_scroll(
                    max(240, len(options) * 84), frame_height=182,
                )
                for index, (model_id, label, detail) in enumerate(options):
                    tags = ["Downloaded"] if model_id in self.ollama_models else []
                    self._add_card_button(
                        label, detail, index, index * 84,
                        self.llm_id == model_id, "selectLlm_",
                        parent=document, tags=tags,
                    )
                if not self.snapshot_mode and not self.ollama_checked:
                    self._start_ollama_check()
                if not self.ollama_running:
                    self._add_text("Ollama is not running. Cleanup is optional.")
                    installed = setup_flow.ollama_installed()
                    action = "recheckAction_" if installed else "ollamaDownloadAction_"
                    title = "Recheck Ollama" if installed else "Get Ollama"
                    self.page.addSubview_(
                        self._button(title, 8, self.cursor_y, 150, action)
                    )
                else:
                    self.page.addSubview_(
                        self._button(
                            "Refresh models", 8, self.cursor_y, 150, "recheckAction_"
                        )
                    )
        elif self.step == 4:
            self._add_text("Pick a microphone and the key you'll hold to talk.")
            self.page.addSubview_(
                self._label("Microphone", 8, self.cursor_y, 200, 20, 11, True)
            )
            self.device_popup = AppKit.NSPopUpButton.alloc().initWithFrame_(
                NSMakeRect(8, self.cursor_y + 22, 710, 30)
            )
            self.device_popup.addItemWithTitle_("System default")
            self.device_popup.lastItem().setRepresentedObject_("")
            for device in setup_flow.list_input_devices():
                self.device_popup.addItemWithTitle_(device)
                self.device_popup.lastItem().setRepresentedObject_(device)
            for index in range(self.device_popup.numberOfItems()):
                represented = self.device_popup.itemAtIndex_(index).representedObject()
                if represented == ("" if self.device == "System default" else self.device):
                    self.device_popup.selectItemAtIndex_(index)
            self.device_popup.setTarget_(self)
            self.device_popup.setAction_(objc.selector(self.deviceChanged_, signature=b"v@:@"))
            self.page.addSubview_(self.device_popup)
            self.cursor_y += 66
            self.page.addSubview_(
                self._label("Hotkey", 8, self.cursor_y, 200, 20, 11, True)
            )
            self.hotkey_popup = AppKit.NSPopUpButton.alloc().initWithFrame_(
                NSMakeRect(8, self.cursor_y + 22, 710, 30)
            )
            for value, label in HOTKEY_OPTIONS:
                self.hotkey_popup.addItemWithTitle_(label)
                self.hotkey_popup.lastItem().setRepresentedObject_(value)
            for index in range(self.hotkey_popup.numberOfItems()):
                if self.hotkey_popup.itemAtIndex_(index).representedObject() == self.hotkey:
                    self.hotkey_popup.selectItemAtIndex_(index)
            self.hotkey_popup.setTarget_(self)
            self.hotkey_popup.setAction_(objc.selector(self.hotkeyChanged_, signature=b"v@:@"))
            self.page.addSubview_(self.hotkey_popup)
            self.cursor_y += 64
            shortcut_card = FlippedView.alloc().initWithFrame_(
                NSMakeRect(8, self.cursor_y, 710, 66)
            )
            style_surface(shortcut_card, self.tokens)
            shortcut_card.addSubview_(
                self._label(
                    f"Hold {self.hotkey}  ·  Double-tap locks  ·  Esc cancels\n"
                    f"Paste last {self.choices.paste_last_hotkey or 'disabled'}  ·  "
                    f"Command {self.choices.command_hotkey or 'disabled'}  ·  "
                    f"Polish {self.choices.polish_hotkey or 'disabled'}  ·  "
                    f"Prompt {self.choices.prompt_hotkey or 'disabled'}",
                    16, 12, 678, 44, 11,
                )
            )
            self.page.addSubview_(shortcut_card)
            self.cursor_y += 76
            self.pause_switch = NSButton.alloc().initWithFrame_(
                NSMakeRect(8, self.cursor_y, 320, 24)
            )
            self.pause_switch.setButtonType_(AppKit.NSSwitchButton)
            self.pause_switch.setTitle_("Pause music while I talk")
            self.pause_switch.setState_(
                AppKit.NSOnState if self.pause_media else AppKit.NSOffState
            )
            self.pause_switch.setTarget_(self)
            self.pause_switch.setAction_(
                objc.selector(self.pauseMediaChanged_, signature=b"v@:@")
            )
            self.page.addSubview_(self.pause_switch)
            self.cursor_y += 34
            self._add_text("Live mic level", size=11, bold=True)
            self.meter = NSProgressIndicator.alloc().initWithFrame_(
                NSMakeRect(8, self.cursor_y, 710, 18)
            )
            self.meter.setIndeterminate_(False)
            self.meter.setMinValue_(0)
            self.meter.setMaxValue_(1)
            self.page.addSubview_(self.meter)
            if not self.snapshot_mode:
                self._start_monitor()
        elif self.step == 5:
            self._add_text(
                "Record five seconds of speech. Your selected speech model will transcribe it.",
                size=15, height=40,
            )
            self._add_text(
                self.status_message or "Try a short phrase near your microphone.",
                size=13, height=30,
            )
            if self.error_message:
                self._add_callout()
            self.page.addSubview_(
                self._button("Skip", 8, self.cursor_y, 110, "skipAction_")
            )
        else:
            self._add_text(
                f"Speech: {self.stt_id}  ·  Cleanup: "
                f"{self.llm_id if self.llm_id != 'none' else 'off'}",
                size=15, bold=True,
            )
            self._add_text(f"Microphone: {self.device}  ·  Hotkey: {self.hotkey}")
            if self.hardware.system == "Darwin":
                self.login_switch = NSButton.alloc().initWithFrame_(
                    NSMakeRect(8, self.cursor_y, 320, 28)
                )
                self.login_switch.setButtonType_(AppKit.NSSwitchButton)
                self.login_switch.setTitle_("Start LocalFlow on login")
                self.login_switch.setState_(
                    AppKit.NSOnState if self.start_on_login else AppKit.NSOffState
                )
                self.login_switch.setTarget_(self)
                self.login_switch.setAction_(
                    objc.selector(self.loginChanged_, signature=b"v@:@")
                )
                self.page.addSubview_(self.login_switch)
                self.cursor_y += 36
            if self.saved:
                self._add_text("You're all set. Run `localflow` whenever you're ready.")
                self.page.addSubview_(
                    self._button(
                        "Launch localflow now", 8, self.cursor_y, 190, "launchAction_"
                    )
                )

        if self.step in (2, 3) and (self.error_message or self.status_message):
            self.status.setStringValue_(self.error_message or self.status_message)
        if self.saved:
            self.primary_button.setTitle_("Close")
        elif self.step == 2:
            self.primary_button.setTitle_(
                "Continue" if self.downloaded.get(self.stt_id) else "Download & continue"
            )
        elif self.step == 3:
            self.primary_button.setTitle_(
                "Continue" if self.choices.llm_provider != "ollama"
                or self.llm_id == "none" or not self.ollama_running
                or self.llm_id in self.ollama_models else "Download & continue"
            )
        elif self.step == 5:
            self.primary_button.setTitle_(
                "Recording…" if "trial" in self.busy else "Record 5 seconds"
            )
        elif self.step == 6:
            self.primary_button.setTitle_("Finish")
        else:
            self.primary_button.setTitle_("Get started" if self.step == 0 else "Continue")
        self.window.makeKeyAndOrderFront_(None)

    @objc.python_method
    def _add_callout(self):
        error = bool(self.error_message)
        box = FlippedView.alloc().initWithFrame_(
            NSMakeRect(8, self.cursor_y, 710, 58)
        )
        style_surface(box, self.tokens)
        bar = AppKit.NSView.alloc().initWithFrame_(NSMakeRect(0, 0, 4, 58))
        bar.setWantsLayer_(True)
        bar.layer().setBackgroundColor_(
            _color("red" if error else "cobalt", self.tokens).CGColor()
        )
        box.addSubview_(bar)
        box.addSubview_(
            self._label(
                "Download error" if error else "Download progress",
                16, 8, 670, 18, 13, True,
            )
        )
        box.addSubview_(
            self._label(
                self.error_message or self.status_message,
                16, 30, 670, 18, 11,
            )
        )
        self.page.addSubview_(box)
        self.cursor_y += 70

    def windowDidChangeEffectiveAppearance_(self, _notification):
        self._render()

    @objc.python_method
    def _start_worker(self, key, function, model_id=None, job_token=None):
        self.busy.add(key)
        self.error_message = ""
        if model_id is not None:
            self._render()

        def run():
            try:
                result = function()
            except Exception as exc:
                if job_token is not None:
                    self.events.put(("error", key, job_token, model_id, exc))
                elif model_id is not None:
                    self.events.put(("error", key, model_id, exc))
                else:
                    self.events.put(("error", key, exc))
            else:
                if job_token is not None:
                    self.events.put(("done", key, job_token, model_id, result))
                elif model_id is not None:
                    self.events.put(("done", key, model_id, result))
                else:
                    self.events.put(("done", key, result))

        threading.Thread(target=run, name=f"setup-{key}", daemon=True).start()
        self.primary_button.setEnabled_(False)

    @objc.python_method
    def _start_checks(self):
        if "checks" not in self.busy:
            self._start_worker("checks", lambda: setup_flow.run_checks(self.config))

    @objc.python_method
    def _start_download_checks(self):
        self._start_worker(
            "stt-checks",
            lambda: {model.id: is_stt_downloaded(model) for model in self.models},
        )

    @objc.python_method
    def _start_ollama_check(self):
        if self.ollama_checked or self.choices.llm_provider != "ollama":
            return
        self.ollama_checked = True

        def check():
            client = OllamaClient(self.config.llm_base_url)
            return (True, client.installed()) if client.is_running() else (False, [])
        self._start_worker("ollama-check", check)

    @objc.python_method
    def _start_monitor(self):
        if self.monitor is None:
            self.monitor = MicLevelMonitor("" if self.device == "System default" else self.device)
            self._start_worker("monitor", self.monitor.start)

    @objc.python_method
    def _start_download(self):
        model = next(item for item in self.models if item.id == self.stt_id)
        self.cancel_event = threading.Event()
        token = self.job_token = object()
        self.error_message = ""
        self.status_message = "Preparing download…"
        self._last_progress_at = self._clock()
        self._last_progress_bytes = 0
        self._progress_stalled = False
        cancel = self.cancel_event

        def progress(done, total):
            self.events.put(("progress", "stt-download", token, done, total))

        self._start_worker(
            "stt-download",
            lambda: download_stt(model, progress, cancel),
            model_id=model.id,
            job_token=token,
        )

    @objc.python_method
    def _start_pull(self):
        model_id = self.llm_id
        self.cancel_event = threading.Event()
        token = self.job_token = object()
        self.error_message = ""
        self.status_message = "Preparing Ollama download…"
        self._last_progress_at = self._clock()
        self._last_progress_bytes = 0
        self._progress_stalled = False
        cancel = self.cancel_event
        client = OllamaClient(self.config.llm_base_url)

        def pull():
            client.pull(
                model_id,
                lambda done, total, status: self.events.put(
                    ("progress", "llm-pull", token, done, total, status)
                ),
                cancel,
            )
            return model_id
        self._start_worker("llm-pull", pull, model_id=model_id, job_token=token)

    @objc.python_method
    def _start_trial(self):
        model = next(item for item in self.models if item.id == self.stt_id)
        language = self.language
        device = "" if self.device == "System default" else self.device
        self.status_message = "Loading speech model…"

        def trial():
            transcriber = Transcriber(model_size=model.model_size, language=language, backend=model.backend)
            return setup_flow.try_dictation(5, device, transcriber)
        self._start_worker("trial", trial)

    @objc.python_method
    def _advance(self):
        if self.step == 4:
            self.choices = replace(
                self.choices,
                stt_id=self.stt_id,
                llm_id=None if self.llm_id == "none" else self.llm_id,
                input_device="" if self.device == "System default" else self.device,
                hotkey=self.hotkey,
                language=self.language,
                start_on_login=self.start_on_login,
                pause_media_while_dictating=self.pause_media,
            )
        if self.step == 4 and self.monitor is not None:
            self.monitor.stop()
            self.monitor = None
        self.step = min(6, self.step + 1)
        self.status_message = ""
        self.error_message = ""
        self._render()

    @objc.python_method
    def _handle_events(self):
        while True:
            try:
                kind, key, *data = self.events.get_nowait()
            except queue.Empty:
                self._check_download_stall()
                return
            model_job = key in {"stt-download", "llm-pull"}
            if model_job:
                token, *data = data
                if token is not self.job_token:
                    if kind == "done":
                        model_id, _result = data
                        if key == "stt-download":
                            self.downloaded[model_id] = True
                        elif model_id not in self.ollama_models:
                            self.ollama_models.append(model_id)
                    continue
            if kind == "progress":
                done, total, *rest = data
                if model_job:
                    if done != self._last_progress_bytes:
                        self._last_progress_bytes = done
                        self._last_progress_at = self._clock()
                        self._progress_stalled = False
                    elif self._progress_stalled:
                        continue
                self.status_message = (
                    f"{done / 1_000_000:.0f} / {total / 1_000_000:.0f} MB"
                    + (f"  ·  {rest[0]}" if rest and rest[0] else "")
                )
            elif kind == "error":
                model_id, error = (
                    data if key in {"stt-download", "llm-pull"} else (None, data[0])
                )
                self.busy.discard(key)
                if model_job:
                    self.job_token = None
                self.cancel_event = None
                if isinstance(error, DownloadCancelled):
                    self.status_message = "Download cancelled."
                else:
                    self.error_message = (
                        friendly_download_error(error) if model_job
                        else str(error) or error.__class__.__name__
                    )
                if key == "checks":
                    self.checks = []
                if key == "stt-checks":
                    self.downloaded = {model.id: False for model in self.models}
                if key == "ollama-check":
                    self.ollama_checked = True
            else:
                model_id, result = (
                    data if key in {"stt-download", "llm-pull"} else (None, data[0])
                )
                self.busy.discard(key)
                if model_job:
                    self.job_token = None
                self.cancel_event = None
                if key == "checks":
                    self.checks = result
                elif key == "stt-checks":
                    self.downloaded.update(result)
                elif key == "ollama-check":
                    self.ollama_running, self.ollama_models = result
                    self.ollama_checked = True
                elif key == "stt-download":
                    self.downloaded[model_id] = True
                    self.status_message = "Speech model downloaded."
                    if self.step == 2 and self.stt_id == model_id:
                        self._advance()
                elif key == "llm-pull":
                    if model_id not in self.ollama_models:
                        self.ollama_models.append(model_id)
                    self.status_message = f"Downloaded {model_id}"
                    if self.step == 3 and self.llm_id == model_id:
                        self._advance()
                elif key == "trial":
                    self.status_message = result or "No speech detected. Try again near your microphone."
                elif key == "apply":
                    self.saved = True
                    self.status_message = str(result)
                elif key == "monitor":
                    self.status_message = ""
            self._render()

    @objc.python_method
    def _check_download_stall(self):
        if not self.busy.intersection({"stt-download", "llm-pull"}):
            return
        if self._last_progress_at is None or self._progress_stalled:
            return
        if self._clock() - self._last_progress_at < self._stall_timeout:
            return
        self._progress_stalled = True
        self.status_message = "No progress for 30s — check your connection, or Cancel."
        self.error_message = ""
        self._render()

    @objc.python_method
    def _fix_check(self, index):
        if not self.checks or index >= len(self.checks):
            return
        item = self.checks[index]
        if item.fix_url.startswith(("https://", "http://", "x-apple.systempreferences:")):
            AppKit.NSWorkspace.sharedWorkspace().openURL_(NSURL.URLWithString_(item.fix_url))
        elif item.fix_url:
            alert = AppKit.NSAlert.alloc().init()
            alert.setMessageText_("Run this command")
            alert.setInformativeText_(item.fix_url)
            alert.runModal()

    @objc.python_method
    def _select_stt(self, index):
        if "stt-download" in self.busy:
            return
        if 0 <= index < len(self.models):
            self.stt_id = self.models[index].id
            self._render()

    @objc.python_method
    def _select_llm(self, index):
        if "llm-pull" in self.busy:
            return
        options = ["none", *(model.id for model in LLM_MODELS)]
        if 0 <= index < len(options):
            self.llm_id = options[index]
            self._render()

    def cleanupLevelAction_(self, sender):
        levels = ("none", "light", "medium")
        index = sender.tag()
        if 0 <= index < len(levels):
            self.cleanup_level = levels[index]
            self.choices = replace(self.choices, cleanup_level=self.cleanup_level)
            self._render()

    def stepAction_(self, sender):
        if self.saved:
            self.window.close()
        elif self.step in (0, 1):
            self._advance()
        elif self.step == 2:
            if self.downloaded.get(self.stt_id):
                self._advance()
            elif "stt-download" not in self.busy:
                self._start_download()
        elif self.step == 3:
            if (
                self.choices.llm_provider == "ollama"
                and self.llm_id != "none"
                and not self.ollama_running
            ):
                self.error_message = "Ollama isn't running — start it, or choose No cleanup."
                self._render()
            elif (
                self.choices.llm_provider != "ollama"
                or self.llm_id == "none"
                or self.llm_id in self.ollama_models
            ):
                self._advance()
            elif "llm-pull" not in self.busy:
                self._start_pull()
        elif self.step == 4:
            self._advance()
        elif self.step == 5:
            if "trial" not in self.busy:
                self._start_trial()
        elif self.step == 6:
            if "apply" not in self.busy:
                self.choices = replace(
                    self.choices,
                    start_on_login=bool(self.start_on_login),
                    cleanup_level=self.cleanup_level,
                )
                self._start_worker("apply", lambda: setup_flow.apply(self.choices))

    def backAction_(self, sender):
        if self.busy & {"stt-download", "llm-pull"}:
            return
        if self.step > 0 and not self.saved:
            if self.step == 4 and self.monitor is not None:
                self.monitor.stop()
                self.monitor = None
            self.step -= 1
            self.status_message = ""
            self.error_message = ""
            self._render()

    def selectStt_(self, sender):
        self._select_stt(sender.tag())

    def selectLlm_(self, sender):
        self._select_llm(sender.tag())

    def deviceChanged_(self, sender):
        self.device = sender.selectedItem().representedObject() or "System default"
        if self.monitor is not None:
            self.monitor.stop()
            self.monitor = None
        self._start_monitor()

    def hotkeyChanged_(self, sender):
        self.hotkey = sender.selectedItem().representedObject()

    def pauseMediaChanged_(self, sender):
        self.pause_media = sender.state() == AppKit.NSOnState

    def skipAction_(self, sender):
        self._advance()

    def recheckAction_(self, sender):
        if self.step == 1:
            self.checks = None
            self._render()
            self._start_checks()
        elif self.step == 3:
            self.ollama_checked = False
            self._start_ollama_check()

    def ollamaDownloadAction_(self, sender):
        AppKit.NSWorkspace.sharedWorkspace().openURL_(
            NSURL.URLWithString_("https://ollama.com/download")
        )

    def cancelAction_(self, sender):
        if self.cancel_event is not None:
            self.cancel_event.set()
            self.busy.difference_update({"stt-download", "llm-pull"})
            self.cancel_event = None
            self.job_token = None
            self.error_message = ""
            self.status_message = "Download cancelled."
            self._render()

    def settingsAction_(self, sender):
        webbrowser.open(f"http://127.0.0.1:{self.config.server_port}/settings")

    def fixAction_(self, sender):
        self._fix_check(sender.tag())

    def loginChanged_(self, sender):
        self.start_on_login = sender.state() == AppKit.NSOnState

    def launchAction_(self, sender):
        import subprocess

        import sys

        subprocess.Popen([sys.executable, "-m", "localflow.app"])

    def timerTick_(self, timer):
        self._handle_events()
        if self.step == 4 and self.monitor is not None and hasattr(self, "meter"):
            self.meter.setDoubleValue_(self.monitor.level())

    def windowWillClose_(self, notification):
        if self.monitor is not None:
            self.monitor.stop()
            self.monitor = None


def build_window():
    app = NSApplication.sharedApplication()
    app.setActivationPolicy_(AppKit.NSApplicationActivationPolicyRegular)
    controller = SetupWindowController.alloc().init()
    return controller


def main():
    build_window()
    NSApplication.sharedApplication().run()


if __name__ == "__main__":
    main()
