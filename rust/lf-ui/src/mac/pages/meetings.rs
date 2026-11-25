//! Meetings page (port of `_page_meetings` and every `_meeting_*` path in
//! `localflow/hub_mac.py`).
//!
//! All AppKit objects and all state transitions run on the Hub's main thread.
//! Meeting requests run on worker threads and return through an MPSC queue; a
//! repeating run-loop timer drains that queue, services notes debounce and polls
//! status. Workers never touch a view. A generation attached to each result
//! keeps results belonging to a replaced session (or a closed page) from being
//! applied. Notes writes use the process-wide [`MeetingNotesWriter`], including
//! the final close flush, so an older request can never overwrite newer notes.

#![allow(deprecated)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBorderType, NSButton, NSPopUpButton,
    NSScrollView, NSTextAlignment, NSTextField, NSView, NSWorkspace,
};
use objc2_foundation::{NSRange, NSSize, NSString, NSURL};
use serde_json::{json, Value};

use crate::backends::meeting::{
    vault_notes_path, MeetingClient, MeetingNotesWriter, MeetingResult,
};
use crate::hub_model::{
    meeting_capture_status, meeting_form_layout, meeting_notes_height, meeting_speaker_status,
    meeting_speakers, merge_choice_for_title, merge_picker, merge_target_items, note_date_label,
    note_meeting_id, recent_notes, resolve_merge_target, saved_meeting_id, saved_notes_message,
    speaker_panel_height, speaker_status_height, speaker_talk_time, speaker_titles, MeetingSpeaker,
    MergeTarget, RecentNote, SpeakerEditor, SpeakerMode, MEETING_NOTES_PLACEHOLDER,
    MEETING_SUBTITLE, MEETING_TITLE_PLACEHOLDER, MERGE_PICK_AGAIN, RECENT_EMPTY,
    RECENT_NOTES_LIMIT, RECENT_ROW_H, SPEAKER_ERROR_LINE_H, SPEAKER_HEADER_H, SPEAKER_PAD,
    SPEAKER_ROW_H,
};
use crate::mac::actions::Timer;
use crate::mac::page::{HubPage, PageCtx};
use crate::mac::style::{
    color, font, hide_from_accessibility, input_box, rect, style_surface, ButtonKind, FlippedView,
    PlaceholderTextView, SurfaceStyle,
};
use crate::mac::widgets::{button, field, label, popup, LabelStyle};
use crate::pystr::PyStr;
use crate::text_wrap::wrap_text;
use crate::tokens::{Palette, Weight, CARD_BORDER_WIDTH, CARD_PADDING};

const PAGE: &str = "Meetings";
const TICK_SECONDS: f64 = 0.2;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const NOTES_DEBOUNCE: Duration = Duration::from_millis(800);
const CONNECTING: &str = "Connecting to LocalFlow…";
const LISTENING: &str = "(listening — first transcript segment lands after ~30s)";
const CLOSE_SAVE_FAILED: &str = "Notes could not be saved; the meeting is still running — ";
const OPEN_FAILED: &str = "Couldn't open the meeting notes.";

/// One loaded notes template. The backend payload can contain other fields; the
/// picker needs only these two.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingTemplate {
    pub id: String,
    pub name: String,
}

/// One visible question/answer pair, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingAnswer {
    pub question: String,
    pub answer: String,
    pub error: bool,
}

/// Complete deterministic Meetings content for snapshots and UI tests.
///
/// `status` is the real `/meeting/status` JSON object, or `None` for the
/// connecting/server-down presentation. `last_saved` and `answers` are separate
/// because both also survive status refreshes in the running Hub. Seeding never
/// invokes capture or claims that audio hardware is recording: an `Active`
/// scenario is represented explicitly by a session in `status`.
#[derive(Clone, Debug, PartialEq)]
pub struct MeetingsPageSnapshot {
    pub status: Option<Value>,
    pub last_saved: Option<Value>,
    pub templates: Vec<MeetingTemplate>,
    pub template_default: String,
    pub template: String,
    pub title: String,
    pub title_edited: bool,
    pub notes: String,
    pub question: String,
    pub answers: Vec<MeetingAnswer>,
    pub error: String,
    pub connection_message: String,
    pub retry_visible: bool,
    pub notes_notice: String,
    pub recent: Vec<RecentNote>,
    pub recent_error: String,
}

impl Default for MeetingsPageSnapshot {
    fn default() -> Self {
        Self {
            status: None,
            last_saved: None,
            templates: Vec::new(),
            template_default: "general".into(),
            template: String::new(),
            title: String::new(),
            title_edited: false,
            notes: String::new(),
            question: String::new(),
            answers: Vec::new(),
            error: String::new(),
            connection_message: CONNECTING.into(),
            retry_visible: false,
            notes_notice: String::new(),
            recent: Vec::new(),
            recent_error: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveKind {
    Autosave,
    StopFlush,
}

#[derive(Clone, Debug)]
struct AnswerItem {
    id: u64,
    question: String,
    answer: String,
    error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Templates,
    Status,
    Start,
    NotesAutosave,
    NotesStopFlush,
    Stop,
    Chat,
    SpeakerProbe,
    SpeakerEdit,
}

impl RequestKind {
    fn name(self) -> &'static str {
        match self {
            Self::Templates => "templates",
            Self::Status => "status",
            Self::Start => "start",
            Self::NotesAutosave => "notes_autosave",
            Self::NotesStopFlush => "notes_stop_flush",
            Self::Stop => "stop",
            Self::Chat => "chat",
            Self::SpeakerProbe => "speaker_probe",
            Self::SpeakerEdit => "speaker_edit",
        }
    }
}

/// A rename or merge of one saved meeting's speakers.
enum SpeakerChange {
    Rename { speaker_id: String, name: String },
    Merge { from: String, into: String },
}

enum Request {
    Templates,
    Status,
    Start {
        title: String,
        template: String,
        notes: String,
        auto_title: bool,
    },
    Notes {
        kind: SaveKind,
        ticket: u64,
        text: String,
    },
    Stop,
    Chat {
        id: u64,
        question: String,
    },
    /// Which of these meetings (ids from note frontmatter) have speaker data.
    SpeakerProbe {
        ids: Vec<String>,
    },
    SpeakerEdit {
        token: u64,
        meeting_id: String,
        change: SpeakerChange,
    },
}

enum EventKind {
    Templates(MeetingResult<Value>),
    Status(MeetingResult<Value>),
    Start {
        notes: String,
        result: MeetingResult<Value>,
    },
    Notes {
        kind: SaveKind,
        text: String,
        result: MeetingResult<Value>,
    },
    Stop(MeetingResult<Value>),
    Chat {
        id: u64,
        result: MeetingResult<Value>,
    },
    SpeakerProbe(Vec<(String, MeetingResult<Option<Value>>)>),
    SpeakerEdit {
        token: u64,
        meeting_id: String,
        result: MeetingResult<Value>,
        /// After a failed edit, the meeting's speakers read again once.
        refetch: Option<MeetingResult<Option<Value>>>,
    },
}

struct Event {
    generation: u64,
    kind: EventKind,
}

#[derive(Clone, Copy)]
enum FocusedEditor {
    Title,
    Notes,
    Question,
    SpeakerName,
}

struct Controls {
    title: Retained<NSTextField>,
    template: Retained<NSPopUpButton>,
    notes: Retained<PlaceholderTextView>,
    notes_status: Retained<NSTextField>,
    question: Retained<NSTextField>,
    /// The speaker rename field, while a rename is open.
    speaker_name: Option<Retained<NSTextField>>,
    /// The open merge's picker, its items and its Merge button.
    merge_target: Option<MergeControl>,
    palette: Palette,
}

/// The open merge row as drawn: a pick updates it in place (no rebuild), so
/// the focused picker keeps keyboard focus.
struct MergeControl {
    picker: Retained<NSPopUpButton>,
    /// The picker's items (unique title -> speaker id).
    items: Vec<MergeTarget>,
    button: Retained<NSButton>,
    /// Every picker item is its own speaker (no title collapsed).
    complete: bool,
}

impl MergeControl {
    fn selected_title(&self) -> String {
        self.picker
            .titleOfSelectedItem()
            .map(|title| title.to_string())
            .unwrap_or_default()
    }
}

struct State {
    client: RefCell<Option<Arc<dyn MeetingClient>>>,
    writer: Arc<MeetingNotesWriter>,
    sender: mpsc::Sender<Event>,
    receiver: mpsc::Receiver<Event>,
    timer: RefCell<Option<Timer>>,
    rebuild: RefCell<Option<Rc<dyn Fn()>>>,
    visible: Cell<bool>,
    closed: Cell<bool>,
    generation: Cell<u64>,

    status: RefCell<Option<Value>>,
    status_in_flight: Cell<bool>,
    next_poll: Cell<Option<Instant>>,
    templates: RefCell<Vec<MeetingTemplate>>,
    templates_requested: Cell<bool>,
    template_default: RefCell<String>,
    last_saved: RefCell<Option<Value>>,
    session_active: Cell<bool>,
    snapshot_mode: Cell<bool>,

    title_draft: RefCell<String>,
    title_edited: Cell<bool>,
    title_shows_session: Cell<bool>,
    template_value: RefCell<String>,
    notes_text: RefCell<String>,
    adopting_notes: Cell<bool>,
    question_draft: RefCell<String>,
    answers: RefCell<Vec<AnswerItem>>,
    next_answer: Cell<u64>,

    busy: Cell<bool>,
    enhancing: Cell<bool>,
    error: RefCell<String>,
    connection_message: RefCell<String>,
    retry_visible: Cell<bool>,
    notes_notice: RefCell<String>,

    notes_dirty: Cell<bool>,
    notes_due: Cell<Option<Instant>>,
    save_pending: Cell<bool>,
    stop_flush_pending: Cell<bool>,
    save_in_flight: Cell<bool>,
    save_kind: Cell<Option<SaveKind>>,

    recent: RefCell<Vec<RecentNote>>,
    recent_error: RefCell<String>,
    recent_loaded: Cell<bool>,
    vault_config: RefCell<(String, String)>,
    controls: RefCell<Option<Controls>>,
    focus: Cell<Option<(FocusedEditor, NSRange)>>,

    // Speaker labels (U3/U4). Their requests never use `busy`, so Start, Stop
    // and Ask stay usable while a rename or merge runs.
    speaker_editor: RefCell<SpeakerEditor>,
    /// Typed name of an open rename; the merge target of an open merge.
    speaker_name_draft: RefCell<String>,
    merge_choice: RefCell<String>,
    /// Put the caret in the rename field on the next render.
    focus_speaker_name: Cell<bool>,
    /// Recent note path -> the meeting id in its frontmatter (diarized only).
    recent_ids: RefCell<HashMap<String, String>>,
    /// Meeting id -> its speakers, or `None` when the server has no speaker
    /// data (404). A failed request is not cached, so it is asked again.
    recent_speakers: RefCell<HashMap<String, Option<Vec<MeetingSpeaker>>>>,
    probe_in_flight: Cell<bool>,
    /// Meeting ids to read next (one probe runs at a time; an explicit
    /// refresh waits here and is always read, cached or not).
    probe_queue: RefCell<std::collections::BTreeSet<String>>,
    /// The ids of the probe in flight.
    probe_ids: RefCell<Vec<String>>,
    /// The server came back while a probe was in flight: that probe may
    /// have failed against the old connection, so once it finishes the
    /// recent ids still uncached are read again (once, not in a loop).
    probe_after_reconnect: Cell<bool>,
    /// Counts applied speaker edits. A probe or status read launched before
    /// the latest edit was applied may hold older speakers: its speakers are
    /// not used (a probe's ids are read again).
    speaker_seq: Cell<u64>,
    probe_seq: Cell<u64>,
    status_seq: Cell<u64>,
    /// The open edit is on the Recent card (else the saved-notes panel).
    edit_on_recent: Cell<bool>,
    /// Set by the render that drew the open edit's row.
    edit_drawn: Cell<bool>,
    /// The meeting whose speakers are open under its Recent row.
    open_recent: RefCell<Option<String>>,
}

impl State {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            client: RefCell::new(None),
            writer: notes_writer(),
            sender,
            receiver,
            timer: RefCell::new(None),
            rebuild: RefCell::new(None),
            visible: Cell::new(false),
            closed: Cell::new(false),
            generation: Cell::new(1),
            status: RefCell::new(None),
            status_in_flight: Cell::new(false),
            next_poll: Cell::new(None),
            templates: RefCell::new(Vec::new()),
            templates_requested: Cell::new(false),
            template_default: RefCell::new("general".into()),
            last_saved: RefCell::new(None),
            session_active: Cell::new(false),
            snapshot_mode: Cell::new(false),
            title_draft: RefCell::new(String::new()),
            title_edited: Cell::new(false),
            title_shows_session: Cell::new(false),
            template_value: RefCell::new(String::new()),
            notes_text: RefCell::new(String::new()),
            adopting_notes: Cell::new(false),
            question_draft: RefCell::new(String::new()),
            answers: RefCell::new(Vec::new()),
            next_answer: Cell::new(1),
            busy: Cell::new(false),
            enhancing: Cell::new(false),
            error: RefCell::new(String::new()),
            connection_message: RefCell::new(CONNECTING.into()),
            retry_visible: Cell::new(false),
            notes_notice: RefCell::new(String::new()),
            notes_dirty: Cell::new(false),
            notes_due: Cell::new(None),
            save_pending: Cell::new(false),
            stop_flush_pending: Cell::new(false),
            save_in_flight: Cell::new(false),
            save_kind: Cell::new(None),
            recent: RefCell::new(Vec::new()),
            recent_error: RefCell::new(String::new()),
            recent_loaded: Cell::new(false),
            vault_config: RefCell::new((String::new(), String::new())),
            controls: RefCell::new(None),
            focus: Cell::new(None),
            speaker_editor: RefCell::new(SpeakerEditor::default()),
            speaker_name_draft: RefCell::new(String::new()),
            merge_choice: RefCell::new(String::new()),
            focus_speaker_name: Cell::new(false),
            recent_ids: RefCell::new(HashMap::new()),
            recent_speakers: RefCell::new(HashMap::new()),
            probe_in_flight: Cell::new(false),
            probe_queue: RefCell::new(std::collections::BTreeSet::new()),
            probe_ids: RefCell::new(Vec::new()),
            probe_after_reconnect: Cell::new(false),
            speaker_seq: Cell::new(0),
            probe_seq: Cell::new(0),
            status_seq: Cell::new(0),
            edit_on_recent: Cell::new(false),
            edit_drawn: Cell::new(false),
            open_recent: RefCell::new(None),
        }
    }

    fn install_client(&self, client: Arc<dyn MeetingClient>) {
        *self.client.borrow_mut() = Some(client);
    }

    fn status_value(&self) -> Value {
        self.status.borrow().clone().unwrap_or_else(|| json!({}))
    }

    fn session(&self) -> Option<Value> {
        self.status
            .borrow()
            .as_ref()
            .and_then(|status| status.get("session"))
            .filter(|session| session.is_object())
            .cloned()
    }

    fn current_notes(&self) -> String {
        self.controls
            .borrow()
            .as_ref()
            .map(|controls| controls.notes.string().to_string())
            .unwrap_or_else(|| self.notes_text.borrow().clone())
    }

