//! Home: dictation status, setup guidance, stats from the history store,
//! recent dictations and shortcuts.

use gpui::{div, prelude::*, px, ClipboardItem, Context, FontWeight, Window};
use lf_core::history::{HistoryEntry, Stats};
use lf_ui::hub_model;
use serde_json::json;

use crate::app::PageCx;
use crate::controllers::status::{self, Level};
use crate::controllers::{history, setup};
use crate::page::Page;
use crate::pages::{column, header, page};
use crate::remote::Health;
use crate::theme::{space, text, theme, Tone};
use crate::ui::{self, Button, IconButton, Switch};

pub struct HomePage {
    pcx: PageCx,
    stats: Option<Stats>,
    recent: Vec<HistoryEntry>,
    error: Option<String>,
    loading: bool,
    generation: u64,
    setup_open: bool,
    save_error: Option<String>,
}

impl HomePage {
    pub fn new(pcx: PageCx, setup_open: bool, cx: &mut Context<Self>) -> Self {
        let configured = setup::configured(&pcx.services.root);
        let mut page = HomePage {
            pcx,
            stats: None,
            recent: Vec::new(),
            error: None,
            loading: true,
            generation: 0,
            setup_open: setup_open || !configured,
            save_error: None,
        };
        page.refresh(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let root = self.pcx.services.root.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { history::load(&root, generation, "", 0, true) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(loaded) => {
                        this.stats = loaded.stats;
                        this.recent = loaded.entries.into_iter().take(5).collect();
                        this.error = None;
                    }
                    Err(err) => this.error = Some(format!("Couldn't read your history: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_dictation(&mut self, on: bool, cx: &mut Context<Self>) {
        match self
            .pcx
            .services
            .apply(&json!({"dictation": {"enabled": on}}))
        {
            Ok(_) => {
                self.save_error = None;
                self.pcx.toast(
                    if on {
                        "Dictation turned on"
                    } else {
                        "Dictation turned off"
                    },
                    cx,
                );
            }
            Err(err) => self.save_error = Some(format!("Couldn't save: {err:#}")),
        }
        self.pcx.config_changed(cx);
        cx.notify();
    }

    fn render_card(&self, health: &Health, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
        let key = hub_model::format_hotkey(config.field_str("hotkey"));
        let sandbox = self.pcx.services.is_sandbox();
        let address = self.pcx.services.remote.base_url(&config);
        let status = status::describe(
            sandbox,
            enabled,
            &key,
            health,
            &address,
            cfg!(target_os = "macos"),
        );
        let this = cx.entity().downgrade();
        let toggle =
            Switch::new("dictation-switch", enabled, "Dictation").on_toggle(move |on, _, cx| {
                let _ = this.update(cx, |p, cx| p.set_dictation(on, cx));
            });
        let how = (!sandbox && *health == Health::Offline).then(|| {
            Button::new("how-to-start", "How to start")
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.setup_open = true;
                    cx.notify();
                }))
        });
        let (sz, _) = text::supporting();
        ui::card(&t)
            .items_start()
            .gap(space::s6())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(40.))
                    .rounded_full()
                    .bg(t.accent_muted)
                    .child(ui::icon_lg("mic", t.icon_accent)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s2())
                    .flex_1()
                    .min_w_0()
                    .child(ui::heading(&t, status.lead))
                    .child(ui::secondary(&t, status.support))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s1())
                            .pt(space::s2())
                            .child(
                                ui::status_dot(&t, tone(status.service.0), status.service.1)
                                    .text_size(sz),
                            )
                            .child(
                                ui::status_dot(&t, Tone::Neutral, status.input)
                                    .text_size(sz)
                                    .text_color(t.text_secondary),
                            ),
                    )
                    .when_some(self.save_error.clone(), |d, e| {
                        d.child(ui::error_line(&t, e))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(space::s4())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::label(&t, "Dictation"))
                            .child(toggle),
                    )
                    .children(how),
            )
    }

    fn render_setup(&self, health: &Health, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let count = self.stats.as_ref().map_or(0, |s| s.total_dictations);
        let steps = setup::steps(&services.root, &services.config(), health, count);
        let done = steps
            .iter()
            .filter(|s| s.state == setup::State::Done)
            .count();
        let collapse = Button::new("setup-hide", "Hide")
            .ghost()
            .small()
            .on_click(cx.listener(|this, _, _, cx| {
                this.setup_open = false;
                cx.notify();
            }));
        let mut list = ui::list(&t);
        for (i, step) in steps.into_iter().enumerate() {
            let (icon, color) = match step.state {
                setup::State::Done => ("circle-check", t.icon_accent),
                setup::State::Todo => ("circle-alert", t.dot(Tone::Warning)),
                setup::State::Unavailable => ("info", t.icon_secondary),
            };
            let pcx = self.pcx.clone();
            let action = step.action.map(|(label, page, target)| {
                Button::new(("setup-action", i as u64), label)
                    .small()
                    .on_click(move |_, _, cx| pcx.navigate(page, target, cx))
            });
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s5())
                    .py(space::s5())
                    .when(i > 0, |d| d.border_t_1().border_color(t.border))
                    .child(ui::icon(icon, color))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(ui::label(&t, step.title))
                            .child(ui::supporting(&t, step.detail)),
                    )
                    .children(action),
            );
        }
        ui::section(
            &t,
            "Get set up",
            Some(format!("{done} of 6 ready. Full model downloads and the mic test run in `lf setup` for now.").into()),
            Some(collapse.into_any_element()),
        )
        .child(list)
    }

    fn render_stats(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let Some(stats) = &self.stats else {
            return div().child(ui::secondary(
                &t,
                if self.loading { "Loading stats…" } else { "" },
            ));
        };
        let cards = hub_model::home_stat_cards(stats);
        let (vs, vl) = text::stat();
        div()
            .flex()
            .flex_col()
            .gap(space::s4())
            .child(
                div()
                    .flex()
                    .w_full()
                    .children(cards.into_iter().enumerate().map(|(i, (name, value))| {
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(space::s2())
                            .px(space::s6())
                            .when(i > 0, |d| d.border_l_1().border_color(t.border))
                            .when(i == 0, |d| d.pl(px(0.)))
                            .child(
                                div()
                                    .text_size(vs)
                                    .line_height(vl)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(value),
                            )
                            .child(ui::supporting(&t, name))
                    })),
            )
            .child(ui::supporting(&t, hub_model::home_stats_summary(stats)))
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let pcx = self.pcx.clone();
        let view_all = Button::new("view-all", "View all")
            .ghost()
            .small()
            .icon("chevron-right")
            .on_click(move |_, _, cx| pcx.navigate(Page::History, None, cx));
        let section = ui::section(&t, "Recent", None, Some(view_all.into_any_element()));
        if let Some(err) = &self.error {
            return section.child(ui::banner(&t, Tone::Error, err.clone(), None));
        }
        if self.recent.is_empty() && !self.loading {
            let (h, d, _, _) = hub_model::history_empty_state(false, true);
            return section.child(ui::empty_state(
                &t,
                "history",
                h,
                d,
                Some(
                    Button::new("home-empty-setup", "Show setup steps")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.setup_open = true;
                            cx.notify();
                        }))
                        .into_any_element(),
                ),
            ));
        }
        let mut list = ui::list(&t);
        for (i, entry) in self.recent.iter().enumerate() {
            let id = entry.id;
            let text_value = history::row_text(entry);
            let chip = hub_model::history_status_chip(&entry.status);
            let copy_text = entry.final_text.clone();
            let open = self.pcx.clone();
            let pcx = self.pcx.clone();
            list = list.child(
                ui::row_button(&t, ("recent", id as u64), false)
                    .items_center()
                    .gap(space::s6())
                    .h(px(56.))
                    .px(space::s4())
                    .when(i > 0, |d| d.border_t_1().border_color(t.border))
                    .on_click(move |_, _, cx| {
                        open.open_history(id, cx);
                    })
                    .child(
                        ui::supporting(&t, history::time_label(entry.ts))
                            .w(px(72.))
                            .flex_none(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(text_value),
                    )
                    .when_some(chip, |d, (label, failed)| {
                        d.child(ui::token(
                            &t,
                            if failed { Tone::Error } else { Tone::Neutral },
                            label,
                        ))
                    })
                    .when(!copy_text.is_empty(), |d| {
                        d.child(
                            IconButton::new(("copy-recent", id as u64), "copy", "Copy text")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_text.clone(),
                                    ));
                                    pcx.toast("Copied", cx);
                                }),
                        )
                    }),
            );
        }
        section.child(list)
    }

    fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let pcx = self.pcx.clone();
        let change = Button::new("change-shortcuts", "Change")
            .ghost()
            .small()
            .on_click(move |_, _, cx| pcx.navigate(Page::Settings, Some("shortcuts"), cx));
        let mut rows = vec![(
            "Push-to-talk".to_string(),
            "Hold to dictate, release to paste.".to_string(),
            hub_model::format_hotkey(config.field_str("hotkey")),
        )];
        rows.extend(
            hub_model::shortcut_rows(&config)
                .into_iter()
                .map(|r| (r.name.to_string(), r.detail.to_string(), r.hotkey)),
        );
        let mut grid = div().flex().flex_wrap().w_full();
        for (name, detail, hotkey) in rows {
            grid = grid.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::s4())
                    .w(relative_half())
                    .py(space::s4())
                    .pr(space::s8())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(ui::label(&t, name))
                            .child(
                                ui::supporting(&t, detail)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis(),
                            ),
                    )
                    .child(ui::keys(&t, &hotkey)),
            );
        }
        ui::section(&t, "Shortcuts", None, Some(change.into_any_element())).child(grid)
    }
}

