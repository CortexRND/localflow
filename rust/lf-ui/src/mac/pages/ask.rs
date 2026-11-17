//! Native Ask page (port of `localflow/ask_mac.py`).
//!
//! The shell owns this page for the lifetime of the Hub. Conversation turns,
//! the draft, filters, caret intent and conversation scroll position therefore
//! live here rather than in the views, which are replaced on every render. Ask
//! calls run on a worker thread; only their completions cross back to the main
//! run loop, where a generation token prevents an old answer from replacing a
//! newer page state.

#![allow(deprecated)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::SystemTime;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAutoresizingMaskOptions, NSBorderType, NSButton,
    NSFontAttributeName, NSForegroundColorAttributeName, NSPopUpButton, NSScrollView,
    NSScrollerStyle, NSWorkspace,
};
use objc2_app_kit::{NSControlStateValueOff, NSControlStateValueOn, NSTextField};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSPoint, NSRange, NSString, NSURL,
};

use crate::ask_layout::{
    at_bottom, clamp_selection, conversation_layout, page_layout, resolve_scroll, ConversationItem,
    Placement, PlacementKind, AT_BOTTOM_SLACK, BODY_FONT, CAPTION_FONT, CHIP_FONT,
};
use crate::backends::ask::{
    citation_document, citation_label, citation_target, AskBackend, Citation, CitationTarget,
    FakeAskBackend, Turn, ERROR_GENERIC, ERROR_UNREACHABLE, EXAMPLES, INTRO, PRIVACY, SOURCES,
    THINKING, TIME_FILTERS,
};
use crate::mac::actions::Timer;
use crate::mac::page::{HubPage, HubServices, PageCtx};
use crate::mac::style::{
    font, rect, rect_of, style_bubble, style_chip, BubbleRole, ButtonKind, FlippedView,
};
use crate::mac::widgets::{button, field, label, popup, LabelStyle};
use crate::pystr::PyStr;
use crate::tokens::{Weight, ASK_BUBBLE_PADDING, ASK_CONTROL_FONT};

const PAGE: &str = "Ask";
const POLL_SECONDS: f64 = 0.2;
const PLACEHOLDER: &str = "Ask a question about your notes or dictations";
const SNAPSHOT_LONG_ANSWER: &str = concat!(
    "We agreed to launch on Friday and finish the checklist first [1]. ",
    "Marketing owns the announcement and engineering owns the rollout plan [2]. ",
    "We agreed to launch on Friday and finish the checklist first [1]. ",
    "Marketing owns the announcement and engineering owns the rollout plan [2]. ",
    "We agreed to launch on Friday and finish the checklist first [1]. ",
    "Marketing owns the announcement and engineering owns the rollout plan [2]. ",
    "We agreed to launch on Friday and finish the checklist first [1]. ",
    "Marketing owns the announcement and engineering owns the rollout plan [2]. "
);

/// Self-contained states used by native snapshots. Seeding is synchronous and
/// starts no worker or network request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskSnapshot {
    /// The first visit: intro, example chips and an empty composer.
    Empty,
    /// Two answered turns, including a long answer and citation chips.
    Populated,
    /// One answered launch-planning question.
    Conversation,
    /// The pending question is visible while the worker is thinking.
    Thinking,
    /// Four long answered turns for the scrollable conversation state.
    LongConversation,
    /// One answer with five sources, including long titles that need wrapping.
    Citations,
    /// A failed answer rendered as a red assistant bubble.
    Error,
}

#[derive(Default)]
struct Views {
    field: Option<Retained<NSTextField>>,
    ask: Option<Retained<NSButton>>,
    popup: Option<Retained<NSPopUpButton>>,
    sources: Vec<Retained<NSButton>>,
    clear: Option<Retained<NSButton>>,
    scroll: Option<Retained<NSScrollView>>,
}

struct Work {
    generation: u64,
    backend: Box<dyn AskBackend>,
    question: String,
    time_filter: String,
    sources: BTreeSet<String>,
}

struct Completion {
    generation: u64,
    backend: Box<dyn AskBackend>,
    turns: Vec<Turn>,
}

struct State {
    turns: Vec<Turn>,
    loaded: bool,
    draft: String,
    time_filter: String,
    sources: BTreeSet<String>,
    busy: bool,
    pending: String,
    generation: u64,
    closed: bool,
    /// Set by render and cleared by capture. A rebuild restores it before the
    /// run loop can poll again; navigation away leaves it false.
    visible: bool,
    focus: bool,
    selection: Option<(i64, i64)>,
    scroll_y: Option<f64>,
    was_at_bottom: bool,
    follow: bool,
    views: Views,
    sender: Sender<Completion>,
    receiver: Option<Receiver<Completion>>,
    timer: Option<Timer>,
    services: Option<Rc<HubServices>>,
    rebuild: Option<Rc<dyn Fn()>>,
}

type Shared = Rc<RefCell<State>>;

impl State {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        State {
            turns: Vec::new(),
            loaded: false,
            draft: String::new(),
            time_filter: TIME_FILTERS[0].0.to_string(),
            sources: SOURCES.iter().map(|(key, _)| (*key).to_string()).collect(),
            busy: false,
            pending: String::new(),
            generation: 0,
            closed: false,
            visible: false,
            focus: true,
            selection: None,
            scroll_y: None,
            was_at_bottom: true,
            follow: false,
            views: Views::default(),
            sender,
            receiver: Some(receiver),
            timer: None,
            services: None,
            rebuild: None,
        }
    }

    fn accepts(&self, generation: u64) -> bool {
        !self.closed && self.busy && self.generation == generation
    }
}

