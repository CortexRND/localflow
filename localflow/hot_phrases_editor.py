"""AppKit-free state behind the Hot Phrases page of the native Hub.

`HubWindowController` (hub_mac) only renders this object and forwards user
events to it. All behaviour that must be tested without macOS lives here:
autosave debounce, validation through `PhrasesModel`, the navigation guard,
selection, search, delete/undo, external-change reconciliation and the
dictation preview. Nothing here imports AppKit, Tk or the network.

Vocabulary used below:
  * "pending": the editor values differ from what was last loaded/committed.
  * "draft": a phrase that has no saved record yet (`current_id is None`).
Local edits always win over external changes, and an invalid edit is never
written: it stays in the editor with an inline message.
"""

import time

AUTOSAVE_DELAY = 0.6
POLL_INTERVAL = 1.5
UNDO_SECONDS = 6.0

EMPTY_MESSAGE = "Say a word, get a saved prompt. Add your first hot phrase."
READ_ERROR = (
    "Couldn't read your hot phrases. Check that ~/.localflow is readable, "
    "then try again."
)
WRITE_ERROR = "Couldn't save your hot phrases. Your edits are kept here; try again."
DELETED_ELSEWHERE = (
    "This phrase was deleted elsewhere. Your edits will be saved as a new phrase."
)
PREVIEW_PROMPT = "Type a test phrase to see what dictation would paste."
PREVIEW_ERROR = "Preview isn't available right now."
# Drawn (never inserted) in the empty read-only preview box.
PREVIEW_PLACEHOLDER = "What dictation would paste appears here."
PREVIEW_EMPTY_RESULT = "Nothing would be pasted."
# The switch's visible label follows its state; "Off" fits ENABLED_LABEL_W.
ENABLED_LABELS = {True: "On", False: "Off"}
ENABLED_ACCESSIBILITY = "Phrase enabled"


def enabled_label(enabled) -> str:
    """The text beside the enabled switch for its current state."""
    return ENABLED_LABELS[bool(enabled)]


def preview_placeholder(test_input, output, status) -> str:
    """Muted text drawn in the preview box only while it has no output.

    No test phrase: say what the box is for. A test phrase whose result is
    empty: say so, rather than leaving a blank box. Real output: nothing (the
    output itself is shown, unchanged). A preview error is already in the
    status line, so the box stays plain.
    """
    if output:
        return ""
    if not (test_input or "").strip():
        return PREVIEW_PLACEHOLDER
    if status == PREVIEW_ERROR:
        return ""
    return PREVIEW_EMPTY_RESULT
DISABLED_BANNER = (
    "Hot phrases are turned off in your settings, so they won't expand yet."
)

EXAMPLE_TRIGGER = "review checklist"
EXAMPLE_TEXT = (
    "Review this PR:\n"
    "- correctness and edge cases\n"
    "- tests cover the change\n"
    "- no unrelated churn\n"
    "Report findings by severity."
)

_FAILED = object()