    fn capture(&self) {
        let controls_ref = self.controls.borrow();
        let Some(controls) = controls_ref.as_ref() else {
            return;
        };
        let speaker_editor = controls
            .speaker_name
            .as_ref()
            .and_then(|field| field.currentEditor());
        let focus = if let Some(editor) = controls.title.currentEditor() {
            Some((FocusedEditor::Title, editor.selectedRange()))
        } else if let Some(editor) = controls.question.currentEditor() {
            Some((FocusedEditor::Question, editor.selectedRange()))
        } else if let Some(editor) = speaker_editor {
            Some((FocusedEditor::SpeakerName, editor.selectedRange()))
        } else if controls
            .notes
            .window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| {
                std::ptr::eq(
                    &*responder as *const objc2_app_kit::NSResponder as *const (),
                    &*controls.notes as *const _ as *const (),
                )
            })
        {
            Some((FocusedEditor::Notes, controls.notes.selectedRange()))
        } else {
            None
        };
        self.focus.set(focus);
        *self.notes_text.borrow_mut() = controls.notes.string().to_string();
        *self.question_draft.borrow_mut() = controls.question.stringValue().to_string();
        if !self.title_shows_session.get() {
            *self.title_draft.borrow_mut() = controls.title.stringValue().to_string();
        }
        let selected = controls.template.indexOfSelectedItem();
        if selected >= 0 {
            if let Some(template) = self.templates.borrow().get(selected as usize) {
                *self.template_value.borrow_mut() = template.id.clone();
            }
        }
        if let Some(field) = controls.speaker_name.as_ref() {
            *self.speaker_name_draft.borrow_mut() = field.stringValue().to_string();
        }
        if let Some(merge) = controls.merge_target.as_ref() {
            // Matched by the selected item's own (unique) title, never by a
            // position that a collapsed or reordered menu could shift.
            *self.merge_choice.borrow_mut() =
                merge_choice_for_title(&merge.items, &merge.selected_title());
        }
    }

    fn restore_focus(&self) {
        let Some((focus, range)) = self.focus.take() else {
            return;
        };
        let controls_ref = self.controls.borrow();
        let Some(controls) = controls_ref.as_ref() else {
            return;
        };
        let clamp = |text: &str| {
            let count = text.encode_utf16().count();
            let location = range.location.min(count);
            NSRange::new(location, range.length.min(count - location))
        };
        match focus {
            FocusedEditor::Notes => {
                if let Some(window) = controls.notes.window() {
                    window.makeFirstResponder(Some(&controls.notes));
                    controls
                        .notes
                        .setSelectedRange(clamp(&controls.notes.string().to_string()));
                }
            }
            FocusedEditor::Title | FocusedEditor::Question | FocusedEditor::SpeakerName => {
                let field = match focus {
                    FocusedEditor::Title => &controls.title,
                    FocusedEditor::Question => &controls.question,
                    _ => match controls.speaker_name.as_ref() {
                        Some(field) => field,
                        None => return,
                    },
                };
                if let Some(window) = field.window() {
                    window.makeFirstResponder(Some(field));
                    if let Some(editor) = field.currentEditor() {
                        editor.setSelectedRange(clamp(&field.stringValue().to_string()));
                    }
                }
            }
        }
    }

    fn request_rebuild(&self) {
        if self.visible.get() && !self.closed.get() {
            if let Some(rebuild) = self.rebuild.borrow().as_ref() {
                rebuild();
            }
        }
    }

    fn set_notes_notice(&self, text: impl Into<String>) {
        let text = text.into();
        *self.notes_notice.borrow_mut() = text.clone();
        if let Some(controls) = self.controls.borrow().as_ref() {
            controls
                .notes_status
                .setStringValue(&NSString::from_str(&text));
            let token = if text.starts_with("Saved") {
                "success"
            } else {
                "muted"
            };
            controls
                .notes_status
                .setTextColor(Some(&color(token, &controls.palette)));
        }
    }

    fn next_generation(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        // An earlier status response is stale now. Let the new lifecycle start
        // its own request rather than leaving this flag stuck forever.
        self.status_in_flight.set(false);
        generation
    }

    fn launch(&self, kind: RequestKind, request: Request) {
        if self.closed.get() {
            return;
        }
        let client = self.client.borrow().clone();
        let Some(client) = client else {
            self.launch_failed(kind, "LocalFlow meeting client is unavailable.".into());
            return;
        };
        let generation = self.generation.get();
        let sender = self.sender.clone();
        let writer = self.writer.clone();
        let name = format!("localflow-meeting-{}", kind.name());
        // Register every durable write before spawning: notes (autosaves
        // included) and speaker renames or merges. The process-wide registry
        // lets application quit wait for writes from replaced Hub instances as
        // well as the currently displayed one.
        let pending_write = matches!(
            &request,
            Request::Notes { .. } | Request::SpeakerEdit { .. }
        )
        .then(crate::shutdown::register_pending_write);
        let spawned = thread::Builder::new().name(name).spawn(move || {
            let mut pending_write = pending_write;
            let event_kind = match request {
                Request::Templates => EventKind::Templates(client.templates()),
                Request::Status => EventKind::Status(client.status()),
                Request::Start {
                    title,
                    template,
                    notes,
                    auto_title,
                } => {
                    let result = client.start(&title, "", &template, &notes, Some(auto_title));
                    EventKind::Start { notes, result }
                }
                Request::Notes { kind, ticket, text } => {
                    let result = writer.save(&*client, ticket, &text);
                    EventKind::Notes { kind, text, result }
                }
                Request::Stop => EventKind::Stop(client.stop()),
                Request::Chat { id, question } => EventKind::Chat {
                    id,
                    result: client.chat(&question),
                },
                Request::SpeakerProbe { ids } => EventKind::SpeakerProbe(
                    ids.into_iter()
                        .map(|id| {
                            let result = client.speakers(&id);
                            (id, result)
                        })
                        .collect(),
                ),
                Request::SpeakerEdit {
                    token,
                    meeting_id,
                    change,
                } => {
                    let result = match &change {
                        SpeakerChange::Rename { speaker_id, name } => {
                            client.rename_speaker(&meeting_id, speaker_id, name)
                        }
                        SpeakerChange::Merge { from, into } => {
                            client.merge_speakers(&meeting_id, from, into)
                        }
                    };
                    // The durable write is done; the refetch below is only a
                    // read and must not hold quit back.
                    drop(pending_write.take());
                    // A refused edit may mean the data changed underneath
                    // (another window, the CLI): read it again once.
                    let refetch = result.is_err().then(|| client.speakers(&meeting_id));
                    EventKind::SpeakerEdit {
                        token,
                        meeting_id,
                        result,
                        refetch,
                    }
                }
            };
            // Closing the page drops the receiver; the worker then simply ends.
            let _ = sender.send(Event {
                generation,
                kind: event_kind,
            });
        });
        if let Err(error) = spawned {
            self.launch_failed(kind, format!("Couldn't run the meeting request: {error}"));
        }
    }

    fn launch_failed(&self, kind: RequestKind, message: String) {
        match kind {
            RequestKind::Templates => self.templates_requested.set(false),
            RequestKind::Status => {
                self.status_in_flight.set(false);
                *self.status.borrow_mut() = None;
                self.session_active.set(false);
                *self.connection_message.borrow_mut() = message;
                self.retry_visible.set(true);
            }
            RequestKind::Start | RequestKind::Stop => {
                self.busy.set(false);
                *self.error.borrow_mut() = message;
            }
            RequestKind::NotesAutosave => {
                self.save_in_flight.set(false);
                self.save_kind.set(None);
                self.notes_dirty.set(true);
                *self.error.borrow_mut() = message;
            }
            RequestKind::NotesStopFlush => {
                self.save_in_flight.set(false);
                self.save_kind.set(None);
                self.stop_flush_pending.set(false);
                self.save_pending.set(false);
                self.notes_dirty.set(true);
                self.busy.set(false);
                *self.error.borrow_mut() = format!("{CLOSE_SAVE_FAILED}{message}");
            }
            RequestKind::Chat => {
                self.busy.set(false);
                let id = self.next_answer.get().wrapping_sub(1);
                if let Some(answer) = self
                    .answers
                    .borrow_mut()
                    .iter_mut()
                    .find(|answer| answer.id == id)
                {
                    answer.answer = format!("Couldn’t answer — {message}");
                    answer.error = true;
                }
            }
            RequestKind::SpeakerProbe => {
                // Keep the ids for the next probe; a failed spawn caches nothing.
                self.probe_in_flight.set(false);
                let ids = std::mem::take(&mut *self.probe_ids.borrow_mut());
                self.probe_queue.borrow_mut().extend(ids);
            }
            RequestKind::SpeakerEdit => {
                let mut editor = self.speaker_editor.borrow_mut();
                let (token, meeting_id) = (editor.token, editor.meeting_id.clone());
                editor.finish(token, &meeting_id, Some(&message));
            }
        }
        self.request_rebuild();
    }

    fn poll(&self) {
        if self.closed.get() || self.status_in_flight.replace(true) {
            return;
        }
        self.status_seq.set(self.speaker_seq.get());
        self.next_poll.set(Some(Instant::now() + POLL_INTERVAL));
        self.launch(RequestKind::Status, Request::Status);
    }

    fn poll_if_due(&self, now: Instant) {
        if self.visible.get()
            && !self.snapshot_mode.get()
            && self.next_poll.get().is_none_or(|next| now >= next)
        {
            self.poll();
        }
    }

    fn load_templates(&self) {
        if self.closed.get() || self.templates_requested.replace(true) {
            return;
        }
        self.launch(RequestKind::Templates, Request::Templates);
    }

    fn primary_action(&self) {
        if self.busy.get() || self.closed.get() {
            return;
        }
        self.capture();
        if self.session().is_none() {
            let raw = self.title_draft.borrow().clone();
            let typed = raw.py_strip().to_string();
            let auto_title = typed.is_empty() || (!self.title_edited.get() && typed == "Meeting");
            let title = if typed.is_empty() {
                "Meeting".into()
            } else {
                typed
            };
            let template = self.template_value.borrow().clone();
            let notes = self.notes_text.borrow().clone();
            self.next_generation();
            self.snapshot_mode.set(false);
            self.busy.set(true);
            self.enhancing.set(false);
            self.error.borrow_mut().clear();
            self.launch(
                RequestKind::Start,
                Request::Start {
                    title,
                    template,
                    notes,
                    auto_title,
                },
            );
        } else {
            // A stop can supersede an autosave that is still in flight. The new
            // writer ticket makes the final text win even if that old worker
            // finishes later; its UI completion is rejected by the generation.
            self.next_generation();
            self.busy.set(true);
            self.enhancing.set(false);
            self.error.borrow_mut().clear();
            self.save_in_flight.set(false);
            self.save_kind.set(None);
            self.notes_due.set(None);
            self.request_notes_save(true);
        }
        self.request_rebuild();
    }

    fn retry(&self) {
        if self.closed.get() {
            return;
        }
        self.snapshot_mode.set(false);
        self.status_in_flight.set(false);
        *self.connection_message.borrow_mut() = CONNECTING.into();
        self.retry_visible.set(false);
        self.poll();
        self.request_rebuild();
    }

    fn notes_edited(&self, text: String) {
        self.notes_edited_at(text, Instant::now());
    }

    fn notes_edited_at(&self, text: String, now: Instant) {
        if self.closed.get() {
            return;
        }
        *self.notes_text.borrow_mut() = text;
        self.notes_dirty.set(true);
        self.set_notes_notice("Editing…");
        if self.session().is_some() {
            self.notes_due.set(Some(now + NOTES_DEBOUNCE));
        } else {
            self.notes_due.set(None);
            self.set_notes_notice("Kept for when you start");
        }
    }

    fn adopt_notes(&self, text: String) {
        *self.notes_text.borrow_mut() = text.clone();
        if let Some(controls) = self.controls.borrow().as_ref() {
            self.adopting_notes.set(true);
            controls.notes.setString(&NSString::from_str(&text));
            self.adopting_notes.set(false);
        }
    }

    fn request_notes_save(&self, stop_flush: bool) {
        if stop_flush {
            self.stop_flush_pending.set(true);
            self.save_pending.set(false);
            self.set_notes_notice("Saving…");
        } else {
            self.save_pending.set(true);
        }
        self.advance_notes_save();
    }

    fn advance_notes_save(&self) {
        if self.closed.get() || self.save_in_flight.get() {
            return;
        }
        let kind = if self.stop_flush_pending.replace(false) {
            SaveKind::StopFlush
        } else if self.save_pending.replace(false) {
            SaveKind::Autosave
        } else {
            return;
        };
        self.capture();
        let text = self.current_notes();
        *self.notes_text.borrow_mut() = text.clone();
        self.notes_dirty.set(true);
        self.save_in_flight.set(true);
        self.save_kind.set(Some(kind));
        self.set_notes_notice("Saving…");
        let ticket = self.writer.ticket();
        self.launch(
            match kind {
                SaveKind::Autosave => RequestKind::NotesAutosave,
                SaveKind::StopFlush => RequestKind::NotesStopFlush,
            },
            Request::Notes { kind, ticket, text },
        );
    }

    fn notes_completed(&self, kind: SaveKind, text: String, result: MeetingResult<Value>) {
        self.save_in_flight.set(false);
        self.save_kind.set(None);
        let current = self.current_notes();
        match result {
            Err(error) => {
                self.notes_dirty.set(true);
                if kind == SaveKind::StopFlush {
                    self.busy.set(false);
                    self.stop_flush_pending.set(false);
                    self.save_pending.set(false);
                    *self.error.borrow_mut() = format!("{CLOSE_SAVE_FAILED}{error}");
                    self.request_rebuild();
                    return;
                }
                *self.error.borrow_mut() = error.to_string();
                if self.stop_flush_pending.get() || self.save_pending.get() {
                    self.advance_notes_save();
                } else {
                    self.request_rebuild();
                }
            }
            Ok(_) => {
                self.error.borrow_mut().clear();
                self.notes_dirty.set(current != text);
                if kind == SaveKind::StopFlush {
                    if self.notes_dirty.get() {
                        self.save_pending.set(false);
                        self.stop_flush_pending.set(true);
                        self.advance_notes_save();
                        return;
                    }
                    self.enhancing.set(true);
                    self.launch(RequestKind::Stop, Request::Stop);
                    self.request_rebuild();
                    return;
                }
                if self.stop_flush_pending.get() || self.save_pending.get() {
                    self.advance_notes_save();
                } else if !self.notes_dirty.get() {
                    self.set_notes_notice(format!("Saved {}", Local::now().format("%H:%M")));
                }
            }
        }
    }

    fn title_edited(&self) {
        if !self.title_shows_session.get() {
            self.title_edited.set(true);
            self.capture();
        }
    }

    fn ask(&self) {
        if self.busy.get() || self.closed.get() || self.session().is_some() {
            return;
        }
        self.capture();
        let question = self.question_draft.borrow().py_strip().to_string();
        let available = self.status_value()["chat_available"]
            .as_bool()
            .unwrap_or(false)
            || self.last_saved.borrow().is_some();
        if question.is_empty() || !available {
            return;
        }
        let generation = self.generation.get();
        let id = self.next_answer.get();
        self.next_answer.set(id.wrapping_add(1));
        self.question_draft.borrow_mut().clear();
        if let Some(controls) = self.controls.borrow().as_ref() {
            controls.question.setStringValue(&NSString::from_str(""));
        }
        self.busy.set(true);
        self.answers.borrow_mut().insert(
            0,
            AnswerItem {
                id,
                question: question.clone(),
                answer: "Reading the transcript…".into(),
                error: false,
            },
        );
        // The generation is read before launch; a subsequent start, stop, seed
        // or shutdown invalidates this answer. The id also prevents an old chat
        // from updating a newer first row.
        debug_assert_eq!(generation, self.generation.get());
        self.launch(RequestKind::Chat, Request::Chat { id, question });
        self.request_rebuild();
    }

    fn reload_recent(&self) {
        let config = self.vault_config.borrow().clone();
        let (notes, error) = recent_notes(&config.0, &config.1, RECENT_NOTES_LIMIT, None);
        *self.recent.borrow_mut() = notes;
        *self.recent_error.borrow_mut() = error;
        self.recent_loaded.set(true);
        self.read_recent_ids();
        self.probe_recent();
    }

    /// Meeting ids from the recent notes' frontmatter. Only notes saved with
    /// speaker labels have one, so un-diarized notes never cause a request.
    fn read_recent_ids(&self) {
        let ids: HashMap<String, String> = self
            .recent
            .borrow()
            .iter()
            .filter_map(|note| {
                let id = note_meeting_id(std::path::Path::new(&note.path))?;
                Some((note.path.clone(), id))
            })
            .collect();
        *self.recent_ids.borrow_mut() = ids;
    }

    /// Ask the server, off the main thread, which recent meetings have speaker
    /// data. Answers are cached; while one probe runs a newer list waits.
    fn probe_recent(&self) {
        if self.closed.get() || self.snapshot_mode.get() {
            return;
        }
        let ids: Vec<String> = {
            let cache = self.recent_speakers.borrow();
            self.recent_ids
                .borrow()
                .values()
                .filter(|id| !cache.contains_key(*id))
                .cloned()
                .collect()
        };
        self.probe(ids);
    }

    /// The server answered again after a failed (or first) status read. A
    /// probe still in flight may fail against the old connection, so wait for
    /// it and read the uncached ids then; otherwise read them now.
    fn probe_recent_after_reconnect(&self) {
        if self.probe_in_flight.get() {
            self.probe_after_reconnect.set(true);
        } else {
            self.probe_recent();
        }
    }

    /// Queue `ids` to be read (cached or not) and start a probe unless one is
    /// running; the queue is sent, in id order, when that one finishes.
    fn probe(&self, ids: Vec<String>) {
        if self.closed.get() || self.snapshot_mode.get() {
            return;
        }
        self.probe_queue.borrow_mut().extend(ids);
        self.launch_probe();
    }

    fn launch_probe(&self) {
        if self.probe_in_flight.get() || self.probe_queue.borrow().is_empty() {
            return;
        }
        let ids: Vec<String> = std::mem::take(&mut *self.probe_queue.borrow_mut())
            .into_iter()
            .collect();
        self.probe_in_flight.set(true);
        self.probe_seq.set(self.speaker_seq.get());
        *self.probe_ids.borrow_mut() = ids.clone();
        self.launch(RequestKind::SpeakerProbe, Request::SpeakerProbe { ids });
    }

    /// Drop an open rename or merge (and its drafts) whose panel is no longer
    /// drawn, e.g. the saved panel hid when the next meeting started; it must
    /// not reappear later, or on the other surface, with an old draft.
    /// A running request is left to finish (its reply returns to the list).
    fn forget_undrawn_edit(&self, drawn: bool) {
        let stale = {
            let editor = self.speaker_editor.borrow();
            !drawn && !editor.busy && editor.mode != SpeakerMode::List
        };
        if stale {
            self.speaker_editor.borrow_mut().cancel();
            self.speaker_name_draft.borrow_mut().clear();
            self.merge_choice.borrow_mut().clear();
            self.focus_speaker_name.set(false);
        }
    }

    /// The meeting id shown by the saved-notes speaker panel, if any.
    fn saved_speaker_meeting(&self) -> Option<String> {
        if self.session().is_some() {
            return None;
        }
        self.last_saved.borrow().as_ref().and_then(saved_meeting_id)
    }

    /// Show or hide the speakers of a Recent meeting (its "Speakers…" row
    /// action). Opening reads them again so the list is current.
    fn toggle_recent_speakers(&self, meeting_id: &str) {
        if self.closed.get() || self.speaker_editor.borrow().busy {
            return;
        }
        self.capture();
        let open = self.open_recent.borrow().clone();
        if open.as_deref() == Some(meeting_id) {
            *self.open_recent.borrow_mut() = None;
            if self.speaker_editor.borrow().meeting_id == meeting_id {
                self.speaker_editor.borrow_mut().reset();
            }
        } else {
            *self.open_recent.borrow_mut() = Some(meeting_id.to_string());
            // Always a fresh list: an edit left open on the saved panel for
            // this meeting must not reopen here with its old draft.
            let mut editor = self.speaker_editor.borrow_mut();
            editor.reset();
            editor.open(meeting_id);
            drop(editor);
            self.speaker_name_draft.borrow_mut().clear();
            self.merge_choice.borrow_mut().clear();
            self.probe(vec![meeting_id.to_string()]);
        }
        self.request_rebuild();
    }

    /// The speakers of `meeting_id` as currently shown.
    fn speakers_of(&self, meeting_id: &str) -> Vec<MeetingSpeaker> {
        if self.saved_speaker_meeting().as_deref() == Some(meeting_id) {
            if let Some(saved) = self.last_saved.borrow().as_ref() {
                return meeting_speakers(saved);
            }
        }
        self.recent_speakers
            .borrow()
            .get(meeting_id)
            .cloned()
            .flatten()
            .unwrap_or_default()
    }

    /// `on_recent`: the row is on the Recent card (else the saved panel).
    fn begin_rename(&self, meeting_id: &str, speaker: &MeetingSpeaker, on_recent: bool) {
        self.capture();
        let started = self
            .speaker_editor
            .borrow_mut()
            .begin(meeting_id, SpeakerMode::Rename(speaker.id.clone()));
        if started {
            self.edit_on_recent.set(on_recent);
            *self.speaker_name_draft.borrow_mut() = speaker.label.clone();
            self.focus_speaker_name.set(true);
            self.request_rebuild();
        }
    }

    fn begin_merge(&self, meeting_id: &str, from: &str, on_recent: bool) {
        self.capture();
        let started = self
            .speaker_editor
            .borrow_mut()
            .begin(meeting_id, SpeakerMode::Merge(from.to_string()));
        if started {
            self.edit_on_recent.set(on_recent);
            // Preselect the first picker item, so the choice is always the
            // item shown as selected.
            let first = merge_target_items(&self.speakers_of(meeting_id), from)
                .into_iter()
                .next()
                .map(|item| item.id)
                .unwrap_or_default();
            *self.merge_choice.borrow_mut() = first;
            self.request_rebuild();
        }
    }

    /// The merge picker selected `title` (from its drawn `items`; `complete`
    /// when no title collapsed). Stores the exact id behind it (empty for
    /// the placeholder; commit still checks it again) and calls
    /// `set_enabled` so Merge is on exactly for a real speaker. Updated in
    /// place, never rebuilt, so a keyboard pick keeps focus on the picker.
    /// Returns `false` when the picker no longer matches the open merge:
    /// nothing is stored and the caller redraws ([`Self::redraw_merge_pick`]).
    fn pick_merge_target(
        &self,
        items: &[MergeTarget],
        complete: bool,
        title: &str,
        set_enabled: impl FnOnce(bool),
    ) -> bool {
        if self.closed.get() {
            return true;
        }
        let (meeting_id, from) = {
            let editor = self.speaker_editor.borrow();
            match &editor.mode {
                SpeakerMode::Merge(from) if !editor.busy => {
                    (editor.meeting_id.clone(), from.clone())
                }
                _ => return false,
            }
        };
        let speakers = self.speakers_of(&meeting_id);
        if merge_target_items(&speakers, &from) != items {
            return false;
        }
        let choice = merge_choice_for_title(items, title);
        let chosen = resolve_merge_target(&speakers, &from, &choice).is_some();
        *self.merge_choice.borrow_mut() = choice;
        set_enabled(complete && chosen);
        true
    }

    /// A pick on a picker that no longer matches the open merge: read it
    /// back and redraw (the previous behaviour for every pick).
    fn redraw_merge_pick(&self) {
        if self.closed.get() {
            return;
        }
        self.capture();
        self.request_rebuild();
    }

    fn cancel_speaker_edit(&self) {
        self.capture();
        self.speaker_editor.borrow_mut().cancel();
        self.request_rebuild();
    }

    /// Save the open rename or merge (Return in the field, Save or Merge).
    fn commit_speaker_edit(&self) {
        if self.closed.get() {
            return;
        }
        self.capture();
        let (meeting_id, mode) = {
            let editor = self.speaker_editor.borrow();
            (editor.meeting_id.clone(), editor.mode.clone())
        };
        let change = match mode {
            SpeakerMode::List => return,
            SpeakerMode::Rename(speaker_id) => SpeakerChange::Rename {
                speaker_id,
                name: self.speaker_name_draft.borrow().py_strip().to_string(),
            },
            SpeakerMode::Merge(from) => {
                let speakers = self.speakers_of(&meeting_id);
                let choice = self.merge_choice.borrow().clone();
                // Exactly the picked speaker, or nothing: merging is not undoable.
                let Some(into) = resolve_merge_target(&speakers, &from, &choice) else {
                    self.speaker_editor.borrow_mut().error = MERGE_PICK_AGAIN.into();
                    self.request_rebuild();
                    return;
                };
                SpeakerChange::Merge { from, into }
            }
        };
        let Some(token) = self.speaker_editor.borrow_mut().start() else {
            return;
        };
        self.launch(
            RequestKind::SpeakerEdit,
            Request::SpeakerEdit {
                token,
                meeting_id,
                change,
            },
        );
        self.request_rebuild();
    }

    /// New speakers for `meeting_id` (an edit reply or a fresh read):
    /// `None` means the meeting has no speaker data any more.
    fn store_speakers(&self, meeting_id: &str, payload: Option<&Value>) {
        let speakers = payload
            .map(meeting_speakers)
            .filter(|list| !list.is_empty());
        let is_saved = self
            .last_saved
            .borrow()
            .as_ref()
            .and_then(saved_meeting_id)
            .as_deref()
            == Some(meeting_id);
        if is_saved {
            let list = payload
                .and_then(|value| value.get("speakers"))
                .filter(|_| speakers.is_some())
                .cloned()
                .unwrap_or_else(|| json!([]));
            if let Some(saved) = self.last_saved.borrow_mut().as_mut() {
                saved["speakers"] = list.clone();
            }
            if let Some(status) = self.status.borrow_mut().as_mut() {
                if status["last_saved"]["meeting_id"].as_str() == Some(meeting_id) {
                    status["last_saved"]["speakers"] = list;
                }
            }
        }
        // Always cached, the saved meeting included: once the next meeting
        // starts the saved panel hides and its Recent row needs this answer.
        self.recent_speakers
            .borrow_mut()
            .insert(meeting_id.to_string(), speakers.clone());
        if speakers.is_none() {
            if self.open_recent.borrow().as_deref() == Some(meeting_id) {
                *self.open_recent.borrow_mut() = None;
            }
            if self.speaker_editor.borrow().meeting_id == meeting_id {
                self.speaker_editor.borrow_mut().reset();
            }
        }
    }

    fn apply_event(&self, event: Event) {
        if self.closed.get() {
            return;
        }
        // Template metadata is not tied to a particular recording lifecycle,
        // but a reseeded snapshot must stay isolated from every older worker.
        // Speaker data belongs to a saved meeting, not to the recording
        // lifecycle either: a Start or Stop while it loads keeps it. Edits are
        // also matched by the editor's token and meeting id.
        let is_templates = matches!(
            &event.kind,
            EventKind::Templates(_) | EventKind::SpeakerProbe(_) | EventKind::SpeakerEdit { .. }
        );
        if event.generation != self.generation.get() && (!is_templates || self.snapshot_mode.get())
        {
            if matches!(&event.kind, EventKind::SpeakerProbe(_)) {
                self.probe_in_flight.set(false);
                self.probe_ids.borrow_mut().clear();
                self.probe_after_reconnect.set(false);
            }
            return;
        }
        match event.kind {
            EventKind::Templates(result) => match result {
                Ok(result) => {
                    *self.templates.borrow_mut() = result["templates"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| {
                            Some(MeetingTemplate {
                                id: item.get("id")?.as_str()?.to_string(),
                                name: item
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            })
                        })
                        .collect();
                    *self.template_default.borrow_mut() =
                        result["default"].as_str().unwrap_or("general").to_string();
                    self.request_rebuild();
                }
                Err(_) => self.templates_requested.set(false),
            },
            EventKind::Status(result) => {
                self.status_in_flight.set(false);
                match result {
                    Err(error) => {
                        *self.status.borrow_mut() = None;
                        self.error.borrow_mut().clear();
                        self.session_active.set(false);
                        *self.connection_message.borrow_mut() = error.to_string();
                        self.retry_visible.set(true);
                    }
                    Ok(mut result) => {
                        // Read before the latest speaker edit was applied: keep
                        // the speakers that edit returned for the same meeting.
                        if self.status_seq.get() != self.speaker_seq.get() {
                            let local = self.last_saved.borrow().clone();
                            if let Some(local) = local {
                                let same = local.get("meeting_id").is_some_and(|id| {
                                    !id.is_null()
                                        && result["last_saved"].get("meeting_id") == Some(id)
                                });
                                if same {
                                    result["last_saved"]["speakers"] =
                                        local.get("speakers").cloned().unwrap_or(Value::Null);
                                }
                            }
                        }
                        if let Some(saved) = result.get("last_saved").filter(|v| !v.is_null()) {
                            *self.last_saved.borrow_mut() = Some(saved.clone());
                        }
                        let session = result.get("session").filter(|v| v.is_object());
                        if let Some(session) = session {
                            if !self.session_active.get() && !self.notes_dirty.get() {
                                self.adopt_notes(
                                    session["user_notes"].as_str().unwrap_or("").to_string(),
                                );
                            }
                            if let Some(template) = session["template"].as_str() {
                                let selected = self
                                    .templates
                                    .borrow()
                                    .iter()
                                    .position(|choice| choice.id == template);
                                if let Some(index) = selected {
                                    *self.template_value.borrow_mut() = template.to_string();
                                    if let Some(controls) = self.controls.borrow().as_ref() {
                                        controls.template.selectItemAtIndex(index as isize);
                                    }
                                }
                            }
                        }
                        self.session_active.set(session.is_some());
                        *self.status.borrow_mut() = Some(result);
                        // A connection message means the last read failed (or
                        // none succeeded yet): recent speaker reads may have
                        // failed with it, and nothing else asks again.
                        let reconnected = !self.connection_message.borrow().is_empty();
                        self.connection_message.borrow_mut().clear();
                        self.retry_visible.set(false);
                        if reconnected {
                            self.probe_recent_after_reconnect();
                        }
                    }
                }
                self.next_poll.set(Some(Instant::now() + POLL_INTERVAL));
                self.request_rebuild();
            }
            EventKind::Start { notes, result } => {
                self.busy.set(false);
                match result {
                    Err(error) => {
                        *self.error.borrow_mut() = error.to_string();
                        self.request_rebuild();
                    }
                    Ok(_) => {
                        *self.last_saved.borrow_mut() = None;
                        self.answers.borrow_mut().clear();
                        let current = self.current_notes();
                        if current == notes {
                            self.notes_dirty.set(false);
                            self.set_notes_notice(if current.is_empty() {
                                String::new()
                            } else {
                                "Saved".into()
                            });
                        } else {
                            // Typed while the start was loading: save it, and never adopt
                            // the notes a status read before that save reports.
                            self.notes_dirty.set(true);
                            self.session_active.set(true);
                            self.request_notes_save(false);
                        }
                        self.poll();
                    }
                }
            }
            EventKind::Stop(result) => {
                self.busy.set(false);
                self.enhancing.set(false);
                match result {
                    Err(error) => *self.error.borrow_mut() = error.to_string(),
                    Ok(result) => {
                        let previous = self.session().unwrap_or_else(|| json!({}));
                        let mut saved = result;
                        if previous["title_auto"].as_bool() != Some(true) {
                            if let Some(title) = previous["title"].as_str() {
                                if saved.get("title").is_none() {
                                    saved["title"] = Value::String(title.into());
                                }
                            }
                        }
                        let mut status = self.status_value();
                        status["session"] = Value::Null;
                        status["last_saved"] = saved.clone();
                        *self.status.borrow_mut() = Some(status);
                        *self.last_saved.borrow_mut() = Some(saved);
                        self.adopt_notes(String::new());
                        self.notes_dirty.set(false);
                        self.session_active.set(false);
                        self.notes_due.set(None);
                        self.set_notes_notice("");
                        self.reload_recent();
                    }
                }
                self.request_rebuild();
                self.poll();
            }
            EventKind::Notes { kind, text, result } => self.notes_completed(kind, text, result),
            EventKind::Chat { id, result } => {
                self.busy.set(false);
                let answer = match result {
                    Ok(result) => (result["answer"].as_str().unwrap_or("").to_string(), false),
                    Err(error) => (format!("Couldn’t answer — {error}"), true),
                };
                if let Some(item) = self
                    .answers
                    .borrow_mut()
                    .iter_mut()
                    .find(|answer| answer.id == id)
                {
                    item.answer = answer.0;
                    item.error = answer.1;
                }
                self.request_rebuild();
            }
            EventKind::SpeakerProbe(results) => {
                self.probe_in_flight.set(false);
                self.probe_ids.borrow_mut().clear();
                if self.probe_seq.get() != self.speaker_seq.get() {
                    // Launched before the latest edit was applied: these
                    // reads may predate it. Read every id again instead.
                    self.probe_queue
                        .borrow_mut()
                        .extend(results.into_iter().map(|(id, _)| id));
                } else {
                    for (meeting_id, result) in results {
                        match result {
                            Ok(payload) => self.store_speakers(&meeting_id, payload.as_ref()),
                            // Transient: not cached, asked again on the next load.
                            Err(error) => log::debug!("meeting speakers unavailable: {error}"),
                        }
                    }
                }
                if self.probe_after_reconnect.replace(false) {
                    // Queues only ids still uncached (a 404 stays cached).
                    self.probe_recent();
                }
                self.launch_probe();
                self.request_rebuild();
            }
            EventKind::SpeakerEdit {
                token,
                meeting_id,
                result,
                refetch,
            } => {
                let error = result.as_ref().err().map(|error| error.0.clone());
                // Any reply means the server may have changed this meeting:
                // reads already on their way are older than it.
                self.speaker_seq.set(self.speaker_seq.get().wrapping_add(1));
                if !self
                    .speaker_editor
                    .borrow_mut()
                    .finish(token, &meeting_id, error.as_deref())
                {
                    // The panel moved on (closed, reseeded, another edit), so
                    // this reply's order against newer data is unknown: read
                    // the meeting again rather than keep a stale cache.
                    self.probe(vec![meeting_id]);
                    return;
                }
                match (result, refetch) {
                    (Ok(payload), _) => self.store_speakers(&meeting_id, Some(&payload)),
                    (Err(_), Some(Ok(payload))) => {
                        self.store_speakers(&meeting_id, payload.as_ref())
                    }
                    (Err(_), _) => {}
                }
                self.request_rebuild();
            }
        }
    }

    fn tick(&self) {
        self.tick_at(Instant::now());
    }

    fn tick_at(&self, now: Instant) {
        if self.closed.get() {
            return;
        }
        while let Ok(event) = self.receiver.try_recv() {
            self.apply_event(event);
        }
        if self.notes_due.get().is_some_and(|due| now >= due) {
            self.notes_due.set(None);
            self.request_notes_save(false);
        }
        self.poll_if_due(now);
    }

    fn flush(&self) {
        if self.closed.get() {
            return;
        }
        self.capture();
        self.visible.set(false);
        if self.notes_due.take().is_some() {
            self.save_pending.set(true);
        }
        self.advance_notes_save();
    }

    fn shutdown(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.capture();
        self.visible.set(false);
        let stop = self.stop_flush_pending.get()
            || (self.save_in_flight.get() && self.save_kind.get() == Some(SaveKind::StopFlush));
        // An autosave already on a worker is not yet durable either. Close with
        // a newer final ticket even when no debounce or pending flag remains.
        let unsaved = stop
            || self.save_in_flight.get()
            || self.notes_due.get().is_some()
            || self.save_pending.get();
        self.notes_due.set(None);
        self.save_pending.set(false);
        self.stop_flush_pending.set(false);
        self.next_generation();
        self.next_poll.set(None);
        self.controls.borrow_mut().take();
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        if !unsaved {
            return;
        }
        let text = self.current_notes();
        let Some(client) = self.client.borrow().clone() else {
            log::warn!("meeting notes flush on close failed: no meeting client");
            return;
        };
        let ticket = self.writer.ticket();
        let writer = self.writer.clone();
        // Increment before spawn so an immediate bounded termination wait can
        // never observe zero while the final worker is still being created.
        let notes_write = crate::shutdown::register_pending_write();
        let spawned = thread::Builder::new()
            .name("localflow-meeting-close-flush".into())
            .spawn(move || {
                let _notes_write = notes_write;
                let result = writer.save(&*client, ticket, &text);
                match result {
                    Ok(_) if stop => {
                        if client.stop().is_err() {
                            log::warn!("meeting stop on close failed");
                        }
                    }
                    Ok(_) => {}
                    Err(_) => log::warn!("meeting notes flush on close failed"),
                }
            });
        if let Err(error) = spawned {
            log::warn!("could not start meeting close flush: {error}");
        }
    }
}

