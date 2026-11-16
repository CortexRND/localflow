"""Settings page of the Hub: state and geometry, without AppKit.

hub_settings_mac renders this; tests drive it directly. The page asks the
local Settings agent (settings_agent) for a preview off the UI thread; a
result is only accepted for the request that is still current (`token`),
so navigating away, resizing, closing the window or asking again never lets
a late answer overwrite newer state.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from localflow.text_wrap import wrap_lines

HEADING = "Tell LocalFlow what to change"
PLACEHOLDER = (
    "e.g. “Turn off sounds”, “Dictate with Right Command”, "
    "“Add Kubernetes to my dictionary”, “Make work messages casual”"
)
INTRO = "Describe a change in plain words. Nothing changes until you press Apply."
EMPTY_HINT = (
    "Your request goes to a model running on this Mac. It can only change the "
    "settings listed here and add dictionary words or replacements."
)
NOTE_UNKNOWN = "Runs on your Mac with a local Ollama model"
BUSY_PREVIEW = "Asking the local model…"
CURRENT_TITLE = "Current settings"
PREVIEW_TITLE = "Preview"
APPLIED_TITLE = "Saved"
ERROR_TITLE = "Couldn't preview that"
UNDO_TITLE = "Undo last change"
MAX_DRAFT_CHARS = 500


def model_note(model: str | None) -> str:
    return f"Runs on your Mac with {model}" if model else NOTE_UNKNOWN


@dataclass
class SettingsPageState:
    """Everything the page shows; survives navigation, resize and appearance."""

    draft: str = ""
    status: str = "empty"          # empty | busy | preview | applied | error
    proposal: object = None        # settings_agent.Proposal while previewing
    lines: list[str] = field(default_factory=list)   # preview/applied lines
    notes: list[str] = field(default_factory=list)   # rejected or skipped
    message: str = ""
    model: str | None = None
    token: int = 0
    pending: str | None = None     # the request in flight, if any
    undo_lines: list[str] | None = None

    @property
    def busy(self) -> bool:
        return self.status == "busy"

    def begin_preview(self, request: str) -> int | None:
        """Start a preview; returns its token, or None when there's nothing to ask."""
        text = (request or "").strip()
        self.draft = request or ""
        if not text or self.busy:
            return None
        self.token += 1
        self.pending = text
        self.status = "busy"
        self.proposal = None
        self.lines, self.notes, self.message = [], [], ""
        return self.token

    def finish_preview(self, token: int, proposal=None, error: str | None = None,
                       model: str | None = None) -> bool:
        """Accept a worker result; False (ignored) when it is stale."""
        if token != self.token or self.status != "busy":
            return False
        self.pending = None
        if model:
            self.model = model
        if error is not None:
            self.status, self.message = "error", error
            return True
        self.proposal = proposal
        self.model = getattr(proposal, "model", None) or self.model
        self.lines = [change.line() for change in proposal.changes]
        self.notes = list(proposal.rejected)
        self.status = "preview"
        return True

    def cancel(self) -> None:
        """Drop the preview (or an in-flight request: its answer is ignored)."""
        self.token += 1
        self.pending = None
        self.proposal = None
        self.lines, self.notes, self.message = [], [], ""
        self.status = "empty"

    def applied(self, result) -> None:
        self.proposal = None
        self.lines = [change.line() for change in result.applied]
        self.notes = []
        self.message = result.message
        self.status = "applied"
        self.draft = ""

    def undone(self, result) -> None:
        self.proposal = None
        self.lines = list(result.reverted)
        self.notes = list(result.skipped)
        self.message = result.message
        self.status = "applied"

    def failed(self, message: str) -> None:
        self.status, self.message = "error", message
        self.proposal = None
        self.lines, self.notes = [], []


# ------------------------------------------------------------------ layout ---

BOX_HEIGHT = 96
BUTTON_H = 36
ROW_H = 24
LINE_H = 18
CARD_PAD = 16
GAP = 16
SIDE_WIDTH = 280
WIDE_MIN = 840          # content width at which the current settings move right
CURRENT_ROW_H = 22
TITLE_H = 22


