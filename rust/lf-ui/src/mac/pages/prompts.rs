//! Prompts page (port of `_page_prompts`, `_prompt_row`, `_review_prompt`,
//! `reloadPrompts_`, `openMeetings_` and the "See Hot Phrases" teaser of
//! `localflow/hub_mac.py`).
//!
//! Three sections: the prompt shortcuts read from the live config, the meeting
//! work prompts awaiting approval (read from the injected [`PromptQueue`]) and
//! a teaser linking to Hot Phrases. Approve / Reject go through the guarded
//! compare-and-set helpers ([`review_prompt`]); a refusal is shown inline at
//! the top of the page (Prompts never uses the shell callout). Dispatching an
//! approved prompt is CLI-only in Python and is not offered here either.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAccessibility, NSView};
use objc2_foundation::{NSPoint, NSString};

use crate::backends::prompts::{PromptEntry, PromptQueue};
use crate::hub_model::{
    prompt_entries, prompt_row_layout, prompt_rows, prompt_status_pill, review_prompt,
    shortcut_rows, PromptRow, ReviewAction, PROMPTS_EMPTY, PROMPTS_UNREADABLE,
};
use crate::mac::actions::Timer;
use crate::mac::page::{HubPage, PageCtx};
use crate::mac::style::{
    color, hide_from_accessibility, make_pill, rect, style_surface, ButtonKind, FlippedView,
    SurfaceStyle,
};
use crate::mac::widgets::{button, label, LabelStyle};
use crate::tokens::{Palette, Weight, CARD_PADDING};

const LOADING: &str = "Loading prompts…";

type Entries = Result<Vec<PromptEntry>, &'static str>;

struct Completion {
    generation: u64,
    entries: Entries,
    error: Option<String>,
}

struct State {
    entries: RefCell<Option<Entries>>,
    error: RefCell<Option<String>>,
    busy: Cell<bool>,
    visible: Cell<bool>,
    closed: Cell<bool>,
    generation: Cell<u64>,
    sender: mpsc::Sender<Completion>,
    receiver: mpsc::Receiver<Completion>,
    timer: RefCell<Option<Timer>>,
    rebuild: RefCell<Option<Rc<dyn Fn()>>>,
}

impl State {
    fn new() -> Rc<Self> {
        let (sender, receiver) = mpsc::channel();
        Rc::new(Self {
            entries: RefCell::new(None),
            error: RefCell::new(None),
            busy: Cell::new(false),
            visible: Cell::new(false),
            closed: Cell::new(false),
            generation: Cell::new(0),
            sender,
            receiver,
            timer: RefCell::new(None),
            rebuild: RefCell::new(None),
        })
    }

    fn start(&self, queue: Arc<dyn PromptQueue>, action: Option<(String, ReviewAction)>) {
        if self.closed.get() || self.busy.replace(true) {
            return;
        }
        self.entries.borrow_mut().take();
        self.error.borrow_mut().take();
        let generation = self.generation.get();
        let sender = self.sender.clone();
        let pending_write = action
            .as_ref()
            .map(|_| crate::shutdown::register_pending_write());
        if thread::Builder::new()
            .name("localflow-prompts".into())
            .spawn(move || {
                let error = action.and_then(|(id, action)| review_prompt(&*queue, &id, action));
                drop(pending_write);
                let entries = prompt_entries(&*queue);
                let _ = sender.send(Completion {
                    generation,
                    entries,
                    error,
                });
            })
            .is_err()
        {
            self.busy.set(false);
            *self.entries.borrow_mut() = Some(Err(PROMPTS_UNREADABLE));
        }
    }

    fn apply(&self, completion: Completion) {
        if self.closed.get() || completion.generation != self.generation.get() {
            return;
        }
        self.busy.set(false);
        *self.entries.borrow_mut() = Some(completion.entries);
        *self.error.borrow_mut() = completion.error;
        if self.visible.get() {
            if let Some(rebuild) = self.rebuild.borrow().as_ref() {
                rebuild();
            }
        }
    }

    fn drain(&self) {
        while let Ok(completion) = self.receiver.try_recv() {
            self.apply(completion);
        }
        if !self.visible.get() && !self.busy.get() {
            self.timer.borrow_mut().take();
        }
    }

    fn shutdown(&self) {
        self.closed.set(true);
        self.visible.set(false);
        self.generation.set(self.generation.get().wrapping_add(1));
        self.timer.borrow_mut().take();
        self.rebuild.borrow_mut().take();
    }
}

