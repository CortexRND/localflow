//! Meetings: start/stop a meeting on the local service, take notes (autosaved
//! in order through `MeetingNotesWriter`), ask about the saved meeting, and
//! open recent notes from the vault.

use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use lf_ui::backends::meeting::{vault_notes_path, MeetingNotesWriter, MAX_USER_NOTES_CHARS};
use lf_ui::hub_model::{self, RecentNote, SpeakerMode};
use serde_json::json;

use crate::app::PageCx;
use crate::controllers::meetings::{self as ctl, NotesSync, Phase, Reconcile, Template};
use crate::controllers::speakers::{self as spk, SpeakerStore};
use crate::page::Page;
use crate::pages::{header, page, sandbox_note, service_banner, split};
use crate::remote::Health;
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button};

pub struct MeetingsPage {
    pcx: PageCx,
    phase: Phase,
    templates: Vec<Template>,
    template: Entity<Select>,
    title: Entity<TextInput>,
    notes: Entity<TextInput>,
    question: Entity<TextInput>,
    answers: Vec<(String, Result<String, String>)>,
    chat_available: bool,
    writer: Arc<MeetingNotesWriter>,
    notes_ticket: u64,
    /// Composer vs server notes (adoption without clobbering local edits).
    sync: NotesSync,
    recent: (Vec<RecentNote>, String),
    busy: bool,
    /// Speaker labels of saved meetings (post-stop and Recent panels).
    speakers: SpeakerStore,
    /// The rename field of the open speaker panel.
    speaker_name: Entity<TextInput>,
    /// The merge picker of the open speaker panel, and its titles.
    merge_select: Entity<Select>,
    merge_titles: Vec<String>,
    /// `--sandbox-fixture speakers-saved`: stop once the start finished.
    stop_after_start: bool,
}

