//! Agents: sessions LocalFlow handed to agents (design sections 6–8).
//!
//! Shown only while the service reports the feature on (the shell hides
//! the sidebar entry and refuses the route otherwise, like People). The
//! page reads `agents-api v1` through `controllers::agents::AgentsBackend`:
//! status, providers and the session list once, then `/changes` (a 25 s
//! long-poll in real mode) while the page is on screen, and a session's
//! detail and activity when it is selected.
//!
//! The detail pane draws only what v1 carries: header, state banner, the
//! task and its frozen receipt, Activity (events), Results (`result_text`;
//! v1 has no artifact rows, so none are drawn) and, when the provider takes
//! live messages and the session isn't final, the reply field. Actions are
//! capability-gated (`controllers::agents::actions`): an absent capability
//! is an absent control, and nothing is ever called that the provider
//! doesn't offer. Stop and Remove ask first (focus starts on Cancel).
//!
//! Activity is read once on selection and again whenever `/changes` shows
//! the session's `last_seq` moved; only a live, streaming session on a real
//! service holds an events long-poll, on its own thread, and only while
//! this page shows that session (at most [`MAX_LONG_POLLS`] at once).
//! Provider output is plain text, masked before it is stored; links in it
//! are never made clickable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, Entity, FocusHandle, FontWeight,
    KeyDownEvent, KeyUpEvent, ScrollHandle, SharedString, Window,
};

