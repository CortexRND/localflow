"""Native Tk hub for history, insights, and local settings."""

import subprocess
import sys
import tkinter as tk
from tkinter import filedialog, messagebox

from localflow.config import apply_config_update, load_config, save_config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.setup_tk import TOKENS

PAGES = ("Home", "History", "Dictionary", "Style", "Models")


class HubWindow:
    def __init__(
        self,
        root: tk.Tk,
        history_store: HistoryStore | None = None,
        dictionary_store: DictionaryStore | None = None,
        config=None,
    ):
        self.root = root
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
        root.configure(bg=TOKENS["paper"])
        self._build_shell()
        self.show_page("Home")

    def _build_shell(self):
        self.shell = tk.Frame(self.root, bg=TOKENS["paper"])
        self.shell.pack(fill="both", expand=True)
        self.sidebar = tk.Frame(self.shell, bg=TOKENS["ink"], width=205, padx=16, pady=22)
        self.sidebar.pack(side="left", fill="y")
        self.sidebar.pack_propagate(False)
        tk.Label(
            self.sidebar, text="LOCALFLOW", bg=TOKENS["ink"], fg=TOKENS["sun"],
            font=("TkDefaultFont", 16, "bold"),
        ).pack(anchor="w", pady=(0, 4))
        tk.Label(
            self.sidebar, text="HUB  /  LOCAL", bg=TOKENS["ink"], fg=TOKENS["card"],
            font=("TkDefaultFont", 9, "bold"),
        ).pack(anchor="w", pady=(0, 24))
        for page in PAGES:
            self._button(self.sidebar, page, lambda page=page: self.show_page(page),
                         bg=TOKENS["ink"], fg=TOKENS["card"], anchor="w").pack(
                             fill="x", pady=3
                         )
        self.body = tk.Frame(self.shell, bg=TOKENS["paper"], padx=28, pady=24)
        self.body.pack(side="left", fill="both", expand=True)

    def _button(self, parent, label, command, **style):
        options = {
            "text": label, "command": command, "relief": "flat", "cursor": "hand2",
            "font": ("TkDefaultFont", 10, "bold"), "padx": 10, "pady": 7,
            "bg": TOKENS["cobalt"], "fg": "white",
            "activebackground": TOKENS["sun"], "activeforeground": TOKENS["ink"],
        }
        options.update(style)
        return tk.Button(parent, **options)

    def _clear(self):
        for child in self.body.winfo_children():
            child.destroy()

    def _header(self, title):
        tk.Label(
            self.body, text=title, bg=TOKENS["paper"], fg=TOKENS["ink"],
            font=("TkDefaultFont", 22, "bold"),
        ).pack(anchor="w", pady=(0, 14))
        error = self.page_errors.get(self.page)
        if error:
            tk.Label(
                self.body, text=error, bg=TOKENS["red"], fg="white",
                padx=10, pady=8, wraplength=720, justify="left",
            ).pack(fill="x", pady=(0, 12))

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
        background = TOKENS["sun"] if highlighted else TOKENS["card"]
        card = tk.Frame(parent, bg=background, padx=15, pady=12,
                        highlightbackground=TOKENS["ink"], highlightthickness=1)
        tk.Label(card, text=title.upper(), bg=background, fg=TOKENS["muted"],
                 font=("TkDefaultFont", 9, "bold")).pack(anchor="w")
        tk.Label(card, text=value, bg=background, fg=TOKENS["ink"],
                 font=("TkDefaultFont", 20, "bold")).pack(anchor="w", pady=(5, 0))
        return card

    def _page_home(self):
        stats = self.history.stats()
        streak = stats["current_streak_days"]
        longest_streak = stats["longest_streak_days"]
        cards = tk.Frame(self.body, bg=TOKENS["paper"])
        cards.pack(fill="x")
        values = (
            ("Words", stats["total_words"]),
            ("Dictations", stats["total_dictations"]),
            ("Avg WPM", f'{stats["avg_wpm"]:.1f}'),
            ("Streak", f"{streak} day{'s' if streak != 1 else ''}"),
        )
        for title, value in values:
            self._card(cards, title, value, highlighted=title == "Streak").pack(
                side="left", fill="x", expand=True, padx=(0, 10)
            )
        tk.Label(
            self.body,
            text=(
                f'Today: {stats["words_today"]} words  ·  '
                f'Last 7 days: {stats["words_7d"]} words  ·  '
                f"Longest streak: {longest_streak} day"
                f"{'s' if longest_streak != 1 else ''}"
            ),
            bg=TOKENS["paper"],
            fg=TOKENS["muted"],
        ).pack(anchor="w", pady=(10, 0))
        tk.Label(self.body, text="Recent dictations", bg=TOKENS["paper"],
                 fg=TOKENS["ink"], font=("TkDefaultFont", 14, "bold")).pack(
                     anchor="w", pady=(24, 8)
                 )
        rows = self.history.recent(limit=10)
        if not rows:
            tk.Label(self.body, text="Your recent dictations will appear here.",
                     bg=TOKENS["paper"], fg=TOKENS["muted"]).pack(anchor="w")
        for row in rows:
            self._history_line(self.body, row, actions=False)

    def _page_history(self):
        bar = tk.Frame(self.body, bg=TOKENS["paper"])
        bar.pack(fill="x", pady=(0, 10))
        query = tk.StringVar()
        entry = tk.Entry(bar, textvariable=query, font=("TkDefaultFont", 11))
        entry.pack(side="left", fill="x", expand=True, ipady=6)
        self._button(bar, "Search", lambda: self._render_history(query.get())).pack(
            side="left", padx=8
        )
        self._button(
            bar, "Clear history", self._clear_history, bg=TOKENS["red"]
        ).pack(side="right")
        self.history_rows = tk.Frame(self.body, bg=TOKENS["paper"])
        self.history_rows.pack(fill="both", expand=True)
        self._render_history("")

    def _render_history(self, query):
        for child in self.history_rows.winfo_children():
            child.destroy()
        rows = self.history.recent(query=query or None, limit=100)
        if not rows:
            tk.Label(self.history_rows, text="No matching history entries.",
                     bg=TOKENS["paper"], fg=TOKENS["muted"]).pack(anchor="w")
        for row in rows:
            self._history_line(self.history_rows, row, actions=True)

    def _history_line(self, parent, row, actions):
        frame = tk.Frame(parent, bg=TOKENS["card"], padx=10, pady=8,
                         highlightbackground=TOKENS["ink"], highlightthickness=1)
        frame.pack(fill="x", pady=3)
        text = row.get("final_text") or row.get("raw_text") or "(no text)"
        label = f'{row["kind"].title()} · {row["app_name"] or "Unknown"} · {row["status"]}   {text[:90]}'
        tk.Label(frame, text=label, bg=TOKENS["card"], fg=TOKENS["ink"],
                 anchor="w", wraplength=720, justify="left").pack(side="left", fill="x", expand=True)
        if actions:
            actions = [
                ("Copy", lambda r=row: self._copy(r, False)),
                ("Raw", lambda r=row: self._copy(r, True)),
            ]
            if row["kind"] == "dictate" and row.get("audio_path"):
                actions.append(("Retry", lambda r=row: self._retry(r)))
            actions.append(("Delete", lambda r=row: self._delete(r)))
            for name, callback in actions:
                self._button(frame, name, callback, padx=5, pady=4).pack(side="left", padx=2)

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
        controls = tk.Frame(self.body, bg=TOKENS["paper"])
        controls.pack(fill="x", pady=(0, 12))
        self.word_var = tk.StringVar()
        self.from_var = tk.StringVar()
        self.to_var = tk.StringVar()
        self.editing_word = None
        self.editing_source = None
        tk.Entry(controls, textvariable=self.word_var, width=12).pack(side="left", padx=3)
        self._button(controls, "Add / save word", self._add_word).pack(side="left", padx=3)
        tk.Entry(controls, textvariable=self.from_var, width=10).pack(side="left", padx=3)
        tk.Entry(controls, textvariable=self.to_var, width=10).pack(side="left", padx=3)
        self._button(
            controls, "Add / save replacement", self._add_replacement
        ).pack(side="left", padx=3)
        self._button(controls, "Import…", self._import_dictionary).pack(side="right")
        self.dictionary_rows = tk.Frame(self.body, bg=TOKENS["paper"])
        self.dictionary_rows.pack(fill="both", expand=True)
        self.dictionary_notice = ""
        self._render_dictionary()

    def _render_dictionary(self):
        for child in self.dictionary_rows.winfo_children():
            child.destroy()
        if self.dictionary_notice:
            tk.Label(self.dictionary_rows, text=self.dictionary_notice,
                     bg=TOKENS["sun"], fg=TOKENS["ink"], padx=8, pady=6).pack(
                         fill="x", pady=(0, 6)
                     )
        data = self.dictionary.list()
        entries = [(word, word, "word", "") for word in data["words"]]
        entries += [
            (f'{p["from"]} → {p["to"]}', p["from"], "replacement", p["to"])
            for p in data["replacements"]
        ]
        if not entries:
            tk.Label(self.dictionary_rows, text="No vocabulary or replacements yet.",
                     bg=TOKENS["paper"], fg=TOKENS["muted"]).pack(anchor="w")
        for value, key, kind, target in entries:
            row = tk.Frame(self.dictionary_rows, bg=TOKENS["card"], padx=8, pady=6)
            row.pack(fill="x", pady=2)
            tk.Label(row, text=value, bg=TOKENS["card"], fg=TOKENS["ink"]).pack(
                side="left", fill="x", expand=True
            )
            self._button(
                row,
                "Edit",
                lambda k=key, t=target, kind=kind: self._edit_dictionary(k, t, kind),
            ).pack(side="right", padx=2)
            self._button(row, "Remove", lambda key=key: self._remove_dictionary(key)).pack(
                side="right"
            )

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
                self.dictionary.remove(self.editing_word)
            self.dictionary.add_word(self.word_var.get())
            self.editing_word = None
            self.word_var.set("")
            self._render_dictionary()
        except Exception as exc:  # noqa: BLE001
            self.set_page_error("Dictionary", str(exc))

    def _add_replacement(self):
        try:
            if self.editing_source:
                self.dictionary.remove(self.editing_source)
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

    def _page_style(self):
        self.cleanup_var = tk.StringVar(value=self.config.cleanup_level)
        tk.Label(self.body, text="Cleanup level", bg=TOKENS["paper"]).pack(anchor="w")
        tk.OptionMenu(self.body, self.cleanup_var, "none", "light", "medium", "strict").pack(anchor="w", pady=5)
        self.style_vars = {}
        examples = {
            "work": "Please share the updated timeline.",
            "email": "Thanks for following up.",
            "personal": "Let's catch up soon!",
            "other": "The main point is clear.",
        }
        for category, example in examples.items():
            row = tk.Frame(self.body, bg=TOKENS["paper"])
            row.pack(fill="x", pady=5)
            tk.Label(row, text=f"{category.title()} style", bg=TOKENS["paper"], width=18, anchor="w").pack(side="left")
            var = tk.StringVar(value=getattr(self.config, f"style_{category}"))
            self.style_vars[category] = var
            tk.OptionMenu(row, var, "formal", "casual", "very_casual").pack(side="left")
            tk.Label(row, text=example, bg=TOKENS["paper"], fg=TOKENS["muted"]).pack(side="left", padx=10)
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
        tk.Label(
            self.body,
            text=f"Speech model: {self.config.stt_backend} / {self.config.model_size}\n"
                 f"Cleanup model: {self.config.llm_provider} / {self.config.llm_model or 'not selected'}",
            bg=TOKENS["card"], fg=TOKENS["ink"], justify="left", padx=18, pady=16,
            font=("TkDefaultFont", 12),
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