/// The AppKit Ask page.
pub struct AskPage {
    state: Shared,
}

impl Default for AskPage {
    fn default() -> Self {
        Self::new()
    }
}

impl AskPage {
    pub fn new() -> Self {
        AskPage {
            state: Rc::new(RefCell::new(State::new())),
        }
    }

    /// Replace the conversation and composer with a deterministic snapshot
    /// fixture. Call before the next render. No AppKit object, timer, backend or
    /// network access is needed. Any in-flight completion is made stale.
    pub fn seed_snapshot(&self, snapshot: AskSnapshot) {
        let turns = match snapshot {
            AskSnapshot::Empty => Vec::new(),
            AskSnapshot::Populated => vec![
                Turn {
                    question: "What did we decide about the launch?".into(),
                    answer: "We agreed to launch on Friday and finish the checklist first [1]."
                        .into(),
                    citations: vec![fixture_citation(
                        1,
                        "meeting",
                        "Launch planning",
                        "/tmp/localflow-ask-launch.md#01:20",
                    )],
                    error: String::new(),
                },
                Turn {
                    question: "And who owns the rollout checklist?".into(),
                    answer: concat!(
                        "Marketing owns the announcement and engineering owns the rollout ",
                        "plan [2]. The checklist should be complete before the launch review ",
                        "so support, documentation, and release owners can confirm their work."
                    )
                    .into(),
                    citations: vec![
                        fixture_citation(2, "dictation", "Launch marketing", "1"),
                        fixture_citation(
                            3,
                            "notes",
                            "Launch checklist",
                            "/tmp/localflow-ask-checklist.md",
                        ),
                    ],
                    error: String::new(),
                },
            ],
            AskSnapshot::Conversation => vec![Turn {
                question: "What did we decide about the launch?".into(),
                answer: "We agreed to launch on Friday and finish the checklist first [1].".into(),
                citations: vec![fixture_citation(
                    1,
                    "meeting",
                    "Launch planning",
                    "/tmp/localflow-ask-launch.md#01:20",
                )],
                error: String::new(),
            }],
            AskSnapshot::Thinking => Vec::new(),
            AskSnapshot::LongConversation => (1..=4)
                .map(|number| Turn {
                    question: format!("Question {number}: what did we decide about the launch?"),
                    answer: SNAPSHOT_LONG_ANSWER.into(),
                    citations: vec![fixture_citation(
                        1,
                        "meeting",
                        "Launch planning",
                        "/tmp/localflow-ask-launch.md#01:20",
                    )],
                    error: String::new(),
                })
                .collect(),
            AskSnapshot::Citations => vec![Turn {
                question: "Where did we talk about the launch?".into(),
                answer: SNAPSHOT_LONG_ANSWER.into(),
                citations: vec![
                    fixture_citation(
                        1,
                        "meeting",
                        "Launch planning",
                        "/tmp/localflow-ask-launch.md#01:20",
                    ),
                    fixture_citation(
                        2,
                        "dictation",
                        "A long dictation title about launch marketing",
                        "1",
                    ),
                    fixture_citation(3, "notes", "Launch checklist", "1"),
                    fixture_citation(
                        4,
                        "meeting",
                        "Rollout retrospective with engineering",
                        "/tmp/localflow-ask-launch.md#01:20",
                    ),
                    fixture_citation(5, "notes", "Marketing announcement plan", "1"),
                ],
                error: String::new(),
            }],
            AskSnapshot::Error => vec![Turn {
                question: "What did we decide about the launch?".into(),
                answer: String::new(),
                citations: Vec::new(),
                error: ERROR_UNREACHABLE.into(),
            }],
        };
        let mut state = self.state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        state.turns = turns;
        state.loaded = true;
        state.draft.clear();
        state.time_filter = TIME_FILTERS[0].0.to_string();
        state.sources = SOURCES.iter().map(|(key, _)| (*key).to_string()).collect();
        state.busy = false;
        state.pending.clear();
        if snapshot == AskSnapshot::Thinking {
            state.busy = true;
            state.pending = "And who owns the rollout checklist?".into();
        }
        state.focus = true;
        state.selection = None;
        state.scroll_y = None;
        state.was_at_bottom = true;
        state.follow = false;
    }
}

fn fixture_citation(n: i64, source: &str, title: &str, reference: &str) -> Citation {
    Citation {
        n,
        source: source.into(),
        title: title.into(),
        app: String::new(),
        when: "2026-04-15T12:00".into(),
        snippet: String::new(),
        reference: reference.into(),
    }
}