/// Queue operations run on workers; only completions mutate the rendered state.
pub struct PromptsPage {
    state: Rc<State>,
}

impl Default for PromptsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptsPage {
    pub fn new() -> Self {
        Self {
            state: State::new(),
        }
    }
}

fn card_label(
    mtm: MainThreadMarker,
    card: &FlippedView,
    text: &str,
    y: f64,
    style: LabelStyle,
    palette: &Palette,
) {
    let width = card.frame().size.width - 2.0 * f64::from(CARD_PADDING);
    card.addSubview(&label(
        mtm,
        text,
        f64::from(CARD_PADDING),
        y,
        width,
        style,
        palette,
    ));
}

/// `_empty_state`: a title, wrapped detail and one action button.
#[allow(clippy::too_many_arguments)]
fn empty_state(
    ctx: &PageCtx,
    y: f64,
    height: f64,
    title: &str,
    detail: &str,
    action: &str,
    on_action: impl Fn() + 'static,
) {
    let mtm = ctx.mtm;
    let p = &ctx.palette;
    let state = FlippedView::new(mtm, rect(0.0, y, ctx.width, height));
    let w = ctx.width - 48.0;
    state.addSubview(&label(
        mtm,
        title,
        24.0,
        58.0,
        w,
        LabelStyle {
            size: 15.0,
            height: 28.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        },
        p,
    ));
    state.addSubview(&label(
        mtm,
        detail,
        24.0,
        92.0,
        w,
        LabelStyle {
            size: 13.0,
            color: "muted",
            height: 44.0,
            wrap: true,
            ..LabelStyle::default()
        },
        p,
    ));
    let btn = button(
        mtm,
        action,
        24.0,
        140.0,
        138.0,
        36.0,
        ButtonKind::Secondary,
        p,
    );
    ctx.target.on_action(&btn, on_action);
    state.addSubview(&btn);
    ctx.body.addSubview(&state);
}

impl PromptsPage {
    /// One queue row at `y` inside `listing`; returns its height.
    fn row(&self, ctx: &PageCtx, listing: &FlippedView, row: &PromptRow, y: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = &ctx.palette;
        let (tint, text_token) = prompt_status_pill(&row.status).unwrap_or(("line", "muted"));
        let pill = make_pill(mtm, &row.status_label, tint, text_token, p);
        let pill_width = pill.frame().size.width;
        let geometry = prompt_row_layout(
            ctx.width as i32,
            row,
            pill_width.ceil() as i32,
            Some(&*ctx.measure),
        );

        let mut x = f64::from(geometry.action_x);
        let mut actions: Vec<(&str, f64, ButtonKind, ReviewAction)> = Vec::new();
        if row.can_approve {
            actions.push(("Approve", 88.0, ButtonKind::Primary, ReviewAction::Approve));
        }
        if row.can_reject {
            actions.push((
                "Reject",
                80.0,
                ButtonKind::DestructiveGhost,
                ReviewAction::Reject,
            ));
        }
        for (title, width, kind, action) in actions {
            let btn = button(mtm, title, x, y + 16.0, width, 36.0, kind, p);
            btn.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "{title} prompt: {}",
                row.title
            ))));
            let services = ctx.services.clone();
            let weak = Rc::downgrade(&self.state);
            let rebuild = ctx.rebuild.clone();
            let id = row.id.clone();
            ctx.target.on_action(&btn, move || {
                if let Some(state) = weak.upgrade() {
                    services.clear_page_error("Prompts");
                    state.start(services.prompt_queue.clone(), Some((id.clone(), action)));
                    rebuild();
                }
            });
            listing.addSubview(&btn);
            x += width + 8.0;
        }
        pill.setFrameOrigin(NSPoint::new(f64::from(geometry.pill_x), y + 22.0));
        listing.addSubview(&pill);
        let text_width = f64::from(geometry.text_width);
        listing.addSubview(&label(
            mtm,
            &row.title,
            14.0,
            y + 12.0,
            text_width,
            LabelStyle {
                size: 13.0,
                height: 20.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            p,
        ));
        listing.addSubview(&label(
            mtm,
            &row.context,
            14.0,
            y + 36.0,
            text_width,
            LabelStyle {
                size: 11.0,
                color: if row.error.is_empty() { "muted" } else { "red" },
                height: f64::from(geometry.context_h),
                wrap: true,
                ..LabelStyle::default()
            },
            p,
        ));
        f64::from(geometry.height)
    }
}