class HotPhrasesEditor:
    """State and actions for the embedded editor; `clock` is injectable."""

    def __init__(self, model, clock=time.monotonic):
        self.model = model
        self._clock = clock
        self.query = ""
        self.current_id = None
        self.draft = False
        self.trigger = ""
        self.text = ""
        self.enabled = True
        self.loaded = ("", "", True)
        self.touched = False
        self.error = ""
        self.notice = ""
        self.undo_token = None
        self.undo_due = None
        self.autosave_due = None
        self.next_poll = 0.0
        self.test_input = ""
        self.all_entries = []
        self.entries = []
        # The view pushes trigger/text/enabled into its controls when this is
        # set (programmatic loads), then calls `ack_fields`. Typing never sets
        # it, so the caret is never disturbed.
        self.fields_stale = True
        self.reload()

    # ------------------------------------------------------------ helpers ---

    def _call(self, function, *args, message=WRITE_ERROR):
        """(True, value) or (False, None) with a friendly `error` set."""
        try:
            return True, function(*args)
        except Exception:  # noqa: BLE001 - a store failure must not kill the event loop
            self.error = message
            return False, None

    def ack_fields(self):
        self.fields_stale = False

    def _values(self):
        return (self.trigger.strip(), self.text, bool(self.enabled))

    def pending(self):
        return self._values() != self.loaded

    def blank(self):
        return not self.trigger.strip() and not self.text.strip()

    def is_untouched_draft(self):
        return self.current_id is None and self.blank()

    @property
    def empty_state_visible(self):
        return not self.all_entries and self.current_id is None and not self.draft

    @property
    def editor_visible(self):
        return not self.empty_state_visible

    def reload(self):
        """Re-read the entry list. A failing store keeps the previous list."""
        ok, everything = self._call(self.model.entries, message=READ_ERROR)
        if not ok:
            return False
        self.all_entries = list(everything)
        if self.query:
            ok, visible = self._call(
                self.model.entries, self.query, message=READ_ERROR
            )
            self.entries = list(visible) if ok else self.entries
        else:
            self.entries = self.all_entries
        return True

    def rows(self):
        """List rows for the view: id, title, subtitle, enabled, selected."""
        rows = []
        for entry in self.entries:
            first = next(
                (ln.strip() for ln in entry["text"].splitlines() if ln.strip()), ""
            )
            enabled = bool(entry.get("enabled"))
            rows.append(
                {
                    "id": entry["id"],
                    "title": entry["trigger"],
                    "subtitle": first,
                    "enabled": enabled,
                    "selected": entry["id"] == self.current_id,
                    "accessibility": (
                        f"{entry['trigger']}, {'on' if enabled else 'off'}"
                    ),
                }
            )
        return rows

    @property
    def no_matches(self):
        return bool(self.query) and not self.entries and bool(self.all_entries)

    # -------------------------------------------------------------- hints ---

    def _check_trigger(self):
        """(ok, message). The duplicate check reads the store, which can fail
        (permissions, lock); that becomes a friendly read error, never a raise."""
        try:
            return self.model.trigger_hint(self.trigger, exclude_id=self.current_id)
        except Exception:  # noqa: BLE001
            return False, READ_ERROR

    def _check_text(self):
        try:
            return self.model.text_hint(self.text)
        except Exception:  # noqa: BLE001
            return False, READ_ERROR

    def trigger_hint(self):
        """(message, is_error). Neutral until the user has touched the form."""
        ok, message = self._check_trigger()
        return message, (not ok and (self.touched or bool(self.error)))

    def text_hint(self):
        ok, message = self._check_text()
        return message, (not ok and (self.touched or bool(self.error)))

    def _first_problem(self):
        for ok, message in (self._check_trigger(), self._check_text()):
            if not ok:
                return message
        return ""

    @property
    def banner(self):
        return "" if self.model.hot_phrases_enabled else DISABLED_BANNER

    # ------------------------------------------------------------ editing ---

    def edit(self, trigger=None, text=None, enabled=None):
        """The user changed a field. Debounces an autosave."""
        if trigger is not None:
            self.trigger = trigger
        if text is not None:
            self.text = text
        if enabled is not None:
            self.enabled = bool(enabled)
        self.touched = True
        self.notice = ""
        self.error = ""
        if self.pending():
            self.autosave_due = self._clock() + AUTOSAVE_DELAY
        else:
            self.autosave_due = None

    def fill(self, trigger, text):
        """Programmatic typing (snapshots, tests): like `edit`, but the view is
        told to push the values into its controls, which hold stale text."""
        self.edit(trigger=trigger, text=text)
        self.fields_stale = True

    def _commit(self):
        """(entry, None) or (None, message); handles a record deleted elsewhere."""
        if self.current_id is not None:
            ok, existing = self._call(self.model.get, self.current_id)
            if not ok:
                return None, self.error
            if existing is None:
                self.current_id = None
                self.draft = True
        ok, result = self._call(
            self.model.commit, self.current_id, self.trigger, self.text, self.enabled
        )
        if not ok:
            return None, self.error
        entry, message = result
        return (entry, None) if message is None else (None, message)

    def _adopt(self, entry):
        self.current_id = entry["id"]
        self.draft = False
        self.loaded = (entry["trigger"], entry["text"], bool(entry.get("enabled")))
        self.error = ""
        self.notice = "Saved ✓"
        self.autosave_due = None
        self.reload()

    def flush(self):
        """Commit now. True when nothing remains unsaved; invalid edits stay
        local and set `error`."""
        self.autosave_due = None
        if self.is_untouched_draft():
            return True
        if not self.pending():
            return True
        problem = self._first_problem()
        if problem:
            self.error = problem
            return False
        entry, message = self._commit()
        if entry is None:
            self.error = message or WRITE_ERROR
            return False
        self._adopt(entry)
        return True

    def set_enabled(self, value):
        """The switch changed: persist it right away (invalid drafts stay local)."""
        value = bool(value)
        self.enabled = value
        self.touched = True
        self.notice = ""
        self.error = ""
        if self.current_id is None:
            return self.flush()
        ok, existing = self._call(self.model.get, self.current_id)
        if not ok:
            return False
        if existing is None:
            self.current_id = None
            self.draft = True
            return self.flush()
        if value == self.loaded[2]:
            return True
        ok, updated = self._call(self.model.set_enabled, self.current_id, value)
        if not ok:
            return False
        if updated is None:
            self.current_id = None
            self.draft = True
            return self.flush()
        # Only the switch was persisted: other pending edits stay pending.
        self.loaded = (self.loaded[0], self.loaded[1], value)
        self.notice = "Saved ✓"
        self.reload()
        if self.pending():
            self.autosave_due = self._clock() + AUTOSAVE_DELAY
        return True

    # --------------------------------------------------------- navigation ---

    def leave(self):
        """Guard for leaving the editor (page change, selection, new phrase,
        window close). Commits a valid dirty draft; True means "go ahead".

        An untouched empty draft never traps the user. An invalid edit keeps
        the user here with a friendly inline message and nothing is lost.
        """
        self.autosave_due = None
        if self.is_untouched_draft():
            self.draft = False
            self.error = ""
            return True
        if not self.pending():
            return True
        if self.flush():
            return True
        message = self.error or WRITE_ERROR
        self.error = message
        if message == READ_ERROR:
            self.error = f"{message} Your edits are kept here."
        elif message != WRITE_ERROR:  # WRITE_ERROR already says edits are kept
            self.error = f"{message} Fix it to continue, or clear the fields."
        return False

    def cancel(self):
        """Stop pending autosave/undo timers (page hidden or window closed)."""
        self.autosave_due = None

    def _load(self, entry):
        self.current_id = entry["id"]
        self.draft = False
        self.trigger = entry["trigger"]
        self.text = entry["text"]
        self.enabled = bool(entry.get("enabled"))
        self.loaded = (self.trigger, self.text, self.enabled)
        self.touched = False
        self.error = ""
        self.notice = ""
        self.autosave_due = None
        self.fields_stale = True

    def _clear(self, *, draft):
        self.current_id = None
        self.draft = draft
        self.trigger = ""
        self.text = ""
        self.enabled = True
        self.loaded = ("", "", True)
        self.touched = False
        self.error = ""
        self.autosave_due = None
        self.fields_stale = True

    def _settle(self, old_index):
        """Select the neighbour at `old_index` (clamped), else show empty."""
        pool = self.entries or self.all_entries
        if pool:
            self._load(pool[min(max(old_index, 0), len(pool) - 1)])
        else:
            self._clear(draft=False)

    def _index_of(self, entry_id):
        return next(
            (i for i, e in enumerate(self.entries) if e["id"] == entry_id), 0
        )

    def select(self, entry_id):
        if entry_id == self.current_id:
            return True
        if not self.leave():
            return False
        ok, entry = self._call(self.model.get, entry_id)
        if not ok:
            return False
        if entry is None:
            self.reload()
            self.error = ""
            return True
        self._load(entry)
        return True

    def new_phrase(self):
        if self.current_id is None and self.draft and self.blank():
            return True
        if not self.leave():
            return False
        self._clear(draft=True)
        self.notice = ""
        return True

    def add_example(self):
        if not self.leave():
            return False
        ok, result = self._call(
            self.model.commit, None, EXAMPLE_TRIGGER, EXAMPLE_TEXT, True
        )
        if not ok:
            return False
        entry, message = result
        if entry is None:
            self.error = message or WRITE_ERROR
            return False
        self.reload()
        self._load(entry)
        return True

    def set_query(self, query):
        """Search is live. It filters the list only: the editor (and any
        invalid draft in it) is untouched, so nothing can be lost."""
        self.query = query or ""
        self.reload()

    # ------------------------------------------------------ delete / undo ---

    def delete(self):
        if self.current_id is None:
            if self.draft or not self.blank():
                self._clear(draft=False)
                self.reload()
                self._settle(0)
            return True
        old_index = self._index_of(self.current_id)
        ok, token = self._call(self.model.delete, self.current_id)
        if not ok:
            return False
        if token is not None:
            self.undo_token = token
            self.undo_due = self._clock() + UNDO_SECONDS
        self.reload()
        self._settle(old_index)
        return True

    def undo_delete(self):
        token = self.undo_token
        if token is None:
            return False
        # The token is cleared only on success (or expiry), so a failed restore
        # can be retried while the Undo bar is still showing.
        ok, result = self._call(self.model.undo_delete, token)
        if not ok:
            return False
        entry, message = result
        if entry is None:
            self.error = message or WRITE_ERROR
            return False
        self.undo_token = self.undo_due = None
        self.error = ""
        self.reload()
        if not self.select(entry["id"]):
            self.notice = f"Restored “{entry['trigger']}”"
        return True

    @property
    def undo_label(self):
        if self.undo_token is None:
            return ""
        return f"Deleted “{self.undo_token['trigger']}”"

    @property
    def notice_visibility(self):
        """(error shown, Undo shown): the view re-lays out only when this changes."""
        return (bool(self.error), self.undo_token is not None)

    # ---------------------------------------------------- external changes ---

    def on_show(self):
        """The page became visible: prime the disk signature, reconcile."""
        self._call(self.model.changed_on_disk, message=READ_ERROR)
        self.next_poll = self._clock() + POLL_INTERVAL
        self.reconcile()

    def poll(self):
        ok, changed = self._call(self.model.changed_on_disk, message=READ_ERROR)
        if not ok or not changed:
            return False
        self.reconcile()
        return True

    def reconcile(self):
        """Merge the on-disk state: dirty local edits win, clean ones refresh."""
        old_index = self._index_of(self.current_id) if self.current_id else 0
        if not self.reload():
            return
        if self.current_id is not None:
            ok, current = self._call(self.model.get, self.current_id, message=READ_ERROR)
            if not ok:
                return
            if current is None:
                if self.pending():
                    # Keep the draft; the next valid commit re-adds it.
                    self.current_id = None
                    self.draft = True
                    self.error = DELETED_ELSEWHERE
                else:
                    self._settle(old_index)
            elif not self.pending() and (
                current["trigger"], current["text"], bool(current.get("enabled"))
            ) != self.loaded:
                self._load(current)
            return
        if not self.draft:
            self._settle(0)

    def tick(self):
        """Timer hook. True when something visible changed."""
        now = self._clock()
        changed = False
        if self.undo_due is not None and now >= self.undo_due:
            self.undo_token = None
            self.undo_due = None
            changed = True
        if self.autosave_due is not None and now >= self.autosave_due:
            self.flush()
            changed = True
        if now >= self.next_poll:
            self.next_poll = now + POLL_INTERVAL
            changed = self.poll() or changed
        return changed

    # ------------------------------------------------------------ preview ---

    def set_test_input(self, said):
        self.test_input = said or ""

    def preview(self):
        """(output, status) for the typed test phrase; no LLM, no network."""
        if not self.test_input.strip():
            return "", PREVIEW_PROMPT
        try:
            output, matched = self.model.preview(self.test_input)
        except Exception:  # noqa: BLE001 - never crash the event loop
            return "", PREVIEW_ERROR
        if matched:
            quoted = "”, “".join(matched)
            return output, f"Matched: “{quoted}”"
        return output, "No hot phrase matched"


