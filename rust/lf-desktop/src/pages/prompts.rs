//! Prompts: review work prompts drafted from meetings (local service queue).

use std::rc::Rc;

use gpui::{div, prelude::*, Context, Window};
use lf_ui::hub_model::{self, PromptRow};
use serde_json::json;

use crate::app::PageCx;
use crate::controllers::prompts as ctl;
use crate::page::Page;
use crate::pages::{column, header, page, sandbox_note, service_banner};
use crate::remote::Health;
use crate::theme::{space, theme, Tone};
use crate::ui::{self, Button, Switch};

pub struct PromptsPage {
    pcx: PageCx,
    rows: Vec<PromptRow>,
    filter: Option<&'static str>,
    loading: bool,
    error: Option<String>,
    notice: Option<(Tone, String)>,
    busy: Option<String>,
    generation: u64,
}

impl PromptsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let mut page = PromptsPage {
            pcx,
            rows: Vec::new(),
            filter: None,
            loading: true,
            error: None,
            notice: None,
            busy: None,
            generation: 0,
        };
        page.load(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let queue = self
            .pcx
            .services
            .remote
            .prompts(&self.pcx.services.config());
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ctl::load(queue.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(rows) => {
                        this.pcx.set_pending_prompts(ctl::pending_count(&rows), cx);
                        this.rows = rows;
                        this.error = None;
                    }
                    Err(e) => {
                        this.rows.clear();
                        this.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn review(&mut self, id: String, approve: bool, cx: &mut Context<Self>) {
        let queue = self
            .pcx
            .services
            .remote
            .prompts(&self.pcx.services.config());
        self.busy = Some(id.clone());
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { ctl::review(queue.as_ref(), &id, approve) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.notice = Some(match outcome {
                    Ok(()) => (
                        Tone::Success,
                        if approve {
                            "Prompt approved. Run lf prompts dispatch --all to send all approved prompts."
                        } else {
                            "Prompt rejected."
                        }
                        .into(),
                    ),
                    Err(message) => (Tone::Warning, message),
                });
                this.load(cx);
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for PromptsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let health = self.pcx.health(cx);
        let config = self.pcx.services.config();
        let address = self.pcx.services.remote.base_url(&config);
        let refresh = Button::new("prompts-refresh", "Refresh")
            .icon("refresh-cw")
            .busy(self.loading)
            .on_click(cx.listener(|this, _, _, cx| {
                this.pcx.recheck_health(cx);
                this.load(cx);
            }))
            .into_any_element();
        let mut body = div();
        if health == Health::Simulated {
            body = body.child(sandbox_note(&t));
        } else if let Some(b) = {
            let pcx = self.pcx.clone();
            service_banner(&t, &health, &address, move |_, _, cx| {
                pcx.recheck_health(cx)
            })
        } {
            body = body.child(b);
        }
        if !config.field_bool("work_prompts").unwrap_or(true) {
            let this = cx.entity().downgrade();
            body = body.child(ui::banner(
                &t,
                Tone::Info,
                "Work prompts are off, so meetings won't draft new prompts.",
                Some(
                    Switch::new("work-prompts", false, "Work prompts")
                        .on_toggle(move |on, _, cx| {
                            let _ = this.update(cx, |p, cx| {
                                if let Err(e) = p
                                    .pcx
                                    .services
                                    .apply(&json!({"meetings": {"work_prompts": on}}))
                                {
                                    p.pcx.toast(format!("Couldn't save: {e:#}"), cx);
                                }
                                p.pcx.config_changed(cx);
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                ),
            ));
        }
        if let Some((tone, msg)) = self.notice.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        let this = cx.entity().downgrade();
        let mut items: Vec<(Option<&'static str>, &'static str)> = vec![(None, "All")];
        for (status, _) in hub_model::PROMPT_STATUS_PILLS {
            items.push((
                Some(status),
                match status {
                    "pending" => "Pending",
                    "approved" => "Approved",
                    "rejected" => "Rejected",
                    "dispatched" => "Dispatched",
                    _ => "Failed",
                },
            ));
        }
        body = body.child(ui::tabs(
            &t,
            "prompt-filter",
            &items,
            self.filter,
            ui::TabDensity::Regular,
            Rc::new(move |f, _, cx| {
                let _ = this.update(cx, |p, cx| {
                    p.filter = f;
                    cx.notify();
                });
            }),
        ));
        if let Some(err) = self.error.clone() {
            body = body.child(ui::empty_state(
                &t,
                "circle-alert",
                "Couldn't load prompts",
                err,
                Some(
                    Button::new("prompts-retry", "Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                        .into_any_element(),
                ),
            ));
        } else {
            let rows = ctl::filter(&self.rows, self.filter);
            if rows.is_empty() && !self.loading {
                let pcx = self.pcx.clone();
                body = body.child(ui::empty_state(
                    &t,
                    "list-checks",
                    if self.filter.is_some() {
                        "Nothing with this status"
                    } else {
                        "No prompts yet"
                    },
                    hub_model::PROMPTS_EMPTY,
                    Some(
                        Button::new("prompts-meetings", "Open Meetings")
                            .on_click(move |_, _, cx| pcx.navigate(Page::Meetings, None, cx))
                            .into_any_element(),
                    ),
                ));
            } else {
                let mut list = ui::list(&t);
                for (i, row) in rows.into_iter().enumerate() {
                    let tone = match row.status.as_str() {
                        "pending" => Tone::Warning,
                        "approved" => Tone::Success,
                        "dispatched" => Tone::Info,
                        "failed" => Tone::Error,
                        _ => Tone::Neutral,
                    };
                    let busy = self.busy.as_deref() == Some(row.id.as_str());
                    let (a, r) = (row.id.clone(), row.id.clone());
                    if i > 0 {
                        list = list.child(ui::separator(&t, ui::row_inset()));
                    }
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s5())
                            .pl(ui::row_inset())
                            .pr(space::s4())
                            .py(space::s5())
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(space::s1())
                                    .flex_1()
                                    .min_w_0()
                                    .child(ui::label(&t, row.title.clone()))
                                    .child(ui::supporting(&t, row.context.clone())),
                            )
                            .child(ui::token(&t, tone, row.status_label.clone()))
                            .when(row.can_approve, |d| {
                                d.child(
                                    Button::new(("approve", i as u64), "Approve")
                                        .small()
                                        .busy(busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.review(a.clone(), true, cx)
                                        })),
                                )
                            })
                            .when(row.can_reject, |d| {
                                d.child(
                                    Button::new(("reject", i as u64), "Reject")
                                        .small()
                                        .ghost()
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.review(r.clone(), false, cx)
                                        })),
                                )
                            }),
                    );
                }
                body = body.child(list);
            }
        }
        page(
            header(
                &t,
                "Prompts",
                Some("Run lf prompts dispatch --all to send all approved prompts.".into()),
                vec![refresh],
            ),
            column("prompts-scroll", body),
        )
    }
}