/// Theme tone for a status level.
fn tone(level: Level) -> Tone {
    match level {
        Level::Ok => Tone::Success,
        Level::Warn => Tone::Warning,
        Level::Neutral => Tone::Neutral,
    }
}

fn relative_half() -> gpui::DefiniteLength {
    gpui::relative(0.5)
}

impl Render for HomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let health = self.pcx.health(cx);
        let mut body = div().child(self.render_card(&health, cx));
        if self.setup_open {
            body = body.child(self.render_setup(&health, cx));
        }
        if let Some(err) = self.pcx.services.config_error.borrow().clone() {
            body = body.child(ui::banner(
                &t,
                Tone::Error,
                format!("Your config file couldn't be read, so defaults are shown: {err}"),
                None,
            ));
        }
        let history_off = !self
            .pcx
            .services
            .config()
            .field_bool("history_enabled")
            .unwrap_or(true);
        if history_off {
            let pcx = self.pcx.clone();
            body = body.child(ui::banner(
                &t,
                Tone::Info,
                "History is off. New dictations aren't being saved.",
                Some(
                    Button::new("home-history-off", "Privacy settings")
                        .small()
                        .on_click(move |_, _, cx| pcx.navigate(Page::Settings, Some("privacy"), cx))
                        .into_any_element(),
                ),
            ));
        }
        body = body
            .child(self.render_stats(cx))
            .child(self.render_recent(cx))
            .child(self.render_shortcuts(cx));
        let setup_btn = (!self.setup_open).then(|| {
            Button::new("open-setup", "Setup steps")
                .ghost()
                .icon("list-checks")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.setup_open = true;
                    cx.notify();
                }))
                .into_any_element()
        });
        page(
            header(&t, "Home", None, setup_btn.into_iter().collect()),
            column("home-scroll", body),
        )
    }
}