impl MeetingsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| {
            TextInput::new(cx, hub_model::MEETING_TITLE_PLACEHOLDER)
                .label("Title")
                .max_chars(200)
        });
        let notes = cx.new(|cx| {
            TextInput::new(cx, hub_model::MEETING_NOTES_PLACEHOLDER)
                .multiline(6, 18)
                .max_chars(MAX_USER_NOTES_CHARS)
                .label("Notes")
        });
        let question = cx.new(|cx| TextInput::new(cx, "Ask about this meeting").max_chars(2000));
        let template = cx.new(|cx| Select::new(cx, Vec::new(), None).width(220.));
        cx.subscribe(&notes, |this, _, e, cx| {
            if *e == InputEvent::Changed {
                // Typing only: programmatic loads (`set_text`) emit nothing.
                this.sync.edited();
                this.schedule_notes_save(cx);
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&question, |this, _, e, cx| {
            if *e == InputEvent::Submit {
                this.ask(cx);
            }
        })
        .detach();
        let speaker_name = cx.new(|cx| {
            TextInput::new(cx, "Speaker name")
                .label("Speaker name")
                .max_chars(60)
        });
        cx.subscribe(&speaker_name, |this, _, e, cx| match e {
            InputEvent::Submit => this.commit_speaker(cx),
            InputEvent::Changed => {
                if !this.speakers.editor.busy && !this.speakers.editor.error.is_empty() {
                    this.speakers.editor.error.clear();
                    cx.notify();
                }
            }
        })
        .detach();
        let merge_select = cx.new(|cx| Select::new(cx, Vec::new(), None).width(220.));
        cx.subscribe(&merge_select, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let Some(title) = this.merge_titles.get(*i).cloned() {
                // The select already shows the pick: no resync, so its
                // focus and options stay exactly as the user left them.
                this.speakers.choose_merge_title(&title);
                cx.notify();
            }
        })
        .detach();
        let mut page = MeetingsPage {
            pcx,
            phase: Phase::Loading,
            templates: Vec::new(),
            template,
            title,
            notes,
            question,
            answers: Vec::new(),
            chat_available: false,
            writer: Arc::new(MeetingNotesWriter::new()),
            notes_ticket: 0,
            sync: NotesSync::default(),
            recent: (Vec::new(), String::new()),
            busy: false,
            speakers: SpeakerStore::default(),
            speaker_name,
            merge_select,
            merge_titles: Vec::new(),
            stop_after_start: false,
        };
        page.refresh(cx);
        // Status poll every 2 s while this page is shown or a meeting is live.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let alive = this.update(cx, |this, cx| {
                let shown = *this.pcx.current_page.borrow() == Page::Meetings;
                let live = matches!(this.phase, Phase::Recording { .. });
                if (shown || live) && !this.busy && this.enabled() {
                    this.poll(false, cx);
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        page
    }

    fn enabled(&self) -> bool {
        self.pcx
            .services
            .config()
            .field_bool("meetings_enabled")
            .unwrap_or(false)
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let config = self.pcx.services.config();
        self.recent = hub_model::recent_notes(
            config.field_str("vault_path").unwrap_or(""),
            config.field_str("notes_folder").unwrap_or(""),
            hub_model::RECENT_NOTES_LIMIT,
            None,
        );
        if self.enabled() {
            // Only notes saved with speaker labels (a frontmatter
            // `meeting_id`) are asked about.
            self.speakers.set_recent(&self.recent.0);
            self.run_probe(cx);
            self.poll(true, cx);
        }
        cx.notify();
    }

    fn poll(&mut self, with_templates: bool, cx: &mut Context<Self>) {
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let sent = self.sync.poll_started();
        let saved_ticket = self.speakers.poll_started();
        cx.spawn(async move |this, cx| {
            let snap = cx
                .background_executor()
                .spawn(async move { ctl::poll(client.as_ref(), with_templates) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Ignore answers that arrive mid-action, and whole snapshots
                // sent before the latest Start/Stop finished.
                if this.busy || !this.sync.is_current(sent) {
                    return;
                }
                let was_unreachable = matches!(this.phase, Phase::Offline(_) | Phase::Loading);
                let reachable = !matches!(snap.phase, Phase::Offline(_));
                let keep = matches!(this.phase, Phase::Saved { .. } | Phase::Error(_))
                    && snap.phase == Phase::Idle;
                // The retained "Notes saved" banner belongs to this window's
                // Stop: its speaker panel stays on that exact record.
                let pinned = keep && matches!(this.phase, Phase::Saved { .. });
                if reachable {
                    let before = this.speakers.merge_picker().1;
                    this.speakers
                        .adopt_polled(saved_ticket, snap.last_saved.clone(), pinned);
                    // Another client may have renamed or merged speakers.
                    this.resync_merge_select_if_changed(before, cx);
                }
                if !keep {
                    this.phase = snap.phase;
                }
                if reachable && was_unreachable {
                    // Speaker reads may have failed with the connection.
                    this.speakers.reconnected();
                    this.run_probe(cx);
                }
                this.forget_undrawn_speaker_edit(cx);
                if matches!(this.phase, Phase::Recording { .. }) {
                    let local = this.notes.read(cx).text().to_string();
                    match this.sync.reconcile(sent, &local, snap.notes.as_deref()) {
                        Reconcile::Adopt(server) => {
                            this.notes.update(cx, |i, cx| i.set_text(server, cx));
                        }
                        // Local edits the server hasn't acknowledged: send them.
                        Reconcile::Resend => this.schedule_notes_save(cx),
                        Reconcile::Keep => {}
                    }
                }
                this.chat_available = snap.chat_available;
                if with_templates && !snap.templates.is_empty() {
                    let names: Vec<SharedString> = snap
                        .templates
                        .iter()
                        .map(|(_, n)| n.clone().into())
                        .collect();
                    let sel = snap
                        .templates
                        .iter()
                        .position(|(id, _)| *id == snap.default_template)
                        .or(Some(0));
                    this.templates = snap.templates;
                    this.template
                        .update(cx, |s, cx| s.set_options(names, sel, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let title = self.title.read(cx).text().to_string();
        let notes = self.notes.read(cx).text().to_string();
        // Start writes these notes: acknowledge exactly this text on success.
        let token = self.sync.begin_write();
        let template = self
            .template
            .read(cx)
            .selected()
            .and_then(|i| self.templates.get(i))
            .map(|(id, _)| id.clone())
            .unwrap_or_default();
        self.phase = Phase::Starting;
        self.busy = true;
        self.answers.clear();
        // Start writes the notes: a close waits for the server request itself.
        let guard = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            let phase = cx
                .background_executor()
                .spawn(async move {
                    let _guard = guard;
                    ctl::start(client.as_ref(), &title, &template, &notes)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                let started = matches!(phase, Phase::Recording { .. });
                this.sync.action_finished(token, started);
                this.phase = phase;
                // The saved panel hides while recording: drop its open edit.
                this.forget_undrawn_speaker_edit(cx);
                if std::mem::take(&mut this.stop_after_start) && started {
                    this.stop(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn schedule_notes_save(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        self.notes_ticket += 1;
        let local = self.notes_ticket;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.notes_ticket != local || !matches!(this.phase, Phase::Recording { .. }) {
                    return;
                }
                let client = this
                    .pcx
                    .services
                    .remote
                    .meetings(&this.pcx.services.config());
                let writer = this.writer.clone();
                let ticket = writer.ticket();
                let text = this.notes.read(cx).text().to_string();
                let token = this.sync.begin_write();
                // Registered so a close or quit waits for this save.
                let guard = lf_ui::shutdown::register_pending_write();
                cx.spawn(async move |this, cx| {
                    let ok = cx
                        .background_executor()
                        .spawn(async move {
                            let _guard = guard;
                            writer.save(client.as_ref(), ticket, &text).is_ok()
                        })
                        .await;
                    let _ = this.update(cx, |this, _| this.sync.write_finished(token, ok));
                })
                .detach();
            });
        })
        .detach();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let writer = self.writer.clone();
        let ticket = writer.ticket();
        self.notes_ticket += 1; // cancel any pending debounce
        let notes = self.notes.read(cx).text().to_string();
        // Stop writes these notes first: acknowledge exactly this text.
        let token = self.sync.begin_write();
        // Read-only until Stop returns: later typing would be lost.
        self.notes.update(cx, |i, cx| i.set_disabled(true, cx));
        self.phase = Phase::Stopping;
        self.busy = true;
        // A close waits for the notes write and stop request themselves.
        let guard = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            let (phase, saved) = cx
                .background_executor()
                .spawn(async move {
                    let _guard = guard;
                    ctl::stop_saved(client.as_ref(), &writer, ticket, &notes)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if saved.is_some() {
                    // The newest saved meeting: its speakers (if labelled)
                    // show in the post-stop panel.
                    this.speakers.present_saved(saved);
                    this.sync_merge_select(cx);
                }
                this.notes.update(cx, |i, cx| i.set_disabled(false, cx));
                this.sync
                    .action_finished(token, matches!(phase, Phase::Saved { .. }));
                if matches!(phase, Phase::Saved { .. }) {
                    this.chat_available = true;
                    this.sync.reset();
                }
                this.phase = phase;
                this.refresh(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn ask(&mut self, cx: &mut Context<Self>) {
        let q = self.question.read(cx).text().trim().to_string();
        if q.is_empty() {
            return;
        }
        self.question.update(cx, |i, cx| i.clear(cx));
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let index = self.answers.len();
        self.answers.push((q.clone(), Ok("Thinking…".into())));
        cx.spawn(async move |this, cx| {
            let answer = cx
                .background_executor()
                .spawn(async move { ctl::chat(client.as_ref(), &q) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(slot) = this.answers.get_mut(index) {
                    slot.1 = answer;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Window close / quit during a meeting: send notes that are dirty or
    /// still inside the autosave debounce right away, on a registered writer
    /// thread the shell then waits for.
    pub fn flush_for_close(
        &mut self,
        failures: crate::controllers::shutdown::Failures,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let composer_empty = self.notes.read(cx).is_empty();
        match ctl::close_notes(&self.phase, self.sync.is_dirty(), composer_empty) {
            ctl::CloseNotes::Nothing => Ok(()),
            ctl::CloseNotes::Blocked(reason) => Err(reason),
            ctl::CloseNotes::Send => {
                self.notes_ticket += 1; // cancel the pending debounce
                let client = self
                    .pcx
                    .services
                    .remote
                    .meetings(&self.pcx.services.config());
                let ticket = self.writer.ticket();
                let text = self.notes.read(cx).text().to_string();
                crate::controllers::shutdown::send_meeting_notes(
                    client,
                    self.writer.clone(),
                    ticket,
                    text,
                    failures,
                );
                Ok(())
            }
        }
    }

    fn new_meeting(&mut self, cx: &mut Context<Self>) {
        self.title.update(cx, |i, cx| i.clear(cx));
        self.notes.update(cx, |i, cx| i.clear(cx));
        self.sync.reset();
        self.phase = Phase::Idle;
        cx.notify();
    }

    // ---------------------------------------------------------- speakers ---

    /// Send the next queued speaker read (one at a time).
    fn run_probe(&mut self, cx: &mut Context<Self>) {
        let Some(probe) = self.speakers.next_probe() else {
            return;
        };
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let ids = probe.ids.clone();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move { spk::probe(client.as_ref(), &ids) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let before = this.speakers.merge_picker().1;
                this.speakers.probe_finished(probe, results);
                this.resync_merge_select_if_changed(before, cx);
                this.run_probe(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn recording(&self) -> bool {
        matches!(self.phase, Phase::Recording { .. })
    }

    /// Whether the detail pane can show a speaker panel in this phase.
    fn speakers_drawable(&self) -> bool {
        !matches!(
            self.phase,
            Phase::Loading | Phase::Offline(_) | Phase::Starting | Phase::Stopping
        )
    }

    /// The meeting whose speaker panel is drawn, if any.
    fn speaker_panel(&self) -> Option<String> {
        self.speakers_drawable()
            .then(|| self.speakers.panel(self.recording()))
            .flatten()
    }

    fn forget_undrawn_speaker_edit(&mut self, cx: &mut Context<Self>) {
        let drawn = self
            .speaker_panel()
            .is_some_and(|id| id == self.speakers.editor.meeting_id);
        let before = self.speakers.editor.mode.clone();
        self.speakers.forget_undrawn_edit(drawn);
        if self.speakers.editor.mode != before {
            self.sync_merge_select(cx);
        }
    }

    /// Put the open merge's picker into the select (titles are unique, and
    /// a stale choice shows the unmapped placeholder, never a guess).
    fn sync_merge_select(&mut self, cx: &mut Context<Self>) {
        let (_, picker) = self.speakers.merge_picker();
        if !matches!(self.speakers.editor.mode, SpeakerMode::Merge(_)) {
            self.merge_titles.clear();
            return;
        }
        let options: Vec<SharedString> = picker
            .titles
            .iter()
            .cloned()
            .map(SharedString::from)
            .collect();
        self.merge_titles = picker.titles;
        let selected = Some(picker.selected);
        let busy = self.speakers.editor.busy;
        self.merge_select.update(cx, |s, cx| {
            s.set_options(options, selected, cx);
            s.set_disabled(busy, cx);
        });
    }

    /// New speaker data arrived: resync the merge picker only when what it
    /// shows actually changed (titles, selection or whether it is a pick).
    fn resync_merge_select_if_changed(
        &mut self,
        before: hub_model::MergePicker,
        cx: &mut Context<Self>,
    ) {
        if self.speakers.merge_picker().1 != before {
            self.sync_merge_select(cx);
        }
    }

    fn begin_speaker_rename(
        &mut self,
        meeting_id: &str,
        speaker_id: &str,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.speakers.begin_rename(meeting_id, speaker_id) {
            self.speaker_name.update(cx, |i, cx| {
                i.set_disabled(false, cx);
                i.set_text(label, cx);
            });
            self.speaker_name.read(cx).focus(window);
            self.sync_merge_select(cx);
        }
        cx.notify();
    }

    fn begin_speaker_merge(&mut self, meeting_id: &str, speaker_id: &str, cx: &mut Context<Self>) {
        if self.speakers.begin_merge(meeting_id, speaker_id) {
            self.sync_merge_select(cx);
        }
        cx.notify();
    }

    fn cancel_speaker_edit(&mut self, cx: &mut Context<Self>) {
        self.speakers.cancel();
        self.sync_merge_select(cx);
        cx.notify();
    }

    /// Save the open rename or merge on the server. Registered as a pending
    /// write until the server answered, so a close or quit waits for it; it
    /// never makes the page busy, so Start, Stop and Ask stay usable.
    fn commit_speaker(&mut self, cx: &mut Context<Self>) {
        let name = self.speaker_name.read(cx).text().to_string();
        let Some(request) = self.speakers.commit(&name) else {
            self.sync_merge_select(cx);
            cx.notify();
            return;
        };
        let client = self
            .pcx
            .services
            .remote
            .meetings(&self.pcx.services.config());
        let guard = lf_ui::shutdown::register_pending_write();
        self.speaker_name
            .update(cx, |i, cx| i.set_disabled(true, cx));
        self.sync_merge_select(cx);
        cx.spawn(async move |this, cx| {
            let (request, reply) = cx
                .background_executor()
                .spawn(async move {
                    let reply = spk::apply_edit(client.as_ref(), &request, guard);
                    (request, reply)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.speakers.edit_finished(&request, reply);
                this.speaker_name
                    .update(cx, |i, cx| i.set_disabled(false, cx));
                this.sync_merge_select(cx);
                this.run_probe(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A Recent row's "Speakers…": show (or hide) that meeting's panel.
    fn toggle_recent_speakers(&mut self, meeting_id: &str, cx: &mut Context<Self>) {
        if self.speakers.toggle_recent(meeting_id) {
            self.speaker_name.update(cx, |i, cx| i.clear(cx));
            self.sync_merge_select(cx);
            self.run_probe(cx);
        }
        cx.notify();
    }

    /// `--sandbox-fixture speakers-saved`: record and save a meeting (the
    /// sandbox saves it with speaker labels) through the page's own flow.
    pub fn start_and_stop(&mut self, cx: &mut Context<Self>) {
        if !self.enabled() || self.busy {
            return;
        }
        self.stop_after_start = true;
        self.start(cx);
    }

    /// `--sandbox-fixture speakers-recent`: "Speakers…" on the first Recent
    /// note saved with speaker labels.
    pub fn open_first_recent_speakers(&mut self, cx: &mut Context<Self>) {
        let first = self
            .recent
            .0
            .iter()
            .find_map(|note| self.speakers.recent_meeting(&note.path))
            .map(str::to_string);
        if let Some(id) = first {
            if self.speakers.open_recent() != Some(id.as_str()) {
                self.toggle_recent_speakers(&id, cx);
            }
        }
    }

    fn render_speakers(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let meeting_id = self.speaker_panel()?;
        let t = theme(cx).clone();
        let speakers = self.speakers.speakers_of(&meeting_id);
        let titles = hub_model::speaker_titles(&speakers);
        let editor = &self.speakers.editor;
        let editing = editor.meeting_id == meeting_id;
        let busy = editing && editor.busy;
        let mode = if editing {
            editor.mode.clone()
        } else {
            SpeakerMode::List
        };
        let recent = self.speakers.open_recent() == Some(meeting_id.as_str());
        let support = if recent {
            self.recent
                .0
                .iter()
                .find(|note| self.speakers.recent_meeting(&note.path) == Some(meeting_id.as_str()))
                .map(|note| format!("From “{}”.", note.title))
                .unwrap_or_else(|| "From a recent meeting.".into())
        } else {
            "From the meeting you just saved.".to_string()
        };
        let done = recent.then(|| {
            let id = meeting_id.clone();
            Button::new("speakers-done", "Done")
                .small()
                .ghost()
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_recent_speakers(&id, cx)))
                .into_any_element()
        });
        let (_, picker) = self.speakers.merge_picker();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (speaker, title) in speakers.iter().zip(titles) {
            let sid = speaker.id.clone();
            let element = match &mode {
                SpeakerMode::Rename(id) if *id == sid => div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .min_h(size::row())
                    .px(ui::row_inset())
                    .py(space::s4())
                    .child(div().flex_1().min_w_0().child(self.speaker_name.clone()))
                    .child(
                        Button::new(SharedString::from(format!("speaker-save-{sid}")), "Save")
                            .primary()
                            .small()
                            .busy(busy)
                            .tooltip(format!("Save the new name for {title}"))
                            .on_click(cx.listener(|this, _, _, cx| this.commit_speaker(cx))),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("speaker-cancel-{sid}")),
                            "Cancel",
                        )
                        .ghost()
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_speaker_edit(cx))),
                    )
                    .into_any_element(),
                SpeakerMode::Merge(id) if *id == sid => div()
                    .flex()
                    .flex_col()
                    .gap(space::s3())
                    .px(ui::row_inset())
                    .py(space::s4())
                    .child(ui::label(&t, format!("Merge {title} into")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(self.merge_select.clone())
                            .child(
                                Button::new(
                                    SharedString::from(format!("speaker-merge-go-{sid}")),
                                    "Merge",
                                )
                                .destructive()
                                .small()
                                .busy(busy)
                                .disabled(!picker.chosen)
                                .tooltip(format!("Merge {title} into the chosen speaker"))
                                .on_click(cx.listener(|this, _, _, cx| this.commit_speaker(cx))),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("speaker-merge-cancel-{sid}")),
                                    "Cancel",
                                )
                                .ghost()
                                .small()
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_speaker_edit(cx)),
                                ),
                            ),
                    )
                    .child(ui::supporting(&t, spk::MERGE_FINAL))
                    .into_any_element(),
                _ => {
                    let rename_id = meeting_id.clone();
                    let rename_sid = sid.clone();
                    let label = speaker.label.clone();
                    let merge_id = meeting_id.clone();
                    let merge_sid = sid.clone();
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(space::s6())
                        .min_h(size::row())
                        .px(ui::row_inset())
                        .py(space::s4())
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(space::s1())
                                .min_w_0()
                                .child(ui::label(&t, title.clone()))
                                .child(ui::supporting(
                                    &t,
                                    hub_model::speaker_talk_time(speaker.seconds),
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(space::s2())
                                .child(
                                    Button::new(
                                        SharedString::from(format!("speaker-rename-{sid}")),
                                        "Rename",
                                    )
                                    .ghost()
                                    .small()
                                    .icon("pencil")
                                    .disabled(busy)
                                    .tooltip(format!("Rename {title}"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.begin_speaker_rename(
                                                &rename_id,
                                                &rename_sid,
                                                label.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                                .child(
                                    Button::new(
                                        SharedString::from(format!("speaker-merge-{sid}")),
                                        "Merge…",
                                    )
                                    .ghost()
                                    .small()
                                    .disabled(busy || speakers.len() < 2)
                                    .tooltip(format!("Merge {title} into another speaker"))
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.begin_speaker_merge(&merge_id, &merge_sid, cx)
                                        },
                                    )),
                                ),
                        )
                        .into_any_element()
                }
            };
            rows.push(element);
        }
        let error =
            (editing && !editor.error.is_empty()).then(|| ui::error_line(&t, editor.error.clone()));
        Some(
            ui::section(&t, "Speakers", Some(support.into()), done)
                .child(ui::group_rows(&t, rows, ui::row_inset()))
                .children(error),
        )
    }

    fn open_note(&self, path: &str, cx: &mut Context<Self>) {
        let vault = self
            .pcx
            .services
            .config()
            .field_str("vault_path")
            .unwrap_or("")
            .to_string();
        match vault_notes_path(path, &vault) {
            Some(p) => cx.open_with_system(std::path::Path::new(&p)),
            None => self.pcx.toast(
                "That note isn't a Markdown file inside your vault, so it wasn't opened.",
                cx,
            ),
        }
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut list = div().flex().flex_col();
        if !self.recent.1.is_empty() {
            list = list.child(div().p(space::s6()).child(ui::banner(
                &t,
                Tone::Warning,
                self.recent.1.clone(),
                None,
            )));
        } else if self.recent.0.is_empty() {
            list = list.child(
                div()
                    .p(space::s6())
                    .child(ui::secondary(&t, hub_model::RECENT_EMPTY)),
            );
        }
        let speakers_open = self.speakers.open_recent().map(str::to_string);
        let edit_busy = self.speakers.editor.busy;
        for (i, note) in self.recent.0.iter().enumerate() {
            let path = note.path.clone();
            // Only a meeting known to have speakers offers "Speakers…"; every
            // other row keeps exactly its plain layout.
            let action = self
                .enabled()
                .then(|| self.speakers.recent_action(&note.path, self.recording()))
                .flatten();
            let row = ui::row_button(&t, ("note", i as u64), false)
                .flex_col()
                .px(space::s6())
                .py(space::s4())
                .on_click(cx.listener(move |this, _, _, cx| this.open_note(&path, cx)))
                .child(ui::label(&t, note.title.clone()))
                .child(ui::supporting(&t, note.date_label.clone()));
            let Some(meeting_id) = action else {
                list = list.child(row.border_b_1().border_color(t.border));
                continue;
            };
            let open = speakers_open.as_deref() == Some(meeting_id.as_str());
            let button = Button::new(("note-speakers", i as u64), "Speakers…")
                .small()
                .icon("users")
                .disabled(edit_busy)
                .tooltip(format!(
                    "{} the speakers of {}",
                    if open { "Hide" } else { "Show" },
                    note.title
                ))
                .variant(if open {
                    ui::Variant::Primary
                } else {
                    ui::Variant::Secondary
                })
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_recent_speakers(&meeting_id, cx)),
                );
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.border)
                    .child(row.flex_1().min_w_0())
                    .child(div().flex_none().pr(space::s4()).child(button)),
            );
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                ui::supporting(&t, "Recent notes")
                    .px(space::s6())
                    .pt(space::s6())
                    .pb(space::s2()),
            )
            .child(
                div()
                    .id("notes-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut body = div().flex().flex_col().gap(space::s6());
        // An open Recent speaker panel was just asked for: it leads the pane
        // (and the tab order), above the start/notes form. The post-stop
        // panel keeps its place after the saved banner.
        let mut speakers = self.render_speakers(cx);
        let recent_open = self.speakers.open_recent().is_some()
            && self.speaker_panel().as_deref() == self.speakers.open_recent();
        if recent_open {
            if let Some(panel) = speakers.take() {
                body = body.child(panel);
            }
        }
        let notes_len = self.notes.read(cx).text().chars().count();
        let counter = (notes_len * 10 >= MAX_USER_NOTES_CHARS * 9).then(|| {
            ui::supporting(
                &t,
                format!("{notes_len} / {MAX_USER_NOTES_CHARS} characters"),
            )
        });
        match &self.phase {
            Phase::Loading => body = body.child(ui::secondary(&t, "Checking the meeting service…")),
            Phase::Offline(_) => {
                body = body.child(ui::empty_state(
                    &t,
                    "users",
                    "Meetings need the LocalFlow service",
                    "Start LocalFlow, then try again. Your notes are kept here meanwhile.",
                    Some(
                        Button::new("meet-retry", "Try again")
                            .icon("refresh-cw")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pcx.recheck_health(cx);
                                this.poll(true, cx);
                            }))
                            .into_any_element(),
                    ),
                ));
            }
            Phase::Idle | Phase::Starting | Phase::Error(_) => {
                let starting = self.phase == Phase::Starting;
                if let Phase::Error(e) = &self.phase {
                    body = body.child(ui::banner(
                        &t,
                        Tone::Error,
                        e.clone(),
                        Some(
                            Button::new("meet-err-retry", "Try again")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.phase = Phase::Idle;
                                    this.poll(true, cx);
                                }))
                                .into_any_element(),
                        ),
                    ));
                }
                body = body
                    .child(ui::secondary(&t, hub_model::MEETING_SUBTITLE))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s2())
                            .child(ui::label(&t, "Title"))
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::label(&t, "Template"))
                            .child(self.template.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s2())
                            .child(ui::label(&t, "Notes"))
                            .child(self.notes.clone())
                            .children(counter),
                    )
                    .child(
                        div().child(
                            Button::new("meet-start", "Start recording")
                                .primary()
                                .icon("mic")
                                .busy(starting)
                                .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                        ),
                    );
            }
            Phase::Recording { seconds, capture } => {
                body = body
                    .child(
                        ui::card(&t)
                            .items_center()
                            .gap(space::s6())
                            .child(div().size(px(10.)).rounded_full().bg(t.recording))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .child(ui::heading(
                                        &t,
                                        format!("Recording · {}", ctl::elapsed(*seconds)),
                                    ))
                                    .child(ui::secondary(
                                        &t,
                                        if capture.is_empty() {
                                            "Recording your microphone.".to_string()
                                        } else {
                                            capture.clone()
                                        },
                                    )),
                            )
                            .child(
                                Button::new("meet-stop", "Stop and save")
                                    .primary()
                                    .icon("square")
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s2())
                            .child(ui::label(&t, "Notes"))
                            .child(self.notes.clone())
                            .child(ui::supporting(
                                &t,
                                "Notes save automatically while you record.",
                            ))
                            .children(counter),
                    );
            }
            Phase::Stopping => {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .child(ui::icon("loader-circle", t.icon_accent))
                        .child(ui::secondary(
                            &t,
                            "Writing your notes… This can take a few minutes for long meetings.",
                        )),
                );
            }
            Phase::Saved {
                message,
                notes_path,
            } => {
                let path = notes_path.clone();
                body = body
                    .child(ui::banner(&t, Tone::Success, message.clone(), None))
                    .child(
                        div()
                            .flex()
                            .gap(space::s4())
                            .child(
                                Button::new("open-saved", "Open note")
                                    .primary()
                                    .icon("external-link")
                                    .disabled(path.is_empty())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.open_note(&path, cx)
                                        }),
                                    ),
                            )
                            .child(
                                Button::new("meet-new", "New meeting")
                                    .icon("plus")
                                    .on_click(cx.listener(|this, _, _, cx| this.new_meeting(cx))),
                            ),
                    );
            }
        }
        if let Some(speakers) = speakers {
            body = body.child(speakers);
        }
        if self.chat_available && !matches!(self.phase, Phase::Offline(_) | Phase::Loading) {
            let mut chat = ui::section(&t, "Ask about the last meeting", None, None).child(
                div()
                    .flex()
                    .gap(space::s4())
                    .child(div().flex_1().child(self.question.clone()))
                    .child(
                        Button::new("meet-ask", "Ask")
                            .icon("send")
                            .on_click(cx.listener(|this, _, _, cx| this.ask(cx))),
                    ),
            );
            for (q, a) in &self.answers {
                chat = chat.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::s2())
                        .child(ui::label(&t, q.clone()))
                        .child(match a {
                            Ok(text) => ui::reading(&t, text.clone()),
                            Err(e) => ui::error_line(&t, e.clone()),
                        }),
                );
            }
            body = body.child(chat);
        }
        div()
            .id("meeting-detail")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .p(space::s10())
                    .max_w(px(size::READING + 64.))
                    .child(body),
            )
    }
}

impl Render for MeetingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        // Fixture scenes only: report that the real panel is on screen.
        if self.enabled() {
            let panel = self.speaker_panel();
            let recent = self.speakers.open_recent();
            if matches!(self.phase, Phase::Saved { .. })
                && recent.is_none()
                && panel.as_deref() == Some(crate::sandbox::FIXTURE_SAVED_ID)
            {
                crate::sandbox::scene_ready(crate::sandbox::Fixture::SpeakersSaved);
            }
            if recent == Some(crate::sandbox::FIXTURE_RECENT_ID) && panel.as_deref() == recent {
                crate::sandbox::scene_ready(crate::sandbox::Fixture::SpeakersRecent);
            }
        }
        let head = header(
            &t,
            "Meetings",
            Some("Record, take notes, save them to your vault.".into()),
            vec![],
        );
        if !self.enabled() {
            let this = cx.entity().downgrade();
            return page(
                head,
                div().p(space::s10()).child(ui::empty_state(
                    &t,
                    "users",
                    "Meetings are turned off",
                    "Meetings record a call, fill in your notes from the transcript and save them as Markdown in your vault. Turning this on takes effect after LocalFlow restarts.",
                    Some(
                        Button::new("meet-enable", "Turn on meetings")
                            .primary()
                            .on_click(move |_, _, cx| {
                                let _ = this.update(cx, |p, cx| {
                                    match p.pcx.services.apply(&json!({"features": {"meetings": true}})) {
                                        Ok(_) => p.pcx.toast("Meetings turned on. Restart LocalFlow to apply.", cx),
                                        Err(e) => p.pcx.toast(format!("Couldn't save: {e:#}"), cx),
                                    }
                                    p.pcx.config_changed(cx);
                                    p.refresh(cx);
                                });
                            })
                            .into_any_element(),
                    ),
                )),
            );
        }
        let health = self.pcx.health(cx);
        let config = self.pcx.services.config();
        let address = self.pcx.services.remote.base_url(&config);
        let pcx = self.pcx.clone();
        let banner = if health == Health::Simulated {
            Some(sandbox_note(&t))
        } else {
            service_banner(&t, &health, &address, move |_, _, cx| {
                pcx.recheck_health(cx)
            })
        };
        let linux = (!cfg!(target_os = "macos")).then(|| {
            ui::supporting(&t, "Meeting detection isn't available on Linux yet: start meetings here. System audio needs a monitor input set in Settings → Meetings.")
        });
        let list = self.render_recent(cx);
        let detail = self.render_detail(cx);
        page(
            head,
            div()
                .flex()
                .flex_col()
                .size_full()
                .children(banner.map(|b| div().px(space::s10()).pt(space::s5()).child(b)))
                .children(linux.map(|l| div().px(space::s10()).pt(space::s4()).child(l)))
                .child(split(&t, list, detail)),
        )
    }
}

