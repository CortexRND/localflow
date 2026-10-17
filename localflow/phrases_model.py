"""UI-free model behind the hot-phrase editor windows.

Both the AppKit window (phrases_mac) and the Tk window (phrases_tk) talk to
a PhrasesModel: validation hints, commit/delete/undo, the dictation
preview, and on-disk change detection all live here so the views stay thin.
"""

from localflow.commands import discover_commands
from localflow.config import load_config
from localflow.hotphrases import (
    MAX_TEXT_CHARS,
    MAX_TRIGGER_CHARS,
    MAX_TRIGGER_WORDS,
    HotPhraseError,
    HotPhraseStore,
    matched_triggers,
    normalize_trigger,
    render_dictation,
)
from localflow.symbols import apply_spoken_symbols

_UNSET = object()


class PhrasesModel:
    def __init__(self, store: HotPhraseStore, config=None):
        self.store = store
        self.config = config if config is not None else load_config()
        self.commands = (
            discover_commands(self.config.command_names, self.config.skills_dirs)
            if self.config.spoken_symbols
            else []
        )
        self._file_sig = _UNSET

    @property
    def hot_phrases_enabled(self) -> bool:
        return bool(self.config.hot_phrases)

    # ----------------------------------------------------------------- data ---

    def entries(self, query: str = "") -> list[dict]:
        entries = sorted(self.store.list(), key=lambda e: e["trigger"].casefold())
        if query:
            needle = query.casefold()
            entries = [
                e
                for e in entries
                if needle in e["trigger"].casefold() or needle in e["text"].casefold()
            ]
        return entries

    def get(self, entry_id: str) -> dict | None:
        for entry in self.store.list():
            if entry.get("id") == entry_id:
                return entry
        return None

    # ----------------------------------------------------------------- hints ---

    def trigger_hint(self, trigger: str, exclude_id: str | None = None) -> tuple[bool, str]:
        """(ok, message) for the trigger field — mirrors _validate_trigger and
        the duplicate check in HotPhraseStore so the UI can warn early."""
        stripped = trigger.strip()
        if not stripped:
            return False, "Type the words you'll say"
        if len(stripped) > MAX_TRIGGER_CHARS:
            return False, f"Keep it under {MAX_TRIGGER_CHARS} characters"
        normalized = normalize_trigger(stripped)
        if not normalized:
            return False, "Use at least one word"
        words = normalized.split()
        if len(words) > MAX_TRIGGER_WORDS:
            return False, f"Use {MAX_TRIGGER_WORDS} words or fewer"
        for other in self.store.list():
            if other.get("id") == exclude_id:
                continue
            if normalize_trigger(other.get("trigger", "")) == normalized:
                return False, f"Already used by \u201c{other['trigger']}\u201d"
        n = len(words)
        hint = f"\u2713 {n} word" + ("s" if n != 1 else "")
        if normalized != " ".join(stripped.lower().split()):
            hint += f" \u00b7 matches \u201c{normalized}\u201d (case and punctuation ignored)"
        return True, hint

    def text_hint(self, text: str) -> tuple[bool, str]:
        if not text or not text.strip():
            return False, "Type the text to paste"
        if len(text) > MAX_TEXT_CHARS:
            return False, f"Keep it under {MAX_TEXT_CHARS:,} characters"
        lines = len(text.splitlines()) or 1
        line_word = "line" if lines == 1 else "lines"
        char_word = "character" if len(text) == 1 else "characters"
        return True, f"{lines} {line_word} \u00b7 {len(text):,} {char_word}"

    # ---------------------------------------------------------------- actions ---

    def commit(
        self,
        entry_id: str | None,
        trigger: str,
        text: str,
        enabled: bool,
    ) -> tuple[dict | None, str | None]:
        """Validate and write. Returns (entry, None) or (None, error message);
        an invalid draft writes nothing and a no-op commit skips the write."""
        ok, message = self.trigger_hint(trigger, exclude_id=entry_id)
        if not ok:
            return None, message
        ok, message = self.text_hint(text)
        if not ok:
            return None, message

        stripped = trigger.strip()
        if entry_id is not None:
            existing = self.get(entry_id)
            if existing is not None and (
                existing["trigger"] == stripped
                and existing["text"] == text
                and bool(existing.get("enabled")) == bool(enabled)
            ):
                return existing, None
        try:
            if entry_id is None:
                entry = self.store.add(stripped, text, enabled)
            else:
                entry = self.store.update(
                    entry_id, trigger=stripped, text=text, enabled=enabled
                )
        except HotPhraseError as exc:
            # Race guard: the hints checked a moment ago; another process may
            # have claimed the trigger since.
            return None, str(exc)
        if entry is None:
            return None, "This hot phrase was deleted elsewhere"
        return entry, None

    def set_enabled(self, entry_id: str, enabled: bool) -> dict | None:
        return self.store.update(entry_id, enabled=enabled)

    def delete(self, entry_id: str) -> dict | None:
        """Delete and return the entry as an undo token."""
        entry = self.get(entry_id)
        if entry is None or not self.store.delete(entry_id):
            return None
        return entry

    def undo_delete(self, token: dict) -> tuple[dict | None, str | None]:
        """Re-add a deleted entry (a fresh id is fine)."""
        try:
            return (
                self.store.add(
                    token["trigger"], token["text"], bool(token.get("enabled"))
                ),
                None,
            )
        except HotPhraseError as exc:
            return None, str(exc)

    # ---------------------------------------------------------------- preview ---

    def preview(self, said: str) -> tuple[str, list[str]]:
        """What dictation would paste for `said` (minus Ollama cleanup), plus
        the enabled triggers that matched."""
        symbols = None
        if self.config.spoken_symbols:
            symbols = lambda t: apply_spoken_symbols(t, self.commands)  # noqa: E731
        phrases = self.store.enabled() if self.config.hot_phrases else []
        return render_dictation(said, phrases, symbols=symbols), matched_triggers(
            said, phrases
        )

    # ------------------------------------------------------------- disk watch ---

    def _file_signature(self):
        try:
            st = self.store.path.stat()
        except OSError:
            return None
        return (st.st_mtime_ns, st.st_size)

    def changed_on_disk(self) -> bool:
        """True when the store file changed since the last call. The first
        call primes the signature and returns False."""
        sig = self._file_signature()
        if self._file_sig is _UNSET:
            self._file_sig = sig
            return False
        changed = sig != self._file_sig
        self._file_sig = sig
        return changed
