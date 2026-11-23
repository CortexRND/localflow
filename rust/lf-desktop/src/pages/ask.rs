//! Ask: questions about your own dictations and work prompts, answered by
//! the configured model through LocalFlow's read-only MCP tools
//! (`controllers::ask_agent`), or by the service's `/api/recall` when the
//! tools are missing or fail. The conversation and tool cards live in
//! `controllers::ask::Conversation`; a worker thread streams events into it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, Pixels, SharedString, Window};
use lf_ui::backends::ask::{
    citation_document, citation_label, citation_target, CitationTarget, Turn, ERROR_GENERIC,
    EXAMPLES, INTRO, PRIVACY, SOURCES, SOURCES_ONLY, THINKING, TIME_FILTERS,
};

use crate::app::PageCx;
use crate::controllers::ask::{
    restored_text, Activity, AgentTurn, CardStatus, Conversation, StatusTone,
};
use crate::controllers::ask_agent::{self, AgentEvent, Answer, Question, Via, LIMITS};
use crate::controllers::mcp_client::McpStatus;
use crate::pages::{header, page, sandbox_note, service_banner};
use crate::remote::Health;
use crate::theme::{size, space, theme, Theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::Select;
use crate::ui::{self, Button};

/// Sandbox only: how long each fake tool call and model step takes, so the
/// thinking and running states can be seen. Real mode ignores it.
const SANDBOX_LATENCY: Duration = Duration::from_millis(450);

/// With tools, Ask searches only dictations and work prompts (no meeting
/// transcripts), so this replaces the shared meeting example.
const TOOL_MEETING_SUGGESTION: &str = "Which work prompts came out of my meetings?";

/// Empty-state suggestions: the shared `EXAMPLES` for basic recall; with
/// tools, any example that needs meeting transcripts is swapped for one the
/// tools can answer.
fn suggestions(tools_mode: bool) -> [&'static str; 3] {
    EXAMPLES.map(|ex| {
        if tools_mode && ex.to_lowercase().contains("meeting") {
            TOOL_MEETING_SUGGESTION
        } else {
            ex
        }
    })
}

/// The composer's Send/Stop slot (fits either button with its icon).
const ACTION_WIDTH: f32 = 96.;

/// `ui::row_button` on an opaque fill, with `ui::surface_ring`: the focus
/// ring is a shadow painted under the row, so a transparent row (or a
/// translucent hover/pressed fill) would show the ring colour through it
/// instead of an edge. Same tab stop and cursor; the hover and pressed
/// tints are pre-blended onto the surface the rows sit on (the content pane
/// and the composer). Callers round the row.
fn surface_row(t: &Theme, id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    let ring = ui::surface_ring(t);
    let (hover, pressed) = (t.bg_surface.blend(t.hover), t.bg_surface.blend(t.pressed));
    div()
        .id(id)
        .relative()
        .flex()
        .w_full()
        .tab_index(0)
        .cursor_pointer()
        .bg(t.bg_surface)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .focus(move |s| s.shadow(ring.clone()))
}

/// Sources the tools search (versus `SOURCES`, which basic recall searches).
const TOOL_SOURCES: [&str; 2] = ["Dictation", "Work prompt"];

pub struct AskPage {
    pcx: PageCx,
    composer: Entity<TextInput>,
    time: Entity<Select>,
    convo: Conversation,
    /// Stop flag of the running question.
    cancel: Option<Arc<AtomicBool>>,
    probe_generation: u64,
}

impl Drop for AskPage {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl AskPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new(cx, "Ask about your dictations and work prompts")
                .multiline(2, 8)
                .submit_on_enter()
                .max_chars(2000)
                .label("Question")
        });
        cx.subscribe(&composer, |this, _, e, cx| {
            if *e == InputEvent::Submit {
                this.send(cx);
            }
        })
        .detach();
        let time = cx.new(|cx| {
            Select::new(
                cx,
                TIME_FILTERS
                    .iter()
                    .map(|(n, _)| SharedString::from(*n))
                    .collect(),
                Some(0),
            )
            .width(150.)
            .surface_ring()
        });
        let mut page = AskPage {
            pcx,
            composer,
            time,
            convo: Conversation::default(),
            cancel: None,
            probe_generation: 0,
        };
        page.probe(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn new_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |c, cx| c.clear(cx));
        self.composer.read(cx).focus(window);
        cx.notify();
    }

    /// Escape: stop a running question. `false` when there was none.
    pub fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.convo.running() {
            self.stop(cx);
            true
        } else {
            false
        }
    }

    fn agent(&self) -> Box<dyn ask_agent::AgentBackend> {
        let services = &self.pcx.services;
        services
            .remote
            .agent(services.config(), services.secrets.clone(), SANDBOX_LATENCY)
    }

    /// Check once whether the tools start (connect, list, stop).
    fn probe(&mut self, cx: &mut Context<Self>) {
        self.probe_generation += 1;
        let generation = self.probe_generation;
        self.convo.status = McpStatus::Checking;
        let backend = self.agent();
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { ask_agent::probe(backend.as_ref(), &AtomicBool::new(false)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A question's own status is newer than a probe started before it.
                if this.probe_generation == generation && !this.convo.running() {
                    this.convo.status = status;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text().trim().to_string();
        if text.is_empty() || self.convo.running() {
            return;
        }
        if !self.pcx.health(cx).usable() {
            self.pcx.recheck_health(cx);
        }
        let question = Question {
            text: text.clone(),
            filter: TIME_FILTERS[self.time.read(cx).selected().unwrap_or(0)]
                .0
                .to_string(),
            context: self.convo.context(),
        };
        // A running probe must not overwrite this question's status.
        self.probe_generation += 1;
        let generation = self.convo.begin(&text);
        self.composer.update(cx, |c, cx| c.clear(cx));
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let backend = self.agent();
        let (tx, rx) = async_channel::unbounded::<AgentEvent>();
        let worker = std::thread::Builder::new()
            .name("ask-agent".into())
            .spawn(move || {
                ask_agent::answer(backend.as_ref(), &question, &LIMITS, &cancel, &mut |e| {
                    let _ = tx.send_blocking(e);
                });
            });
        if worker.is_err() {
            let mut turn = Turn::new(text);
            turn.error = ERROR_GENERIC.into();
            self.convo.apply(
                generation,
                AgentEvent::Finished(Answer {
                    turn,
                    note: None,
                    via: Via::Tools,
                }),
            );
            self.cancel = None;
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    if this.convo.apply(generation, event) {
                        if !this.convo.running() {
                            this.cancel = None;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            // Closed without `Finished` (worker gone): don't stay pending.
            // A stopped or newer run has another generation and is untouched.
            let _ = this.update(cx, |this, cx| {
                if this.convo.abandon(generation) {
                    this.cancel = None;
                    this.recheck_if_stale(cx);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let stopped = self.convo.stop();
        // Never overwrite a new question typed while this one ran.
        if let Some(q) = restored_text(self.composer.read(cx).text(), stopped) {
            self.composer.update(cx, |c, cx| c.set_text(q, cx));
        }
        self.recheck_if_stale(cx);
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.convo.clear();
        self.recheck_if_stale(cx);
        cx.notify();
    }

    /// A question stopped before it reported the tools' status invalidated
    /// the page's probe: check again rather than show "Checking" forever.
    fn recheck_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.convo.needs_probe() {
            self.probe(cx);
        }
    }

    fn open_citation(&mut self, turn: usize, index: usize, cx: &mut Context<Self>) {
        let Some(c) = self
            .convo
            .turns
            .get(turn)
            .and_then(|t| t.citations.get(index))
            .cloned()
        else {
            return;
        };
        match citation_target(&c) {
            Ok(CitationTarget::History(id)) => {
                self.pcx.open_history(id, cx);
            }
            Ok(CitationTarget::File(path)) => {
                let vault = self
                    .pcx
                    .services
                    .config()
                    .field_str("vault_path")
                    .unwrap_or("")
                    .to_string();
                match citation_document(&path, &vault) {
                    Some(doc) => cx.open_with_system(&doc),
                    None => self.pcx.toast("That source isn't a document inside your notes vault, so it wasn't opened.", cx),
                }
            }
            Err(e) => self
                .pcx
                .toast(format!("Couldn't open that source: {e}"), cx),
        }
    }

    /// The tools are answering (versus basic recall).
    fn tools_mode(&self) -> bool {
        matches!(
            self.convo.status,
            McpStatus::Connected { .. } | McpStatus::Simulated
        )
    }

    /// Tool cards of one turn (`index` `None` = the running turn).
    fn render_cards(
        &self,
        t: &Theme,
        index: Option<usize>,
        turn: &AgentTurn,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if turn.cards.is_empty() {
            return None;
        }
        let key = index.unwrap_or(self.convo.turns.len()) as u64;
        let mut list = div().flex().flex_col().gap(space::s2());
        if turn.grouped() {
            let running = turn.cards.iter().any(|c| c.status == CardStatus::Running);
            let failed = turn
                .cards
                .iter()
                .filter(|c| c.status == CardStatus::Error)
                .count();
            let mut words = format!("{} tool calls", turn.cards.len());
            if running {
                words.push_str(" · running");
            } else if failed > 0 {
                words.push_str(&format!(" · {failed} failed"));
            }
            list = list.child(
                surface_row(t, ("ask-cards", key))
                    .items_center()
                    .gap(space::s3())
                    .px(space::s4())
                    .py(space::s2())
                    .rounded(size::radius_element())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.convo.toggle_group(index);
                        cx.notify();
                    }))
                    .child(ui::icon("list-checks", t.icon_secondary))
                    .child(ui::secondary(t, words))
                    .child(ui::icon(
                        if turn.cards_open {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        t.icon_secondary,
                    )),
            );
        }
        if turn.cards_visible() {
            for (j, card) in turn.cards.iter().enumerate() {
                let (icon_color, word_color) = match card.status {
                    CardStatus::Running => (t.icon_accent, t.text_secondary),
                    CardStatus::Ok => (t.tone(Tone::Success).0, t.text_secondary),
                    CardStatus::Error => (t.tone(Tone::Error).0, t.tone(Tone::Error).0),
                };
                let mut result = card.summary.clone();
                if let Some(ms) = card.millis.filter(|_| card.status != CardStatus::Running) {
                    if !result.is_empty() {
                        result.push_str(" · ");
                    }
                    result.push_str(&format!("{ms} ms"));
                }
                let row = surface_row(t, ("ask-card", key * 1000 + j as u64))
                    .items_center()
                    .gap(space::s3())
                    .px(space::s4())
                    .py(space::s2())
                    .rounded(size::radius_element())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.convo.toggle_card(index, j);
                        cx.notify();
                    }))
                    .child(ui::icon(card.status.icon(), icon_color))
                    .child(ui::mono(t, card.name.clone()).flex_none())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(ui::supporting(t, card.args.clone())),
                    )
                    .when(!result.is_empty(), |d| {
                        d.child(ui::supporting(t, result).flex_none())
                    })
                    .child(
                        ui::supporting(t, card.status.word())
                            .flex_none()
                            .text_color(word_color)
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(ui::icon(
                        if card.expanded {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        t.icon_secondary,
                    ));
                let detail = card.expanded.then(|| {
                    let body = match (card.status, card.preview.is_empty()) {
                        (CardStatus::Running, _) => "Waiting for the result…".to_string(),
                        (_, true) => card.summary.clone(),
                        (_, false) => card.preview.clone(),
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::s2())
                        .ml(space::s10())
                        .px(space::s4())
                        .py(space::s3())
                        .rounded(size::radius_element())
                        .bg(t.bg_muted)
                        .when(!card.args.is_empty(), |d| {
                            d.child(ui::supporting(t, format!("Arguments: {}", card.args)))
                        })
                        .child(ui::mono(t, body))
                });
                list = list.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::s2())
                        .child(row)
                        .children(detail),
                );
            }
        }
        Some(list.into_any_element())
    }

    fn render_turn(
        &self,
        index: Option<usize>,
        turn: &AgentTurn,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut answer = div().flex().flex_col().gap(space::s4());
        if let Some(cards) = self.render_cards(&t, index, turn, cx) {
            answer = answer.child(cards);
        }
        match index {
            None => {
                let activity = match self.convo.activity() {
                    Activity::Tool(name) => format!("Running {name}…"),
                    _ => THINKING.trim_end_matches('.').to_string() + "…",
                };
                answer = answer.child(
                    div()
                        .id("ask-activity")
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .child(ui::icon("loader-circle", t.icon_accent))
                        .child(ui::secondary(&t, activity)),
                );
            }
            Some(i) => {
                if !turn.error.is_empty() {
                    answer = answer.child(ui::banner(&t, Tone::Error, turn.error.clone(), None));
                } else if turn.sources_only() {
                    // No model answer: no empty prose. The recall note says
                    // why; without one, say it here.
                    if turn.note.is_none() {
                        answer = answer.child(ui::secondary(&t, SOURCES_ONLY));
                    }
                } else {
                    answer = answer.child(ui::reading(&t, turn.answer.clone()));
                }
                if let Some(note) = &turn.note {
                    // Definite widths so the note wraps inside the column
                    // (an intrinsic-width text runs off the window).
                    let text_width = width - size::icon() - space::s3();
                    let text_width = if text_width > px(0.) {
                        text_width
                    } else {
                        px(0.)
                    };
                    answer = answer.child(
                        div()
                            .w(width)
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap(space::s3())
                            .child(ui::icon("info", t.icon_secondary).flex_none())
                            .child(
                                div()
                                    .w(text_width)
                                    .min_w_0()
                                    .flex_none()
                                    .whitespace_normal()
                                    .child(ui::supporting(&t, note.clone())),
                            ),
                    );
                }
                if !turn.citations.is_empty() {
                    let mut list = div().flex().flex_col().gap(space::s1());
                    for (j, c) in turn.citations.iter().enumerate() {
                        let label =
                            citation_label(c).unwrap_or_else(|_| format!("[{}] {}", c.n, c.title));
                        list = list.child(
                            surface_row(&t, ("cite", (i * 100 + j) as u64))
                                .flex_col()
                                .px(space::s4())
                                .py(space::s3())
                                .rounded(size::radius_element())
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_citation(i, j, cx)),
                                )
                                .child(ui::label(&t, label).text_color(t.text_accent))
                                .when(!c.snippet.is_empty(), |d| {
                                    d.child(ui::supporting(&t, c.snippet.clone()))
                                }),
                        );
                    }
                    answer = answer
                        .child(ui::supporting(&t, "Sources").font_weight(FontWeight::MEDIUM))
                        .child(list);
                }
            }
        }
        // Full width so the question bubble right-aligns in every state.
        div()
            .w(width)
            .flex()
            .flex_col()
            .gap(space::s5())
            .child(
                div().w(width).flex().flex_row().justify_end().child(
                    div()
                        .max_w(px(520.))
                        .px(space::s5())
                        .py(space::s3())
                        .rounded(size::radius_container())
                        .bg(t.accent_muted)
                        .child(ui::body(&t, turn.question.clone())),
                ),
            )
            .child(answer.w(width))
    }

    fn render_status(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let (word, tone) = self.convo.status_label();
        let tone = match tone {
            StatusTone::Neutral => Tone::Neutral,
            StatusTone::Good => Tone::Success,
            StatusTone::Warn => Tone::Warning,
            StatusTone::Bad => Tone::Error,
        };
        let detail = SharedString::from(self.convo.status_detail());
        let checkable = !matches!(
            self.convo.status,
            McpStatus::Checking | McpStatus::Simulated
        ) && !self.convo.running();
        surface_row(t, "ask-tools")
            .w_auto()
            .flex_none()
            .items_center()
            .px(space::s3())
            .py(space::s1())
            .rounded(size::radius_element())
            .child(ui::status_dot(t, tone, word))
            .tooltip(move |_, cx| ui::Tooltip::view(detail.clone(), cx))
            .when(checkable, |d| {
                d.on_click(cx.listener(|this, _, _, cx| this.probe(cx)))
            })
    }
}

