//! Settings page of the Hub: state, geometry and the UI-free controller
//! (port of `localflow/hub_settings.py` and the pure parts of
//! `localflow/hub_settings_mac.py`).
//!
//! A native view renders this; tests drive it directly. The page asks the
//! local Settings agent ([`crate::backends::settings_agent`]) for a preview
//! off the UI thread; a result is only accepted for the request that is still
//! current (`token`), so navigating away, resizing, closing the window or
//! asking again never lets a late answer overwrite newer state.
//!
//! Text widths are injected as [`TextMeasure`] closures
//! `(text, font size, weight) -> width`: the layouts measure the page's 13 pt
//! text, the change lines at 13 pt semibold and the manual panel at 12 pt,
//! exactly the fonts the native view draws in.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::backends::settings_agent::{
    editable_shortcut, safe_text, safe_text_default, warnings_for, ApplyResult, Change,
    ManualSetting, Proposal, SettingsAgent, SettingsAgentFactory, UndoResult,
};
use crate::text_wrap::{wrap_text, TextMeasure};
use crate::tokens::{Rect, Weight, SWITCH_WIDTH};

pub const HEADING: &str = "Tell LocalFlow what to change";
pub const PLACEHOLDER: &str = "e.g. “Turn off sounds”, “Dictate with Right Command”, “Add Kubernetes to my dictionary”, “Make work messages casual”";
pub const INTRO: &str = "Describe a change in plain words. Nothing changes until you press Apply.";
pub const EMPTY_HINT: &str = "Your request goes to a model running on this Mac. It can only change the settings listed here and add dictionary words or replacements.";
pub const NOTE_UNKNOWN: &str = "Runs on your Mac with a local Ollama model";
pub const BUSY_PREVIEW: &str = "Asking the local model…";
pub const CURRENT_TITLE: &str = "Current settings";
pub const PREVIEW_TITLE: &str = "Preview";
pub const APPLIED_TITLE: &str = "Saved";
pub const ERROR_TITLE: &str = "Couldn't preview that";
pub const UNDO_TITLE: &str = "Undo last change";
pub const MAX_DRAFT_CHARS: usize = 500;
pub const MANUAL_TITLE: &str = "Settings";
pub const MANUAL_INTRO: &str = "Changes save as you make them. Undo works for these too.";
pub const WARNINGS_TITLE: &str = "Check before applying";
pub const STALE_PREVIEW: &str =
    "Your settings changed, so that preview is out of date. Preview again.";
pub const STALE_PENDING: &str = "Your settings changed while the local model was thinking, so its answer was dropped. Preview again.";
/// Title of the empty-state card (`hub_settings_mac`).
pub const EMPTY_TITLE: &str = "Try asking";
/// First item of a choice popup whose stored value is invalid.
pub const CHOOSE_TITLE: &str = "Choose…";
pub const NOTHING_TO_UNDO: &str = "Nothing to undo yet.";
pub const PREVIEW_FAILED: &str = "Something went wrong asking the local model.";

/// `"Runs on your Mac with <model>"`, or the generic note for no model.
pub fn model_note(model: Option<&str>) -> String {
    match model.filter(|m| !m.is_empty()) {
        Some(model) => format!("Runs on your Mac with {}", safe_text(model, 120)),
        None => NOTE_UNKNOWN.to_string(),
    }
}

/// What the agent card shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Empty,
    Busy,
    Preview,
    Applied,
    Error,
}

impl Status {
    /// The Python status string (`"empty"`, `"busy"`, `"preview"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Empty => "empty",
            Status::Busy => "busy",
            Status::Preview => "preview",
            Status::Applied => "applied",
            Status::Error => "error",
        }
    }
}

fn char_prefix(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Everything the page shows; survives navigation, resize and appearance.
#[derive(Clone, Debug, Default)]
pub struct SettingsPageState {
    pub draft: String,
    pub status: Status,
    /// The proposal while previewing.
    pub proposal: Option<Proposal>,
    /// Preview/applied lines.
    pub lines: Vec<String>,
    /// Rejected or skipped.
    pub notes: Vec<String>,
    pub message: String,
    pub model: Option<String>,
    pub token: i64,
    /// The request in flight, if any.
    pub pending: Option<String>,
    pub undo_lines: Option<Vec<String>>,
    /// Preview cautions.
    pub warnings: Vec<String>,
    /// Manual controls: an invalid typed value is kept (per key) with its
    /// friendly error until it is fixed or another value is committed.
    pub manual_drafts: BTreeMap<String, String>,
    pub manual_errors: BTreeMap<String, String>,
    pub manual_notice: String,
}

impl SettingsPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn busy(&self) -> bool {
        self.status == Status::Busy
    }

    fn clear_shown(&mut self) {
        self.lines.clear();
        self.notes.clear();
        self.warnings.clear();
    }

    // ------------------------------------------------------------ manual ---

    /// A manual change was written: clear that control's draft/error and
    /// retire any preview computed against the old settings, so neither a
    /// stale Apply nor a late model answer can overwrite what was just set.
    pub fn manual_saved(&mut self, key: &str, message: &str) {
        self.manual_drafts.remove(key);
        self.manual_errors.remove(key);
        // Built by the controller from Change.line() (already validated and
        // escaped), so LocalFlow's own "→" survives; only bounded here.
        self.manual_notice = char_prefix(message, MAX_DRAFT_CHARS);
        self.settings_changed();
    }

    pub fn manual_failed(&mut self, key: &str, typed: Option<&str>, message: &str) {
        if let Some(typed) = typed {
            self.manual_drafts
                .insert(key.to_string(), char_prefix(typed, MAX_DRAFT_CHARS));
        }
        self.manual_errors
            .insert(key.to_string(), safe_text_default(message));
        self.manual_notice.clear();
    }

    /// Keep a shortcut field's text across a rebuild (resize, navigation).
    pub fn manual_draft(&mut self, key: &str, typed: &str) {
        self.manual_drafts
            .insert(key.to_string(), char_prefix(typed, MAX_DRAFT_CHARS));
    }

    /// Settings moved under the page: drop a now-stale preview or request.
    pub fn settings_changed(&mut self) {
        match self.status {
            Status::Preview => {
                self.token += 1;
                self.proposal = None;
                self.clear_shown();
                self.status = Status::Error;
                self.message = STALE_PREVIEW.to_string();
            }
            Status::Busy => {
                self.token += 1; // the in-flight answer is ignored when it lands
                self.pending = None;
                self.proposal = None;
                self.clear_shown();
                self.status = Status::Error;
                self.message = STALE_PENDING.to_string();
            }
            _ => {}
        }
    }

    /// After an agent Apply/Undo the controls show the fresh config.
    pub fn clear_manual(&mut self) {
        self.manual_drafts.clear();
        self.manual_errors.clear();
        self.manual_notice.clear();
    }

    /// Start a preview; returns its token, or `None` when there's nothing to
    /// ask (blank request, or one is already in flight).
    pub fn begin_preview(&mut self, request: &str) -> Option<i64> {
        let text = lf_providers::pyfmt::py_strip(request).to_string();
        self.draft = request.to_string();
        if text.is_empty() || self.busy() {
            return None;
        }
        self.token += 1;
        self.pending = Some(text);
        self.status = Status::Busy;
        self.proposal = None;
        self.clear_shown();
        self.message.clear();
        Some(self.token)
    }

    /// Accept a worker result; `false` (ignored) when it is stale.
    ///
    /// Everything shown is rebuilt or escaped here ([`Change::line`],
    /// `safe_text`): nothing the model or Ollama sent is displayed verbatim.
    /// A result with neither a proposal nor an error (Python would crash on
    /// it) is shown as an error.
    pub fn finish_preview(
        &mut self,
        token: i64,
        proposal: Option<Proposal>,
        error: Option<&str>,
        model: Option<&str>,
    ) -> bool {
        if token != self.token || self.status != Status::Busy {
            return false;
        }
        let request = self.pending.take().unwrap_or_default();
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            self.model = Some(safe_text(model, 120));
        }
        let (proposal, error) = match (proposal, error) {
            (_, Some(error)) => (None, Some(error.to_string())),
            (None, None) => (None, Some(PREVIEW_FAILED.to_string())),
            (Some(proposal), None) => (Some(proposal), None),
        };
        if let Some(error) = error {
            self.status = Status::Error;
            self.message = safe_text_default(&error);
            return true;
        }
        let Some(proposal) = proposal else {
            return true;
        };
        if !proposal.model.is_empty() {
            self.model = Some(safe_text(&proposal.model, 120));
        }
        self.lines = proposal.changes.iter().map(Change::line).collect();
        let basis = if proposal.request.is_empty() {
            request.as_str()
        } else {
            proposal.request.as_str()
        };
        self.warnings = warnings_for(&proposal.changes, basis);
        self.notes = proposal
            .rejected
            .iter()
            .map(|n| safe_text_default(n))
            .collect();
        self.proposal = Some(proposal);
        self.status = Status::Preview;
        true
    }

    /// Drop the preview (or an in-flight request: its answer is ignored).
    pub fn cancel(&mut self) {
        self.token += 1;
        self.pending = None;
        self.proposal = None;
        self.clear_shown();
        self.message.clear();
        self.status = Status::Empty;
    }

    pub fn applied(&mut self, result: &ApplyResult) {
        self.proposal = None;
        self.lines = result.applied.iter().map(Change::line).collect();
        self.notes.clear();
        self.warnings.clear();
        self.message = safe_text_default(&result.message);
        self.status = Status::Applied;
        self.draft.clear();
        self.clear_manual();
    }

    pub fn undone(&mut self, result: &UndoResult) {
        self.proposal = None;
        // Undo lines are built by the agent from a validated record.
        self.lines = result
            .reverted
            .iter()
            .map(|l| safe_text_default(l))
            .collect();
        self.notes = result
            .skipped
            .iter()
            .map(|n| safe_text_default(n))
            .collect();
        self.warnings.clear();
        self.message = safe_text_default(&result.message);
        self.status = Status::Applied;
        self.clear_manual();
    }

    pub fn failed(&mut self, message: &str) {
        self.status = Status::Error;
        self.message = safe_text_default(message);
        self.proposal = None;
        self.clear_shown();
    }
}

// ------------------------------------------------------------------ layout ---

pub const BOX_HEIGHT: f64 = 96.0;
pub const BUTTON_H: f64 = 36.0;
pub const ROW_H: f64 = 24.0;
pub const LINE_H: f64 = 18.0;
pub const CARD_PAD: f64 = 16.0;
pub const GAP: f64 = 16.0;
pub const CURRENT_ROW_H: f64 = 22.0;
pub const TITLE_H: f64 = 22.0;
// Manual controls.
/// A group heading.
pub const GROUP_H: f64 = 26.0;
/// Popup / shortcut field.
pub const CONTROL_H: f64 = 26.0;
pub const SWITCH_H: f64 = 22.0;
pub const MANUAL_ROW_GAP: f64 = 8.0;
/// Popup / shortcut field, right-aligned.
pub const CONTROL_W: f64 = 240.0;
/// Least room between a label and its control.
pub const LABEL_GAP: f64 = 16.0;