/// Explicit text color is needed on native checkboxes in dark mode.
fn source_title(ctx: &PageCtx, text: &str) -> Retained<NSAttributedString> {
    let text_font = font(f64::from(ASK_CONTROL_FONT), Weight::Regular);
    let ink = crate::mac::style::color("ink", &ctx.palette);
    // SAFETY: immutable AppKit attribute-name constants.
    let (font_key, color_key) = unsafe { (NSFontAttributeName, NSForegroundColorAttributeName) };
    let attrs = NSDictionary::from_slices(
        &[font_key, color_key],
        &[text_font.as_ref() as &AnyObject, ink.as_ref() as &AnyObject],
    );
    // SAFETY: normal immutable title initialization on the main thread.
    unsafe {
        NSAttributedString::initWithString_attributes(
            ctx.mtm.alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    }
}

/// Build the data shown by the conversation and flatten citations in exactly
/// the same order as their chips.
fn conversation_items(
    turns: &[Turn],
    busy: bool,
    pending: &str,
) -> (Vec<ConversationItem>, Vec<Citation>) {
    let mut items = Vec::new();
    let mut citations = Vec::new();
    if turns.is_empty() && !busy {
        items.push(ConversationItem::Intro(INTRO.to_string()));
        items.push(ConversationItem::Examples(
            EXAMPLES.iter().map(|text| (*text).to_string()).collect(),
        ));
    }
    for turn in turns {
        items.push(ConversationItem::User(turn.question.clone()));
        items.push(ConversationItem::Assistant {
            text: if turn.error.is_empty() {
                turn.answer.clone()
            } else {
                turn.error.clone()
            },
            is_error: !turn.error.is_empty(),
        });
        if !turn.citations.is_empty() {
            citations.extend(turn.citations.iter().cloned());
            let labels = turn
                .citations
                .iter()
                .map(|citation| {
                    citation_label(citation)
                        .unwrap_or_else(|_| format!("[{}] {}", citation.n, citation.title))
                })
                .collect();
            items.push(ConversationItem::Citations(labels));
        }
    }
    if busy {
        items.push(ConversationItem::User(pending.to_string()));
        items.push(ConversationItem::Assistant {
            text: THINKING.to_string(),
            is_error: false,
        });
    }
    (items, citations)
}

fn now_seconds() -> Option<f64> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64())
}

/// Transfer the backend to a worker only after the thread was created
/// successfully. Returning `Work` on failure lets the caller restore it on the
/// main thread instead of losing the injected service.
fn launch(work: Work, sender: Sender<Completion>) -> Result<(), Work> {
    let (job_sender, job_receiver) = mpsc::channel::<Work>();
    let spawned = std::thread::Builder::new()
        .name("localflow-ask".into())
        .spawn(move || {
            let Ok(work) = job_receiver.recv() else {
                return;
            };
            let Work {
                generation,
                mut backend,
                question,
                time_filter,
                sources,
            } = work;
            // AskModel.ask never raises for model/source failures: it stores the
            // friendly error on the returned turn and appends it to its turns.
            backend.ask(&question, &time_filter, Some(&sources), now_seconds());
            let turns = backend.turns();
            let _ = sender.send(Completion {
                generation,
                backend,
                turns,
            });
        });
    match spawned {
        Ok(_) => job_sender.send(work).map_err(|error| error.0),
        Err(_) => Err(work),
    }
}

fn submit(state: &Shared, services: &Rc<HubServices>, rebuild: &Rc<dyn Fn()>) {
    let Some(field) = state.borrow().views.field.clone() else {
        return;
    };
    let question = field.stringValue().to_string();
    let question = question.py_strip().to_string();
    if question.is_empty() {
        return;
    }

    let work = {
        let mut state = state.borrow_mut();
        if state.closed || state.busy {
            return;
        }
        state.busy = true;
        state.pending = question.clone();
        state.draft.clear();
        state.selection = None;
        state.focus = true;
        state.follow = true;
        state.generation = state.generation.wrapping_add(1);
        // A second action from the old field must not submit twice while the
        // shell's rebuild is deferred to the next run-loop turn.
        state.views.field = None;

        let mut backend = services
            .ask_backend
            .replace(Box::new(FakeAskBackend::default()));
        backend.set_turns(state.turns.clone());
        Work {
            generation: state.generation,
            backend,
            question,
            time_filter: state.time_filter.clone(),
            sources: state.sources.clone(),
        }
    };

    // Give the controls their busy state immediately; the full rebuild below
    // also replaces the conversation with the pending question and Thinking...
    // bubble. No RefCell borrow is held while AppKit is called.
    field.setEnabled(false);
    {
        let state = state.borrow();
        if let Some(button) = &state.views.ask {
            button.setEnabled(false);
        }
        if let Some(popup) = &state.views.popup {
            popup.setEnabled(false);
        }
        for source in &state.views.sources {
            source.setEnabled(false);
        }
        if let Some(clear) = &state.views.clear {
            clear.setEnabled(false);
        }
    }

    let sender = state.borrow().sender.clone();
    if let Err(work) = launch(work, sender) {
        // Thread creation/job handoff failure is not a model success. Keep the
        // service usable and show the same generic failed-answer fixture that
        // AskBackend reserves for unexpected errors.
        let Work {
            mut backend,
            question,
            ..
        } = work;
        let (generation, mut turns) = {
            let state = state.borrow();
            (state.generation, state.turns.clone())
        };
        let mut failed = Turn::new(question);
        failed.error = ERROR_GENERIC.into();
        turns.push(failed);
        backend.set_turns(turns.clone());
        *services.ask_backend.borrow_mut() = backend;
        {
            let mut state = state.borrow_mut();
            if state.accepts(generation) {
                state.turns = turns;
                state.busy = false;
                state.pending.clear();
                state.follow = true;
            }
        }
    }
    rebuild();
}

