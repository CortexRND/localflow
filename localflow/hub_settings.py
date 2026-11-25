"""Settings page of the Hub: state and geometry, without AppKit.

hub_settings_mac renders this; tests drive it directly. The page asks the
local Settings agent (settings_agent) for a preview off the UI thread; a
result is only accepted for the request that is still current (`token`),
so navigating away, resizing, closing the window or asking again never lets
a late answer overwrite newer state.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from localflow.settings_agent import safe_text, warnings_for
from localflow.text_wrap import wrap_lines
from localflow.ui_tokens import SWITCH_WIDTH

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
MANUAL_TITLE = "Settings"
MANUAL_INTRO = "Changes save as you make them. Undo works for these too."
WARNINGS_TITLE = "Check before applying"
STALE_PREVIEW = "Your settings changed, so that preview is out of date. Preview again."
STALE_PENDING = (
    "Your settings changed while the local model was thinking, so its answer "
    "was dropped. Preview again."
)


# Speaker-label switches: disabled (with a reason) while the server says the
# diarization backend can't run.
DIARIZATION_KEYS = ("speaker_diarization", "diarize_mic")
DIAR_NO_RUNTIME = "Install localflow[diarization]"
DIAR_NO_MODELS = "Download speaker models in Models & keys"
DIAR_DOWNLOADING = "Downloading speaker models..."


def diarization_disabled_reason(status) -> str | None:
    """Why the speaker-label switches can't be used, from a
    `GET /api/diarization/status` answer; None when ready or unknown (an
    unreachable or older server is not treated as "not ready"). Server text
    is never shown verbatim."""
    if not isinstance(status, dict):
        return None
    if "extra_installed" not in status and "models_ready" not in status:
        return None
    if "extra_installed" in status and not status.get("extra_installed"):
        return DIAR_NO_RUNTIME
    if "models_ready" in status and not status.get("models_ready"):
        download = status.get("download")
        if isinstance(download, dict) and download.get("state") == "downloading":
            return DIAR_DOWNLOADING
        reason = status.get("reason")
        shown = safe_text(reason, 200) if isinstance(reason, str) else ""
        return shown.strip() or DIAR_NO_MODELS
    return None


def model_note(model: str | None) -> str:
    return f"Runs on your Mac with {safe_text(model, 120)}" if model else NOTE_UNKNOWN


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
    warnings: list[str] = field(default_factory=list)  # preview cautions
    # Manual controls: an invalid typed value is kept (per key) with its
    # friendly error until it is fixed or another value is committed.
    manual_drafts: dict[str, str] = field(default_factory=dict)
    manual_errors: dict[str, str] = field(default_factory=dict)
    manual_notice: str = ""
    # Rows that can't be used right now, with a muted reason shown under them
    # (from the server's diarization status; see set_diarization_status).
    disabled_reasons: dict[str, str] = field(default_factory=dict)

    @property
    def busy(self) -> bool:
        return self.status == "busy"

    # ------------------------------------------------------------ manual ---

    def manual_saved(self, key: str, message: str) -> None:
        """A manual change was written: clear that control's draft/error and
        retire any preview computed against the old settings, so neither a
        stale Apply nor a late model answer can overwrite what was just set."""
        self.manual_drafts.pop(key, None)
        self.manual_errors.pop(key, None)
        # Built by the controller from Change.line() (already validated and
        # escaped), so LocalFlow's own "→" survives; only bounded here.
        self.manual_notice = message[:MAX_DRAFT_CHARS]
        self.settings_changed()

    def manual_failed(self, key: str, typed: str | None, message: str) -> None:
        if typed is not None:
            self.manual_drafts[key] = typed[:MAX_DRAFT_CHARS]
        self.manual_errors[key] = safe_text(message)
        self.manual_notice = ""

    def manual_draft(self, key: str, typed: str) -> None:
        """Keep a shortcut field's text across a rebuild (resize, navigation)."""
        self.manual_drafts[key] = typed[:MAX_DRAFT_CHARS]

    def set_diarization_status(self, status) -> bool:
        """Fill or clear the speaker-label rows' reason; True when it changed."""
        reason = diarization_disabled_reason(status)
        before = dict(self.disabled_reasons)
        for key in DIARIZATION_KEYS:
            if reason:
                self.disabled_reasons[key] = reason
            else:
                self.disabled_reasons.pop(key, None)
        return before != self.disabled_reasons

    def settings_changed(self) -> None:
        """Settings moved under the page: drop a now-stale preview or request."""
        if self.status == "preview":
            self.token += 1
            self.proposal = None
            self.lines, self.notes, self.warnings = [], [], []
            self.status, self.message = "error", STALE_PREVIEW
        elif self.status == "busy":
            self.token += 1  # the in-flight answer is ignored when it lands
            self.pending = None
            self.proposal = None
            self.lines, self.notes, self.warnings = [], [], []
            self.status, self.message = "error", STALE_PENDING

    def clear_manual(self) -> None:
        """After an agent Apply/Undo the controls show the fresh config."""
        self.manual_drafts.clear()
        self.manual_errors.clear()
        self.manual_notice = ""

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
        self.lines, self.notes, self.warnings, self.message = [], [], [], ""
        return self.token

    def finish_preview(self, token: int, proposal=None, error: str | None = None,
                       model: str | None = None) -> bool:
        """Accept a worker result; False (ignored) when it is stale.

        Everything shown is rebuilt or escaped here (Change.line(),
        safe_text): nothing the model or Ollama sent is displayed verbatim.
        """
        if token != self.token or self.status != "busy":
            return False
        request = self.pending or ""
        self.pending = None
        if model:
            self.model = safe_text(model, 120)
        if error is not None:
            self.status, self.message = "error", safe_text(error)
            return True
        self.proposal = proposal
        shown_model = getattr(proposal, "model", None)
        self.model = safe_text(shown_model, 120) if shown_model else self.model
        self.lines = [change.line() for change in proposal.changes]
        self.warnings = warnings_for(
            proposal.changes, getattr(proposal, "request", "") or request
        )
        self.notes = [safe_text(note) for note in proposal.rejected]
        self.status = "preview"
        return True

    def cancel(self) -> None:
        """Drop the preview (or an in-flight request: its answer is ignored)."""
        self.token += 1
        self.pending = None
        self.proposal = None
        self.lines, self.notes, self.warnings, self.message = [], [], [], ""
        self.status = "empty"

    def applied(self, result) -> None:
        self.proposal = None
        self.lines = [change.line() for change in result.applied]
        self.notes, self.warnings = [], []
        self.message = safe_text(result.message)
        self.status = "applied"
        self.draft = ""
        self.clear_manual()

    def undone(self, result) -> None:
        self.proposal = None
        # Undo lines are built by the agent from a validated record.
        self.lines = [safe_text(line) for line in result.reverted]
        self.notes = [safe_text(note) for note in result.skipped]
        self.warnings = []
        self.message = safe_text(result.message)
        self.status = "applied"
        self.clear_manual()

    def failed(self, message: str) -> None:
        self.status, self.message = "error", safe_text(message)
        self.proposal = None
        self.lines, self.notes, self.warnings = [], [], []


