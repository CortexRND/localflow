//! Models & keys: speech provider/model, cleanup model, and provider keys.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, Context, Entity, PathPromptOptions, SharedString, Window};
use lf_ui::models_layout::{self, ListState};
use lf_ui::setup_flow::SttModel;

use crate::app::{Confirm, PageCx};
use crate::controllers::jobs;
use crate::controllers::key_import::{self, KeyImport, Source, TestOutcome};
use crate::controllers::model_storage::{self as storage, Inventory, RowState};
use crate::controllers::models as ctl;
use crate::controllers::provider_ops::{self, EditorSlot, ImportDone, ImportSession, OpGate};
use crate::controllers::setup_flow::Loadable;
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
    /// The open key/Use editor instance (generation-bound, holds no drafts).
    editing: EditorSlot<Editing>,
    key_input: Entity<TextInput>,
    model_input: Entity<TextInput>,
    url_input: Entity<TextInput>,
    provider_message: Option<(Tone, String)>,
    catalog: Vec<SttModel>,
    recommended: String,
    installed: Loadable<HashMap<String, bool>>,
    inventory: Loadable<Inventory>,
    storage_message: Option<(Tone, String)>,
    polling: bool,
    import: ImportSession,
    /// One provider mutation (save key, use, remove, import) at a time.
    ops: OpGate,
    path_input: Entity<TextInput>,
    tests: HashMap<&'static str, TestState>,
    test_generation: u64,
    /// Key presence per provider, loaded off the UI thread (render never
    /// reads the keyring).
    providers: Loadable<ctl::ProviderSnapshot>,
    providers_generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestState {
    Busy(u64),
    Done(TestOutcome),
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
        let path_input =
            cx.new(|cx| TextInput::new(cx, "Path to a .env file, e.g. ~/.env").label(".env file"));
        cx.subscribe(&path_input, |this, input, e, cx| {
            if *e == InputEvent::Submit {
                let text = input.read(cx).text().to_string();
                match key_import::typed_path(&text) {
                    Ok(path) => this.detect_keys(Source::File(path), cx),
                    Err(error) => {
                        if let Some(card) = this.import.card.as_mut() {
                            card.error = Some(error);
                        }
                        cx.notify();
                    }
                }
            }
        })
        .detach();
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
            editing: EditorSlot::default(),
            key_input,
            model_input,
            url_input,
            provider_message: None,
            catalog: Vec::new(),
            recommended: String::new(),
            installed: Loadable::Idle,
            inventory: Loadable::Idle,
            storage_message: None,
            polling: false,
            import: ImportSession::default(),
            ops: OpGate::default(),
            path_input,
            tests: HashMap::new(),
            test_generation: 0,
            providers: Loadable::Idle,
            providers_generation: 0,
        };
        page.sync(cx);
        page.load_ollama(cx);
        page.load_catalog(cx);
        page.load_providers(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.invalidate_tests();
        self.load_providers(cx);
        self.sync(cx);
        cx.notify();
    }

    /// Drop any half-entered key (tab switch, leaving the page).
    pub fn clear_secret(&mut self, cx: &mut Context<Self>) {
        // Detected key candidates and test results never outlive the tab.
        self.import.close();
        self.tests.clear();
        self.test_generation += 1;
        self.key_input.update(cx, |i, cx| i.clear(cx));
        if matches!(self.editing.current(), Some(Editing::Key(_))) {
            self.editing.close();
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
            // The shared simulated backend (cleanup pulls made in setup show
            // up here); read on a worker like the real list.
            let backend = self.pcx.services.setup.models.clone();
            cx.spawn(async move |this, cx| {
                let listing = cx
                    .background_executor()
                    .spawn(async move { provider_ops::ollama_listing(backend.as_ref(), "") })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.ollama = listing;
                    this.sync(cx);
                    cx.notify();
                });
            })
            .detach();
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
        self.editing.open(editing);
        cx.notify();
    }

    /// Results of Test connection are about a key/endpoint/model that may
    /// just have changed: drop them (and any in flight).
    fn invalidate_tests(&mut self) {
        self.tests.clear();
        self.test_generation += 1;
    }

    fn busy_message(&mut self, cx: &mut Context<Self>) {
        self.provider_message = Some((
            Tone::Info,
            "Another key change is still being saved. Try again in a moment.".into(),
        ));
        cx.notify();
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some(ticket @ (Editing::Key(id), _)) = self.editing.ticket() else {
            return;
        };
        let Some(op) = self.ops.begin() else {
            self.busy_message(cx);
            return;
        };
        // Read and immediately clear: the value is never kept in page
        // state. It moves into the worker that writes the keyring.
        let key = self.key_input.read(cx).text().to_string();
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.invalidate_tests();
        let secrets = self.pcx.services.secrets.clone();
        let file = self.pcx.services.config_file.clone();
        let write = lf_ui::shutdown::register_pending_write();
        self.provider_message = Some((Tone::Info, "Saving the key…".into()));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = ctl::save_key_and_apply(secrets.as_ref(), &file, id, &key);
                    drop(write);
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ops.finish(op);
                this.invalidate_tests();
                // Only close the editor instance this save was started from,
                // and only while nothing new was typed into it (the input
                // was cleared at submit, so any text is a newer draft).
                let untouched = this.key_input.read(cx).text().is_empty();
                this.provider_message = Some(match result {
                    Ok(ctl::KeySaved::Activated(saved)) => {
                        this.pcx.services.set_cached_config(saved.clone());
                        this.pcx.config_changed(cx);
                        (
                            Tone::Success,
                            format!(
                                "New key saved and in use: cleanup and Ask now use {} with it.",
                                lf_ui::models_layout::summary_values(&saved).1
                            ),
                        )
                    }
                    Ok(ctl::KeySaved::NotActivated(reason)) => (
                        Tone::Warning,
                        format!(
                            "New key saved, but cleanup and Ask still use the previous key: {reason} Use “Use new key” to try again."
                        ),
                    ),
                    Ok(ctl::KeySaved::Saved) => (
                        Tone::Success,
                        "Key saved to your system keyring. Use the provider to switch cleanup to it.".into(),
                    ),
                    Err(err) => (Tone::Error, ctl::error_text(&err)),
                });
                if !matches!(this.provider_message, Some((Tone::Error, _))) {
                    this.editing.close_after_success(ticket, untouched);
                }
                this.load_providers(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        let Some(ticket @ (Editing::Use(id), _)) = self.editing.ticket() else {
            return;
        };
        let Some(op) = self.ops.begin() else {
            self.busy_message(cx);
            return;
        };
        let model = self.model_input.read(cx).text().trim().to_string();
        let url = self.url_input.read(cx).text().trim().to_string();
        // What was submitted (model and URL aren't secrets), to tell a
        // newer draft typed while switching from the submitted one.
        let submitted = (model.clone(), url.clone());
        let secrets = self.pcx.services.secrets.clone();
        let file = self.pcx.services.config_file.clone();
        let write = lf_ui::shutdown::register_pending_write();
        self.invalidate_tests();
        self.provider_message = Some((Tone::Info, "Switching provider…".into()));
        cx.spawn(async move |this, cx| {
            // Activation reads the key and writes the keyring snapshot: off
            // the UI thread.
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = ctl::activate(
                        secrets.as_ref(),
                        &file,
                        id,
                        (id == "custom").then_some(url.as_str()),
                        Some(model.as_str()).filter(|m| !m.is_empty()),
                    );
                    drop(write);
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ops.finish(op);
                this.invalidate_tests();
                let untouched = (
                    this.model_input.read(cx).text().trim().to_string(),
                    this.url_input.read(cx).text().trim().to_string(),
                ) == submitted;
                this.provider_message = Some(match result {
                    Ok(saved) => {
                        this.pcx.services.set_cached_config(saved.clone());
                        this.editing.close_after_success(ticket, untouched);
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
                this.load_providers(cx);
                this.pcx.config_changed(cx);
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
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
                    let _ = this.update(cx, |p, cx| p.remove_key(id, cx));
                }),
            },
            cx,
        );
    }

    fn remove_key(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(op) = self.ops.begin() else {
            self.busy_message(cx);
            return;
        };
        self.invalidate_tests();
        let secrets = self.pcx.services.secrets.clone();
        let file = self.pcx.services.config_file.clone();
        let write = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = ctl::remove_key(secrets.as_ref(), &file, id);
                    drop(write);
                    result
                })
                .await;
            let _ = this.update(cx, |p, cx| {
                p.ops.finish(op);
                p.invalidate_tests();
                p.provider_message = Some(match result {
                    Ok(()) => (Tone::Success, "Key removed.".into()),
                    Err(e) => (Tone::Error, ctl::error_text(&e)),
                });
                p.load_providers(cx);
                p.pcx.config_changed(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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
            .child(ui::group_rows(
                &t,
                [
                    ui::setting_row(
                        &t,
                        "Engine",
                        "Automatic picks the best engine for this computer when LocalFlow starts.",
                        self.speech_provider.clone().into_any_element(),
                        ctl::speech_unavailable_reason(&current).map(|r| (Tone::Warning, r.into())),
                    )
                    .into_any_element(),
                    ui::setting_row(
                        &t,
                        "Model size",
                        if self.model_ids.is_empty() {
                            "This engine runs one fixed model."
                        } else {
                            "Larger models are more accurate and slower."
                        },
                        self.speech_model.clone().into_any_element(),
                        None,
                    )
                    .into_any_element(),
                ],
                ui::row_inset(),
            )),
        );
        if !ctl::speech_available("mlx-whisper") {
            body = body.child(ui::supporting(
                &t,
                "MLX Whisper isn't offered here: it needs a Mac with Apple silicon.",
            ));
        }
        if let Some((tone, msg)) = self.speech_message.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        body.child(self.render_storage(cx))
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
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
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
            rows.push(
                ui::setting_row(
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
                )
                .into_any_element(),
            );
        }
        rows.push(
            ui::setting_row(
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
            )
            .into_any_element(),
        );
        // Test what cleanup really uses — its provider, host and model —
        // on explicit click only.
        if !config.field_str("llm_provider").unwrap_or("").is_empty() {
            let target = provider_ops::configured_target(&config);
            let blocker = match &target {
                provider_ops::TestTarget::Configured { model, .. } if model.trim().is_empty() => {
                    Some("Choose a model first")
                }
                _ => None,
            };
            rows.push(
                ui::setting_row(
                    &t,
                    "Connection",
                    format!("Checks that {} answers.", target.endpoint()),
                    self.test_button(target, blocker, cx).into_any_element(),
                    None,
                )
                .into_any_element(),
            );
        }
        sec = sec.child(ui::group_rows(&t, rows, ui::row_inset()));
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
        let rows = match &self.providers {
            Loadable::Ready(snapshot) => snapshot.rows.clone(),
            _ => Vec::new(),
        };
        let mut list = ui::list(&t);
        if !matches!(self.providers, Loadable::Ready(_)) {
            list = list.child(
                div()
                    .pl(ui::row_inset())
                    .pr(space::s4())
                    .py(space::s5())
                    .child(ui::secondary(&t, "Checking saved keys…")),
            );
        }
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
                            .disabled(self.ops.busy())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.ask_remove(id, label, cx)),
                            ),
                    );
                }
            }
            if row.has_key || !row.needs_key || row.env_override.is_some() {
                if let Ok(preset) = lf_providers::keys::get_preset(id) {
                    let target = provider_ops::row_target(preset, &config);
                    let blocker = match &target {
                        provider_ops::TestTarget::Preset {
                            needs_key, model, ..
                        } => key_import::test_blocker(
                            *needs_key,
                            row.has_key,
                            row.env_override.is_some(),
                            model,
                        ),
                        provider_ops::TestTarget::Configured { model, .. } => {
                            model.trim().is_empty().then_some("Choose a model first")
                        }
                    };
                    // The custom preset has no endpoint until it's in use.
                    if id != "custom" || row.active {
                        actions = actions.child(self.test_button(target, blocker, cx));
                    }
                }
            }
            let editing = self
                .editing
                .current()
                .filter(|e| matches!(e, Editing::Key(x) | Editing::Use(x) if *x == id));
            let mut item = div()
                .flex()
                .flex_col()
                .gap(space::s4())
                .pl(ui::row_inset())
                .pr(space::s4())
                .py(space::s5())
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
                            .child(Button::new("save-key", "Save key").primary().small().disabled(self.ops.busy()).on_click(cx.listener(|this, _, _, cx| this.save_key(cx))))
                            .child(Button::new("cancel-key", "Cancel").ghost().small().on_click(cx.listener(|this, _, _, cx| {
                                this.key_input.update(cx, |i, cx| i.clear(cx));
                                this.editing.close();
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
                                    .busy(self.ops.busy())
                                    .on_click(cx.listener(|this, _, _, cx| this.activate(cx))),
                            )
                            .child(
                                Button::new("cancel-use", "Cancel")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editing.close();
                                        cx.notify();
                                    })),
                            ),
                    );
                }
                _ => {}
            }
            if i > 0 {
                list = list.child(ui::separator(&t, ui::row_inset()));
            }
            list = list.child(item);
        }
        let import = (self.import.card.is_none()).then(|| {
            Button::new("import-open", "Import keys…")
                .small()
                .icon("upload")
                .on_click(cx.listener(|this, _, _, cx| this.open_import(cx)))
                .into_any_element()
        });
        let mut body = div().flex().flex_col().gap(space::s6());
        if let Some(card) = self.render_import(cx) {
            body = body.child(card);
        }
        body = body.child(ui::section(&t, "Providers", Some("Where cleanup and Ask run. Ollama stays on this device; the others are remote services.".into()), import).child(list));
        if let Some((tone, msg)) = self.provider_message.clone() {
            body = body.child(ui::banner(&t, tone, msg, None));
        }
        if services.is_sandbox() {
            body = body.child(ui::supporting(&t, "Sandbox: keys are kept in memory for this window only and are never written to your keyring."));
        }
        body
    }
}