/// Font size of the agent text (and the change lines).
pub const PAGE_FONT: f64 = 13.0;
/// Font size of the manual panel's labels, intro and errors.
pub const MANUAL_FONT: f64 = 12.0;

/// Width of the manual panel: always the full content width, under the agent
/// (a narrow side column stacked every label over its control).
pub fn manual_panel_width(width: f64) -> f64 {
    width
}

/// A row's control width: the switch's own, else `CONTROL_W`, never more
/// than half the row so its label keeps the left side.
pub fn manual_control_width(kind: &str, inner: f64) -> f64 {
    if kind == "switch" {
        return f64::from(SWITCH_WIDTH);
    }
    CONTROL_W.min((inner / 2.0).max(120.0))
}

fn wrapped_height(
    text: &str,
    width: f64,
    measure: TextMeasure<'_>,
    size: f64,
    weight: Weight,
) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    wrap_text(text, width.max(40.0), measure, size, weight).len() as f64 * LINE_H
}

/// Frames of one manual row.
#[derive(Clone, Debug, PartialEq)]
pub struct ManualRowFrames {
    pub label: Rect,
    pub control: Rect,
    pub error: Option<Rect>,
}

/// Frames of the manual panel, relative to its own top-left.
#[derive(Clone, Debug, PartialEq)]
pub struct ManualLayout {
    pub height: f64,
    pub title: Rect,
    pub intro: Rect,
    pub notice: Option<Rect>,
    pub groups: Vec<(String, Rect)>,
    /// `(key, frames)` in page order.
    pub rows: Vec<(String, ManualRowFrames)>,
}

impl ManualLayout {
    pub fn row(&self, key: &str) -> Option<&ManualRowFrames> {
        self.rows.iter().find(|(k, _)| k == key).map(|(_, f)| f)
    }
}

/// Frames for the manual panel, relative to its own top-left.
///
/// `rows` are [`ManualSetting`]s in page order (grouped). Every row has its
/// label on the left and its control right-aligned on the same line, at every
/// supported width; a long label wraps in its column. A row with an error
/// gets a wrapped error line under it, across the full width (sized by
/// `measure`, called with the 12 pt regular font), so nothing is clipped at
/// any width.
pub fn manual_layout(
    width: f64,
    rows: &[ManualSetting],
    state: &SettingsPageState,
    measure: TextMeasure<'_>,
) -> ManualLayout {
    let wrap = |text: &str, w: f64| wrapped_height(text, w, measure, MANUAL_FONT, Weight::Regular);
    let inner = width - 2.0 * CARD_PAD;
    let mut y = CARD_PAD;
    let title = (CARD_PAD, y, inner, TITLE_H);
    y += TITLE_H + 2.0;
    let intro_h = wrap(MANUAL_INTRO, inner);
    let intro = (CARD_PAD, y, inner, intro_h);
    y += intro_h + 6.0;
    let mut notice = None;
    if !state.manual_notice.is_empty() {
        let notice_h = wrap(&state.manual_notice, inner);
        notice = Some((CARD_PAD, y, inner, notice_h));
        y += notice_h + 6.0;
    }
    let mut groups = Vec::new();
    let mut out_rows = Vec::new();
    let mut group: Option<&str> = None;
    for row in rows {
        if group != Some(row.group.as_str()) {
            group = Some(row.group.as_str());
            y += 6.0;
            groups.push((row.group.clone(), (CARD_PAD, y, inner, GROUP_H - 6.0)));
            y += GROUP_H;
        }
        let control_h = if row.kind == "switch" {
            SWITCH_H
        } else {
            CONTROL_H
        };
        let control_w = manual_control_width(&row.kind, inner);
        let label_w = inner - control_w - LABEL_GAP;
        let label_h = wrap(&row.label, label_w).max(LINE_H);
        let line_h = control_h.max(label_h);
        let label = (CARD_PAD, y + (line_h - label_h) / 2.0, label_w, label_h);
        let control = (
            CARD_PAD + inner - control_w,
            y + (line_h - control_h) / 2.0,
            control_w,
            control_h,
        );
        y += line_h;
        let mut error = None;
        if let Some(text) = state.manual_errors.get(&row.key).filter(|e| !e.is_empty()) {
            let error_h = wrap(text, inner);
            error = Some((CARD_PAD, y + 4.0, inner, error_h));
            y += error_h + 4.0;
        }
        out_rows.push((
            row.key.clone(),
            ManualRowFrames {
                label,
                control,
                error,
            },
        ));
        y += MANUAL_ROW_GAP;
    }
    ManualLayout {
        height: y + CARD_PAD,
        title,
        intro,
        notice,
        groups,
        rows: out_rows,
    }
}

// --------------------------------------------------- speaker-label switches ---
//
// Two `[meetings]` switches shown at the end of the Meetings group. They are
// not settings-agent keys (the agent's allowlist and its model tool schema
// stay as they are), so they are written straight to the config like the
// Style page does, and they are not in the agent's Undo history: the caption
// says so, keeping the panel's "Undo works for these too" honest.

/// `(config field, label)` of the switches, in page order.
pub const SPEAKER_SWITCHES: [(&str, &str); 2] = [
    ("speaker_diarization", "Speaker labels"),
    ("diarize_mic", "Label speakers on my microphone"),
];
pub const SPEAKER_CAPTION: &str =
    "Speaker labels start with your next meeting. Changes here aren't part of Undo.";
pub const SPEAKERS_CHECKING: &str = "Checking the speaker models…";
pub const SPEAKERS_NO_RUNTIME: &str =
    "This LocalFlow server can't label speakers: its speaker-label runtime isn't installed.";
/// The servers' own reason when the models are missing (`NOT_DOWNLOADED`).
pub const SPEAKERS_NOT_DOWNLOADED: &str =
    "Speaker models are not downloaded. Run `lf speakers download`.";
pub const SPEAKERS_MIC_NEEDS_LABELS: &str = "Turn on Speaker labels first.";

/// Whether speaker labels can be turned on, from `GET /api/diarization/status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeakerReadiness {
    /// The status request has not answered yet.
    Checking,
    Ready,
    /// Not ready, with the reason shown under the switches.
    NotReady(String),
}

/// Readiness from the status reply (`None`: still checking). Ready needs the
/// runtime and every verified model; the server's own `reason` explains a
/// missing model, and a failed request says why it could not be checked.
pub fn speaker_readiness(status: Option<&Result<Value, String>>) -> SpeakerReadiness {
    match status {
        None => SpeakerReadiness::Checking,
        Some(Err(error)) => SpeakerReadiness::NotReady(safe_text_default(&format!(
            "Couldn't check the speaker models: {error}"
        ))),
        Some(Ok(status)) => {
            let installed = status.get("extra_installed") == Some(&Value::Bool(true));
            let models = status.get("models_ready") == Some(&Value::Bool(true));
            if installed && models {
                SpeakerReadiness::Ready
            } else if !installed {
                SpeakerReadiness::NotReady(SPEAKERS_NO_RUNTIME.into())
            } else {
                let reason = status
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(safe_text_default)
                    .unwrap_or_default();
                SpeakerReadiness::NotReady(if reason.trim().is_empty() {
                    SPEAKERS_NOT_DOWNLOADED.into()
                } else {
                    reason
                })
            }
        }
    }
}

/// One speaker switch as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerSwitch {
    pub key: &'static str,
    pub label: &'static str,
    pub on: bool,
    pub enabled: bool,
}

/// The switches for the saved values. A switch that is on can always be
/// turned off; turning one on needs ready models, and the microphone switch
/// also needs Speaker labels on. `busy` (a settings write or preview running)
/// disables both.
pub fn speaker_switches(
    speaker_diarization: bool,
    diarize_mic: bool,
    readiness: &SpeakerReadiness,
    busy: bool,
) -> [SpeakerSwitch; 2] {
    let ready = *readiness == SpeakerReadiness::Ready;
    let [(labels_key, labels), (mic_key, mic)] = SPEAKER_SWITCHES;
    [
        SpeakerSwitch {
            key: labels_key,
            label: labels,
            on: speaker_diarization,
            enabled: !busy && (speaker_diarization || ready),
        },
        SpeakerSwitch {
            key: mic_key,
            label: mic,
            on: diarize_mic,
            enabled: !busy && (diarize_mic || (ready && speaker_diarization)),
        },
    ]
}

/// The muted line under the switches, if any: why they can't be turned on.
pub fn speaker_note(readiness: &SpeakerReadiness, speaker_diarization: bool) -> Option<String> {
    match readiness {
        SpeakerReadiness::Checking => Some(SPEAKERS_CHECKING.into()),
        SpeakerReadiness::NotReady(reason) => Some(reason.clone()),
        SpeakerReadiness::Ready if !speaker_diarization => Some(SPEAKERS_MIC_NEEDS_LABELS.into()),
        SpeakerReadiness::Ready => None,
    }
}

/// `{"meetings": {"<file key>": on}}`: both fields keep their file key.
pub fn speaker_update(key: &str, on: bool) -> Value {
    serde_json::json!({ "meetings": { key: on } })
}

/// Frames of the speaker section, relative to the manual panel.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerSectionLayout {
    pub caption: Rect,
    /// `(key, label, control)` in [`SPEAKER_SWITCHES`] order.
    pub rows: Vec<(&'static str, Rect, Rect)>,
    pub note: Option<Rect>,
    pub error: Option<Rect>,
    /// The manual panel's new height with this section at its end.
    pub height: f64,
}

/// Lays the section out after the manual rows (`manual.height` is the panel
/// with its rows): caption, the two switch rows like any manual switch row,
/// then the note and a write error, each wrapped in full.
pub fn speaker_section_layout(
    manual: &ManualLayout,
    width: f64,
    note: Option<&str>,
    error: &str,
    measure: TextMeasure<'_>,
) -> SpeakerSectionLayout {
    let wrap = |text: &str, w: f64| wrapped_height(text, w, measure, MANUAL_FONT, Weight::Regular);
    let inner = width - 2.0 * CARD_PAD;
    let mut y = manual.height - CARD_PAD;
    let caption_h = wrap(SPEAKER_CAPTION, inner);
    let caption = (CARD_PAD, y, inner, caption_h);
    y += caption_h + 6.0;
    let control_w = manual_control_width("switch", inner);
    let label_w = inner - control_w - LABEL_GAP;
    let mut rows = Vec::new();
    for (key, text) in SPEAKER_SWITCHES {
        let label_h = wrap(text, label_w).max(LINE_H);
        let line_h = SWITCH_H.max(label_h);
        rows.push((
            key,
            (CARD_PAD, y + (line_h - label_h) / 2.0, label_w, label_h),
            (
                CARD_PAD + inner - control_w,
                y + (line_h - SWITCH_H) / 2.0,
                control_w,
                SWITCH_H,
            ),
        ));
        y += line_h + MANUAL_ROW_GAP;
    }
    let mut note_frame = None;
    if let Some(note) = note.filter(|note| !note.is_empty()) {
        let h = wrap(note, inner);
        note_frame = Some((CARD_PAD, y, inner, h));
        y += h + 4.0;
    }
    let mut error_frame = None;
    if !error.is_empty() {
        let h = wrap(error, inner);
        error_frame = Some((CARD_PAD, y, inner, h));
        y += h + 4.0;
    }
    SpeakerSectionLayout {
        caption,
        rows,
        note: note_frame,
        error: error_frame,
        height: y + CARD_PAD,
    }
}

