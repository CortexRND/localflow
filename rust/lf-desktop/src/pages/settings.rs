//! Settings: every manual row is a real `[section] key` from
//! `controllers::settings::SPECS`, saved on change with validation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{div, prelude::*, ClipboardItem, Context, Entity, SharedString, Window};
use serde_json::{json, Value};

use crate::app::{Confirm, PageCx};
use crate::controllers::history;
use crate::controllers::models::data_leaves_text;
use crate::controllers::settings::{self as ctl, Control, Spec, Tab, SPECS};
use crate::page::Page;
use crate::pages::{column, header, page};
use crate::remote::Health;
use crate::theme::{space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button, Switch};

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
}

fn spec_index(spec: &Spec) -> usize {
    SPECS
        .iter()
        .position(|s| std::ptr::eq(s, spec))
        .unwrap_or(0)
}

impl SettingsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let mut page = SettingsPage {
            pcx,
            tab: Tab::General,
            selects: HashMap::new(),
            inputs: HashMap::new(),
            errors: HashMap::new(),
            restart: Vec::new(),
            mics: None,
            synced: HashMap::new(),
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
        page.sync(cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.sync(cx);
        cx.notify();
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

    fn render_extra(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let t = theme(cx).clone();
        let services = &self.pcx.services;
        let root = &services.root;
        let config = services.config();
        let mut out = Vec::new();
        match self.tab {
            Tab::General => {
                out.push(ui::setting_row(
                    &t,
                    "Setup",
                    "Download speech models and test your microphone with `lf setup` in a terminal. The checklist on Home shows what's ready.",
                    {
                        let pcx = self.pcx.clone();
                        Button::new("setup-home", "Setup steps")
                            .small()
                            .on_click(move |_, _, cx| pcx.navigate(Page::Home, None, cx))
                            .into_any_element()
                    },
                    None,
                ).into_any_element());
                out.push(ui::setting_row(
                    &t,
                    "Launch at login",
                    if cfg!(target_os = "macos") {
                        "Managed with `lf agent install` / `lf agent uninstall`."
                    } else {
                        "Not available on Linux yet. Start LocalFlow from your session's autostart if you want it at login."
                    },
                    ui::token(&t, Tone::Neutral, if cfg!(target_os = "macos") { "CLI" } else { "Unavailable" }).into_any_element(),
                    None,
                ).into_any_element());
            }
            Tab::Shortcuts => {
                out.insert(0, ui::banner(
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
                out.push(
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
                out.push(ui::section(&t, "What leaves this device", None, None)
                    .child(ui::secondary(&t, data_leaves_text(&config)))
                    .child(ui::supporting(&t, "Speech is transcribed locally. API keys stay in your system keyring and are never shown here."))
                    .into_any_element());
                let mut paths = ui::section(&t, "Where your data lives", None, None);
                paths = paths
                    .child(self.path_row("Config file", root.config_path.clone(), false, cx))
                    .child(self.path_row("History", root.history_path.clone(), false, cx))
                    .child(self.path_row("Audio clips", root.clips_dir.clone(), true, cx))
                    .child(self.path_row("Dictionary", root.dictionary_path.clone(), false, cx))
                    .child(self.path_row("Snippets", root.hot_phrases_path.clone(), false, cx));
                out.push(paths.into_any_element());
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
                out.push(
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
                out.push(
                    self.path_row("Config file", root.config_path.clone(), false, cx)
                        .into_any_element(),
                );
                let mut keys = ui::section(
                    &t,
                    "Current settings",
                    Some("Read-only view of what LocalFlow will use.".into()),
                    None,
                );
                let dict = lf_core::config::config_to_dict(&config);
                if let Some(obj) = dict.as_object() {
                    for (section, values) in obj {
                        if let Some(values) = values.as_object() {
                            for (key, value) in values {
                                if key.contains("api_key") {
                                    continue;
                                }
                                keys = keys.child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .gap(space::s6())
                                        .py(space::s2())
                                        .border_b_1()
                                        .border_color(t.border)
                                        .child(ui::mono(&t, format!("{section}.{key}")))
                                        .child(
                                            ui::mono(&t, value.to_string())
                                                .text_color(t.text_secondary)
                                                .overflow_hidden()
                                                .text_ellipsis(),
                                        ),
                                );
                            }
                        }
                    }
                }
                out.push(keys.into_any_element());
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
            Rc::new(move |tab, _, cx| {
                let _ = this.update(cx, |p, cx| {
                    p.tab = tab;
                    cx.notify();
                });
            }),
        );
        let mut rows = div().flex().flex_col();
        let extra = self.render_extra(cx);
        let (top, bottom): (Vec<_>, Vec<_>) = if self.tab == Tab::Shortcuts {
            let mut e = extra.into_iter();
            (e.next().into_iter().collect(), e.collect())
        } else {
            (Vec::new(), extra)
        };
        for el in top {
            rows = rows.child(div().pb(space::s5()).child(el));
        }
        for spec in ctl::specs_for(self.tab) {
            rows = rows.child(self.render_spec(spec, cx));
        }
        if self.tab == Tab::Cleanup {
            let pcx = self.pcx.clone();
            rows = rows.child(ui::setting_row(
                &t,
                "Tone by app",
                "Work, email, personal and other tones are set on the Style page.",
                Button::new("to-style", "Open Style")
                    .small()
                    .on_click(move |_, _, cx| pcx.navigate(Page::Style, None, cx))
                    .into_any_element(),
                None,
            ));
        }
        let mut body = div().child(tabs).child(rows);
        for el in bottom {
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
