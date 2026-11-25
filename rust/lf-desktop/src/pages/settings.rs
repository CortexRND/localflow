//! Settings: every manual row is a real `[section] key` from
//! `controllers::settings::SPECS`, saved on change with validation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{div, prelude::*, ClipboardItem, Context, Entity, SharedString, Window};
use serde_json::{json, Value};

use crate::app::{Confirm, PageCx};
use crate::appearance::{self, AppearanceMode};
use crate::controllers::assistant::{mask_secrets, Assistant};
use crate::controllers::history;
use crate::controllers::models::data_leaves_text;
use crate::controllers::runtime_state::SimulatedState;
use crate::controllers::settings::{self as ctl, Control, Spec, Tab, SPECS};
use crate::page::Page;
use crate::pages::{column, header, page};
use crate::remote::Health;
use crate::theme::{space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button, Switch};
use lf_ui::hub_settings::{self as hs, Status};

pub struct SettingsPage {
    pcx: PageCx,
    tab: Tab,
    selects: HashMap<usize, (Entity<Select>, Vec<Value>)>,
    inputs: HashMap<usize, Entity<TextInput>>,
    errors: HashMap<usize, String>,
    restart: Vec<usize>,
    mics: Option<Vec<String>>,
    /// Last value pushed into each text field; a field whose text differs is
    /// being edited and is left alone by config syncs.
    synced: HashMap<usize, String>,
    assistant: Assistant,
    request: Entity<TextInput>,
    /// The config the assistant's preview was computed against.
    seen_config: Value,
    simulated: Entity<Select>,
    /// The appearance choice couldn't be stored (it still applies now).
    appearance_error: Option<String>,
}

/// Extra rows and sections of a tab, beside its `[section] key` rows.
struct Extra {
    /// Above the tab's group (the Shortcuts note).
    top: Vec<gpui::AnyElement>,
    /// Rows that join the tab's group.
    rows: Vec<gpui::AnyElement>,
    /// Sections after the group.
    sections: Vec<gpui::AnyElement>,
}

fn spec_index(spec: &Spec) -> usize {
    SPECS
        .iter()
        .position(|s| s.section == spec.section && s.key == spec.key)
        .expect("spec must exist in SPECS")
}