def _wrapped_height(text: str, width: float, measure) -> int:
    if not text:
        return 0
    return len(wrap_lines(text, max(width, 40), measure)) * LINE_H


def settings_layout(
    width, top, state: SettingsPageState, current_count: int, measure, measure_change=None,
):
    """Frames (x, y, w, h) for every element; `measure(text) -> points` (13pt).

    `measure_change` measures the change lines (13pt semibold); defaults to
    `measure`.
    """
    wide = width >= WIDE_MIN
    main_w = width - SIDE_WIDTH - GAP if wide else width
    layout = {"wide": wide, "main_w": main_w}
    y = top
    layout["box_label"] = (0, y, main_w, TITLE_H)
    y += TITLE_H + 2
    intro_h = _wrapped_height(INTRO, main_w, measure)
    layout["intro"] = (0, y, main_w, intro_h)
    y += intro_h + 8
    layout["box"] = (0, y, main_w, BOX_HEIGHT)
    y += BOX_HEIGHT + 12
    layout["preview"] = (0, y, 120, BUTTON_H)
    layout["note"] = (132, y + 9, max(main_w - 132, 80), LINE_H)
    y += BUTTON_H + GAP

    inner_w = main_w - 2 * CARD_PAD
    card_y = y
    cy = CARD_PAD
    layout["card_title"] = (CARD_PAD, cy, inner_w, TITLE_H)
    cy += TITLE_H + 6
    rows = []
    if state.status in ("empty", "busy"):
        text = EMPTY_HINT if state.status == "empty" else (state.pending or "")
        h = _wrapped_height(text, inner_w, measure)
        layout["card_text"] = (CARD_PAD, cy, inner_w, h)
        cy += h
    elif state.status == "error":
        h = _wrapped_height(state.message, inner_w, measure)
        layout["card_text"] = (CARD_PAD, cy, inner_w, h)
        cy += h
    else:
        if state.status == "applied" and state.message:
            h = _wrapped_height(state.message, inner_w, measure)
            layout["card_text"] = (CARD_PAD, cy, inner_w, h)
            cy += h + 6
        for line in state.lines:
            # Wrapped, never truncated: a change (e.g. a 100-character
            # replacement) must be seen in full before it is applied. One
            # line keeps the familiar ROW_H row.
            h = _wrapped_height(line, inner_w, measure_change or measure)
            h = ROW_H if h <= LINE_H else h + (ROW_H - LINE_H)
            rows.append((CARD_PAD, cy, inner_w, h))
            cy += h
        for note in state.notes:
            h = max(_wrapped_height(note, inner_w, measure), LINE_H)
            rows.append((CARD_PAD, cy, inner_w, h))
            cy += h
    layout["rows"] = rows
    if state.status == "preview":
        cy += 12
        layout["apply"] = (CARD_PAD, cy, 110, BUTTON_H)
        layout["cancel"] = (CARD_PAD + 122, cy, 100, BUTTON_H)
        cy += BUTTON_H
    cy += CARD_PAD
    layout["card"] = (0, card_y, main_w, cy)
    y = card_y + cy + GAP

    undo_text = "; ".join(state.undo_lines or []) or "Nothing to undo yet."
    layout["undo"] = (0, y, 170, BUTTON_H)
    undo_h = max(_wrapped_height(undo_text, main_w - 186, measure), LINE_H)
    layout["undo_text"] = (186, y + max(0, (BUTTON_H - undo_h) / 2), main_w - 186, undo_h)
    layout["undo_detail"] = undo_text
    y += max(BUTTON_H, undo_h) + GAP

    current_h = CARD_PAD * 2 + TITLE_H + 6 + current_count * CURRENT_ROW_H
    if wide:
        cx, cyy, cw = main_w + GAP, top, SIDE_WIDTH
    else:
        cx, cyy, cw = 0, y, width
        y += current_h + GAP
    layout["current"] = (cx, cyy, cw, current_h)
    layout["current_rows"] = [
        (CARD_PAD, CARD_PAD + TITLE_H + 6 + i * CURRENT_ROW_H, cw - 2 * CARD_PAD, CURRENT_ROW_H)
        for i in range(current_count)
    ]
    layout["bottom"] = max(y, cyy + current_h + GAP)
    return layout