# ------------------------------------------------------------------ layout ---

PAD = 16
GAP = 16
SEARCH_H = 32
BUTTON_H = 36
LIST_TOP = SEARCH_H + 8 + BUTTON_H + 8
LIST_MIN_W = 190
LIST_MAX_W = 300
TEXT_MIN_H = 110


ENABLED_LABEL_W = 64
ENABLED_NOTICE_X = PAD + 48 + ENABLED_LABEL_W
NOTICE_GAP = 8  # between the enabled row and an error / Undo notice below it


def _editor_rows(inner, text_h, error_visible=False, undo_visible=False):
    """Card-relative frames (x, y, w, h) for the editor controls, top down.

    The error and Undo notices take room only while they are visible; hidden
    ones are absent from the result, so "Try it" sits directly below the
    enabled row. A short status ("Saved ✓") is shown inline in the enabled row
    and never moves anything.
    """
    rows = {}
    y = PAD
    rows["trigger_label"] = (PAD, y, inner, 18)
    y += 20
    rows["trigger_field"] = (PAD, y, inner, 32)
    y += 36
    rows["trigger_hint"] = (PAD, y, inner, 16)
    y += 24
    rows["text_label"] = (PAD, y, inner, 18)
    y += 20
    rows["text_scroll"] = (PAD, y, inner, text_h)
    y += text_h + 4
    rows["text_hint"] = (PAD, y, inner, 16)
    y += 28
    rows["enabled_switch"] = (PAD, y + 5, 40, 22)
    rows["enabled_label"] = (PAD + 48, y + 7, ENABLED_LABEL_W, 18)
    rows["enabled_notice"] = (
        ENABLED_NOTICE_X, y + 7, max(inner - 48 - ENABLED_LABEL_W - 96, 40), 18,
    )
    rows["delete_button"] = (PAD + inner - 88, y, 88, 32)
    y += 40
    if error_visible:
        rows["error_label"] = (PAD, y, inner, 34)
        y += 34 + NOTICE_GAP
    if undo_visible:
        rows["undo_bar"] = (PAD, y, inner, 36)
        y += 36 + NOTICE_GAP
    rows["preview_label"] = (PAD, y, inner, 18)
    y += 20
    rows["test_field"] = (PAD, y, inner, 32)
    y += 36
    rows["preview_output"] = (PAD, y, inner, 56)
    y += 60
    rows["preview_status"] = (PAD, y, inner, 16)
    y += 16 + PAD
    return rows, y


