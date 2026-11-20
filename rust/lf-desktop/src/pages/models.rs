//! Models & keys: speech provider/model, cleanup model, and provider keys.

use std::rc::Rc;

use gpui::{div, prelude::*, Context, Entity, SharedString, Window};
use lf_ui::models_layout::{self, ListState};

use crate::app::{Confirm, PageCx};
use crate::controllers::models as ctl;
use crate::pages::{column, header, page};
use crate::theme::{space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Speech,
    Cleanup,
    Providers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Editing {
    Key(&'static str),
    Use(&'static str),
}

pub struct ModelsPage {
    pcx: PageCx,
    tab: Tab,
    speech_provider: Entity<Select>,
    speech_model: Entity<Select>,
    provider_ids: Vec<String>,
    model_ids: Vec<String>,
    speech_message: Option<(Tone, String)>,
    cleanup_select: Entity<Select>,
    cleanup_items: Vec<String>,
    ollama: (Vec<String>, ListState),
    cleanup_input: Entity<TextInput>,
    cleanup_message: Option<(Tone, String)>,
    editing: Option<Editing>,
    key_input: Entity<TextInput>,
    model_input: Entity<TextInput>,
    url_input: Entity<TextInput>,
    provider_message: Option<(Tone, String)>,
}

impl ModelsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let speech_provider = cx.new(|cx| Select::new(cx, Vec::new(), None).width(240.));
        let speech_model = cx.new(|cx| Select::new(cx, Vec::new(), None).width(240.));
        let cleanup_select = cx.new(|cx| Select::new(cx, Vec::new(), None).width(280.));
        cx.subscribe(&speech_provider, |this, _, e, cx| {
            let SelectEvent::Changed(i) = e;
            if let Some(id) = this.provider_ids.get(*i).cloned() {
                this.save_speech(&id, None, cx);
            }
        })
        .detach();
        cx.subscribe(&speech_model, |this, _, e, cx| {
            let SelectEvent::Changed(i) = e;
            let provider = models_layout::current_speech_provider(&this.pcx.services.config());
            if let Some(model) = this.model_ids.get(*i).cloned() {
                this.save_speech(&provider, Some(&model), cx);
            }
        })
        .detach();
        cx.subscribe(&cleanup_select, |this, _, e, cx| {
            let SelectEvent::Changed(i) = e;
            if let Some(model) = this.cleanup_items.get(*i).cloned() {
                this.save_cleanup(&model, cx);
            }
        })
        .detach();
        let cleanup_input =
            cx.new(|cx| TextInput::new(cx, "Model ID, e.g. llama3.2:3b").label("Cleanup model"));
        cx.subscribe(&cleanup_input, |this, input, e, cx| {
            if *e == InputEvent::Submit {
                let model = input.read(cx).text().to_string();
                this.save_cleanup(&model, cx);
            }
        })
        .detach();
        let key_input = cx.new(|cx| {
            TextInput::new(cx, "Paste the API key")
                .masked()
                .label("API key")
        });
        cx.subscribe(&key_input, |this, _, e, cx| {
            if *e == InputEvent::Submit {
                this.save_key(cx);
            }
        })
        .detach();
        let model_input = cx.new(|cx| TextInput::new(cx, "Model ID").label("Model"));
        let url_input = cx.new(|cx| TextInput::new(cx, "https://host/v1").label("Base URL"));
        let mut page = ModelsPage {
            pcx,
            tab: Tab::Speech,
            speech_provider,
            speech_model,
            provider_ids: Vec::new(),
            model_ids: Vec::new(),
            speech_message: None,
            cleanup_select,
            cleanup_items: Vec::new(),
            ollama: (Vec::new(), ListState::Loading),
            cleanup_input,
            cleanup_message: None,
            editing: None,
            key_input,
            model_input,
            url_input,
            provider_message: None,
        };
        page.sync(cx);
        page.load_ollama(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.sync(cx);
        cx.notify();
    }

    /// Drop any half-entered key (tab switch, leaving the page).
    pub fn clear_secret(&mut self, cx: &mut Context<Self>) {
        self.key_input.update(cx, |i, cx| i.clear(cx));
        if matches!(self.editing, Some(Editing::Key(_))) {
            self.editing = None;
        }
        cx.notify();
    }

    pub fn open_tab(&mut self, target: &str, cx: &mut Context<Self>) {
        self.tab = match target {
            "cleanup" => Tab::Cleanup,
            "providers" | "keys" => Tab::Providers,
            _ => Tab::Speech,
        };
        cx.notify();
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let config = self.pcx.services.config();
        let choices = models_layout::speech_provider_choices(&config, &ctl::speech_available);
        let current = models_layout::current_speech_provider(&config);
        self.provider_ids = choices.iter().map(|(id, _)| id.clone()).collect();
        let labels: Vec<SharedString> = choices.iter().map(|(_, l)| l.clone().into()).collect();
        let sel = self.provider_ids.iter().position(|id| *id == current);
        self.speech_provider
            .update(cx, |s, cx| s.set_options(labels, sel, cx));
        let models = models_layout::speech_model_choices(&current);
        self.model_ids = models.iter().map(|(id, _)| id.to_string()).collect();
        let size = config.field_str("model_size").unwrap_or("").to_string();
        let sel = self.model_ids.iter().position(|m| *m == size);
        let labels: Vec<SharedString> =
            models.iter().map(|(_, l)| SharedString::from(*l)).collect();
        let empty = labels.is_empty();
        self.speech_model.update(cx, |s, cx| {
            s.set_options(labels, sel, cx);
            s.set_disabled(empty, cx);
        });
        let (items, selected, _) =
            models_layout::cleanup_picker(&config, self.ollama.1, &self.ollama.0);
        let idx = items.iter().position(|m| *m == selected);
        self.cleanup_items = items.clone();
        self.cleanup_select.update(cx, |s, cx| {
            s.set_options(items.into_iter().map(SharedString::from).collect(), idx, cx)
        });
    }

    fn load_ollama(&mut self, cx: &mut Context<Self>) {
        self.ollama.1 = ListState::Loading;
        if self.pcx.services.is_sandbox() {
            self.ollama = (
                vec!["llama3.2:3b".into(), "qwen3:4b".into(), "gemma4:e2b".into()],
                ListState::Ready,
            );
            self.sync(cx);
            return;
        }
        let base = self
            .pcx
            .services
            .config()
            .field_str("llm_base_url")
            .unwrap_or("http://localhost:11434")
            .to_string();
        cx.spawn(async move |this, cx| {
            let listing = cx
                .background_executor()
                .spawn(async move { models_layout::ollama_models(&base, None) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ollama = listing;
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_speech(&mut self, provider: &str, model: Option<&str>, cx: &mut Context<Self>) {
        self.speech_message = Some(
            match ctl::set_speech(&self.pcx.services.config_file, provider, model) {
                Ok(saved) => {
                    self.pcx.services.set_cached_config(saved.clone());
                    (
                    Tone::Success,
                    format!(
                        "Saved: {}. LocalFlow loads it next time it starts dictating; download it with `lf setup` if needed.",
                        models_layout::speech_summary(&saved)
                    ),
                )
                }
                Err(err) => (Tone::Error, ctl::error_text(&err)),
            },
        );
        self.pcx.config_changed(cx);
        self.sync(cx);
        cx.notify();
    }

    fn save_cleanup(&mut self, model: &str, cx: &mut Context<Self>) {
        self.cleanup_message = Some(
            match ctl::set_cleanup_model(&self.pcx.services.config_file, model) {
                Ok(saved) => {
                    self.pcx.services.set_cached_config(saved);
                    self.cleanup_input.update(cx, |i, cx| i.clear(cx));
                    (Tone::Success, format!("Cleanup now uses {}.", model.trim()))
                }
                Err(err) => (Tone::Error, ctl::error_text(&err)),
            },
        );
        self.pcx.config_changed(cx);
        self.sync(cx);
        cx.notify();
    }

    fn start_edit(&mut self, editing: Editing, window: &mut Window, cx: &mut Context<Self>) {
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.provider_message = None;
        if let Editing::Use(id) = editing {
            let config = self.pcx.services.config();
            let preset = lf_providers::keys::get_preset(id).ok();
            let active = preset.is_some_and(|p| models_layout::is_active(p, &config, None));
            let model = if active {
                config.field_str("llm_model").unwrap_or("").to_string()
            } else {
                preset
                    .map(|p| p.default_model.to_string())
                    .unwrap_or_default()
            };
            self.model_input.update(cx, |i, cx| i.set_text(model, cx));
            let url = if id == "custom" {
                config.field_str("llm_base_url").unwrap_or("").to_string()
            } else {
                String::new()
            };
            self.url_input.update(cx, |i, cx| i.set_text(url, cx));
            self.model_input.read(cx).focus(window);
        } else {
            self.key_input.read(cx).focus(window);
        }
        self.editing = Some(editing);
        cx.notify();
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some(Editing::Key(id)) = self.editing else {
            return;
        };
        // Read and immediately clear: the value is never kept in page state.
        let key = self.key_input.read(cx).text().to_string();
        self.key_input.update(cx, |i, cx| i.clear(cx));
        let services = &self.pcx.services;
        self.provider_message = Some(
            match ctl::save_key_and_apply(
                services.secrets.as_ref(),
                &services.config_file,
                id,
                &key,
            ) {
                Ok(ctl::KeySaved::Activated(saved)) => {
                    self.editing = None;
                    self.pcx.services.set_cached_config(saved.clone());
                    self.pcx.config_changed(cx);
                    (
                        Tone::Success,
                        format!(
                            "New key saved and in use: cleanup and Ask now use {} with it.",
                            lf_ui::models_layout::summary_values(&saved).1
                        ),
                    )
                }
                Ok(ctl::KeySaved::NotActivated(reason)) => {
                    self.editing = None;
                    (
                        Tone::Warning,
                        format!(
                            "New key saved, but cleanup and Ask still use the previous key: {reason} Use “Use new key” to try again."
                        ),
                    )
                }
                Ok(ctl::KeySaved::Saved) => {
                    self.editing = None;
                    (Tone::Success, "Key saved to your system keyring. Use the provider to switch cleanup to it.".into())
                }
                Err(err) => (Tone::Error, ctl::error_text(&err)),
            },
        );
        cx.notify();
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        let Some(Editing::Use(id)) = self.editing else {
            return;
        };
        let model = self.model_input.read(cx).text().trim().to_string();
        let url = self.url_input.read(cx).text().trim().to_string();
        let services = &self.pcx.services;
        let result = ctl::activate(
            services.secrets.as_ref(),
            &services.config_file,
            id,
            (id == "custom").then_some(url.as_str()),
            Some(model.as_str()).filter(|m| !m.is_empty()),
        );
        self.provider_message = Some(match result {
            Ok(saved) => {
                self.pcx.services.set_cached_config(saved.clone());
                self.editing = None;
                (
                    Tone::Success,
                    format!(
                        "Cleanup and Ask now use {}.",
                        models_layout::summary_values(&saved).1
                    ),
                )
            }
            Err(err) => (Tone::Error, ctl::error_text(&err)),
        });
        self.pcx.config_changed(cx);
        self.sync(cx);
        cx.notify();
    }

    fn ask_remove(&mut self, id: &'static str, label: &'static str, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: format!("Remove the {label} key?").into(),
                body: "The key is deleted from your system keyring. If cleanup uses this provider, it stops working until you add a key or switch providers.".into(),
                verb: "Remove key".into(),
                destructive: true,
                on_confirm: Rc::new(move |_, cx| {
                    let _ = this.update(cx, |p, cx| {
                        let s = &p.pcx.services;
                        p.provider_message = Some(match ctl::remove_key(s.secrets.as_ref(), &s.config_file, id) {
                            Ok(()) => (Tone::Success, "Key removed.".into()),
                            Err(e) => (Tone::Error, ctl::error_text(&e)),
                        });
                        p.pcx.config_changed(cx);
                        cx.notify();
                    });
                }),
            },
            cx,
        );
    }

    fn render_speech(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let current = models_layout::current_speech_provider(&config);
        let mut body = div().flex().flex_col().gap(space::s8()).child(
            ui::section(
                &t,
                "Speech model",
                Some(format!("In use: {}", models_layout::speech_summary(&config)).into()),
                None,
            )
            .child(ui::setting_row(
                &t,
                "Engine",
                "Automatic picks the best engine for this computer when LocalFlow starts.",
                self.speech_provider.clone().into_any_element(),
                ctl::speech_unavailable_reason(&current).map(|r| (Tone::Warning, r.into())),
            ))
            .child(ui::setting_row(
                &t,
                "Model size",
                if self.model_ids.is_empty() {
                    "This engine runs one fixed model."
                } else {
                    "Larger models are more accurate and slower."
                },
                self.speech_model.clone().into_any_element(),
                None,
            )),
        );
        if !ctl::speech_available("mlx-whisper") {
            body = body.child(ui::supporting(
                &t,
                "MLX Whisper isn't offered here: it needs a Mac with Apple silicon.",
            ));
        }
        body = body.child(ui::banner(
            &t,
            Tone::Info,
            "Downloading and deleting speech models still happens in `lf setup` (or the existing Hub). This page changes which model LocalFlow uses.",
            None,
        ));
        if let Some((tone, msg)) = self.speech_message.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        body
    }

    fn render_cleanup(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let config = self.pcx.services.config();
        let (_, current) = models_layout::summary_values(&config);
        let is_ollama = config.field_str("llm_provider") == Some("ollama");
        let mut sec = ui::section(
            &t,
            "Cleanup & Ask model",
            Some(format!("In use: {current}").into()),
            None,
        );
        if is_ollama {
            let (_, _, note) =
                models_layout::cleanup_picker(&config, self.ollama.1, &self.ollama.0);
            let base = config.field_str("llm_base_url").unwrap_or("").to_string();
            let state_text = match self.ollama.1 {
                ListState::Loading => "Asking Ollama for installed models…".to_string(),
                ListState::Ready => format!(
                    "{} models installed in Ollama at {base}",
                    self.ollama.0.len()
                ),
                ListState::Empty => format!("Ollama at {base} has no models installed."),
                ListState::Unavailable => format!("Ollama isn't answering at {base}."),
            };
            sec = sec.child(ui::setting_row(
                &t,
                "Installed model",
                state_text,
                div()
                    .flex()
                    .gap(space::s4())
                    .child(self.cleanup_select.clone())
                    .child(
                        Button::new("ollama-refresh", "Refresh")
                            .small()
                            .icon("refresh-cw")
                            .busy(self.ollama.1 == ListState::Loading)
                            .on_click(cx.listener(|this, _, _, cx| this.load_ollama(cx))),
                    )
                    .into_any_element(),
                (!note.is_empty()).then(|| (Tone::Warning, note.into())),
            ));
        }
        sec = sec.child(ui::setting_row(
            &t,
            "Model ID",
            if is_ollama {
                "Or type any model ID Ollama has, then press Enter."
            } else {
                "The provider's model ID, for example gpt-5-mini."
            },
            div()
                .flex()
                .gap(space::s4())
                .child(div().w(gpui::px(240.)).child(self.cleanup_input.clone()))
                .child(
                    Button::new("cleanup-use", "Use")
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let model = this.cleanup_input.read(cx).text().to_string();
                            this.save_cleanup(&model, cx);
                        })),
                )
                .into_any_element(),
            None,
        ));
        let mut body = div().flex().flex_col().gap(space::s8()).child(sec);
        body = body.child(ui::supporting(&t, ctl::data_leaves_text(&config)));
        if let Some((tone, msg)) = self.cleanup_message.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        body
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let config = services.config();
        // The sandbox never consults the real environment.
        let sandbox = services.is_sandbox();
        let env_set = move |name: &str| !sandbox && std::env::var_os(name).is_some();
        let rows = ctl::provider_rows(
            services.secrets.as_ref(),
            &services.config_file,
            &config,
            &env_set,
        );
        let mut list = ui::list(&t);
        for (i, row) in rows.into_iter().enumerate() {
            let id = row.id;
            let label = row.label;
            let tone = match row.status {
                "Active" => Tone::Accent,
                "Key saved" => Tone::Success,
                "Local" => Tone::Info,
                _ => Tone::Neutral,
            };
            let mut actions = div().flex().items_center().gap(space::s3());
            if !row.active || id == "custom" || row.needs_reactivation {
                actions = actions.child(
                    Button::new(
                        ("use", i as u64),
                        if row.needs_reactivation {
                            "Use new key"
                        } else {
                            "Use"
                        },
                    )
                    .small()
                    .disabled(row.needs_key && !row.has_key)
                    .tooltip(if row.needs_key && !row.has_key {
                        "Add a key first"
                    } else {
                        "Use this provider for cleanup and Ask"
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_edit(Editing::Use(id), window, cx)
                    })),
                );
            }
            if row.needs_key && row.env_override.is_none() {
                actions = actions.child(
                    Button::new(
                        ("key", i as u64),
                        if row.has_key {
                            "Replace key"
                        } else {
                            "Add key"
                        },
                    )
                    .small()
                    .icon("key-round")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_edit(Editing::Key(id), window, cx)
                    })),
                );
                if row.has_key {
                    actions = actions.child(
                        Button::new(("remove-key", i as u64), "Remove")
                            .small()
                            .ghost()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.ask_remove(id, label, cx)),
                            ),
                    );
                }
            }
            let editing = self
                .editing
                .filter(|e| matches!(e, Editing::Key(x) | Editing::Use(x) if *x == id));
            let mut item = div()
                .flex()
                .flex_col()
                .gap(space::s4())
                .px(space::s4())
                .py(space::s5())
                .when(i > 0, |d| d.border_t_1().border_color(t.border))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s5())
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(space::s4())
                                        .child(ui::label(&t, label))
                                        .child(ui::token(&t, tone, row.status)),
                                )
                                .when(row.needs_reactivation, |d| {
                                    d.child(ui::supporting(
                                        &t,
                                        "A newer key is saved but cleanup and Ask still use the previous one. Choose “Use new key” to switch.",
                                    ))
                                })
                                .when_some(row.env_override.clone(), |d, var| {
                                    d.child(ui::supporting(
                                        &t,
                                        format!("Key comes from the {var} environment variable, which overrides the keyring. Change or unset it there."),
                                    ))
                                })
                                .child(ui::supporting(
                                    &t,
                                    if row.remote {
                                        format!(
                                            "{} · Text is sent to {label} for cleanup and Ask.",
                                            if row.base_url.is_empty() {
                                                "Your endpoint"
                                            } else {
                                                row.base_url
                                            }
                                        )
                                    } else {
                                        format!("{} · Runs on this device.", row.base_url)
                                    },
                                )),
                        )
                        .child(actions),
                );
            match editing {
                Some(Editing::Key(_)) => {
                    item = item.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(div().flex_1().child(self.key_input.clone()))
                            .child(Button::new("save-key", "Save key").primary().small().on_click(cx.listener(|this, _, _, cx| this.save_key(cx))))
                            .child(Button::new("cancel-key", "Cancel").ghost().small().on_click(cx.listener(|this, _, _, cx| {
                                this.key_input.update(cx, |i, cx| i.clear(cx));
                                this.editing = None;
                                cx.notify();
                            }))),
                    )
                    .child(ui::supporting(&t, format!(
                        "Stored in your system keyring under “localflow”. It's never shown again. Without a keyring, set {} instead.",
                        lf_core::secrets::env_name(&format!("provider_key_{id}"))
                    )));
                }
                Some(Editing::Use(_)) => {
                    item = item.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .when(id == "custom", |d| {
                                d.child(div().flex_1().child(self.url_input.clone()))
                            })
                            .child(div().flex_1().child(self.model_input.clone()))
                            .child(
                                Button::new("activate", "Use provider")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| this.activate(cx))),
                            )
                            .child(
                                Button::new("cancel-use", "Cancel")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editing = None;
                                        cx.notify();
                                    })),
                            ),
                    );
                }
                _ => {}
            }
            list = list.child(item);
        }
        let mut body = div()
            .flex()
            .flex_col()
            .gap(space::s6())
            .child(ui::section(&t, "Providers", Some("Where cleanup and Ask run. Ollama stays on this device; the others are remote services.".into()), None).child(list));
        if let Some((tone, msg)) = self.provider_message.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        if services.is_sandbox() {
            body = body.child(ui::supporting(&t, "Sandbox: keys are kept in memory for this window only and are never written to your keyring."));
        }
        body
    }
}

impl Render for ModelsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let this = cx.entity().downgrade();
        let tabs = ui::tabs(
            &t,
            "models-tab",
            &[
                (Tab::Speech, "Speech"),
                (Tab::Cleanup, "Cleanup"),
                (Tab::Providers, "Providers & keys"),
            ],
            self.tab,
            Rc::new(move |tab, _, cx| {
                let _ = this.update(cx, |p, cx| {
                    p.tab = tab;
                    p.clear_secret(cx);
                });
            }),
        );
        let content = match self.tab {
            Tab::Speech => self.render_speech(cx),
            Tab::Cleanup => self.render_cleanup(cx),
            Tab::Providers => self.render_providers(cx),
        };
        page(
            header(
                &t,
                "Models & keys",
                Some("Speech model, cleanup model and provider keys.".into()),
                vec![],
            ),
            column("models-scroll", div().child(tabs).child(content)),
        )
    }
}