/// Frames (x, y, w, h) for every element of the Settings page.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsLayout {
    pub main_w: f64,
    pub box_label: Rect,
    pub intro: Rect,
    /// The request box (`layout["box"]`).
    pub request_box: Rect,
    pub preview: Rect,
    pub note: Rect,
    pub card_title: Rect,
    pub card_text: Option<Rect>,
    /// Change rows followed by note rows.
    pub rows: Vec<Rect>,
    pub warnings_title: Option<Rect>,
    pub warning_rows: Vec<Rect>,
    pub apply: Option<Rect>,
    pub cancel: Option<Rect>,
    /// `(x, y, w, card height)`: the fourth item is the card's height.
    pub card: Rect,
    pub undo: Rect,
    pub undo_text: Rect,
    pub undo_detail: String,
    /// The manual panel's frame: full width, under the agent and its Undo row.
    pub current: Rect,
    pub current_rows: Vec<Rect>,
    pub bottom: f64,
}

/// Frames for every element; `measure(text, size, weight) -> points`.
///
/// The page text is measured at 13 pt regular and the change lines at 13 pt
/// semibold. `manual_height` is the manual panel's height ([`manual_layout`]
/// at [`manual_panel_width`]); `layout.current` is that panel's frame: full
/// width, under the agent and its Undo row, at every width. Without it the
/// panel is sized for `current_count` plain rows.
pub fn settings_layout(
    width: f64,
    top: f64,
    state: &SettingsPageState,
    current_count: usize,
    measure: TextMeasure<'_>,
    manual_height: Option<f64>,
) -> SettingsLayout {
    let wrap = |text: &str, w: f64| wrapped_height(text, w, measure, PAGE_FONT, Weight::Regular);
    let wrap_change =
        |text: &str, w: f64| wrapped_height(text, w, measure, PAGE_FONT, Weight::Semibold);
    let main_w = width;
    let mut y = top;
    let box_label = (0.0, y, main_w, TITLE_H);
    y += TITLE_H + 2.0;
    let intro_h = wrap(INTRO, main_w);
    let intro = (0.0, y, main_w, intro_h);
    y += intro_h + 8.0;
    let request_box = (0.0, y, main_w, BOX_HEIGHT);
    y += BOX_HEIGHT + 12.0;
    let preview = (0.0, y, 120.0, BUTTON_H);
    let note = (132.0, y + 9.0, (main_w - 132.0).max(80.0), LINE_H);
    y += BUTTON_H + GAP;

    let inner_w = main_w - 2.0 * CARD_PAD;
    let card_y = y;
    let mut cy = CARD_PAD;
    let card_title = (CARD_PAD, cy, inner_w, TITLE_H);
    cy += TITLE_H + 6.0;
    let mut rows: Vec<Rect> = Vec::new();
    let mut card_text = None;
    let mut warnings_title = None;
    let mut warning_rows = Vec::new();
    match state.status {
        Status::Empty | Status::Busy => {
            let text = if state.status == Status::Empty {
                EMPTY_HINT
            } else {
                state.pending.as_deref().unwrap_or("")
            };
            let h = wrap(text, inner_w);
            card_text = Some((CARD_PAD, cy, inner_w, h));
            cy += h;
        }
        Status::Error => {
            let h = wrap(&state.message, inner_w);
            card_text = Some((CARD_PAD, cy, inner_w, h));
            cy += h;
        }
        Status::Preview | Status::Applied => {
            if state.status == Status::Applied && !state.message.is_empty() {
                let h = wrap(&state.message, inner_w);
                card_text = Some((CARD_PAD, cy, inner_w, h));
                cy += h + 6.0;
            }
            for line in &state.lines {
                // Wrapped, never truncated: a change (e.g. a 100-character
                // replacement) must be seen in full before it is applied.
                // One line keeps the familiar ROW_H row.
                let mut h = wrap_change(line, inner_w);
                h = if h <= LINE_H {
                    ROW_H
                } else {
                    h + (ROW_H - LINE_H)
                };
                rows.push((CARD_PAD, cy, inner_w, h));
                cy += h;
            }
            if !state.warnings.is_empty() {
                cy += 6.0;
                warnings_title = Some((CARD_PAD, cy, inner_w, LINE_H));
                cy += LINE_H + 2.0;
                for warning in &state.warnings {
                    let h = wrap(warning, inner_w).max(LINE_H);
                    warning_rows.push((CARD_PAD, cy, inner_w, h));
                    cy += h;
                }
                cy += 4.0;
            }
            for note_text in &state.notes {
                let h = wrap(note_text, inner_w).max(LINE_H);
                rows.push((CARD_PAD, cy, inner_w, h));
                cy += h;
            }
        }
    }
    let (mut apply, mut cancel) = (None, None);
    if state.status == Status::Preview {
        cy += 12.0;
        apply = Some((CARD_PAD, cy, 110.0, BUTTON_H));
        cancel = Some((CARD_PAD + 122.0, cy, 100.0, BUTTON_H));
        cy += BUTTON_H;
    }
    cy += CARD_PAD;
    let card = (0.0, card_y, main_w, cy);
    y = card_y + cy + GAP;

    let joined = state.undo_lines.as_deref().unwrap_or(&[]).join("; ");
    let undo_text_value = if joined.is_empty() {
        NOTHING_TO_UNDO.to_string()
    } else {
        joined
    };
    let undo = (0.0, y, 170.0, BUTTON_H);
    let undo_h = wrap(&undo_text_value, main_w - 186.0).max(LINE_H);
    let undo_text = (
        186.0,
        y + ((BUTTON_H - undo_h) / 2.0).max(0.0),
        main_w - 186.0,
        undo_h,
    );
    y += BUTTON_H.max(undo_h) + GAP;

    let current_h = manual_height
        .unwrap_or(CARD_PAD * 2.0 + TITLE_H + 6.0 + current_count as f64 * CURRENT_ROW_H);
    let current = (0.0, y, width, current_h);
    let current_rows = (0..current_count)
        .map(|i| {
            (
                CARD_PAD,
                CARD_PAD + TITLE_H + 6.0 + i as f64 * CURRENT_ROW_H,
                width - 2.0 * CARD_PAD,
                CURRENT_ROW_H,
            )
        })
        .collect();
    y += current_h + GAP;
    SettingsLayout {
        main_w,
        box_label,
        intro,
        request_box,
        preview,
        note,
        card_title,
        card_text,
        rows,
        warnings_title,
        warning_rows,
        apply,
        cancel,
        card,
        undo,
        undo_text,
        undo_detail: undo_text_value,
        current,
        current_rows,
        bottom: y,
    }
}

// ----------------------------------------------------- card presentation ---

/// `(title, colour token)` of the agent card for a status.
pub fn card_title(status: Status) -> (&'static str, &'static str) {
    match status {
        Status::Empty => (EMPTY_TITLE, "ink"),
        Status::Busy => (BUSY_PREVIEW, "cobalt"),
        Status::Preview => (PREVIEW_TITLE, "ink"),
        Status::Applied => (APPLIED_TITLE, "success"),
        Status::Error => (ERROR_TITLE, "red"),
    }
}

/// The card's paragraph (hint, quoted request or message), when it has one.
pub fn card_text(state: &SettingsPageState) -> Option<String> {
    match state.status {
        Status::Empty => Some(EMPTY_HINT.to_string()),
        Status::Busy => Some(format!(
            "“{}”",
            safe_text_default(state.pending.as_deref().unwrap_or(""))
        )),
        Status::Error => Some(state.message.clone()),
        Status::Applied if !state.message.is_empty() => Some(state.message.clone()),
        _ => None,
    }
}

/// A warning as drawn (`⚠ …`).
pub fn warning_text(warning: &str) -> String {
    format!("⚠ {warning}")
}

/// A skipped/rejected note as drawn (`Skipped: …`).
pub fn skipped_text(note: &str) -> String {
    format!("Skipped: {note}")
}

/// Popup items of a choice row: the titles (with `Choose…` first when the
/// stored value is invalid) and the index to select.
pub fn choice_items(row: &ManualSetting) -> (Vec<String>, usize) {
    let mut titles: Vec<String> = row.choices.iter().map(|(_, t)| t.clone()).collect();
    let position = row
        .value
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|current| row.choices.iter().position(|(value, _)| value == current));
    match position {
        Some(index) => (titles, index),
        None => {
            titles.insert(0, CHOOSE_TITLE.to_string());
            (titles, 0)
        }
    }
}

/// The value a popup index stands for (`None` for the placeholder or an
/// out-of-range index).
pub fn choice_value_at(row: &ManualSetting, index: i64) -> Option<String> {
    let current = row.value.as_ref().and_then(Value::as_str);
    let known = current.is_some_and(|c| row.choices.iter().any(|(value, _)| value == c));
    let index = if known { index } else { index - 1 }; // skip the "Choose…" placeholder
    usize::try_from(index)
        .ok()
        .and_then(|i| row.choices.get(i))
        .map(|(value, _)| value.clone())
}

/// What a shortcut field shows: the kept draft, else the editable form of the
/// saved value.
pub fn shortcut_field_text(row: &ManualSetting, state: &SettingsPageState) -> String {
    match state.manual_drafts.get(&row.key) {
        Some(draft) => draft.clone(),
        None => editable_shortcut(row.value.as_ref().unwrap_or(&Value::Null)),
    }
}

pub fn shortcut_placeholder(key: &str) -> &'static str {
    if key == "hotkey" {
        "e.g. right_option"
    } else {
        "Off"
    }
}

// -------------------------------------------------------------- controller ---

/// One finished preview request (`(token, proposal, error, model)`).
#[derive(Clone, Debug)]
pub struct PreviewResult {
    pub token: i64,
    pub proposal: Option<Proposal>,
    pub error: Option<String>,
    pub model: Option<String>,
}

type ResultQueue = Arc<Mutex<VecDeque<PreviewResult>>>;

