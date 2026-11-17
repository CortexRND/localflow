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
use std::rc::{Rc, Weak};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBorderType, NSPopUpButton, NSScrollView,
    NSTextAlignment, NSTextField, NSView, NSWorkspace,
};
use objc2_foundation::{NSRange, NSSize, NSString, NSURL};
use serde_json::{json, Value};

use crate::backends::meeting::{
    vault_notes_path, MeetingClient, MeetingNotesWriter, MeetingResult,
};
use crate::hub_model::{
    meeting_capture_status, meeting_form_layout, meeting_notes_height, note_date_label,
    recent_notes, saved_notes_message, RecentNote, MEETING_NOTES_PLACEHOLDER, MEETING_SUBTITLE,
    MEETING_TITLE_PLACEHOLDER, RECENT_EMPTY, RECENT_NOTES_LIMIT, RECENT_ROW_H,
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
        }
    }
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
}

enum EventKind {
    Templates(MeetingResult<Value>),
    Status(MeetingResult<Value>),
    Start(MeetingResult<Value>),
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
}

struct Controls {
    title: Retained<NSTextField>,
    template: Retained<NSPopUpButton>,
    notes: Retained<PlaceholderTextView>,
    notes_status: Retained<NSTextField>,
    question: Retained<NSTextField>,
    palette: Palette,
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
        let focus = if let Some(editor) = controls.title.currentEditor() {
            Some((FocusedEditor::Title, editor.selectedRange()))
        } else if let Some(editor) = controls.question.currentEditor() {
            Some((FocusedEditor::Question, editor.selectedRange()))
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
            FocusedEditor::Title | FocusedEditor::Question => {
                let field = match focus {
                    FocusedEditor::Title => &controls.title,
                    _ => &controls.question,
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
        // Register every notes write before spawning, including autosaves.
        // The process-wide registry lets application quit wait for writes from
        // replaced Hub instances as well as the currently displayed one.
        let notes_write =
            matches!(&request, Request::Notes { .. }).then(crate::shutdown::register_pending_write);
        let spawned = thread::Builder::new().name(name).spawn(move || {
            let _notes_write = notes_write;
            let event_kind = match request {
                Request::Templates => EventKind::Templates(client.templates()),
                Request::Status => EventKind::Status(client.status()),
                Request::Start {
                    title,
                    template,
                    notes,
                    auto_title,
                } => {
                    EventKind::Start(client.start(&title, "", &template, &notes, Some(auto_title)))
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
        }
        self.request_rebuild();
    }

    fn poll(&self) {
        if self.closed.get() || self.status_in_flight.replace(true) {
            return;
        }
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
    }

    fn apply_event(&self, event: Event) {
        if self.closed.get() {
            return;
        }
        // Template metadata is not tied to a particular recording lifecycle,
        // but a reseeded snapshot must stay isolated from every older worker.
        let is_templates = matches!(&event.kind, EventKind::Templates(_));
        if event.generation != self.generation.get() && (!is_templates || self.snapshot_mode.get())
        {
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
                    Ok(result) => {
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
                        self.connection_message.borrow_mut().clear();
                        self.retry_visible.set(false);
                    }
                }
                self.next_poll.set(Some(Instant::now() + POLL_INTERVAL));
                self.request_rebuild();
            }
            EventKind::Start(result) => {
                self.busy.set(false);
                match result {
                    Err(error) => {
                        *self.error.borrow_mut() = error.to_string();
                        self.request_rebuild();
                    }
                    Ok(_) => {
                        self.notes_dirty.set(false);
                        *self.last_saved.borrow_mut() = None;
                        self.answers.borrow_mut().clear();
                        self.set_notes_notice(if self.current_notes().is_empty() {
                            String::new()
                        } else {
                            "Saved".into()
                        });
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
        let live_y = watch_y + if watch.is_empty() { 0.0 } else { 26.0 };
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

        let chat_available = session.is_none()
            && (status["chat_available"].as_bool().unwrap_or(false) || saved.is_some());
        let chat_y = saved_y + if saved.is_some() { 48.0 } else { 0.0 };
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
        let recent_height = if recent.is_empty() {
            f64::from(RECENT_ROW_H)
        } else {
            recent.len() as f64 * f64::from(RECENT_ROW_H)
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
            for (index, note) in recent.iter().enumerate() {
                let y = index as f64 * f64::from(RECENT_ROW_H);
                let open_x = width - f64::from(CARD_PADDING) - open_width;
                let date_x = open_x - 12.0 - date_width;
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
                if index + 1 < recent.len() {
                    add_divider(ctx, &recent_card, y + f64::from(RECENT_ROW_H) - 1.0, width);
                }
            }
        }
        ctx.body.addSubview(&recent_card);

        *state.controls.borrow_mut() = Some(Controls {
            title,
            template,
            notes,
            notes_status,
            question,
            palette: ctx.palette,
        });
        state.restore_focus();
        recent_y + 28.0 + recent_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::meeting::{FakeMeetingClient, MeetingCall};
    use crate::hub_model::RECENT_NOTES_LIMIT;
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
        assert!(matches!(event.kind, EventKind::Start(Ok(_))));
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
}
