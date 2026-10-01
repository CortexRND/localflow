"""Native hot-phrase editor: `lf phrases`, the menu bar's "Hot Phrases…",
or the LocalFlow Hot Phrases.app bundle (`lf phrases install-app`).

Edits ~/.localflow/hot_phrases.json directly through HotPhraseStore, so it
works without the server; the dictation app re-reads the file on change.
"""

import sys
import tkinter as tk
from pathlib import Path
from tkinter import messagebox, ttk

from localflow.commands import discover_commands
from localflow.config import load_config
from localflow.hotphrases import (
    HOT_PHRASES_PATH,
    HotPhraseError,
    HotPhraseStore,
    render_dictation,
)
from localflow.symbols import apply_spoken_symbols

_POLL_MS = 1500
_PREVIEW_DEBOUNCE_MS = 200
_ON, _OFF = "\u25cf", "\u25cb"  # ● ○


def _first_line(text: str, limit: int = 60) -> str:
    line = next((ln.strip() for ln in text.splitlines() if ln.strip()), "")
    return line if len(line) <= limit else line[: limit - 1] + "\u2026"


class HotPhrasesWindow:
    def __init__(self, root: tk.Tk, store: HotPhraseStore, config=None):
        self.root = root
        self.store = store
        self.config = config if config is not None else load_config()
        self.commands = (
            discover_commands(self.config.command_names, self.config.skills_dirs)
            if self.config.spoken_symbols
            else []
        )
        self.entries: dict[str, dict] = {}
        self.current_id: str | None = None
        self._loaded = ("", "", True)  # editor contents as last loaded/saved
        self._file_sig = None
        self._preview_job = None

        root.title("LocalFlow \u2014 Hot Phrases")
        root.geometry("940x620")
        root.minsize(720, 480)
        self._build()
        self._bind_keys()
        self.refresh()
        if not self.entries:
            self.new_phrase()
        self.root.after(_POLL_MS, self._poll)

    # ---------------------------------------------------------------- layout ---

    def _build(self) -> None:
        outer = ttk.Frame(self.root, padding=14)
        outer.pack(fill="both", expand=True)
        outer.columnconfigure(0, weight=2, uniform="cols")
        outer.columnconfigure(1, weight=3, uniform="cols")
        outer.rowconfigure(1, weight=1)

        intro = ttk.Label(
            outer,
            text=(
                "Say a trigger while dictating and LocalFlow pastes the saved text. "
                "On its own it pastes the text exactly; mid-sentence it expands in place."
            ),
            wraplength=880,
            justify="left",
        )
        intro.grid(row=0, column=0, columnspan=2, sticky="w", pady=(0, 10))
        outer.bind("<Configure>", lambda e: intro.configure(wraplength=max(300, e.width - 40)))

        # -- list --
        left = ttk.Frame(outer)
        left.grid(row=1, column=0, sticky="nsew", padx=(0, 12))
        left.rowconfigure(1, weight=1)
        left.columnconfigure(0, weight=1)
        ttk.Label(left, text="Hot phrases", style="Heading.TLabel").grid(
            row=0, column=0, sticky="w", pady=(0, 6)
        )
        self.tree = ttk.Treeview(
            left, columns=("on", "trigger", "preview"), show="headings",
            selectmode="browse",
        )
        self.tree.heading("on", text="On")
        self.tree.heading("trigger", text="Trigger")
        self.tree.heading("preview", text="Pastes")
        self.tree.column("on", width=44, minwidth=44, stretch=False, anchor="center")
        self.tree.column("trigger", width=150, minwidth=90)
        self.tree.column("preview", width=180, minwidth=90)
        self.tree.tag_configure("off", foreground="#8a8a8a")
        self.tree.grid(row=1, column=0, sticky="nsew")
        scroll = ttk.Scrollbar(left, orient="vertical", command=self.tree.yview)
        scroll.grid(row=1, column=1, sticky="ns")
        self.tree.configure(yscrollcommand=scroll.set)
        self.tree.bind("<<TreeviewSelect>>", self._on_select)
        self.tree.bind("<Button-1>", self._on_tree_click, add=True)
        self.tree.bind("<space>", lambda _e: self.toggle_selected())

        list_buttons = ttk.Frame(left)
        list_buttons.grid(row=2, column=0, columnspan=2, sticky="w", pady=(8, 0))
        ttk.Button(list_buttons, text="New", command=self.new_phrase).pack(side="left")
        self.toggle_btn = ttk.Button(
            list_buttons, text="Turn off", command=self.toggle_selected
        )
        self.toggle_btn.pack(side="left", padx=(6, 0))
        self.delete_btn = ttk.Button(
            list_buttons, text="Delete", command=self.delete_selected
        )
        self.delete_btn.pack(side="left", padx=(6, 0))

        # -- editor --
        right = ttk.Frame(outer)
        right.grid(row=1, column=1, sticky="nsew")
        right.columnconfigure(0, weight=1)
        right.rowconfigure(4, weight=1)
        self.editor_title = ttk.Label(right, text="New hot phrase", style="Heading.TLabel")
        self.editor_title.grid(row=0, column=0, sticky="w", pady=(0, 6))

        ttk.Label(right, text="Trigger \u2014 what you say").grid(row=1, column=0, sticky="w")
        self.trigger_var = tk.StringVar()
        self.trigger_entry = ttk.Entry(right, textvariable=self.trigger_var)
        self.trigger_entry.grid(row=2, column=0, sticky="ew", pady=(2, 10))

        ttk.Label(right, text="Pasted text").grid(row=3, column=0, sticky="w")
        text_frame = ttk.Frame(right)
        text_frame.grid(row=4, column=0, sticky="nsew", pady=(2, 8))
        text_frame.rowconfigure(0, weight=1)
        text_frame.columnconfigure(0, weight=1)
        self.text = tk.Text(
            text_frame, wrap="word", undo=True, height=10, relief="solid",
            borderwidth=1, padx=8, pady=6, highlightthickness=0,
        )
        self.text.grid(row=0, column=0, sticky="nsew")
        text_scroll = ttk.Scrollbar(text_frame, orient="vertical", command=self.text.yview)
        text_scroll.grid(row=0, column=1, sticky="ns")
        self.text.configure(yscrollcommand=text_scroll.set)

        self.enabled_var = tk.BooleanVar(value=True)
        ttk.Checkbutton(right, text="Enabled", variable=self.enabled_var).grid(
            row=5, column=0, sticky="w"
        )

        edit_buttons = ttk.Frame(right)
        edit_buttons.grid(row=6, column=0, sticky="w", pady=(8, 0))
        self.save_btn = ttk.Button(
            edit_buttons, text="Add", command=self.save, style="Accent.TButton"
        )
        self.save_btn.pack(side="left")
        self.revert_btn = ttk.Button(edit_buttons, text="Revert", command=self.revert)
        self.revert_btn.pack(side="left", padx=(6, 0))

        # -- try it --
        try_frame = ttk.LabelFrame(outer, text="Try it \u2014 type what you'd say", padding=10)
        try_frame.grid(row=2, column=0, columnspan=2, sticky="ew", pady=(14, 0))
        try_frame.columnconfigure(0, weight=1)
        self.try_var = tk.StringVar()
        self.try_entry = ttk.Entry(try_frame, textvariable=self.try_var)
        self.try_entry.grid(row=0, column=0, sticky="ew")
        self.try_out = tk.Text(
            try_frame, height=4, wrap="word", relief="flat", borderwidth=0,
            highlightthickness=0, padx=4, pady=6, state="disabled",
            background=self.root.cget("background"),
        )
        self.try_out.grid(row=1, column=0, sticky="ew", pady=(6, 0))
        self.try_var.trace_add("write", lambda *_: self._schedule_preview())

        self.status_var = tk.StringVar()
        self.status = ttk.Label(outer, textvariable=self.status_var)
        self.status.grid(row=3, column=0, columnspan=2, sticky="w", pady=(10, 0))

        self.trigger_var.trace_add("write", lambda *_: self._update_buttons())
        self.text.bind("<<Modified>>", self._on_text_modified)
        self.enabled_var.trace_add("write", lambda *_: self._update_buttons())

    def _bind_keys(self) -> None:
        mod = "Command" if sys.platform == "darwin" else "Control"
        self.root.bind_all(f"<{mod}-s>", lambda _e: (self.save(), "break")[1])
        self.root.bind_all(f"<{mod}-n>", lambda _e: (self.new_phrase(), "break")[1])
        self.root.bind_all(f"<{mod}-w>", lambda _e: self.close())
        self.root.protocol("WM_DELETE_WINDOW", self.close)

    # ----------------------------------------------------------------- state ---

    def _editor_values(self) -> tuple[str, str, bool]:
        return (
            self.trigger_var.get(),
            self.text.get("1.0", "end-1c"),
            bool(self.enabled_var.get()),
        )

    def dirty(self) -> bool:
        return self._editor_values() != self._loaded

    def _set_editor(self, trigger: str, text: str, enabled: bool) -> None:
        self.trigger_var.set(trigger)
        self.text.delete("1.0", "end")
        self.text.insert("1.0", text)
        self.text.edit_reset()
        self.text.edit_modified(False)
        self.enabled_var.set(enabled)
        self._loaded = (trigger, text, enabled)
        self._update_buttons()

    def _on_text_modified(self, _event=None) -> None:
        if self.text.edit_modified():
            self.text.edit_modified(False)
            self._update_buttons()

    def _update_buttons(self) -> None:
        is_dirty = self.dirty()
        editing = self.current_id is not None
        self.save_btn.configure(text="Save" if editing else "Add")
        self.save_btn.state(["!disabled"] if is_dirty else ["disabled"])
        self.revert_btn.state(["!disabled"] if is_dirty else ["disabled"])
        selected = self._selected_id()
        self.delete_btn.state(["!disabled"] if selected else ["disabled"])
        self.toggle_btn.state(["!disabled"] if selected else ["disabled"])
        if selected and selected in self.entries:
            self.toggle_btn.configure(
                text="Turn off" if self.entries[selected].get("enabled") else "Turn on"
            )
        title = (f"Edit \u201c{self._loaded[0]}\u201d" if editing else "New hot phrase")
        self.editor_title.configure(text=title + (" \u2022" if is_dirty else ""))

    def _flash(self, message: str, error: bool = False) -> None:
        self.status_var.set(message)
        self.status.configure(foreground="#c0392b" if error else "")

    def _selected_id(self) -> str | None:
        sel = self.tree.selection()
        return sel[0] if sel else None

    def _confirm_discard(self) -> bool:
        if not self.dirty():
            return True
        return messagebox.askyesno(
            "Discard changes?",
            "You have unsaved changes to this hot phrase. Discard them?",
            parent=self.root,
        )

    # ------------------------------------------------------------------ list ---

    def _file_signature(self):
        try:
            st = self.store.path.stat()
        except OSError:
            return None
        return (st.st_mtime_ns, st.st_size)

    def refresh(self) -> None:
        self._file_sig = self._file_signature()
        keep = self.current_id
        entries = self.store.list()
        self.entries = {e["id"]: e for e in entries}
        self.tree.delete(*self.tree.get_children())
        for e in entries:
            self.tree.insert(
                "", "end", iid=e["id"],
                values=(_ON if e.get("enabled") else _OFF, e["trigger"], _first_line(e["text"])),
                tags=() if e.get("enabled") else ("off",),
            )
        if keep in self.entries:
            self.tree.selection_set(keep)
            self.tree.see(keep)
        if self.current_id is not None and self.current_id not in self.entries:
            # deleted elsewhere (CLI / another window)
            self.current_id = None
            if not self.dirty():
                self._set_editor("", "", True)
        self._update_buttons()
        self._schedule_preview()

    def _poll(self) -> None:
        try:
            if self._file_signature() != self._file_sig:
                self.refresh()
                current = self.entries.get(self.current_id) if self.current_id else None
                if current is not None and not self.dirty():
                    self._load_entry(current)
        finally:
            self.root.after(_POLL_MS, self._poll)

    def _load_entry(self, entry: dict) -> None:
        self.current_id = entry["id"]
        self._set_editor(entry["trigger"], entry["text"], bool(entry.get("enabled")))

    def _on_select(self, _event=None) -> None:
        selected = self._selected_id()
        if selected is None or selected == self.current_id:
            self._update_buttons()
            return
        if not self._confirm_discard():
            if self.current_id in self.entries:
                self.tree.selection_set(self.current_id)
            else:
                self.tree.selection_remove(selected)
            return
        self._load_entry(self.entries[selected])
        self._flash("")

    def _on_tree_click(self, event) -> str | None:
        if self.tree.identify_region(event.x, event.y) != "cell":
            return None
        if self.tree.identify_column(event.x) != "#1":
            return None
        row = self.tree.identify_row(event.y)
        if row:
            self._set_enabled(row, not self.entries[row].get("enabled"))
            return "break"
        return None

    # --------------------------------------------------------------- actions ---

    def new_phrase(self) -> None:
        if not self._confirm_discard():
            return
        self.current_id = None
        self.tree.selection_remove(*self.tree.selection())
        self._set_editor("", "", True)
        self._flash("")
        self.trigger_entry.focus_set()

    def save(self) -> None:
        if not self.dirty():
            return
        trigger, text, enabled = self._editor_values()
        try:
            if self.current_id is None:
                entry = self.store.add(trigger, text, enabled)
                verb = "Added"
            else:
                entry = self.store.update(
                    self.current_id, trigger=trigger, text=text, enabled=enabled
                )
                verb = "Saved"
                if entry is None:
                    self._flash("This hot phrase was deleted elsewhere; save again to re-add it.", True)
                    self.current_id = None
                    self.refresh()
                    return
        except HotPhraseError as exc:
            msg = str(exc)
            self._flash(msg[:1].upper() + msg[1:], error=True)
            return
        self.current_id = entry["id"]
        self.trigger_var.set(entry["trigger"])  # stored form (stripped)
        self._loaded = self._editor_values()
        self.refresh()
        self._flash(f"{verb} \u201c{entry['trigger']}\u201d")

    def revert(self) -> None:
        if self.current_id in self.entries:
            self._load_entry(self.entries[self.current_id])
        else:
            self._set_editor("", "", True)
        self._flash("")

    def _set_enabled(self, entry_id: str, enabled: bool) -> None:
        entry = self.store.update(entry_id, enabled=enabled)
        if entry is None:
            self.refresh()
            return
        if entry_id == self.current_id:
            loaded_trigger, loaded_text, _ = self._loaded
            # keep unsaved trigger/text edits; only the checkbox follows the list
            self.enabled_var.set(enabled)
            self._loaded = (loaded_trigger, loaded_text, enabled)
        self.refresh()
        self._flash(f"\u201c{entry['trigger']}\u201d turned {'on' if enabled else 'off'}")

    def toggle_selected(self) -> None:
        selected = self._selected_id()
        if selected in self.entries:
            self._set_enabled(selected, not self.entries[selected].get("enabled"))

    def delete_selected(self) -> None:
        selected = self._selected_id()
        if selected not in self.entries:
            return
        trigger = self.entries[selected]["trigger"]
        if not messagebox.askyesno(
            "Delete hot phrase?", f"Delete \u201c{trigger}\u201d? This can't be undone.",
            parent=self.root,
        ):
            return
        self.store.delete(selected)
        if selected == self.current_id:
            self.current_id = None
            self._set_editor("", "", True)
        self.refresh()
        self._flash(f"Deleted \u201c{trigger}\u201d")

    def close(self) -> None:
        if self._confirm_discard():
            self.root.destroy()

    # --------------------------------------------------------------- preview ---

    def _schedule_preview(self) -> None:
        if self._preview_job is not None:
            self.root.after_cancel(self._preview_job)
        self._preview_job = self.root.after(_PREVIEW_DEBOUNCE_MS, self._render_preview)

    def preview_text(self, said: str) -> str:
        """What dictation would paste for `said` (minus Ollama cleanup)."""
        symbols = None
        if self.config.spoken_symbols:
            symbols = lambda t: apply_spoken_symbols(t, self.commands)  # noqa: E731
        phrases = self.store.enabled() if self.config.hot_phrases else []
        return render_dictation(said, phrases, symbols=symbols)

    def _render_preview(self) -> None:
        self._preview_job = None
        said = self.try_var.get()
        out = self.preview_text(said) if said.strip() else ""
        if said.strip() and not self.config.hot_phrases:
            out = "(hot phrases are disabled: hot_phrases = false in ~/.localflow.toml)"
        self.try_out.configure(state="normal")
        self.try_out.delete("1.0", "end")
        self.try_out.insert("1.0", out)
        lines = int(self.try_out.index("end-1c").split(".")[0])
        self.try_out.configure(height=max(2, min(lines, 8)), state="disabled")


def _style(root: tk.Tk) -> None:
    style = ttk.Style(root)
    if sys.platform != "darwin" and "clam" in style.theme_names():
        style.theme_use("clam")
    style.configure("Heading.TLabel", font="TkHeadingFont")
    style.configure("Accent.TButton", padding=(14, 4))


def main(path: Path | None = None) -> None:
    root = tk.Tk()
    _style(root)
    HotPhrasesWindow(root, HotPhraseStore(path or HOT_PHRASES_PATH))
    root.lift()
    root.attributes("-topmost", True)
    root.after(300, lambda: root.attributes("-topmost", False))
    root.mainloop()


if __name__ == "__main__":
    main()