fn notes_writer() -> Arc<MeetingNotesWriter> {
    // Like Python's `_MEETING_NOTES_WRITER`: a reopened Hub's closing flush is
    // ordered after the previous Hub's writes as well.
    static WRITER: OnceLock<Arc<MeetingNotesWriter>> = OnceLock::new();
    WRITER
        .get_or_init(|| Arc::new(MeetingNotesWriter::new()))
        .clone()
}

/// The native Meetings page. Long-lived so every draft survives rebuilds.
pub struct MeetingsPage {
    state: Rc<State>,
}

impl Default for MeetingsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl MeetingsPage {
    pub fn new() -> Self {
        Self {
            state: Rc::new(State::new()),
        }
    }

    /// Seed a complete scenario before rendering. This is intentionally the one
    /// snapshot mutation hook; it suppresses automatic fixture polling so a
    /// later fake response cannot unexpectedly replace the requested scenario.
    pub fn seed_snapshot(&self, snapshot: MeetingsPageSnapshot) {
        let state = &self.state;
        state.capture();
        state.next_generation();
        state.controls.borrow_mut().take();
        *state.status.borrow_mut() = snapshot.status;
        state.session_active.set(state.session().is_some());
        *state.last_saved.borrow_mut() = snapshot.last_saved;
        *state.templates.borrow_mut() = snapshot.templates;
        state.templates_requested.set(true);
        *state.template_default.borrow_mut() = snapshot.template_default;
        *state.template_value.borrow_mut() = snapshot.template;
        *state.title_draft.borrow_mut() = snapshot.title;
        state.title_edited.set(snapshot.title_edited);
        state.title_shows_session.set(false);
        *state.notes_text.borrow_mut() = snapshot.notes;
        *state.question_draft.borrow_mut() = snapshot.question;
        *state.answers.borrow_mut() = snapshot
            .answers
            .into_iter()
            .map(|answer| AnswerItem {
                id: 0,
                question: answer.question,
                answer: answer.answer,
                error: answer.error,
            })
            .collect();
        *state.error.borrow_mut() = snapshot.error;
        *state.connection_message.borrow_mut() = snapshot.connection_message;
        state.retry_visible.set(snapshot.retry_visible);
        *state.notes_notice.borrow_mut() = snapshot.notes_notice;
        *state.recent.borrow_mut() = snapshot.recent;
        *state.recent_error.borrow_mut() = snapshot.recent_error;
        state.recent_loaded.set(true);
        state.busy.set(false);
        state.enhancing.set(false);
        state.status_in_flight.set(false);
        state.notes_dirty.set(false);
        state.notes_due.set(None);
        state.save_pending.set(false);
        state.stop_flush_pending.set(false);
        state.save_in_flight.set(false);
        state.save_kind.set(None);
        state.speaker_editor.borrow_mut().reset();
        state.speaker_name_draft.borrow_mut().clear();
        state.merge_choice.borrow_mut().clear();
        state.focus_speaker_name.set(false);
        state.recent_ids.borrow_mut().clear();
        state.recent_speakers.borrow_mut().clear();
        state.probe_in_flight.set(false);
        state.probe_queue.borrow_mut().clear();
        state.probe_ids.borrow_mut().clear();
        state.probe_after_reconnect.set(false);
        state.edit_on_recent.set(false);
        *state.open_recent.borrow_mut() = None;
        state.snapshot_mode.set(true);
        state.request_rebuild();
    }

    /// Read the current deterministic content. Live controls are captured first
    /// so a snapshot taken just before a rebuild sees the user's latest draft.
    pub fn snapshot(&self) -> MeetingsPageSnapshot {
        self.state.capture();
        MeetingsPageSnapshot {
            status: self.state.status.borrow().clone(),
            last_saved: self.state.last_saved.borrow().clone(),
            templates: self.state.templates.borrow().clone(),
            template_default: self.state.template_default.borrow().clone(),
            template: self.state.template_value.borrow().clone(),
            title: self.state.title_draft.borrow().clone(),
            title_edited: self.state.title_edited.get(),
            notes: self.state.notes_text.borrow().clone(),
            question: self.state.question_draft.borrow().clone(),
            answers: self
                .state
                .answers
                .borrow()
                .iter()
                .map(|answer| MeetingAnswer {
                    question: answer.question.clone(),
                    answer: answer.answer.clone(),
                    error: answer.error,
                })
                .collect(),
            error: self.state.error.borrow().clone(),
            connection_message: self.state.connection_message.borrow().clone(),
            retry_visible: self.state.retry_visible.get(),
            notes_notice: self.state.notes_notice.borrow().clone(),
            recent: self.state.recent.borrow().clone(),
            recent_error: self.state.recent_error.borrow().clone(),
        }
    }
}

fn frame(value: (i32, i32, i32, i32)) -> objc2_foundation::NSRect {
    rect(
        f64::from(value.0),
        f64::from(value.1),
        f64::from(value.2),
        f64::from(value.3),
    )
}

fn action_title(session: &Value, enhancing: bool) -> String {
    if enhancing {
        return "Enhancing your notes…".into();
    }
    let seconds = session["seconds"].as_u64().unwrap_or(0);
    let segments = session["segment_count"].as_u64().unwrap_or(0);
    format!(
        "Stop & save notes — {}m{:02}s · {} segments",
        seconds / 60,
        seconds % 60,
        segments
    )
}