impl Render for AskPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        // Definite pixel widths: percentage and auto widths are measured
        // intrinsically here, which left the question bubble, the empty state
        // and the input sized to their content. The pane is the window minus
        // the shell's sidebar; both the scroll line and the composer row are
        // inset by `s10` on each side.
        let at_least_zero = |w: Pixels| if w > px(0.) { w } else { px(0.) };
        let pane = at_least_zero(window.viewport_size().width - px(size::SIDEBAR));
        let inner = at_least_zero(pane - space::s10() * 2.);
        let column = if inner < px(size::COLUMN) {
            inner
        } else {
            px(size::COLUMN)
        };
        let field = at_least_zero(inner - px(ACTION_WIDTH) - space::s4());
        let health = self.pcx.health(cx);
        let config = self.pcx.services.config();
        let address = self.pcx.services.remote.base_url(&config);
        let pcx = self.pcx.clone();
        let tools_mode = self.tools_mode();
        // The service matters only when basic recall answers.
        let banner = if health == Health::Simulated {
            Some(sandbox_note(&t))
        } else if tools_mode {
            None
        } else {
            service_banner(&t, &health, &address, move |_, _, cx| {
                pcx.recheck_health(cx)
            })
        };
        let running = self.convo.running();
        let clear = (!self.convo.turns.is_empty() || running).then(|| {
            Button::new("ask-clear", "New conversation")
                .ghost()
                .surface_ring(t.bg_surface)
                .icon("plus")
                .on_click(cx.listener(|this, _, _, cx| this.clear(cx)))
                .into_any_element()
        });
        let mut convo = div().flex().flex_col().gap(space::s10());
        if self.convo.turns.is_empty() && !running {
            let examples = suggestions(tools_mode)
                .into_iter()
                .enumerate()
                .map(|(i, ex)| {
                    let text = ex.to_string();
                    Button::new(("example", i as u64), ex)
                        .small()
                        .surface_ring(t.bg_surface)
                        .icon("sparkles")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.composer
                                .update(cx, |c, cx| c.set_text(text.clone(), cx));
                            this.composer.read(cx).focus(window);
                        }))
                });
            convo = convo.child(ui::empty_state(
                &t,
                "message-circle-question",
                "Ask your history",
                INTRO,
                Some(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_center()
                        .gap(space::s4())
                        .children(examples)
                        .into_any_element(),
                ),
            ));
        }
        for (i, turn) in self.convo.turns.iter().enumerate() {
            convo = convo.child(self.render_turn(Some(i), turn, column, cx));
        }
        if let Some(turn) = &self.convo.pending {
            convo = convo.child(self.render_turn(None, turn, column, cx));
        }
        let (source_names, source_tip): (Vec<&'static str>, &'static str) = if tools_mode {
            (
                TOOL_SOURCES.to_vec(),
                "With tools, Ask searches your dictations and meeting work prompts. \
                 Meetings and notes are searched only by basic recall.",
            )
        } else {
            (
                SOURCES.iter().map(|(_, name)| *name).collect(),
                "All sources are searched. Filtering by source isn't available yet.",
            )
        };
        let sources = div()
            .flex()
            .items_center()
            .gap(space::s3())
            .child(ui::supporting(&t, "Sources:"))
            .children(
                source_names
                    .into_iter()
                    .map(|name| ui::token(&t, Tone::Neutral, name)),
            );
        let action = if running {
            Button::new("ask-stop", "Stop")
                .surface_ring(t.bg_surface)
                .icon("square")
                .tooltip("Stop (Esc)")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("ask-send", "Send")
                .primary()
                .surface_ring(t.bg_surface)
                .icon("send")
                .on_click(cx.listener(|this, _, _, cx| this.send(cx)))
        };
        // Always shown, before the first question too: tools can run while
        // their status is still being checked.
        let privacy = format!(
            "{PRIVACY} When LocalFlow's tools are available, it also receives each tool's \
             result (dictations and work prompts, up to {} characters per call), not only \
             the short preview in the card. Questions are kept until you close this window.",
            ask_agent::MODEL_RESULT_CHARS
        );
        let composer = div()
            .flex()
            .flex_col()
            .gap(space::s4())
            .px(space::s10())
            .py(space::s6())
            .border_t_1()
            .border_color(t.border)
            .bg(t.bg_surface)
            .child(
                // The input gets every pixel the button slot leaves (its
                // intrinsic width would be only the placeholder's).
                div()
                    .w(inner)
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(space::s4())
                    .child(
                        div()
                            .w(field)
                            .flex_none()
                            .min_w_0()
                            .child(self.composer.clone()),
                    )
                    .child(
                        div()
                            .w(px(ACTION_WIDTH))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .child(action),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(space::s6())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::supporting(&t, "Time"))
                            .child(self.time.clone())
                            .child(self.render_status(&t, cx)),
                    )
                    .child(
                        div()
                            .id("ask-sources")
                            .child(sources)
                            .tooltip(move |_, cx| ui::Tooltip::view(source_tip.into(), cx)),
                    ),
            )
            .child(ui::supporting(&t, privacy));
        page(
            header(
                &t,
                "Ask",
                Some("Answers from your own dictations, work prompts, meetings and notes.".into()),
                clear.into_iter().collect(),
            ),
            div()
                .flex()
                .flex_col()
                .size_full()
                .children(banner.map(|b| div().px(space::s10()).pt(space::s5()).child(b)))
                .child(
                    div()
                        .id("ask-scroll")
                        .w(pane)
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(
                            // The pane-wide line the column centres in.
                            div()
                                .w(pane)
                                .flex()
                                .flex_row()
                                .justify_center()
                                .px(space::s10())
                                .py(space::s8())
                                .child(convo.w(column)),
                        ),
                )
                .child(composer),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_suggestions_avoid_meeting_transcripts() {
        assert_eq!(suggestions(false), EXAMPLES);
        let tools = suggestions(true);
        assert!(tools.contains(&TOOL_MEETING_SUGGESTION));
        // Only the meeting example is replaced; the others keep their order.
        for (shared, shown) in EXAMPLES.iter().zip(tools) {
            if !shared.to_lowercase().contains("meeting") {
                assert_eq!(*shared, shown);
            }
        }
        assert!(!tools.contains(&EXAMPLES[0]), "the transcript question");
    }
}