# ------------------------------------------------------------------ layout ---

BOX_HEIGHT = 96
BUTTON_H = 36
ROW_H = 24
LINE_H = 18
CARD_PAD = 16
GAP = 16
CURRENT_ROW_H = 22
TITLE_H = 22
# Manual controls.
GROUP_H = 26            # a group heading
CONTROL_H = 26          # popup / shortcut field
SWITCH_H = 22
MANUAL_ROW_GAP = 8
CONTROL_W = 240         # popup / shortcut field, right-aligned
LABEL_GAP = 16          # least room between a label and its control


def manual_panel_width(width) -> float:
    """Width of the manual panel: always the full content width, under the
    agent (a narrow side column stacked every label over its control)."""
    return width


def manual_control_width(kind, inner) -> float:
    """A row's control width: the switch's own, else CONTROL_W, never more
    than half the row so its label keeps the left side."""
    if kind == "switch":
        return SWITCH_WIDTH
    return min(CONTROL_W, max(inner / 2, 120))


def manual_layout(width, rows, state: SettingsPageState, measure):
    """Frames for the manual panel, relative to its own top-left.

    `rows` are settings_agent.ManualSetting in page order (grouped). Every
    row has its label on the left and its control right-aligned on the same
    line, at every supported width; a long label wraps in its column. A row
    with an error gets a wrapped error line under it, across the full width
    (sized by `measure`), so nothing is clipped at any width. A row in
    `state.disabled_reasons` gets its muted reason the same way, above any
    error.
    Returns {"height", "title", "intro", "groups": [(name, frame)],
    "rows": {key: {"label", "control", "reason"?, "error"?}}, "notice"?}.
    """
    inner = width - 2 * CARD_PAD
    layout = {"groups": [], "rows": {}}
    y = CARD_PAD
    layout["title"] = (CARD_PAD, y, inner, TITLE_H)
    y += TITLE_H + 2
    intro_h = _wrapped_height(MANUAL_INTRO, inner, measure)
    layout["intro"] = (CARD_PAD, y, inner, intro_h)
    y += intro_h + 6
    if state.manual_notice:
        notice_h = _wrapped_height(state.manual_notice, inner, measure)
        layout["notice"] = (CARD_PAD, y, inner, notice_h)
        y += notice_h + 6
    group = None
    for row in rows:
        if row.group != group:
            group = row.group
            y += 6
            layout["groups"].append((group, (CARD_PAD, y, inner, GROUP_H - 6)))
            y += GROUP_H
        control_h = SWITCH_H if row.kind == "switch" else CONTROL_H
        control_w = manual_control_width(row.kind, inner)
        label_w = inner - control_w - LABEL_GAP
        label_h = max(_wrapped_height(row.label, label_w, measure), LINE_H)
        line_h = max(control_h, label_h)
        frames = {
            "label": (CARD_PAD, y + (line_h - label_h) / 2, label_w, label_h),
            "control": (CARD_PAD + inner - control_w, y + (line_h - control_h) / 2,
                        control_w, control_h),
        }
        y += line_h
        reason = state.disabled_reasons.get(row.key)
        if reason:
            reason_h = _wrapped_height(reason, inner, measure)
            frames["reason"] = (CARD_PAD, y + 2, inner, reason_h)
            y += reason_h + 2
        error = state.manual_errors.get(row.key)
        if error:
            error_h = _wrapped_height(error, inner, measure)
            frames["error"] = (CARD_PAD, y + 4, inner, error_h)
            y += error_h + 4
        layout["rows"][row.key] = frames
        y += MANUAL_ROW_GAP
    layout["height"] = y + CARD_PAD
    return layout