/// A preview to run off the UI thread: call [`PreviewJob::run`] on a worker
/// and then tell the UI thread to call
/// [`SettingsPageController::preview_done`].
pub struct PreviewJob {
    pub token: i64,
    pub request: String,
    agent: Box<dyn SettingsAgent>,
    results: ResultQueue,
}

impl PreviewJob {
    /// Ask the model and queue the answer for the UI thread. Never panics on
    /// agent errors; they become a friendly message.
    pub fn run(self) {
        let result = match self.agent.propose(&self.request) {
            Ok(proposal) => PreviewResult {
                token: self.token,
                proposal: Some(proposal),
                error: None,
                model: self.agent.model(),
            },
            Err(error) => PreviewResult {
                token: self.token,
                proposal: None,
                error: Some(error.0),
                model: self.agent.model(),
            },
        };
        self.results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(result);
    }
}

/// The UI-free half of `SettingsPageController`: owns the page state, the
/// manual rows and the agent factory, and turns user events into state
/// changes. The native layer builds views from [`SettingsPageState`] and the
/// layouts, reads typed text out of its controls and forwards events here.
pub struct SettingsPageController {
    pub state: SettingsPageState,
    pub manual_rows: Vec<ManualSetting>,
    /// The Hub window closed: results may still arrive but nothing repaints.
    pub closed: bool,
    agent_factory: SettingsAgentFactory,
    results: ResultQueue,
}