fn transcript_text(session: &Value) -> String {
    let lines: Vec<String> = session["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|segment| {
            format!(
                "[{}] {}: {}",
                segment["stamp"].as_str().unwrap_or("00:00"),
                if segment["source"].as_str() == Some("me") {
                    "Me"
                } else {
                    "Others"
                },
                segment["text"].as_str().unwrap_or("")
            )
        })
        .collect();
    if lines.is_empty() {
        LISTENING.into()
    } else {
        lines.join("\n\n")
    }
}

fn idle_watch(status: &Value) -> &'static str {
    if status["watching"].as_bool().unwrap_or(false)
        && status["mic_busy"].as_bool().unwrap_or(false)
    {
        "mic in use"
    } else if status["watching"].as_bool().unwrap_or(false) {
        "watching"
    } else if status["watch_error"]
        .as_str()
        .is_some_and(|error| !error.is_empty())
    {
        "watcher off"
    } else {
        ""
    }
}

fn open_note(path: &str, vault_path: &str, state: &Rc<State>) {
    // Never hand an unchecked server-provided path to the OS opener.
    let Some(path) = vault_notes_path(path, vault_path) else {
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
    if !NSWorkspace::sharedWorkspace().openURL(&url) {
        *state.error.borrow_mut() = OPEN_FAILED.into();
        state.request_rebuild();
    }
}

fn add_divider(ctx: &PageCtx, parent: &NSView, y: f64, width: f64) {
    let divider = FlippedView::new(ctx.mtm, rect(12.0, y, width - 24.0, 1.0));
    divider.setWantsLayer(true);
    if let Some(layer) = divider.layer() {
        layer.setBackgroundColor(Some(&color("line", &ctx.palette).CGColor()));
    }
    hide_from_accessibility(&divider);
    parent.addSubview(&divider);
}

/// Speaker row geometry (points): Ghost row actions like Recent's "Open".
const SPEAKER_BUTTON_W: f64 = 84.0;
const SPEAKER_TIME_W: f64 = 72.0;
const SPEAKER_GAP: f64 = 8.0;
/// The Recent row action that opens a meeting's speakers.
const SPEAKERS_ACTION_W: f64 = 112.0;

/// The panel's inline error for `meeting_id` on one surface (`on_recent`),
/// if the edit opened there failed.
fn speaker_error(state: &State, meeting_id: &str, on_recent: bool) -> String {
    let editor = state.speaker_editor.borrow();
    if editor.meeting_id == meeting_id && state.edit_on_recent.get() == on_recent {
        editor.error.clone()
    } else {
        String::new()
    }
}

/// Height of `meeting_id`'s speaker panel at `width`; 0 without speakers.
fn speaker_panel_size(
    ctx: &PageCtx,
    state: &State,
    meeting_id: &str,
    rows: usize,
    width: f64,
    on_recent: bool,
) -> f64 {
    let error = speaker_error(state, meeting_id, on_recent);
    let inner = (width - 2.0 * f64::from(CARD_PADDING)).max(1.0);
    let lines = if error.is_empty() {
        0
    } else {
        wrap_text(&error, inner, &*ctx.measure, 12.0, Weight::Regular)
            .len()
            .max(1)
    };
    f64::from(speaker_panel_height(rows, lines))
}

/// The speaker being renamed or merged can vanish (merged elsewhere, then
/// read again). With no row left to draw its Cancel, every row action would
/// stay locked: drop such an edit (and its drafts) before laying out.
fn drop_orphaned_edit(
    state: &State,
    meeting_id: &str,
    speakers: &[MeetingSpeaker],
    on_recent: bool,
) {
    let orphaned = {
        let editor = state.speaker_editor.borrow();
        let edited = match &editor.mode {
            SpeakerMode::Rename(id) | SpeakerMode::Merge(id) => Some(id.as_str()),
            SpeakerMode::List => None,
        };
        editor.meeting_id == meeting_id
            && state.edit_on_recent.get() == on_recent
            && edited.is_some_and(|id| !speakers.iter().any(|speaker| speaker.id == id))
    };
    if orphaned {
        // Not drawn: cancelled unless a request is running (its reply
        // returns the editor to the list, which unlocks the rows).
        state.forget_undrawn_edit(false);
    }
}

type SpeakerControls = (Option<Retained<NSTextField>>, Option<MergeControl>);

/// Draw a saved meeting's speakers at `(x, y)` in `parent` (U3/U4): each row
/// is the label, talk time, and Rename / Merge… row actions. Rename turns the
/// row into a name field with Save / Cancel; Merge… into a picker of the other
/// speakers with Merge / Cancel, so a merge always takes a second, explicit
/// step. A failed edit's server detail is shown under the rows. `surface`
/// draws the panel as its own card (under the saved notes); inside the Recent
/// card it is plain rows. Returns the open field / picker for draft capture.
#[allow(clippy::too_many_arguments)]
fn speaker_panel(
    ctx: &PageCtx,
    state: &Rc<State>,
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    meeting_id: &str,
    speakers: &[MeetingSpeaker],
    surface: bool,
) -> SpeakerControls {
    drop_orphaned_edit(state, meeting_id, speakers, !surface);
    let height = speaker_panel_size(ctx, state, meeting_id, speakers.len(), width, !surface);
    if height <= 0.0 {
        return (None, None);
    }
    let mtm = ctx.mtm;
    let palette = &ctx.palette;
    let editor = state.speaker_editor.borrow().clone();
    // An edit belongs to the surface it was opened on: the same meeting on
    // the other surface shows its plain list.
    let here = editor.meeting_id == meeting_id && state.edit_on_recent.get() == !surface;
    if here && editor.mode != SpeakerMode::List {
        state.edit_drawn.set(true);
    }
    let list = SpeakerMode::List;
    let mode = if here { &editor.mode } else { &list };
    let busy = here && editor.busy;
    let locked = editor.busy || (here && editor.mode != SpeakerMode::List);
    let inset = f64::from(CARD_PADDING);
    let right = width - inset;
    let panel = FlippedView::new(mtm, rect(x, y, width, height));
    if surface {
        style_surface(&panel, palette, &SurfaceStyle::default());
    }
    panel.addSubview(&label(
        mtm,
        "Speakers",
        inset,
        f64::from(SPEAKER_PAD) + 4.0,
        (right - inset).max(1.0),
        LabelStyle {
            size: 13.0,
            height: 22.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        },
        palette,
    ));
    let action = |title: &str, accessibility: &str, frame: (f64, f64, f64), kind: ButtonKind| {
        let control = button(mtm, title, frame.0, frame.1, frame.2, 28.0, kind, palette);
        control.setAccessibilityLabel(Some(&NSString::from_str(accessibility)));
        panel.addSubview(&control);
        control
    };
    let commit = {
        let weak = Rc::downgrade(state);
        Rc::new(move || {
            if let Some(state) = weak.upgrade() {
                state.commit_speaker_edit();
            }
        })
    };
    let cancel = {
        let weak = Rc::downgrade(state);
        Rc::new(move || {
            if let Some(state) = weak.upgrade() {
                state.cancel_speaker_edit();
            }
        })
    };
    let mut name_field = None;
    let mut merge_target = None;
    // Unique per speaker and the same titles the merge picker shows, so two
    // rows with one label (and their actions) can be told apart.
    let titles = speaker_titles(speakers);
    for (index, (speaker, title)) in speakers.iter().zip(&titles).enumerate() {
        let row_y = f64::from(SPEAKER_PAD + SPEAKER_HEADER_H + index as i32 * SPEAKER_ROW_H);
        let cancel_x = right - SPEAKER_BUTTON_W;
        let commit_x = cancel_x - SPEAKER_GAP - SPEAKER_BUTTON_W;
        match mode {
            SpeakerMode::Rename(id) if *id == speaker.id => {
                let field_w = (commit_x - SPEAKER_GAP - inset).max(80.0);
                let name = field(
                    mtm,
                    &state.speaker_name_draft.borrow(),
                    inset,
                    row_y + 4.0,
                    field_w,
                    &format!("New name for {title}"),
                );
                name.setPlaceholderString(Some(&NSString::from_str(&speaker.label)));
                name.setEditable(!busy);
                name.setSelectable(true);
                name.setTextColor(Some(&color("ink", palette)));
                {
                    let commit = commit.clone();
                    ctx.target.on_action(&name, move || commit());
                }
                panel.addSubview(&name);
                let save = action(
                    if busy { "Saving…" } else { "Save" },
                    &format!("Save the new name for {title}"),
                    (commit_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Primary,
                );
                save.setEnabled(!busy);
                {
                    let commit = commit.clone();
                    ctx.target.on_action(&save, move || commit());
                }
                let close = action(
                    "Cancel",
                    "Cancel renaming",
                    (cancel_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Secondary,
                );
                close.setEnabled(!busy);
                {
                    let cancel = cancel.clone();
                    ctx.target.on_action(&close, move || cancel());
                }
                name_field = Some(name);
            }
            SpeakerMode::Merge(id) if *id == speaker.id => {
                // Unique titles: an NSPopUpButton keeps one item per title.
                let targets = merge_target_items(speakers, &speaker.id);
                let prompt = format!("Merge {title} into");
                let prompt_w = ((ctx.measure)(&prompt, 13.0, Weight::Regular) + 8.0)
                    .min(((commit_x - inset) / 2.0).max(40.0));
                panel.addSubview(&label(
                    mtm,
                    &prompt,
                    inset,
                    row_y + 10.0,
                    prompt_w,
                    LabelStyle {
                        size: 13.0,
                        height: 20.0,
                        ..LabelStyle::default()
                    },
                    palette,
                ));
                let picker_x = inset + prompt_w + SPEAKER_GAP;
                // A choice that no longer names a target (e.g. removed by a
                // refresh) selects an unmapped placeholder, never another
                // speaker; Merge stays off until a speaker is picked.
                let choice = state.merge_choice.borrow().clone();
                let layout = merge_picker(&targets, speakers, &choice);
                if !layout.chosen && !choice.is_empty() {
                    // Invalidated (e.g. by a refresh): drop it, so only a new
                    // explicit pick can commit.
                    state.merge_choice.borrow_mut().clear();
                }
                let picker = popup(
                    mtm,
                    &layout.titles,
                    layout.selected,
                    picker_x,
                    row_y + 4.0,
                    (commit_x - SPEAKER_GAP - picker_x).max(80.0),
                    &prompt,
                );
                picker.setEnabled(!busy);
                {
                    // A pick updates Merge in place (no rebuild, so focus
                    // stays here): enabled only for a real speaker.
                    let weak = Rc::downgrade(state);
                    ctx.target.on_action(&picker, move || {
                        let Some(state) = weak.upgrade() else {
                            return;
                        };
                        // The controls borrow ends before any redraw.
                        let handled = state
                            .controls
                            .borrow()
                            .as_ref()
                            .and_then(|controls| controls.merge_target.as_ref())
                            .is_some_and(|merge| {
                                state.pick_merge_target(
                                    &merge.items,
                                    merge.complete,
                                    &merge.selected_title(),
                                    |enabled| merge.button.setEnabled(enabled),
                                )
                            });
                        if !handled {
                            state.redraw_merge_pick();
                        }
                    });
                }
                panel.addSubview(&picker);
                // Every item must be its own speaker; otherwise refuse to merge.
                let complete = usize::try_from(picker.numberOfItems()) == Ok(layout.titles.len());
                let merge = action(
                    if busy { "Merging…" } else { "Merge" },
                    &format!("Merge {title} into the chosen speaker"),
                    (commit_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Primary,
                );
                merge.setEnabled(!busy && complete && layout.chosen);
                {
                    let commit = commit.clone();
                    ctx.target.on_action(&merge, move || commit());
                }
                let close = action(
                    "Cancel",
                    "Cancel merging",
                    (cancel_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Secondary,
                );
                close.setEnabled(!busy);
                {
                    let cancel = cancel.clone();
                    ctx.target.on_action(&close, move || cancel());
                }
                merge_target = Some(MergeControl {
                    picker,
                    items: targets,
                    button: merge,
                    complete,
                });
            }
            _ => {
                let merge_x = right - SPEAKER_BUTTON_W;
                let rename_x = merge_x - SPEAKER_GAP - SPEAKER_BUTTON_W;
                let time_x = rename_x - SPEAKER_GAP - SPEAKER_TIME_W;
                panel.addSubview(&label(
                    mtm,
                    title,
                    inset,
                    row_y + 10.0,
                    (time_x - SPEAKER_GAP - inset).max(40.0),
                    LabelStyle {
                        size: 13.0,
                        height: 20.0,
                        ..LabelStyle::default()
                    },
                    palette,
                ));
                let talk = speaker_talk_time(speaker.seconds);
                let time = label(
                    mtm,
                    &talk,
                    time_x,
                    row_y + 12.0,
                    SPEAKER_TIME_W,
                    LabelStyle {
                        size: 11.0,
                        color: "muted",
                        height: 18.0,
                        ..LabelStyle::default()
                    },
                    palette,
                );
                time.setAlignment(NSTextAlignment::Right);
                time.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                    "{title} spoke for {talk}"
                ))));
                panel.addSubview(&time);
                let rename = action(
                    "Rename",
                    &format!("Rename {title}"),
                    (rename_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Ghost,
                );
                rename.setEnabled(!locked);
                {
                    let weak = Rc::downgrade(state);
                    let meeting_id = meeting_id.to_string();
                    let speaker = speaker.clone();
                    ctx.target.on_action(&rename, move || {
                        if let Some(state) = weak.upgrade() {
                            state.begin_rename(&meeting_id, &speaker, !surface);
                        }
                    });
                }
                let merge = action(
                    "Merge…",
                    &format!("Merge {title} into another speaker"),
                    (merge_x, row_y + 6.0, SPEAKER_BUTTON_W),
                    ButtonKind::Ghost,
                );
                merge.setEnabled(!locked && speakers.len() > 1);
                {
                    let weak = Rc::downgrade(state);
                    let meeting_id = meeting_id.to_string();
                    let from = speaker.id.clone();
                    ctx.target.on_action(&merge, move || {
                        if let Some(state) = weak.upgrade() {
                            state.begin_merge(&meeting_id, &from, !surface);
                        }
                    });
                }
            }
        }
    }
    let error = speaker_error(state, meeting_id, !surface);
    if !error.is_empty() {
        let error_y =
            f64::from(SPEAKER_PAD + SPEAKER_HEADER_H + speakers.len() as i32 * SPEAKER_ROW_H);
        let lines = wrap_text(
            &error,
            (right - inset).max(1.0),
            &*ctx.measure,
            12.0,
            Weight::Regular,
        )
        .len()
        .max(1);
        panel.addSubview(&label(
            mtm,
            &error,
            inset,
            error_y + 2.0,
            (right - inset).max(1.0),
            LabelStyle {
                size: 12.0,
                color: "red",
                height: f64::from(lines as i32 * SPEAKER_ERROR_LINE_H),
                wrap: true,
                ..LabelStyle::default()
            },
            palette,
        ));
    }
    parent.addSubview(&panel);
    (name_field, merge_target)
}

impl HubPage for MeetingsPage {
    fn prepare_snapshot(
        &mut self,
        preparation: &str,
        root: &std::path::Path,
    ) -> anyhow::Result<()> {
        let templates = vec![
            MeetingTemplate {
                id: "general".into(),
                name: "General".into(),
            },
            MeetingTemplate {
                id: "standup".into(),
                name: "Standup".into(),
            },
        ];
        let mut recent = Vec::new();
        if preparation != "recent-empty" {
            for (stamp, title) in [
                ("2026-11-10 0930", "Design review"),
                ("2026-11-11 1100", "Hiring sync"),
                ("2026-11-12 1500", "Customer call: Acme"),
                ("2026-11-13 1000", "Weekly standup"),
                ("2026-11-14 1600", "Retro"),
                ("2026-11-15 1430", "Planning"),
            ] {
                let path = root.join(format!("{title}.md"));
                std::fs::write(&path, format!("# {title}\n\n## Summary\n- Decided.\n"))?;
                recent.push(RecentNote {
                    title: title.into(),
                    date_label: note_date_label(
                        chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H%M")?,
                        None,
                    ),
                    path: path.to_string_lossy().into_owned(),
                });
            }
            recent.reverse();
            recent.truncate(RECENT_NOTES_LIMIT);
        }
        let session = |system_audio: &str, system_audio_error: &str, auto: bool| {
            serde_json::json!({
                "session": {
                    "title": if auto { "Meeting" } else { "Planning" },
                    "title_auto": auto,
                    "template": "standup",
                    "user_notes": "Check launch date",
                    "system_audio": system_audio,
                    "system_audio_error": system_audio_error,
                    "seconds": 185,
                    "segment_count": 2,
                    "segments": [
                        {"stamp": "00:04", "source": "me", "text": if auto { "Can we lock the launch date?" } else { "I can take this" }},
                        {"stamp": "01:12", "source": "others", "text": if auto { "March 3 works for everyone" } else { "Sounds good" }},
                    ],
                },
                "last_saved": null,
                "chat_available": false,
            })
        };
        let mut snapshot = MeetingsPageSnapshot {
            status: Some(
                serde_json::json!({"session": null, "last_saved": null, "chat_available": false}),
            ),
            templates,
            template_default: "general".into(),
            template: "general".into(),
            recent,
            ..MeetingsPageSnapshot::default()
        };
        match preparation {
            "idle" | "recent-empty" => {}
            "running" => {
                snapshot.status = Some(session(
                    "error",
                    "ScreenCaptureKit: no display available — grant Screen Recording permission to this Python binary (None)",
                    false,
                ));
                snapshot.notes = "Check launch date".into();
                snapshot.template = "standup".into();
            }
            "running-no-system-audio" => {
                snapshot.status = Some(session(
                    "error",
                    "no system audio source found: set up BlackHole and set [meetings] system_audio_device",
                    false,
                ));
                snapshot.notes = "Check launch date".into();
                snapshot.template = "standup".into();
            }
            "running-auto" => {
                snapshot.status = Some(session("ok", "", true));
                snapshot.notes = "Check launch date".into();
                snapshot.template = "standup".into();
            }
            "saved" | "saved-generated" => {
                let title = if preparation == "saved" {
                    "Planning"
                } else {
                    "Launch date review"
                };
                let path = root.join(format!("{title}.md"));
                std::fs::write(&path, format!("# {title}\n\n## Summary\n- Decided.\n"))?;
                snapshot.template = "standup".into();
                snapshot.notes = "Check launch date".into();
                if preparation == "saved-generated" {
                    snapshot.recent.insert(
                        0,
                        RecentNote {
                            title: title.into(),
                            date_label: note_date_label(
                                chrono::NaiveDateTime::parse_from_str(
                                    "2026-11-16 1000",
                                    "%Y-%m-%d %H%M",
                                )?,
                                None,
                            ),
                            path: path.to_string_lossy().into_owned(),
                        },
                    );
                    snapshot.recent.truncate(RECENT_NOTES_LIMIT);
                }
                let saved = serde_json::json!({"title": title, "title_generated": preparation == "saved-generated", "enhanced": true, "notes_path": path});
                snapshot.last_saved = Some(saved.clone());
                snapshot.status = Some(serde_json::json!({
                    "session": null,
                    "last_saved": saved,
                    "chat_available": true,
                }));
            }
            other => anyhow::bail!("unknown Meetings snapshot preparation {other:?}"),
        }
        self.seed_snapshot(snapshot);
        Ok(())
    }

    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        self.state.capture();
    }

    fn flush(&mut self) {
        self.state.flush();
    }

    fn release_views(&mut self) {
        self.state.visible.set(false);
        self.state.controls.borrow_mut().take();
        self.state.rebuild.borrow_mut().take();
    }

    fn shutdown(&mut self) {
        self.state.shutdown();
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let state = &self.state;
        state.install_client(ctx.services.meeting_client.clone());
        *state.rebuild.borrow_mut() = Some(ctx.rebuild.clone());
        state.visible.set(true);
        let config = ctx.services.config();
        *state.vault_config.borrow_mut() = (
            config.field_str("vault_path").unwrap_or("").to_string(),
            config.field_str("notes_folder").unwrap_or("").to_string(),
        );
        if !state.recent_loaded.get() {
            state.reload_recent();
        }
        if state.timer.borrow().is_none() && !state.closed.get() {
            let weak: Weak<State> = Rc::downgrade(&self.state);
            *state.timer.borrow_mut() = Some(Timer::schedule(TICK_SECONDS, true, move || {
                if let Some(state) = weak.upgrade() {
                    state.tick();
                }
            }));
        }
        if !state.snapshot_mode.get() && !state.closed.get() {
            state.load_templates();
            state.poll_if_due(Instant::now());
        }

        let mtm = ctx.mtm;
        let palette = &ctx.palette;
        let width = ctx.width;
        let notes_height = meeting_notes_height(ctx.viewport_height as i32, top as i32);

        // Connecting and connection errors replace the whole form. Drafts stay
        // in State and reappear intact after a successful retry.
        let Some(status) = state.status.borrow().clone() else {
            let message = state.connection_message.borrow().clone();
            ctx.body.addSubview(&label(
                mtm,
                if message.is_empty() {
                    CONNECTING
                } else {
                    &message
                },
                0.0,
                top,
                width,
                LabelStyle {
                    size: 13.0,
                    color: "muted",
                    height: 20.0,
                    ..LabelStyle::default()
                },
                palette,
            ));
            if state.retry_visible.get() {
                let layout = meeting_form_layout(width as i32, top as i32, notes_height, 0);
                let retry = button(
                    mtm,
                    "Retry",
                    frame(layout.retry).origin.x,
                    frame(layout.retry).origin.y,
                    100.0,
                    36.0,
                    ButtonKind::Secondary,
                    palette,
                );
                retry.setAccessibilityLabel(Some(&NSString::from_str("Retry")));
                let weak = Rc::downgrade(&self.state);
                ctx.target.on_action(&retry, move || {
                    if let Some(state) = weak.upgrade() {
                        state.retry();
                    }
                });
                ctx.body.addSubview(&retry);
                state.controls.borrow_mut().take();
                return frame(layout.retry).origin.y + 36.0;
            }
            state.controls.borrow_mut().take();
            return top + 28.0;
        };

        let session = status.get("session").filter(|value| value.is_object());
        let error = state.error.borrow().clone();
        let error_height = if error.is_empty() {
            0
        } else {
            (wrap_text(
                &error,
                (width - 8.0).max(1.0),
                &*ctx.measure,
                12.0,
                Weight::Regular,
            )
            .len()
            .max(1) as i32)
                * 16
                + 2
        };
        let layout = meeting_form_layout(width as i32, top as i32, notes_height, error_height);

        ctx.body.addSubview(&label(
            mtm,
            MEETING_SUBTITLE,
            0.0,
            top,
            width,
            LabelStyle {
                size: 13.0,
                color: "muted",
                height: 20.0,
                ..LabelStyle::default()
            },
            palette,
        ));

        // Title and template share an input row. During capture the title shows
        // the session, not the next meeting's draft; the draft is kept aside.
        ctx.body.addSubview(&label(
            mtm,
            "Meeting title",
            0.0,
            frame(layout.title_label).origin.y,
            frame(layout.title_label).size.width,
            LabelStyle {
                size: 11.0,
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            palette,
        ));
        let shown_title = if let Some(session) = session {
            if session["title_auto"].as_bool().unwrap_or(false) {
                String::new()
            } else {
                session["title"].as_str().unwrap_or("Meeting").to_string()
            }
        } else {
            state.title_draft.borrow().clone()
        };
        let title = field(
            mtm,
            &shown_title,
            0.0,
            0.0,
            frame(layout.title_field).size.width,
            "Meeting title",
        );
        title.setPlaceholderString(Some(&NSString::from_str(MEETING_TITLE_PLACEHOLDER)));
        title.setEditable(session.is_none());
        title.setSelectable(true);
        title.setTextColor(Some(&color("ink", palette)));
        let title_box = input_box(
            mtm,
            &title,
            (
                0.0,
                frame(layout.title_field).origin.y,
                frame(layout.title_field).size.width,
                frame(layout.title_field).size.height,
            ),
            palette,
            session.is_some(),
        );
        ctx.body.addSubview(&title_box);
        state.title_shows_session.set(session.is_some());
        {
            let weak = Rc::downgrade(&self.state);
            ctx.target.on_field_change(&title, move || {
                if let Some(state) = weak.upgrade() {
                    state.title_edited();
                }
            });
        }

        ctx.body.addSubview(&label(
            mtm,
            "Notes template",
            frame(layout.template_label).origin.x,
            frame(layout.template_label).origin.y,
            frame(layout.template_label).size.width,
            LabelStyle {
                size: 11.0,
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            palette,
        ));
        let templates = state.templates.borrow().clone();
        let default = state.template_default.borrow().clone();
        let desired = {
            let selected = state.template_value.borrow().clone();
            if selected.is_empty() {
                default.clone()
            } else {
                selected
            }
        };
        let selected = templates
            .iter()
            .position(|template| template.id == desired)
            .or_else(|| templates.iter().position(|template| template.id == default))
            .unwrap_or(0);
        if let Some(template) = templates.get(selected) {
            *state.template_value.borrow_mut() = template.id.clone();
        }
        let names: Vec<String> = templates
            .iter()
            .map(|template| template.name.clone())
            .collect();
        let template = popup(
            mtm,
            &names,
            selected,
            0.0,
            0.0,
            frame(layout.template_field).size.width,
            "Notes template",
        );
        template.setEnabled(session.is_none());
        let template_box = input_box(
            mtm,
            &template,
            (
                frame(layout.template_field).origin.x,
                frame(layout.template_field).origin.y,
                frame(layout.template_field).size.width,
                frame(layout.template_field).size.height,
            ),
            palette,
            session.is_some(),
        );
        ctx.body.addSubview(&template_box);

        ctx.body.addSubview(&label(
            mtm,
            "Your notes",
            0.0,
            frame(layout.notes_label).origin.y,
            frame(layout.notes_label).size.width,
            LabelStyle {
                size: 13.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            palette,
        ));
        let notes_notice = state.notes_notice.borrow().clone();
        let notes_status = label(
            mtm,
            &notes_notice,
            frame(layout.notes_status).origin.x,
            frame(layout.notes_status).origin.y,
            frame(layout.notes_status).size.width,
            LabelStyle {
                size: 11.0,
                color: if notes_notice.starts_with("Saved") {
                    "success"
                } else {
                    "muted"
                },
                height: 18.0,
                ..LabelStyle::default()
            },
            palette,
        );
        notes_status.setAlignment(NSTextAlignment::Right);
        ctx.body.addSubview(&notes_status);

        let notes_frame = frame(layout.notes);
        let notes_card = FlippedView::new(mtm, notes_frame);
        style_surface(
            &notes_card,
            palette,
            &SurfaceStyle {
                border: "ink",
                width: Some(f64::from(CARD_BORDER_WIDTH)),
                ..SurfaceStyle::default()
            },
        );
        let notes_scroll = NSScrollView::initWithFrame(
            mtm.alloc::<NSScrollView>(),
            rect(
                4.0,
                4.0,
                (notes_frame.size.width - 8.0).max(1.0),
                (notes_frame.size.height - 8.0).max(1.0),
            ),
        );
        notes_scroll.setHasVerticalScroller(true);
        notes_scroll.setAutohidesScrollers(true);
        notes_scroll.setBorderType(NSBorderType::NoBorder);
        notes_scroll.setDrawsBackground(false);
        let content = notes_scroll.contentSize();
        let notes = PlaceholderTextView::new(mtm, rect(0.0, 0.0, content.width, content.height));
        notes.setMinSize(NSSize::new(0.0, content.height));
        notes.setMaxSize(NSSize::new(10_000_000.0, 10_000_000.0));
        notes.setVerticallyResizable(true);
        notes.setHorizontallyResizable(false);
        notes.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        if let Some(container) = unsafe { notes.textContainer() } {
            container.setWidthTracksTextView(true);
        }
        notes.setTextContainerInset(NSSize::new(6.0, 8.0));
        notes.setFont(Some(&font(13.0, Weight::Regular)));
        notes.setRichText(false);
        notes.setDrawsBackground(false);
        notes.setTextColor(Some(&color("ink", palette)));
        notes.setInsertionPointColor(Some(&color("ink", palette)));
        notes.setAccessibilityLabel(Some(&NSString::from_str("Your notes")));
        notes.set_placeholder(MEETING_NOTES_PLACEHOLDER, Some(color("muted", palette)));
        notes.setString(&NSString::from_str(&state.notes_text.borrow()));
        notes_scroll.setDocumentView(Some(&notes));
        notes_card.addSubview(&notes_scroll);
        ctx.body.addSubview(&notes_card);
        {
            let weak = Rc::downgrade(&self.state);
            ctx.target.on_text_change(&notes, move || {
                if let Some(state) = weak.upgrade() {
                    if state.adopting_notes.get() {
                        return;
                    }
                    let text = state
                        .controls
                        .borrow()
                        .as_ref()
                        .map(|controls| controls.notes.string().to_string());
                    if let Some(text) = text {
                        state.notes_edited(text);
                    }
                }
            });
        }

        if error_height > 0 {
            let error_frame = layout.error.expect("nonzero error has a frame");
            ctx.body.addSubview(&label(
                mtm,
                &error,
                0.0,
                frame(error_frame).origin.y,
                frame(error_frame).size.width,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: frame(error_frame).size.height,
                    wrap: true,
                    ..LabelStyle::default()
                },
                palette,
            ));
        }

        let action_text = if let Some(session) = session {
            action_title(session, state.enhancing.get())
        } else {
            "Start meeting transcription".into()
        };
        let action_width = ((ctx.measure)(&action_text, 13.0, Weight::Semibold) + 28.0)
            .clamp(140.0, width.max(140.0));
        let action = button(
            mtm,
            &action_text,
            0.0,
            f64::from(layout.action_y),
            action_width,
            36.0,
            ButtonKind::Primary,
            palette,
        );
        action.setAccessibilityLabel(Some(&NSString::from_str(&action_text)));
        action.setEnabled(!state.busy.get());
        {
            let weak = Rc::downgrade(&self.state);
            ctx.target.on_action(&action, move || {
                if let Some(state) = weak.upgrade() {
                    state.primary_action();
                }
            });
        }
        ctx.body.addSubview(&action);

        let watch = session.map_or_else(|| idle_watch(&status).to_string(), meeting_capture_status);
        let watch_y = f64::from(layout.action_y) + 40.0;
        if !watch.is_empty() {
            ctx.body.addSubview(&label(
                mtm,
                &watch,
                0.0,
                watch_y,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 20.0,
                    ..LabelStyle::default()
                },
                palette,
            ));
        }
        // U5: the speaker-label state on its own line under the capture
        // status; absent (`off`, older servers) the page is unchanged.
        // Wrapped in full (the server's reason is capped, never clipped).
        let speaker_line = session.and_then(meeting_speaker_status);
        let speaker_line_h = speaker_line.as_deref().map_or(0.0, |line| {
            speaker_status_height(
                wrap_text(line, width.max(1.0), &*ctx.measure, 12.0, Weight::Regular).len(),
            )
        });
        if let Some(line) = speaker_line.as_deref() {
            ctx.body.addSubview(&label(
                mtm,
                line,
                0.0,
                watch_y + 20.0,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: speaker_line_h,
                    wrap: true,
                    ..LabelStyle::default()
                },
                palette,
            ));
        }
        let live_y = watch_y + if watch.is_empty() { 0.0 } else { 26.0 } + speaker_line_h;
        if let Some(session) = session {
            let live = FlippedView::new(mtm, rect(0.0, live_y, width, 98.0));
            style_surface(&live, palette, &SurfaceStyle::default());
            live.addSubview(&label(
                mtm,
                &transcript_text(session),
                8.0,
                8.0,
                (width - 16.0).max(1.0),
                LabelStyle {
                    size: 11.0,
                    height: 82.0,
                    wrap: true,
                    ..LabelStyle::default()
                },
                palette,
            ));
            ctx.body.addSubview(&live);
        }

        let saved = if session.is_none() {
            state.last_saved.borrow().clone()
        } else {
            None
        };
        let saved_y = live_y + if session.is_some() { 108.0 } else { 8.0 };
        if let Some(saved) = saved.as_ref() {
            ctx.body.addSubview(&label(
                mtm,
                &saved_notes_message(saved),
                0.0,
                saved_y + 4.0,
                (width - 160.0).max(1.0),
                LabelStyle {
                    size: 13.0,
                    height: 22.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                palette,
            ));
            let open = button(
                mtm,
                "Open notes",
                width - 140.0,
                saved_y,
                140.0,
                36.0,
                ButtonKind::Secondary,
                palette,
            );
            open.setAccessibilityLabel(Some(&NSString::from_str("Open notes")));
            let weak = Rc::downgrade(&self.state);
            let path = saved["notes_path"].as_str().unwrap_or("").to_string();
            let vault = state.vault_config.borrow().0.clone();
            ctx.target.on_action(&open, move || {
                if let Some(state) = weak.upgrade() {
                    open_note(&path, &vault, &state);
                }
            });
            ctx.body.addSubview(&open);
        }

        // U3: the saved meeting's speakers, only when it was saved with
        // speaker labels (a valid meeting id and speakers in the payload).
        let mut speaker_name = None;
        let mut merge_target = None;
        state.edit_drawn.set(false);
        let saved_meeting = state.saved_speaker_meeting();
        let saved_panel_h = match (saved.as_ref(), saved_meeting.as_deref()) {
            (Some(saved), Some(meeting_id)) => {
                let speakers = meeting_speakers(saved);
                let (name, merge) = speaker_panel(
                    ctx,
                    &self.state,
                    &ctx.body,
                    0.0,
                    saved_y + 48.0,
                    width,
                    meeting_id,
                    &speakers,
                    true,
                );
                speaker_name = name;
                merge_target = merge;
                speaker_panel_size(ctx, state, meeting_id, speakers.len(), width, false)
            }
            _ => 0.0,
        };

        let chat_available = session.is_none()
            && (status["chat_available"].as_bool().unwrap_or(false) || saved.is_some());
        let chat_y = saved_y
            + if saved.is_some() { 48.0 } else { 0.0 }
            + if saved_panel_h > 0.0 {
                saved_panel_h + 12.0
            } else {
                0.0
            };
        let question = field(
            mtm,
            &state.question_draft.borrow(),
            0.0,
            chat_y + 26.0,
            (width - 120.0).max(1.0),
            "Ask about this meeting",
        );
        question.setPlaceholderString(Some(&NSString::from_str("Ask about this meeting")));
        if chat_available {
            ctx.body.addSubview(&label(
                mtm,
                "Ask the transcript",
                0.0,
                chat_y,
                400.0_f64.min(width),
                LabelStyle {
                    size: 13.0,
                    height: 22.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                palette,
            ));
            ctx.body.addSubview(&question);
            let ask = button(
                mtm,
                "Ask",
                width - 100.0,
                chat_y + 26.0,
                100.0,
                36.0,
                ButtonKind::Primary,
                palette,
            );
            ask.setAccessibilityLabel(Some(&NSString::from_str("Ask")));
            ask.setEnabled(!state.busy.get());
            let weak = Rc::downgrade(&self.state);
            let submit = {
                let weak = weak.clone();
                Rc::new(move || {
                    if let Some(state) = weak.upgrade() {
                        state.ask();
                    }
                })
            };
            ctx.target.on_action(&question, {
                let submit = submit.clone();
                move || submit()
            });
            ctx.target.on_action(&ask, move || submit());
            ctx.body.addSubview(&ask);

            let answers = state.answers.borrow().clone();
            if !answers.is_empty() {
                let height = (answers.len() as f64 * 44.0).max(64.0);
                let content = FlippedView::new(mtm, rect(0.0, 0.0, width, height));
                for (index, answer) in answers.iter().enumerate() {
                    let answer_label = label(
                        mtm,
                        &format!("You: {}\n{}", answer.question, answer.answer),
                        0.0,
                        index as f64 * 44.0,
                        (width - 20.0).max(1.0),
                        LabelStyle {
                            size: 11.0,
                            color: if answer.error { "red" } else { "ink" },
                            height: 42.0,
                            wrap: true,
                            ..LabelStyle::default()
                        },
                        palette,
                    );
                    answer_label.setMaximumNumberOfLines(2);
                    content.addSubview(&answer_label);
                }
                let answer_scroll = NSScrollView::initWithFrame(
                    mtm.alloc::<NSScrollView>(),
                    rect(0.0, chat_y + 68.0, width, 64.0),
                );
                answer_scroll.setHasVerticalScroller(true);
                answer_scroll.setAutohidesScrollers(true);
                answer_scroll.setDrawsBackground(false);
                answer_scroll
                    .setAccessibilityLabel(Some(&NSString::from_str("Transcript answers")));
                answer_scroll.setDocumentView(Some(&content));
                ctx.body.addSubview(&answer_scroll);
            }
        }

        let has_answers = !state.answers.borrow().is_empty();
        let recent_y = if chat_available {
            chat_y + if has_answers { 140.0 } else { 74.0 }
        } else {
            chat_y
        } + 8.0;
        ctx.body.addSubview(&label(
            mtm,
            "Recent meetings",
            0.0,
            recent_y,
            400.0_f64.min(width),
            LabelStyle {
                size: 13.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            palette,
        ));
        let recent = state.recent.borrow().clone();
        let recent_error = state.recent_error.borrow().clone();
        // U4: a row gets "Speakers…" only once the server said its meeting
        // has speaker data (and it isn't the saved meeting already shown
        // above). Missing metadata, a 404 or a pending probe: no action.
        let row_meetings: Vec<Option<String>> = {
            let ids = state.recent_ids.borrow();
            let cache = state.recent_speakers.borrow();
            recent
                .iter()
                .map(|note| {
                    let id = ids.get(&note.path)?;
                    let known = cache
                        .get(id)
                        .is_some_and(|speakers| speakers.as_ref().is_some_and(|s| !s.is_empty()));
                    (known && saved_meeting.as_deref() != Some(id.as_str())).then(|| id.clone())
                })
                .collect()
        };
        let any_speakers = row_meetings.iter().any(Option::is_some);
        let open_recent = state.open_recent.borrow().clone();
        let open_index = open_recent.as_deref().and_then(|open| {
            row_meetings
                .iter()
                .position(|id| id.as_deref() == Some(open))
        });
        let open_speakers = open_recent
            .as_deref()
            .filter(|_| open_index.is_some())
            .map(|id| state.speakers_of(id))
            .unwrap_or_default();
        if let (Some(id), Some(_)) = (open_recent.as_deref(), open_index) {
            // Before sizing the card, so a dropped edit's error takes no room.
            drop_orphaned_edit(state, id, &open_speakers, true);
        }
        let open_h = match (open_recent.as_deref(), open_index) {
            (Some(id), Some(_)) => {
                speaker_panel_size(ctx, state, id, open_speakers.len(), width, true)
            }
            _ => 0.0,
        };
        let recent_height = if recent.is_empty() {
            f64::from(RECENT_ROW_H)
        } else {
            recent.len() as f64 * f64::from(RECENT_ROW_H) + open_h
        };
        let recent_card = FlippedView::new(mtm, rect(0.0, recent_y + 28.0, width, recent_height));
        style_surface(&recent_card, palette, &SurfaceStyle::default());
        if recent.is_empty() {
            recent_card.addSubview(&label(
                mtm,
                if recent_error.is_empty() {
                    RECENT_EMPTY
                } else {
                    &recent_error
                },
                f64::from(CARD_PADDING),
                12.0,
                (width - 2.0 * f64::from(CARD_PADDING)).max(1.0),
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 20.0,
                    ..LabelStyle::default()
                },
                palette,
            ));
        } else {
            let date_width = 150.0;
            let open_width = 72.0;
            let speakers_busy = state.speaker_editor.borrow().busy;
            for (index, note) in recent.iter().enumerate() {
                let below_open = open_index.is_some_and(|open| index > open);
                let y =
                    index as f64 * f64::from(RECENT_ROW_H) + if below_open { open_h } else { 0.0 };
                let open_x = width - f64::from(CARD_PADDING) - open_width;
                let speakers_x = open_x - SPEAKER_GAP - SPEAKERS_ACTION_W;
                // Dates line up across rows; without any speaker action the
                // row is laid out exactly as before.
                let date_x = if any_speakers { speakers_x } else { open_x } - 12.0 - date_width;
                recent_card.addSubview(&label(
                    mtm,
                    &note.title,
                    f64::from(CARD_PADDING),
                    y + 12.0,
                    (date_x - f64::from(CARD_PADDING) - 12.0).max(20.0),
                    LabelStyle {
                        size: 13.0,
                        height: 20.0,
                        weight: Weight::Semibold,
                        ..LabelStyle::default()
                    },
                    palette,
                ));
                let date = label(
                    mtm,
                    &note.date_label,
                    date_x,
                    y + 14.0,
                    date_width,
                    LabelStyle {
                        size: 11.0,
                        color: "muted",
                        height: 18.0,
                        ..LabelStyle::default()
                    },
                    palette,
                );
                date.setAlignment(NSTextAlignment::Right);
                recent_card.addSubview(&date);
                let open = button(
                    mtm,
                    "Open",
                    open_x,
                    y + 8.0,
                    open_width,
                    28.0,
                    ButtonKind::Ghost,
                    palette,
                );
                open.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                    "Open notes: {}",
                    note.title
                ))));
                let weak = Rc::downgrade(&self.state);
                let path = note.path.clone();
                let vault = state.vault_config.borrow().0.clone();
                ctx.target.on_action(&open, move || {
                    if let Some(state) = weak.upgrade() {
                        open_note(&path, &vault, &state);
                    }
                });
                recent_card.addSubview(&open);
                let meeting_id = row_meetings.get(index).cloned().flatten();
                let is_open = meeting_id.is_some() && open_index == Some(index);
                if let Some(meeting_id) = meeting_id.as_ref() {
                    let title = if is_open {
                        "Hide speakers"
                    } else {
                        "Speakers…"
                    };
                    let speakers = button(
                        mtm,
                        title,
                        speakers_x,
                        y + 8.0,
                        SPEAKERS_ACTION_W,
                        28.0,
                        ButtonKind::Ghost,
                        palette,
                    );
                    speakers.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                        "{title}: {}",
                        note.title
                    ))));
                    speakers.setEnabled(!speakers_busy);
                    let weak = Rc::downgrade(&self.state);
                    let meeting_id = meeting_id.clone();
                    ctx.target.on_action(&speakers, move || {
                        if let Some(state) = weak.upgrade() {
                            state.toggle_recent_speakers(&meeting_id);
                        }
                    });
                    recent_card.addSubview(&speakers);
                }
                if index + 1 < recent.len() || is_open {
                    add_divider(ctx, &recent_card, y + f64::from(RECENT_ROW_H) - 1.0, width);
                }
                if let (true, Some(meeting_id)) = (is_open, meeting_id.as_deref()) {
                    let (name, merge) = speaker_panel(
                        ctx,
                        &self.state,
                        &recent_card,
                        0.0,
                        y + f64::from(RECENT_ROW_H),
                        width,
                        meeting_id,
                        &open_speakers,
                        false,
                    );
                    speaker_name = speaker_name.or(name);
                    merge_target = merge_target.or(merge);
                    if index + 1 < recent.len() {
                        add_divider(
                            ctx,
                            &recent_card,
                            y + f64::from(RECENT_ROW_H) + open_h - 1.0,
                            width,
                        );
                    }
                }
            }
        }
        ctx.body.addSubview(&recent_card);

        // An open edit whose row this render did not draw is dropped, so it
        // cannot come back later (or on the other surface) with old drafts.
        state.forget_undrawn_edit(state.edit_drawn.get());
        let focus_name = state.focus_speaker_name.replace(false) && speaker_name.is_some();
        *state.controls.borrow_mut() = Some(Controls {
            title,
            template,
            notes,
            notes_status,
            question,
            speaker_name,
            merge_target,
            palette: ctx.palette,
        });
        if focus_name {
            // A rename just opened: type straight into it, name selected.
            state.focus.set(Some((
                FocusedEditor::SpeakerName,
                NSRange::new(0, usize::MAX),
            )));
        }
        state.restore_focus();
        recent_y + 28.0 + recent_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::meeting::{FakeMeetingClient, MeetingCall};
    use crate::hub_model::{MERGE_PLACEHOLDER, RECENT_NOTES_LIMIT};
    use std::sync::{Condvar, Mutex};

    #[test]
    fn status_rebuilds_do_not_poll_again_until_the_python_interval() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(2));
        let client = Arc::new(FakeMeetingClient::new());
        let page = page(client.clone(), idle());
        page.state.snapshot_mode.set(false);
        page.state.visible.set(true);
        page.state.poll_if_due(Instant::now());
        assert!(page.state.status_in_flight.get());
        page.state.apply_event(receive(&page));
        let due = page.state.next_poll.get().unwrap();
        for _ in 0..10 {
            page.state.poll_if_due(due - Duration::from_millis(1));
        }
        assert_eq!(
            client
                .calls()
                .iter()
                .filter(|call| matches!(call, MeetingCall::Status))
                .count(),
            1
        );
        page.state.poll_if_due(due);
        page.state.apply_event(receive(&page));
        assert_eq!(
            client
                .calls()
                .iter()
                .filter(|call| matches!(call, MeetingCall::Status))
                .count(),
            2
        );
        page.state.visible.set(false);
        page.state.poll_if_due(due + POLL_INTERVAL * 2);
        assert!(!page.state.status_in_flight.get());
        page.state.shutdown();
    }

    #[test]
    fn snapshots_match_python_meeting_templates_transcripts_and_saved_notes() {
        let root = tempfile::tempdir().unwrap();
        let mut page = MeetingsPage::new();
        for preparation in ["running", "running-no-system-audio", "running-auto"] {
            page.prepare_snapshot(preparation, root.path()).unwrap();
            let snapshot = page.snapshot();
            assert_eq!(snapshot.template, "standup");
            let status = snapshot.status.unwrap();
            let expected = if preparation == "running-auto" {
                "Can we lock the launch date?"
            } else {
                "I can take this"
            };
            assert_eq!(status["session"]["segments"][0]["text"], expected);
            assert_eq!(snapshot.recent.len(), RECENT_NOTES_LIMIT);
        }
        for preparation in ["saved", "saved-generated"] {
            page.prepare_snapshot(preparation, root.path()).unwrap();
            let snapshot = page.snapshot();
            assert_eq!(snapshot.notes, "Check launch date");
            let saved = snapshot.last_saved.unwrap();
            assert!(std::path::Path::new(saved["notes_path"].as_str().unwrap()).is_file());
            assert!(snapshot.answers.is_empty());
            if preparation == "saved-generated" {
                assert_eq!(snapshot.recent[0].title, "Launch date review");
            }
        }
        page.prepare_snapshot("recent-empty", root.path()).unwrap();
        assert!(page.snapshot().recent.is_empty());
        assert!(page.prepare_snapshot("unknown", root.path()).is_err());
    }

    struct ObservedChatClient {
        inner: FakeMeetingClient,
        chat_thread: Mutex<Option<thread::ThreadId>>,
    }

    struct BlockingFirstSaveClient {
        inner: FakeMeetingClient,
        save_calls: Mutex<usize>,
        first_save_started: mpsc::Sender<()>,
        release_first: Arc<(Mutex<bool>, Condvar)>,
    }

    impl MeetingClient for BlockingFirstSaveClient {
        fn status(&self) -> MeetingResult<Value> {
            self.inner.status()
        }

        fn templates(&self) -> MeetingResult<Value> {
            self.inner.templates()
        }

        fn start(
            &self,
            title: &str,
            category: &str,
            template: &str,
            user_notes: &str,
            auto_title: Option<bool>,
        ) -> MeetingResult<Value> {
            self.inner
                .start(title, category, template, user_notes, auto_title)
        }

        fn save_notes(&self, notes: &str) -> MeetingResult<Value> {
            let call = {
                let mut calls = self.save_calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            if call == 1 {
                self.first_save_started.send(()).unwrap();
                let (released, idle) = &*self.release_first;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = idle.wait(released).unwrap();
                }
            }
            self.inner.save_notes(notes)
        }

        fn stop(&self) -> MeetingResult<Value> {
            self.inner.stop()
        }

        fn chat(&self, question: &str) -> MeetingResult<Value> {
            self.inner.chat(question)
        }

        fn dismiss(&self) -> MeetingResult<()> {
            self.inner.dismiss()
        }
    }

    impl MeetingClient for ObservedChatClient {
        fn status(&self) -> MeetingResult<Value> {
            self.inner.status()
        }

        fn templates(&self) -> MeetingResult<Value> {
            self.inner.templates()
        }

        fn start(
            &self,
            title: &str,
            category: &str,
            template: &str,
            user_notes: &str,
            auto_title: Option<bool>,
        ) -> MeetingResult<Value> {
            self.inner
                .start(title, category, template, user_notes, auto_title)
        }

        fn save_notes(&self, notes: &str) -> MeetingResult<Value> {
            self.inner.save_notes(notes)
        }

        fn stop(&self) -> MeetingResult<Value> {
            self.inner.stop()
        }

        fn chat(&self, question: &str) -> MeetingResult<Value> {
            *self.chat_thread.lock().unwrap() = Some(thread::current().id());
            self.inner.chat(question)
        }

        fn dismiss(&self) -> MeetingResult<()> {
            self.inner.dismiss()
        }
    }

    fn idle() -> MeetingsPageSnapshot {
        MeetingsPageSnapshot {
            status: Some(json!({"session": null, "chat_available": false})),
            connection_message: String::new(),
            ..MeetingsPageSnapshot::default()
        }
    }

    fn active(seconds: u64) -> MeetingsPageSnapshot {
        MeetingsPageSnapshot {
            status: Some(json!({
                "session": {
                    "title": "Planning",
                    "title_auto": false,
                    "template": "general",
                    "user_notes": "agenda",
                    "seconds": seconds,
                    "segment_count": 2,
                    "segments": [],
                    "system_audio": "off"
                },
                "chat_available": false
            })),
            notes: "agenda".into(),
            template: "general".into(),
            title: "Next draft".into(),
            connection_message: String::new(),
            ..MeetingsPageSnapshot::default()
        }
    }

    fn page(client: Arc<dyn MeetingClient>, snapshot: MeetingsPageSnapshot) -> MeetingsPage {
        let mut page = MeetingsPage::new();
        Rc::get_mut(&mut page.state).unwrap().writer = Arc::new(MeetingNotesWriter::new());
        page.state.install_client(client);
        page.seed_snapshot(snapshot);
        page
    }

    fn receive(page: &MeetingsPage) -> Event {
        page.state
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("worker did not deliver an event")
    }

    #[test]
    fn contract_empty_active_completed_error_and_populated_snapshots() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = MeetingsPage::new();
        assert_eq!(page.name(), "Meetings");
        assert_eq!(page.snapshot(), MeetingsPageSnapshot::default());

        page.seed_snapshot(active(65));
        assert_eq!(page.snapshot().status.unwrap()["session"]["seconds"], 65);
        assert_eq!(
            action_title(&active(65).status.unwrap()["session"], false),
            "Stop & save notes — 1m05s · 2 segments"
        );

        page.seed_snapshot(MeetingsPageSnapshot {
            last_saved: Some(json!({"title": "Planning", "notes_path": "/v/Planning.md"})),
            answers: vec![MeetingAnswer {
                question: "Decisions?".into(),
                answer: "Ship Friday.".into(),
                error: false,
            }],
            recent: (0..RECENT_NOTES_LIMIT)
                .map(|index| RecentNote {
                    title: format!("Meeting {index}"),
                    date_label: "Today, 10:00".into(),
                    path: format!("/v/{index}.md"),
                })
                .collect(),
            ..idle()
        });
        let snapshot = page.snapshot();
        assert_eq!(snapshot.answers.len(), 1);
        assert_eq!(snapshot.recent.len(), RECENT_NOTES_LIMIT);
        assert_eq!(snapshot.last_saved.unwrap()["title"], "Planning");

        page.seed_snapshot(MeetingsPageSnapshot {
            error: "start refused".into(),
            title: "Keep this draft".into(),
            notes: "Keep these notes".into(),
            ..idle()
        });
        let snapshot = page.snapshot();
        assert_eq!(snapshot.error, "start refused");
        assert_eq!(snapshot.title, "Keep this draft");
        assert_eq!(snapshot.notes, "Keep these notes");
        drop(client);
    }

    #[test]
    fn start_uses_typed_title_template_and_notes_then_adopts_real_status() {
        let client = Arc::new(FakeMeetingClient::new());
        let mut fixture = idle();
        fixture.title = "  Planning  ".into();
        fixture.title_edited = true;
        fixture.template = "general".into();
        fixture.notes = "agenda".into();
        let page = page(client.clone(), fixture);
        page.state.primary_action();
        let event = receive(&page);
        assert!(matches!(event.kind, EventKind::Start { result: Ok(_), .. }));
        let calls = client.calls();
        assert!(
            matches!(calls.as_slice(), [MeetingCall::Start { title, template, user_notes, auto_title: Some(false), .. }] if title == "Planning" && template == "general" && user_notes == "agenda"),
            "unexpected meeting calls: {calls:?}"
        );
        page.state.apply_event(event);

        let status = receive(&page);
        page.state.apply_event(status);
        assert!(page.state.session().is_some());
        assert!(!page.state.busy.get());
        assert_eq!(page.snapshot().title, "  Planning  ");
        assert_eq!(page.snapshot().notes, "agenda");
        page.state.shutdown();
    }

    #[test]
    fn notes_typed_while_start_is_in_flight_are_saved_not_overwritten() {
        let client = Arc::new(FakeMeetingClient::new());
        let mut fixture = idle();
        fixture.notes = "agenda".into();
        let page = page(client.clone(), fixture);
        page.state.primary_action();
        let start = receive(&page);
        assert!(
            matches!(&start.kind, EventKind::Start { notes, result: Ok(_) } if notes == "agenda")
        );

        // A status read from before the notes save, delivered after it below.
        let stale = client.status().unwrap();
        assert_eq!(stale["session"]["user_notes"], "agenda");

        let typed = "agenda\n- typed while starting";
        page.state.notes_edited_at(typed.into(), Instant::now());
        page.state.apply_event(start);
        assert!(page.state.notes_dirty.get());

        let (first, second) = (receive(&page), receive(&page));
        let (saved, status) = if matches!(first.kind, EventKind::Notes { .. }) {
            (first, second)
        } else {
            (second, first)
        };
        assert!(matches!(saved.kind, EventKind::Notes { result: Ok(_), .. }));
        page.state.apply_event(saved);
        assert!(!page.state.notes_dirty.get());
        page.state.apply_event(Event {
            generation: page.state.generation.get(),
            kind: EventKind::Status(Ok(stale)),
        });
        assert_eq!(page.snapshot().notes, typed);
        page.state.apply_event(status);

        assert_eq!(client.status().unwrap()["session"]["user_notes"], typed);
        assert_eq!(page.snapshot().notes, typed);
        assert!(!page.state.notes_dirty.get());
        assert!(page.state.session().is_some());
        page.state.shutdown();
    }

    #[test]
    fn notes_debounce_and_stop_flush_save_before_stopping() {
        let client = Arc::new(FakeMeetingClient::new());
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(31));
        let now = Instant::now();
        page.state.notes_edited_at("agenda\n- final".into(), now);
        page.state.tick_at(now + Duration::from_millis(799));
        assert!(matches!(
            page.state.receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        page.state.tick_at(now + NOTES_DEBOUNCE);
        let autosave = receive(&page);
        page.state.apply_event(autosave);
        assert_eq!(
            client.status().unwrap()["session"]["user_notes"],
            "agenda\n- final"
        );
        assert!(!page.state.notes_dirty.get());
        assert!(page.state.notes_notice.borrow().starts_with("Saved "));

        page.state.primary_action();
        let flush = receive(&page);
        assert!(matches!(
            flush.kind,
            EventKind::Notes {
                kind: SaveKind::StopFlush,
                ..
            }
        ));
        page.state.apply_event(flush);
        assert!(page.state.enhancing.get());
        let stop = receive(&page);
        assert!(matches!(stop.kind, EventKind::Stop(Ok(_))));
        page.state.apply_event(stop);
        let snapshot = page.snapshot();
        assert!(snapshot.notes.is_empty());
        assert!(snapshot.status.unwrap()["session"].is_null());
        assert_eq!(snapshot.last_saved.unwrap()["title"], "Planning");
        let calls = client.calls();
        let last_save = calls
            .iter()
            .rposition(|call| matches!(call, MeetingCall::SaveNotes(_)))
            .unwrap();
        let stop_at = calls
            .iter()
            .rposition(|call| matches!(call, MeetingCall::Stop))
            .unwrap();
        assert!(last_save < stop_at);
        page.state.shutdown();
    }

    #[test]
    fn failed_stop_flush_keeps_notes_and_leaves_meeting_running() {
        let client = Arc::new(FakeMeetingClient::new());
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(1));
        client.set_down(Some("offline"));
        page.state.primary_action();
        let flush = receive(&page);
        page.state.apply_event(flush);
        assert!(page.snapshot().error.starts_with(CLOSE_SAVE_FAILED));
        assert_eq!(page.snapshot().notes, "agenda");
        assert!(page.state.session().is_some());
        assert!(!page.state.busy.get());
        assert!(!matches!(client.calls().last(), Some(MeetingCall::Stop)));
        page.state.shutdown();
    }

    #[test]
    fn chat_runs_off_main_and_stale_generation_cannot_apply() {
        let client = Arc::new(ObservedChatClient {
            inner: FakeMeetingClient::new(),
            chat_thread: Mutex::new(None),
        });
        client
            .inner
            .set_last_saved(Some(json!({"title": "Planning"})));
        client.inner.set_chat_answer(Some("Decision: ship"));
        let mut fixture = idle();
        fixture.status = Some(client.status().unwrap());
        fixture.last_saved = Some(json!({"title": "Planning"}));
        let page = MeetingsPage::new();
        page.state.install_client(client.clone());
        page.seed_snapshot(fixture);
        page.state.question_draft.replace(" Decision? ".into());
        page.state.ask();
        assert_eq!(page.snapshot().answers[0].answer, "Reading the transcript…");
        let answer = receive(&page);
        assert_ne!(
            client.chat_thread.lock().unwrap().unwrap(),
            thread::current().id()
        );
        page.seed_snapshot(idle());
        page.state.apply_event(answer);
        assert!(page.snapshot().answers.is_empty());
        assert!(!page.state.busy.get());

        let mut fixture = idle();
        fixture.status = Some(json!({"session": null, "chat_available": true}));
        page.seed_snapshot(fixture);
        page.state.question_draft.replace("Decision?".into());
        page.state.ask();
        let current = receive(&page);
        page.state.apply_event(current);
        let answers = page.snapshot().answers;
        assert_eq!(answers[0].answer, "Decision: ship");
        assert!(!answers[0].error);

        client.inner.set_down(Some("no transcript"));
        page.state.question_draft.replace("Decision?".into());
        page.state.ask();
        let failed = receive(&page);
        page.state.apply_event(failed);
        let answers = page.snapshot().answers;
        assert_eq!(answers[0].answer, "Couldn’t answer — no transcript");
        assert!(answers[0].error);
        page.state.shutdown();
    }

    #[test]
    fn connection_failure_keeps_drafts_and_retry_recovers() {
        let client = Arc::new(FakeMeetingClient::new());
        let mut fixture = idle();
        fixture.title = "Draft title".into();
        fixture.notes = "Draft notes".into();
        fixture.question = "Unsent question".into();
        let page = page(client, fixture);
        let generation = page.state.generation.get();
        page.state.apply_event(Event {
            generation,
            kind: EventKind::Status(Err(crate::backends::meeting::MeetingClientError(
                "server down".into(),
            ))),
        });
        let failed = page.snapshot();
        assert!(failed.status.is_none());
        assert!(failed.retry_visible);
        assert_eq!(failed.connection_message, "server down");
        assert_eq!(failed.title, "Draft title");
        assert_eq!(failed.notes, "Draft notes");
        assert_eq!(failed.question, "Unsent question");

        page.state.retry();
        let recovered = receive(&page);
        page.state.apply_event(recovered);
        let recovered = page.snapshot();
        assert!(recovered.status.is_some());
        assert!(!recovered.retry_visible);
        assert_eq!(recovered.title, "Draft title");
        assert_eq!(recovered.notes, "Draft notes");
        page.state.shutdown();
    }

    #[test]
    fn closing_during_stop_flushes_notes_then_stops_once() {
        let client = Arc::new(FakeMeetingClient::new());
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(14));
        page.state
            .notes_edited_at("final close text".into(), Instant::now());
        page.state.primary_action();
        // The worker has sent its notes result, but AppKit has not delivered it.
        let queued = receive(&page);
        assert!(matches!(
            queued.kind,
            EventKind::Notes {
                kind: SaveKind::StopFlush,
                ..
            }
        ));
        page.state.shutdown();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if client.status().unwrap()["session"].is_null() {
                break;
            }
            assert!(Instant::now() < deadline, "closing stop did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(client.status().unwrap()["last_saved"]["enhanced"], true);
        let calls = client.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, MeetingCall::Stop))
                .count(),
            1
        );
        let last_save = calls
            .iter()
            .rposition(
                |call| matches!(call, MeetingCall::SaveNotes(text) if text == "final close text"),
            )
            .unwrap();
        let stop = calls
            .iter()
            .position(|call| matches!(call, MeetingCall::Stop))
            .unwrap();
        assert!(last_save < stop);
        page.state.apply_event(queued);
        assert!(page.state.closed.get());
    }

    #[test]
    fn shutdown_flushes_pending_notes_once_and_rejects_late_events() {
        let client = Arc::new(FakeMeetingClient::new());
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(8));
        page.state
            .notes_edited_at("closing draft".into(), Instant::now());
        page.state.shutdown();
        page.state.shutdown();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if client.status().unwrap()["session"]["user_notes"] == "closing draft" {
                break;
            }
            assert!(Instant::now() < deadline, "close flush did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(page.state.closed.get());
        assert!(page.state.timer.borrow().is_none());
        let saves = client
            .calls()
            .into_iter()
            .filter(|call| matches!(call, MeetingCall::SaveNotes(text) if text == "closing draft"))
            .count();
        assert_eq!(saves, 1);
        let generation = page.state.generation.get();
        page.state.apply_event(Event {
            generation,
            kind: EventKind::Status(Ok(json!({"session": {"seconds": 99}}))),
        });
        assert!(page.state.closed.get());
        assert!(page.state.status.borrow().is_some());
        assert_ne!(page.state.status_value()["session"]["seconds"], 99);
    }

    const MEETING: &str = "0123456789abcdef0123456789abcdef";

    fn two_speakers() -> Vec<Value> {
        vec![
            json!({"id": "me", "label": "Me", "name": null, "source": "me", "seconds": 60.0}),
            json!({"id": "S1", "label": "Speaker 1", "name": null, "source": "others", "seconds": 245.0}),
        ]
    }

    /// Apply delivered events in order until one matches `done`.
    fn apply_until(page: &MeetingsPage, done: impl Fn(&EventKind) -> bool) {
        loop {
            let event = receive(page);
            let hit = done(&event.kind);
            page.state.apply_event(event);
            if hit {
                return;
            }
        }
    }

    fn is_edit(kind: &EventKind) -> bool {
        matches!(kind, EventKind::SpeakerEdit { .. })
    }

    /// Start (fake) -> diarized stop through the page, then settle.
    fn stopped_with_speakers(client: &Arc<FakeMeetingClient>) -> MeetingsPage {
        stopped_with(client, two_speakers())
    }

    fn stopped_with(client: &Arc<FakeMeetingClient>, speakers: Vec<Value>) -> MeetingsPage {
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        client.seed_stop_speakers(MEETING, speakers);
        let page = page(client.clone(), active(31));
        page.state.primary_action();
        apply_until(&page, |kind| matches!(kind, EventKind::Stop(_)));
        page
    }

    #[test]
    fn a_saved_meeting_without_speaker_labels_has_no_panel_or_requests() {
        let client = Arc::new(FakeMeetingClient::new());
        client
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(31));
        page.state.primary_action();
        apply_until(&page, |kind| matches!(kind, EventKind::Stop(_)));
        assert!(page.state.last_saved.borrow().is_some());
        assert_eq!(page.state.saved_speaker_meeting(), None);
        // A live session without the field shows no speaker line.
        assert_eq!(
            meeting_speaker_status(&active(1).status.unwrap()["session"]),
            None
        );
        assert!(!client.calls().iter().any(|call| matches!(
            call,
            MeetingCall::Speakers(_)
                | MeetingCall::RenameSpeaker { .. }
                | MeetingCall::MergeSpeakers { .. }
                | MeetingCall::DiarizationStatus
        )));
        page.state.shutdown();
    }

    #[test]
    fn stop_speakers_fill_the_panel_and_rename_and_merge_apply() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        assert_eq!(page.state.saved_speaker_meeting().as_deref(), Some(MEETING));
        let speakers = page.state.speakers_of(MEETING);
        assert_eq!(speakers.len(), 2);

        page.state.begin_rename(MEETING, &speakers[1], false);
        assert_eq!(*page.state.speaker_name_draft.borrow(), "Speaker 1");
        *page.state.speaker_name_draft.borrow_mut() = " Ana ".into();
        page.state.commit_speaker_edit();
        assert!(page.state.speaker_editor.borrow().busy);
        // Start, Stop and Ask are not blocked by a speaker edit.
        assert!(!page.state.busy.get());
        apply_until(&page, is_edit);
        assert_eq!(page.state.speakers_of(MEETING)[1].label, "Ana");
        assert_eq!(page.state.speaker_editor.borrow().mode, SpeakerMode::List);
        assert!(client.calls().contains(&MeetingCall::RenameSpeaker {
            meeting_id: MEETING.into(),
            speaker_id: "S1".into(),
            name: "Ana".into(),
        }));

        page.state.begin_merge(MEETING, "S1", false);
        *page.state.merge_choice.borrow_mut() = "me".into();
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        let merged = page.state.speakers_of(MEETING);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "me");
        assert_eq!(merged[0].seconds, 305.0);
        page.state.shutdown();
    }

    #[test]
    fn a_refused_edit_stays_inline_and_rereads_the_speakers() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        let speakers = page.state.speakers_of(MEETING);
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "   ".into();
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        let editor = page.state.speaker_editor.borrow().clone();
        assert_eq!(editor.error, "invalid speaker name");
        assert_eq!(editor.mode, SpeakerMode::Rename("S1".into()));
        assert!(!editor.busy);
        // The failed edit was followed by one fresh read of the speakers.
        let calls = client.calls();
        let rename = calls
            .iter()
            .position(|call| matches!(call, MeetingCall::RenameSpeaker { .. }))
            .unwrap();
        assert!(calls[rename..]
            .iter()
            .any(|call| matches!(call, MeetingCall::Speakers(id) if id == MEETING)));
        // Cancel returns to the list and clears the error.
        page.state.cancel_speaker_edit();
        assert_eq!(page.state.speaker_editor.borrow().mode, SpeakerMode::List);
        assert!(page.state.speaker_editor.borrow().error.is_empty());
        page.state.shutdown();
    }

    #[test]
    fn a_stale_speaker_reply_is_dropped_and_start_still_runs() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        let speakers = page.state.speakers_of(MEETING);
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "Ana".into();
        page.state.commit_speaker_edit();
        // Closing the panel makes the in-flight reply stale.
        page.state.speaker_editor.borrow_mut().reset();
        apply_until(&page, is_edit);
        assert_eq!(page.state.speakers_of(MEETING)[1].label, "Speaker 1");

        // A new edit is in flight while the next meeting starts.
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "Bo".into();
        page.state.commit_speaker_edit();
        page.state.primary_action();
        assert!(page.state.busy.get(), "Start ran during the speaker edit");
        apply_until(&page, is_edit);
        assert!(!page.state.speaker_editor.borrow().busy);
        assert!(client
            .calls()
            .iter()
            .any(|call| matches!(call, MeetingCall::Start { .. })));
        page.state.shutdown();
    }

    #[test]
    fn recent_speakers_are_probed_only_for_diarized_notes_and_404_hides_the_action() {
        let vault = tempfile::tempdir().unwrap();
        let folder = vault.path().join("notes/Other");
        std::fs::create_dir_all(&folder).unwrap();
        let other = "f".repeat(32);
        let write = |name: &str, id: Option<&str>| {
            let meta = id.map_or(String::new(), |id| format!("meeting_id: \"{id}\"\n"));
            std::fs::write(
                folder.join(name),
                format!("---\ntitle: \"{name}\"\n{meta}---\n# {name}\n"),
            )
            .unwrap();
        };
        write("2026-11-13 1000 Plain.md", None);
        write("2026-11-14 1000 Labelled.md", Some(MEETING));
        write("2026-11-15 1000 Gone.md", Some(&other));

        let client = Arc::new(FakeMeetingClient::new());
        client.seed_speakers(MEETING, two_speakers());
        let page = page(client.clone(), idle());
        page.state.snapshot_mode.set(false);
        *page.state.vault_config.borrow_mut() =
            (vault.path().to_string_lossy().into_owned(), "notes".into());
        page.state.reload_recent();
        assert_eq!(page.state.recent.borrow().len(), 3);
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        let mut probed: Vec<String> = client
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MeetingCall::Speakers(id) => Some(id),
                _ => None,
            })
            .collect();
        probed.sort();
        assert_eq!(probed, [MEETING.to_string(), other.clone()]);
        let cache = page.state.recent_speakers.borrow().clone();
        assert_eq!(
            cache.get(MEETING).cloned().flatten().map(|s| s.len()),
            Some(2)
        );
        assert_eq!(cache.get(&other), Some(&None));

        // Cached: a reload asks nothing again; a transient error is not cached.
        page.state.recent_speakers.borrow_mut().remove(MEETING);
        client.set_down(Some("offline"));
        page.state.reload_recent();
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        assert!(!page.state.recent_speakers.borrow().contains_key(MEETING));
        client.set_down(None);
        let before = client.calls().len();
        page.state.reload_recent();
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        assert_eq!(client.calls().len(), before + 1, "only the uncached id");

        // Opening a row's speakers reads them again; closing resets the editor.
        page.state.toggle_recent_speakers(MEETING);
        assert_eq!(page.state.open_recent.borrow().as_deref(), Some(MEETING));
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        page.state.toggle_recent_speakers(MEETING);
        assert_eq!(*page.state.open_recent.borrow(), None);

        // Seeded snapshots never probe.
        page.seed_snapshot(idle());
        let calls = client.calls().len();
        page.state.reload_recent();
        assert_eq!(client.calls().len(), calls);
        page.state.shutdown();
    }

    #[test]
    fn reconnect_reads_uncached_recent_speakers_once_and_keeps_404s() {
        let vault = tempfile::tempdir().unwrap();
        let folder = vault.path().join("notes/Other");
        std::fs::create_dir_all(&folder).unwrap();
        let other = "f".repeat(32);
        for (name, id) in [
            ("2026-11-14 1000 A.md", MEETING),
            ("2026-11-15 1000 B.md", other.as_str()),
        ] {
            std::fs::write(
                folder.join(name),
                format!("---\ntitle: \"{name}\"\nmeeting_id: \"{id}\"\n---\n# {name}\n"),
            )
            .unwrap();
        }
        let speaker_reads = |client: &FakeMeetingClient| {
            client
                .calls()
                .into_iter()
                .filter(|call| matches!(call, MeetingCall::Speakers(_)))
                .count()
        };
        let client = Arc::new(FakeMeetingClient::new());
        client.seed_speakers(MEETING, two_speakers());
        let page = page(client.clone(), idle());
        page.state.snapshot_mode.set(false);
        *page.state.vault_config.borrow_mut() =
            (vault.path().to_string_lossy().into_owned(), "notes".into());

        // The first recent read and the status read both fail.
        client.set_down(Some("offline"));
        page.state.reload_recent();
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        page.state.retry();
        apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        assert!(page.state.retry_visible.get());
        assert!(page.state.recent_speakers.borrow().is_empty());

        // Retry succeeds: the uncached ids are read, the 404 is cached.
        client.set_down(None);
        page.state.retry();
        apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        {
            let cache = page.state.recent_speakers.borrow();
            assert_eq!(
                cache.get(MEETING).cloned().flatten().map(|s| s.len()),
                Some(2)
            );
            assert_eq!(cache.get(&other), Some(&None));
        }
        assert!(!page.state.probe_in_flight.get());

        // Another reconnect with everything cached (a 404 too) asks nothing.
        let before = speaker_reads(&client);
        page.state.retry();
        apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        assert!(!page.state.probe_in_flight.get(), "no busy loop");
        assert_eq!(speaker_reads(&client), before);

        // Reconnect while a probe from the old connection is in flight: it
        // waits for that probe, and its failure is read again once.
        page.state.recent_speakers.borrow_mut().remove(MEETING);
        let in_flight = |page: &MeetingsPage| {
            page.state.probe_in_flight.set(true);
            page.state.probe_seq.set(page.state.speaker_seq.get());
            *page.state.probe_ids.borrow_mut() = vec![MEETING.into()];
            *page.state.connection_message.borrow_mut() = "offline".into();
        };
        in_flight(&page);
        page.state.retry();
        apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        assert!(page.state.probe_after_reconnect.get());
        assert_eq!(
            speaker_reads(&client),
            before,
            "waits for the probe in flight"
        );
        page.state.apply_event(Event {
            generation: page.state.generation.get(),
            kind: EventKind::SpeakerProbe(vec![(
                MEETING.into(),
                Err(crate::backends::meeting::MeetingClientError(
                    "offline".into(),
                )),
            )]),
        });
        assert!(
            page.state.probe_in_flight.get(),
            "the failed id is read again"
        );
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        assert_eq!(speaker_reads(&client), before + 1);
        assert!(page.state.recent_speakers.borrow().contains_key(MEETING));
        assert!(!page.state.probe_in_flight.get());
        assert!(!page.state.probe_after_reconnect.get());

        // The same overlap where that probe succeeds: nothing is read again.
        page.state.recent_speakers.borrow_mut().remove(MEETING);
        in_flight(&page);
        page.state.retry();
        apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        page.state.apply_event(Event {
            generation: page.state.generation.get(),
            kind: EventKind::SpeakerProbe(vec![(
                MEETING.into(),
                Ok(Some(
                    json!({"meeting_id": MEETING, "speakers": two_speakers()}),
                )),
            )]),
        });
        assert!(page.state.recent_speakers.borrow().contains_key(MEETING));
        assert!(!page.state.probe_in_flight.get(), "no second read");
        assert!(!page.state.probe_after_reconnect.get());
        assert_eq!(speaker_reads(&client), before + 1);
        page.state.shutdown();
    }

    fn merge_calls(client: &FakeMeetingClient) -> Vec<(String, String)> {
        client
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MeetingCall::MergeSpeakers { from, into, .. } => Some((from, into)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn repeated_labels_merge_into_the_picked_speaker_and_a_stale_pick_merges_nothing() {
        // Two "Ana" rows with equal talk time, and a third distinct speaker.
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with(
            &client,
            vec![
                json!({"id": "S1", "label": "Ana", "name": "Ana", "source": "others", "seconds": 30.0}),
                json!({"id": "S2", "label": "Ana", "name": "Ana", "source": "others", "seconds": 30.0}),
                json!({"id": "S3", "label": "Bo", "name": null, "source": "others", "seconds": 12.0}),
            ],
        );
        let items = merge_target_items(&page.state.speakers_of(MEETING), "S3");
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles, ["Ana (S1)", "Ana (S2)"]);
        // The rows (and their action labels) use the same unique titles, so
        // either Ana can be told apart as a merge source, and reads the same
        // as a merge target from any other row.
        let rows = speaker_titles(&page.state.speakers_of(MEETING));
        assert_eq!(rows, ["Ana (S1)", "Ana (S2)", "Bo"]);
        let from_first = merge_target_items(&page.state.speakers_of(MEETING), "S1");
        let from_first: Vec<&str> = from_first.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(from_first, ["Ana (S2)", "Bo"]);

        // The picker opens on its first item, and the choice is that item.
        page.state.begin_merge(MEETING, "S3", false);
        assert_eq!(*page.state.merge_choice.borrow(), "S1");
        // The person picks the second Ana: exactly S2 is merged into.
        *page.state.merge_choice.borrow_mut() = "S2".into();
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        assert_eq!(merge_calls(&client), [("S3".to_string(), "S2".to_string())]);
        let left: Vec<String> = page
            .state
            .speakers_of(MEETING)
            .into_iter()
            .map(|speaker| speaker.id)
            .collect();
        assert_eq!(left, ["S1", "S2"]);

        // A stale or invalid choice is refused, never redirected elsewhere.
        for stale in ["S9", "S1", ""] {
            page.state.begin_merge(MEETING, "S1", false);
            *page.state.merge_choice.borrow_mut() = stale.into();
            page.state.commit_speaker_edit();
            let editor = page.state.speaker_editor.borrow().clone();
            assert!(!editor.busy, "{stale:?}");
            assert_eq!(editor.error, MERGE_PICK_AGAIN);
            assert_eq!(editor.mode, SpeakerMode::Merge("S1".into()));
            page.state.cancel_speaker_edit();
        }
        assert_eq!(merge_calls(&client).len(), 1);
        page.state.shutdown();
    }

    #[test]
    fn a_pick_removed_by_a_refresh_needs_a_new_explicit_pick_before_merging() {
        let client = Arc::new(FakeMeetingClient::new());
        let speaker = |id: &str, label: &str| json!({"id": id, "label": label, "name": null, "source": "others", "seconds": 10.0});
        let page = stopped_with(
            &client,
            vec![
                speaker("S0", "Ana"),
                speaker("S1", "Bo"),
                speaker("S2", "Cy"),
            ],
        );
        page.state.begin_merge(MEETING, "S0", false);
        *page.state.merge_choice.borrow_mut() = "S1".into();
        // A refresh removes S1; S2 is still available.
        page.state.store_speakers(
            MEETING,
            Some(&json!({"speakers": [speaker("S0", "Ana"), speaker("S2", "Cy")]})),
        );
        // Drawn exactly as the panel draws it, then read back as capture does.
        let speakers = page.state.speakers_of(MEETING);
        let targets = merge_target_items(&speakers, "S0");
        let choice = page.state.merge_choice.borrow().clone();
        let layout = merge_picker(&targets, &speakers, &choice);
        assert!(!layout.chosen, "Merge must stay disabled");
        assert_ne!(layout.titles[layout.selected], "Cy");
        *page.state.merge_choice.borrow_mut() =
            merge_choice_for_title(&targets, &layout.titles[layout.selected]);
        page.state.commit_speaker_edit();
        let editor = page.state.speaker_editor.borrow().clone();
        assert!(!editor.busy);
        assert_eq!(editor.error, MERGE_PICK_AGAIN);
        assert!(merge_calls(&client).is_empty(), "S2 was never picked");

        // The person now picks S2 explicitly: exactly S2 is merged into.
        *page.state.merge_choice.borrow_mut() = merge_choice_for_title(&targets, "Cy");
        let layout = merge_picker(&targets, &speakers, &page.state.merge_choice.borrow());
        assert!(layout.chosen);
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        assert_eq!(merge_calls(&client), [("S0".to_string(), "S2".to_string())]);
        page.state.shutdown();
    }

    #[test]
    fn a_merge_pick_updates_in_place_without_a_rebuild_so_the_picker_keeps_focus() {
        let client = Arc::new(FakeMeetingClient::new());
        let speaker = |id: &str, label: &str| json!({"id": id, "label": label, "name": null, "source": "others", "seconds": 10.0});
        let page = stopped_with(
            &client,
            vec![
                speaker("S0", "Ana"),
                speaker("S1", "Bo"),
                speaker("S2", "Bo"),
            ],
        );
        let rebuilds = Rc::new(Cell::new(0));
        {
            let rebuilds = rebuilds.clone();
            *page.state.rebuild.borrow_mut() = Some(Rc::new(move || {
                rebuilds.set(rebuilds.get() + 1);
            }));
        }
        page.state.visible.set(true);
        page.state.begin_merge(MEETING, "S0", false);
        let before = rebuilds.get();
        // Drawn exactly as the panel draws it.
        let speakers = page.state.speakers_of(MEETING);
        let targets = merge_target_items(&speakers, "S0");
        let layout = merge_picker(&targets, &speakers, &page.state.merge_choice.borrow());
        // The picker's action as the popup callback runs it: whether it was
        // handled in place, and what Merge was set to (`None`: untouched).
        let pick = |items: &[MergeTarget], complete: bool, title: &str| {
            let enabled = Cell::new(None);
            let set = |on: bool| enabled.set(Some(on));
            let handled = page.state.pick_merge_target(items, complete, title, set);
            (handled, enabled.get())
        };

        // Picking the second "Bo" stores exactly its id and enables Merge,
        // with no rebuild (a rebuild recreates the picker and drops focus).
        assert_eq!(pick(&targets, true, "Bo (S2)"), (true, Some(true)));
        assert_eq!(*page.state.merge_choice.borrow(), "S2");
        // An unmapped title (a placeholder) picks nothing and disables Merge.
        assert_eq!(pick(&targets, true, MERGE_PLACEHOLDER), (true, Some(false)));
        assert_eq!(*page.state.merge_choice.borrow(), "");
        // A picker whose titles collapsed never enables Merge.
        assert_eq!(
            pick(&targets, false, &layout.titles[0]),
            (true, Some(false))
        );
        assert_eq!(rebuilds.get(), before, "a pick must not rebuild the page");

        // A picker drawn for speakers no longer shown is not trusted: nothing
        // is stored or enabled, and the callback redraws instead.
        page.state.store_speakers(
            MEETING,
            Some(&json!({"speakers": [speaker("S0", "Ana"), speaker("S1", "Bo")]})),
        );
        *page.state.merge_choice.borrow_mut() = "S1".into();
        let before = rebuilds.get();
        assert_eq!(pick(&targets, true, "Bo (S2)"), (false, None));
        assert_eq!(*page.state.merge_choice.borrow(), "S1");
        assert_eq!(rebuilds.get(), before);
        page.state.redraw_merge_pick();
        assert_eq!(rebuilds.get(), before + 1);

        // The in-place pick is what commit merges, and commit still re-checks.
        let fresh = merge_target_items(&page.state.speakers_of(MEETING), "S0");
        assert_eq!(pick(&fresh, true, "Bo"), (true, Some(true)));
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        assert_eq!(merge_calls(&client), [("S0".to_string(), "S1".to_string())]);
        page.state.shutdown();
    }

    #[test]
    fn the_saved_meeting_is_cached_for_its_recent_row_during_the_next_meeting() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        page.state.snapshot_mode.set(false);
        assert_eq!(page.state.saved_speaker_meeting().as_deref(), Some(MEETING));
        // A probe of the saved meeting's id is cached, not skipped.
        page.state.probe(vec![MEETING.into()]);
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        let cached = page.state.recent_speakers.borrow().get(MEETING).cloned();
        assert_eq!(cached.flatten().map(|list| list.len()), Some(2));
        // And so is an edit's reply.
        page.state.recent_speakers.borrow_mut().clear();
        let speakers = page.state.speakers_of(MEETING);
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "Ana".into();
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        let cached = page.state.recent_speakers.borrow().get(MEETING).cloned();
        assert_eq!(cached.flatten().unwrap()[1].label, "Ana");
        // The next meeting starts: the saved panel hides, the Recent row
        // still has the answer it needs for its "Speakers…" action.
        page.state.primary_action();
        apply_until(&page, |kind| matches!(kind, EventKind::Start { .. }));
        // An older status read may still be queued (dropped by generation).
        for _ in 0..4 {
            if page.state.session().is_some() {
                break;
            }
            apply_until(&page, |kind| matches!(kind, EventKind::Status(_)));
        }
        assert!(page.state.session().is_some());
        assert_eq!(page.state.saved_speaker_meeting(), None);
        assert_eq!(page.state.speakers_of(MEETING)[1].label, "Ana");
        page.state.shutdown();
    }

    #[test]
    fn reads_started_before_an_edit_never_overwrite_it() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        page.state.snapshot_mode.set(false);
        let generation = page.state.generation.get();
        let old = json!({"meeting_id": MEETING, "speakers": two_speakers()});

        // A probe in flight when an edit is applied: its (older) answer is
        // not used, and the id is read again.
        page.state.probe_in_flight.set(true);
        page.state.probe_seq.set(page.state.speaker_seq.get());
        *page.state.probe_ids.borrow_mut() = vec![MEETING.into()];
        let speakers = page.state.speakers_of(MEETING);
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "Ana".into();
        page.state.commit_speaker_edit();
        apply_until(&page, is_edit);
        assert_eq!(page.state.speakers_of(MEETING)[1].label, "Ana");
        let reads = |client: &FakeMeetingClient| {
            client
                .calls()
                .iter()
                .filter(|call| matches!(call, MeetingCall::Speakers(_)))
                .count()
        };
        let before = reads(&client);
        page.state.apply_event(Event {
            generation,
            kind: EventKind::SpeakerProbe(vec![(MEETING.into(), Ok(Some(old.clone())))]),
        });
        let cached = page.state.recent_speakers.borrow().get(MEETING).cloned();
        assert_ne!(
            cached.flatten().map(|list| list[1].label.clone()),
            Some("Speaker 1".into())
        );
        assert!(page.state.probe_in_flight.get(), "the id is read again");
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        assert_eq!(reads(&client), before + 1);
        let cached = page.state.recent_speakers.borrow().get(MEETING).cloned();
        assert_eq!(cached.flatten().unwrap()[1].label, "Ana");

        // A status read from before the edit keeps the edited speakers.
        page.state
            .status_seq
            .set(page.state.speaker_seq.get().wrapping_sub(1));
        let mut stale_status = client.status().unwrap();
        stale_status["last_saved"]["speakers"] = json!(two_speakers());
        page.state.apply_event(Event {
            generation,
            kind: EventKind::Status(Ok(stale_status)),
        });
        assert_eq!(page.state.speakers_of(MEETING)[1].label, "Ana");
        page.state.shutdown();
    }

    #[test]
    fn an_explicit_refresh_queued_behind_a_probe_reads_its_cached_id() {
        let client = Arc::new(FakeMeetingClient::new());
        client.seed_speakers(MEETING, two_speakers());
        let page = page(client.clone(), idle());
        page.state.snapshot_mode.set(false);
        page.state
            .recent_speakers
            .borrow_mut()
            .insert(MEETING.into(), Some(Vec::new()));
        // Another probe is running when the row's speakers are opened.
        page.state.probe_in_flight.set(true);
        page.state.probe_seq.set(page.state.speaker_seq.get());
        page.state.toggle_recent_speakers(MEETING);
        assert!(page.state.probe_queue.borrow().contains(MEETING));
        assert!(!client
            .calls()
            .contains(&MeetingCall::Speakers(MEETING.into())));
        let other = "f".repeat(32);
        page.state.apply_event(Event {
            generation: page.state.generation.get(),
            kind: EventKind::SpeakerProbe(vec![(other, Ok(None))]),
        });
        // The queued id is read although it was cached.
        apply_until(&page, |kind| matches!(kind, EventKind::SpeakerProbe(_)));
        assert!(client
            .calls()
            .contains(&MeetingCall::Speakers(MEETING.into())));
        let cached = page.state.recent_speakers.borrow().get(MEETING).cloned();
        assert_eq!(cached.flatten().map(|list| list.len()), Some(2));
        page.state.shutdown();
    }

    #[test]
    fn an_edit_never_reopens_with_an_old_draft_on_another_surface() {
        let client = Arc::new(FakeMeetingClient::new());
        let page = stopped_with_speakers(&client);
        let speakers = page.state.speakers_of(MEETING);
        // A rename left open on the saved panel…
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "half typed".into();
        // …is not carried into the Recent panel of the same meeting.
        page.state.toggle_recent_speakers(MEETING);
        assert_eq!(page.state.speaker_editor.borrow().mode, SpeakerMode::List);
        assert!(page.state.speaker_name_draft.borrow().is_empty());

        // An open edit whose row was not drawn (its panel hid) is dropped.
        page.state.begin_merge(MEETING, "S1", true);
        page.state.forget_undrawn_edit(true);
        assert_eq!(
            page.state.speaker_editor.borrow().mode,
            SpeakerMode::Merge("S1".into())
        );
        page.state.forget_undrawn_edit(false);
        assert_eq!(page.state.speaker_editor.borrow().mode, SpeakerMode::List);
        assert!(page.state.merge_choice.borrow().is_empty());

        // The renamed speaker vanished (merged by the CLI, then read again):
        // the edit is dropped before layout, so no row action stays locked.
        page.state.begin_rename(MEETING, &speakers[1], false);
        let remaining = [speakers[0].clone()];
        // Only the matching surface's edit is touched.
        drop_orphaned_edit(&page.state, MEETING, &remaining, true);
        assert_eq!(
            page.state.speaker_editor.borrow().mode,
            SpeakerMode::Rename("S1".into())
        );
        drop_orphaned_edit(&page.state, MEETING, &speakers, false);
        assert_eq!(
            page.state.speaker_editor.borrow().mode,
            SpeakerMode::Rename("S1".into())
        );
        drop_orphaned_edit(&page.state, MEETING, &remaining, false);
        let editor = page.state.speaker_editor.borrow().clone();
        assert_eq!(editor.mode, SpeakerMode::List);
        assert!(!editor.busy && editor.error.is_empty());
        assert!(page.state.speaker_name_draft.borrow().is_empty());
        page.state.shutdown();
    }

    #[test]
    fn shutdown_includes_inflight_autosave_and_waits_for_newest_text() {
        let _writes = crate::shutdown::TestWrites::new();
        let (started, first_save_started) = mpsc::channel();
        let release_first = Arc::new((Mutex::new(false), Condvar::new()));
        let client = Arc::new(BlockingFirstSaveClient {
            inner: FakeMeetingClient::new(),
            save_calls: Mutex::new(0),
            first_save_started: started,
            release_first: release_first.clone(),
        });
        client
            .inner
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        let page = page(client.clone(), active(8));
        let now = Instant::now();
        page.state.notes_edited_at("before quit".into(), now);
        page.state.tick_at(now + NOTES_DEBOUNCE);
        first_save_started
            .recv_timeout(Duration::from_secs(2))
            .expect("autosave did not enter the client");

        // Leave only the normal in-flight write set: no debounce, no pending
        // autosave, and no stop flush. The final draft still has to win.
        let before = (
            page.state.save_in_flight.get(),
            page.state.save_kind.get(),
            page.state.notes_due.get(),
            page.state.save_pending.get(),
            page.state.stop_flush_pending.get(),
        );
        *page.state.notes_text.borrow_mut() = "newest text at quit".into();
        page.state.shutdown();

        {
            let (released, idle) = &*release_first;
            *released.lock().unwrap() = true;
            idle.notify_all();
        }
        assert_eq!(before, (true, Some(SaveKind::Autosave), None, false, false));
        assert!(
            crate::shutdown::wait_for_pending_writes(Duration::from_secs(2)),
            "quit wait returned before both writes completed"
        );
        let saves: Vec<String> = client
            .inner
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                MeetingCall::SaveNotes(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(saves, ["before quit", "newest text at quit"]);
        assert_eq!(
            client.status().unwrap()["session"]["user_notes"],
            "newest text at quit"
        );
    }

    /// A one-shot latch: `wait` blocks until `open`.
    #[derive(Default)]
    struct Gate(Mutex<bool>, Condvar);

    impl Gate {
        fn open(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }

        fn wait(&self) {
            let mut open = self.0.lock().unwrap();
            while !*open {
                open = self.1.wait(open).unwrap();
            }
        }
    }

    /// Blocks the rename until released and then refuses it; the refetch on
    /// the same worker thread blocks on a second gate.
    struct BlockingSpeakerEditClient {
        inner: FakeMeetingClient,
        rename_started: mpsc::Sender<()>,
        release_rename: Gate,
        edit_thread: Mutex<Option<thread::ThreadId>>,
        refetch_started: mpsc::Sender<()>,
        release_refetch: Gate,
    }

    impl MeetingClient for BlockingSpeakerEditClient {
        fn status(&self) -> MeetingResult<Value> {
            self.inner.status()
        }

        fn templates(&self) -> MeetingResult<Value> {
            self.inner.templates()
        }

        fn start(
            &self,
            title: &str,
            category: &str,
            template: &str,
            user_notes: &str,
            auto_title: Option<bool>,
        ) -> MeetingResult<Value> {
            self.inner
                .start(title, category, template, user_notes, auto_title)
        }

        fn save_notes(&self, notes: &str) -> MeetingResult<Value> {
            self.inner.save_notes(notes)
        }

        fn stop(&self) -> MeetingResult<Value> {
            self.inner.stop()
        }

        fn chat(&self, question: &str) -> MeetingResult<Value> {
            self.inner.chat(question)
        }

        fn dismiss(&self) -> MeetingResult<()> {
            self.inner.dismiss()
        }

        fn speakers(&self, meeting_id: &str) -> MeetingResult<Option<Value>> {
            if *self.edit_thread.lock().unwrap() == Some(thread::current().id()) {
                self.refetch_started.send(()).unwrap();
                self.release_refetch.wait();
            }
            self.inner.speakers(meeting_id)
        }

        fn rename_speaker(
            &self,
            _meeting_id: &str,
            _speaker_id: &str,
            _name: &str,
        ) -> MeetingResult<Value> {
            *self.edit_thread.lock().unwrap() = Some(thread::current().id());
            self.rename_started.send(()).unwrap();
            self.release_rename.wait();
            Err(crate::backends::meeting::MeetingClientError(
                "speaker changed underneath".into(),
            ))
        }
    }

    #[test]
    fn speaker_edit_is_a_pending_write_until_the_durable_call_returns() {
        let _writes = crate::shutdown::TestWrites::new();
        let (rename_tx, rename_started) = mpsc::channel();
        let (refetch_tx, refetch_started) = mpsc::channel();
        let client = Arc::new(BlockingSpeakerEditClient {
            inner: FakeMeetingClient::new(),
            rename_started: rename_tx,
            release_rename: Gate::default(),
            edit_thread: Mutex::new(None),
            refetch_started: refetch_tx,
            release_refetch: Gate::default(),
        });
        client
            .inner
            .start("Planning", "", "general", "agenda", Some(false))
            .unwrap();
        client.inner.seed_stop_speakers(MEETING, two_speakers());
        let page = page(client.clone(), active(31));
        page.state.primary_action();
        apply_until(&page, |kind| matches!(kind, EventKind::Stop(_)));
        assert!(crate::shutdown::wait_for_pending_writes(Duration::ZERO));

        let speakers = page.state.speakers_of(MEETING);
        page.state.begin_rename(MEETING, &speakers[1], false);
        *page.state.speaker_name_draft.borrow_mut() = "Ana".into();
        page.state.commit_speaker_edit();
        rename_started
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not enter the client");
        // Quit must wait while the rename is in the client.
        assert!(
            !crate::shutdown::wait_for_pending_writes(Duration::from_millis(50)),
            "an in-flight speaker edit was not a pending write"
        );

        client.release_rename.open();
        refetch_started
            .recv_timeout(Duration::from_secs(2))
            .expect("refused edit did not refetch");
        // The durable call returned: the follow-up read does not hold quit.
        assert!(
            crate::shutdown::wait_for_pending_writes(Duration::from_secs(2)),
            "the refetch after a refused edit extended the quit barrier"
        );

        client.release_refetch.open();
        apply_until(&page, is_edit);
        assert!(!page.state.speaker_editor.borrow().busy);
        page.state.shutdown();
    }
}