// ------------------------------------------------- speech model storage ---

impl ModelsPage {
    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let deps = self.pcx.services.setup.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let hw = deps.platform.detect_hardware()?;
                    let models =
                        lf_ui::setup_flow::available_stt_models(&hw, deps.platform.as_ref());
                    let (recommended, _) =
                        lf_ui::setup_flow::recommend(&hw, deps.platform.as_ref());
                    Ok::<_, String>((models, recommended))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match found {
                    Ok((models, recommended)) => {
                        this.catalog = models;
                        this.recommended = recommended;
                    }
                    Err(error) => this.installed = Loadable::Failed(error),
                }
                this.refresh_storage(cx);
            });
        })
        .detach();
    }

    /// Local inspection only: the cache inventory and which catalogue
    /// models are present. No network until Download.
    fn refresh_storage(&mut self, cx: &mut Context<Self>) {
        let services = &self.pcx.services;
        let storage = services.storage.clone();
        let deps = services.setup.clone();
        let config = services.config();
        let catalog = self.catalog.clone();
        self.inventory = Loadable::Loading;
        if deps.native_speech {
            self.installed = Loadable::Loading;
        } else {
            self.installed = Loadable::Failed(deps.speech_reason.clone().unwrap_or_default());
        }
        cx.spawn(async move |this, cx| {
            let (inventory, installed) = cx
                .background_executor()
                .spawn(async move {
                    let inventory = storage.inventory(&config);
                    let installed = deps
                        .native_speech
                        .then(|| deps.models.speech_downloads(&catalog));
                    (inventory, installed)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.inventory = match inventory {
                    Ok(inv) => Loadable::Ready(inv),
                    Err(error) => Loadable::Failed(error),
                };
                if let Some(installed) = installed {
                    this.installed = match installed {
                        Ok(map) => Loadable::Ready(map),
                        Err(error) => Loadable::Failed(error),
                    };
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn download(&mut self, model_id: &str, cx: &mut Context<Self>) {
        if let Some(reason) = self.pcx.runtime_busy() {
            self.storage_message = Some((Tone::Warning, reason.into()));
            cx.notify();
            return;
        }
        let Some(model) = self.catalog.iter().find(|m| m.id == model_id).cloned() else {
            return;
        };
        let backend = self.pcx.services.setup.models.clone();
        self.pcx.services.downloads.start(
            &jobs::speech_key(&model.id),
            Box::new(move |progress, cancel| backend.download_speech(&model, progress, cancel)),
        );
        self.poll_downloads(cx);
        cx.notify();
    }

    fn poll_downloads(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let go_on = this
                .update(cx, |this, cx| {
                    let running = this.pcx.services.downloads.any_running();
                    if !running {
                        this.polling = false;
                        this.refresh_storage(cx);
                    }
                    cx.notify();
                    running
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        })
        .detach();
    }

    fn use_model(&mut self, model_id: &str, cx: &mut Context<Self>) {
        let Some(model) = self.catalog.iter().find(|m| m.id == model_id).cloned() else {
            return;
        };
        let installed = self
            .installed
            .ready()
            .and_then(|m| m.get(model_id).copied());
        self.save_speech(model.backend, Some(model.model_size), cx);
        if installed == Some(false) {
            self.storage_message = Some((
                Tone::Warning,
                format!(
                    "{} isn't downloaded yet. LocalFlow can't transcribe with it until it is.",
                    storage::engine_label(model.backend).to_string() + " " + model.label
                ),
            ));
        }
        self.refresh_storage(cx);
    }

    fn ask_delete(&mut self, repo: String, label: String, size: u64, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: format!("Delete {label}?").into(),
                body: format!(
                    "Frees {}. You can download it again later.",
                    storage::format_bytes(size)
                )
                .into(),
                verb: "Delete".into(),
                destructive: true,
                on_confirm: Rc::new(move |_, cx| {
                    let repo = repo.clone();
                    let label = label.clone();
                    let _ = this.update(cx, |p, cx| p.delete(repo, label, cx));
                }),
            },
            cx,
        );
    }

    fn delete(&mut self, repo: String, label: String, cx: &mut Context<Self>) {
        if let Some(reason) = self.pcx.runtime_busy() {
            self.storage_message = Some((Tone::Warning, reason.into()));
            cx.notify();
            return;
        }
        // No speech download (from here or setup) may run alongside, and
        // none can start until this deletion has finished.
        let Some(guard) = self.pcx.services.downloads.begin_delete() else {
            self.storage_message = Some((Tone::Warning, DOWNLOADING.into()));
            cx.notify();
            return;
        };
        let storage = self.pcx.services.storage.clone();
        let root = self.pcx.services.root.clone();
        let write = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            // Protection is re-checked against the config as it is now (in
            // the config transaction) and the current leases.
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result =
                        storage::delete_with_current_config(storage.as_ref(), &repo, || {
                            crate::data::load_config_at(&root)
                        });
                    drop(guard);
                    drop(write);
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(freed) => {
                        this.storage_message = None;
                        this.pcx.toast(
                            format!("Deleted {label} · freed {}", storage::format_bytes(freed)),
                            cx,
                        );
                    }
                    Err(error) => this.storage_message = Some((Tone::Error, error)),
                }
                this.refresh_storage(cx);
            });
        })
        .detach();
    }

    fn render_storage(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let config = services.config();
        let simulated = services.setup.simulated;
        let unavailable = services.setup.speech_reason.clone();
        let selected = lf_ui::setup_flow::current_stt_id(&config, services.setup.platform.as_ref());
        let rows = storage::model_rows(
            &self.catalog,
            &self.installed,
            &services.downloads,
            &selected,
            &self.recommended,
            unavailable.as_deref(),
        );
        let busy = self.pcx.runtime_busy();
        // A download may be writing into any managed folder (several GGML
        // sizes share one repo), so nothing is deleted while one runs.
        let downloading = speech_download_running(&services.downloads);
        let refresh = Button::new("storage-refresh", "Refresh")
            .small()
            .icon("refresh-cw")
            .on_click(cx.listener(|this, _, _, cx| this.refresh_storage(cx)));
        let mut list = ui::list(&t);
        for (i, row) in rows.into_iter().enumerate() {
            let id = row.id.clone();
            let (tone, word) = match &row.state {
                RowState::NotInstalled => (Tone::Neutral, "Not downloaded"),
                RowState::Downloading(_) => (Tone::Info, "Downloading"),
                RowState::Failed(_) => (Tone::Error, "Failed"),
                RowState::Installed => (Tone::Neutral, "Downloaded"),
                RowState::Active => (Tone::Success, "In use"),
                RowState::Unavailable(_) => (Tone::Neutral, "Not available"),
                RowState::Unknown(_) => (Tone::Neutral, "Unknown"),
            };
            let size = storage::format_bytes(u64::from(row.size_mb) * 1_000_000);
            let mut actions = div().flex().items_center().gap(space::s3());
            match &row.state {
                RowState::NotInstalled | RowState::Failed(_) => {
                    let label = if matches!(row.state, RowState::Failed(_)) {
                        "Retry"
                    } else {
                        "Download"
                    };
                    let id = id.clone();
                    actions = actions.child(
                        Button::new(("model-download", i as u64), label)
                            .small()
                            .icon("download")
                            .disabled(busy.is_some())
                            .tooltip(busy.unwrap_or("Download this model now"))
                            .on_click(cx.listener(move |this, _, _, cx| this.download(&id, cx))),
                    );
                }
                RowState::Downloading(_) => {
                    let downloads = services.downloads.clone();
                    let key = jobs::speech_key(&id);
                    actions = actions.child(
                        Button::new(("model-cancel", i as u64), "Cancel")
                            .small()
                            .ghost()
                            .on_click(move |_, _, _| {
                                downloads.cancel(&key);
                            }),
                    );
                }
                _ => {}
            }
            if !row.selected
                && !matches!(
                    row.state,
                    RowState::Unavailable(_) | RowState::Downloading(_)
                )
            {
                let id = id.clone();
                actions = actions.child(
                    Button::new(("model-use", i as u64), "Use")
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.use_model(&id, cx))),
                );
            }
            let mut item = div()
                .flex()
                .flex_col()
                .gap(space::s2())
                .pl(ui::row_inset())
                .pr(space::s4())
                .py(space::s4())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .child(ui::label(&t, row.label.clone()).flex_1())
                        .when(row.recommended, |d| {
                            d.child(ui::token(&t, Tone::Accent, "Recommended"))
                        })
                        .child(ui::token(&t, tone, word))
                        .when(simulated, |d| {
                            d.child(ui::token(&t, Tone::Warning, "Simulated"))
                        })
                        .child(actions),
                )
                .child(ui::supporting(&t, format!("{} · about {size}", row.blurb)));
            match &row.state {
                RowState::Downloading(job) => {
                    item = item.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(div().flex_1().child(ui::progress(&t, job.fraction())))
                            .child(ui::supporting(&t, job.progress_text())),
                    );
                    if job.stalled(Instant::now()) {
                        item =
                            item.child(ui::supporting(&t, "Still working… check your connection."));
                    }
                }
                RowState::Failed(error) => item = item.child(ui::error_line(&t, error.clone())),
                RowState::Active => item = item.child(ui::supporting(&t, "Selected in settings")),
                RowState::Unknown(reason) if !reason.is_empty() => {
                    item = item.child(ui::supporting(&t, reason.clone()))
                }
                _ => {}
            }
            if row.selected && matches!(row.state, RowState::NotInstalled) {
                item = item.child(
                    ui::supporting(&t, "Selected in settings, but not downloaded yet.")
                        .text_color(t.tone(Tone::Warning).0),
                );
            }
            if i > 0 {
                list = list.child(ui::separator(&t, ui::row_inset()));
            }
            list = list.child(item);
        }
        let mut section = ui::section(
            &t,
            "Models on this computer",
            Some("Downloads start only when you press Download.".into()),
            Some(refresh.into_any_element()),
        );
        if let Some(reason) = &unavailable {
            section = section.child(ui::banner(&t, Tone::Info, reason.clone(), None));
        }
        section = section.child(list);
        // Cached repository folders (what Delete removes).
        match &self.inventory {
            Loadable::Ready(inv) => {
                let mut stored = ui::list(&t);
                if inv.models.is_empty() {
                    stored = stored.child(
                        div()
                            .pl(ui::row_inset())
                            .pr(space::s4())
                            .py(space::s4())
                            .child(ui::secondary(&t, "No speech models are stored yet.")),
                    );
                }
                for (i, model) in inv.models.iter().enumerate() {
                    let reason = model
                        .protection_reason()
                        .or(busy)
                        .or(downloading.then_some(DOWNLOADING));
                    let (repo, label, size) =
                        (model.repo.clone(), model.label.clone(), model.size_bytes);
                    if i > 0 {
                        stored = stored.child(ui::separator(&t, ui::row_inset()));
                    }
                    stored = stored.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .pl(ui::row_inset())
                            .pr(space::s4())
                            .py(space::s4())
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(ui::label(&t, model.label.clone()))
                                    .child(ui::supporting(
                                        &t,
                                        format!(
                                            "{} · {}",
                                            storage::format_bytes(model.size_bytes),
                                            model.protection_reason().unwrap_or("Not in use")
                                        ),
                                    )),
                            )
                            .when(model.protected(), |d| {
                                d.child(ui::token(&t, Tone::Success, model.status.clone()))
                            })
                            .child(
                                Button::new(("model-delete", i as u64), "Delete…")
                                    .small()
                                    .ghost()
                                    .icon("trash-2")
                                    .disabled(reason.is_some())
                                    .tooltip(reason.unwrap_or("Delete this model from disk"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ask_delete(repo.clone(), label.clone(), size, cx)
                                    })),
                            ),
                    );
                }
                let footer = match &inv.cache_dir {
                    Some(dir) => {
                        let open = dir.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(
                                ui::supporting(
                                    &t,
                                    format!(
                                        "Stored in {} · {} used",
                                        dir.display(),
                                        storage::format_bytes(inv.total_bytes())
                                    ),
                                )
                                .flex_1(),
                            )
                            .child(
                                Button::new("storage-open", "Open folder")
                                    .small()
                                    .icon("folder-open")
                                    .disabled(!dir.exists())
                                    .on_click(move |_, _, cx| cx.open_with_system(&open)),
                            )
                    }
                    None => div().child(ui::supporting(
                        &t,
                        format!(
                        "Sandbox: {} of example models kept in memory. Nothing is stored on disk.",
                        storage::format_bytes(inv.total_bytes())
                    ),
                    )),
                };
                section = section
                    .child(ui::label(&t, "Stored speech model files"))
                    .child(stored)
                    .child(footer);
            }
            Loadable::Failed(error) => {
                section = section.child(ui::supporting(&t, format!("Stored models: {error}")));
            }
            _ => section = section.child(ui::supporting(&t, "Checking stored models…")),
        }
        if let Some((tone, msg)) = self.storage_message.clone() {
            section = section.child(ui::banner(&t, tone, msg, None));
        }
        section
    }
}

