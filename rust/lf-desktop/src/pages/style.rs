//! Style: cleanup level and tone per app category (`[cleanup]` keys), with
//! autosave through `hub_model::style_update`.

use gpui::{div, prelude::*, Context, Window};
use lf_ui::hub_model::{
    self, STYLE_CATEGORIES, STYLE_CLEANUP_OPTIONS, STYLE_CLEANUP_TITLES, STYLE_OPTIONS,
};
use lf_ui::models_layout;
use serde_json::{json, Value};

use crate::app::PageCx;
use crate::controllers::models::data_leaves_text;
use crate::page::Page;
use crate::pages::{column, header, page};
use crate::theme::{space, theme, Tone};
use crate::ui::{self, Button, Segmented, Switch};

pub struct StylePage {
    pcx: PageCx,
    error: Option<String>,
    last: Option<Value>,
}

const LEVEL_DETAIL: [&str; 3] = [
    "No cleanup: you get exactly what the speech model heard.",
    "Light: fixes punctuation, capitalisation and filler words; keeps your wording.",
    "Medium: also smooths phrasing and grammar while keeping your meaning.",
];

impl StylePage {
    pub fn new(pcx: PageCx, _: &mut Context<Self>) -> Self {
        StylePage {
            pcx,
            error: None,
            last: None,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn save(&mut self, update: Value, cx: &mut Context<Self>) {
        match self.pcx.services.apply(&update) {
            Ok(_) => {
                self.error = None;
                self.last = None;
            }
            Err(err) => {
                self.error = Some(hub_model::style_save_error_text(&err));
                self.last = Some(update);
            }
        }
        self.pcx.config_changed(cx);
        cx.notify();
    }

    fn save_styles(
        &mut self,
        level: Option<&str>,
        change: Option<(&str, &str)>,
        cx: &mut Context<Self>,
    ) {
        let config = self.pcx.services.config();
        let level = level.map(str::to_string).unwrap_or_else(|| {
            config
                .field_str("cleanup_level")
                .unwrap_or("light")
                .to_string()
        });
        let current: Vec<(String, String)> = STYLE_CATEGORIES
            .iter()
            .map(|(cat, _)| {
                let value = match change {
                    Some((c, v)) if c == *cat => v.to_string(),
                    _ => config
                        .field_str(&format!("style_{cat}"))
                        .unwrap_or("formal")
                        .to_string(),
                };
                (cat.to_string(), value)
            })
            .collect();
        let pairs: Vec<(&str, &str)> = current
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        self.save(hub_model::style_update(&level, &pairs), cx);
    }
}

impl Render for StylePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let cleanup_on = config.field_bool("cleanup_enabled").unwrap_or(true);
        let styles_on = config.field_bool("styles_enabled").unwrap_or(true);
        let level = config
            .field_str("cleanup_level")
            .unwrap_or("light")
            .to_string();
        let level_index = STYLE_CLEANUP_OPTIONS.iter().position(|l| *l == level);
        let (_, cleanup_model) = models_layout::summary_values(&config);

        let mut body = div();
        if let Some(err) = self.error.clone() {
            let retry = self.last.clone();
            body = body.child(ui::banner(
                &t,
                Tone::Error,
                err,
                retry.map(|u| {
                    Button::new("style-retry", "Retry")
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.save(u.clone(), cx)))
                        .into_any_element()
                }),
            ));
        }
        if !cleanup_on {
            let this = cx.entity().downgrade();
            body = body.child(ui::banner(
                &t,
                Tone::Info,
                "Cleanup is off: transcripts are typed exactly as heard, and styles don't apply.",
                Some(
                    Switch::new("cleanup-on", false, "Cleanup")
                        .on_toggle(move |on, _, cx| {
                            let _ = this.update(cx, |p, cx| {
                                p.save(json!({"llm": {"cleanup_enabled": on}}), cx)
                            });
                        })
                        .into_any_element(),
                ),
            ));
        }
        let this = cx.entity().downgrade();
        body = body.child(
            ui::section(
                &t,
                "Cleanup",
                Some(hub_model::STYLE_AUTOSAVE_HINT.into()),
                None,
            )
            .child(
                Segmented::new("cleanup-level", STYLE_CLEANUP_TITLES, level_index)
                    .disabled(!cleanup_on)
                    .on_select(move |i, _, cx| {
                        let _ = this.update(cx, |p, cx| {
                            p.save_styles(Some(STYLE_CLEANUP_OPTIONS[i]), None, cx)
                        });
                    }),
            )
            .child(ui::secondary(
                &t,
                level_index.map_or("", |i| LEVEL_DETAIL[i]),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(ui::status_dot(
                        &t,
                        if cleanup_on {
                            Tone::Success
                        } else {
                            Tone::Neutral
                        },
                        format!("Cleanup model: {cleanup_model}"),
                    ))
                    .child({
                        let pcx = self.pcx.clone();
                        Button::new("style-models", "Change model")
                            .ghost()
                            .small()
                            .on_click(move |_, _, cx| {
                                pcx.navigate(Page::Models, Some("cleanup"), cx)
                            })
                    }),
            )
            .child(ui::supporting(&t, data_leaves_text(&config))),
        );

        let this = cx.entity().downgrade();
        let mut tones = ui::section(
            &t,
            "Tone by app",
            Some("How cleaned-up text should sound in each kind of app.".into()),
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(ui::label(&t, "Use writing styles"))
                    .child(
                        Switch::new("styles-on", styles_on, "Use writing styles")
                            .disabled(!cleanup_on)
                            .on_toggle(move |on, _, cx| {
                                let _ = this.update(cx, |p, cx| {
                                    p.save(json!({"cleanup": {"styles_enabled": on}}), cx)
                                });
                            }),
                    )
                    .into_any_element(),
            ),
        );
        if !cfg!(target_os = "macos") {
            tones = tones.child(ui::banner(
                &t,
                Tone::Info,
                "On Linux LocalFlow can't tell which app is in front yet, so every dictation uses the “Other” style.",
                None,
            ));
        }
        let titles: Vec<String> = STYLE_OPTIONS
            .iter()
            .map(|o| hub_model::style_option_title(o))
            .collect();
        for (category, example) in STYLE_CATEGORIES {
            let value = config
                .field_str(&format!("style_{category}"))
                .unwrap_or("formal")
                .to_string();
            let index = STYLE_OPTIONS.iter().position(|o| *o == value);
            let this = cx.entity().downgrade();
            let describe = lf_providers::cleanup::style_line(&value).unwrap_or("");
            let enabled = cleanup_on && styles_on;
            tones = tones.child(ui::setting_row(
                &t,
                hub_model::style_category_label(category),
                format!("For example: “{example}” {describe}"),
                Segmented::new(
                    (
                        "tone",
                        category.len() as u64 * 7 + category.as_bytes()[0] as u64,
                    ),
                    titles.clone(),
                    index,
                )
                .disabled(!enabled)
                .on_select(move |i, _, cx| {
                    let _ = this.update(cx, |p, cx| {
                        p.save_styles(None, Some((category, STYLE_OPTIONS[i])), cx)
                    });
                })
                .into_any_element(),
                None,
            ));
        }
        body = body.child(tones);
        page(
            header(
                &t,
                "Style",
                Some("Cleanup level and tone. Saved to your LocalFlow config.".into()),
                vec![],
            ),
            column("style-scroll", body),
        )
    }
}