def _wrapped_height(text: str, width: float, measure) -> int:
    if not text:
        return 0
    return len(wrap_lines(text, max(width, 40), measure)) * LINE_H


def settings_layout(
    width, top, state: SettingsPageState, current_count: int, measure, measure_change=None,
    *, manual_height=None,
):
    """Frames (x, y, w, h) for every element; `measure(text) -> points` (13pt).

    `measure_change` measures the change lines (13pt semibold); defaults to
    `measure`. `manual_height` is the manual panel's height (manual_layout at
    manual_panel_width(width)); `layout["current"]` is that panel's frame:
    full width, under the agent and its Undo row, at every width. Without it
    the panel is sized for `current_count` plain rows.
    """
    main_w = width
    layout = {"main_w": main_w}
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
        if state.warnings:
            cy += 6
            layout["warnings_title"] = (CARD_PAD, cy, inner_w, LINE_H)
            cy += LINE_H + 2
            warning_rows = []
            for warning in state.warnings:
                h = max(_wrapped_height(warning, inner_w, measure), LINE_H)
                warning_rows.append((CARD_PAD, cy, inner_w, h))
                cy += h
            layout["warning_rows"] = warning_rows
            cy += 4
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

    if manual_height is not None:
        current_h = manual_height
    else:
        current_h = CARD_PAD * 2 + TITLE_H + 6 + current_count * CURRENT_ROW_H
    layout["current"] = (0, y, width, current_h)
    layout["current_rows"] = [
        (CARD_PAD, CARD_PAD + TITLE_H + 6 + i * CURRENT_ROW_H, width - 2 * CARD_PAD,
         CURRENT_ROW_H)
        for i in range(current_count)
    ]
    y += current_h + GAP
    layout["bottom"] = y
    return layout