/// Deliver all queued worker completions. This function is called only by the
/// main-run-loop timer. Stale/shutdown generations drop both answer and turns;
/// they can never overwrite a seeded, cleared or newer conversation.
fn poll(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else {
        return;
    };
    let mut completed = Vec::new();
    {
        let state = state.borrow();
        let Some(receiver) = &state.receiver else {
            return;
        };
        while let Ok(completion) = receiver.try_recv() {
            completed.push(completion);
        }
    }

    for completion in completed {
        let Completion {
            generation,
            backend,
            turns,
        } = completion;
        let (rebuild, visible) = {
            let mut state = state.borrow_mut();
            if !state.accepts(generation) {
                continue;
            }
            let Some(services) = state.services.clone() else {
                continue;
            };
            *services.ask_backend.borrow_mut() = backend;
            state.turns = turns;
            state.busy = false;
            state.pending.clear();
            state.follow = true;
            (state.rebuild.clone(), state.visible)
        };
        // An answer arriving on another page updates the model but never
        // navigates or repaints that other page. `follow` is consumed by the
        // next Ask render instead.
        if visible {
            if let Some(rebuild) = rebuild {
                rebuild();
            }
        }
    }
}

fn clear(state: &Shared, services: &HubServices, rebuild: &Rc<dyn Fn()>) {
    {
        let mut state = state.borrow_mut();
        if state.busy || state.closed {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        state.turns.clear();
        state.draft.clear();
        state.selection = None;
        state.focus = true;
        state.scroll_y = None;
        state.was_at_bottom = true;
        state.follow = false;
        // Do not let capture read a pre-clear field or scroller back into the
        // newly reset state during the deferred rebuild.
        state.views = Views::default();
    }
    services.ask_backend.borrow_mut().clear();
    rebuild();
}

fn open_citation(mtm: MainThreadMarker, services: &HubServices, citation: &Citation) {
    let title = citation_label(citation).unwrap_or_else(|_| {
        if citation.title.is_empty() {
            "Source".to_string()
        } else {
            citation.title.clone()
        }
    });
    match citation_target(citation) {
        Ok(CitationTarget::History(id)) => {
            let text = match services.history.get(id) {
                Ok(Some(row)) => {
                    if row.final_text.is_empty() {
                        if row.raw_text.is_empty() {
                            "(no text)".to_string()
                        } else {
                            row.raw_text
                        }
                    } else {
                        row.final_text
                    }
                }
                _ => "This history entry is no longer available.".to_string(),
            };
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(&title));
            alert.setInformativeText(&NSString::from_str(&text));
            alert.runModal();
        }
        Ok(CitationTarget::File(path)) => {
            let config = services.config();
            let Some(path) = citation_document(&path, config.field_str("vault_path").unwrap_or(""))
            else {
                source_open_error(mtm);
                return;
            };
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            NSWorkspace::sharedWorkspace()
                .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
        }
        Err(_) => source_open_error(mtm),
    }
}

fn source_open_error(mtm: MainThreadMarker) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Couldn't open that source file."));
    alert.setInformativeText(&NSString::from_str("It may have been moved or deleted."));
    alert.runModal();
}

fn chip(ctx: &PageCtx, parent: &FlippedView, placement: &Placement, on_click: impl Fn() + 'static) {
    // Layout labels are fixed non-empty examples/citation labels. Keep a
    // fallback so even a malformed backend citation never creates an unnamed
    // accessible button.
    let title = if placement.text.is_empty() {
        "Open source"
    } else {
        &placement.text
    };
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, ctx.mtm)
    };
    button.setFrame(rect(placement.x, placement.y, placement.w, placement.h));
    button.setAccessibilityLabel(Some(&NSString::from_str(title)));
    button.setToolTip(Some(&NSString::from_str(title)));
    style_chip(&button, &ctx.palette, CHIP_FONT);
    ctx.target.on_action(&button, on_click);
    parent.addSubview(&button);
}

fn add_placement(ctx: &PageCtx, document: &FlippedView, placement: &Placement) {
    match placement.kind {
        PlacementKind::Intro => document.addSubview(&label(
            ctx.mtm,
            &placement.text,
            placement.x,
            placement.y,
            placement.w,
            LabelStyle {
                size: BODY_FONT,
                color: "muted",
                height: placement.h,
                wrap: true,
                ..LabelStyle::default()
            },
            &ctx.palette,
        )),
        PlacementKind::User | PlacementKind::Assistant => {
            let bubble = FlippedView::new(
                ctx.mtm,
                rect(placement.x, placement.y, placement.w, placement.h),
            );
            style_bubble(
                &bubble,
                &ctx.palette,
                if placement.kind == PlacementKind::User {
                    BubbleRole::User
                } else {
                    BubbleRole::Assistant
                },
            );
            let text = label(
                ctx.mtm,
                &placement.text,
                f64::from(ASK_BUBBLE_PADDING),
                f64::from(ASK_BUBBLE_PADDING),
                placement.text_w.unwrap_or(placement.w),
                LabelStyle {
                    size: BODY_FONT,
                    color: if placement.error { "red" } else { "ink" },
                    height: placement.h - 2.0 * f64::from(ASK_BUBBLE_PADDING),
                    wrap: true,
                    ..LabelStyle::default()
                },
                &ctx.palette,
            );
            text.setSelectable(true);
            bubble.addSubview(&text);
            document.addSubview(&bubble);
        }
        PlacementKind::Chip | PlacementKind::Cite => {}
    }
}