#[cfg(test)]
mod speaker_tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use lf_ui::backends::meeting::{FakeMeetingClient, MeetingCall};

    use super::MeetingsPage;
    use crate::app::{focus_tests::open_with, Shell, View};
    use crate::page::Page;
    use crate::remote::Remote;
    use crate::sandbox::{FIXTURE_RECENT_ID, FIXTURE_SAVED_ID};

    fn page(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<MeetingsPage> {
        shell.read_with(cx, |shell, _| match shell.view(Page::Meetings) {
            Some(View::Meetings(v)) => v.clone(),
            _ => panic!("Meetings is open"),
        })
    }

    fn fake(
        shell: &Entity<Shell>,
        cx: &mut VisualTestContext,
    ) -> std::sync::Arc<FakeMeetingClient> {
        shell.read_with(cx, |shell, _| match &shell.cx.services.remote {
            Remote::Sandbox { meetings, .. } => meetings.clone(),
            Remote::Http { .. } => panic!("sandbox"),
        })
    }

    fn speaker_calls(client: &FakeMeetingClient) -> usize {
        client
            .calls()
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    MeetingCall::Speakers(_)
                        | MeetingCall::RenameSpeaker { .. }
                        | MeetingCall::MergeSpeakers { .. }
                        | MeetingCall::DiarizationStatus
                )
            })
            .count()
    }

    /// A fixture sandbox with the Meetings page loaded (idle, not Loading).
    fn open_fixture(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Shell>,
        Entity<MeetingsPage>,
        &mut VisualTestContext,
        tempfile::TempDir,
    ) {
        let (shell, cx, dir) = open_with(
            Page::Meetings,
            |root| crate::sandbox::add_fixture(root).unwrap(),
            cx,
        );
        let meetings = page(&shell, cx);
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert!(p.enabled(), "the fixture shows Meetings on, on every OS");
            assert_eq!(p.phase, super::Phase::Idle);
        });
        (shell, meetings, cx, dir)
    }

    /// Flags off (the default sandbox): Meetings makes no speaker request
    /// and draws no speaker panel or Recent action.
    #[gpui::test]
    fn the_default_sandbox_never_asks_about_speakers(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open_with(Page::Meetings, |_| {}, cx);
        let meetings = page(&shell, cx);
        // Outside macOS every config load turns meetings off; turn them on in
        // memory (speaker flags stay off) so the page really polls.
        meetings.update(cx, |p, cx| {
            let mut config = p.pcx.services.config();
            assert_eq!(config.field_bool("speaker_diarization"), Some(false));
            config.set_field("meetings_enabled", serde_json::json!(true));
            p.pcx.services.set_cached_config(config);
            p.refresh(cx);
        });
        cx.run_until_parked();
        let client = fake(&shell, cx);
        assert!(client.calls().contains(&MeetingCall::Status));
        assert_eq!(speaker_calls(&client), 0);
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.speaker_panel(), None);
            assert!(p
                .recent
                .0
                .iter()
                .all(|n| p.speakers.recent_action(&n.path, false).is_none()));
        });
    }

    /// The fixture: Recent "Speakers…" opens the panel; a rename goes to
    /// the picked speaker of that meeting and waits as a pending write.
    #[gpui::test]
    fn a_recent_panel_renames_its_own_meeting(cx: &mut TestAppContext) {
        let (shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.open_first_recent_speakers(cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_RECENT_ID));
            assert_eq!(p.speakers.speakers_of(FIXTURE_RECENT_ID).len(), 2);
        });
        meetings.update(cx, |p, cx| {
            assert!(p.speakers.begin_rename(FIXTURE_RECENT_ID, "S1"));
            p.speaker_name
                .update(cx, |i, cx| i.set_text("Ana Lima", cx));
            p.commit_speaker(cx);
        });
        cx.run_until_parked();
        let client = fake(&shell, cx);
        assert!(client.calls().contains(&MeetingCall::RenameSpeaker {
            meeting_id: FIXTURE_RECENT_ID.into(),
            speaker_id: "S1".into(),
            name: "Ana Lima".into(),
        }));
        meetings.read_with(cx, |p, _| {
            assert_eq!(
                p.speakers.speakers_of(FIXTURE_RECENT_ID)[0].label,
                "Ana Lima"
            );
            assert!(!p.speakers.editor.busy);
        });
    }

    /// The fixture: a stop saved with speakers shows the post-stop panel.
    #[gpui::test]
    fn a_labelled_stop_shows_the_saved_panel(cx: &mut TestAppContext) {
        let (shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert!(
                matches!(p.phase, super::Phase::Saved { .. }),
                "{:?}",
                p.phase
            );
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_SAVED_ID));
        });
        let calls = fake(&shell, cx).calls();
        let start = calls
            .iter()
            .position(|c| matches!(c, MeetingCall::Start { .. }));
        let stop = calls.iter().position(|c| *c == MeetingCall::Stop);
        assert!(start.is_some() && start < stop, "{calls:?}");
    }

    /// Recent panel open -> Start -> Stop of a labelled meeting shows the
    /// just-saved meeting; a Recent panel opened afterwards survives polls.
    #[gpui::test]
    fn a_stop_after_a_recent_panel_presents_the_saved_meeting(cx: &mut TestAppContext) {
        let (_shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.open_first_recent_speakers(cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_RECENT_ID));
        });
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert!(matches!(p.phase, super::Phase::Saved { .. }));
            assert_eq!(p.speakers.open_recent(), None);
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_SAVED_ID));
        });

        meetings.update(cx, |p, cx| p.toggle_recent_speakers(FIXTURE_RECENT_ID, cx));
        cx.run_until_parked();
        for _ in 0..3 {
            meetings.update(cx, |p, cx| p.poll(false, cx));
            cx.run_until_parked();
        }
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_RECENT_ID));
        });
    }

    /// Another client merges away the chosen merge target: the next status
    /// poll updates the picker to the placeholder and drops the choice.
    #[gpui::test]
    fn a_poll_refreshes_the_merge_picker_when_its_target_vanishes(cx: &mut TestAppContext) {
        use crate::ui::select::SelectEvent;
        use lf_ui::backends::meeting::MeetingClient;
        let (shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        meetings.update(cx, |p, cx| {
            p.begin_speaker_merge(FIXTURE_SAVED_ID, "S1", cx)
        });
        let (select, titles) =
            meetings.read_with(cx, |p, _| (p.merge_select.clone(), p.merge_titles.clone()));
        assert_eq!(titles[1], "Priya Raman");
        select.update(cx, |s, cx| {
            s.set_selected(Some(1), cx);
            cx.emit(SelectEvent::Changed(1));
        });
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.speakers.merge_choice, "S2");
            assert_eq!(p.merge_titles, titles, "an ordinary pick doesn't resync");
        });

        fake(&shell, cx)
            .merge_speakers(FIXTURE_SAVED_ID, "S2", "S1")
            .unwrap();
        meetings.update(cx, |p, cx| p.poll(false, cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, cx| {
            assert_eq!(p.speakers.merge_choice, "");
            assert_eq!(
                p.merge_titles,
                vec![lf_ui::hub_model::MERGE_PLACEHOLDER.to_string()]
            );
            assert_eq!(p.merge_select.read(cx).selected(), Some(0));
        });
        let merges = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
            fake(shell, cx)
                .calls()
                .iter()
                .filter(|c| matches!(c, MeetingCall::MergeSpeakers { .. }))
                .count()
        };
        let before = merges(&shell, cx);
        meetings.update(cx, |p, cx| p.commit_speaker(cx));
        cx.run_until_parked();
        assert_eq!(merges(&shell, cx), before, "nothing is merged by itself");
    }

    /// Meeting B, saved by another client.
    const ELSEWHERE_ID: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";

    fn elsewhere() -> serde_json::Value {
        serde_json::json!({
            "title": "Elsewhere", "notes_path": "/elsewhere.md",
            "meeting_id": ELSEWHERE_ID, "diarization": "ok",
            "speakers": [{"id": "S1", "label": "Bo", "seconds": 10.0}],
        })
    }

    /// Local Stop of meeting A, then idle polls report B as `last_saved`:
    /// the kept "Notes saved" banner keeps A's panel and edits go to A.
    /// "New meeting" drops the banner and the next poll adopts B.
    #[gpui::test]
    fn idle_polls_for_another_meeting_keep_the_local_saved_panel(cx: &mut TestAppContext) {
        let (shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        fake(&shell, cx).set_last_saved(Some(elsewhere()));
        for _ in 0..3 {
            meetings.update(cx, |p, cx| p.poll(false, cx));
            cx.run_until_parked();
        }
        meetings.read_with(cx, |p, _| {
            assert!(
                matches!(p.phase, super::Phase::Saved { .. }),
                "{:?}",
                p.phase
            );
            assert_eq!(p.speaker_panel().as_deref(), Some(FIXTURE_SAVED_ID));
            assert!(p.speakers.speakers_of(FIXTURE_SAVED_ID).len() > 1);
        });

        meetings.update(cx, |p, cx| p.new_meeting(cx));
        meetings.update(cx, |p, cx| p.poll(false, cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert_eq!(p.phase, super::Phase::Idle);
            assert_eq!(p.speaker_panel().as_deref(), Some(ELSEWHERE_ID));
        });
    }

    /// A local save without speaker labels shows no panel, even when a
    /// poll then reports a labelled meeting saved elsewhere.
    #[gpui::test]
    fn an_unlabelled_local_save_never_shows_another_meetings_speakers(cx: &mut TestAppContext) {
        let (shell, meetings, cx, _dir) = open_fixture(cx);
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        // The fixture labels only its first stop: this one is unlabelled.
        meetings.update(cx, |p, cx| p.start_and_stop(cx));
        cx.run_until_parked();
        meetings.read_with(cx, |p, _| {
            assert!(
                matches!(p.phase, super::Phase::Saved { .. }),
                "{:?}",
                p.phase
            );
            assert_eq!(p.speaker_panel(), None);
        });
        fake(&shell, cx).set_last_saved(Some(elsewhere()));
        for _ in 0..2 {
            meetings.update(cx, |p, cx| p.poll(false, cx));
            cx.run_until_parked();
        }
        meetings.read_with(cx, |p, _| {
            assert!(matches!(p.phase, super::Phase::Saved { .. }));
            assert_eq!(p.speaker_panel(), None);
        });
    }
}
