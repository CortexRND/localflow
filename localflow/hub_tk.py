"""Native Tk hub for history, insights, and local settings."""

import queue
import subprocess
import sys
import threading
import time
import tkinter as tk
from dataclasses import replace
from datetime import datetime
from pathlib import Path
from tkinter import filedialog, messagebox
from tkinter import font as tkfont

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
from localflow.hub_model import (
    SPEAKER_TARGET_PLACEHOLDER,
    SPEAKERS_HEADING,
    SpeakerPanel,
    dictation_state,
    speaker_row_meta,
)
from localflow.live_config import LazyConfig
from localflow.meeting_client import MeetingClient, VoiceRecording, vault_notes_path
from localflow.people_model import (
    CONSENT_CANCEL,
    CONSENT_CONFIRM,
    CONSENT_DRAFT_NOTE,
    CONSENT_POINTS,
    CONSENT_TITLE,
    FORGET_ALL_PROMPT,
    PEOPLE_EMPTY,
    PEOPLE_HEADING,
    PEOPLE_LOADING,
    RECORD_STARTING,
    RECORD_UPLOADING,
    SAVE_FAILED,
    STALE_BADGE,
    TOGGLE_TITLE,
    PeopleState,
    can_remember,
    forget_prompt,
    is_generic_label,
    recognition_update,
    recording_status,
)
from localflow.ui_tokens import (
    CARD_GAP,
    CARD_PADDING,
    PAGE_MARGIN,
    SUN_TEXT,
    resolve_tk_font,
    tokens_for,
)
from localflow.ui_tokens import (
    TOKENS as TOKENS,  # noqa: PLC0414 - public palette re-export
)

PAGES = ("Home", "Meetings", "History", "Dictionary", "Style", "Models")


def _one_line(text, limit):
    value = " ".join(str(text).split())
    return value if len(value) <= limit else value[: limit - 1] + "…"