impl HubPage for PromptsPage {
    fn name(&self) -> &'static str {
        "Prompts"
    }

    fn drain_requests(&mut self) -> usize {
        self.state.drain();
        self.pending_requests()
    }

    fn pending_requests(&self) -> usize {
        usize::from(self.state.busy.get() && !self.state.closed.get())
    }

    fn flush(&mut self) {
        self.state.visible.set(false);
        if !self.state.busy.get() {
            self.state.timer.borrow_mut().take();
        }
    }

    fn shutdown(&mut self) {
        self.state.shutdown();
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        *self.state.rebuild.borrow_mut() = Some(ctx.rebuild.clone());
        if self.state.timer.borrow().is_none() && !self.state.closed.get() {
            let weak = Rc::downgrade(&self.state);
            *self.state.timer.borrow_mut() = Some(Timer::schedule(0.05, true, move || {
                if let Some(state) = weak.upgrade() {
                    state.drain();
                }
            }));
        }
        if !self.state.visible.replace(true) {
            self.state.start(ctx.services.prompt_queue.clone(), None);
        }
        let mtm = ctx.mtm;
        let p = ctx.palette;
        let width = ctx.width;
        let inner = width - 2.0 * f64::from(CARD_PADDING);
        let mut cursor = top;

        // Inline error from the last approve/reject (never the shell callout).
        if let Some(error) = self
            .state
            .error
            .borrow()
            .clone()
            .or_else(|| ctx.services.page_error("Prompts"))
        {
            ctx.body.addSubview(&label(
                mtm,
                &error,
                0.0,
                cursor,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 22.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
            cursor += 30.0;
        }

        // Section 1: prompt shortcuts, read from the current config.
        let shortcuts = shortcut_rows(&ctx.services.config());
        let card_height = 76.0 + shortcuts.len() as f64 * 70.0;
        let card = FlippedView::new(mtm, rect(0.0, cursor, width, card_height));
        style_surface(&card, &p, &SurfaceStyle::default());
        ctx.body.addSubview(&card);
        card_label(
            mtm,
            &card,
            "Prompt shortcuts",
            f64::from(CARD_PADDING),
            LabelStyle {
                size: 15.0,
                weight: Weight::Semibold,
                height: 22.0,
                ..LabelStyle::default()
            },
            &p,
        );
        card_label(
            mtm,
            &card,
            "Select rough text in any app, then press a shortcut.",
            f64::from(CARD_PADDING) + 24.0,
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 18.0,
                ..LabelStyle::default()
            },
            &p,
        );
        for (index, row) in shortcuts.iter().enumerate() {
            let y = 64.0 + index as f64 * 70.0;
            // The hotkey pill shares the name row; the explanation gets the
            // full card width below it and wraps instead of truncating.
            let pill = make_pill(mtm, &row.hotkey, "line", "ink", &p);
            let pill_width = pill.frame().size.width;
            pill.setFrameOrigin(NSPoint::new(
                width - f64::from(CARD_PADDING) - pill_width,
                y,
            ));
            card.addSubview(&pill);
            card.addSubview(&label(
                mtm,
                row.name,
                f64::from(CARD_PADDING),
                y + 2.0,
                inner - pill_width - 12.0,
                LabelStyle {
                    size: 13.0,
                    height: 20.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                &p,
            ));
            card.addSubview(&label(
                mtm,
                row.detail,
                f64::from(CARD_PADDING),
                y + 28.0,
                inner,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 34.0,
                    wrap: true,
                    ..LabelStyle::default()
                },
                &p,
            ));
        }
        cursor += card_height + 12.0;

        // Section 2: meeting work prompts awaiting approval.
        ctx.body.addSubview(&label(
            mtm,
            "Meeting work prompts",
            0.0,
            cursor,
            width,
            LabelStyle {
                size: 15.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor += 22.0 + 8.0;
        match self.state.entries.borrow().clone() {
            None => {
                ctx.body.addSubview(&label(
                    mtm,
                    LOADING,
                    0.0,
                    cursor + 58.0,
                    width,
                    LabelStyle {
                        size: 15.0,
                        height: 28.0,
                        ..LabelStyle::default()
                    },
                    &p,
                ));
                cursor += 202.0;
            }
            Some(Err(unreadable)) => {
                let rebuild_services = ctx.services.clone();
                let weak = Rc::downgrade(&self.state);
                let rebuild = ctx.rebuild.clone();
                empty_state(
                    ctx,
                    cursor,
                    190.0,
                    "Can't read the queue",
                    unreadable,
                    "Try again",
                    move || {
                        // reloadPrompts_: drop a stale error and re-read.
                        rebuild_services.clear_page_error("Prompts");
                        if let Some(state) = weak.upgrade() {
                            state.start(rebuild_services.prompt_queue.clone(), None);
                            rebuild();
                        }
                    },
                );
                cursor += 202.0;
            }
            Some(Ok(entries)) => {
                let rows = prompt_rows(&entries);
                if rows.is_empty() {
                    let navigate = ctx.navigate.clone();
                    empty_state(
                        ctx,
                        cursor,
                        190.0,
                        "No prompts to review yet",
                        PROMPTS_EMPTY,
                        "Open Meetings",
                        move || navigate("Meetings"),
                    );
                    cursor += 202.0;
                } else {
                    let listing =
                        FlippedView::new(mtm, rect(0.0, cursor, width, rows.len() as f64 * 68.0));
                    style_surface(&listing, &p, &SurfaceStyle::default());
                    ctx.body.addSubview(&listing);
                    // Rows grow with their wrapped context line (a failure
                    // sentence is never cut off), so each row's top is the sum
                    // of those above.
                    let mut y = 0.0;
                    for (index, row) in rows.iter().enumerate() {
                        y += self.row(ctx, &listing, row, y);
                        if index + 1 < rows.len() {
                            listing.addSubview(&divider(mtm, y - 1.0, width, &p));
                        }
                    }
                    listing.setFrame(rect(0.0, cursor, width, y));
                    cursor += y + 12.0;
                }
            }
        }

        // Section 3: teaser linking to the Hot Phrases page.
        let teaser = FlippedView::new(mtm, rect(0.0, cursor, width, 64.0));
        style_surface(&teaser, &p, &SurfaceStyle::default());
        ctx.body.addSubview(&teaser);
        // The label stops before the button instead of running behind it.
        teaser.addSubview(&label(
            mtm,
            "Say a word, get a saved prompt",
            f64::from(CARD_PADDING),
            18.0,
            width - 3.0 * f64::from(CARD_PADDING) - 160.0,
            LabelStyle {
                size: 13.0,
                height: 28.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        let see = button(
            mtm,
            "See Hot Phrases",
            width - 160.0 - f64::from(CARD_PADDING),
            14.0,
            160.0,
            36.0,
            ButtonKind::Secondary,
            &p,
        );
        let navigate = ctx.navigate.clone();
        ctx.target.on_action(&see, move || navigate("Hot Phrases"));
        teaser.addSubview(&see);
        cursor + 64.0
    }
}

/// A 1pt `line`-coloured row divider (decorative: hidden from accessibility).
fn divider(mtm: MainThreadMarker, y: f64, width: f64, palette: &Palette) -> Retained<NSView> {
    let view = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(12.0, y, width - 24.0, 1.0));
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&color("line", palette).CGColor()));
    }
    hide_from_accessibility(&view);
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::prompts::{
        FakePromptQueue, QueueError, UnavailablePromptQueue, WorkPrompt,
    };
    use serde_json::{Map, Value};
    use std::sync::Mutex;
    use std::time::Duration;

    struct ObservedQueue {
        inner: FakePromptQueue,
        owner: thread::ThreadId,
        started: mpsc::Sender<thread::ThreadId>,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl ObservedQueue {
        fn observe(&self) {
            assert_ne!(thread::current().id(), self.owner);
        }
    }

    impl PromptQueue for ObservedQueue {
        fn add(
            &self,
            prompts: &[WorkPrompt],
            paths: &[String],
            title: &str,
        ) -> Result<Vec<PromptEntry>, QueueError> {
            self.observe();
            self.inner.add(prompts, paths, title)
        }
        fn list(&self, status: Option<&str>) -> Result<Vec<PromptEntry>, QueueError> {
            self.observe();
            let _ = self.started.send(thread::current().id());
            if let Some(gate) = self.gate.lock().unwrap().take() {
                gate.recv().unwrap();
            }
            self.inner.list(status)
        }
        fn get(&self, id: &str) -> Result<Option<PromptEntry>, QueueError> {
            self.observe();
            self.inner.get(id)
        }
        fn set_status(
            &self,
            id: &str,
            status: &str,
            expect: Option<&[&str]>,
            fields: &Map<String, Value>,
        ) -> Result<Option<PromptEntry>, QueueError> {
            self.observe();
            self.inner.set_status(id, status, expect, fields)
        }
    }

    fn receive(state: &State) -> Completion {
        state
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("queue worker completion")
    }

    #[test]
    fn blocked_queue_loading_leaves_the_caller_responsive_and_applies_on_delivery() {
        let (started, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let queue = Arc::new(ObservedQueue {
            inner: FakePromptQueue::new(),
            owner: thread::current().id(),
            started,
            gate: Mutex::new(Some(gate)),
        });
        let state = State::new();
        state.visible.set(true);
        let rebuilt = Rc::new(Cell::new(0));
        *state.rebuild.borrow_mut() = Some({
            let rebuilt = rebuilt.clone();
            Rc::new(move || rebuilt.set(rebuilt.get() + 1))
        });
        state.start(queue, None);
        assert_ne!(
            entered.recv_timeout(Duration::from_secs(2)).unwrap(),
            thread::current().id()
        );
        assert!(state.busy.get());
        assert!(state.entries.borrow().is_none());
        assert_eq!(rebuilt.get(), 0);
        let mut page = PromptsPage {
            state: state.clone(),
        };
        assert_eq!(page.drain_requests(), 1);
        release.send(()).unwrap();
        let completion = receive(&state);
        assert!(state.entries.borrow().is_none());
        state.apply(completion);
        assert!(!state.busy.get());
        assert_eq!(*state.entries.borrow(), Some(Ok(Vec::new())));
        assert_eq!(rebuilt.get(), 1);
    }

    #[test]
    fn approvals_rejections_and_stale_ids_are_guarded_on_the_worker() {
        for (action, status) in [
            (ReviewAction::Approve, "approved"),
            (ReviewAction::Reject, "rejected"),
        ] {
            let (started, _) = mpsc::channel();
            let queue = Arc::new(ObservedQueue {
                inner: FakePromptQueue::new(),
                owner: thread::current().id(),
                started,
                gate: Mutex::new(None),
            });
            let entries = queue
                .inner
                .add(
                    &[WorkPrompt::new("Review", "Review changes")],
                    &["/notes/prompt.md".into()],
                    "Meeting",
                )
                .unwrap();
            let id = entries[0]["id"].as_str().unwrap().to_string();
            let state = State::new();
            state.start(queue.clone(), Some((id.clone(), action)));
            state.apply(receive(&state));
            assert!(state.error.borrow().is_none());
            assert_eq!(
                state.entries.borrow().as_ref().unwrap().as_ref().unwrap()[0]["status"],
                status
            );
            state.start(queue.clone(), Some((id, action)));
            state.apply(receive(&state));
            assert!(state.error.borrow().as_ref().unwrap().contains("changed"));
            state.start(queue, Some(("missing".into(), action)));
            state.apply(receive(&state));
            assert!(state.error.borrow().as_ref().unwrap().contains("no longer"));
        }
    }

    #[test]
    fn queued_completion_is_delivered_without_a_timer() {
        for queue in [
            Arc::new(FakePromptQueue::new()) as Arc<dyn PromptQueue>,
            Arc::new(UnavailablePromptQueue),
        ] {
            let mut page = PromptsPage::new();
            page.state.start(queue, None);
            let completion = receive(&page.state);
            page.state.sender.send(completion).unwrap();
            assert!(page.state.timer.borrow().is_none());
            assert!(page.state.busy.get());
            assert_eq!(page.drain_requests(), 0);
            assert!(page.state.entries.borrow().is_some());
        }
    }

    #[test]
    fn queue_errors_and_closed_or_stale_completions_cannot_rebuild_the_ui() {
        let state = State::new();
        state.start(Arc::new(UnavailablePromptQueue), None);
        state.apply(receive(&state));
        assert_eq!(*state.entries.borrow(), Some(Err(PROMPTS_UNREADABLE)));
        state.start(Arc::new(FakePromptQueue::new()), None);
        let completion = receive(&state);
        state.shutdown();
        state.apply(completion);
        assert!(state.entries.borrow().is_none());
        let state = State::new();
        state.apply(Completion {
            generation: 1,
            entries: Ok(Vec::new()),
            error: None,
        });
        assert!(state.entries.borrow().is_none());
    }
}
