"""Native Tk hub for history, insights, and local settings."""

import queue
import subprocess
import sys
import threading
import tkinter as tk
from datetime import datetime
from tkinter import filedialog, font as tkfont, messagebox

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
from localflow.meeting_client import MeetingClient
from localflow.ui_tokens import (
    CARD_GAP,
    CARD_PADDING,
    PAGE_MARGIN,
    SUN_TEXT,
    TOKENS as TOKENS,
    resolve_tk_font,
    tokens_for,
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
        self._meeting_title = "Meeting"
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
        self.page = "Home"
        self.page_errors: dict[str, str] = {}
        root.title("LocalFlow Hub")
        root.geometry("1040x700")
        root.minsize(820, 560)
        root.configure(bg=self.tokens["paper"])
        self._build_shell()
        self._meeting_dispatch_after = root.after(
            25, self._meeting_dispatch_results
        )
        self.show_page(initial_page)

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
            self.config = apply_config_update(self.config, update)
            save_config(self.config)
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

    def _meeting_request(self, operation, callback, *args):
        def run():
            try:
                result = getattr(self.meeting_client, operation)(*args)
                error = None
            except Exception as exc:  # noqa: BLE001
                result, error = None, exc
            self._meeting_results.put((callback, result, error))

        threading.Thread(
            target=run, name=f"localflow-meeting-{operation}", daemon=True
        ).start()

    def _meeting_dispatch_results(self):
        self._meeting_dispatch_after = None
        while True:
            try:
                callback, result, error = self._meeting_results.get_nowait()
            except queue.Empty:
                break
            try:
                callback(result, error)
            except tk.TclError:
                pass
        try:
            if self.root.winfo_exists():
                self._meeting_dispatch_after = self.root.after(
                    25, self._meeting_dispatch_results
                )
        except tk.TclError:
            pass

    def _page_meetings(self):
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
        self._meeting_request("status", self._meeting_status_received)

    def _meeting_status_received(self, result, error):
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
        if status.get("chat_available") or self._meeting_last_saved:
            self.meeting_chat_frame.pack(fill="x")
        else:
            self.meeting_chat_frame.pack_forget()
        self._meeting_render_chat()

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
            title = self.meeting_title_entry.get().strip() or "Meeting"
            template = self.meeting_template_var.get()
            notes = self.meeting_notes.get("1.0", "end-1c")

            def started(result, error):
                self._meeting_busy = False
                if error:
                    self._meeting_show_error(error)
                    return
                self._meeting_notes_dirty = False
                self._meeting_last_saved = None
                self._meeting_chat.clear()
                if self._meeting_notes_visible():
                    self.meeting_notes_status.configure(
                        text="Saved" if notes else "", fg=self.tokens["muted"]
                    )
                self._meeting_poll()

            self._meeting_request(
                "start", started, title, "", template, notes
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

    def _meeting_show_error(self, error):
        self._meeting_error_message = str(error)
        if not self._meeting_notes_visible():
            return
        self.meeting_error.configure(text=f"Meeting: {error}")
        self.meeting_error.pack(fill="x", pady=(0, 8), before=self.meeting_action)
        self.meeting_action.configure(state="normal")

    def _meeting_open_notes(self):
        if self._meeting_last_saved and self._meeting_last_saved.get("notes_path"):
            from localflow.platform import current

            current().open_path(self._meeting_last_saved["notes_path"])

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


def main(initial_page="Home"):
    root = tk.Tk()
    HubWindow(root, initial_page=initial_page)
    root.mainloop()


if __name__ == "__main__":
    main()
