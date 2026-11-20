//! Ask: questions about your own dictations and notes through the local
//! service's `/api/recall` (HTTP adapter from `lf_ui::backends::http`).

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use lf_ui::backends::ask::{
    citation_document, citation_label, citation_target, CitationTarget, Turn, EXAMPLES, INTRO,
    PRIVACY, SOURCES, TIME_FILTERS,
};

use crate::app::PageCx;
use crate::pages::{header, page, sandbox_note, service_banner};
use crate::remote::Health;
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::Select;
use crate::ui::{self, Button};

pub struct AskPage {
    pcx: PageCx,
    composer: Entity<TextInput>,
    time: Entity<Select>,
    turns: Vec<Turn>,
    waiting: Option<String>,
    generation: u64,
}

impl AskPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new(cx, "Ask about your dictations and notes")
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
        });
        AskPage {
            pcx,
            composer,
            time,
            turns: Vec::new(),
            waiting: None,
            generation: 0,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn new_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |c, cx| c.clear(cx));
        self.composer.read(cx).focus(window);
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let question = self.composer.read(cx).text().trim().to_string();
        if question.is_empty() || self.waiting.is_some() {
            return;
        }
        if !self.pcx.health(cx).usable() {
            self.pcx.recheck_health(cx);
        }
        let filter = TIME_FILTERS[self.time.read(cx).selected().unwrap_or(0)]
            .0
            .to_string();
        self.generation += 1;
        let generation = self.generation;
        self.waiting = Some(question.clone());
        self.composer.update(cx, |c, cx| c.clear(cx));
        let remote = self.pcx.services.remote.clone();
        let config = self.pcx.services.config();
        cx.spawn(async move |this, cx| {
            let turn = cx
                .background_executor()
                .spawn(async move { remote.ask(&config, &question, &filter) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return; // stopped
                }
                this.waiting = None;
                this.turns.push(turn);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        if let Some(q) = self.waiting.take() {
            self.composer.update(cx, |c, cx| c.set_text(q, cx));
        }
        cx.notify();
    }

    fn open_citation(&mut self, turn: usize, index: usize, cx: &mut Context<Self>) {
        let Some(c) = self
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

    fn render_turn(&self, i: usize, turn: &Turn, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut answer = div().flex().flex_col().gap(space::s4());
        if !turn.error.is_empty() {
            answer = answer.child(ui::banner(&t, Tone::Error, turn.error.clone(), None));
        } else {
            answer = answer.child(ui::reading(&t, turn.answer.clone()));
        }
        if !turn.citations.is_empty() {
            let mut list = div().flex().flex_col().gap(space::s1());
            for (j, c) in turn.citations.iter().enumerate() {
                let label = citation_label(c).unwrap_or_else(|_| format!("[{}] {}", c.n, c.title));
                list = list.child(
                    ui::row_button(&t, ("cite", (i * 100 + j) as u64), false)
                        .flex_col()
                        .px(space::s4())
                        .py(space::s3())
                        .rounded(size::radius_element())
                        .on_click(cx.listener(move |this, _, _, cx| this.open_citation(i, j, cx)))
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
        div()
            .flex()
            .flex_col()
            .gap(space::s5())
            .pb(space::s8())
            .border_b_1()
            .border_color(t.border)
            .child(
                div().flex().justify_end().child(
                    div()
                        .max_w(px(520.))
                        .px(space::s6())
                        .py(space::s4())
                        .rounded(size::radius_container())
                        .bg(t.accent_muted)
                        .child(ui::body(&t, turn.question.clone())),
                ),
            )
            .child(answer)
    }
}

impl Render for AskPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
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
        let clear = (!self.turns.is_empty()).then(|| {
            Button::new("ask-clear", "New conversation")
                .ghost()
                .icon("plus")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.turns.clear();
                    this.generation += 1;
                    this.waiting = None;
                    cx.notify();
                }))
                .into_any_element()
        });
        let mut convo = div().flex().flex_col().gap(space::s8());
        if self.turns.is_empty() && self.waiting.is_none() {
            let examples = EXAMPLES.iter().enumerate().map(|(i, ex)| {
                let text = ex.to_string();
                Button::new(("example", i as u64), *ex)
                    .small()
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
        for (i, turn) in self.turns.iter().enumerate() {
            convo = convo.child(self.render_turn(i, turn, cx));
        }
        if let Some(q) = &self.waiting {
            convo = convo.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s4())
                    .child(div().flex().justify_end().child(ui::body(&t, q.clone())))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::icon("loader-circle", t.icon_accent))
                            .child(ui::secondary(&t, "Searching your dictations and notes…"))
                            .child(
                                Button::new("ask-stop", "Stop")
                                    .small()
                                    .icon("square")
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            ),
                    ),
            );
        }
        let sources = div()
            .flex()
            .items_center()
            .gap(space::s3())
            .child(ui::supporting(&t, "Sources:"))
            .children(
                SOURCES
                    .iter()
                    .map(|(_, name)| ui::token(&t, Tone::Neutral, *name)),
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
                div()
                    .flex()
                    .items_end()
                    .gap(space::s4())
                    .child(div().flex_1().child(self.composer.clone()))
                    .child(
                        Button::new("ask-send", "Send")
                            .primary()
                            .icon("send")
                            .busy(self.waiting.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::s6())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::supporting(&t, "Time"))
                            .child(self.time.clone()),
                    )
                    .child(div().id("ask-sources").child(sources).tooltip(|_, cx| {
                        ui::Tooltip::view(
                            "All sources are searched. Filtering by source isn't available yet."
                                .into(),
                            cx,
                        )
                    })),
            )
            .child(ui::supporting(
                &t,
                format!("{PRIVACY} Questions are kept until you close this window."),
            ));
        page(
            header(
                &t,
                "Ask",
                Some("Answers from your own dictations, meetings and notes.".into()),
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
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(
                            div()
                                .flex()
                                .justify_center()
                                .px(space::s10())
                                .py(space::s8())
                                .child(convo.w_full().max_w(px(size::COLUMN))),
                        ),
                )
                .child(composer),
        )
    }
}