use crate::app::{Confirm, PageCx};
use crate::controllers::agents::{
    self as ctl, Activity, AgentsBackend, AgentsError, Changes, EventsPage, Filter, Results, Row,
    Store, UiState,
};
use crate::controllers::agents_launcher::{Draft, DraftStore};
use crate::controllers::history::time_label;
use crate::launcher::{spawn, Spawn};
use crate::page::Page;
use crate::pages::agents_poll::{backoff, blocking, LONG_POLL, MIN_PAUSE, POLL_PAUSE};
use crate::pages::{header, page, split};
use crate::remote::Health;
use crate::sandbox::Fixture;
use crate::theme::{size, space, theme, Theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::{self, Button, IconButton, Segmented};

/// Events long-polls allowed at once (one is the normal case; a second
/// covers the old one finishing after a selection change).
const MAX_LONG_POLLS: usize = 2;
/// Activity lines drawn (the newest; the log itself keeps
/// [`ctl::MAX_EVENTS`]).
const SHOWN_EVENTS: usize = 100;

/// Long-poll threads alive, process-wide.
static LONG_POLLS: AtomicUsize = AtomicUsize::new(0);

/// What the poll loop does next.
enum Next {
    Stop,
    Pause(Duration),
    Poll(Arc<dyn AgentsBackend>, u64, u32),
}

/// The selected session's detail read (its task text).
#[derive(Clone, Debug, Default, PartialEq)]
enum Detail {
    #[default]
    Idle,
    Loading,
    Failed(AgentsError),
}

/// The Task section's text when the session's task text isn't in hand.
fn task_placeholder(detail: &Detail) -> String {
    match detail {
        Detail::Loading => "Reading this task…".into(),
        Detail::Failed(AgentsError::Refused(reason)) => {
            format!("LocalFlow couldn't read this task: {reason}")
        }
        Detail::Failed(_) => {
            "LocalFlow couldn't read this task because its agent service isn't answering.".into()
        }
        Detail::Idle => "LocalFlow doesn't have this task's text.".into(),
    }
}

pub struct AgentsPage {
    pcx: PageCx,
    /// The service, re-resolved on every refresh (config may change).
    backend: Arc<dyn AgentsBackend>,
    search: Entity<TextInput>,
    filter: Filter,
    store: Store,
    loading: bool,
    selected: Option<String>,
    detail_generation: u64,
    detail: Detail,
    list_focus: FocusHandle,
    list_scroll: ScrollHandle,
    /// The selection moved by key: bring it into view on the next frame.
    reveal: bool,
    /// A `/changes` loop is running.
    polling: bool,
    /// Failed `/changes` reads in a row (the backoff).
    failures: u32,
    /// A fixture scene's selection, applied once the list has loaded.
    fixture: Option<Fixture>,
    /// The selected session's events.
    activity: Option<Activity>,
    events_generation: u64,
    events_reading: bool,
    /// Failed events reads in a row (the backoff).
    events_failures: u32,
    reply: Entity<TextInput>,
    reply_error: Option<String>,
    sending: bool,
    /// The `…` menu is open.
    more_open: bool,
    /// A stop or remove is in flight for this session.
    busy: Option<String>,
    /// Run again: always drawn, so Enter on a row can land focus here.
    run_again_focus: FocusHandle,
    more_focus: FocusHandle,
    /// The `…` menu's entries: Copy link, Copy ID, Remove from LocalFlow.
    menu_focus: [FocusHandle; 3],
    /// The `…` menu just opened: focus its first entry once drawn.
    menu_focus_pending: bool,
    /// Enter's key-down moved focus onto a control: its key-up must not
    /// click it (GPUI clicks a focused element on Enter's key-up).
    eat_enter_up: bool,
    /// A session asked for (`--agent-session`, the launcher's Cmd+Enter)
    /// that isn't listed yet: selected once it arrives.
    pending_open: Option<String>,
    /// Unsent reply text per session, so a reply typed for one session
    /// never shows (or is sent) under another.
    reply_drafts: std::collections::HashMap<String, String>,
    /// The reply field's `(placeholder, enabled)` last applied.
    reply_state: Option<(String, bool)>,
    /// Focus the reply field (or Run again) on the next frame.
    focus_pending: bool,
}

impl AgentsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search sessions").label("Search sessions"));
        cx.subscribe(&search, |_, _, event, cx| match event {
            InputEvent::Changed | InputEvent::Submit => cx.notify(),
        })
        .detach();
        let reply = cx.new(|cx| {
            TextInput::new(cx, ctl::reply_placeholder("the agent"))
                .multiline(1, 6)
                .submit_on_enter()
                .max_chars(20_000)
                .label("Reply")
        });
        cx.subscribe(&reply, |this: &mut Self, _, event, cx| match event {
            InputEvent::Submit => this.send_reply(cx),
            InputEvent::Changed => {
                this.reply_error = None;
                cx.notify();
            }
        })
        .detach();
        let backend = pcx.services.remote.agents(&pcx.services.config());
        let mut page = AgentsPage {
            pcx,
            backend,
            search,
            filter: Filter::All,
            store: Store::default(),
            loading: false,
            selected: None,
            detail_generation: 0,
            detail: Detail::Idle,
            list_focus: cx.focus_handle().tab_stop(true),
            list_scroll: ScrollHandle::new(),
            reveal: false,
            polling: false,
            failures: 0,
            fixture: None,
            activity: None,
            events_generation: 0,
            events_reading: false,
            events_failures: 0,
            reply,
            reply_error: None,
            sending: false,
            more_open: false,
            busy: None,
            run_again_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            menu_focus: [
                cx.focus_handle().tab_stop(true),
                cx.focus_handle().tab_stop(true),
                cx.focus_handle().tab_stop(true),
            ],
            menu_focus_pending: false,
            eat_enter_up: false,
            pending_open: None,
            reply_drafts: Default::default(),
            reply_state: None,
            focus_pending: false,
        };
        page.refresh(cx);
        page
    }

    fn shown(&self) -> bool {
        *self.pcx.current_page.borrow() == Page::Agents
    }

    fn now(&self) -> f64 {
        self.backend.now()
    }

    /// Nothing is sent: the in-process fake, or a service running mock
    /// providers (`agents.sandbox`).
    fn simulated(&self) -> bool {
        ctl::simulated(self.backend.simulated(), self.store.status.as_ref())
    }

    /// Read everything again and keep the list live, only while on screen.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.backend = self.pcx.services.remote.agents(&self.pcx.services.config());
        if !self.shown() {
            return;
        }
        self.load(cx);
        self.watch(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let generation = self.store.begin();
        let backend = self.backend.clone();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ctl::load(backend.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let now = this.now();
                if this.store.finish(generation, result, now) {
                    this.loading = false;
                    this.report_attention(cx);
                    this.loaded(cx);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// A full read landed: apply a fixture's selection, refresh the detail.
    fn loaded(&mut self, cx: &mut Context<Self>) {
        if self.adopt_pending_open(cx) {
            return;
        }
        let wanted = match self.fixture {
            Some(Fixture::AgentsPage) => Some(UiState::NeedsInput),
            Some(Fixture::AgentsDetail) => Some(UiState::Running),
            _ => None,
        };
        if let (Some(state), None) = (wanted, &self.selected) {
            let first = self
                .store
                .rows(self.now())
                .into_iter()
                .find(|r| r.state == state && r.caps.live_message == (state == UiState::Running))
                .map(|r| r.session.id);
            if let Some(id) = first {
                self.select(id, cx);
                return;
            }
        }
        if let Some(id) = self.selected.clone() {
            self.fetch_detail(id.clone(), cx);
            // Back on a session that moved meanwhile (finished while the
            // page was hidden): its Activity catches up now.
            let read = self.activity.as_ref().map_or(0, |a| a.after);
            if self.store.session(&id).is_some_and(|s| s.last_seq > read) {
                self.read_events(cx);
            }
        }
    }

    /// Keep reading `/changes` while the page is on screen. One loop at a
    /// time: it stops itself when the page is hidden, the service has no
    /// agents API, or the page goes away.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        let gate = self.pcx.agents_changes.clone();
        cx.spawn(async move |this, cx| loop {
            let next = this
                .update(cx, |this, _| this.next_poll())
                .unwrap_or(Next::Stop);
            let pause = match next {
                Next::Stop => break,
                Next::Pause(pause) => pause,
                Next::Poll(backend, since, wait) => {
                    // One `/changes` call per window: the shell's watcher
                    // may still hold one from before this page was shown
                    // (it answers as soon as anything changes).
                    let Some(ticket) = gate.enter() else {
                        cx.background_executor().timer(MIN_PAUSE).await;
                        continue;
                    };
                    // A long-poll holds its thread for up to 30 s: give it
                    // its own rather than parking a background-executor one.
                    let result = blocking(cx, wait > 0, move || backend.changes(since, wait)).await;
                    drop(ticket);
                    match this.update(cx, |this, cx| this.polled(result, cx)) {
                        Ok(Some(pause)) => pause,
                        _ => break,
                    }
                }
            };
            cx.background_executor().timer(pause).await;
        })
        .detach();
    }

    fn next_poll(&mut self) -> Next {
        if !self.shown() {
            self.polling = false;
            return Next::Stop;
        }
        if self.loading || !self.store.loaded {
            return Next::Pause(POLL_PAUSE);
        }
        let backend = self.backend.clone();
        let wait = if backend.simulated() { 0 } else { LONG_POLL };
        Next::Poll(backend, self.store.revision, wait)
    }

    /// Apply one `/changes` answer; returns the pause before the next, or
    /// `None` to stop (the service has no agents API: polling it again only
    /// turns that into a "couldn't read" error).
    fn polled(
        &mut self,
        result: Result<Changes, AgentsError>,
        cx: &mut Context<Self>,
    ) -> Option<Duration> {
        let pause = match result {
            Ok(changes) => {
                self.failures = 0;
                let held = changes.waited;
                let never_read = self.store.status.is_none();
                let before = self.selected_marks();
                if self.store.apply_changes(changes) || never_read {
                    self.load(cx);
                }
                self.report_attention(cx);
                if !self.adopt_pending_open(cx) {
                    self.follow_selected(before, cx);
                }
                Some(if held && !self.backend.simulated() {
                    MIN_PAUSE
                } else {
                    POLL_PAUSE
                })
            }
            Err(error) => {
                let absent = error == AgentsError::Absent;
                let now = self.now();
                self.store.failed(error, now);
                self.failures = self.failures.saturating_add(1);
                if absent {
                    self.polling = false;
                    None
                } else {
                    Some(backoff(self.failures))
                }
            }
        };
        self.sync_reply(cx);
        cx.notify();
        pause
    }

    /// While this page is shown it is the one `/changes` reader: it feeds
    /// the sidebar badge from its own list (the shell's watcher pauses).
    fn report_attention(&self, cx: &mut Context<Self>) {
        if !self.shown() {
            return;
        }
        let n = self
            .store
            .rows(self.now())
            .iter()
            .filter(|r| r.state == UiState::NeedsInput)
            .count();
        self.pcx.set_agents_attention(n, cx);
    }

    /// `(revision, last_seq)` of the selected session, to see what moved.
    fn selected_marks(&self) -> Option<(u64, u64)> {
        let s = self.store.session(self.selected.as_ref()?)?;
        Some((s.revision, s.last_seq))
    }

    /// The selected session changed at the service: read its detail again
    /// when its revision moved (the result arrives with the detail), and
    /// its events when its log grew. A removed session clears the detail.
    fn follow_selected(&mut self, before: Option<(u64, u64)>, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(after) = self.selected_marks() else {
            if before.is_some() {
                self.clear_selection(cx);
            }
            return;
        };
        if before.is_some_and(|(rev, _)| rev != after.0) {
            self.refetch_detail(id, cx);
        }
        let read = self.activity.as_ref().map_or(0, |a| a.after);
        if after.1 > read {
            self.read_events(cx);
        }
    }

    /// The reply field names the selected session's provider and is usable
    /// only while a reply can be sent (offline or in flight: disabled).
    /// Applied when it changes, never from inside a frame.
    fn sync_reply(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let state = (
            ctl::reply_placeholder(&row.provider),
            ctl::actions(&row).reply == Some(true) && !self.sending,
        );
        if self.reply_state.as_ref() != Some(&state) {
            let (placeholder, enabled) = state.clone();
            self.reply.update(cx, |r, cx| {
                r.set_placeholder(placeholder, cx);
                r.set_disabled(!enabled, cx);
            });
            self.reply_state = Some(state);
        }
    }

    /// Open on one session (the launcher's Enter on a session row): select
    /// it once listed, and focus its reply field when it has one.
    /// A session just launched may not be listed yet: it is selected when
    /// a full read or `/changes` brings it (review M1).
    pub fn open_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.filter = Filter::All;
        self.reveal = true;
        self.focus_pending = true;
        if self.store.session(&id).is_some() || !self.store.loaded {
            self.pending_open = None;
            self.select(id, cx);
        } else {
            self.pending_open = Some(id);
        }
        cx.notify();
    }

    /// Select the session asked for once it is listed; `true` when it was.
    fn adopt_pending_open(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self
            .pending_open
            .take_if(|id| self.store.session(id).is_some())
        else {
            return false;
        };
        self.reveal = true;
        self.select(id, cx);
        true
    }

    /// The user picked a session: forget a pending open and its focus move.
    fn select_by_user(&mut self, id: String, cx: &mut Context<Self>) {
        self.pending_open = None;
        self.focus_pending = false;
        self.select(id, cx);
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.activity = None;
        self.events_generation += 1;
        self.events_reading = false;
        self.more_open = false;
        cx.notify();
    }

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(id.as_str()) {
            // The reply field is per session: keep what was typed for the
            // old one, show what was typed for the new one.
            let typed = self.reply.read(cx).text().to_string();
            if let Some(old) = self.selected.take() {
                if typed.trim().is_empty() {
                    self.reply_drafts.remove(&old);
                } else {
                    self.reply_drafts.insert(old, typed);
                }
            }
            let restored = self.reply_drafts.remove(&id).unwrap_or_default();
            self.reply.update(cx, |r, cx| r.set_text(restored, cx));
            self.selected = Some(id.clone());
            self.more_open = false;
            self.reply_error = None;
            self.activity = Some(Activity::new(&id));
            self.events_generation += 1;
            self.events_reading = false;
            self.fetch_detail(id, cx);
            self.read_events(cx);
            self.sync_reply(cx);
        }
        cx.notify();
    }

    /// The full session (task text and result) for the detail pane.
    fn fetch_detail(&mut self, id: String, cx: &mut Context<Self>) {
        self.detail = Detail::Loading;
        self.refetch_detail(id, cx);
    }

    /// [`fetch_detail`] without the "Reading this task…" placeholder (the
    /// task is already in hand; only state or result moved).
    fn refetch_detail(&mut self, id: String, cx: &mut Context<Self>) {
        self.detail_generation += 1;
        let generation = self.detail_generation;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.session(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.detail_generation == generation {
                    this.detail = match result {
                        Ok(session) => {
                            this.store.apply_detail(session);
                            Detail::Idle
                        }
                        Err(error) => Detail::Failed(error),
                    };
                    this.sync_reply(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// May the selected session's events be long-polled right now? Only a
    /// live, streaming session on a real service, on screen, once its log
    /// was read at least once.
    fn may_hold_events(&self) -> bool {
        let Some(id) = &self.selected else {
            return false;
        };
        let streaming = self.store.session(id).is_some_and(|s| {
            !s.runtime.terminal()
                && self
                    .store
                    .provider(&s.provider, &s.profile)
                    .is_some_and(|p| p.caps.streaming)
        });
        streaming
            && self.shown()
            && !self.backend.simulated()
            && self.activity.as_ref().is_some_and(|a| a.loaded && !a.done)
            && LONG_POLLS.load(Ordering::Relaxed) < MAX_LONG_POLLS
    }

    /// Read the selected session's events after the cursor. One read at a
    /// time; an answer for another selection is dropped.
    fn read_events(&mut self, cx: &mut Context<Self>) {
        if self.events_reading {
            return;
        }
        let (Some(id), Some(activity)) = (self.selected.clone(), self.activity.as_ref()) else {
            return;
        };
        let after = activity.after;
        let wait = if self.may_hold_events() { LONG_POLL } else { 0 };
        let generation = self.events_generation;
        let backend = self.backend.clone();
        self.events_reading = true;
        cx.spawn(async move |this, cx| {
            let held = wait > 0;
            if held {
                LONG_POLLS.fetch_add(1, Ordering::Relaxed);
            }
            let result = blocking(cx, held, move || {
                backend.events(&id, after, ctl::EVENTS_LIMIT, wait)
            })
            .await;
            if held {
                LONG_POLLS.fetch_sub(1, Ordering::Relaxed);
            }
            let again = this
                .update(cx, |this, cx| this.events_read(generation, result, cx))
                .ok()
                .flatten();
            if let Some(pause) = again {
                cx.background_executor().timer(pause).await;
                let _ = this.update(cx, |this, cx| {
                    if this.events_generation == generation {
                        this.read_events(cx);
                    }
                });
            }
        })
        .detach();
    }

    /// One events read landed; the pause before the next, if one follows.
    fn events_read(
        &mut self,
        generation: u64,
        result: Result<EventsPage, AgentsError>,
        cx: &mut Context<Self>,
    ) -> Option<Duration> {
        if generation != self.events_generation {
            return None;
        }
        self.events_reading = false;
        let page = match result {
            Ok(page) => {
                self.events_failures = 0;
                page
            }
            // Try again, backing off like `/changes` (a 404: the session
            // went away; the list's next read says so).
            Err(AgentsError::Absent) => return None,
            Err(_) => {
                self.events_failures = self.events_failures.saturating_add(1);
                return Some(backoff(self.events_failures));
            }
        };
        let waited = page.waited;
        let more = self.activity.as_mut()?.apply(page);
        cx.notify();
        if more {
            Some(Duration::ZERO)
        } else if self.may_hold_events() {
            // `waited: false` after asking to wait: don't spin.
            Some(if waited { MIN_PAUSE } else { POLL_PAUSE })
        } else {
            None
        }
    }

    /// `--sandbox-fixture agents-page|agents-states|agents-detail`.
    pub fn run_fixture(&mut self, fixture: Fixture, cx: &mut Context<Self>) {
        self.fixture = Some(fixture);
        if fixture == Fixture::AgentsStates {
            if let Some(fake) = self.pcx.services.remote.sandbox_agents() {
                fake.add_state_examples();
            }
        }
        self.load(cx);
    }

    fn query(&self, cx: &gpui::App) -> String {
        self.search.read(cx).text().to_string()
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.read(cx).focus(window);
    }

    /// Esc: close the `…` menu; clear the search; from the detail pane,
    /// back to the list. `false` when there was nothing to do.
    pub fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.more_open {
            self.more_open = false;
            window.focus(&self.more_focus);
            cx.notify();
            return true;
        }
        if !self.search.read(cx).is_empty() {
            self.search.update(cx, |s, cx| s.set_text("", cx));
            window.focus(&self.list_focus);
            cx.notify();
            return true;
        }
        let in_detail = self.reply.read(cx).is_focused(window)
            || self.run_again_focus.is_focused(window)
            || self.more_focus.is_focused(window);
        if in_detail {
            window.focus(&self.list_focus);
            cx.notify();
            return true;
        }
        false
    }

    /// The rows as drawn (filter and search applied), flattened.
    pub(crate) fn visible_ids(&self, cx: &gpui::App) -> Vec<String> {
        let rows = self.store.rows(self.now());
        let query = self.query(cx);
        ctl::grouped(&rows, self.filter, &query)
            .into_iter()
            .flat_map(|(_, rows)| rows.into_iter().map(|r| r.session.id.clone()))
            .collect()
    }

    fn selected_row(&self) -> Option<Row> {
        let id = self.selected.as_ref()?;
        self.store
            .rows(self.now())
            .into_iter()
            .find(|r| &r.session.id == id)
    }

    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.secondary() {
            match keystroke.key.as_str() {
                // Open in provider; copy the link (or the ID without one).
                "o" => {
                    if let Some(url) = self.selected_row().and_then(|r| ctl::actions(&r).open) {
                        cx.open_url(&url);
                        cx.stop_propagation();
                    }
                }
                "c" => {
                    if let Some(row) = self.selected_row() {
                        match ctl::actions(&row).open {
                            Some(url) => self.copy(url, "Link copied", cx),
                            None => self.copy(row.session.id, "Session ID copied", cx),
                        }
                        cx.stop_propagation();
                    }
                }
                _ => {}
            }
            return;
        }
        if keystroke.key == "enter" {
            // Into the detail: the reply field when there is one, else
            // the first action. This Enter's key-up must not then click
            // Run again.
            if let Some(row) = self.selected_row() {
                if ctl::actions(&row).reply == Some(true) {
                    self.reply.read(cx).focus(window);
                } else {
                    self.eat_enter_up = true;
                    window.focus(&self.run_again_focus);
                }
                cx.stop_propagation();
            }
            return;
        }
        let ids = self.visible_ids(cx);
        if ids.is_empty() {
            return;
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id));
        let last = ids.len() - 1;
        let next = match keystroke.key.as_str() {
            "down" => index.map_or(0, |i| (i + 1).min(last)),
            "up" => index.map_or(0, |i| i.saturating_sub(1)),
            "home" => 0,
            "end" => last,
            _ => return,
        };
        self.reveal = true;
        self.select_by_user(ids[next].clone(), cx);
        cx.stop_propagation();
    }

    /// The `…` menu's entries drawn for `row`, in order (their focus
    /// handles' indexes).
    fn menu_entries(row: &Row) -> Vec<usize> {
        let actions = ctl::actions(row);
        let mut entries = Vec::new();
        if actions.open.is_some() {
            entries.push(0);
        }
        entries.push(1);
        if actions.remove {
            entries.push(2);
        }
        entries
    }

    /// Up/Down/Home/End inside the `…` menu (Tab also moves; Esc closes).
    fn on_menu_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let entries = Self::menu_entries(&row);
        let at = entries
            .iter()
            .position(|&i| self.menu_focus[i].is_focused(window));
        let last = entries.len() - 1;
        let next = match (event.keystroke.key.as_str(), at) {
            ("down", Some(i)) => (i + 1).min(last),
            ("up", Some(i)) => i.saturating_sub(1),
            ("down" | "home", None) | ("home", _) => 0,
            ("up" | "end", None) | ("end", _) => last,
            _ => return,
        };
        window.focus(&self.menu_focus[entries[next]]);
        cx.stop_propagation();
        cx.notify();
    }

    /// `…`: open or close the menu; opening moves focus to its first entry.
    fn toggle_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.more_open = !self.more_open;
        self.menu_focus_pending = self.more_open;
        if !self.more_open {
            window.focus(&self.more_focus);
        }
        cx.notify();
    }

    // --------------------------------------------------------- actions ---

    fn copy(&mut self, text: String, toast: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.more_open = false;
        self.pcx.toast(toast, cx);
        cx.notify();
    }

    /// New task (`Cmd/Ctrl+N`, the header button): open the launcher, its
    /// own window. `draft` prefills it (Run again).
    pub fn new_task(&mut self, draft: Option<Draft>, cx: &mut Context<Self>) {
        let root = self.pcx.services.root.clone();
        let prefill = draft.is_some();
        let mut replaced = false;
        if let Some(draft) = draft {
            let drafts = DraftStore::new(&crate::controllers::agents_launcher::launcher_dir(&root));
            // An unsent launcher task is replaced, and the toast says so.
            replaced = drafts
                .load(ctl::now())
                .is_some_and(|old| old.text.trim() != draft.text.trim());
            drafts.save(&draft);
        }
        if spawn(&root, Spawn::Launcher { prefill }).is_err() {
            self.pcx
                .toast("LocalFlow couldn't open the agent launcher.", cx);
        } else if replaced {
            self.pcx
                .toast("Replaced the unsent task in the launcher", cx);
        }
    }

    /// Run again: the launcher, prefilled with the task, provider and
    /// workspace, as a new draft (v1 has no context to re-attach).
    fn run_again(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(prompt) = row.session.prompt.clone() else {
            self.pcx.toast(task_placeholder(&self.detail), cx);
            return;
        };
        self.more_open = false;
        let draft = Draft {
            text: prompt,
            provider: Some((row.session.provider.clone(), row.session.profile.clone())),
            workspace: row.session.repo.clone(),
            saved_at: ctl::now(),
        };
        self.new_task(Some(draft), cx);
    }

    fn ask_stop(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if ctl::actions(&row).stop != Some(true) {
            return;
        }
        let (title, body, verb) = ctl::stop_confirm(&row);
        let this = cx.entity().downgrade();
        let id = row.session.id.clone();
        self.pcx.confirm(
            Confirm {
                title: title.into(),
                body: body.into(),
                verb: verb.into(),
                destructive: true,
                on_confirm: std::rc::Rc::new(move |_, cx| {
                    let id = id.clone();
                    let _ = this.update(cx, |p, cx| p.stop(id, cx));
                }),
            },
            cx,
        );
    }

    /// `POST …/cancel`, only for a provider that can cancel.
    pub(crate) fn stop(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().filter(|r| r.session.id == id) else {
            return;
        };
        if ctl::actions(&row).stop != Some(true) || self.busy.is_some() {
            return;
        }
        self.busy = Some(id.clone());
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.cancel(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(session) => {
                        // A provider may take a while to wind down: say
                        // "stopped" only once it has.
                        let stopped = session.runtime.terminal();
                        this.store.apply_session(session);
                        this.sync_reply(cx);
                        this.pcx.toast(
                            if stopped {
                                "Session stopped"
                            } else {
                                "Stopping the session…"
                            },
                            cx,
                        );
                    }
                    Err(error) => this.pcx.toast(ctl::stop_failed(&row, &error), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn ask_remove(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().filter(|r| ctl::actions(r).remove) else {
            return;
        };
        self.more_open = false;
        let (title, body, verb) = ctl::remove_confirm(&row);
        let this = cx.entity().downgrade();
        let id = row.session.id.clone();
        self.pcx.confirm(
            Confirm {
                title: title.into(),
                body: body.into(),
                verb: verb.into(),
                destructive: true,
                on_confirm: std::rc::Rc::new(move |_, cx| {
                    let id = id.clone();
                    let _ = this.update(cx, |p, cx| p.remove(id, cx));
                }),
            },
            cx,
        );
        cx.notify();
    }

    /// `DELETE …` (final sessions only). Local only: the copy says so.
    pub(crate) fn remove(&mut self, id: String, cx: &mut Context<Self>) {
        let final_row = self
            .store
            .rows(self.now())
            .into_iter()
            .any(|r| r.session.id == id && ctl::actions(&r).remove);
        if !final_row || self.busy.is_some() {
            return;
        }
        self.busy = Some(id.clone());
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let target = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { backend.remove(&target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.store.remove(&id);
                        if this.selected.as_deref() == Some(id.as_str()) {
                            this.clear_selection(cx);
                        }
                        this.pcx.toast("Removed from LocalFlow", cx);
                    }
                    Err(error) => this.pcx.toast(ctl::remove_failed(&error), cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Enter in the reply field: `POST …/messages` with this text only.
    /// A failure keeps the text; success shows it as a "You" line and
    /// keeps focus in the field.
    fn send_reply(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let text = self.reply.read(cx).text().to_string();
        if self.sending || text.trim().is_empty() || ctl::actions(&row).reply != Some(true) {
            return;
        }
        self.sending = true;
        self.reply_error = None;
        self.reply.update(cx, |r, cx| r.set_disabled(true, cx));
        self.reply_state = None;
        let backend = self.backend.clone();
        let id = row.session.id.clone();
        let provider = row.provider.clone();
        cx.spawn(async move |this, cx| {
            let target = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { backend.message(&target, &text) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sending = false;
                // The field shows another session's draft if the selection
                // moved meanwhile; this reply's text is then in its draft.
                let here = this.selected.as_deref() == Some(id.as_str());
                match result {
                    Ok((session, event)) => {
                        this.store.apply_session(session);
                        if let Some(a) = this.activity.as_mut().filter(|a| a.session == id) {
                            a.push(event);
                        }
                        if here {
                            // Anything written before the reply, unread
                            // yet, is read now (the cursor didn't move).
                            this.read_events(cx);
                            this.reply.update(cx, |r, cx| r.clear(cx));
                        } else {
                            this.reply_drafts.remove(&id);
                        }
                        this.pcx.toast(format!("Reply sent to {provider}"), cx);
                    }
                    Err(error) if here => this.reply_error = Some(ctl::reply_error(&error)),
                    Err(error) => this.pcx.toast(ctl::reply_error(&error), cx),
                }
                this.sync_reply(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------ test hooks ---

    #[cfg(test)]
    pub(crate) fn list_scroll(&self) -> ScrollHandle {
        self.list_scroll.clone()
    }

    #[cfg(test)]
    pub(crate) fn task_text(&self) -> String {
        let row = self.selected.as_ref().and_then(|id| self.store.session(id));
        match row.and_then(|s| s.prompt.clone()) {
            Some(prompt) => prompt,
            None => task_placeholder(&self.detail),
        }
    }

    #[cfg(test)]
    pub(crate) fn polling(&self) -> bool {
        self.polling
    }

    /// The session list's focus handle, for shell tests.
    #[cfg(test)]
    pub(crate) fn list_focus(&self) -> FocusHandle {
        self.list_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn run_again_focus(&self) -> FocusHandle {
        self.run_again_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn more_focus(&self) -> FocusHandle {
        self.more_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn menu_focus(&self) -> [FocusHandle; 3] {
        self.menu_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn more_open(&self) -> bool {
        self.more_open
    }

    #[cfg(test)]
    pub(crate) fn read_events_for_test(&mut self, cx: &mut Context<Self>) {
        self.read_events(cx);
    }

    #[cfg(test)]
    pub(crate) fn send_reply_for_test(&mut self, cx: &mut Context<Self>) {
        self.send_reply(cx);
    }

    #[cfg(test)]
    pub(crate) fn select_by_user_for_test(&mut self, id: &str, cx: &mut Context<Self>) {
        self.select_by_user(id.to_string(), cx);
    }

    #[cfg(test)]
    pub(crate) fn reply_input(&self) -> Entity<TextInput> {
        self.reply.clone()
    }

    #[cfg(test)]
    pub(crate) fn reply_error(&self) -> Option<String> {
        self.reply_error.clone()
    }

    #[cfg(test)]
    pub(crate) fn activity(&self) -> Option<&Activity> {
        self.activity.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<String> {
        self.selected.clone()
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    #[cfg(test)]
    pub(crate) fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn search_text(&self, cx: &gpui::App) -> String {
        self.search.read(cx).text().to_string()
    }

    #[cfg(test)]
    pub(crate) fn select_for_test(&mut self, id: &str, cx: &mut Context<Self>) {
        self.select(id.to_string(), cx);
    }

    #[cfg(test)]
    pub(crate) fn ask_stop_for_test(&mut self, cx: &mut Context<Self>) {
        self.ask_stop(cx);
    }

    #[cfg(test)]
    pub(crate) fn ask_remove_for_test(&mut self, cx: &mut Context<Self>) {
        self.ask_remove(cx);
    }

    #[cfg(test)]
    pub(crate) fn run_again_for_test(&mut self, cx: &mut Context<Self>) {
        self.run_again(cx);
    }

    fn report_scene(&self, rows: &[Row]) {
        let selected = |state: UiState| {
            rows.iter()
                .any(|r| Some(&r.session.id) == self.selected.as_ref() && r.state == state)
        };
        let has_activity = self.activity.as_ref().is_some_and(|a| !a.is_empty());
        match self.fixture {
            Some(Fixture::AgentsPage) if selected(UiState::NeedsInput) && has_activity => {
                crate::sandbox::scene_ready(Fixture::AgentsPage)
            }
            Some(Fixture::AgentsDetail) if selected(UiState::Running) && has_activity => {
                crate::sandbox::scene_ready(Fixture::AgentsDetail)
            }
            Some(Fixture::AgentsStates)
                if UiState::ALL
                    .iter()
                    .all(|s| rows.iter().any(|r| r.state == *s)) =>
            {
                crate::sandbox::scene_ready(Fixture::AgentsStates)
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------- render ---

    fn render_row(&self, t: &Theme, row: &Row, now: f64, cx: &mut Context<Self>) -> AnyElement {
        let id = row.session.id.clone();
        let selected = self.selected.as_ref() == Some(&id);
        // Over the selection fill the meta line and time use primary text
        // (secondary is about 4.3:1 there in Light).
        let quiet = if selected { t.text } else { t.text_secondary };
        let when = row
            .session
            .updated_at
            .or(row.session.created_at)
            .map(|ts| ctl::relative(ts, now))
            .unwrap_or_default();
        let mut meta = format!(" · {}", row.provider);
        if let Some(repo) = &row.session.repo {
            meta.push_str(&format!(" · {repo}"));
        }
        let (hover, pressed) = (t.hover, t.pressed);
        let name = SharedString::from(format!("agents-row-{id}"));
        let selector = name.to_string();
        div()
            .id(name)
            .debug_selector(move || selector.clone())
            .flex()
            .items_center()
            .gap(space::s5())
            .h(px(56.))
            .flex_none()
            .mx(space::s2())
            .px(space::s4())
            .rounded(size::radius_element())
            .cursor_pointer()
            .when(selected, |d| d.bg(t.selected))
            .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
            .active(move |s| s.bg(pressed))
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.list_focus);
                this.select_by_user(id.clone(), cx);
            }))
            .child(ui::monogram_tile(t, row.monogram.clone(), px(28.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .min_w_0()
                            .child(
                                // No `whitespace_nowrap`: GPUI then skips the
                                // truncating re-measure (see History rows).
                                ui::label(t, row.title.clone())
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis(),
                            )
                            .child(ui::supporting(t, when).flex_none().text_color(quiet)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .min_w_0()
                            .child(ui::status_glyph(
                                t,
                                row.state.tone(),
                                row.state.icon(),
                                row.state.label(),
                            ))
                            .child(
                                ui::supporting(t, meta)
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .text_color(quiet),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_list(
        &mut self,
        rows: &[Row],
        now: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        let query = self.query(cx);
        let groups = ctl::grouped(rows, self.filter, &query);
        // Headings and rows are the scrolled element's own children, so the
        // scroll handle can bring the selected row into view by index.
        let mut items: Vec<AnyElement> = Vec::new();
        let mut selected_item = None;
        if groups.is_empty() {
            items.push(
                if !query.trim().is_empty() {
                    ui::empty_state(
                        &t,
                        "search",
                        ctl::search_empty(&query),
                        "",
                        Some(
                            Button::new("agents-clear-search", "Clear search")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.escape(window, cx);
                                }))
                                .into_any_element(),
                        ),
                    )
                } else {
                    ui::empty_state(&t, "bot", ctl::FILTER_EMPTY, "", None)
                }
                .into_any_element(),
            );
        }
        for (group, rows) in groups {
            items.push(
                ui::supporting(&t, group.heading())
                    .font_weight(FontWeight::SEMIBOLD)
                    .px(space::s6())
                    .pt(space::s5())
                    .pb(space::s2())
                    .flex_none()
                    .into_any_element(),
            );
            let heading = items.len() - 1;
            for (i, row) in rows.into_iter().enumerate() {
                if self.selected.as_ref() == Some(&row.session.id) {
                    // A group's first row brings its heading into view too.
                    selected_item = Some(if i == 0 { heading } else { items.len() });
                }
                items.push(self.render_row(&t, row, now, cx));
            }
        }
        if std::mem::take(&mut self.reveal) {
            if let Some(ix) = selected_item {
                self.list_scroll.scroll_to_item(ix);
            }
        }
        let this = cx.entity().downgrade();
        let focused = self.list_focus.is_focused(window);
        let ring = ui::ring(&t);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .px(space::s6())
                    .pt(space::s6())
                    .pb(space::s4())
                    .child(ui::icon("search", t.icon_secondary))
                    .child(self.search.clone()),
            )
            .child(
                div().px(space::s6()).pb(space::s2()).child(
                    Segmented::new(
                        "agents-filter",
                        Filter::ALL.map(Filter::label),
                        Some(self.filter.index()),
                    )
                    .on_select(move |i, _, cx| {
                        let _ = this.update(cx, |p, cx| {
                            p.filter = Filter::ALL[i];
                            cx.notify();
                        });
                    }),
                ),
            )
            .child(
                div()
                    .id("agents-list")
                    .debug_selector(|| "agents-list".into())
                    .track_focus(&self.list_focus)
                    .on_key_down(cx.listener(Self::on_list_key))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .pb(space::s4())
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .when(focused, move |d| d.shadow(ring.clone()))
                    .children(items),
            )
    }

    /// The action row: at most three buttons (Open in provider, Stop, Run
    /// again), then `…` (Copy link, Copy ID, Remove from LocalFlow).
    fn render_actions(&self, t: &Theme, row: &Row, cx: &mut Context<Self>) -> gpui::Div {
        let actions = ctl::actions(row);
        let busy = self.busy.as_deref() == Some(row.session.id.as_str());
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(url) = actions.open.clone() {
            buttons.push(
                Button::new("agents-open", format!("Open in {}", row.provider))
                    .small()
                    .icon("external-link")
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .into_any_element(),
            );
        }
        if let Some(enabled) = actions.stop {
            buttons.push(
                Button::new("agents-stop", "Stop")
                    .small()
                    .icon("square")
                    .disabled(!enabled)
                    .busy(busy || row.state == UiState::Stopping)
                    .on_click(cx.listener(|this, _, _, cx| this.ask_stop(cx)))
                    .into_any_element(),
            );
        }
        buttons.push(
            Button::new("agents-run-again", "Run again")
                .small()
                .icon("rotate-cw")
                .track_focus(&self.run_again_focus)
                .on_click(cx.listener(|this, _, _, cx| this.run_again(cx)))
                .into_any_element(),
        );
        let more = div()
            .id("agents-more")
            .debug_selector(|| "agents-more".into())
            .track_focus(&self.more_focus)
            .flex()
            .items_center()
            .justify_center()
            .size(size::control_sm())
            .rounded(size::radius_inner())
            .cursor_pointer()
            .hover({
                let hover = t.hover;
                move |s| s.bg(hover)
            })
            .focus({
                let ring = ui::ring(t);
                move |s| s.shadow(ring.clone())
            })
            .tooltip(|_, cx| ui::Tooltip::view("More actions".into(), cx))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_more(window, cx)))
            .child(ui::icon("ellipsis", t.icon_secondary));
        let menu = self.more_open.then(|| {
            let item = |id: &'static str, label: &'static str, focus: &FocusHandle| {
                div()
                    .id(id)
                    .debug_selector(move || format!("menu:{label}"))
                    .track_focus(focus)
                    .px(space::s5())
                    .py(space::s3())
                    .rounded(size::radius_inner())
                    .cursor_pointer()
                    .hover({
                        let hover = t.hover;
                        move |s| s.bg(hover)
                    })
                    .focus({
                        let ring = ui::ring_over(t, t.bg_popover);
                        move |s| s.shadow(ring.clone())
                    })
                    .child(ui::body(t, label))
            };
            let mut menu = div()
                .absolute()
                .top(size::control_sm() + px(4.))
                .right_0()
                .w(px(220.))
                .p(space::s2())
                .flex()
                .flex_col()
                .rounded(size::radius_element())
                .border_1()
                .border_color(t.border)
                .bg(t.bg_popover)
                .shadow(crate::theme::elevation::popover(t))
                .debug_selector(|| "agents-menu".into())
                .id("agents-menu")
                .occlude()
                .on_key_down(cx.listener(Self::on_menu_key));
            if let Some(url) = actions.open.clone() {
                menu = menu.child(
                    item("agents-copy-link", "Copy link", &self.menu_focus[0]).on_click(
                        cx.listener(move |this, _, window, cx| {
                            window.focus(&this.more_focus);
                            this.copy(url.clone(), "Link copied", cx)
                        }),
                    ),
                );
            }
            let id = row.session.id.clone();
            menu = menu.child(
                item("agents-copy-id", "Copy ID", &self.menu_focus[1]).on_click(cx.listener(
                    move |this, _, window, cx| {
                        window.focus(&this.more_focus);
                        this.copy(id.clone(), "Session ID copied", cx)
                    },
                )),
            );
            if actions.remove {
                menu = menu.child(
                    item(
                        "agents-remove",
                        "Remove from LocalFlow",
                        &self.menu_focus[2],
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.ask_remove(cx))),
                );
            }
            menu
        });
        div()
            .debug_selector(|| "agents-actions".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap(space::s3())
            .children(buttons)
            .child(div().relative().child(more).children(menu))
    }

    fn render_activity(&self, t: &Theme, row: &Row) -> Option<gpui::Div> {
        let activity = self.activity.as_ref().filter(|a| !a.is_empty())?;
        let lines: Vec<AnyElement> = activity
            .newest_first()
            .take(SHOWN_EVENTS)
            .map(|event| {
                let (label, text) = ctl::activity_line(event, &row.provider);
                let when = event.ts.map(time_label).unwrap_or_default();
                div()
                    .flex()
                    .items_start()
                    .gap(space::s4())
                    .px(ui::row_inset())
                    .py(space::s3())
                    .min_w_0()
                    .child(
                        ui::mono(t, when)
                            .text_color(t.text_secondary)
                            .flex_none()
                            .w(px(48.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .flex_1()
                            .min_w_0()
                            .child(ui::supporting(t, label).font_weight(FontWeight::MEDIUM))
                            .when(!text.is_empty(), |d| {
                                // Plain text: never parsed as markup, never a link.
                                d.child(ui::body(t, text).min_w_0())
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        let more = activity.len().saturating_sub(SHOWN_EVENTS);
        Some(
            ui::section(
                t,
                "Activity",
                (more > 0).then(|| format!("Newest {SHOWN_EVENTS} lines").into()),
                None,
            )
            .debug_selector(|| "agents-activity".into())
            .child(ui::group(t).children(lines)),
        )
    }

    fn render_results(&self, t: &Theme, row: &Row) -> Option<gpui::Div> {
        let results = ctl::results(row)?;
        let usage = row.session.usage.as_ref().and_then(ctl::usage_line);
        let body = match results {
            Results::Text { text, truncated } => div()
                .flex()
                .flex_col()
                .gap(space::s4())
                .px(ui::row_inset())
                .py(space::s5())
                .child(ui::reading(t, text))
                .when(truncated, |d| {
                    d.child(ui::supporting(t, ctl::RESULT_TRUNCATED))
                })
                .when_some(usage, |d, u| d.child(ui::supporting(t, u))),
            Results::Preview(text) => div()
                .flex()
                .flex_col()
                .gap(space::s4())
                .px(ui::row_inset())
                .py(space::s5())
                .child(ui::reading(t, text))
                .child(ui::supporting(t, "Reading the full result…")),
            Results::Empty(heading, body) => {
                let open = ctl::actions(row).open.map(|url| {
                    Button::new("agents-results-open", format!("Open in {}", row.provider))
                        .small()
                        .icon("external-link")
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .into_any_element()
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s3())
                    .px(ui::row_inset())
                    .py(space::s5())
                    .child(ui::label(t, heading))
                    .child(ui::secondary(t, body))
                    .when_some(open, |d, b| d.child(div().pt(space::s2()).child(b)))
            }
        };
        Some(
            ui::section(t, "Results", None, None)
                .debug_selector(|| "agents-results".into())
                .child(ui::group(t).child(body)),
        )
    }

    fn render_reply(&self, t: &Theme, row: &Row, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let enabled = ctl::actions(row).reply?;
        let text_empty = self.reply.read(cx).text().trim().is_empty();
        let disabled = !enabled || self.sending;
        Some(
            div()
                .debug_selector(|| "agents-reply".into())
                .flex()
                .flex_col()
                .gap(space::s3())
                .px(space::s8())
                .py(space::s5())
                .border_t_1()
                .border_color(t.separator)
                .bg(t.bg_surface)
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(space::s4())
                        .child(div().flex_1().min_w_0().child(self.reply.clone()))
                        .child(
                            Button::new("agents-send", "Send")
                                .primary()
                                .icon("send")
                                .busy(self.sending)
                                .disabled(disabled || text_empty)
                                .on_click(cx.listener(|this, _, _, cx| this.send_reply(cx))),
                        ),
                )
                .when_some(self.reply_error.clone(), |d, e| {
                    d.child(ui::error_line(t, e).debug_selector(|| "agents-reply-error".into()))
                }),
        )
    }

    fn render_detail(&self, rows: &[Row], now: f64, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let row = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().find(|r| &r.session.id == id));
        let Some(row) = row else {
            return div().id("agents-detail").size_full().child(ui::empty_state(
                &t,
                "bot",
                ctl::SELECT_HEADING,
                ctl::SELECT_BODY,
                None,
            ));
        };
        let s = &row.session;
        let updated = s.updated_at.map(time_label);
        let meta = ctl::meta_line(row, now, time_label);
        let note = ctl::state_note(row, updated.as_deref());
        let question = self
            .activity
            .as_ref()
            .filter(|a| a.session == s.id)
            .and_then(Activity::question);
        let banner =
            ctl::banner(row, updated.as_deref(), question).map(|(tone, message, retry)| {
                let action = retry.then(|| {
                    Button::new("agents-retry", "Try again")
                        .small()
                        .icon("refresh-cw")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pcx.recheck_health(cx);
                            this.load(cx);
                        }))
                        .into_any_element()
                });
                ui::banner(&t, tone, message, action)
                    .debug_selector(|| "agents-state-banner".into())
            });
        let task = match &s.prompt {
            Some(prompt) => ui::reading(&t, prompt.clone()),
            None => ui::secondary(&t, task_placeholder(&self.detail)),
        };
        let payload = ctl::payload_for(row);
        let (sent_to, items) = ctl::receipt(&payload);
        let workspace = ctl::workspace_note(&payload);
        let simulated = self.simulated() || s.sandbox;
        let receipt = div()
            .debug_selector(|| "agents-receipt".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap(space::s3())
            .child(ui::icon("shield", t.icon_secondary).size(px(14.)))
            .child(ui::supporting(&t, sent_to))
            .children(
                items
                    .into_iter()
                    .map(|item| ui::token(&t, Tone::Neutral, item)),
            )
            .when_some(workspace, |d, note| {
                d.child(ui::supporting(&t, note).debug_selector(|| "agents-workspace".into()))
            })
            .when(simulated, |d| {
                d.child(ui::token(&t, Tone::Warning, "Simulated"))
            });
        let id = s.id.clone();
        let copy_id = IconButton::new("agents-copy-id-icon", "copy", "Copy session ID").on_click(
            cx.listener(move |this, _, _, cx| this.copy(id.clone(), "Session ID copied", cx)),
        );
        let actions = self.render_actions(&t, row, cx);
        let activity = self.render_activity(&t, row);
        let results = self.render_results(&t, row);
        let reply = self.render_reply(&t, row, cx);
        div()
            .id("agents-detail")
            .debug_selector(|| "agents-detail".into())
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .child(
                div()
                    .id("agents-detail-scroll")
                    .debug_selector(|| "agents-detail-scroll".into())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        // Never shrunk to the pane's height: taller content
                        // scrolls. (Shrunk, its sections were squeezed until
                        // headings wrapped a letter a line and cards clipped.)
                        div()
                            .debug_selector(|| "agents-detail-column".into())
                            .flex()
                            .flex_col()
                            .flex_none()
                            .gap(space::s6())
                            .p(space::s8())
                            .w_full()
                            .min_w_0()
                            .max_w(px(size::READING + 64.))
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(space::s5())
                                    .min_w_0()
                                    .child(ui::monogram_tile(&t, row.monogram.clone(), px(36.)))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(space::s2())
                                            .flex_1()
                                            .min_w_0()
                                            .child(ui::heading(&t, row.title.clone()))
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_wrap()
                                                    .items_center()
                                                    .gap(space::s3())
                                                    .child(ui::token(
                                                        &t,
                                                        row.state.tone(),
                                                        row.state.label(),
                                                    ))
                                                    .child(ui::supporting(&t, meta)),
                                            )
                                            .when_some(note, |d, note| {
                                                d.child(ui::supporting(&t, note))
                                            })
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(space::s2())
                                                    .child(
                                                        ui::mono(&t, s.id.clone())
                                                            .text_color(t.text_secondary),
                                                    )
                                                    .child(copy_id),
                                            ),
                                    ),
                            )
                            .child(actions)
                            .children(banner)
                            .child(
                                ui::section(&t, "Task", None, None)
                                    .debug_selector(|| "agents-task".into())
                                    .child(
                                        ui::group(&t)
                                            .debug_selector(|| "agents-task-card".into())
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap(space::s5())
                                                    .px(ui::row_inset())
                                                    .py(space::s5())
                                                    .child(task)
                                                    .child(ui::divider(&t))
                                                    .child(receipt),
                                            ),
                                    ),
                            )
                            .children(activity)
                            .children(results),
                    ),
            )
            .children(reply)
    }

    /// Nothing has ever been read and the service is failing: the error
    /// banner says so, and no empty state guesses at why the list is empty.
    fn never_read(&self) -> bool {
        self.store.error.is_some() && self.store.status.is_none()
    }

    /// The whole-page empty state's heading and body (no sessions at all);
    /// an empty heading while the first read runs.
    fn empty_copy(&self) -> (&'static str, String) {
        if !self.store.loaded {
            ("", "Loading sessions…".into())
        } else if !self.store.any_connected() {
            (ctl::NO_PROVIDER_HEADING, ctl::no_provider_body())
        } else {
            // No global shortcut is registered yet, so none is named.
            (ctl::NO_SESSIONS_HEADING, ctl::no_sessions_body(None))
        }
    }

    /// The whole-page empty state shown, for shell tests.
    #[cfg(test)]
    pub(crate) fn empty_heading(&self) -> Option<&'static str> {
        (self.store.is_empty() && !self.never_read()).then(|| self.empty_copy().0)
    }

    fn render_empty(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let (heading, body) = self.empty_copy();
        let content = if heading.is_empty() {
            div().p(space::s10()).child(ui::secondary(t, body))
        } else {
            let action = if heading == ctl::NO_SESSIONS_HEADING {
                Some(
                    Button::new("agents-empty-new", "New task")
                        .primary()
                        .icon("plus")
                        .on_click(cx.listener(|this, _, _, cx| this.new_task(None, cx)))
                        .into_any_element(),
                )
            } else if heading == ctl::NO_PROVIDER_HEADING {
                let pcx = self.pcx.clone();
                Some(
                    Button::new("agents-empty-settings", ctl::OPEN_AGENT_SETTINGS)
                        .primary()
                        .on_click(move |_, _, cx| {
                            pcx.navigate(crate::page::Page::Settings, Some("agents"), cx)
                        })
                        .into_any_element(),
                )
            } else {
                None
            };
            ui::empty_state(t, "bot", heading, body, action)
        };
        div()
            .debug_selector(|| "agents-empty".into())
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .child(content)
    }

    /// The service can't be read: an Error banner with Try again.
    fn service_banner(&self, health: &Health, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let t = theme(cx).clone();
        let message = match (&self.store.error, health) {
            (Some(AgentsError::Offline), _) | (_, Health::Offline) => ctl::SERVICE_DOWN.to_string(),
            (Some(error @ AgentsError::Refused(_)), _) => error.message(),
            _ => return None,
        };
        let retry = Button::new("agents-service-retry", "Try again")
            .small()
            .icon("refresh-cw")
            .on_click(cx.listener(|this, _, _, cx| {
                this.pcx.recheck_health(cx);
                this.load(cx);
            }))
            .into_any_element();
        Some(div().px(space::s10()).pt(space::s5()).child(ui::banner(
            &t,
            Tone::Error,
            message,
            Some(retry),
        )))
    }
}

impl Render for AgentsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        // One snapshot of the rows per frame, shared by every part.
        let now = self.now();
        let rows = self.store.rows(now);
        self.report_scene(&rows);
        // Once the list (and so the providers' capabilities) is read.
        if std::mem::take(&mut self.menu_focus_pending) && self.more_open {
            let first = self
                .selected_row()
                .and_then(|row| Self::menu_entries(&row).first().copied());
            if let Some(first) = first {
                // After this frame, once the entry is drawn.
                let focus = self.menu_focus[first].clone();
                cx.defer_in(window, move |_, window, _| window.focus(&focus));
            }
        }
        if self.focus_pending && self.pending_open.is_none() && self.store.loaded && !self.loading {
            let listed = self
                .selected
                .as_ref()
                .is_some_and(|id| rows.iter().any(|r| &r.session.id == id));
            if listed {
                self.focus_pending = false;
                // After this frame, once the control is drawn.
                cx.defer_in(window, move |this, window, cx| {
                    let reply = this
                        .selected_row()
                        .is_some_and(|r| ctl::actions(&r).reply == Some(true));
                    if reply {
                        this.reply.read(cx).focus(window);
                    } else {
                        window.focus(&this.run_again_focus);
                    }
                    cx.notify();
                });
            }
        }
        let support = (self.store.loaded && !self.never_read())
            .then(|| SharedString::from(ctl::support_line(&rows, self.store.counts)));
        let health = self.pcx.health(cx);
        let banner = self.service_banner(&health, cx);
        let body = if self.never_read() {
            div().debug_selector(|| "agents-unread".into())
        } else if self.store.is_empty() {
            self.render_empty(&t, cx)
        } else {
            let list = self.render_list(&rows, now, window, cx);
            let detail = self.render_detail(&rows, now, cx);
            split(&t, list, detail)
        };
        let new_task = Button::new("agents-new-task", "New task")
            .primary()
            .icon("plus")
            .on_click(cx.listener(|this, _, _, cx| this.new_task(None, cx)))
            .into_any_element();
        page(
            header(&t, "Agents", support, vec![new_task]),
            div()
                .id("agents-page")
                // An Enter whose key-down moved focus onto Run again must
                // not click it on key-up.
                .capture_key_up(cx.listener(|this, event: &KeyUpEvent, window, _| {
                    if event.keystroke.key == "enter" && std::mem::take(&mut this.eat_enter_up) {
                        window.prevent_default();
                    }
                }))
                .flex()
                .flex_col()
                .size_full()
                .children(banner)
                .child(body),
        )
    }
}