def hot_phrases_layout(
    width, available_height, *, error_visible=False, undo_visible=False,
):
    """Frames for the two-column page, relative to the page origin.

    Width-aware: the list column is 30% of the page (190-300). The editor
    takes the rest. Height-aware: the text box grows into spare height; when
    even the minimum does not fit, the page grows and the Hub scrolls it, so
    controls are never squeezed, overlapped or clipped. Error and Undo notices
    reserve height only while visible (the text box gives it up and takes it
    back), so "Try it" stays right below the enabled row.
    """
    width = max(int(width), LIST_MIN_W + GAP + 300)
    list_w = min(LIST_MAX_W, max(LIST_MIN_W, round(width * 0.3)))
    editor_x = list_w + GAP
    editor_w = width - editor_x
    inner = editor_w - 2 * PAD
    _, needed = _editor_rows(inner, TEXT_MIN_H, error_visible, undo_visible)
    height = max(int(available_height), needed, LIST_TOP + 120)
    text_h = TEXT_MIN_H + (height - needed)
    card, _ = _editor_rows(inner, text_h, error_visible, undo_visible)
    return {
        "height": height,
        "search": (0, 0, list_w, SEARCH_H),
        "new": (0, SEARCH_H + 8, list_w, BUTTON_H),
        "list": (0, LIST_TOP, list_w, height - LIST_TOP),
        "card": (editor_x, 0, editor_w, height),
        "card_rows": card,
    }


def empty_layout(width, *, error_visible=False, undo_visible=False):
    """(frames, card height) for the empty-state card; notices only when shown."""
    inner = width - 2 * PAD
    rows = {
        "message": (PAD, PAD + 8, inner, 48),
        "detail": (PAD, PAD + 62, inner, 36),
        "new": (PAD, 124, 150, BUTTON_H),
        "example": (PAD + 162, 124, 160, BUTTON_H),
    }
    y = 124 + BUTTON_H
    if error_visible:
        rows["error"] = (PAD, y + NOTICE_GAP, inner, 34)
        y += NOTICE_GAP + 34
    if undo_visible:
        rows["undo"] = (PAD, y + NOTICE_GAP, inner, 36)
        y += NOTICE_GAP + 36
    return rows, y + PAD
