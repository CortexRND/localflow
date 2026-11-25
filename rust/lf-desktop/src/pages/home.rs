//! Home: dictation status, setup guidance, stats from the history store,
//! recent dictations and shortcuts.

use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context, FontWeight, Window};
use lf_core::history::{HistoryEntry, Stats};
use lf_ui::hub_model;
use serde_json::json;

use crate::app::{signal_tone, PageCx};
use crate::controllers::runtime_state::OwnEngine;
use crate::controllers::status::{self, Level};
use crate::controllers::{history, setup};
use crate::page::Page;
use crate::pages::{column, header, page};
use crate::remote::Health;
use crate::theme::{size, space, text, theme, Theme, Tone};
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

    /// Start/Stop for this window's engine (`--engine` builds only), with
    /// the reason when it can't start.
    fn engine_controls(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let services = &self.pcx.services;
        let t = theme(cx).clone();
        if !services.caps.engine_allowed() {
            return None;
        }
        let state = self.pcx.engine_state().unwrap_or(OwnEngine::None);
        let config = services.config();
        let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
        let configured = setup::configured(&services.root);
        let view = self.pcx.runtime();
        // Only a provable owner blocks Start; where ownership can't be
        // proven the engine's own lock acquisition decides.
        let other_owner = view.external_owner.is_some()
            && !matches!(
                state,
                OwnEngine::Running | OwnEngine::Starting | OwnEngine::Stopping
            );
        let blocker = if !enabled {
            Some("Turn dictation on first.".to_string())
        } else if !configured {
            Some("Finish setup first.".to_string())
        } else if other_owner {
            Some(match view.external_owner {
                Some(pid) => {
                    format!("Another LocalFlow (pid {pid}) is already handling dictation.")
                }
                None => "Another LocalFlow is already handling dictation.".into(),
            })
        } else {
            None
        };
        let pcx = self.pcx.clone();
        let button = match state {
            OwnEngine::Running | OwnEngine::Stopping => Button::new("engine-stop", "Stop")
                .small()
                .icon("square")
                .busy(state == OwnEngine::Stopping)
                .on_click(move |_, _, cx| pcx.engine_stop(cx)),
            _ => Button::new("engine-start", "Start here")
                .small()
                .icon("play")
                .busy(state == OwnEngine::Starting)
                .disabled(blocker.is_some())
                .tooltip(
                    blocker
                        .clone()
                        .unwrap_or_else(|| "Run dictation in this window".into()),
                )
                .on_click(move |_, _, cx| pcx.engine_start(cx)),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap(space::s2())
                .child(button)
                .when_some(blocker.filter(|_| state != OwnEngine::Running), |d, b| {
                    d.child(ui::supporting(&t, b).max_w(px(220.)))
                })
                .into_any_element(),
        )
    }

    /// The status hero: glyph, lead, support and the Dictation switch, then
    /// the shortcut, microphone and local-service lines as inset rows.
    fn render_card(&self, health: &Health, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
        let key = hub_model::format_hotkey(config.field_str("hotkey"));
        let sandbox = self.pcx.services.is_sandbox();
        let address = self.pcx.services.remote.base_url(&config);
        // The service line is kept apart: it never changes the lead.
        let legacy = status::describe(
            sandbox,
            enabled,
            &key,
            health,
            &address,
            cfg!(target_os = "macos"),
        );
        let runtime = self.pcx.runtime();
        let this = cx.entity().downgrade();
        let toggle =
            Switch::new("dictation-switch", enabled, "Dictation").on_toggle(move |on, _, cx| {
                let _ = this.update(cx, |p, cx| p.set_dictation(on, cx));
            });
        let engine = self.engine_controls(cx);
        let note = (!self.pcx.services.caps.engine_allowed())
            .then(|| ui::supporting(&t, self.pcx.services.caps.engine_note()));
        let service = (
            tone(legacy.service.0),
            format!(
                "Local service (Ask, Meetings, Prompts): {}",
                legacy.service.1
            ),
        );
        let lines = [
            (signal_tone(runtime.shortcut.0), runtime.shortcut.1.clone()),
            (
                signal_tone(runtime.microphone.0),
                runtime.microphone.1.clone(),
            ),
            service,
        ];
        let (lead, support) = (runtime.lead.clone(), runtime.support.clone());
        let hero = div()
            .flex()
            .items_start()
            .gap(space::s5())
            .px(ui::row_inset())
            .py(space::s6())
            .child(ui::glyph_tile("mic", t.icon_accent, t.accent_muted))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s1())
                    .flex_1()
                    .min_w_0()
                    .child(ui::heading(&t, lead))
                    .when(!support.is_empty(), |d| d.child(ui::secondary(&t, support))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
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
                    .children(engine),
            );
        let mut rows: Vec<AnyElement> = vec![hero.into_any_element()];
        for (tone, line) in lines {
            rows.push(status_row(&t, tone, line).into_any_element());
        }
        let save_error = self.save_error.clone();
        if note.is_some() || save_error.is_some() {
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s2())
                    .px(ui::row_inset())
                    .py(space::s4())
                    .children(note)
                    .when_some(save_error, |d, e| d.child(ui::error_line(&t, e)))
                    .into_any_element(),
            );
        }
        ui::group_rows(&t, rows, ui::row_inset())
    }

    fn render_warnings(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let t = theme(cx).clone();
        let warnings = self.pcx.runtime().warnings;
        if warnings.is_empty() {
            return None;
        }
        let mut column = div().flex().flex_col().gap(space::s3());
        for (i, warning) in warnings.into_iter().enumerate() {
            let pcx = self.pcx.clone();
            let action = Button::new(("runtime-warning", i as u64), "Shortcuts")
                .small()
                .on_click(move |_, _, cx| pcx.navigate(Page::Settings, Some("shortcuts"), cx));
            column = column.child(ui::banner(
                &t,
                Tone::Warning,
                warning,
                Some(action.into_any_element()),
            ));
        }
        Some(column)
    }

    fn render_setup(&self, health: &Health, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let count = self.stats.as_ref().map_or(0, |s| s.total_dictations);
        let listening = self.pcx.runtime().listening;
        let steps = setup::steps(&services.root, &services.config(), health, count, listening);
        let total = steps.len();
        let done = steps
            .iter()
            .filter(|s| s.state == setup::State::Done)
            .count();
        let pcx = self.pcx.clone();
        let run = Button::new(
            "setup-run",
            if self.pcx.setup_draft.get() {
                "Resume setup"
            } else {
                "Run setup"
            },
        )
        .small()
        .primary()
        .on_click(move |_, _, cx| pcx.open_setup(cx));
        let collapse = div()
            .flex()
            .items_center()
            .gap(space::s3())
            .child(run)
            .child(
                Button::new("setup-hide", "Hide")
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.setup_open = false;
                        cx.notify();
                    })),
            );
        let mut rows = Vec::new();
        for (i, step) in steps.into_iter().enumerate() {
            let (icon, color) = match step.state {
                setup::State::Done => ("circle-check", t.dot(Tone::Success)),
                setup::State::Todo => ("circle-alert", t.dot(Tone::Warning)),
                setup::State::Unavailable => ("info", t.icon_secondary),
            };
            let pcx = self.pcx.clone();
            let action = step.action.map(|(label, page, target)| {
                Button::new(("setup-action", i as u64), label)
                    .small()
                    .on_click(move |_, _, cx| pcx.navigate(page, target, cx))
            });
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s5())
                    .min_h(size::row())
                    .px(ui::row_inset())
                    .py(space::s4())
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
                    .children(action)
                    .into_any_element(),
            );
        }
        // Separators start at the step titles (after the 16 px glyph).
        let inset = ui::row_inset() + size::icon() + space::s5();
        ui::section(
            &t,
            "Get set up",
            Some(format!("{done} of {total} ready. Run setup to pick and download models, test the microphone and choose a shortcut.").into()),
            Some(collapse.into_any_element()),
        )
        .child(ui::group_rows(&t, rows, inset))
    }

    fn render_stats(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let Some(stats) = &self.stats else {
            if !self.loading {
                return div();
            }
            return ui::section(&t, "Activity", None, None)
                .child(ui::secondary(&t, "Loading stats…"));
        };
        let cards = hub_model::home_stat_cards(stats);
        let (vs, vl) = text::stat();
        ui::section(&t, "Activity", None, None)
            .child(
                ui::group(&t).child(div().flex().w_full().py(space::s4()).children(
                    cards.into_iter().enumerate().map(|(i, (name, value))| {
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(space::s1())
                            .px(ui::row_inset())
                            .when(i > 0, |d| d.border_l_1().border_color(t.separator))
                            .child(
                                div()
                                    .text_size(vs)
                                    .line_height(vl)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.text)
                                    .child(value),
                            )
                            .child(ui::supporting(&t, name))
                    }),
                )),
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
            return section.child(
                ui::group(&t).child(ui::empty_state(
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
                )),
            );
        }
        let mut rows = Vec::new();
        for entry in self.recent.iter() {
            let id = entry.id;
            let text_value = history::row_preview(entry);
            let chip = hub_model::history_status_chip(&entry.status);
            let copy_text = entry.final_text.clone();
            let open = self.pcx.clone();
            let pcx = self.pcx.clone();
            rows.push(
                ui::row_button(&t, ("recent", id as u64), false)
                    .items_center()
                    .gap(space::s5())
                    .min_h(size::row())
                    .pl(ui::row_inset())
                    .pr(space::s4())
                    .rounded(size::radius_inner())
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
                            .line_clamp(1)
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
                    })
                    .into_any_element(),
            );
        }
        section.child(ui::group_rows(&t, rows, ui::row_inset()))
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
        let rows = rows.into_iter().map(|(name, detail, hotkey)| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(space::s6())
                .min_h(size::row())
                .px(ui::row_inset())
                .py(space::s3())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(ui::label(&t, name))
                        .child(
                            ui::supporting(&t, detail)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis(),
                        ),
                )
                .child(ui::keys(&t, &hotkey))
                .into_any_element()
        });
        let rows: Vec<AnyElement> = rows.collect();
        ui::section(&t, "Shortcuts", None, Some(change.into_any_element())).child(ui::group_rows(
            &t,
            rows,
            ui::row_inset(),
        ))
    }
}