// ------------------------------------------------ key import & testing ---

impl ModelsPage {
    fn snapshot(&self) -> ctl::ProviderSnapshot {
        self.providers.ready().cloned().unwrap_or_default()
    }

    /// From the cached snapshot (no keyring or environment read here).
    fn env_override(&self) -> impl Fn(&str) -> Option<String> + Send + 'static {
        let snapshot = self.snapshot();
        move |id: &str| snapshot.env_override(id)
    }

    /// From the cached snapshot (no keyring read here).
    fn has_key(&self) -> impl Fn(&str) -> bool + Send + 'static {
        let snapshot = self.snapshot();
        move |id: &str| snapshot.has_key(id)
    }

    /// Re-read key presence (keyring + environment) on a worker; a newer
    /// load wins over an older one.
    fn load_providers(&mut self, cx: &mut Context<Self>) {
        self.providers_generation += 1;
        let generation = self.providers_generation;
        if !matches!(self.providers, Loadable::Ready(_)) {
            self.providers = Loadable::Loading;
        }
        let services = &self.pcx.services;
        let secrets = services.secrets.clone();
        let file = services.config_file.clone();
        let config = services.config();
        // The sandbox never consults the real environment.
        let sandbox = services.is_sandbox();
        cx.spawn(async move |this, cx| {
            let snapshot = cx
                .background_executor()
                .spawn(async move {
                    let env_set = move |name: &str| !sandbox && std::env::var_os(name).is_some();
                    ctl::ProviderSnapshot::load(secrets.as_ref(), &file, &config, &env_set)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.providers_generation == generation {
                    this.providers = Loadable::Ready(snapshot);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_import(&mut self, cx: &mut Context<Self>) {
        self.import.open();
        self.detect_keys(Source::Environment, cx);
    }

    /// Scan a source for the current card; a result for an older card,
    /// source or picker launch is dropped.
    fn detect_keys(&mut self, source: Source, cx: &mut Context<Self>) {
        let Some(generation) = self.import.begin_scan(matches!(source, Source::File(_))) else {
            return;
        };
        let services = &self.pcx.services;
        // The sandbox never reads the real environment or a real file: a
        // fixed example environment, and a fixed example .env inside the
        // sandbox whatever file was chosen.
        let sandbox_dir = services.root.sandbox_dir.clone();
        let environ = if sandbox_dir.is_some() {
            key_import::sandbox_environment()
        } else {
            std::env::vars().collect()
        };
        cx.spawn(async move |this, cx| {
            let detected = cx
                .background_executor()
                .spawn(async move {
                    let source = match (source, sandbox_dir) {
                        (Source::File(_), Some(dir)) => {
                            match provider_ops::sandbox_env_file(&dir) {
                                Ok(fixture) => Source::File(fixture),
                                Err(e) => {
                                    return Err(format!(
                                        "Couldn't prepare the sandbox example: {e}"
                                    ))
                                }
                            }
                        }
                        (source, _) => source,
                    };
                    KeyImport::detect(source, &environ)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.import.scan_done(generation, detected) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn pick_env_file(&mut self, cx: &mut Context<Self>) {
        // Launching the picker supersedes any pending scan of this card.
        let Some(generation) = self.import.begin_scan(true) else {
            return;
        };
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| {
                if this.import.generation() != generation {
                    return; // the card changed while the picker was open
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.detect_keys(Source::File(path), cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        if let Some(card) = this.import.card.as_mut() {
                            card.error = Some("Couldn't open the file picker on this desktop. Type the file's path instead.".into());
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn import_keys(&mut self, cx: &mut Context<Self>) {
        let has_key = self.has_key();
        let env_override = self.env_override();
        if self.ops.busy() {
            self.busy_message(cx);
            return;
        }
        let Some((generation, data)) = self.import.begin_import() else {
            return;
        };
        let Some(op) = self.ops.begin() else {
            return;
        };
        self.invalidate_tests();
        let secrets = self.pcx.services.secrets.clone();
        let file = self.pcx.services.config_file.clone();
        let write = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            // Keyring writes happen on a worker; the candidates come back so
            // a failure on the same card keeps it as it was.
            let (data, result) = cx
                .background_executor()
                .spawn(async move {
                    let keys = lf_providers::ProviderKeys::new(secrets.as_ref(), &file);
                    let result = data.import(&keys, &has_key, &env_override);
                    drop(write);
                    (data, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ops.finish(op);
                this.invalidate_tests();
                if let ImportDone::Imported(labels) =
                    this.import.import_done(generation, data, result)
                {
                    this.pcx.toast(
                        format!(
                            "Imported {} key{}: {}",
                            labels.len(),
                            if labels.len() == 1 { "" } else { "s" },
                            labels.join(", ")
                        ),
                        cx,
                    );
                    this.pcx.config_changed(cx);
                }
                this.load_providers(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn test(&mut self, target: provider_ops::TestTarget, cx: &mut Context<Self>) {
        self.test_generation += 1;
        let generation = self.test_generation;
        let key = target.key();
        self.tests.insert(key, TestState::Busy(generation));
        let services = &self.pcx.services;
        let sandbox = services.is_sandbox();
        let secrets = services.secrets.clone();
        let file = services.config_file.clone();
        let config = services.config();
        cx.spawn(async move |this, cx| {
            let outcome = if sandbox {
                // No socket: the sandbox fake answers OK after 400 ms.
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                TestOutcome::Ok
            } else {
                cx.background_executor()
                    .spawn(async move {
                        provider_ops::run_test(&target, secrets.as_ref(), &file, &config)
                    })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                if this.tests.get(key) == Some(&TestState::Busy(generation)) {
                    this.tests.insert(key, TestState::Done(outcome));
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn test_button(
        &self,
        target: provider_ops::TestTarget,
        blocker: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let t = theme(cx).clone();
        let key = target.key();
        let state = self.tests.get(key).copied();
        // The tooltip names exactly what the click will contact.
        let tip = blocker
            .map(str::to_string)
            .unwrap_or_else(|| target.tooltip());
        let button = Button::new(SharedString::from(format!("test-{key}")), "Test connection")
            .small()
            .busy(matches!(state, Some(TestState::Busy(_))))
            .disabled(blocker.is_some())
            .tooltip(tip)
            .on_click(cx.listener(move |this, _, _, cx| this.test(target.clone(), cx)));
        let result = match state {
            Some(TestState::Done(outcome)) => Some(ui::status_dot(
                &t,
                match outcome {
                    TestOutcome::Ok => Tone::Success,
                    TestOutcome::Failed => Tone::Error,
                    TestOutcome::Unable => Tone::Warning,
                },
                outcome.text(),
            )),
            _ => None,
        };
        div()
            .flex()
            .items_center()
            .gap(space::s4())
            .child(button)
            .children(result)
            .when(
                self.pcx.services.is_sandbox() && matches!(state, Some(TestState::Done(_))),
                |d| d.child(ui::token(&t, Tone::Warning, "Simulated")),
            )
    }

    fn render_import(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let card = self.import.card.as_ref()?;
        let t = theme(cx).clone();
        let has_key = self.has_key();
        let env_override = self.env_override();
        let this = cx.entity().downgrade();
        let source = ui::Segmented::new(
            "import-source",
            ["Environment", ".env file"],
            Some(usize::from(card.file)),
        )
        .disabled(card.saving)
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |p, cx| {
                if i == 0 {
                    p.detect_keys(Source::Environment, cx);
                } else if p.import.begin_scan(true).is_some() {
                    // Waits for a chosen or typed file.
                    cx.notify();
                }
            });
        });
        let mut body = ui::card(&t).flex_col().gap(space::s5()).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(ui::heading(&t, "Import keys"))
                .child(source),
        );
        if card.file {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(div().flex_1().child(self.path_input.clone()))
                    .child(
                        Button::new("import-choose", "Choose file…")
                            .small()
                            .icon("folder-open")
                            .disabled(card.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.pick_env_file(cx))),
                    ),
            );
        }
        if let Some(error) = &card.error {
            body = body.child(ui::error_line(&t, error.clone()));
        }
        if let Some(data) = &card.data {
            body = body.child(ui::secondary(&t, data.summary()));
            let rows = data.rows(&has_key, &env_override);
            // The card is the group: rows sit directly in it (no inner list
            // container), separated by the shared inset hairline.
            let mut list = div().flex().flex_col().w_full();
            for (i, row) in rows.iter().enumerate() {
                let key = row.key.clone();
                let this = cx.entity().downgrade();
                if i > 0 {
                    list = list.child(ui::separator(&t, gpui::px(0.)));
                }
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .py(space::s4())
                        .child(
                            ui::Switch::new(
                                ("import-row", i as u64),
                                row.selected,
                                format!("Import {}", row.provider_label),
                            )
                            .disabled(row.blocked.is_some())
                            .on_toggle(move |on, _, cx| {
                                let key = key.clone();
                                let _ = this.update(cx, |p, cx| {
                                    if let Some(data) =
                                        p.import.card.as_mut().and_then(|c| c.data.as_mut())
                                    {
                                        data.toggle(&key, on);
                                    }
                                    cx.notify();
                                });
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(ui::label(&t, row.provider_label.clone()))
                                .child(ui::supporting(&t, row.source.clone()))
                                .when_some(row.blocked.clone(), |d, why| {
                                    d.child(ui::supporting(&t, why))
                                }),
                        )
                        .when(row.replaces, |d| {
                            d.child(ui::token(&t, Tone::Warning, "Replaces saved key"))
                        }),
                );
            }
            if !rows.is_empty() {
                body = body.child(list);
            }
            let count = data.selected_count(&has_key, &env_override);
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(space::s4())
                    .when(count == 0, |d| {
                        d.child(ui::supporting(&t, "Nothing selected"))
                    })
                    .child(
                        Button::new("import-cancel", "Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.import.close();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(
                            "import-go",
                            format!("Import {count} key{}", if count == 1 { "" } else { "s" }),
                        )
                        .primary()
                        .small()
                        .busy(card.saving)
                        .disabled(count == 0 || self.ops.busy())
                        .on_click(cx.listener(|this, _, _, cx| this.import_keys(cx))),
                    ),
            );
            body = body.child(ui::supporting(&t, "Importing saves keys only. Which provider cleanup and Ask use doesn't change; choose Use on a provider to switch."));
        } else if card.file && card.error.is_none() {
            body = body.child(ui::supporting(&t, "Choose a .env file or type its path and press Enter. Only literal NAME=value lines are read; nothing is executed."));
        }
        if self.pcx.services.is_sandbox() {
            body = body.child(
                div()
                    .flex()
                    .child(ui::token(&t, Tone::Warning, "Simulated"))
                    .child(ui::supporting(
                        &t,
                        " Sandbox: a fixed example environment is scanned; keys stay in memory.",
                    )),
            );
        }
        Some(body)
    }
}

const DOWNLOADING: &str =
    "A speech model is downloading. Wait for it to finish or cancel it first.";

fn speech_download_running(downloads: &crate::controllers::jobs::Downloads) -> bool {
    downloads
        .running()
        .iter()
        .any(|key| key.starts_with("speech:"))
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