impl SettingsPageController {
    /// `agent_factory` is the Hub's `settings_agent_factory` (a fresh agent
    /// handle per use; the default talks to the user's config and a loopback
    /// Ollama in the real implementation).
    pub fn new(agent_factory: SettingsAgentFactory) -> Self {
        SettingsPageController {
            state: SettingsPageState::new(),
            manual_rows: Vec::new(),
            closed: false,
            agent_factory,
            results: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// The Hub is tearing views down: keep the typed request, and the typed
    /// text of every live shortcut field `(key, typed)`.
    pub fn capture(&mut self, draft: Option<&str>, shortcut_fields: &[(String, String)]) {
        self.take_draft(draft);
        self.take_manual_drafts(shortcut_fields);
    }

    /// The Hub window closed.
    pub fn shutdown(&mut self) {
        self.closed = true;
    }

    /// Repaint only while the Hub is open and showing Settings.
    pub fn should_repaint(&self, current_page: &str) -> bool {
        !self.closed && current_page == "Settings"
    }

    /// Manual settings and the undo summary (local reads only).
    pub fn refresh_local(&mut self) {
        let agent = (self.agent_factory)();
        let rows = agent
            .load()
            .and_then(|config| agent.manual_settings(Some(&config)));
        match rows {
            Ok(rows) => self.manual_rows = rows,
            Err(error) => {
                log::warn!("could not read settings for the Settings page: {error}");
                self.manual_rows = Vec::new();
            }
        }
        self.state.undo_lines = agent.pending_undo();
    }

    /// Move the typed text into the state (the caller forgets its view).
    pub fn take_draft(&mut self, text: Option<&str>) {
        if let Some(text) = text {
            self.state.draft = text.to_string();
        }
    }

    /// Keep typed-but-uncommitted shortcut text `(key, typed)`; forget the rest.
    pub fn take_manual_drafts(&mut self, fields: &[(String, String)]) {
        for (key, typed) in fields {
            let Some(row) = self.manual_rows.iter().find(|r| &r.key == key) else {
                continue;
            };
            if row.kind != "shortcut" {
                continue;
            }
            let saved = editable_shortcut(row.value.as_ref().unwrap_or(&Value::Null));
            if *typed != saved {
                self.state.manual_draft(key, typed);
            } else if !self.state.manual_errors.contains_key(key) {
                self.state.manual_drafts.remove(key);
            }
        }
    }

    /// The Preview button: `None` when one is already running or the request
    /// is blank, else the job to run on a worker thread.
    pub fn start_preview(&mut self, request: &str) -> Option<PreviewJob> {
        if self.state.busy() {
            return None;
        }
        let token = self.state.begin_preview(request)?;
        Some(PreviewJob {
            token,
            request: request.to_string(),
            agent: (self.agent_factory)(),
            results: Arc::clone(&self.results),
        })
    }

    /// Main-thread delivery of finished previews; `true` when something
    /// changed (stale tokens are dropped by the state).
    pub fn preview_done(&mut self) -> bool {
        let mut changed = false;
        loop {
            let next = self
                .results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front();
            let Some(result) = next else { break };
            changed |= self.state.finish_preview(
                result.token,
                result.proposal,
                result.error.as_deref(),
                result.model.as_deref(),
            );
        }
        changed
    }

    /// Write one manual change through the agent; errors stay inline.
    /// The caller captures drafts first ([`Self::capture`]) and reloads the
    /// shared config afterwards.
    pub fn manual_commit(&mut self, key: &str, value: &Value, typed: Option<&str>) {
        match (self.agent_factory)().set_setting(key, value) {
            Err(error) => self.state.manual_failed(key, typed, &error.0),
            Ok(result) if !result.applied.is_empty() => {
                let lines: Vec<String> = result.applied.iter().map(Change::line).collect();
                self.state
                    .manual_saved(key, &format!("Saved: {}", lines.join("; ")));
            }
            Ok(_) => {
                self.state.manual_drafts.remove(key);
                self.state.manual_errors.remove(key);
            }
        }
    }

    /// A switch was toggled.
    pub fn manual_toggle(&mut self, key: &str, on: bool) {
        self.manual_commit(key, &Value::Bool(on), None);
    }

    /// A popup item was chosen (`index` as the popup reports it).
    pub fn manual_choose(&mut self, key: &str, index: i64) {
        let Some(row) = self.manual_rows.iter().find(|r| r.key == key) else {
            return;
        };
        let Some(value) = choice_value_at(row, index) else {
            return;
        };
        self.manual_commit(key, &Value::String(value), None);
    }

    /// A shortcut field ended editing or got Return.
    pub fn manual_shortcut(&mut self, key: &str, typed: &str) {
        let Some(row) = self.manual_rows.iter().find(|r| r.key == key) else {
            return;
        };
        let saved = editable_shortcut(row.value.as_ref().unwrap_or(&Value::Null));
        if typed == saved && !self.state.manual_errors.contains_key(key) {
            return; // ended editing without a change
        }
        let trimmed = lf_providers::pyfmt::py_strip(typed);
        let value = if trimmed.is_empty() { "off" } else { trimmed };
        self.manual_commit(key, &Value::String(value.to_string()), Some(typed));
    }

    /// The Apply button.
    pub fn apply(&mut self) {
        if self.state.status != Status::Preview {
            return;
        }
        let Some(proposal) = self.state.proposal.clone() else {
            return;
        };
        match (self.agent_factory)().apply(&proposal.changes) {
            Err(error) => self.state.failed(&error.0),
            Ok(result) => self.state.applied(&result),
        }
    }

    /// The Cancel button.
    pub fn cancel(&mut self) {
        self.state.cancel();
    }

    /// The Undo button.
    pub fn undo(&mut self) {
        if self.state.busy() {
            return;
        }
        match (self.agent_factory)().undo() {
            Err(error) => self.state.failed(&error.0),
            Ok(result) => self.state.undone(&result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::settings_agent::{FakeCall, FakeSettingsAgent, SettingsAgentError};
    use crate::tokens::{HUB_CONTENT_GUTTER, HUB_CONTENT_X, HUB_HEADER_OFFSET, HUB_MIN_WIDTH};
    use serde_json::json;

    const MIN_CONTENT: f64 = (HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER) as f64;
    const WIDE_CONTENT: f64 = (1280 - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER) as f64;
    const TOP: f64 = HUB_HEADER_OFFSET as f64;

    fn measure(text: &str, _size: f64, _weight: Weight) -> f64 {
        text.chars().count() as f64 * 7.0
    }

    fn sounds_change() -> Change {
        Change::setting("sounds_enabled", json!(true), json!(false))
    }

    fn proposal(model: &str) -> Proposal {
        Proposal::new(
            "turn off sounds",
            model,
            vec![sounds_change()],
            vec!["Server port is not a setting LocalFlow can change here".into()],
        )
    }

    // ------------------------------------------------------------ state ---

    #[test]
    fn preview_tokens_drop_stale_results() {
        let mut state = SettingsPageState::new();
        assert_eq!(state.begin_preview("   "), None);
        assert_eq!(state.status, Status::Empty);
        let first = state.begin_preview("turn off sounds").unwrap();
        assert!(state.busy());
        assert_eq!(state.pending.as_deref(), Some("turn off sounds"));
        assert_eq!(state.begin_preview("again"), None); // one request at a time

        state.cancel();
        assert!(!state.finish_preview(first, Some(proposal("m")), None, None)); // cancelled
        assert_eq!(state.status, Status::Empty);

        let second = state.begin_preview("turn off sounds").unwrap();
        assert_ne!(second, first);
        assert!(state.finish_preview(second, Some(proposal("gemma4:e4b")), None, None));
        assert_eq!(state.status, Status::Preview);
        assert_eq!(state.lines, vec!["Sounds: On → Off"]);
        assert_eq!(
            state.notes,
            vec!["Server port is not a setting LocalFlow can change here"]
        );
        assert_eq!(
            state.warnings,
            vec!["Sounds: On → Off — Check: turns Sounds off."]
        );
        assert_eq!(state.model.as_deref(), Some("gemma4:e4b"));
        assert_eq!(
            model_note(state.model.as_deref()),
            "Runs on your Mac with gemma4:e4b"
        );
        assert!(!state.finish_preview(second, Some(proposal("m")), None, None));
        // delivered once
    }

    #[test]
    fn blank_request_keeps_the_draft_and_busy_blocks_a_second_request() {
        let mut state = SettingsPageState::new();
        assert_eq!(state.begin_preview("  \n "), None);
        assert_eq!(state.draft, "  \n ");
        let token = state.begin_preview("  turn off sounds  ").unwrap();
        assert_eq!(state.pending.as_deref(), Some("turn off sounds"));
        assert_eq!(state.draft, "  turn off sounds  ");
        assert_eq!(state.token, token);
        // A refused second request still records the draft.
        assert_eq!(state.begin_preview("other"), None);
        assert_eq!(state.draft, "other");
        assert_eq!(state.token, token);
    }

    #[test]
    fn error_applied_and_undone_states() {
        let mut state = SettingsPageState::new();
        let token = state.begin_preview("x").unwrap();
        state.finish_preview(token, None, Some("Couldn't reach Ollama"), None);
        assert_eq!(state.status, Status::Error);
        assert_eq!(state.message, "Couldn't reach Ollama");
        assert_eq!(state.draft, "x"); // kept so the user can retry

        let token = state.begin_preview("x").unwrap();
        state.finish_preview(token, Some(proposal("m")), None, None);
        let change = state.proposal.as_ref().unwrap().changes[0].clone();
        state.applied(&ApplyResult {
            applied: vec![change],
            message: "Applied 1 change.".into(),
        });
        assert_eq!(state.status, Status::Applied);
        assert_eq!(state.message, "Applied 1 change.");
        assert_eq!(state.draft, "");
        assert_eq!(state.lines, vec!["Sounds: On → Off"]);
        assert!(state.proposal.is_none() && state.warnings.is_empty());

        state.undone(&UndoResult {
            reverted: vec!["Sounds: On".into()],
            skipped: vec!["word “x”".into()],
            message: "Undid the last change.".into(),
        });
        assert_eq!(state.status, Status::Applied);
        assert_eq!(state.notes, vec!["word “x”"]);
        assert_eq!(state.lines, vec!["Sounds: On"]);
        assert_eq!(model_note(None), NOTE_UNKNOWN);
        assert_eq!(model_note(Some("")), NOTE_UNKNOWN);
        state.failed("bad\nthing → \u{202e}");
        assert_eq!(state.status, Status::Error);
        assert_eq!(state.message, "bad\\u000athing \\u2192 \\u202e");
        assert!(state.lines.is_empty());
    }

    #[test]
    fn everything_shown_is_escaped_never_verbatim() {
        let mut state = SettingsPageState::new();
        let token = state.begin_preview("x").unwrap();
        let mut p = proposal("evil\nmodel");
        p.rejected = vec!["a → b\n".into()];
        state.finish_preview(token, Some(p), None, Some("ignored-when-proposal-has-one"));
        assert_eq!(state.model.as_deref(), Some("evil\\u000amodel"));
        assert_eq!(state.notes, vec!["a \\u2192 b\\u000a"]);
        let token = state.begin_preview("x").unwrap();
        assert!(state.finish_preview(token, None, Some("boom"), Some("m\u{7}")));
        assert_eq!(state.model.as_deref(), Some("m\\u0007"));
        // No proposal and no error: shown as an error, not a crash.
        let token = state.begin_preview("x").unwrap();
        assert!(state.finish_preview(token, None, None, None));
        assert_eq!(
            (state.status, state.message.as_str()),
            (Status::Error, PREVIEW_FAILED)
        );
    }

    #[test]
    fn preview_warnings_use_the_proposal_request_or_the_pending_one() {
        let mut state = SettingsPageState::new();
        let token = state.begin_preview("something unrelated").unwrap();
        let mut p = proposal("m");
        p.request = String::new();
        state.finish_preview(token, Some(p), None, None);
        assert_eq!(
            state.warnings,
            vec!["Sounds: On → Off — Check: turns Sounds off; your request doesn't seem to mention this."]
        );
    }

    #[test]
    fn manual_changes_retire_a_stale_preview_or_pending_request() {
        let mut state = SettingsPageState::new();
        let token = state.begin_preview("turn off sounds").unwrap();
        state.finish_preview(token, Some(proposal("m")), None, None);
        let shown = state.token;
        state.manual_saved("hotkey", "Saved: Dictation shortcut: …");
        assert_eq!(
            (state.status, state.message.as_str()),
            (Status::Error, STALE_PREVIEW)
        );
        assert!(state.proposal.is_none() && state.lines.is_empty() && state.warnings.is_empty());
        assert!(state.token > shown);
        assert_eq!(state.manual_notice, "Saved: Dictation shortcut: …");

        // A late answer after a manual change while busy is dropped.
        let token = state.begin_preview("turn off sounds").unwrap();
        state.manual_saved("hotkey", "Saved: x");
        assert_eq!(
            (state.status, state.message.as_str()),
            (Status::Error, STALE_PENDING)
        );
        assert_eq!(state.pending, None);
        assert!(!state.finish_preview(token, Some(proposal("m")), None, None));
        assert_eq!(state.status, Status::Error);
        // Idle/applied states are left alone.
        let mut idle = SettingsPageState::new();
        idle.manual_saved("hotkey", "Saved");
        assert_eq!(idle.status, Status::Empty);
        assert_eq!(idle.token, 0);
    }

    #[test]
    fn manual_drafts_errors_and_notice_bounds() {
        let mut state = SettingsPageState::new();
        state.manual_notice = "old".into();
        state.manual_failed("hotkey", Some("ctrl+"), "not a valid\nshortcut");
        assert_eq!(state.manual_drafts["hotkey"], "ctrl+");
        assert_eq!(state.manual_errors["hotkey"], "not a valid\\u000ashortcut");
        assert_eq!(state.manual_notice, "");
        state.manual_failed("polish_hotkey", None, "nope");
        assert!(!state.manual_drafts.contains_key("polish_hotkey"));
        let long: String = "é".repeat(600);
        state.manual_draft("prompt_hotkey", &long);
        assert_eq!(
            state.manual_drafts["prompt_hotkey"].chars().count(),
            MAX_DRAFT_CHARS
        );
        state.manual_saved("hotkey", &"ü".repeat(600));
        assert_eq!(state.manual_notice.chars().count(), MAX_DRAFT_CHARS);
        assert!(!state.manual_drafts.contains_key("hotkey"));
        assert!(!state.manual_errors.contains_key("hotkey"));
        assert!(state.manual_errors.contains_key("polish_hotkey"));
        state.applied(&ApplyResult {
            applied: vec![],
            message: "m".into(),
        });
        assert!(state.manual_drafts.is_empty() && state.manual_errors.is_empty());
        assert_eq!(state.manual_notice, "");
    }

    // ----------------------------------------------------------- layout ---

    fn status_state(status: &str) -> SettingsPageState {
        let mut state = SettingsPageState::new();
        if status != "empty" {
            let token = state.begin_preview("turn off sounds").unwrap();
            match status {
                "preview" => {
                    state.finish_preview(token, Some(proposal("m")), None, None);
                }
                "error" => {
                    state.finish_preview(token, None, Some(&"A long error ".repeat(12)), None);
                }
                "applied" => {
                    state.finish_preview(token, Some(proposal("m")), None, None);
                    let applied = state.proposal.as_ref().unwrap().changes.clone();
                    state.applied(&ApplyResult {
                        applied,
                        message: "Applied 1 change.".into(),
                    });
                }
                _ => {}
            }
        }
        state.undo_lines = Some(vec!["Sounds: Off → On".into()]);
        state
    }

    fn bottom(r: Rect) -> f64 {
        r.1 + r.3
    }

    #[test]
    fn layout_fits_and_never_overlaps() {
        for width in [MIN_CONTENT, WIDE_CONTENT] {
            for status in ["empty", "busy", "preview", "applied", "error"] {
                let state = status_state(status);
                let layout = settings_layout(width, TOP, &state, 8, &measure, None);
                assert_eq!(layout.main_w, width);
                let stack = [
                    layout.box_label,
                    layout.intro,
                    layout.request_box,
                    layout.preview,
                    layout.card,
                    layout.undo,
                ];
                for pair in stack.windows(2) {
                    assert!(bottom(pair[0]) <= pair[1].1, "{status} {pair:?}");
                }
                for (x, y, w, h) in stack.into_iter().chain([layout.current]) {
                    assert!(
                        x >= 0.0 && x + w <= width && y >= TOP && h >= 0.0,
                        "{status}"
                    );
                }
                assert_eq!(layout.card.2, width);
                assert_eq!((layout.current.0, layout.current.2), (0.0, width));
                assert!(layout.current.1 >= bottom(layout.undo));
                assert!(layout.bottom >= bottom(layout.current));
                assert_eq!(layout.apply.is_some(), status == "preview");
                assert_eq!(layout.cancel.is_some(), status == "preview");
                let card_h = layout.card.3;
                for (x, y, w, h) in &layout.rows {
                    assert!(y + h <= card_h && x + w <= layout.main_w);
                }
                assert_eq!(layout.current_rows.len(), 8);
                assert_eq!(layout.undo_detail, "Sounds: Off → On");
            }
        }
    }

    #[test]
    fn empty_state_numbers() {
        let state = SettingsPageState::new();
        let layout = settings_layout(600.0, 48.0, &state, 0, &measure, Some(100.0));
        assert_eq!(layout.box_label, (0.0, 48.0, 600.0, 22.0));
        // INTRO is 72 chars * 7 = 504 <= 600: one line.
        assert_eq!(layout.intro, (0.0, 72.0, 600.0, 18.0));
        assert_eq!(layout.request_box, (0.0, 98.0, 600.0, 96.0));
        assert_eq!(layout.preview, (0.0, 206.0, 120.0, 36.0));
        assert_eq!(layout.note, (132.0, 215.0, 468.0, 18.0));
        assert_eq!(layout.card_title, (16.0, 16.0, 568.0, 22.0));
        let text = layout.card_text.unwrap();
        assert_eq!(text.1, 44.0);
        assert_eq!(layout.undo_detail, NOTHING_TO_UNDO);
        assert_eq!(layout.current.3, 100.0);
        assert_eq!(layout.bottom, layout.current.1 + 100.0 + 16.0);
    }

    #[test]
    fn long_change_lines_wrap_in_full_and_short_rows_keep_their_size() {
        use crate::text_wrap::wrap_text;
        for width in [MIN_CONTENT, WIDE_CONTENT] {
            for status in ["preview", "applied"] {
                let long_text = format!("{}{}", "Ünïcødé ẞtraße — ".repeat(5), "x".repeat(15));
                let long_change = Change::replacement(&"ü".repeat(10), &long_text);
                assert_eq!(
                    long_change.replacement_pair().unwrap().1.chars().count(),
                    100
                );
                let short = sounds_change();
                let mut state = SettingsPageState::new();
                let token = state.begin_preview("x").unwrap();
                state.finish_preview(
                    token,
                    Some(Proposal::new(
                        "x",
                        "m",
                        vec![short.clone(), long_change.clone()],
                        vec![],
                    )),
                    None,
                    None,
                );
                if status == "applied" {
                    state.applied(&ApplyResult {
                        applied: vec![short.clone(), long_change.clone()],
                        message: "Applied 2 changes.".into(),
                    });
                }
                let layout = settings_layout(width, TOP, &state, 8, &measure, None);
                let (_sx, sy, sw, sh) = layout.rows[0];
                let (_lx, ly, lw, lh) = layout.rows[1];
                let inner = layout.main_w - 2.0 * CARD_PAD;
                assert_eq!(sh, ROW_H);
                assert_eq!((sw, lw), (inner, inner));
                let line = long_change.line();
                let lines = wrap_text(&line, inner, &measure, 13.0, Weight::Semibold);
                assert_eq!(
                    lines.len() > 1,
                    measure(&line, 13.0, Weight::Semibold) > inner
                );
                assert_eq!(
                    lines.concat().replace(' ', ""),
                    line.replace(' ', ""),
                    "every character is shown somewhere"
                );
                assert!(lh >= lines.len() as f64 * LINE_H);
                assert_eq!(ly, sy + sh); // stacked, no overlap
                assert!(ly + lh <= layout.card.3);
                if status == "preview" {
                    assert!(layout.apply.unwrap().1 >= ly + lh);
                }
            }
        }
    }

    #[test]
    fn change_lines_are_measured_with_the_semibold_font() {
        // Wider glyphs only for semibold: the rows must grow, plain text not.
        let wide_semibold = |text: &str, _s: f64, w: Weight| {
            text.chars().count() as f64 * if w == Weight::Semibold { 40.0 } else { 7.0 }
        };
        let state = status_state("preview");
        let narrow = settings_layout(MIN_CONTENT, TOP, &state, 0, &measure, None);
        let wide = settings_layout(MIN_CONTENT, TOP, &state, 0, &wide_semibold, None);
        assert!(wide.rows[0].3 > narrow.rows[0].3);
        assert_eq!(wide.intro, narrow.intro);
    }

    #[test]
    fn warnings_and_notes_get_their_own_rows() {
        let state = status_state("preview");
        let layout = settings_layout(MIN_CONTENT, TOP, &state, 0, &measure, None);
        let title = layout.warnings_title.unwrap();
        assert_eq!(layout.warning_rows.len(), 1);
        assert!(layout.warning_rows[0].1 >= bottom(title));
        // rows: one change row then one note row, after the warnings block.
        assert_eq!(layout.rows.len(), 2);
        assert!(layout.rows[1].1 >= bottom(layout.warning_rows[0]));
        assert!(layout.apply.unwrap().1 >= bottom(layout.rows[1]));
    }

    #[test]
    fn long_undo_text_and_narrow_widths_stay_inside() {
        let mut state = status_state("empty");
        state.undo_lines = Some(
            (0..6)
                .map(|i| format!("Remove word “word number {i}”"))
                .collect(),
        );
        let layout = settings_layout(MIN_CONTENT, TOP, &state, 8, &measure, None);
        assert!(layout.undo_text.3 > LINE_H);
        assert!(layout.current.1 >= layout.undo_text.1 + layout.undo_text.3);
        // Degenerate widths never produce negative sizes.
        let tiny = settings_layout(100.0, TOP, &state, 2, &measure, None);
        for (_x, _y, w, h) in [tiny.note, tiny.card_title] {
            assert!(w >= 0.0 && h >= 0.0);
        }
    }

    // ----------------------------------------------------- manual layout ---

    fn rows_with(agent: &FakeSettingsAgent) -> Vec<ManualSetting> {
        agent.manual_settings(None).unwrap()
    }

    fn agent() -> FakeSettingsAgent {
        FakeSettingsAgent::new()
    }

    #[test]
    fn manual_control_widths() {
        assert_eq!(manual_panel_width(700.0), 700.0);
        assert_eq!(
            manual_control_width("switch", 500.0),
            f64::from(SWITCH_WIDTH)
        );
        assert_eq!(manual_control_width("choice", 1000.0), 240.0);
        assert_eq!(manual_control_width("shortcut", 400.0), 200.0);
        assert_eq!(manual_control_width("shortcut", 100.0), 120.0);
    }

    #[test]
    fn manual_layout_rows_never_overlap_at_any_width() {
        let rows = rows_with(&agent());
        for width in [MIN_CONTENT, WIDE_CONTENT, 300.0] {
            let mut state = SettingsPageState::new();
            state.manual_notice = "Saved: Sounds: On → Off".into();
            state
                .manual_errors
                .insert("hotkey".into(), "e ".repeat(120));
            let layout = manual_layout(width, &rows, &state, &measure);
            assert_eq!(layout.rows.len(), 24);
            assert_eq!(
                layout
                    .groups
                    .iter()
                    .map(|(g, _)| g.as_str())
                    .collect::<Vec<_>>(),
                ["Dictation", "Cleanup & style", "Shortcuts", "Meetings"]
            );
            let inner = width - 2.0 * CARD_PAD;
            let mut last_bottom = bottom(layout.notice.unwrap());
            let mut group_names = layout.groups.iter();
            let mut next_group = group_names.next();
            let mut current_group = String::new();
            for row in &rows {
                if row.group != current_group {
                    current_group = row.group.clone();
                    let (name, frame) = next_group.unwrap();
                    assert_eq!(name, &current_group);
                    assert!(frame.1 >= last_bottom);
                    last_bottom = bottom(*frame);
                    next_group = group_names.next();
                }
                let frames = layout.row(&row.key).unwrap();
                let top = frames.label.1.min(frames.control.1);
                assert!(top >= last_bottom - 0.0001, "{} overlaps", row.key);
                // Label left, control right-aligned on the same line.
                assert_eq!(frames.label.0, CARD_PAD);
                assert!((frames.control.0 + frames.control.2 - (CARD_PAD + inner)).abs() < 1e-9);
                assert!(frames.label.0 + frames.label.2 <= frames.control.0);
                assert!(
                    frames.label.1 + frames.label.3 <= frames.control.1 + frames.control.3 + 20.0
                );
                last_bottom = bottom(frames.label).max(bottom(frames.control));
                if let Some(error) = frames.error {
                    assert_eq!(row.key, "hotkey");
                    assert!(error.1 >= last_bottom);
                    assert_eq!(error.2, inner);
                    assert!(error.3 > LINE_H, "a long error wraps");
                    last_bottom = bottom(error);
                }
            }
            assert!(layout.height >= last_bottom + CARD_PAD);
            let switch = layout.row("sounds_enabled").unwrap();
            assert_eq!(
                (switch.control.2, switch.control.3),
                (f64::from(SWITCH_WIDTH), SWITCH_H)
            );
            let field = layout.row("hotkey").unwrap();
            assert_eq!(field.control.3, CONTROL_H);
        }
    }

    #[test]
    fn manual_layout_without_rows_or_notice_is_just_the_header() {
        let layout = manual_layout(600.0, &[], &SettingsPageState::new(), &measure);
        assert_eq!(layout.title, (16.0, 16.0, 568.0, 22.0));
        assert_eq!(layout.intro, (16.0, 40.0, 568.0, 18.0));
        assert!(layout.notice.is_none() && layout.rows.is_empty() && layout.groups.is_empty());
        assert_eq!(layout.height, 58.0 + 6.0 + 16.0);
    }

    #[test]
    fn long_labels_wrap_in_their_column() {
        let mut row = rows_with(&agent())[0].clone();
        row.label = "A very long label ".repeat(10);
        let layout = manual_layout(
            MIN_CONTENT,
            &[row.clone()],
            &SettingsPageState::new(),
            &measure,
        );
        let frames = layout.row(&row.key).unwrap();
        assert!(frames.label.3 > LINE_H);
        // The control is centred against the taller label.
        let centre = frames.control.1 + frames.control.3 / 2.0;
        assert!((centre - (frames.label.1 + frames.label.3 / 2.0)).abs() < 1e-9);
    }

    // ------------------------------------------------------- presentation ---

    #[test]
    fn card_presentation_follows_the_status() {
        assert_eq!(card_title(Status::Empty), ("Try asking", "ink"));
        assert_eq!(card_title(Status::Busy), (BUSY_PREVIEW, "cobalt"));
        assert_eq!(card_title(Status::Applied), (APPLIED_TITLE, "success"));
        assert_eq!(card_title(Status::Error), (ERROR_TITLE, "red"));
        let mut state = SettingsPageState::new();
        assert_eq!(card_text(&state).as_deref(), Some(EMPTY_HINT));
        state.begin_preview("turn off “sounds”\n");
        assert_eq!(card_text(&state).as_deref(), Some("“turn off “sounds””"));
        state.pending = Some("a\nb".into());
        assert_eq!(card_text(&state).as_deref(), Some("“a\\u000ab”"));
        state.status = Status::Preview;
        assert_eq!(card_text(&state), None);
        assert_eq!(warning_text("w"), "⚠ w");
        assert_eq!(skipped_text("n"), "Skipped: n");
        assert_eq!(Status::Preview.as_str(), "preview");
        assert!(PLACEHOLDER.contains("Turn off sounds"));
        assert_eq!(HEADING, "Tell LocalFlow what to change");
    }

    #[test]
    fn choice_popups_skip_the_placeholder_for_an_invalid_value() {
        let backend = agent();
        let rows = rows_with(&backend);
        let level = rows.iter().find(|r| r.key == "cleanup_level").unwrap();
        let (titles, selected) = choice_items(level);
        assert_eq!(titles, ["None", "Light", "Medium"]);
        assert_eq!(selected, 1);
        assert_eq!(choice_value_at(level, 2).as_deref(), Some("medium"));
        assert_eq!(choice_value_at(level, 3), None);
        assert_eq!(choice_value_at(level, -1), None);

        // The agent reports a stored value it rejected: no value, not valid.
        let mut invalid = rows.clone();
        let broken = invalid
            .iter_mut()
            .find(|r| r.key == "cleanup_level")
            .unwrap();
        broken.value = None;
        broken.valid = false;
        broken.display = "Not set".into();
        backend.set_rows(invalid);
        let rows = rows_with(&backend);
        let level = rows.iter().find(|r| r.key == "cleanup_level").unwrap();
        assert!(!level.valid);
        let (titles, selected) = choice_items(level);
        assert_eq!(titles, ["Choose…", "None", "Light", "Medium"]);
        assert_eq!(selected, 0);
        assert_eq!(choice_value_at(level, 0), None); // the placeholder
        assert_eq!(choice_value_at(level, 1).as_deref(), Some("none"));
        assert_eq!(choice_value_at(level, 3).as_deref(), Some("medium"));
    }

    #[test]
    fn shortcut_fields_show_the_draft_then_the_saved_value() {
        let backend = agent();
        let rows = rows_with(&backend);
        let hotkey = rows.iter().find(|r| r.key == "hotkey").unwrap();
        let mut state = SettingsPageState::new();
        assert_eq!(shortcut_field_text(hotkey, &state), "left_option");
        state.manual_draft("hotkey", "oops+");
        assert_eq!(shortcut_field_text(hotkey, &state), "oops+");
        assert_eq!(shortcut_placeholder("hotkey"), "e.g. right_option");
        assert_eq!(shortcut_placeholder("polish_hotkey"), "Off");
        let polish = rows.iter().find(|r| r.key == "polish_hotkey").unwrap();
        assert_eq!(
            shortcut_field_text(polish, &SettingsPageState::new()),
            "ctrl+option+1"
        );
    }

    // ------------------------------------------------------- controller ---

    fn controller_over(agent: Arc<FakeSettingsAgent>) -> SettingsPageController {
        SettingsPageController::new(Box::new(move || {
            Box::new(Arc::clone(&agent)) as Box<dyn SettingsAgent>
        }))
    }

    fn setup() -> (Arc<FakeSettingsAgent>, SettingsPageController) {
        let backend = Arc::new(agent());
        let mut controller = controller_over(Arc::clone(&backend));
        controller.refresh_local();
        (backend, controller)
    }

    fn applied(changes: Vec<Change>, message: &str) -> Result<ApplyResult, SettingsAgentError> {
        Ok(ApplyResult {
            applied: changes,
            message: message.into(),
        })
    }

    fn refused(message: &str) -> Result<ApplyResult, SettingsAgentError> {
        Err(SettingsAgentError(message.into()))
    }

    /// The saved-from-the-page notice of one manual setting change.
    fn spoken_symbols_off() -> Change {
        Change::setting("spoken_symbols", json!(true), json!(false))
    }

    #[test]
    fn preview_runs_off_thread_then_apply_and_undo() {
        let (backend, mut controller) = setup();
        assert_eq!(controller.manual_rows.len(), 24);
        assert!(controller.state.undo_lines.is_none());
        backend.script_proposal(proposal("gemma4:e4b"));
        assert!(controller.start_preview("   ").is_none());
        let job = controller.start_preview("turn off sounds").unwrap();
        assert!(controller.state.busy());
        assert!(controller.start_preview("again").is_none());
        std::thread::spawn(move || job.run()).join().unwrap();
        assert!(controller.preview_done());
        assert_eq!(controller.state.status, Status::Preview);
        assert_eq!(controller.state.model.as_deref(), Some("gemma4:e4b"));
        assert!(!controller.preview_done()); // delivered once
                                             // Only the real request reached the agent: blank and busy ones never do.
        assert_eq!(
            backend.calls(),
            vec![FakeCall::Propose("turn off sounds".into())]
        );

        // Apply hands the previewed changes to the agent and shows its result.
        backend.script_apply(applied(vec![sounds_change()], "Applied 1 change."));
        controller.apply();
        assert_eq!(controller.state.status, Status::Applied);
        assert_eq!(controller.state.lines, vec!["Sounds: On → Off"]);
        assert_eq!(controller.state.draft, "");
        assert_eq!(
            backend.calls().last(),
            Some(&FakeCall::Apply(vec![sounds_change()]))
        );
        backend.set_undo_lines(Some(vec!["Sounds: Off → On".to_string()]));
        controller.refresh_local();
        assert_eq!(
            controller.state.undo_lines,
            Some(vec!["Sounds: Off → On".to_string()])
        );

        backend.script_undo(Ok(UndoResult {
            reverted: vec!["Sounds: On".into()],
            skipped: vec![],
            message: "Undid the last change.".into(),
        }));
        controller.undo();
        assert_eq!(controller.state.status, Status::Applied);
        assert_eq!(controller.state.message, "Undid the last change.");
        backend.script_undo(Err(SettingsAgentError("There's no change to undo.".into())));
        controller.undo(); // nothing left
        assert_eq!(controller.state.status, Status::Error);
        assert_eq!(controller.state.message, "There's no change to undo.");
        // Apply is a no-op outside the preview state: the agent is not asked.
        let asked = backend.calls().len();
        controller.apply();
        assert_eq!(controller.state.status, Status::Error);
        assert_eq!(backend.calls().len(), asked);
    }

    #[test]
    fn a_dictation_preview_shows_its_line_and_safety_warning() {
        let (backend, mut controller) = setup();
        let off = Change::setting("dictation_enabled", json!(true), json!(false));
        backend.script_proposal(Proposal::new(
            "Turn off dictation.",
            "gemma4:e4b",
            vec![off.clone()],
            vec![],
        ));
        let job = controller.start_preview("Turn off dictation.").unwrap();
        std::thread::spawn(move || job.run()).join().unwrap();
        assert!(controller.preview_done());
        assert_eq!(controller.state.status, Status::Preview);
        assert_eq!(controller.state.lines, vec!["Dictation: On → Off"]);
        assert_eq!(
            controller.state.warnings,
            vec!["Dictation: On → Off — Check: turns Dictation off."]
        );

        backend.script_apply(applied(vec![off], "Applied 1 change."));
        controller.apply();
        assert_eq!(controller.state.status, Status::Applied);
        assert_eq!(controller.state.lines, vec!["Dictation: On → Off"]);
    }

    #[test]
    fn a_refused_apply_shows_the_agents_message_and_drops_the_preview() {
        let (backend, mut controller) = setup();
        backend.script_proposal(proposal("m"));
        controller.start_preview("turn off sounds").unwrap().run();
        controller.preview_done();
        backend.script_apply(refused("Nothing was changed: your settings changed."));
        controller.apply();
        assert_eq!(controller.state.status, Status::Error);
        assert_eq!(
            controller.state.message,
            "Nothing was changed: your settings changed."
        );
        assert!(controller.state.proposal.is_none());
    }

    #[test]
    fn cancel_while_busy_ignores_the_late_answer() {
        let (backend, mut controller) = setup();
        backend.script_proposal(proposal("m"));
        let job = controller.start_preview("turn off sounds").unwrap();
        controller.cancel();
        assert_eq!(controller.state.status, Status::Empty);
        job.run();
        assert!(!controller.preview_done());
        assert_eq!(controller.state.status, Status::Empty);
        assert!(controller.state.proposal.is_none());
    }

    #[test]
    fn errors_are_friendly_and_keep_the_draft() {
        let (backend, mut controller) = setup();
        backend.script_error(
            Some("gemma4:e4b"),
            "Couldn't reach Ollama at http://localhost:11434.",
        );
        let job = controller.start_preview("turn off sounds").unwrap();
        job.run();
        assert!(controller.preview_done());
        assert_eq!(controller.state.status, Status::Error);
        assert_eq!(controller.state.draft, "turn off sounds");
        assert_eq!(
            controller.state.message,
            "Couldn't reach Ollama at http://localhost:11434."
        );
        assert_eq!(controller.state.model.as_deref(), Some("gemma4:e4b"));
        // Nothing scripted: the fake says so instead of inventing a change.
        let job = controller.start_preview("turn off sounds").unwrap();
        job.run();
        controller.preview_done();
        assert!(controller
            .state
            .message
            .contains("no scripted model answer"));
        assert_eq!(controller.state.status, Status::Error);
    }

    #[test]
    fn manual_changes_go_through_set_setting_and_retire_a_preview() {
        let (backend, mut controller) = setup();
        backend.script_proposal(proposal("m"));
        controller.start_preview("turn off sounds").unwrap().run();
        controller.preview_done();
        assert_eq!(controller.state.status, Status::Preview);

        backend.script_set_setting(applied(vec![spoken_symbols_off()], "Applied 1 change."));
        controller.manual_toggle("spoken_symbols", false);
        assert_eq!(
            backend.calls().last(),
            Some(&FakeCall::SetSetting {
                key: "spoken_symbols".into(),
                value: json!(false)
            })
        );
        assert_eq!(
            controller.state.manual_notice,
            "Saved: Spoken symbols: On → Off"
        );
        assert_eq!(controller.state.status, Status::Error);
        assert_eq!(controller.state.message, STALE_PREVIEW);
        // Apply of the retired preview is a no-op: the agent is not asked.
        let asked = backend.calls().len();
        controller.apply();
        assert_eq!(backend.calls().len(), asked);

        // Unchanged value: the agent applied nothing, the notice stays.
        backend.script_set_setting(applied(vec![], "Nothing needed changing."));
        controller.manual_toggle("spoken_symbols", false);
        assert_eq!(
            controller.state.manual_notice,
            "Saved: Spoken symbols: On → Off"
        );
        backend.set_undo_lines(Some(vec!["Spoken symbols: Off → On".to_string()]));
        controller.refresh_local();
        assert_eq!(
            controller.state.undo_lines,
            Some(vec!["Spoken symbols: Off → On".to_string()])
        );
    }

    fn set_setting_calls(backend: &FakeSettingsAgent) -> Vec<(String, Value)> {
        backend
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                FakeCall::SetSetting { key, value } => Some((key, value)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn manual_choose_and_shortcut_commit_and_errors() {
        let (backend, mut controller) = setup();
        backend.script_set_setting(applied(
            vec![Change::setting(
                "style_work",
                json!("casual"),
                json!("excited"),
            )],
            "Applied 1 change.",
        ));
        controller.manual_choose("style_work", 3); // formal, casual, very_casual, excited
        assert_eq!(
            set_setting_calls(&backend),
            vec![("style_work".to_string(), json!("excited"))]
        );
        assert_eq!(
            controller.state.manual_notice,
            "Saved: Work style: Casual → Excited"
        );
        controller.manual_choose("style_work", 9); // out of range: ignored
        controller.manual_choose("nope", 0);
        assert_eq!(set_setting_calls(&backend).len(), 1);

        // Unchanged text and no error: nothing is sent to the agent.
        controller.manual_shortcut("hotkey", "left_option");
        assert_eq!(set_setting_calls(&backend).len(), 1);
        // A shortcut the agent refuses stays in the field with its error.
        backend.script_set_setting(refused("“a” is not a key you can hold"));
        controller.manual_shortcut("hotkey", "a");
        assert_eq!(
            set_setting_calls(&backend).last(),
            Some(&("hotkey".to_string(), json!("a")))
        );
        assert!(controller.state.manual_errors["hotkey"].contains("is not a key you can hold"));
        assert_eq!(controller.state.manual_drafts["hotkey"], "a");
        // The unchanged text is retried because an error is showing.
        backend.script_set_setting(applied(vec![], "Nothing needed changing."));
        controller.manual_shortcut("hotkey", "left_option");
        assert_eq!(
            set_setting_calls(&backend).last(),
            Some(&("hotkey".to_string(), json!("left_option")))
        );
        assert!(!controller.state.manual_errors.contains_key("hotkey"));
        assert!(!controller.state.manual_drafts.contains_key("hotkey"));
        // The agent's clash message is shown inline as given.
        backend.script_set_setting(refused(
            "Nothing was changed: ctrl+alt+1 is already the Polish shortcut.",
        ));
        controller.manual_shortcut("prompt_hotkey", "ctrl+alt+1");
        assert!(controller.state.manual_errors["prompt_hotkey"].starts_with("Nothing was changed:"));
        assert!(controller.state.manual_errors["prompt_hotkey"].contains("already the"));
        // Blank means "off": the text sent is the word `off`, not an empty value.
        backend.script_set_setting(applied(vec![], "Nothing needed changing."));
        controller.manual_shortcut("polish_hotkey", "   ");
        assert_eq!(
            set_setting_calls(&backend).last(),
            Some(&("polish_hotkey".to_string(), json!("off")))
        );
    }

    #[test]
    fn capture_keeps_typed_text_and_forgets_matching_fields() {
        let (_backend, mut controller) = setup();
        controller.capture(
            Some("my request"),
            &[
                ("hotkey".into(), "right_option".into()), // edited, uncommitted
                ("polish_hotkey".into(), "ctrl+option+1".into()), // equals saved
                ("sounds_enabled".into(), "ignored".into()), // not a shortcut
                ("ghost".into(), "x".into()),             // unknown key
            ],
        );
        assert_eq!(controller.state.draft, "my request");
        assert_eq!(controller.state.manual_drafts.len(), 1);
        assert_eq!(controller.state.manual_drafts["hotkey"], "right_option");
        // A later capture where the text matches again drops the draft...
        controller.capture(None, &[("hotkey".into(), "left_option".into())]);
        assert!(controller.state.manual_drafts.is_empty());
        assert_eq!(controller.state.draft, "my request");
        // ...but keeps it while an error is showing.
        controller.state.manual_failed("hotkey", Some("a"), "bad");
        controller.capture(None, &[("hotkey".into(), "left_option".into())]);
        assert!(controller.state.manual_drafts.contains_key("hotkey"));
    }

    #[test]
    fn repaint_only_while_open_and_on_settings() {
        let (_backend, mut controller) = setup();
        assert!(controller.should_repaint("Settings"));
        assert!(!controller.should_repaint("Home"));
        controller.shutdown();
        assert!(!controller.should_repaint("Settings"));
    }

    #[test]
    fn unreadable_settings_leave_the_page_usable() {
        let (backend, mut controller) = setup();
        backend.set_load_error(Some("bad toml"));
        controller.refresh_local();
        assert!(controller.manual_rows.is_empty());
        assert!(controller.state.undo_lines.is_none());
        // Writes report the agent's load failure inline.
        backend.script_set_setting(refused("Couldn't read your settings (bad toml)."));
        controller.manual_toggle("sounds_enabled", false);
        assert_eq!(
            controller.state.manual_errors["sounds_enabled"],
            "Couldn't read your settings (bad toml)."
        );
    }

    struct FailingAgent;

    impl SettingsAgent for FailingAgent {
        fn load(&self) -> Result<lf_core::config::Config, SettingsAgentError> {
            Ok(lf_core::config::Config::default())
        }
        fn current_settings(
            &self,
            _: Option<&lf_core::config::Config>,
        ) -> Result<Vec<(String, String)>, SettingsAgentError> {
            Ok(vec![])
        }
        fn manual_settings(
            &self,
            _: Option<&lf_core::config::Config>,
        ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
            Err(SettingsAgentError("nope".into()))
        }
        fn pending_undo(&self) -> Option<Vec<String>> {
            None
        }
        fn propose(&self, _: &str) -> Result<Proposal, SettingsAgentError> {
            Err(SettingsAgentError("Couldn't reach Ollama".into()))
        }
        fn set_setting(&self, _: &str, _: &Value) -> Result<ApplyResult, SettingsAgentError> {
            Err(SettingsAgentError("no".into()))
        }
        fn apply(&self, _: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
            Err(SettingsAgentError("couldn't save".into()))
        }
        fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
            Err(SettingsAgentError("There's no change to undo.".into()))
        }
        fn model(&self) -> Option<String> {
            None
        }
    }

    #[test]
    fn a_failing_agent_surfaces_errors_without_panicking() {
        let mut controller = SettingsPageController::new(Box::new(|| Box::new(FailingAgent)));
        controller.refresh_local();
        assert!(controller.manual_rows.is_empty());
        controller.start_preview("anything").unwrap().run();
        assert!(controller.preview_done());
        assert_eq!(
            (controller.state.status, controller.state.message.as_str()),
            (Status::Error, "Couldn't reach Ollama")
        );
        assert_eq!(controller.state.model, None);
        // An apply that fails shows the agent's message and drops the proposal.
        controller.state.status = Status::Preview;
        controller.state.proposal = Some(proposal("m"));
        controller.apply();
        assert_eq!(
            (controller.state.status, controller.state.message.as_str()),
            (Status::Error, "couldn't save")
        );
        assert!(controller.state.proposal.is_none());
    }

    // ------------------------------------------------ speaker-label switches

    /// The status body both servers return (Python `api_diarization_status`,
    /// Rust `speakers::diarization_status` + `download`).
    fn diarization_status(installed: bool, models: bool, reason: &str) -> Value {
        json!({
            "models_ready": models, "reason": reason, "extra_installed": installed,
            "models": [], "speaker_diarization": false, "diarize_mic": false,
            "speaker_recognition": false,
            "download": {"state": "idle", "done_bytes": 0, "total_bytes": 0, "error": null},
        })
    }

    #[test]
    fn speaker_readiness_follows_the_server_capability_and_reason() {
        assert_eq!(speaker_readiness(None), SpeakerReadiness::Checking);
        assert_eq!(
            speaker_readiness(Some(&Ok(diarization_status(true, true, "")))),
            SpeakerReadiness::Ready
        );
        assert_eq!(
            speaker_readiness(Some(&Ok(diarization_status(false, true, "")))),
            SpeakerReadiness::NotReady(SPEAKERS_NO_RUNTIME.into())
        );
        let reason = "The speaker model cache folder isn't a plain folder.";
        assert_eq!(
            speaker_readiness(Some(&Ok(diarization_status(true, false, reason)))),
            SpeakerReadiness::NotReady(reason.into())
        );
        assert_eq!(
            speaker_readiness(Some(&Ok(diarization_status(true, false, " ")))),
            SpeakerReadiness::NotReady(SPEAKERS_NOT_DOWNLOADED.into())
        );
        // An older server (404) or a down one: not ready, and says why.
        assert_eq!(
            speaker_readiness(Some(&Err("Not Found".into()))),
            SpeakerReadiness::NotReady("Couldn't check the speaker models: Not Found".into())
        );
        // A reason is display text, never raw control characters.
        let SpeakerReadiness::NotReady(shown) =
            speaker_readiness(Some(&Ok(diarization_status(true, false, "bad\nreason"))))
        else {
            panic!("not ready expected");
        };
        assert!(!shown.contains('\n'));
    }

    #[test]
    fn speaker_switches_can_always_be_turned_off_but_on_needs_ready_models() {
        let ready = SpeakerReadiness::Ready;
        let missing = SpeakerReadiness::NotReady(SPEAKERS_NOT_DOWNLOADED.into());
        let enabled = |sd: bool, mic: bool, readiness: &SpeakerReadiness, busy: bool| {
            let [labels, mic_switch] = speaker_switches(sd, mic, readiness, busy);
            assert_eq!((labels.on, mic_switch.on), (sd, mic));
            (labels.enabled, mic_switch.enabled)
        };
        assert_eq!(enabled(false, false, &missing, false), (false, false));
        assert_eq!(
            enabled(false, false, &SpeakerReadiness::Checking, false),
            (false, false)
        );
        assert_eq!(enabled(true, true, &missing, false), (true, true));
        assert_eq!(enabled(false, false, &ready, false), (true, false));
        assert_eq!(enabled(true, false, &ready, false), (true, true));
        assert_eq!(enabled(true, true, &ready, true), (false, false));
        let [labels, mic] = speaker_switches(false, false, &ready, false);
        assert_eq!(
            (labels.key, labels.label, mic.key, mic.label),
            (
                "speaker_diarization",
                "Speaker labels",
                "diarize_mic",
                "Label speakers on my microphone"
            )
        );
        assert_eq!(speaker_note(&ready, true), None);
        assert_eq!(
            speaker_note(&ready, false).as_deref(),
            Some(SPEAKERS_MIC_NEEDS_LABELS)
        );
        assert_eq!(
            speaker_note(&missing, true).as_deref(),
            Some(SPEAKERS_NOT_DOWNLOADED)
        );
        assert_eq!(
            speaker_update("diarize_mic", true),
            json!({"meetings": {"diarize_mic": true}})
        );
    }

    #[test]
    fn speaker_section_follows_the_manual_rows_and_never_overlaps() {
        let agent = agent();
        let rows = rows_with(&agent);
        let state = SettingsPageState::new();
        for width in [MIN_CONTENT, WIDE_CONTENT] {
            let manual = manual_layout(width, &rows, &state, &measure);
            let last = manual.rows.last().unwrap().1.clone();
            for (note, error) in [
                (None, ""),
                (Some(SPEAKERS_NOT_DOWNLOADED), ""),
                (
                    Some(SPEAKERS_CHECKING),
                    "Couldn't save the setting: disk full",
                ),
            ] {
                let section = speaker_section_layout(&manual, width, note, error, &measure);
                assert!(section.caption.1 >= last.control.1 + last.control.3);
                let mut frames = vec![section.caption];
                for (_, label, control) in &section.rows {
                    assert!(label.0 + label.2 <= control.0, "label overlaps its switch");
                    assert!(control.0 + control.2 <= width - CARD_PAD + 0.5);
                    frames.push(*label);
                }
                frames.extend(section.note);
                frames.extend(section.error);
                for pair in frames.windows(2) {
                    assert!(pair[0].1 + pair[0].3 <= pair[1].1, "{pair:?}");
                }
                assert_eq!(section.note.is_some(), note.is_some());
                assert_eq!(section.error.is_some(), !error.is_empty());
                let bottom = frames.last().map(|f| f.1 + f.3).unwrap();
                assert!(section.height >= bottom + CARD_PAD);
            }
        }
    }

    #[derive(Clone)]
    struct NoChat;

    impl lf_agent::ChatClient for NoChat {
        fn tags(&self, _: &str) -> anyhow::Result<Value> {
            anyhow::bail!("no model in this test")
        }
        fn chat(&self, _: &str, _: &Value) -> anyhow::Result<Value> {
            anyhow::bail!("no model in this test")
        }
    }

    #[test]
    fn a_speaker_switch_writes_meetings_and_survives_an_agent_write() {
        use lf_core::config::{apply_config_update, load_from, save_config, Config};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save_config(&Config::defaults_for(true), Some(&path)).unwrap();
        crate::config_write::update(Some(&path), |latest| {
            apply_config_update(latest, &speaker_update("speaker_diarization", true))
        })
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("speaker_diarization = true"), "{text}");
        let agent = crate::backends::settings_agent_native::NativeSettingsAgent::with_client(
            NoChat,
            path.clone(),
            dir.path().join("dictionary.json"),
            dir.path().join("undo.json"),
        );
        agent.set_setting("sounds_enabled", &json!(false)).unwrap();
        let saved = load_from(&path).unwrap();
        assert_eq!(saved.field_bool("speaker_diarization"), Some(true));
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
        // Undo restores only the agent's own change.
        agent.undo().unwrap();
        let saved = load_from(&path).unwrap();
        assert_eq!(saved.field_bool("speaker_diarization"), Some(true));
        assert_eq!(saved.field_bool("sounds_enabled"), Some(true));
    }
}
