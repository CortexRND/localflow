//! Meetings: start/stop a meeting on the local service, take notes (autosaved
//! in order through `MeetingNotesWriter`), ask about the saved meeting, and
//! open recent notes from the vault.

use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use lf_ui::backends::meeting::{vault_notes_path, MeetingNotesWriter, MAX_USER_NOTES_CHARS};
use lf_ui::hub_model::{self, RecentNote};
use serde_json::json;

use crate::app::PageCx;
use crate::controllers::meetings::{self as ctl, NotesSync, Phase, Template};
use crate::page::Page;
use crate::pages::{header, page, sandbox_note, service_banner, split};
use crate::remote::Health;
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::Select;
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
        cx.spawn(async move |this, cx| {
            let snap = cx
                .background_executor()
                .spawn(async move { ctl::poll(client.as_ref(), with_templates) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.busy {
                    return;
                }
                let keep = matches!(this.phase, Phase::Saved { .. } | Phase::Error(_))
                    && snap.phase == Phase::Idle;
                if !keep {
                    this.phase = snap.phase;
                }
                if matches!(this.phase, Phase::Recording { .. }) {
                    let local = this.notes.read(cx).text().to_string();
                    if let Some(server) = this.sync.adopt(sent, &local, snap.notes.as_deref()) {
                        this.notes.update(cx, |i, cx| i.set_text(server, cx));
                    } else if this.sync.is_dirty() && snap.notes.as_deref() != Some(local.as_str())
                    {
                        // Local edits the server hasn't got yet (e.g. typed
                        // before this meeting was found running): send them.
                        this.schedule_notes_save(cx);
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
        cx.spawn(async move |this, cx| {
            let phase = cx
                .background_executor()
                .spawn(async move { ctl::start(client.as_ref(), &title, &template, &notes) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.phase = phase;
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
                let token = this.sync.edited_token();
                cx.spawn(async move |this, cx| {
                    let ok = cx
                        .background_executor()
                        .spawn(async move { writer.save(client.as_ref(), ticket, &text).is_ok() })
                        .await;
                    let _ = this.update(cx, |this, _| this.sync.save_finished(token, ok));
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
        self.phase = Phase::Stopping;
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let phase = cx
                .background_executor()
                .spawn(async move { ctl::stop(client.as_ref(), &writer, ticket, &notes) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
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

    fn new_meeting(&mut self, cx: &mut Context<Self>) {
        self.title.update(cx, |i, cx| i.clear(cx));
        self.notes.update(cx, |i, cx| i.clear(cx));
        self.sync.reset();
        self.phase = Phase::Idle;
        cx.notify();
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
        for (i, note) in self.recent.0.iter().enumerate() {
            let path = note.path.clone();
            list = list.child(
                ui::row_button(&t, ("note", i as u64), false)
                    .flex_col()
                    .px(space::s6())
                    .py(space::s4())
                    .border_b_1()
                    .border_color(t.border)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_note(&path, cx)))
                    .child(ui::label(&t, note.title.clone()))
                    .child(ui::supporting(&t, note.date_label.clone())),
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