impl SettingsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let request = cx.new(|cx| {
            TextInput::new(cx, hs::PLACEHOLDER)
                .multiline(2, 4)
                .max_chars(hs::MAX_DRAFT_CHARS)
                .label("Describe a change")
        });
        let simulated = cx.new(|cx| {
            Select::new(
                cx,
                SimulatedState::ALL
                    .iter()
                    .map(|s| SharedString::from(s.label()))
                    .collect(),
                Some(0),
            )
            .width(240.)
        });
        cx.subscribe(&simulated, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let Some(state) = SimulatedState::ALL.get(*i) {
                this.pcx.services.simulated_runtime.set(*state);
                this.pcx.reprobe(cx);
            }
        })
        .detach();
        let assistant = Assistant::new(pcx.services.assistant.clone());
        let seen_config = lf_core::config::config_to_dict(&pcx.services.config());
        let mut page = SettingsPage {
            pcx,
            tab: Tab::General,
            selects: HashMap::new(),
            inputs: HashMap::new(),
            errors: HashMap::new(),
            restart: Vec::new(),
            mics: None,
            synced: HashMap::new(),
            assistant,
            request,
            seen_config,
            simulated,
            // A failure from an earlier visit stays visible.
            appearance_error: appearance::queue().error(),
        };
        for (i, spec) in SPECS.iter().enumerate() {
            match spec.control {
                Control::Choice(_) | Control::Microphone => {
                    let select = cx.new(|cx| Select::new(cx, Vec::new(), None).width(240.));
                    cx.subscribe(&select, move |this, _, event, cx| {
                        let SelectEvent::Changed(choice) = event;
                        this.choose(i, *choice, cx);
                    })
                    .detach();
                    page.selects.insert(i, (select, Vec::new()));
                }
                Control::Shortcut { .. } | Control::Text { .. } => {
                    let placeholder = match spec.control {
                        Control::Text { placeholder } => placeholder,
                        _ => "e.g. alt_r or <ctrl>+<alt>+1 (empty = off)",
                    };
                    let input = cx.new(|cx| TextInput::new(cx, placeholder).label(spec.label));
                    cx.subscribe(&input, move |this, _, event, cx| match event {
                        InputEvent::Submit => this.save_text(i, cx),
                        InputEvent::Changed => {
                            this.errors.remove(&i);
                            cx.notify();
                        }
                    })
                    .detach();
                    page.inputs.insert(i, input);
                }
                Control::Switch => {}
            }
        }
        page.load_mics(cx);
        page.load_assistant_state(cx);
        page.sync(cx);
        page
    }

    /// The assistant's undo summary, via `Request::Refresh` on the settings
    /// worker (never on this thread; no preview or network request).
    fn load_assistant_state(&mut self, cx: &mut Context<Self>) {
        let Ok((epoch, receiver)) = self.assistant.begin_refresh() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let completion = cx
                .background_executor()
                .spawn(async move { receiver.recv().ok() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(completion) = completion {
                    if this.assistant.refresh_done(epoch, completion) {
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.note_config(cx);
        self.sync(cx);
        cx.notify();
    }

    /// Settings moved under the assistant (manual row, Home, another
    /// process): a preview or pending answer becomes stale.
    fn note_config(&mut self, _: &mut Context<Self>) {
        // Our own Apply/Undo is being written: its completion updates the
        // baseline, so the change it makes can't make its preview stale.
        if self.assistant.writing {
            return;
        }
        let now = lf_core::config::config_to_dict(&self.pcx.services.config());
        if now != self.seen_config {
            self.seen_config = now;
            self.assistant.settings_changed();
        }
    }

    // ------------------------------------------------------- assistant ---

    fn preview(&mut self, cx: &mut Context<Self>) {
        let text = self.request.read(cx).text().to_string();
        let Some((token, request)) = self.assistant.start_preview(&text) else {
            cx.notify();
            return;
        };
        let agent = self.assistant.agent();
        cx.spawn(async move |this, cx| {
            let (result, model) = cx
                .background_executor()
                .spawn(async move { Assistant::propose(agent.as_ref(), &request) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.preview_done(token, result, model);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn write(
        &mut self,
        request: lf_ui::backends::settings_worker::Request,
        cx: &mut Context<Self>,
    ) {
        let receiver = match self.assistant.dispatch(request) {
            Ok(receiver) => receiver,
            Err(error) => {
                self.assistant.writing = false;
                self.assistant.state.failed(&error.to_string());
                cx.notify();
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let completion = cx
                .background_executor()
                .spawn(async move { receiver.recv().ok() })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(completion) = completion else {
                    this.assistant.writing = false;
                    this.assistant.state.failed("The settings worker stopped.");
                    cx.notify();
                    return;
                };
                let changed = this.assistant.completion(completion);
                if this.assistant.state.status == Status::Applied {
                    this.request.update(cx, |i, cx| i.clear(cx));
                }
                if changed {
                    this.pcx.services.reload_config();
                    this.seen_config = lf_core::config::config_to_dict(&this.pcx.services.config());
                    this.pcx.config_changed(cx);
                    this.sync(cx);
                    if !this.assistant.undo_skipped.is_empty() {
                        this.pcx.toast(this.assistant.message(), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_assistant(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let a = &self.assistant;
        let undo_tip = a
            .state
            .undo_lines
            .as_ref()
            .map(|lines| mask_secrets(&format!("Undo: {}", lines.join(", "))))
            .unwrap_or_else(|| hs::NOTHING_TO_UNDO.to_string());
        let undo = Button::new("assistant-undo", hs::UNDO_TITLE)
            .ghost()
            .small()
            .icon("undo-2")
            .disabled(a.state.undo_lines.is_none() || a.writing || a.state.busy())
            .tooltip(undo_tip)
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(request) = this.assistant.begin_undo() {
                    this.write(request, cx);
                }
            }));
        let toggle = Button::new("assistant-open", if a.expanded { "Hide" } else { "Open" })
            .small()
            .on_click(cx.listener(|this, _, _, cx| {
                this.assistant.expanded = !this.assistant.expanded;
                cx.notify();
            }));
        let mut card = ui::card(&t).flex_col().gap(space::s5()).child(
            div()
                .flex()
                .items_center()
                .gap(space::s4())
                .child(ui::icon("sparkles", t.icon_accent))
                .child(ui::heading(&t, hs::HEADING).flex_1())
                .child(undo)
                .child(toggle),
        );
        if !a.expanded {
            return card;
        }
        let busy = a.state.busy();
        let model_note = if self.pcx.services.is_sandbox() {
            "Sandbox: answers come from a fixed script; no model is contacted.".to_string()
        } else {
            mask_secrets(
                &hs::model_note(a.state.model.as_deref()).replace("your Mac", "this computer"),
            )
        };
        card = card
            .child(ui::secondary(&t, hs::INTRO))
            .child(self.request.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(ui::supporting(&t, model_note).flex_1())
                    .when(busy, |d| {
                        d.child(ui::supporting(&t, hs::BUSY_PREVIEW)).child(
                            Button::new("assistant-cancel-busy", "Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.assistant.cancel();
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new("assistant-preview", "Preview")
                            .primary()
                            .small()
                            .busy(busy)
                            .disabled(a.writing)
                            .on_click(cx.listener(|this, _, _, cx| this.preview(cx))),
                    ),
            );
        match a.state.status {
            Status::Preview => {
                let lines = a.lines();
                let mut preview = div()
                    .flex()
                    .flex_col()
                    .gap(space::s3())
                    .child(ui::label(&t, hs::PREVIEW_TITLE));
                if lines.is_empty() {
                    preview =
                        preview.child(ui::secondary(&t, "Nothing to change for that request."));
                }
                for line in lines {
                    preview = preview.child(ui::status_dot(&t, Tone::Accent, line));
                }
                let warnings = a.warnings();
                if !warnings.is_empty() {
                    preview = preview.child(ui::label(&t, hs::WARNINGS_TITLE));
                    for warning in warnings {
                        preview = preview.child(
                            ui::supporting(&t, hs::warning_text(&warning))
                                .text_color(t.tone(Tone::Warning).0),
                        );
                    }
                }
                for note in a.notes() {
                    preview = preview.child(ui::supporting(&t, hs::skipped_text(&note)));
                }
                if let Some(error) = &a.apply_error {
                    preview = preview.child(ui::banner(&t, Tone::Error, error.clone(), None));
                }
                let empty = a.state.proposal.as_ref().is_none_or(|p| p.is_empty());
                preview = preview.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(space::s4())
                        .child(
                            Button::new("assistant-cancel", "Cancel")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.assistant.cancel();
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("assistant-apply", "Apply")
                                .primary()
                                .small()
                                .busy(a.writing)
                                .disabled(empty)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(request) = this.assistant.begin_apply() {
                                        this.write(request, cx);
                                    }
                                })),
                        ),
                );
                card = card.child(preview);
            }
            Status::Applied => {
                card = card.child(ui::banner(&t, Tone::Success, a.message(), None));
                for line in a.lines() {
                    card = card.child(ui::supporting(&t, line));
                }
                if !a.undo_skipped.is_empty() {
                    card = card.child(ui::banner(
                        &t,
                        Tone::Warning,
                        format!(
                            "Kept because they changed since: {}",
                            a.undo_skipped.join("; ")
                        ),
                        None,
                    ));
                }
            }
            Status::Error => {
                let stale =
                    a.state.message == hs::STALE_PREVIEW || a.state.message == hs::STALE_PENDING;
                card = card.child(ui::banner(
                    &t,
                    Tone::Error,
                    if stale {
                        a.message()
                    } else {
                        format!("{}: {}", hs::ERROR_TITLE, a.message())
                    },
                    Some(
                        Button::new(
                            "assistant-retry",
                            if stale { "Preview again" } else { "Try again" },
                        )
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.preview(cx)))
                        .into_any_element(),
                    ),
                ));
            }
            _ => {}
        }
        card
    }

    pub fn open_tab(&mut self, target: &str, cx: &mut Context<Self>) {
        self.tab = match target {
            "shortcuts" => Tab::Shortcuts,
            "input" | "dictation" | "languages" => Tab::Dictation,
            "cleanup" => Tab::Cleanup,
            "meetings" => Tab::Meetings,
            "privacy" => Tab::Privacy,
            "advanced" => Tab::Advanced,
            _ => Tab::General,
        };
        cx.notify();
    }

    fn load_mics(&mut self, cx: &mut Context<Self>) {
        if self.pcx.services.is_sandbox() {
            self.mics = Some(vec![
                "Built-in Microphone (example)".into(),
                "USB Headset (example)".into(),
            ]);
            return;
        }
        cx.spawn(async move |this, cx| {
            let devices = cx
                .background_executor()
                .spawn(async { lf_audio::list_input_devices() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.mics = Some(devices);
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Push the config into every control. Unlisted stored values are shown
    /// as an extra "Current: …" option, never replaced by a default.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let config = self.pcx.services.config();
        for (i, spec) in SPECS.iter().enumerate() {
            let current = spec.current(&config);
            if let Some((select, values)) = self.selects.get_mut(&i) {
                let (mut labels, mut vals): (Vec<SharedString>, Vec<Value>) = match spec.control {
                    Control::Choice(choices) => choices
                        .iter()
                        .map(|(lit, label)| (SharedString::from(*label), lit.to_value()))
                        .unzip(),
                    Control::Microphone => {
                        let mut l = vec![SharedString::from("System default")];
                        let mut v = vec![json!("")];
                        for m in self.mics.clone().unwrap_or_default() {
                            l.push(m.clone().into());
                            v.push(json!(m));
                        }
                        (l, v)
                    }
                    _ => (Vec::new(), Vec::new()),
                };
                if !vals.contains(&current) {
                    let shown = match &current {
                        Value::String(s) => s.clone(),
                        Value::Null => "Not set".into(),
                        other => other.to_string(),
                    };
                    let suffix = if spec.control == Control::Microphone {
                        " (not connected)"
                    } else {
                        ""
                    };
                    labels.push(format!("Current: {shown}{suffix}").into());
                    vals.push(current.clone());
                }
                let selected = vals.iter().position(|v| *v == current);
                *values = vals;
                select.update(cx, |s, cx| {
                    s.set_options(labels, selected, cx);
                    s.set_disabled(spec.unavailable.is_some(), cx);
                });
            }
            if let Some(input) = self.inputs.get(&i) {
                let text = current.as_str().unwrap_or("").to_string();
                let shown = input.read(cx).text().to_string();
                let untouched = self.synced.get(&i).is_none_or(|last| *last == shown);
                if untouched && !self.errors.contains_key(&i) {
                    input.update(cx, |inp, cx| inp.set_text(text.clone(), cx));
                    self.synced.insert(i, text);
                }
            }
        }
    }

    fn save(&mut self, i: usize, value: Value, cx: &mut Context<Self>) {
        let spec = &SPECS[i];
        match ctl::save(&self.pcx.services.config_file, spec, value) {
            Ok(saved) => {
                self.pcx.services.set_cached_config(saved);
                self.note_config(cx);
                self.errors.remove(&i);
                self.synced.remove(&i);
                if spec.restart && !self.restart.contains(&i) {
                    self.restart.push(i);
                }
                self.pcx.toast(format!("{} saved", spec.label), cx);
            }
            Err(err) => {
                self.errors.insert(i, ctl::error_text(&err));
            }
        }
        self.pcx.config_changed(cx);
        self.sync(cx);
        cx.notify();
    }

    fn choose(&mut self, i: usize, choice: usize, cx: &mut Context<Self>) {
        let Some(value) = self
            .selects
            .get(&i)
            .and_then(|(_, v)| v.get(choice).cloned())
        else {
            return;
        };
        self.save(i, value, cx);
    }

    fn save_text(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(input) = self.inputs.get(&i) else {
            return;
        };
        let text = input.read(cx).text().to_string();
        let current = SPECS[i].current(&self.pcx.services.config());
        if current.as_str() == Some(text.trim()) {
            return;
        }
        self.save(i, json!(text), cx);
    }

    fn render_spec(&self, spec: &'static Spec, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let i = spec_index(spec);
        let config = self.pcx.services.config();
        let current = spec.current(&config);
        let control = match spec.control {
            Control::Switch => {
                let on = current.as_bool().unwrap_or(false);
                let this = cx.entity().downgrade();
                Switch::new(("setting-switch", i as u64), on, spec.label)
                    .disabled(spec.unavailable.is_some())
                    .on_toggle(move |on, _, cx| {
                        let _ = this.update(cx, |p, cx| p.save(i, json!(on), cx));
                    })
                    .into_any_element()
            }
            Control::Choice(_) | Control::Microphone => self
                .selects
                .get(&i)
                .map(|(s, _)| s.clone().into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            Control::Shortcut { .. } | Control::Text { .. } => {
                let input = self.inputs.get(&i).cloned();
                let keys = matches!(spec.control, Control::Shortcut { .. })
                    .then(|| ui::keys(&t, &lf_ui::hub_model::format_hotkey(current.as_str())));
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .children(keys)
                    .child(div().w(gpui::px(220.)).children(input))
                    .child(
                        Button::new(("setting-save", i as u64), "Save")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| this.save_text(i, cx))),
                    )
                    .into_any_element()
            }
        };
        let note = if let Some(err) = self.errors.get(&i) {
            Some((Tone::Error, SharedString::from(err.clone())))
        } else if let Some(reason) = spec.unavailable {
            Some((Tone::Neutral, reason.into()))
        } else if self.restart.contains(&i) {
            Some((Tone::Warning, "Restart LocalFlow to apply.".into()))
        } else if spec.restart {
            Some((
                Tone::Neutral,
                "Takes effect after LocalFlow restarts.".into(),
            ))
        } else {
            None
        };
        ui::setting_row(&t, spec.label, spec.description, control, note)
    }

    fn path_row(
        &self,
        label: &str,
        path: PathBuf,
        folder: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        let shown = path.display().to_string();
        let copy = shown.clone();
        let open_target = if folder {
            path.clone()
        } else {
            path.parent().map(PathBuf::from).unwrap_or(path.clone())
        };
        let exists = open_target.exists();
        let pcx = self.pcx.clone();
        ui::setting_row(
            &t,
            label.to_string(),
            shown,
            div()
                .flex()
                .gap(space::s4())
                .child(
                    Button::new(SharedString::from(format!("open-{label}")), "Open folder")
                        .small()
                        .icon("folder-open")
                        .disabled(!exists)
                        .on_click(move |_, _, cx| cx.open_with_system(&open_target)),
                )
                .child(
                    Button::new(SharedString::from(format!("copy-{label}")), "Copy path")
                        .small()
                        .ghost()
                        .icon("copy")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            pcx.toast("Path copied", cx);
                        }),
                )
                .into_any_element(),
            None,
        )
    }

    /// Appearance: System / Light / Dark, stored apart from the config file.
    fn appearance_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        let this = cx.entity().downgrade();
        let control = ui::Segmented::new(
            "appearance-mode",
            AppearanceMode::ALL.iter().map(|m| m.label()),
            Some(t.mode.index()),
        )
        .on_select(move |i, _, cx| {
            if let Some(mode) = AppearanceMode::ALL.get(i).copied() {
                let _ = this.update(cx, |p, cx| p.set_appearance(mode, cx));
            }
        });
        ui::setting_row(
            &t,
            "Appearance",
            "System follows your computer's light or dark setting.",
            control.into_any_element(),
            self.appearance_error
                .clone()
                .map(|e| (Tone::Error, SharedString::from(e))),
        )
    }

    /// Apply the choice to every window now, then store it on the
    /// appearance writer (never on this thread). A failed save is shown on
    /// the row; the choice still applies for this session.
    fn set_appearance(&mut self, mode: AppearanceMode, cx: &mut Context<Self>) {
        let queue = appearance::queue();
        if !queue.needs_save(mode, theme(cx).mode) {
            return;
        }
        crate::theme::apply_mode(mode, cx);
        // Registered before queueing; the writer drops it once on disk.
        let write = Box::new(lf_ui::shutdown::register_pending_write());
        let receiver = queue.submit(&self.pcx.services.root, mode, write);
        self.appearance_error = queue.error();
        cx.notify();
        let Ok(receiver) = receiver else {
            return;
        };
        // Awaited here, so no background thread waits while the disk stalls.
        cx.spawn(async move |this, cx| {
            let _ = receiver.recv().await;
            // The writer records only the latest choice's outcome.
            let _ = this.update(cx, |this, cx| {
                this.appearance_error = appearance::queue().error();
                cx.notify();
            });
        })
        .detach();
    }

    fn render_extra(&self, cx: &mut Context<Self>) -> Extra {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let root = &services.root;
        let config = services.config();
        let mut out = Extra {
            top: Vec::new(),
            rows: Vec::new(),
            sections: Vec::new(),
        };
        match self.tab {
            Tab::General => {
                out.rows.push(ui::setting_row(
                    &t,
                    "Setup",
                    "Pick and download a speech model, test your microphone, choose cleanup and a shortcut. Nothing changes until you apply.",
                    {
                        let pcx = self.pcx.clone();
                        Button::new("setup-again", "Run setup again")
                            .small()
                            .on_click(move |_, _, cx| pcx.open_setup(cx))
                            .into_any_element()
                    },
                    None,
                ).into_any_element());
                out.rows.push(
                    ui::setting_row(
                        &t,
                        "Launch at login",
                        launch_at_login_description(std::env::consts::OS),
                        ui::token(
                            &t,
                            Tone::Neutral,
                            if cfg!(target_os = "macos") {
                                "CLI"
                            } else {
                                "Unavailable"
                            },
                        )
                        .into_any_element(),
                        None,
                    )
                    .into_any_element(),
                );
            }
            Tab::Shortcuts => {
                out.top.push(ui::banner(
                    &t,
                    Tone::Info,
                    format!(
                        "Shortcuts are saved to your config file. {} Use names like alt_r, ctrl_l, or chords like <ctrl>+<alt>+1.",
                        crate::controllers::status::shortcut_note(cfg!(target_os = "macos"))
                    ),
                    None,
                ).into_any_element());
            }
            Tab::Privacy => {
                let this = cx.entity().downgrade();
                let pcx = self.pcx.clone();
                out.rows.push(
                    ui::setting_row(
                        &t,
                        "Clear history",
                        "Delete every saved dictation and audio clip on this device.",
                        Button::new("settings-clear", "Clear history…")
                            .destructive()
                            .small()
                            .on_click(move |_, _, cx| {
                                let this = this.clone();
                                pcx.confirm(
                                    Confirm {
                                        title: "Clear all history?".into(),
                                        body: lf_ui::hub_model::HISTORY_CLEAR_CONFIRM.into(),
                                        verb: "Clear history".into(),
                                        destructive: true,
                                        on_confirm: Rc::new(move |_, cx| {
                                            let _ = this.update(cx, |p, cx| {
                                                match history::clear(&p.pcx.services.root) {
                                                    Ok(n) => p.pcx.toast(
                                                        format!("Cleared {n} dictations"),
                                                        cx,
                                                    ),
                                                    Err(e) => p.pcx.toast(
                                                        format!("Couldn't clear: {e:#}"),
                                                        cx,
                                                    ),
                                                }
                                            });
                                        }),
                                    },
                                    cx,
                                );
                            })
                            .into_any_element(),
                        None,
                    )
                    .into_any_element(),
                );
                out.sections.push(ui::section(&t, "What leaves this device", None, None)
                    .child(
                        ui::group(&t).child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(space::s2())
                                .px(ui::row_inset())
                                .py(space::s5())
                                .child(ui::secondary(&t, data_leaves_text(&config)))
                                .child(ui::supporting(&t, "Speech is transcribed locally. API keys stay in your system keyring and are never shown here.")),
                        ),
                    )
                    .into_any_element());
                let paths = vec![
                    self.path_row("Config file", root.config_path.clone(), false, cx)
                        .into_any_element(),
                    self.path_row("History", root.history_path.clone(), false, cx)
                        .into_any_element(),
                    self.path_row("Audio clips", root.clips_dir.clone(), true, cx)
                        .into_any_element(),
                    self.path_row("Dictionary", root.dictionary_path.clone(), false, cx)
                        .into_any_element(),
                    self.path_row("Snippets", root.hot_phrases_path.clone(), false, cx)
                        .into_any_element(),
                ];
                out.sections.push(
                    ui::section(&t, "Where your data lives", None, None)
                        .child(ui::group_rows(&t, paths, ui::row_inset()))
                        .into_any_element(),
                );
            }
            Tab::Advanced => {
                let health = self.pcx.health(cx);
                let (tone, word) = match health {
                    Health::Online => (Tone::Success, "Running"),
                    Health::Simulated => (Tone::Warning, "Sandbox (simulated)"),
                    Health::Offline => (Tone::Warning, "Not running"),
                    Health::Unknown => (Tone::Neutral, "Checking…"),
                };
                let pcx = self.pcx.clone();
                out.rows.push(
                    ui::setting_row(
                        &t,
                        "Local service",
                        format!(
                            "{} · [server] host and port",
                            services.remote.base_url(&config)
                        ),
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::status_dot(&t, tone, word))
                            .child(
                                Button::new("recheck", "Check again")
                                    .small()
                                    .icon("refresh-cw")
                                    .on_click(move |_, _, cx| pcx.recheck_health(cx)),
                            )
                            .into_any_element(),
                        None,
                    )
                    .into_any_element(),
                );
                out.rows.push(
                    self.path_row("Config file", root.config_path.clone(), false, cx)
                        .into_any_element(),
                );
                if services.is_sandbox() {
                    out.rows.push(
                        ui::setting_row(
                            &t,
                            "Simulated runtime state",
                            "Sandbox only: step Home and the sidebar through every dictation state for review.",
                            self.simulated.clone().into_any_element(),
                            Some((Tone::Warning, "Simulated".into())),
                        )
                        .into_any_element(),
                    );
                }
                let keys = ui::section(
                    &t,
                    "Current settings",
                    Some("Read-only view of what LocalFlow will use.".into()),
                    None,
                );
                let mut lines = Vec::new();
                let dict = lf_core::config::config_to_dict(&config);
                if let Some(obj) = dict.as_object() {
                    for (section, values) in obj {
                        if let Some(values) = values.as_object() {
                            for (key, value) in values {
                                if key.contains("api_key") {
                                    continue;
                                }
                                lines.push(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap(space::s6())
                                        .px(ui::row_inset())
                                        .py(space::s2())
                                        .child(ui::mono(&t, format!("{section}.{key}")))
                                        .child(
                                            ui::mono(&t, value.to_string())
                                                .text_color(t.text_secondary)
                                                .overflow_hidden()
                                                .text_ellipsis(),
                                        )
                                        .into_any_element(),
                                );
                            }
                        }
                    }
                }
                out.sections.push(
                    keys.when(!lines.is_empty(), |d| {
                        d.child(ui::group_rows(&t, lines, ui::row_inset()))
                    })
                    .into_any_element(),
                );
            }
            _ => {}
        }
        out
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let this = cx.entity().downgrade();
        let tab_items: Vec<(Tab, &'static str)> =
            Tab::ALL.iter().map(|t| (*t, t.label())).collect();
        let tabs = ui::tabs(
            &t,
            "settings-tab",
            &tab_items,
            self.tab,
            ui::TabDensity::Compact,
            Rc::new(move |tab, _, cx| {
                let _ = this.update(cx, |p, cx| {
                    p.tab = tab;
                    cx.notify();
                });
            }),
        );
        let extra = self.render_extra(cx);
        // One group per tab: Appearance (General), the `[section] key` rows,
        // then the tab's extra rows; standalone sections follow it.
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if self.tab == Tab::General {
            rows.push(self.appearance_row(cx).into_any_element());
        }
        for spec in ctl::specs_for(self.tab) {
            rows.push(self.render_spec(spec, cx).into_any_element());
        }
        if self.tab == Tab::Cleanup {
            let pcx = self.pcx.clone();
            rows.push(
                ui::setting_row(
                    &t,
                    "Tone by app",
                    "Work, email, personal and other tones are set on the Style page.",
                    Button::new("to-style", "Open Style")
                        .small()
                        .on_click(move |_, _, cx| pcx.navigate(Page::Style, None, cx))
                        .into_any_element(),
                    None,
                )
                .into_any_element(),
            );
        }
        rows.extend(extra.rows);
        let mut tab_body = div().flex().flex_col().gap(space::s5()).children(extra.top);
        if !rows.is_empty() {
            tab_body = tab_body.child(ui::group_rows(&t, rows, ui::row_inset()));
        }
        let mut body = div()
            .flex()
            .flex_col()
            .gap(space::s8())
            .child(self.render_assistant(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s6())
                    .child(tabs)
                    .child(tab_body),
            );
        for el in extra.sections {
            body = body.child(el);
        }
        if let Some(err) = self.pcx.services.config_error.borrow().clone() {
            body = div()
                .child(ui::banner(
                    &t,
                    Tone::Error,
                    format!("Config file couldn't be read: {err}"),
                    None,
                ))
                .child(body);
        }
        page(
            header(
                &t,
                "Settings",
                Some("Changes save immediately to your LocalFlow config file.".into()),
                vec![],
            ),
            column("settings-scroll", body),
        )
    }
}

pub(crate) fn launch_at_login_description(os: &str) -> &'static str {
    match os {
        "macos" => "Managed with `lf agent install` / `lf agent uninstall`.",
        "windows" => "Not available on Windows yet. Start LocalFlow manually when you sign in.",
        _ => "Not available on Linux yet. Start LocalFlow from your session's autostart if you want it at login.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn launch_at_login_copy_names_the_right_platform() {
        let windows = launch_at_login_description("windows");
        assert!(windows.contains("Windows") && !windows.contains("Linux"));
        assert_eq!(
            launch_at_login_description("linux"),
            "Not available on Linux yet. Start LocalFlow from your session's autostart if you want it at login."
        );
        assert_eq!(
            launch_at_login_description("macos"),
            "Managed with `lf agent install` / `lf agent uninstall`."
        );
    }

    #[test]
    fn spec_keys_are_unique() {
        let mut seen = HashSet::new();
        for s in SPECS {
            assert!(
                seen.insert((s.section, s.key)),
                "duplicate {}.{}",
                s.section,
                s.key
            );
        }
    }

    #[test]
    fn spec_index_resolves_by_value_not_address() {
        for tab in Tab::ALL {
            for spec in ctl::specs_for(tab) {
                let i = spec_index(spec);
                assert_eq!(SPECS[i].section, spec.section);
                assert_eq!(SPECS[i].key, spec.key);
                // A copy lives at a different address but must resolve the same.
                let copy = Spec { ..*spec };
                assert!(!std::ptr::eq(&copy, spec));
                assert_eq!(spec_index(&copy), i);
            }
        }
    }

    /// `SettingsPage::new` creates `inputs` for Shortcut/Text specs and
    /// `selects` for Choice/Microphone specs, keyed by SPECS index. The index
    /// `spec_index` returns must land on a spec of the same kind (never a
    /// fallback `0`), so the lookups in `render_spec` find the entity.
    #[test]
    fn spec_index_lands_on_the_matching_control_kind() {
        for tab in Tab::ALL {
            for spec in ctl::specs_for(tab) {
                let i = spec_index(spec);
                let (name, ok) = match spec.control {
                    Control::Shortcut { .. } | Control::Text { .. } => (
                        "input",
                        matches!(
                            SPECS[i].control,
                            Control::Shortcut { .. } | Control::Text { .. }
                        ),
                    ),
                    Control::Choice(_) | Control::Microphone => (
                        "select",
                        matches!(SPECS[i].control, Control::Choice(_) | Control::Microphone),
                    ),
                    Control::Switch => ("switch", SPECS[i].control == Control::Switch),
                };
                assert!(ok, "{}.{} has no {name} entry", spec.section, spec.key);
            }
        }
    }
}
