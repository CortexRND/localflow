"""Native Tk hub for history, insights, and local settings."""

import subprocess
import sys
import tkinter as tk
from datetime import datetime
from tkinter import filedialog, font as tkfont, messagebox

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.history import HistoryStore
from localflow.ui_tokens import (
    CARD_GAP,
    CARD_PADDING,
    PAGE_MARGIN,
    SUN_TEXT,
    TOKENS as TOKENS,
    resolve_tk_font,
    tokens_for,
)

PAGES = ("Home", "History", "Dictionary", "Style", "Models")


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
        self.page = "Home"
        self.page_errors: dict[str, str] = {}
        root.title("LocalFlow Hub")
        root.geometry("1040x700")
        root.minsize(820, 560)
        root.configure(bg=self.tokens["paper"])
        self._build_shell()
        self.show_page("Home")

    def _build_shell(self):
        self.shell = tk.Frame(self.root, bg=self.tokens["paper"])
        self.shell.pack(fill="both", expand=True)
        self.sidebar = tk.Frame(
            self.shell, bg=self.tokens["ink"], width=200, padx=16, pady=24,
        )
        self.sidebar.pack(side="left", fill="y")
        self.sidebar.pack_propagate(False)
        brand = tk.Frame(self.sidebar, bg=self.tokens["ink"])
        brand.pack(anchor="w", pady=(0, 24))
        mark = tk.Canvas(
            brand, width=14, height=14, bg=self.tokens["ink"],
            highlightthickness=0,
        )
        mark.create_oval(2, 2, 12, 12, fill=self.tokens["sun"], outline="")
        mark.pack(side="left", padx=(0, 7))
        tk.Label(
            brand, text="localflow", bg=self.tokens["ink"], fg=self.tokens["paper"],
            font=(self.font_family, 17, "bold"),
        ).pack(side="left")
        self.nav_buttons = {}
        paper = self.tokens["paper"].lstrip("#")
        ink = self.tokens["ink"].lstrip("#")
        self.sidebar_muted = "#%02X%02X%02X" % tuple(
            round(int(ink[i:i + 2], 16) * 0.25 + int(paper[i:i + 2], 16) * 0.75)
            for i in (0, 2, 4)
        )
        for page in PAGES:
            button = self._button(
                self.sidebar, page.lower(), lambda page=page: self.show_page(page),
                kind="sidebar",
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
            "sidebar": (self.tokens["ink"], self.sidebar_muted, self.tokens["ink"], 0),
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
                    bg=self.tokens["ink"], fg=self.sidebar_muted,
                    activebackground=self.tokens["ink"], activeforeground=self.sidebar_muted,
                )
        heading = tk.Frame(self.body, bg=self.tokens["paper"])
        heading.pack(fill="x", pady=(0, 20))
        tk.Label(
            heading, text=title, bg=self.tokens["paper"], fg=self.tokens["ink"],
            font=(self.font_family, 20, "bold"),
        ).pack(anchor="w")
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
            shadow = tk.Frame(card, bg=self.tokens["ink"])
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
        self.history_offset = 0
        entry = self._entry(bar, textvariable=self.history_query)
        entry.pack(side="left", fill="x", expand=True, ipady=6)
        self._button(bar, "Search", self._search_history).pack(side="left", padx=8)
        self._button(
            bar, "Clear history", self._clear_history, bg=self.tokens["red"]
        ).pack(side="right")
        self.history_rows = tk.Frame(self.body, bg=self.tokens["paper"])
        self.history_rows.pack(fill="both", expand=True)
        self.history_pager = tk.Frame(self.body, bg=self.tokens["paper"])
        self.history_pager.pack(fill="x")
        self._render_history()

    def _search_history(self):
        self.history_offset = 0
        self._render_history()

    def _change_history_page(self, offset):
        self.history_offset = max(0, self.history_offset + offset)
        self._render_history()

    def _render_history(self):
        for child in self.history_rows.winfo_children():
            child.destroy()
        for child in self.history_pager.winfo_children():
            child.destroy()
        rows = self.history.recent(
            query=self.history_query.get() or None,
            limit=9,
            offset=self.history_offset,
        )
        has_older = len(rows) > 8
        rows = rows[:8]
        if not rows:
            if self.history_query.get():
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
            for row in rows:
                self._history_line(self.history_rows, row, actions=True)
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
        self.history_query.set("")
        self._search_history()

    def _history_line(self, parent, row, actions):
        frame = tk.Frame(
            parent, bg=self.tokens["surface"], height=56, padx=CARD_PADDING,
            highlightbackground=self.tokens["line"], highlightthickness=1,
        )
        frame.pack(fill="x", pady=(0, 1))
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
        text_block.pack(side="left", fill="both", expand=True, pady=7)
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


def main():
    root = tk.Tk()
    HubWindow(root)
    root.mainloop()


if __name__ == "__main__":
    main()