fn restore_focus(state: &Shared) {
    let (field, enabled, focus, selection, draft) = {
        let state = state.borrow();
        (
            state.views.field.clone(),
            !state.busy && !state.closed,
            state.focus,
            state.selection,
            state.draft.clone(),
        )
    };
    let Some(field) = field else {
        return;
    };
    if enabled && focus {
        if let Some(window) = field.window() {
            window.makeFirstResponder(Some(&field));
            if let Some(editor) = field.currentEditor() {
                let (location, length) = clamp_selection(selection, &draft);
                editor.setSelectedRange(NSRange::new(location as usize, length as usize));
            }
        }
        state.borrow_mut().selection = None;
    }
}

impl HubPage for AskPage {
    fn prepare_snapshot(
        &mut self,
        preparation: &str,
        _root: &std::path::Path,
    ) -> anyhow::Result<()> {
        let snapshot = match preparation {
            "empty" => AskSnapshot::Empty,
            "conversation" => AskSnapshot::Conversation,
            "thinking" => AskSnapshot::Thinking,
            "error" => AskSnapshot::Error,
            "long-conversation" => AskSnapshot::LongConversation,
            "citations" => AskSnapshot::Citations,
            other => anyhow::bail!("unknown Ask snapshot preparation {other:?}"),
        };
        self.seed_snapshot(snapshot);
        Ok(())
    }

    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        let views = {
            let mut state = self.state.borrow_mut();
            state.visible = false;
            std::mem::take(&mut state.views)
        };