/// One inset status row under the hero (shortcut, microphone, service).
///
/// A block row, not a flex one: the text sits in normal flow at the card's
/// width, so its height is measured at the width it wraps to and the row
/// grows with it (long copy such as Wayland's stays inside the card instead
/// of being clipped by the group). In a flex row the text was measured
/// unwrapped and the row kept a one-line height. The dot is positioned on the
/// first line, in room reserved by the left padding. The vertical padding
/// makes one line exactly 36 px.
pub(crate) fn status_row(t: &Theme, tone: Tone, line: String) -> gpui::Div {
    let (sz, lh) = text::body();
    let pad = (px(36.) - lh) * 0.5;
    let mark = div()
        .absolute()
        .left(ui::row_inset())
        .top(pad)
        .h(lh)
        .flex()
        .items_center()
        .child(ui::dot(t, tone));
    let words = div().child(line);
    #[cfg(test)]
    let (mark, words) = (
        mark.debug_selector(|| "status-dot".into()),
        words.debug_selector(|| "status-text".into()),
    );
    div()
        .block()
        .relative()
        .w_full()
        .min_w_0()
        .pl(ui::row_inset() + ui::dot_size() + space::s3())
        .pr(ui::row_inset())
        .py(pad)
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text)
        .child(mark)
        .child(words)
}

/// Theme tone for a status level.
fn tone(level: Level) -> Tone {
    match level {
        Level::Ok => Tone::Success,
        Level::Warn => Tone::Warning,
        Level::Neutral => Tone::Neutral,
    }
}

impl Render for HomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let health = self.pcx.health(cx);
        let mut body = div().child(self.render_card(&health, cx));
        if let Some(warnings) = self.render_warnings(cx) {
            body = body.child(warnings);
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
        if self.setup_open {
            body = body.child(self.render_setup(&health, cx));
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