class HubWindow:
    def __init__(
        self,
        root: tk.Tk,
        history_store: HistoryStore | None = None,
        dictionary_store: DictionaryStore | None = None,
        config=None,
        theme="light",
        meeting_client=None,
        initial_page="Home",
    ):
        self.root = root
        self.tokens = tokens_for(theme)
        self.font_family = resolve_tk_font(tkfont.families(root))
        self.config = config or load_config()
        self._live_config = LazyConfig(self.config)
        self.history = (
            history_store
            if history_store is not None
            else HistoryStore(enabled=self.config.history_enabled)
        )
        self.dictionary = dictionary_store or DictionaryStore()
        self.meeting_client = meeting_client or MeetingClient()
        self._meeting_after = None
        self._meeting_notes_after = None
        self._meeting_notes_dirty = False
        self._meeting_notes_text = ""
        # Blank = name the notes automatically on save; anything typed (even
        # "Meeting") is the user's title and is kept.
        self._meeting_title = ""
        self._meeting_templates = []
        self._meeting_template = ""
        self._meeting_status_data = {}
        self._meeting_session_active = False
        self._meeting_last_saved = None
        self._meeting_chat = []
        self._meeting_busy = False
        self._meeting_notes_save_in_flight = False
        self._meeting_notes_save_pending = False
        self._meeting_stop_flush_pending = False
        self._meeting_stop_flush_callback = None
        self._meeting_error_message = ""
        self._meeting_results = queue.Queue()
        self._meeting_dispatch_after = None
        self._meeting_requests_in_flight = 0
        # The post-stop Speakers editor: the panel is the state, the view only
        # draws it and exists only while there are speakers to show.
        self._meeting_speakers = SpeakerPanel()
        self._meeting_speakers_view = None
        # People (speaker recognition): drawn only while speaker_diarization
        # or speaker_recognition is on; the capture holds mic audio in memory.
        self._people = PeopleState()
        self._people_view = None
        self._people_capture = None
        self._people_after = None  # the 1 s retry of a 403 right after consent
        self.page = "Home"
        self.page_errors: dict[str, str] = {}
        root.title("LocalFlow Hub")
        root.geometry("1040x700")
        root.minsize(820, 560)
        root.configure(bg=self.tokens["paper"])
        self._build_shell()
        self.show_page(initial_page)
        self._dictation_after = root.after(500, self._poll_dictation)

    def _build_shell(self):
        self.shell = tk.Frame(self.root, bg=self.tokens["paper"])
        self.shell.pack(fill="both", expand=True)
        self.sidebar = tk.Frame(
            self.shell, bg=self.tokens["sidebar"], width=200, padx=16, pady=24,
        )
        self.sidebar.pack(side="left", fill="y")
        self.sidebar.pack_propagate(False)
        brand = tk.Frame(self.sidebar, bg=self.tokens["sidebar"])
        brand.pack(anchor="w", pady=(0, 24))
        mark = tk.Canvas(
            brand, width=14, height=14, bg=self.tokens["sidebar"],
            highlightthickness=0,
        )
        mark.create_oval(2, 2, 12, 12, fill=self.tokens["sun"], outline="")
        mark.pack(side="left", padx=(0, 7))
        tk.Label(
            brand, text="localflow", bg=self.tokens["sidebar"],
            fg=self.tokens["sidebar_text"],
            font=(self.font_family, 17, "bold"),
        ).pack(side="left")
        self.nav_buttons = {}
        for page in PAGES:
            button = self._button(
                self.sidebar, page, lambda page=page: self.show_page(page),
                kind="sidebar", anchor="w", padx=12,
            )
            button.pack(fill="x", pady=2, ipady=5)
            self.nav_buttons[page] = button
        self.body = tk.Frame(
            self.shell, bg=self.tokens["paper"],
            padx=PAGE_MARGIN, pady=PAGE_MARGIN,
        )
        self.body.pack(side="left", fill="both", expand=True)

    def _button(self, parent, label, command, **style):
        kind = style.pop("kind", "secondary")
        colors = {
            "primary": (self.tokens["cobalt"], "#FFFFFF", self.tokens["ink"], 1),
            "secondary": (self.tokens["surface"], self.tokens["ink"], self.tokens["line"], 1),
            "ghost": (
                self.tokens["surface"], self.tokens["muted"],
                self.tokens["surface"], 0,
            ),
            "destructive": (self.tokens["red"], "#FFFFFF", self.tokens["red"], 0),
            "sidebar": (
                self.tokens["sidebar"], self.tokens["sidebar_text"],
                self.tokens["sidebar"], 0,
            ),
        }
        bg, fg, border, border_width = colors[kind]
        options = {
            "text": label, "command": command, "relief": "flat", "cursor": "hand2",
            "font": (self.font_family, 13, "bold" if kind != "ghost" else "normal"),
            "padx": 16, "pady": 6, "bg": bg, "fg": fg,
            "activebackground": bg, "activeforeground": fg,
            "highlightbackground": border, "highlightcolor": border,
            "highlightthickness": border_width, "bd": 0,
        }
        options.update(style)
        if "fg" in style and "activeforeground" not in style:
            options["activeforeground"] = style["fg"]
        return tk.Button(parent, **options)

    def _entry(self, parent, **options):
        return tk.Entry(
            parent,
            bg=self.tokens["surface"],
            fg=self.tokens["ink"],
            insertbackground=self.tokens["ink"],
            relief="flat",
            highlightbackground=self.tokens["line"],
            highlightcolor=self.tokens["cobalt"],
            highlightthickness=1,
            font=(self.font_family, 13),
            **options,
        )

    def _clear(self):
        for child in self.body.winfo_children():
            child.destroy()

    def _header(self, title):
        self.nav_buttons[title].configure(
            bg=self.tokens["sun"], fg=SUN_TEXT,
            activebackground=self.tokens["sun"], activeforeground=SUN_TEXT,
        )
        for page, button in self.nav_buttons.items():
            if page != title:
                button.configure(
                    bg=self.tokens["sidebar"], fg=self.tokens["sidebar_text"],
                    activebackground=self.tokens["sidebar"],
                    activeforeground=self.tokens["sidebar_text"],
                )
        heading = tk.Frame(self.body, bg=self.tokens["paper"])
        heading.pack(fill="x", pady=(0, 20))
        tk.Label(
            heading, text=title, bg=self.tokens["paper"], fg=self.tokens["ink"],
            font=(self.font_family, 20, "bold"),
        ).pack(side="left")
        if title == "History":
            self._button(
                heading, "Clear history", self._clear_history,
                kind="ghost", fg=self.tokens["red"],
            ).pack(side="right")
        if title == "Home":
            halftone = tk.Canvas(
                heading, width=150, height=100, bg=self.tokens["paper"],
                highlightthickness=0,
            )
            for row in range(8):
                for column in range(8 - row):
                    radius = max(1, 3 - (row + column) // 4)
                    x, y = 8 + column * 16, 6 + row * 12
                    halftone.create_oval(
                        x, y, x + radius * 2, y + radius * 2,
                        fill=self.tokens["sun"], outline="",
                    )
            halftone.place(relx=1, x=-4, y=0, anchor="ne")
        error = self.page_errors.get(self.page)
        if error:
            callout = tk.Frame(
                self.body, bg=self.tokens["surface"],
                highlightbackground=self.tokens["line"], highlightthickness=1,
            )
            callout.pack(fill="x", pady=(0, 12))
            tk.Frame(callout, bg=self.tokens["red"], width=4).pack(side="left", fill="y")
            tk.Label(
                callout, text="Something went wrong", bg=self.tokens["surface"],
                fg=self.tokens["red"], font=(self.font_family, 13, "bold"),
            ).pack(anchor="w", padx=12, pady=(8, 0))
            tk.Label(
                callout, text=error, bg=self.tokens["surface"], fg=self.tokens["muted"],
                padx=12, pady=4, wraplength=720, justify="left",
            ).pack(fill="x", anchor="w")

    def show_page(self, page: str):
        if self.page == "Meetings":
            self._meeting_capture_fields()
            if self._meeting_after is not None:
                self.root.after_cancel(self._meeting_after)
                self._meeting_after = None
        self.page = page
        self._clear()
        self._header(page)
        try:
            getattr(self, f"_page_{page.lower()}")()
        except Exception as exc:  # noqa: BLE001
            self.page_errors[page] = str(exc)
            self._clear()
            self._header(page)

    def set_page_error(self, page: str, message: str):
        self.page_errors[page] = message
        if self.page == page:
            self.show_page(page)

    def _card(self, parent, title, value, highlighted=False):
        background = self.tokens["sun"] if highlighted else self.tokens["surface"]
        card = tk.Frame(parent, bg=self.tokens["paper"], height=112)
        card.pack_propagate(False)
        if highlighted:
            shadow = tk.Frame(card, bg=self.tokens["shadow"])
            shadow.place(x=3, y=3, relwidth=1, relheight=1)
            surface = tk.Frame(
                card, bg=background, padx=CARD_PADDING, pady=14,
                highlightbackground=self.tokens["ink"], highlightthickness=2,
            )
            surface.pack(fill="both", expand=True, padx=(0, 3), pady=(0, 3))
            surface.lift(shadow)
        else:
            surface = tk.Frame(
                card, bg=background, padx=CARD_PADDING, pady=14,
                highlightbackground=self.tokens["line"], highlightthickness=1,
            )
            surface.pack(fill="both", expand=True)
        tk.Label(
            surface, text=title.upper(), bg=background,
            fg=SUN_TEXT if highlighted else self.tokens["muted"],
            font=(self.font_family, 11, "bold"),
        ).pack(anchor="w")
        tk.Label(
            surface, text=value, bg=background,
            fg=SUN_TEXT if highlighted else self.tokens["ink"],
            font=(self.font_family, 28, "bold"),
        ).pack(anchor="w", pady=(8, 0))
        return card

    def _page_home(self):
        self.config = replace(
            self.config,
            dictation_enabled=self._live_config.get().dictation_enabled,
        )
        state = dictation_state(self.config)
        control = tk.Frame(self.body, bg=self.tokens["paper"])
        control.pack(fill="x", pady=(0, 20))
        self.dictation_button = self._button(
            control,
            state["action"],
            self._toggle_dictation,
            kind="primary" if state["enabled"] else "secondary",
        )
        self.dictation_button.pack(side="right", padx=(16, 0))
        tk.Label(
            control,
            text=state["label"],
            bg=self.tokens["paper"],
            fg=self.tokens["ink"],
            font=(self.font_family, 16, "bold"),
        ).pack(anchor="w")
        self.dictation_status = tk.Label(
            control,
            text=state["detail"],
            bg=self.tokens["paper"],
            fg=self.tokens["muted"],
            font=(self.font_family, 12),
            wraplength=340,
            justify="left",
        )
        self.dictation_status.pack(anchor="w", pady=(6, 0))
        stats = self.history.stats()
        streak = stats["current_streak_days"]
        longest_streak = stats["longest_streak_days"]
        cards = tk.Frame(self.body, bg=self.tokens["paper"])
        cards.pack(fill="x")
        values = (
            ("Words", stats["total_words"]),
            ("Dictations", stats["total_dictations"]),
            ("Avg WPM", f'{stats["avg_wpm"]:.1f}'),
            ("Streak", f"{streak} day{'s' if streak != 1 else ''}"),
        )
        for index, (title, value) in enumerate(values):
            self._card(cards, title, value, highlighted=title == "Streak").pack(
                side="left", fill="x", expand=True,
                padx=(0, CARD_GAP if index < len(values) - 1 else 0),
            )
        tk.Label(
            self.body,
            text=(
                f'Today: {stats["words_today"]} words  ·  '
                f'Last 7 days: {stats["words_7d"]} words  ·  '
                f"Longest streak: {longest_streak} day"
                f"{'s' if longest_streak != 1 else ''}"
            ),
            bg=self.tokens["paper"],
            fg=self.tokens["muted"],
        ).pack(anchor="w", pady=(10, 0))
        tk.Label(
            self.body, text="Recent dictations", bg=self.tokens["paper"],
            fg=self.tokens["ink"], font=(self.font_family, 15, "bold"),
        ).pack(anchor="w", pady=(24, 10))
        rows = self.history.recent(limit=4)
        if not rows:
            self._empty_state(
                self.body, "No dictations yet",
                "Your recent dictations will appear here.",
                "Open setup", self._open_setup,
            )
            return
        listing = tk.Frame(
            self.body, bg=self.tokens["surface"],
            highlightbackground=self.tokens["line"], highlightthickness=1,
        )
        listing.pack(fill="x")
        for index, row in enumerate(rows):
            self._history_line(listing, row, actions=False)
            if index < len(rows) - 1:
                tk.Frame(listing, bg=self.tokens["line"], height=1).pack(fill="x")

    def _toggle_dictation(self):
        try:
            fresh = load_config()
            updated = replace(fresh, dictation_enabled=not fresh.dictation_enabled)
            save_config(updated)
            self.config = updated
            self.page_errors.pop("Home", None)
        except Exception as exc:  # noqa: BLE001 - shown inline
            self.page_errors["Home"] = f"Couldn't change dictation: {exc}"
        self.show_page("Home")

    def _poll_dictation(self):
        if self.page == "Home" and (
            self._live_config.get().dictation_enabled != self.config.dictation_enabled
        ):
            self.show_page("Home")
        self._dictation_after = self.root.after(500, self._poll_dictation)

    def _empty_state(self, parent, title, detail, action, callback):
        state = tk.Frame(parent, bg=self.tokens["paper"])
        state.pack(fill="both", expand=True, pady=32)
        tk.Label(
            state, text=title, bg=self.tokens["paper"], fg=self.tokens["ink"],
            font=(self.font_family, 15, "bold"),
        ).pack(pady=(10, 4))
        tk.Label(
            state, text=detail, bg=self.tokens["paper"], fg=self.tokens["muted"],
            font=(self.font_family, 13), wraplength=480, justify="center",
        ).pack(pady=(0, 12))
        self._button(state, action, callback).pack()

    def _open_setup(self):
        subprocess.Popen([sys.executable, "-m", "localflow.setup_window"])

    def _page_history(self):
        bar = tk.Frame(self.body, bg=self.tokens["paper"])
        bar.pack(fill="x", pady=(0, 10))
        self.history_query = tk.StringVar()
        self.history_placeholder = "Search transcripts"
        self.history_offset = 0
        self.history_entry = self._entry(bar, textvariable=self.history_query)
        self.history_entry.pack(side="left", fill="x", expand=True, ipady=6)
        self.history_query.set(self.history_placeholder)
        self.history_entry.configure(fg=self.tokens["muted"])
        self.history_entry.bind("<FocusIn>", self._focus_history_search)
        self.history_entry.bind("<FocusOut>", self._blur_history_search)
        self._button(bar, "Search", self._search_history).pack(side="left", padx=8)
        self.history_rows = tk.Frame(self.body, bg=self.tokens["paper"])
        self.history_rows.pack(fill="both", expand=True)
        self.history_pager = tk.Frame(self.body, bg=self.tokens["paper"])
        self.history_pager.pack(fill="x")
        self._render_history()

    def _search_history(self):
        self.history_offset = 0
        self._render_history()

    def _history_search_value(self):
        value = self.history_query.get()
        return "" if value == self.history_placeholder else value

    def _focus_history_search(self, _event=None):
        if self.history_query.get() == self.history_placeholder:
            self.history_query.set("")
            self.history_entry.configure(fg=self.tokens["ink"])

    def _blur_history_search(self, _event=None):
        if not self.history_query.get():
            self.history_query.set(self.history_placeholder)
            self.history_entry.configure(fg=self.tokens["muted"])

    def _change_history_page(self, offset):
        self.history_offset = max(0, self.history_offset + offset)
        self._render_history()

    def _render_history(self):
        for child in self.history_rows.winfo_children():
            child.destroy()
        for child in self.history_pager.winfo_children():
            child.destroy()
        rows = self.history.recent(
            query=self._history_search_value() or None,
            limit=9,
            offset=self.history_offset,
        )
        has_older = len(rows) > 8
        rows = rows[:8]
        if not rows:
            self.history_rows.configure(
                bg=self.tokens["paper"], highlightthickness=0,
            )
            if self._history_search_value():
                empty = (
                    "No matching entries",
                    "Try another search or start a new dictation.",
                    "Clear search",
                    self._clear_history_search,
                )
            else:
                empty = (
                    "No dictations yet",
                    "Your dictations will appear here.",
                    "Open setup",
                    self._open_setup,
                )
            self._empty_state(self.history_rows, *empty)
        else:
            self.history_rows.configure(
                bg=self.tokens["surface"],
                highlightbackground=self.tokens["line"],
                highlightthickness=1,
            )
            for index, row in enumerate(rows):
                self._history_line(self.history_rows, row, actions=True)
                if index < len(rows) - 1:
                    tk.Frame(
                        self.history_rows, bg=self.tokens["line"], height=1,
                    ).pack(fill="x")
        pager = tk.Frame(self.history_pager, bg=self.tokens["paper"])
        pager.pack(pady=(12, 0))
        controls = tk.Frame(pager, bg=self.tokens["paper"])
        controls.pack()
        newer = self._button(
            controls, "Newer", lambda: self._change_history_page(-8),
        )
        newer.pack(side="left")
        newer.configure(state="normal" if self.history_offset else "disabled")
        tk.Label(
            controls, text=f"Page {self.history_offset // 8 + 1}",
            bg=self.tokens["paper"], fg=self.tokens["muted"],
            font=(self.font_family, 11, "bold"),
        ).pack(side="left", padx=12)
        older = self._button(
            controls, "Older", lambda: self._change_history_page(8),
        )
        older.pack(side="left")
        older.configure(state="normal" if has_older else "disabled")

    def _clear_history_search(self):
        self.history_query.set(self.history_placeholder)
        self.history_entry.configure(fg=self.tokens["muted"])
        self._search_history()

    def _history_line(self, parent, row, actions):
        frame = tk.Frame(
            parent, bg=self.tokens["surface"], height=48, padx=CARD_PADDING,
        )
        frame.pack(fill="x")
        frame.pack_propagate(False)
        text = _one_line(
            row.get("final_text") or row.get("raw_text") or "(no text)",
            60 if actions else 76,
        )
        timestamp = datetime.fromtimestamp(float(row.get("ts") or 0))
        if timestamp.date() == datetime.now().date():
            date_label = timestamp.strftime("%I:%M %p").lstrip("0")
        else:
            date_label = timestamp.strftime("%b %d").replace(" 0", " ")
        if actions:
            self._button(
                frame, "Delete", lambda r=row: self._delete(r),
                kind="ghost", fg=self.tokens["red"],
            ).pack(side="right", padx=(2, 0))
            self._button(
                frame, "Copy original", lambda r=row: self._copy(r, True),
                kind="ghost",
            ).pack(side="right", padx=(2, 0))
            self._button(
                frame, "Copy", lambda r=row: self._copy(r, False),
                kind="ghost",
            ).pack(side="right", padx=(2, 0))
            if row["kind"] == "dictate" and row.get("audio_path"):
                self._button(
                    frame, "Retry", lambda r=row: self._retry(r),
                    kind="ghost",
                ).pack(side="right", padx=(2, 0))
        status = row.get("status", "ok")
        if status != "ok":
            tk.Label(
                frame, text="Failed" if status == "failed" else "Silent",
                bg=self.tokens["red"] if status == "failed" else self.tokens["line"],
                fg="#FFFFFF" if status == "failed" else self.tokens["muted"],
                font=(self.font_family, 9, "bold"), padx=8, pady=3,
            ).pack(side="right", padx=(8, 0))
        text_block = tk.Frame(frame, bg=self.tokens["surface"])
        text_block.pack(side="left", fill="both", expand=True, pady=4)
        tk.Label(
            text_block, text=text, bg=self.tokens["surface"], fg=self.tokens["ink"],
            font=(self.font_family, 13), anchor="w", justify="left",
        ).pack(fill="x")
        metadata = (
            f'{row.get("app_name") or "Unknown"}  ·  {date_label}  ·  '
            f'{row["kind"].title()}'
        )
        tk.Label(
            text_block, text=metadata, bg=self.tokens["surface"],
            fg=self.tokens["muted"], font=(self.font_family, 11),
            anchor="w",
        ).pack(fill="x")

    def _copy(self, row, raw):
        import pyperclip

        pyperclip.copy(row["raw_text" if raw else "final_text"] or "")

    def _retry(self, row):
        if not row.get("audio_path"):
            self.set_page_error("History", "This entry has no stored audio clip.")
            return
        def run():
            try:
                from localflow.app import retry_history_entry
                text = retry_history_entry(row["id"], self.history)
                if text:
                    import pyperclip
                    pyperclip.copy(text)
                self.root.after(0, lambda: self.show_page("History"))
            except Exception as exc:  # noqa: BLE001
                self.root.after(
                    0,
                    lambda message=str(exc): self.set_page_error("History", message),
                )
        import threading
        threading.Thread(target=run, daemon=True).start()

    def _delete(self, row):
        self.history.delete(row["id"])
        self.show_page("History")

    def _clear_history(self):
        if messagebox.askyesno("Clear history", "Delete all history and stored clips?"):
            self.history.clear()
            self.show_page("History")

    def _page_dictionary(self):
        controls = tk.Frame(self.body, bg=self.tokens["paper"])
        controls.pack(fill="x", pady=(0, 12))
        self.word_var = tk.StringVar()
        self.from_var = tk.StringVar()
        self.to_var = tk.StringVar()
        self.editing_word = None
        self.editing_source = None
        word_row = tk.Frame(controls, bg=self.tokens["paper"])
        word_row.pack(fill="x", pady=(0, 8))
        self.word_entry = self._entry(
            word_row, textvariable=self.word_var, width=20,
        )
        self.word_entry.pack(side="left", fill="x", expand=True)
        self._button(word_row, "Add / save word", self._add_word).pack(
            side="left", padx=(8, 0),
        )
        self._button(word_row, "Import…", self._import_dictionary).pack(
            side="right", padx=(8, 0),
        )
        replacement_row = tk.Frame(controls, bg=self.tokens["paper"])
        replacement_row.pack(fill="x")
        self.from_entry = self._entry(
            replacement_row, textvariable=self.from_var, width=16,
        )
        self.from_entry.pack(side="left", fill="x", expand=True)
        tk.Label(
            replacement_row, text="→", bg=self.tokens["paper"],
            fg=self.tokens["muted"], font=(self.font_family, 13),
        ).pack(side="left", padx=8)
        self.to_entry = self._entry(
            replacement_row, textvariable=self.to_var, width=16,
        )
        self.to_entry.pack(side="left", fill="x", expand=True)
        self._button(
            replacement_row, "Add / save replacement", self._add_replacement
        ).pack(side="left", padx=(8, 0))
        self.dictionary_rows = tk.Frame(self.body, bg=self.tokens["paper"])
        self.dictionary_rows.pack(fill="both", expand=True)
        self.dictionary_notice = ""
        self._render_dictionary()

    def _render_dictionary(self):
        for child in self.dictionary_rows.winfo_children():
            child.destroy()
        if self.dictionary_notice:
            tk.Label(
                self.dictionary_rows, text=self.dictionary_notice,
                bg=self.tokens["sun"], fg="#141414", padx=16, pady=10,
                font=(self.font_family, 13),
            ).pack(fill="x", pady=(0, 8))
        data = self.dictionary.list()
        entries = [(word, word, "word", "") for word in data["words"]]
        entries += [
            (f'{p["from"]} → {p["to"]}', p["from"], "replacement", p["to"])
            for p in data["replacements"]
        ]
        if not entries:
            self._empty_state(
                self.dictionary_rows, "No vocabulary yet",
                "Add a word or replacement to personalize dictation.",
                "Add a word", self._focus_word,
            )
            return
        listing = tk.Frame(
            self.dictionary_rows, bg=self.tokens["surface"],
            highlightbackground=self.tokens["line"], highlightthickness=1,
        )
        listing.pack(fill="x")
        for index, (value, key, kind, target) in enumerate(entries):
            row = tk.Frame(
                listing, bg=self.tokens["surface"], height=56, padx=CARD_PADDING,
            )
            row.pack(fill="x")
            row.pack_propagate(False)
            self._button(
                row,
                "Remove",
                lambda key=key: self._remove_dictionary(key),
                kind="ghost",
                fg=self.tokens["red"],
            ).pack(side="right", padx=(8, 0))
            self._button(
                row,
                "Edit",
                lambda k=key, t=target, kind=kind: self._edit_dictionary(k, t, kind),
                kind="ghost",
            ).pack(side="right")
            tk.Label(
                row, text=_one_line(value, 72), bg=self.tokens["surface"], fg=self.tokens["ink"],
                font=(self.font_family, 13), anchor="w",
            ).pack(side="left", fill="x", expand=True)
            if index < len(entries) - 1:
                tk.Frame(listing, bg=self.tokens["line"], height=1).pack(fill="x")

    def _focus_word(self):
        self.word_entry.focus_set()

    def _edit_dictionary(self, key, target, kind):
        if kind == "word":
            self.editing_word = key
            self.word_var.set(key)
        else:
            self.editing_source = key
            self.from_var.set(key)
            self.to_var.set(target)

    def _add_word(self):
        try:
            if self.editing_word:
                if not self.dictionary.update_word(
                    self.editing_word, self.word_var.get()
                ):
                    raise DictionaryError("word already exists or was removed")
            else:
                self.dictionary.add_word(self.word_var.get())
            self.editing_word = None
            self.word_var.set("")
            self._render_dictionary()
        except Exception as exc:  # noqa: BLE001
            self.set_page_error("Dictionary", str(exc))

    def _add_replacement(self):
        try:
            if self.editing_source:
                if not self.dictionary.update_replacement(
                    self.editing_source, self.from_var.get(), self.to_var.get()
                ):
                    raise DictionaryError(
                        "replacement source already exists or was removed"
                    )
            else:
                self.dictionary.add_replacement(self.from_var.get(), self.to_var.get())
            self.editing_source = None
            self.from_var.set("")
            self.to_var.set("")
            self._render_dictionary()
        except Exception as exc:  # noqa: BLE001
            self.set_page_error("Dictionary", str(exc))

    def _remove_dictionary(self, value):
        self.dictionary.remove(value)
        self._render_dictionary()

    def _import_dictionary(self):
        path = filedialog.askopenfilename(filetypes=(("CSV/JSON", "*.csv *.json"),))
        if path:
            try:
                counts = self.dictionary.import_file(path)
                self.dictionary_notice = (
                    f'Imported {counts["added"]}; skipped {counts["skipped_duplicate"]} '
                    f'duplicates and {counts["skipped_invalid"]} invalid.'
                )
                self._render_dictionary()
            except Exception as exc:  # noqa: BLE001
                self.set_page_error("Dictionary", str(exc))

    def _segmented_control(self, parent, variable, choices):
        buttons = {}
        group = tk.Frame(
            parent, bg=self.tokens["surface"],
            highlightbackground=self.tokens["line"], highlightthickness=1,
        )
        group.pack(side="left")

        def select(value):
            variable.set(value)
            for option, button in buttons.items():
                selected = option == value
                button.configure(
                    bg=self.tokens["ink"] if selected else self.tokens["surface"],
                    fg=self.tokens["paper"] if selected else self.tokens["ink"],
                    activebackground=self.tokens["ink"] if selected else self.tokens["surface"],
                    activeforeground=self.tokens["paper"] if selected else self.tokens["ink"],
                )

        for value, label in choices:
            button = tk.Button(
                group,
                text=label,
                command=lambda choice=value: select(choice),
                bg=self.tokens["surface"],
                fg=self.tokens["ink"],
                activebackground=self.tokens["surface"],
                relief="flat",
                bd=0,
                padx=12,
                pady=6,
                font=(self.font_family, 13, "bold"),
            )
            button.pack(side="left", fill="x", expand=True)
            buttons[value] = button
        select(variable.get())
        return buttons

    def _page_style(self):
        self.cleanup_var = tk.StringVar(value=self.config.cleanup_level)
        cleanup_row = tk.Frame(self.body, bg=self.tokens["paper"])
        cleanup_row.pack(fill="x", pady=(0, 10))
        tk.Label(
            cleanup_row, text="Cleanup level", bg=self.tokens["paper"], fg=self.tokens["ink"],
            font=(self.font_family, 10, "bold"),
        ).pack(side="left", padx=(0, 10))
        self.cleanup_buttons = self._segmented_control(
            cleanup_row,
            self.cleanup_var,
            (("none", "None"), ("light", "Light"), ("medium", "Medium")),
        )
        self.style_vars = {}
        self.style_buttons = {}
        style_choices = (
            ("formal", "Formal"),
            ("casual", "Casual"),
            ("very_casual", "Very casual"),
            ("excited", "Excited"),
        )
        examples = {
            "work": "Please share the updated timeline.",
            "email": "Thanks for following up.",
            "personal": "Let's catch up soon!",
            "other": "The main point is clear.",
        }
        for category, example in examples.items():
            row = tk.Frame(self.body, bg=self.tokens["paper"])
            row.pack(fill="x", pady=(7, 0))
            tk.Label(
                row, text=f"{category.title()} style", bg=self.tokens["paper"],
                fg=self.tokens["ink"], width=15, anchor="w",
            ).pack(side="left")
            var = tk.StringVar(value=getattr(self.config, f"style_{category}"))
            self.style_vars[category] = var
            self.style_buttons[category] = self._segmented_control(
                row, var, style_choices
            )
            tk.Label(
                self.body, text=example, bg=self.tokens["paper"], fg=self.tokens["muted"],
            ).pack(anchor="w", padx=15)
        self._button(self.body, "Save style", self._save_style).pack(anchor="w", pady=14)

    def _save_style(self):
        update = {
            "cleanup": {
                "level": self.cleanup_var.get(),
                **{f"style_{key}": var.get() for key, var in self.style_vars.items()},
            }
        }
        try:
            fresh = load_config()
            updated = apply_config_update(fresh, update)
            save_config(updated)
            self.config = updated
            self.page_errors.pop("Style", None)
            self.show_page("Style")
        except Exception as exc:  # noqa: BLE001
            self.set_page_error("Style", str(exc))

    def _page_models(self):
        self.config = load_config()
        tk.Label(
            self.body,
            text=f"Speech model: {self.config.stt_backend} / {self.config.model_size}\n"
                 f"Cleanup model: {self.config.llm_provider} / {self.config.llm_model or 'not selected'}",
            bg=self.tokens["surface"], fg=self.tokens["ink"], justify="left", padx=18, pady=16,
            font=(self.font_family, 12),
        ).pack(anchor="w", fill="x")
        self._button(
            self.body, "Change models…",
            lambda: subprocess.Popen([sys.executable, "-m", "localflow.setup_window"]),
        ).pack(anchor="w", pady=12)

    def _meeting_capture_fields(self):
        if hasattr(self, "meeting_notes"):
            self._meeting_notes_text = self.meeting_notes.get("1.0", "end-1c")
        if hasattr(self, "meeting_title_entry"):
            self._meeting_title = self.meeting_title_entry.get()
        if hasattr(self, "meeting_template_var"):
            self._meeting_template = self.meeting_template_var.get()
        if self._meeting_speakers_view is not None:
            self._meeting_speakers_view.capture()
        # _clear destroys the widgets with the rest of the page.
        self._meeting_speakers_view = None
        if self._people_view is not None:
            self._people_view.capture()
            self._people_view.cancel_timers()
        self._people_cancel_capture()  # the mic never stays open off screen
        if self._people_after is not None:
            self.root.after_cancel(self._people_after)
            self._people_after = None
            if self._people.phase == "loading":
                self._people.phase = ""
        self._people_view = None

    def _meeting_request(self, operation, callback, *args):
        """Run a client method (by name) or a callable off the UI thread."""
        self._meeting_requests_in_flight += 1
        if self._meeting_dispatch_after is None:
            self._meeting_dispatch_after = self.root.after(
                25, self._meeting_dispatch_results
            )
        name = getattr(operation, "__name__", "call") if callable(operation) else operation

        def run():
            try:
                call = (
                    operation if callable(operation)
                    else getattr(self.meeting_client, operation)
                )
                result = call(*args)
                error = None
            except Exception as exc:  # noqa: BLE001
                result, error = None, exc
            self._meeting_results.put((callback, result, error))

        threading.Thread(
            target=run, name=f"localflow-meeting-{name}", daemon=True
        ).start()

    def _meeting_dispatch_results(self):
        self._meeting_dispatch_after = None
        while True:
            try:
                callback, result, error = self._meeting_results.get_nowait()
            except queue.Empty:
                break
            self._meeting_requests_in_flight -= 1
            try:
                callback(result, error)
            except tk.TclError:
                pass
        try:
            if (
                self._meeting_requests_in_flight
                and self.root.winfo_exists()
            ):
                self._meeting_dispatch_after = self.root.after(
                    25, self._meeting_dispatch_results
                )
        except tk.TclError:
            pass

    def _page_meetings(self):
        # The old views went with the old page body; they are rebuilt on demand.
        self._meeting_speakers_view = None
        self._people_view = None
        people_request = self._people_sync_flags(refresh=True)
        self.meeting_down = tk.Frame(self.body, bg=self.tokens["paper"])
        self.meeting_down_label = tk.Label(
            self.meeting_down, text="LocalFlow server not running.",
            bg=self.tokens["paper"], fg=self.tokens["muted"],
            font=(self.font_family, 13),
        )
        self.meeting_down_label.pack(anchor="w", pady=(8, 12))
        self._button(
            self.meeting_down, "Retry", self._meeting_retry, kind="secondary",
        ).pack(anchor="w")

        self.meeting_content = tk.Frame(self.body, bg=self.tokens["paper"])
        self.meeting_watch = tk.Label(
            self.meeting_content, text="", bg=self.tokens["paper"],
            fg=self.tokens["muted"], font=(self.font_family, 11),
            anchor="w", justify="left", wraplength=720,
        )
        self.meeting_watch.pack(fill="x", pady=(0, 12))
        self.meeting_error = tk.Label(
            self.meeting_content, text="", bg=self.tokens["paper"],
            fg=self.tokens["red"], font=(self.font_family, 12),
            anchor="w", justify="left", wraplength=720,
        )

        self.meeting_setup = tk.Frame(self.meeting_content, bg=self.tokens["paper"])
        tk.Label(
            self.meeting_setup, text="Meeting title", bg=self.tokens["paper"],
            fg=self.tokens["ink"], font=(self.font_family, 11, "bold"),
        ).pack(anchor="w", pady=(0, 5))
        self.meeting_title_entry = self._entry(
            self.meeting_setup, width=48,
        )
        self.meeting_title_entry.insert(0, self._meeting_title)
        self.meeting_title_entry.pack(fill="x", ipady=6, pady=(0, 12))
        tk.Label(
            self.meeting_setup, text="Notes template", bg=self.tokens["paper"],
            fg=self.tokens["ink"], font=(self.font_family, 11, "bold"),
        ).pack(anchor="w", pady=(0, 5))
        self.meeting_template_var = tk.StringVar(
            value=self._meeting_template or "general"
        )
        self.meeting_template_label_var = tk.StringVar(value="General")
        self.meeting_template_menu = tk.OptionMenu(
            self.meeting_setup, self.meeting_template_label_var, "General"
        )
        self.meeting_template_menu.configure(
            bg=self.tokens["surface"], fg=self.tokens["ink"],
            activebackground=self.tokens["surface"], relief="flat",
            highlightthickness=1, highlightbackground=self.tokens["line"],
            font=(self.font_family, 12), anchor="w",
        )
        self.meeting_template_menu.pack(fill="x", pady=(0, 12))
        self.meeting_setup.pack(fill="x")

        notes_heading = tk.Frame(self.meeting_content, bg=self.tokens["paper"])
        notes_heading.pack(fill="x", pady=(0, 5))
        tk.Label(
            notes_heading, text="Your notes", bg=self.tokens["paper"],
            fg=self.tokens["ink"], font=(self.font_family, 13, "bold"),
        ).pack(side="left")
        self.meeting_notes_status = tk.Label(
            notes_heading, text="", bg=self.tokens["paper"],
            fg=self.tokens["muted"], font=(self.font_family, 10),
        )
        self.meeting_notes_status.pack(side="right")
        self.meeting_notes = tk.Text(
            self.meeting_content, height=7, wrap="word",
            bg=self.tokens["surface"], fg=self.tokens["ink"],
            insertbackground=self.tokens["ink"], relief="flat",
            highlightbackground=self.tokens["line"],
            highlightcolor=self.tokens["cobalt"], highlightthickness=1,
            font=(self.font_family, 12), padx=10, pady=8,
        )
        self.meeting_notes.pack(fill="x")
        if self._meeting_notes_text:
            self.meeting_notes.insert("1.0", self._meeting_notes_text)
        self.meeting_notes.bind("<<Modified>>", self._meeting_notes_changed)

        self.meeting_action = self._button(
            self.meeting_content, "Start meeting transcription",
            self._meeting_toggle, kind="primary",
        )
        self.meeting_action.pack(anchor="w", pady=(12, 10))
        self.meeting_live = tk.Label(
            self.meeting_content, text="", bg=self.tokens["surface"],
            fg=self.tokens["ink"], justify="left", anchor="nw",
            font=(self.font_family, 11), padx=12, pady=10,
            wraplength=700, highlightbackground=self.tokens["line"],
            highlightthickness=1,
        )
        self.meeting_live.pack(fill="x", pady=(0, 12))

        self.meeting_saved_frame = tk.Frame(
            self.meeting_content, bg=self.tokens["surface"],
            highlightbackground=self.tokens["line"], highlightthickness=1,
            padx=12, pady=10,
        )
        self.meeting_saved_label = tk.Label(
            self.meeting_saved_frame, text="", bg=self.tokens["surface"],
            fg=self.tokens["ink"], anchor="w", justify="left",
            wraplength=580, font=(self.font_family, 11),
        )
        self.meeting_saved_label.pack(side="left", fill="x", expand=True)
        self._button(
            self.meeting_saved_frame, "Open notes", self._meeting_open_notes,
        ).pack(side="right", padx=(12, 0))

        self.meeting_chat_frame = tk.Frame(
            self.meeting_content, bg=self.tokens["paper"]
        )
        tk.Label(
            self.meeting_chat_frame, text="Ask the transcript",
            bg=self.tokens["paper"], fg=self.tokens["ink"],
            font=(self.font_family, 13, "bold"),
        ).pack(anchor="w", pady=(16, 6))
        ask_row = tk.Frame(self.meeting_chat_frame, bg=self.tokens["paper"])
        ask_row.pack(fill="x")
        self.meeting_question = self._entry(ask_row)
        self.meeting_question.pack(side="left", fill="x", expand=True, ipady=6)
        self._button(
            ask_row, "Ask", self._meeting_ask, kind="primary",
        ).pack(side="left", padx=(8, 0))
        self.meeting_chat_answers = tk.Frame(
            self.meeting_chat_frame, bg=self.tokens["paper"]
        )
        self.meeting_chat_answers.pack(fill="x", pady=(8, 0))

        self.meeting_down.pack_forget()
        self.meeting_content.pack(fill="both", expand=True)
        self._meeting_render()
        if self._meeting_error_message:
            self._meeting_show_error(self._meeting_error_message)
        self._meeting_request("templates", self._meeting_templates_loaded)
        self._meeting_poll()
        if people_request:
            self._people_send(people_request)

    def _meeting_retry(self):
        self.meeting_down.pack_forget()
        self.meeting_content.pack(fill="both", expand=True)
        self._meeting_request("templates", self._meeting_templates_loaded)
        self._meeting_poll()

    def _meeting_templates_loaded(self, result, error):
        if error:
            return
        self._meeting_templates = result.get("templates", [])
        if self.page != "Meetings":
            return
        default = result.get("default", "general")
        menu = self.meeting_template_menu["menu"]
        menu.delete(0, "end")
        for template in self._meeting_templates:
            template_id = template["id"]
            menu.add_command(
                label=template["name"],
                command=lambda value=template_id, name=template["name"]:
                    self._meeting_select_template(value, name),
            )
        valid_ids = {template["id"] for template in self._meeting_templates}
        if self._meeting_template not in valid_ids:
            self._meeting_template = default
        self._meeting_select_template(
            self._meeting_template or default,
            next(
                (
                    template["name"] for template in self._meeting_templates
                    if template["id"] == (self._meeting_template or default)
                ),
                default,
            ),
        )

    def _meeting_select_template(self, template_id, name):
        self._meeting_template = template_id
        self.meeting_template_var.set(template_id)
        self.meeting_template_label_var.set(name)

    def _meeting_poll(self):
        if self.page != "Meetings":
            return
        ticket = self._meeting_speakers.ticket()
        self._meeting_request(
            "status",
            lambda result, error: self._meeting_status_received(result, error, ticket),
        )

    def _meeting_status_received(self, result, error, ticket=None):
        if error:
            self._meeting_status_data = {}
            self._meeting_session_active = False
            if self.page != "Meetings":
                return
            self.meeting_content.pack_forget()
            self.meeting_down.pack(fill="x", anchor="w")
            self.meeting_down_label.configure(text=str(error))
            self._meeting_schedule_poll()
            return
        self._meeting_status_data = result
        if result.get("last_saved"):
            self._meeting_last_saved = result["last_saved"]
            self._meeting_speakers.sync(result["last_saved"], ticket)
        session = result.get("session")
        if session and not self._meeting_session_active and not self._meeting_notes_dirty:
            self._meeting_notes_text = session.get("user_notes", "")
            if self.page == "Meetings":
                self.meeting_notes.delete("1.0", "end")
                self.meeting_notes.insert("1.0", self._meeting_notes_text)
                self.meeting_notes.edit_modified(False)
            template_id = session.get("template") or "general"
            template = next(
                (
                    item for item in self._meeting_templates
                    if item["id"] == template_id
                ),
                None,
            )
            self._meeting_template = template_id
            if self.page == "Meetings":
                self._meeting_select_template(
                    template_id, template["name"] if template else template_id
                )
        self._meeting_session_active = bool(session)
        if self.page != "Meetings":
            return
        self.meeting_down.pack_forget()
        self.meeting_content.pack(fill="both", expand=True)
        self._meeting_render()
        self._meeting_schedule_poll()

    def _meeting_schedule_poll(self):
        if self.page == "Meetings" and self._meeting_after is None:
            self._meeting_after = self.root.after(2000, self._meeting_poll_timer)

    def _meeting_poll_timer(self):
        self._meeting_after = None
        self._meeting_poll()

    def _meeting_render(self):
        status = self._meeting_status_data
        session = status.get("session")
        if session:
            audio = session.get("system_audio")
            if audio == "on":
                watch = "mic + system audio"
            elif audio == "error":
                watch = "mic only — system audio unavailable"
                if session.get("system_audio_error"):
                    watch += f" — {session['system_audio_error']}"
            else:
                watch = "mic only"
            if session.get("title"):
                watch = f"{session['title']} · {watch}"
            self.meeting_watch.configure(text=watch)
            self.meeting_watch.pack(fill="x", pady=(0, 12))
            self.meeting_setup.pack_forget()
            self.meeting_action.configure(
                text=(
                    f"Stop & save notes — {int(session.get('seconds', 0)) // 60}m"
                    f"{int(session.get('seconds', 0)) % 60:02d}s · "
                    f"{session.get('segment_count', 0)} segments"
                ),
                state="disabled" if self._meeting_busy else "normal",
            )
            self.meeting_live.configure(
                text=(
                    "\n\n".join(
                        f"[{segment.get('stamp', '00:00')}] "
                        f"{'Me' if segment.get('source') == 'me' else 'Others'}: "
                        f"{segment.get('text', '')}"
                        for segment in session.get("segments", [])
                    )
                    or "(listening — first transcript segment lands after ~30s)"
                )
            )
            self.meeting_live.pack(fill="x", pady=(0, 12))
        else:
            watch = (
                "mic in use" if status.get("watching") and status.get("mic_busy")
                else "watching" if status.get("watching")
                else "watcher off" if status.get("watch_error")
                else ""
            )
            self.meeting_watch.configure(text=watch)
            if watch:
                self.meeting_watch.pack(fill="x", pady=(0, 12))
            else:
                self.meeting_watch.pack_forget()
            self.meeting_setup.pack(fill="x")
            self.meeting_action.configure(
                text="Start meeting transcription",
                state="disabled" if self._meeting_busy else "normal",
            )
            self.meeting_live.pack_forget()

        self.meeting_template_menu.configure(
            state="disabled" if session else "normal"
        )
        if self._meeting_last_saved and not session:
            saved = self._meeting_last_saved
            prefix = "Enhanced notes: " if saved.get("enhanced") else "Saved: "
            self.meeting_saved_label.configure(
                text=prefix + saved.get("notes_path", "")
            )
            self.meeting_saved_frame.pack(fill="x", pady=(0, 10))
        else:
            self.meeting_saved_frame.pack_forget()
        self._meeting_render_speakers(session)
        if status.get("chat_available") or self._meeting_last_saved:
            self.meeting_chat_frame.pack(fill="x")
        else:
            self.meeting_chat_frame.pack_forget()
        self._meeting_render_chat()
        self._meeting_render_people()

    def _meeting_notes_changed(self, _event=None):
        if self.meeting_notes.edit_modified():
            self.meeting_notes.edit_modified(False)
            self._meeting_notes_dirty = True
            self._meeting_notes_text = self.meeting_notes.get("1.0", "end-1c")
            if self._meeting_status_data.get("session"):
                self.meeting_notes_status.configure(
                    text="Editing…", fg=self.tokens["muted"]
                )
                if self._meeting_notes_after is not None:
                    self.root.after_cancel(self._meeting_notes_after)
                self._meeting_notes_after = self.root.after(
                    800, self._meeting_save_notes
                )
            else:
                self.meeting_notes_status.configure(
                    text="Kept for when you start", fg=self.tokens["muted"]
                )

    def _meeting_notes_visible(self):
        if self.page != "Meetings" or not hasattr(self, "meeting_notes"):
            return False
        try:
            return bool(self.meeting_notes.winfo_exists())
        except tk.TclError:
            return False

    def _meeting_current_notes_text(self):
        if self._meeting_notes_visible():
            try:
                return self.meeting_notes.get("1.0", "end-1c")
            except tk.TclError:
                pass
        return self._meeting_notes_text

    def _meeting_set_notes_status(self, text, color):
        if self._meeting_notes_visible():
            try:
                self.meeting_notes_status.configure(text=text, fg=color)
            except tk.TclError:
                pass

    def _meeting_save_notes(self, callback=None):
        self._meeting_notes_after = None
        if callback is not None:
            self._meeting_stop_flush_callback = callback
            self._meeting_stop_flush_pending = True
            self._meeting_notes_save_pending = False
            self._meeting_set_notes_status("Saving…", self.tokens["muted"])
        else:
            self._meeting_notes_save_pending = True
        self._meeting_advance_notes_save()

    def _meeting_advance_notes_save(self):
        if self._meeting_notes_save_in_flight:
            return
        if self._meeting_stop_flush_pending:
            is_stop_flush = True
            self._meeting_stop_flush_pending = False
        elif self._meeting_notes_save_pending:
            is_stop_flush = False
            self._meeting_notes_save_pending = False
        else:
            return
        text = self._meeting_current_notes_text()
        self._meeting_notes_save_in_flight = True
        self._meeting_notes_dirty = True
        self._meeting_set_notes_status("Saving…", self.tokens["muted"])
        self._meeting_request(
            "save_notes",
            lambda _result, error: self._meeting_notes_saved(
                text, is_stop_flush, error
            ),
            text,
        )

    def _meeting_notes_saved(self, text, is_stop_flush, error):
        self._meeting_notes_save_in_flight = False
        current_text = self._meeting_current_notes_text()
        if error:
            self._meeting_notes_dirty = True
            self._meeting_set_notes_status(
                f"Not saved — {error}", self.tokens["red"]
            )
            if is_stop_flush:
                self._meeting_notes_save_pending = False
                self._meeting_stop_flush_pending = False
                callback = self._meeting_stop_flush_callback
                self._meeting_stop_flush_callback = None
                if callback:
                    callback(error)
                return
            if self._meeting_stop_flush_pending or self._meeting_notes_save_pending:
                self._meeting_advance_notes_save()
            return

        self._meeting_notes_dirty = current_text != text
        if is_stop_flush:
            if self._meeting_notes_dirty:
                self._meeting_notes_save_pending = False
                self._meeting_stop_flush_pending = True
                self._meeting_advance_notes_save()
                return
            callback = self._meeting_stop_flush_callback
            self._meeting_stop_flush_callback = None
            if callback:
                callback(None)
            return

        if self._meeting_stop_flush_pending or self._meeting_notes_save_pending:
            self._meeting_advance_notes_save()
        elif self._meeting_notes_dirty:
            self._meeting_set_notes_status("Editing…", self.tokens["muted"])
        else:
            self._meeting_set_notes_status(
                "Saved " + datetime.now().strftime("%H:%M"),
                self.tokens["success"],
            )

    def _meeting_toggle(self):
        if self._meeting_busy:
            return
        session = self._meeting_status_data.get("session")
        self._meeting_busy = True
        self._meeting_error_message = ""
        self.meeting_error.configure(text="")
        self.meeting_error.pack_forget()
        self.meeting_action.configure(state="disabled")
        if not session:
            typed = self.meeting_title_entry.get().strip()
            title = typed or "Meeting"
            auto_title = not typed
            template = self.meeting_template_var.get()
            notes = self.meeting_notes.get("1.0", "end-1c")

            def started(result, error):
                self._meeting_busy = False
                if error:
                    self._meeting_show_error(error)
                    return
                self._meeting_notes_dirty = False
                self._meeting_last_saved = None
                self._meeting_speakers.sync(None)
                self._meeting_chat.clear()
                if self._meeting_notes_visible():
                    self.meeting_notes_status.configure(
                        text="Saved" if notes else "", fg=self.tokens["muted"]
                    )
                self._meeting_poll()

            self._meeting_request(
                "start", started, title, "", template, notes, auto_title
            )
            return

        self._meeting_notes_dirty = True

        def flushed(error):
            if error:
                self._meeting_busy = False
                self._meeting_show_error(
                    f"Notes could not be saved; the meeting is still running — {error}"
                )
                if self.page == "Meetings":
                    self._meeting_render()
                return
            notes_text = self._meeting_current_notes_text()
            if self.page == "Meetings":
                self.meeting_action.configure(
                    text=(
                        "Enhancing your notes…"
                        if notes_text.strip()
                        else "Summarizing & saving…"
                    )
                )

            def stopped(result, stop_error):
                self._meeting_busy = False
                if stop_error:
                    self._meeting_show_error(stop_error)
                else:
                    self._meeting_last_saved = result
                    self._meeting_speakers.sync(result)
                    self._meeting_notes_dirty = False
                    self._meeting_notes_text = ""
                    self._meeting_session_active = False
                    self._meeting_status_data["session"] = None
                    self._meeting_status_data["last_saved"] = result
                    if self._meeting_notes_visible():
                        self.meeting_notes.delete("1.0", "end")
                        self.meeting_notes.edit_modified(False)
                        self.meeting_notes_status.configure(text="")
                self._meeting_poll()

            self._meeting_request("stop", stopped)

        if self._meeting_notes_after is not None:
            self.root.after_cancel(self._meeting_notes_after)
            self._meeting_notes_after = None
        self._meeting_save_notes(flushed)

    def _meeting_render_speakers(self, session=None):
        if session is None:
            session = self._meeting_status_data.get("session")
        panel, view = self._meeting_speakers, self._meeting_speakers_view
        if not (self._meeting_last_saved and not session and panel.visible):
            if panel.mode and not panel.busy:
                panel.cancel()  # nothing stays open off screen
            if view is not None:
                view.destroy()
                self._meeting_speakers_view = None
            return
        if view is None:
            view = self._meeting_speakers_view = SpeakerPanelView(
                self, self.meeting_content, panel, self._meeting_speaker_send,
            )
        # A pack_forget-ed widget re-packs at the end; keep it under the saved row.
        view.frame.pack(fill="x", pady=(0, 10), after=self.meeting_saved_frame)
        view.render()

    def _meeting_speaker_send(self, request):
        operation, args, token = request
        self._meeting_request(
            operation,
            lambda result, error: self._meeting_speaker_done(token, result, error),
            *args,
        )

    def _meeting_speaker_done(self, token, result, error):
        # State first: a TclError below must not leave the panel busy.
        panel = self._meeting_speakers
        if not panel.finish(token, result, error):
            return
        people_request = None
        if panel.last_action == "remember" and error is None:
            # A new voice: the People list shows it.
            people = self._people
            if people.active and not people.busy:
                people_request = people.load()
        if self.page == "Meetings":
            self._meeting_render_speakers()
            self._people_render()
        if people_request:
            self._people_send(people_request)

    # -------------------------------------------------------------- people ---

    def _people_config(self):
        try:
            return load_config()
        except Exception:  # noqa: BLE001 - an unreadable file: what we were given
            return self.config

    def _people_sync_flags(self, refresh=False):
        """Read the speaker flags (LazyConfig doesn't refresh them); a list load or None."""
        config = self._people_config()
        diarization = bool(getattr(config, "speaker_diarization", False))
        recognition = bool(getattr(config, "speaker_recognition", False))
        people = self._people
        was_rec = bool(people.rec)
        request = people.set_flags(diarization, recognition)
        if was_rec and not people.rec:
            capture, self._people_capture = self._people_capture, None
            if capture is not None:
                threading.Thread(
                    target=capture.discard, name="localflow-people-discard", daemon=True,
                ).start()
        if request is None and refresh:
            request = people.refresh()
        self._meeting_speakers.set_remember(can_remember(diarization, recognition))
        return request

    def _people_shown(self):
        return self.page == "Meetings" and self._meeting_notes_visible()

    def _people_render(self):
        if self._people_shown():
            self._meeting_render_people()

    def _meeting_render_people(self):
        people, view = self._people, self._people_view
        if not people.visible:
            if view is not None:
                view.destroy()
                self._people_view = None
            return
        if view is None:
            view = self._people_view = PeoplePanelView(
                self, self.meeting_content, people, self._people_send,
            )
        # Always the last block of the page, under the chat.
        view.frame.pack_forget()
        view.frame.pack(fill="x", pady=(16, 0))
        view.render()

    def _people_send(self, request):
        """Run a PeopleState request off the UI thread (capture calls on the capture)."""
        operation, args, token = request
        if operation == "capture_start":
            capture = self._people_capture = VoiceRecording(self._people_config())
            self._meeting_request(
                capture.start,
                lambda _result, error: self._people_rec_started(token, capture, error),
            )
            return
        if operation == "capture_finish":
            capture, self._people_capture = self._people_capture, None
            client = self.meeting_client

            def finish(name):
                if capture is None:
                    raise RuntimeError("no recording")
                return capture.finish(client, name)

            self._meeting_request(
                finish,
                lambda result, error: self._people_done(token, result, error),
                *args,
            )
            return
        done = self._people_loaded if operation == "enrolled" else self._people_done
        self._meeting_request(
            operation, lambda result, error: done(token, result, error), *args,
        )

    def _people_loaded(self, token, result, error):
        outcome = self._people.loaded(token, result, error)
        if outcome == "retry":
            # The server adopts a consent write when its config watcher runs.
            self._people_after = self.root.after(1000, self._people_retry_load)
            return
        if outcome:
            self._people_render()

    def _people_retry_load(self):
        self._people_after = None
        people = self._people
        if people.phase == "loading":
            self._people_send(("enrolled", (), people.seq))

    def _people_rec_started(self, token, capture, error):
        if not self._people.recording_started(token, error, time.monotonic()):
            return  # cancelled first: the capture was discarded
        if error is not None and self._people_capture is capture:
            self._people_capture = None
        self._people_render()

    def _people_done(self, token, result, error):
        people = self._people
        was_forget_all = people.busy == "forget_all"
        if not people.finish(token, result, error):
            return
        if was_forget_all and error is None:
            self._meeting_speakers.clear_remembered()
            self._meeting_render_speakers()
        request = people.load() if people.reload else None
        self._people_render()
        if request:
            self._people_send(request)

    def _people_cancel_capture(self):
        """Leave the enroll row; a capture in progress is stopped and dropped."""
        if not self._people.cancel_recording():
            return
        capture, self._people_capture = self._people_capture, None
        if capture is not None:
            threading.Thread(
                target=capture.discard, name="localflow-people-discard", daemon=True,
            ).start()

    def _people_toggle(self):
        people = self._people
        action = people.request_recognition(not people.recognition)
        if action == "consent":
            accepted = ConsentDialog(self).run()
            if people.consent_result(accepted):
                self._people_write(True)
            else:
                self._people_render()
        elif action == "write":
            self._people_write(False)

    def _people_write(self, on):
        """Save speaker_recognition (only after consent when turning it on)."""
        people = self._people
        try:
            updated = apply_config_update(load_config(), recognition_update(on))
            save_config(updated)
        except Exception:  # noqa: BLE001 - ConfigError or an unwritable folder
            people.recognition_save_failed(SAVE_FAILED)
            self._people_render()
            return
        self.config = updated
        request = people.recognition_saved(on)
        sync = self._people_sync_flags()
        request = request or sync
        if self._people_shown():
            self._meeting_render()
        if request:
            self._people_send(request)

    def _meeting_show_error(self, error):
        self._meeting_error_message = str(error)
        if not self._meeting_notes_visible():
            return
        self.meeting_error.configure(text=f"Meeting: {error}")
        self.meeting_error.pack(fill="x", pady=(0, 8), before=self.meeting_action)
        self.meeting_action.configure(state="normal")

    def _meeting_open_notes(self):
        saved = self._meeting_last_saved or {}
        path = vault_notes_path(saved.get("notes_path"), load_config().vault_path)
        if path:
            from localflow.platform import current

            current().open_path(path)

    def _meeting_ask(self):
        question = self.meeting_question.get().strip()
        if not question or self._meeting_busy:
            return
        self.meeting_question.delete(0, "end")
        entry = {"question": question, "answer": "Reading the transcript…"}
        self._meeting_chat.insert(0, entry)
        self._meeting_render_chat()

        def answered(result, error):
            entry["answer"] = (
                f"Couldn’t answer — {error}" if error else result.get("answer", "")
            )
            entry["error"] = error is not None
            self._meeting_render_chat()

        self._meeting_request("chat", answered, question)

    def _meeting_render_chat(self):
        if (
            self.page != "Meetings" or not hasattr(self, "meeting_chat_answers")
        ):
            return
        try:
            for child in self.meeting_chat_answers.winfo_children():
                child.destroy()
        except tk.TclError:
            return
        for item in self._meeting_chat:
            tk.Label(
                self.meeting_chat_answers,
                text=f"You: {item['question']}\n{item['answer']}",
                bg=self.tokens["paper"],
                fg=self.tokens["red"] if item.get("error") else self.tokens["ink"],
                anchor="w", justify="left", wraplength=700,
                font=(self.font_family, 11),
            ).pack(fill="x", pady=(0, 8))


class SpeakerPanelView:
    """Draws a SpeakerPanel inside `parent`; all state lives in the panel.

    Plain widgets only (no Tk variables): rename reads Entry.get() and the
    merge target is a Menubutton whose Menu items call back directly.
    """

    def __init__(self, hub, parent, panel, send):
        self.hub = hub
        self.panel = panel
        self.send = send
        tokens = hub.tokens
        family = hub.font_family
        self.frame = tk.Frame(parent, bg=tokens["paper"])
        self.heading = tk.Label(
            self.frame, text=SPEAKERS_HEADING, bg=tokens["paper"], fg=tokens["ink"],
            font=(family, 13, "bold"),
        )
        self.heading.pack(anchor="w", pady=(6, 6))
        self.card = tk.Frame(
            self.frame, bg=tokens["surface"],
            highlightbackground=tokens["line"], highlightthickness=1,
        )
        self.card.pack(fill="x")
        self.error = tk.Label(
            self.frame, text="", bg=tokens["paper"], fg=tokens["red"],
            font=(family, 12), wraplength=700, justify="left", anchor="w",
        )
        self._reset_refs()
        self._rendered = None
        self._edit_key = ("", "")

    def _reset_refs(self):
        self.entry = None
        self.entry_id = ""
        self.menubutton = None
        self.menu = None
        self.buttons = {}
        self.name_labels = []
        self.duration_labels = []

    def render(self):
        """Show the error line; rebuild the rows only when the panel changed."""
        panel = self.panel
        if panel.error:
            self.error.configure(text=panel.error)
            self.error.pack(fill="x", pady=(6, 0))
        else:
            self.error.configure(text="")
            self.error.pack_forget()
        if self._rendered == panel.revision:
            return
        self.capture()
        self._fill()
        self._rendered = panel.revision

    def capture(self):
        """Keep what is typed in the rename field in the panel."""
        if self.entry is None:
            return
        try:
            text = self.entry.get()
        except tk.TclError:
            return
        self.panel.set_draft(text, self.entry_id)

    def destroy(self):
        self._reset_refs()
        self.frame.destroy()

    # ---------------------------------------------------------------- rows ---

    def _fill(self):
        panel = self.panel
        tokens = self.hub.tokens
        for child in self.card.winfo_children():
            child.destroy()
        self._reset_refs()
        focus = False
        for index, row in enumerate(panel.rows):
            if index:
                tk.Frame(self.card, bg=tokens["line"], height=1).pack(fill="x")
            frame = tk.Frame(self.card, bg=tokens["surface"], padx=12, pady=6)
            frame.pack(fill="x")
            mode = panel.mode if row["id"] == panel.speaker_id else ""
            if mode in ("rename", "remember"):
                focus = self._edit_key != (mode, row["id"])
                self._rename_row(frame, row, "Remember" if mode == "remember" else "Save")
            elif mode == "merge":
                self._merge_row(frame, row)
            else:
                self._normal_row(frame, row)
        self._edit_key = (panel.mode, panel.speaker_id)
        if self.entry is not None and not panel.busy:
            self.entry.focus_set()
            if focus:
                self.entry.selection_range(0, "end")
            self.entry.icursor("end")
        if panel.busy:
            for button in self.buttons.values():
                button.configure(state="disabled")
            if self.entry is not None:
                self.entry.configure(state="disabled")
            if self.menubutton is not None:
                self.menubutton.configure(state="disabled")

    def _button(self, parent, key, label, command, kind="ghost"):
        button = self.hub._button(parent, label, command, kind=kind, padx=10, pady=2)
        self.buttons[key] = button
        return button

    def _normal_row(self, frame, row):
        tokens = self.hub.tokens
        family = self.hub.font_family
        speaker_id = row["id"]
        if self.panel.can_merge:
            self._button(
                frame, ("merge", speaker_id), "Merge",
                lambda: self._begin("merge", speaker_id),
            ).pack(side="right", padx=(8, 0))
        self._button(
            frame, ("rename", speaker_id), "Rename",
            lambda: self._begin("rename", speaker_id),
        ).pack(side="right")
        if self.panel.remember:
            # Packed after Rename, so it sits to its left.
            remembered = self.panel.is_remembered(speaker_id)
            remember = self._button(
                frame, ("remember", speaker_id), "Remembered" if remembered else "Remember",
                lambda: self._begin("remember", speaker_id),
            )
            remember.pack(side="right", padx=(0, 8))
            if remembered:
                remember.configure(state="disabled")
        name = tk.Label(
            frame, text=row["label"], bg=tokens["surface"], fg=tokens["ink"],
            font=(family, 12, "bold"), anchor="w",
        )
        name.pack(side="left")
        duration = tk.Label(
            frame, text=speaker_row_meta(self.panel.rows, row),
            bg=tokens["surface"], fg=tokens["muted"], font=(family, 11),
        )
        duration.pack(side="left", padx=(10, 0))
        self.name_labels.append(name)
        self.duration_labels.append(duration)

    def _rename_row(self, frame, row, save_label="Save"):
        """The name prompt of a rename, or of a Remember (`save_label` "Remember")."""
        panel = self.panel
        self._button(frame, ("cancel", row["id"]), "Cancel", self._cancel).pack(
            side="right", padx=(8, 0),
        )
        if panel.busy == "rename":
            save_label = "Saving…"
        elif panel.busy == "remember":
            save_label = "Remembering…"
        self._button(
            frame, ("save", row["id"]), save_label, self._save, kind="secondary",
        ).pack(side="right", padx=(12, 0))
        entry = self.hub._entry(frame)
        entry.insert(0, panel.draft)
        entry.pack(side="left", fill="x", expand=True, ipady=4)
        entry.bind("<Return>", lambda _event: self._save())
        entry.bind("<Escape>", lambda _event: self._cancel())
        self.entry = entry
        self.entry_id = row["id"]

    def _merge_row(self, frame, row):
        panel = self.panel
        tokens = self.hub.tokens
        family = self.hub.font_family
        self._button(frame, ("cancel", row["id"]), "Cancel", self._cancel).pack(
            side="right", padx=(8, 0),
        )
        confirm = self._button(
            frame, ("confirm", row["id"]),
            "Merging…" if panel.busy == "merge" else "Merge", self._confirm,
            kind="secondary",
        )
        confirm.pack(side="right", padx=(12, 0))
        if not panel.into_id:
            confirm.configure(state="disabled")
        choices = panel.choices()
        chosen = next(
            (choice["title"] for choice in choices if choice["id"] == panel.into_id),
            SPEAKER_TARGET_PLACEHOLDER,
        )
        menubutton = tk.Menubutton(
            frame, text=chosen, bg=tokens["surface"], fg=tokens["ink"],
            activebackground=tokens["surface"], relief="flat",
            highlightthickness=1, highlightbackground=tokens["line"],
            font=(family, 12), anchor="w", indicatoron=True,
        )
        menu = tk.Menu(menubutton, tearoff=0)
        for choice in choices:
            menu.add_command(
                label=choice["title"],
                command=lambda speaker_id=choice["id"]: self._choose(speaker_id),
            )
        menubutton.configure(menu=menu)
        menubutton.pack(side="right", padx=(10, 0))
        self.menubutton = menubutton
        self.menu = menu
        prompt = tk.Label(
            frame, text=f"Merge {row['label']} into", bg=tokens["surface"],
            fg=tokens["ink"], font=(family, 12), anchor="w",
        )
        prompt.pack(side="left", fill="x", expand=True)
        self.name_labels.append(prompt)

    # ----------------------------------------------------------- callbacks ---

    def _begin(self, mode, speaker_id):
        if mode == "rename":
            self.panel.begin_rename(speaker_id)
        elif mode == "remember":
            row = self.panel.row(speaker_id)
            label = row["label"] if row is not None else ""
            self.panel.begin_remember(speaker_id, "" if is_generic_label(label) else label)
        else:
            self.panel.begin_merge(speaker_id)
        self.render()

    def _choose(self, speaker_id):
        self.panel.choose_target(speaker_id)
        self.render()

    def _cancel(self):
        self.panel.cancel()
        self.render()

    def _save(self):
        if self.entry is None:
            return
        if self.panel.mode == "remember":
            request = self.panel.submit_remember(self.entry.get())
        else:
            request = self.panel.submit_rename(self.entry.get())
        self.render()
        if request:
            self.send(request)

    def _confirm(self):
        request = self.panel.submit_merge()
        self.render()
        if request:
            self.send(request)


class PeoplePanelView:
    """Draws the People block (a PeopleState) at the bottom of Meetings.

    Plain widgets only (no Tk variables): names are read with Entry.get();
    the countdown is one label updated by root.after while recording.
    """

    def __init__(self, hub, parent, people, send):
        self.hub = hub
        self.people = people
        self.send = send
        tokens = hub.tokens
        family = hub.font_family
        self.frame = tk.Frame(parent, bg=tokens["paper"])
        self.heading = tk.Label(
            self.frame, text=PEOPLE_HEADING, bg=tokens["paper"], fg=tokens["ink"],
            font=(family, 13, "bold"),
        )
        self.heading.pack(anchor="w", pady=(0, 6))
        self.card = tk.Frame(
            self.frame, bg=tokens["surface"],
            highlightbackground=tokens["line"], highlightthickness=1,
        )
        self.card.pack(fill="x")
        self.message = tk.Label(
            self.frame, text="", bg=tokens["paper"], fg=tokens["red"],
            font=(family, 12), wraplength=700, justify="left", anchor="w",
        )
        self._tick_after = None
        self._rendered = None
        self._edit_key = ("", "")
        self._reset_refs()

    def _reset_refs(self):
        self.entry = None
        self.entry_key = None
        self.buttons = {}
        self.toggle = None
        self.toggle_detail = None
        self.status = None
        self.name_labels = []
        self.badges = []
        self.meta_labels = []
        self.list_label = None

    def render(self):
        """Show the message line; rebuild the card only when the state changed."""
        people = self.people
        tokens = self.hub.tokens
        if people.message:
            self.message.configure(
                text=people.message,
                fg=tokens["red"] if people.error else tokens["success"],
            )
            self.message.pack(fill="x", pady=(6, 0))
        else:
            self.message.configure(text="")
            self.message.pack_forget()
        if self._rendered != people.revision:
            self.capture()
            self._fill()
            self._rendered = people.revision
        self._schedule_tick()

    def capture(self):
        """Keep what is typed in the rename or enroll field in the state."""
        if self.entry is None:
            return
        try:
            text = self.entry.get()
        except tk.TclError:
            return
        self.people.set_draft(text, self.entry_key)

    def cancel_timers(self):
        if self._tick_after is not None:
            try:
                self.hub.root.after_cancel(self._tick_after)
            except tk.TclError:
                pass
            self._tick_after = None

    def destroy(self):
        self.cancel_timers()
        self._reset_refs()
        self.frame.destroy()

    # ---------------------------------------------------------------- draw ---

    def _button(self, parent, key, label, command, kind="ghost", **style):
        button = self.hub._button(parent, label, command, kind=kind, padx=10, pady=2, **style)
        self.buttons[key] = button
        return button

    def _line(self):
        tk.Frame(self.card, bg=self.hub.tokens["line"], height=1).pack(fill="x")

    def _row_frame(self):
        frame = tk.Frame(self.card, bg=self.hub.tokens["surface"], padx=12, pady=6)
        frame.pack(fill="x")
        return frame

    def _label(self, parent, text, *, size=11, color="muted", bold=False, **options):
        tokens = self.hub.tokens
        return tk.Label(
            parent, text=text, bg=tokens["surface"], fg=tokens[color],
            font=(self.hub.font_family, size, "bold" if bold else "normal"),
            anchor="w", justify="left", **options,
        )

    def _fill(self):
        people = self.people
        for child in self.card.winfo_children():
            child.destroy()
        self._reset_refs()
        self._toggle_row()
        focus = False
        if people.active:
            self._line()
            self._footer()
            self._line()
            self._list()
            if self.entry is not None:
                focus = self._edit_key != (people.mode, people.person_id)
        self._edit_key = (people.mode, people.person_id)
        if self.entry is not None and not people.busy and not people.rec:
            self.entry.focus_set()
            if focus:
                self.entry.selection_range(0, "end")
            self.entry.icursor("end")
        if people.busy:
            for key, button in self.buttons.items():
                if key != "toggle":
                    button.configure(state="disabled")
            if self.entry is not None:
                self.entry.configure(state="disabled")
        elif people.rec:
            for key, button in self.buttons.items():
                if isinstance(key, tuple):
                    button.configure(state="disabled")

    def _toggle_row(self):
        people = self.people
        frame = tk.Frame(self.card, bg=self.hub.tokens["surface"], padx=12, pady=8)
        frame.pack(fill="x")
        # Packed before the labels, so a narrow window keeps it visible.
        toggle = self._button(
            frame, "toggle", "Turn Off" if people.recognition else "Turn On…",
            self.hub._people_toggle, kind="secondary",
        )
        toggle.pack(side="right")
        if not people.toggle_enabled:
            toggle.configure(state="disabled")
        self.toggle = toggle
        text = tk.Frame(frame, bg=self.hub.tokens["surface"])
        text.pack(side="left", fill="x", expand=True)
        self._label(text, TOGGLE_TITLE, size=12, color="ink", bold=True).pack(anchor="w")
        self.toggle_detail = self._label(text, people.toggle_detail)
        self.toggle_detail.pack(anchor="w")

    def _list(self):
        people = self.people
        if people.phase == "error":
            frame = self._row_frame()
            self._button(frame, "retry", "Retry", self._retry).pack(side="right")
            self.list_label = self._label(
                frame, people.load_error, size=12, color="red", wraplength=560,
            )
            self.list_label.pack(side="left", fill="x", expand=True)
            return
        if people.phase != "ready" or not people.rows:
            frame = self._row_frame()
            self.list_label = self._label(
                frame, PEOPLE_LOADING if people.phase != "ready" else PEOPLE_EMPTY, size=12,
            )
            self.list_label.pack(side="left")
            return
        for index, row in enumerate(people.rows):
            if index:
                tk.Frame(self.card, bg=self.hub.tokens["line"], height=1).pack(fill="x")
            frame = self._row_frame()
            if people.mode == "rename" and people.person_id == row["id"]:
                self._name_row(frame, row["id"], "Saving…" if people.busy == "rename" else "Save",
                               self._save)
            else:
                self._person_row(frame, row)

    def _person_row(self, frame, row):
        person_id = row["id"]
        self._button(
            frame, ("forget", person_id), "Forget", lambda: self._forget(person_id),
            fg=self.hub.tokens["red"],
        ).pack(side="right", padx=(8, 0))
        self._button(
            frame, ("rename", person_id), "Rename", lambda: self._rename(person_id),
        ).pack(side="right")
        name = self._label(frame, row["name"], size=12, color="ink", bold=True)
        name.pack(side="left")
        self.name_labels.append(name)
        if row["stale"]:
            badge = self._label(frame, STALE_BADGE, size=10, color="red")
            badge.pack(side="left", padx=(10, 0))
            self.badges.append(badge)
        meta = self._label(frame, row["meta"])
        meta.pack(side="left", padx=(10, 0))
        self.meta_labels.append(meta)

    def _name_row(self, frame, key, save_label, save):
        """An entry with a submit and a Cancel button (rename and the enroll name)."""
        self._button(frame, ("cancel", key), "Cancel", self._cancel).pack(
            side="right", padx=(8, 0),
        )
        self._button(frame, ("save", key), save_label, save, kind="secondary").pack(
            side="right", padx=(12, 0),
        )
        entry = self.hub._entry(frame)
        entry.insert(0, self.people.draft)
        entry.pack(side="left", fill="x", expand=True, ipady=4)
        entry.bind("<Return>", lambda _event: save())
        entry.bind("<Escape>", lambda _event: self._cancel())
        self.entry = entry
        self.entry_key = key

    def _footer(self):
        people = self.people
        frame = self._row_frame()
        if people.rec:
            uploading = people.rec == "uploading"
            cancel = self._button(frame, "rec_cancel", "Cancel", self._cancel_recording)
            cancel.pack(side="right", padx=(8, 0))
            stop = self._button(
                frame, "stop", "Saving…" if uploading else "Stop & save", self._stop,
                kind="secondary",
            )
            stop.pack(side="right")
            if uploading:
                cancel.configure(state="disabled")
            if not people.can_stop(time.monotonic()):
                stop.configure(state="disabled")
            self.status = self._label(frame, self._status_text(), size=12, color="ink")
            self.status.pack(side="left", fill="x", expand=True)
            return
        if people.mode == "enroll":
            self._name_row(frame, "", "Record", self._record)
            return
        self._button(
            frame, "forget_all", "Forget all…", self._forget_all, fg=self.hub.tokens["red"],
        ).pack(side="right", padx=(8, 0))
        self._button(frame, "export", "Export…", self._export).pack(side="right")
        self._button(frame, "enroll", "Enroll my voice", self._enroll, kind="secondary").pack(
            side="left",
        )

    def _status_text(self):
        people = self.people
        if people.rec == "starting":
            return RECORD_STARTING
        if people.rec == "uploading":
            return RECORD_UPLOADING
        return recording_status(people.remaining(time.monotonic()))

    # ----------------------------------------------------------- countdown ---

    def _schedule_tick(self):
        if self.people.rec == "recording" and self._tick_after is None:
            self._tick_after = self.hub.root.after(250, self._tick)

    def _tick(self):
        self._tick_after = None
        people = self.people
        if people.rec != "recording" or self.status is None:
            return
        now = time.monotonic()
        if people.due(now):
            self._stop()
            return
        try:
            self.status.configure(text=self._status_text())
            stop = self.buttons.get("stop")
            if stop is not None and people.can_stop(now):
                stop.configure(state="normal")
        except tk.TclError:
            return
        self._schedule_tick()

    # ----------------------------------------------------------- callbacks ---

    def _refresh(self, request=None):
        self.render()
        if request:
            self.send(request)

    def _retry(self):
        self._refresh(self.people.retry())

    def _rename(self, person_id):
        self.people.begin_rename(person_id)
        self._refresh()

    def _save(self):
        if self.entry is None:
            return
        self._refresh(self.people.submit_rename(self.entry.get()))

    def _cancel(self):
        self.capture()
        self.people.cancel()
        self._refresh()

    def _forget(self, person_id):
        row = self.people.row(person_id)
        if row is None or self.people.busy:
            return
        if not messagebox.askyesno(
            "Forget", forget_prompt(row["name"]), icon="warning", default="no",
            parent=self.frame,
        ):
            return
        self._refresh(self.people.submit_forget(person_id))

    def _forget_all(self):
        if self.people.busy:
            return
        if not messagebox.askyesno(
            "Forget all", FORGET_ALL_PROMPT, icon="warning", default="no", parent=self.frame,
        ):
            return
        self._refresh(self.people.submit_forget_all())

    def _export(self):
        if self.people.busy:
            return
        folder = filedialog.askdirectory(
            title="Export voiceprints", mustexist=False, parent=self.frame,
        )
        if not folder:
            return
        target = Path(folder).expanduser()
        try:
            nonempty = target.exists() and (not target.is_dir() or any(target.iterdir()))
        except OSError:
            nonempty = True
        request = self.people.submit_export(
            str(target), self.hub._people_config().vault_path, nonempty=nonempty,
            base_url=getattr(self.hub.meeting_client, "base_url", None),
        )
        self._refresh(request)

    def _enroll(self):
        self.people.begin_enroll(getattr(self.hub.meeting_client, "base_url", None))
        self._refresh()

    def _record(self):
        if self.entry is None:
            return
        self._refresh(self.people.start_recording(self.entry.get()))

    def _stop(self):
        request = self.people.stop_recording(time.monotonic())
        if request:
            self.cancel_timers()
        self._refresh(request)

    def _cancel_recording(self):
        self.cancel_timers()
        self.hub._people_cancel_capture()
        self._refresh()


class ConsentDialog:
    """The speaker recognition consent dialog. Cancel is the default: Return and
    Escape both cancel, and Turn On needs a click. Plain widgets only."""

    def __init__(self, hub):
        tokens = hub.tokens
        family = hub.font_family
        self.hub = hub
        self.accepted = False
        top = self.top = tk.Toplevel(hub.root)
        top.title(CONSENT_TITLE)
        top.configure(bg=tokens["paper"], padx=24, pady=20)
        top.resizable(False, False)
        try:
            top.transient(hub.root)
        except tk.TclError:
            pass
        tk.Label(
            top, text=CONSENT_TITLE, bg=tokens["paper"], fg=tokens["ink"],
            font=(family, 15, "bold"), anchor="w", justify="left", wraplength=460,
        ).pack(fill="x")
        self.points = []
        for point in CONSENT_POINTS:
            label = tk.Label(
                top, text=f"• {point}", bg=tokens["paper"], fg=tokens["ink"],
                font=(family, 12), anchor="w", justify="left", wraplength=460,
            )
            label.pack(fill="x", pady=(10, 0))
            self.points.append(label)
        tk.Label(
            top, text=CONSENT_DRAFT_NOTE, bg=tokens["paper"], fg=tokens["muted"],
            font=(family, 11), anchor="w", justify="left", wraplength=460,
        ).pack(fill="x", pady=(14, 0))
        buttons = tk.Frame(top, bg=tokens["paper"])
        buttons.pack(fill="x", pady=(18, 0))
        self.cancel_button = hub._button(buttons, CONSENT_CANCEL, self.cancel, kind="secondary")
        self.cancel_button.pack(side="right")
        self.confirm_button = hub._button(buttons, CONSENT_CONFIRM, self.confirm, kind="primary")
        self.confirm_button.pack(side="right", padx=(0, 8))
        top.bind("<Return>", lambda _event: self.cancel())
        top.bind("<Escape>", lambda _event: self.cancel())
        top.protocol("WM_DELETE_WINDOW", self.cancel)
        self.cancel_button.focus_set()
        try:
            top.grab_set()
        except tk.TclError:
            pass  # not viewable yet (or another grab): still modal by wait_window

    def cancel(self):
        self.accepted = False
        self._close()

    def confirm(self):
        self.accepted = True
        self._close()

    def _close(self):
        try:
            self.top.grab_release()
            self.top.destroy()
        except tk.TclError:
            pass

    def run(self) -> bool:
        """Wait until the dialog closes; True only after Turn On."""
        try:
            self.hub.root.wait_window(self.top)
        except tk.TclError:
            pass
        return self.accepted


def main(initial_page="Home"):
    root = tk.Tk()
    HubWindow(root, initial_page=initial_page)
    root.mainloop()


if __name__ == "__main__":
    main()