        let field_state = views.field.as_ref().map(|field| {
            let draft = field.stringValue().to_string();
            let editor = field.currentEditor();
            let selection = editor.as_ref().map(|editor| {
                let selected = editor.selectedRange();
                (selected.location as i64, selected.length as i64)
            });
            let focus = editor.is_some();
            let enabled = field.isEnabled();
            (draft, selection, focus, enabled)
        });
        let scroll_state = views.scroll.as_ref().map(|scroll| {
            let clip = scroll.contentView().bounds();
            let content_height = scroll
                .documentView()
                .map_or(0.0, |document| document.frame().size.height);
            let y = clip.origin.y;
            (
                y,
                at_bottom(y, content_height, clip.size.height, AT_BOTTOM_SLACK),
            )
        });
        let mut state = self.state.borrow_mut();
        if let Some((draft, selection, focus, enabled)) = field_state {
            state.draft = draft;
            if selection.is_some() {
                state.selection = selection;
            }
            // A disabled (busy) field cannot hold focus; retain the previous
            // intent so the caret returns when the answer arrives.
            if selection.is_some() || enabled {
                state.focus = focus;
            }
        }
        if let Some((y, at_bottom)) = scroll_state {
            state.scroll_y = Some(y);
            state.was_at_bottom = at_bottom;
        }
    }

    fn shutdown(&mut self) {
        let timer = {
            let mut state = self.state.borrow_mut();
            state.closed = true;
            state.generation = state.generation.wrapping_add(1);
            state.visible = false;
            state.views = Views::default();
            state.receiver = None;
            state.rebuild = None;
            state.services = None;
            state.timer.take()
        };
        if let Some(timer) = timer {
            timer.invalidate();
        }
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        {
            let mut state = self.state.borrow_mut();
            if state.closed {
                return top;
            }
            state.visible = true;
            state.services = Some(ctx.services.clone());
            state.rebuild = Some(ctx.rebuild.clone());
            if !state.loaded {
                state.turns = ctx.services.ask_backend.borrow().turns();
                state.loaded = true;
            }
            if state.timer.is_none() {
                let weak = Rc::downgrade(&self.state);
                state.timer = Some(Timer::schedule(POLL_SECONDS, true, move || poll(&weak)));
            }
        }

        let measure = |text: &str, size: f64, weight: Weight| (ctx.measure)(text, size, weight);
        let layout = page_layout(ctx.width, ctx.viewport_height, top, &measure, PRIVACY);
        let (busy, pending, turns, time_filter, sources, draft) = {
            let state = self.state.borrow();
            (
                state.busy,
                state.pending.clone(),
                state.turns.clone(),
                state.time_filter.clone(),
                state.sources.clone(),
                state.draft.clone(),
            )
        };
        let weak = Rc::downgrade(&self.state);

        // Compact controls row: time filter, source checkboxes and Clear.
        let (popup_x, popup_w) = layout.toolbar.popup;
        let titles: Vec<String> = TIME_FILTERS
            .iter()
            .map(|(title, _)| (*title).to_string())
            .collect();
        let selected = TIME_FILTERS
            .iter()
            .position(|(title, _)| *title == time_filter)
            .unwrap_or(0);
        let popup = popup(
            ctx.mtm,
            &titles,
            selected,
            popup_x,
            layout.toolbar_y,
            popup_w,
            "Conversation time filter",
        );
        popup.setFrame(rect(
            popup_x,
            layout.toolbar_y,
            popup_w,
            layout.toolbar.height,
        ));
        popup.setFont(Some(&font(f64::from(ASK_CONTROL_FONT), Weight::Regular)));
        popup.setEnabled(!busy);
        {
            let weak = weak.clone();
            let popup_ref = popup.clone();
            ctx.target.on_action(&popup, move || {
                if let (Some(state), Some(title)) =
                    (weak.upgrade(), popup_ref.titleOfSelectedItem())
                {
                    state.borrow_mut().time_filter = title.to_string();
                }
            });
        }
        ctx.body.addSubview(&popup);

        let mut source_buttons = Vec::new();
        for (index, (key, title)) in SOURCES.iter().enumerate() {
            let spec = &layout.toolbar.toggles[index];
            let checkbox = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str(title),
                    None,
                    None,
                    ctx.mtm,
                )
            };
            checkbox.setFrame(rect(spec.x, layout.toolbar_y + 4.0, spec.w, 20.0));
            checkbox.setAttributedTitle(&source_title(ctx, title));
            checkbox.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "Search {title} source"
            ))));
            checkbox.setState(if sources.contains(*key) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            checkbox.setEnabled(!busy);
            {
                let weak = weak.clone();
                let checkbox_ref = checkbox.clone();
                let key = (*key).to_string();
                ctx.target.on_action(&checkbox, move || {
                    if let Some(state) = weak.upgrade() {
                        let mut state = state.borrow_mut();
                        if checkbox_ref.state() == NSControlStateValueOn {
                            state.sources.insert(key.clone());
                        } else {
                            state.sources.remove(&key);
                        }
                    }
                });
            }
            ctx.body.addSubview(&checkbox);
            source_buttons.push(checkbox);
        }

        let (clear_x, clear_w) = layout.toolbar.clear;
        let clear_button = button(
            ctx.mtm,
            layout.toolbar.clear_title,
            clear_x,
            layout.toolbar_y,
            clear_w,
            layout.toolbar.height,
            ButtonKind::Secondary,
            &ctx.palette,
        );
        clear_button.setAccessibilityLabel(Some(&NSString::from_str("Clear conversation")));
        clear_button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        clear_button.setEnabled(!turns.is_empty() && !busy);
        {
            let weak = weak.clone();
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&clear_button, move || {
                if let Some(state) = weak.upgrade() {
                    clear(&state, &services, &rebuild);
                }
            });
        }
        ctx.body.addSubview(&clear_button);

        // Conversation: independent vertical scroller, a flipped document and
        // the exact measured placements from ask_layout.
        let (x, y, width, height) = layout.conversation;
        let scroll = NSScrollView::initWithFrame(ctx.mtm.alloc(), rect(x, y, width, height));
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setScrollerStyle(NSScrollerStyle::Legacy);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.setDrawsBackground(false);
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let (items, citations) = conversation_items(&turns, busy, &pending);
        let conversation = conversation_layout(&items, width, &measure);
        let document = FlippedView::new(
            ctx.mtm,
            rect(0.0, 0.0, width, conversation.content_height.max(height)),
        );
        document.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let mut citation_index = 0usize;
        for placement in &conversation.placements {
            match placement.kind {
                PlacementKind::Cite => {
                    let citation = citations[citation_index].clone();
                    citation_index += 1;
                    let services = ctx.services.clone();
                    let mtm = ctx.mtm;
                    chip(ctx, &document, placement, move || {
                        open_citation(mtm, &services, &citation);
                    });
                }
                PlacementKind::Chip => {
                    let example = placement.tag.unwrap_or(0);
                    let weak = weak.clone();
                    let services = ctx.services.clone();
                    let rebuild = ctx.rebuild.clone();
                    chip(ctx, &document, placement, move || {
                        let Some(state) = weak.upgrade() else {
                            return;
                        };
                        let field = {
                            let current = state.borrow();
                            if current.busy {
                                return;
                            }
                            current.views.field.clone()
                        };
                        if let Some(field) = field {
                            field.setStringValue(&NSString::from_str(EXAMPLES[example]));
                            submit(&state, &services, &rebuild);
                        }
                    });
                }
                _ => add_placement(ctx, &document, placement),
            }
        }
        scroll.setDocumentView(Some(&document));
        ctx.body.addSubview(&scroll);
        let (saved_y, was_at_bottom, follow) = {
            let mut state = self.state.borrow_mut();
            let saved = (state.scroll_y, state.was_at_bottom, state.follow);
            state.follow = false;
            saved
        };
        let target = resolve_scroll(
            saved_y,
            was_at_bottom,
            follow,
            conversation.content_height,
            height,
            conversation.anchor_y,
        );
        document.scrollPoint(NSPoint::new(0.0, target));

        // Composer pinned above the small privacy note.
        let field = field(
            ctx.mtm,
            &draft,
            layout.field.0,
            layout.field.1,
            layout.field.2,
            "Ask a question",
        );
        field.setFrame(rect_of(layout.field));
        field.setPlaceholderString(Some(&NSString::from_str(PLACEHOLDER)));
        field.setEnabled(!busy);
        field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let ask = button(
            ctx.mtm,
            "Ask",
            layout.ask.0,
            layout.ask.1,
            layout.ask.2,
            layout.ask.3,
            ButtonKind::Primary,
            &ctx.palette,
        );
        ask.setAccessibilityLabel(Some(&NSString::from_str("Ask question")));
        ask.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        ask.setEnabled(!busy);
        {
            let weak = weak.clone();
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&field, move || {
                if let Some(state) = weak.upgrade() {
                    submit(&state, &services, &rebuild);
                }
            });
        }
        {
            let weak = weak.clone();
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&ask, move || {
                if let Some(state) = weak.upgrade() {
                    submit(&state, &services, &rebuild);
                }
            });
        }
        ctx.body.addSubview(&field);
        ctx.body.addSubview(&ask);
        ctx.body.addSubview(&label(
            ctx.mtm,
            PRIVACY,
            layout.privacy.0,
            layout.privacy.1,
            layout.privacy.2,
            LabelStyle {
                size: CAPTION_FONT,
                color: "muted",
                height: layout.privacy.3,
                wrap: true,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        self.state.borrow_mut().views = Views {
            field: Some(field),
            ask: Some(ask),
            popup: Some(popup),
            sources: source_buttons,
            clear: Some(clear_button),
            scroll: Some(scroll),
        };
        restore_focus(&self.state);
        layout.bottom
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;
    use std::time::Duration;

    use super::*;

    struct ThreadBackend {
        turns: Vec<Turn>,
        called_on: Arc<Mutex<Option<ThreadId>>>,
    }

    impl AskBackend for ThreadBackend {
        fn turns(&self) -> Vec<Turn> {
            self.turns.clone()
        }

        fn set_turns(&mut self, turns: Vec<Turn>) {
            self.turns = turns;
        }

        fn clear(&mut self) {
            self.turns.clear();
        }

        fn ask(
            &mut self,
            question: &str,
            _time_filter: &str,
            _sources: Option<&BTreeSet<String>>,
            _now: Option<f64>,
        ) -> Turn {
            *self.called_on.lock().unwrap() = Some(std::thread::current().id());
            let mut turn = Turn::new(question);
            turn.answer = "worker answer".into();
            self.turns.push(turn.clone());
            turn
        }
    }

    fn assert_snapshot_citation(
        citation: &Citation,
        n: i64,
        source: &str,
        title: &str,
        reference: &str,
    ) {
        assert_eq!(citation.n, n);
        assert_eq!(citation.source, source);
        assert_eq!(citation.title, title);
        assert_eq!(citation.when, "2026-04-15T12:00");
        assert_eq!(citation.reference, reference);
        assert!(citation.app.is_empty() && citation.snippet.is_empty());
    }

    #[test]
    fn empty_fixture_has_intro_examples_and_no_citations() {
        let page = AskPage::new();
        assert_eq!(page.name(), "Ask");
        page.seed_snapshot(AskSnapshot::Empty);
        let state = page.state.borrow();
        assert!(state.turns.is_empty());
        let (items, citations) = conversation_items(&state.turns, state.busy, &state.pending);
        assert!(citations.is_empty());
        assert!(matches!(&items[0], ConversationItem::Intro(_)));
        assert!(matches!(&items[1], ConversationItem::Examples(labels) if labels.len() == 3));
        assert!(state.draft.is_empty() && !state.follow);
    }

    #[test]
    fn long_and_citation_fixtures_match_python_snapshot_content() {
        let page = AskPage::new();
        let expected_answer = concat!(
            "We agreed to launch on Friday and finish the checklist first [1]. ",
            "Marketing owns the announcement and engineering owns the rollout plan [2]. "
        )
        .repeat(4);
        let meeting_reference = "/tmp/localflow-ask-launch.md#01:20";

        page.seed_snapshot(AskSnapshot::LongConversation);
        {
            let state = page.state.borrow();
            assert_eq!(state.turns.len(), 4);
            for (index, turn) in state.turns.iter().enumerate() {
                let number = index + 1;
                assert_eq!(
                    turn.question,
                    format!("Question {number}: what did we decide about the launch?")
                );
                assert_eq!(turn.answer, expected_answer);
                assert!(turn.error.is_empty());
                assert_eq!(turn.citations.len(), 1);
                assert_snapshot_citation(
                    &turn.citations[0],
                    1,
                    "meeting",
                    "Launch planning",
                    meeting_reference,
                );
            }
        }

        page.seed_snapshot(AskSnapshot::Citations);
        let state = page.state.borrow();
        assert_eq!(state.turns.len(), 1);
        let turn = &state.turns[0];
        assert_eq!(turn.question, "Where did we talk about the launch?");
        assert_eq!(turn.answer, expected_answer);
        assert!(turn.error.is_empty());
        let expected_citations = [
            (1, "meeting", "Launch planning", meeting_reference),
            (
                2,
                "dictation",
                "A long dictation title about launch marketing",
                "1",
            ),
            (3, "notes", "Launch checklist", "1"),
            (
                4,
                "meeting",
                "Rollout retrospective with engineering",
                meeting_reference,
            ),
            (5, "notes", "Marketing announcement plan", "1"),
        ];
        assert_eq!(turn.citations.len(), expected_citations.len());
        for (citation, (n, source, title, reference)) in
            turn.citations.iter().zip(expected_citations)
        {
            assert_snapshot_citation(citation, n, source, title, reference);
        }
    }

    #[test]
    fn conversation_fixture_matches_python_snapshot_content() {
        let page = AskPage::new();
        page.seed_snapshot(AskSnapshot::Conversation);
        let state = page.state.borrow();
        assert_eq!(state.turns.len(), 1);
        let turn = &state.turns[0];
        assert_eq!(turn.question, "What did we decide about the launch?");
        assert_eq!(
            turn.answer,
            "We agreed to launch on Friday and finish the checklist first [1]."
        );
        assert!(turn.error.is_empty());
        assert_eq!(turn.citations.len(), 1);
        assert_snapshot_citation(
            &turn.citations[0],
            1,
            "meeting",
            "Launch planning",
            "/tmp/localflow-ask-launch.md#01:20",
        );
    }

    #[test]
    fn snapshot_fixtures_match_python_transient_state() {
        let page = AskPage::new();
        let sources = SOURCES
            .iter()
            .map(|(key, _)| (*key).to_string())
            .collect::<BTreeSet<_>>();
        for snapshot in [
            AskSnapshot::Empty,
            AskSnapshot::Conversation,
            AskSnapshot::Thinking,
            AskSnapshot::Error,
            AskSnapshot::LongConversation,
            AskSnapshot::Citations,
        ] {
            {
                let mut state = page.state.borrow_mut();
                state.draft = "stale draft".into();
                state.time_filter = TIME_FILTERS[1].0.into();
                state.sources.clear();
                state.busy = true;
                state.pending = "stale pending question".into();
                state.focus = false;
                state.selection = Some((1, 2));
                state.scroll_y = Some(42.0);
                state.was_at_bottom = false;
                state.follow = true;
            }

            page.seed_snapshot(snapshot);
            let state = page.state.borrow();
            assert!(state.loaded, "{snapshot:?}");
            assert_eq!(
                state.turns.len(),
                match snapshot {
                    AskSnapshot::Empty | AskSnapshot::Thinking => 0,
                    AskSnapshot::LongConversation => 4,
                    _ => 1,
                },
                "{snapshot:?}"
            );
            assert!(state.draft.is_empty(), "{snapshot:?}");
            assert_eq!(state.time_filter, TIME_FILTERS[0].0, "{snapshot:?}");
            assert_eq!(state.sources, sources, "{snapshot:?}");
            assert_eq!(
                state.busy,
                snapshot == AskSnapshot::Thinking,
                "{snapshot:?}"
            );
            assert_eq!(
                state.pending,
                if snapshot == AskSnapshot::Thinking {
                    "And who owns the rollout checklist?"
                } else {
                    ""
                },
                "{snapshot:?}"
            );
            assert!(state.focus, "{snapshot:?}");
            assert_eq!(state.selection, None, "{snapshot:?}");
            assert_eq!(state.scroll_y, None, "{snapshot:?}");
            assert!(state.was_at_bottom, "{snapshot:?}");
            assert!(!state.follow, "{snapshot:?}");
        }
    }

    #[test]
    fn populated_and_error_fixtures_contain_real_conversation_content() {
        let page = AskPage::new();
        page.seed_snapshot(AskSnapshot::Populated);
        {
            let state = page.state.borrow();
            assert_eq!(state.turns.len(), 2);
            assert_eq!(
                state.turns.iter().map(|t| t.citations.len()).sum::<usize>(),
                3
            );
            let (items, citations) = conversation_items(&state.turns, state.busy, &state.pending);
            assert_eq!(citations.len(), 3);
            assert!(items.iter().any(|item| matches!(
                item,
                ConversationItem::Citations(labels) if labels.iter().all(|label| !label.is_empty())
            )));
        }
        let generation = page.state.borrow().generation;
        page.seed_snapshot(AskSnapshot::Error);
        let state = page.state.borrow();
        assert_ne!(generation, state.generation);
        assert_eq!(state.turns.len(), 1);
        assert_eq!(
            state.turns[0].question,
            "What did we decide about the launch?"
        );
        assert!(state.turns[0].answer.is_empty());
        assert!(state.turns[0].citations.is_empty());
        assert_eq!(state.turns[0].error, ERROR_UNREACHABLE);
        let (items, _) = conversation_items(&state.turns, state.busy, &state.pending);
        assert!(items
            .iter()
            .any(|item| matches!(item, ConversationItem::Assistant { is_error: true, .. })));
    }

    #[test]
    fn generation_gate_rejects_stale_and_shutdown_completions() {
        let mut page = AskPage::new();
        page.seed_snapshot(AskSnapshot::Empty);
        {
            let mut state = page.state.borrow_mut();
            state.busy = true;
            state.pending = "question".into();
        }
        let current = page.state.borrow().generation;
        assert!(page.state.borrow().accepts(current));
        assert!(!page.state.borrow().accepts(current.wrapping_add(1)));
        page.seed_snapshot(AskSnapshot::Populated);
        {
            let mut state = page.state.borrow_mut();
            assert!(!state.accepts(current));
            // Isolate the generation check from the normal busy check.
            state.busy = true;
            assert!(!state.accepts(current));
            assert!(state.accepts(state.generation));
        }
        page.shutdown();
        let state = page.state.borrow();
        assert!(state.closed && state.timer.is_none() && state.receiver.is_none());
        assert!(!state.accepts(state.generation));
    }

    #[test]
    fn backend_runs_on_a_worker_and_returns_its_turns() {
        let called_on = Arc::new(Mutex::new(None));
        let backend = ThreadBackend {
            turns: Vec::new(),
            called_on: called_on.clone(),
        };
        let work = Work {
            generation: 41,
            backend: Box::new(backend),
            question: "When is launch?".into(),
            time_filter: "Any time".into(),
            sources: SOURCES.iter().map(|(key, _)| (*key).to_string()).collect(),
        };
        let (sender, receiver) = mpsc::channel();
        assert!(launch(work, sender).is_ok());
        let completion = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(completion.generation, 41);
        assert_eq!(completion.turns.len(), 1);
        assert_eq!(completion.turns[0].answer, "worker answer");
        let called_on = called_on.lock().unwrap().unwrap();
        assert_ne!(called_on, std::thread::current().id());
    }

    #[test]
    fn pending_turn_does_not_show_empty_state_examples() {
        let (items, citations) = conversation_items(&[], true, "When is launch?");
        assert!(citations.is_empty());
        assert!(matches!(&items[0], ConversationItem::User(text) if text == "When is launch?"));
        assert!(matches!(&items[1], ConversationItem::Assistant { text, .. } if text == THINKING));
    }
}
