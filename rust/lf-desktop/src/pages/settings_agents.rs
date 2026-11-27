//! Settings → Agents: the Agents switch, the providers (status, Check, the
//! inline key editor), the defaults and the workspaces.
//!
//! Every row is service-backed (`agents-api v1` plus `PUT /api/config`
//! through [`AgentsBackend`]), never the page's config-file write: the
//! shell's gate reads the *service's* status, and the sandbox fake has no
//! config file. One write at a time; every reply is generation-checked
//! before it is applied; nothing here polls (it reads when the tab is
//! shown, on refresh, after each write, and on window activation while a
//! provider isn't connected). Decisions and copy live in
//! [`crate::controllers::agents_settings`]; nothing branches on a
//! provider's id.
//!
//! A key is read from the masked field, the field is cleared at once, and
//! the value moves into the worker as a [`SecretValue`]. It never enters
//! this entity's state, a toast, an error or a log line.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, Context, Entity, FocusHandle, PathPromptOptions, SharedString, Window,
};

use crate::app::{Confirm, PageCx};
use crate::controllers::agents::{AgentsBackend, AgentsError, Provider, SecretSlot};
use crate::controllers::agents_settings::{self as ctl, AgentSettings, SecretValue};
use crate::controllers::agents_triggers::Rule;
use crate::theme::{space, theme, Theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button, Switch};

#[path = "settings_agents_triggers.rs"]
mod triggers;

pub(crate) use triggers::Triggers;

/// The provider tile's edge in a row.
const TILE: f32 = 28.;

/// A row a write (or its error) belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Row {
    Enabled,
    /// `(id, profile)`.
    Provider(String, String),
    DefaultProvider,
    DefaultRepo,
    Workspace(String),
    AddWorkspace,
    /// The Voice triggers switch (`agents.hot_commands`).
    HotCommands,
    /// One trigger's switch (by rule id).
    Rule(String),
    /// The open trigger editor's Save or Delete.
    RuleEditor,
    /// Try it.
    Test,
}

/// The key editor's target: a provider and one of its secrets. Never the
/// value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyTarget {
    pub id: String,
    pub profile: String,
    pub name: String,
}

impl KeyTarget {
    fn row(&self) -> Row {
        Row::Provider(self.id.clone(), self.profile.clone())
    }
}

/// The last good answer.
struct Loaded {
    settings: AgentSettings,
    providers: Vec<Provider>,
    /// Voice triggers.
    rules: Vec<Rule>,
}

/// Focus moved on the next frame (after the element exists).
enum PendingFocus {
    KeyField,
    Opener,
    /// The trigger editor's Say field.
    SayField,
    /// The Edit (or Add trigger) button that opened the trigger editor.
    RuleOpener,
}

pub struct AgentsSettings {
    pcx: PageCx,
    backend: Arc<dyn AgentsBackend>,
    loaded: Option<Loaded>,
    /// The last read failed: the service isn't answering. Last-known rows
    /// stay visible; every control is disabled.
    load_error: Option<AgentsError>,
    generation: u64,
    /// The one write in flight (generation, row).
    op: Option<(u64, Row)>,
    op_generation: u64,
    /// The Agents switch's value while its write is in flight.
    pending_enabled: Option<bool>,
    errors: HashMap<Row, String>,
    /// The open key editor and its instance (a later reply only closes the
    /// instance it was started from).
    editor: Option<(KeyTarget, u64)>,
    editor_generation: u64,
    key_error: Option<String>,
    key_input: Entity<TextInput>,
    /// The button that opened the editor last; focus returns to it.
    opener: Option<KeyTarget>,
    opener_focus: FocusHandle,
    pending_focus: Option<PendingFocus>,
    provider_select: Entity<Select>,
    provider_values: Vec<String>,
    repo_select: Entity<Select>,
    repo_values: Vec<String>,
    name_input: Entity<TextInput>,
    folder_input: Entity<TextInput>,
    /// Fixture scene `agents-settings-key`: open the first cloud provider's
    /// missing key once providers are read.
    fixture_key: bool,
    /// The Voice triggers section's controls and state.
    triggers: Triggers,
}

impl AgentsSettings {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let backend = pcx.services.remote.agents(&pcx.services.config());
        let provider_select = cx.new(|cx| Select::new(cx, Vec::new(), None).width(240.));
        cx.subscribe(&provider_select, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            this.choose_provider(*i, cx);
        })
        .detach();
        let repo_select = cx.new(|cx| Select::new(cx, Vec::new(), None).width(240.));
        cx.subscribe(&repo_select, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            this.choose_repo(*i, cx);
        })
        .detach();
        let name_input = cx.new(|cx| {
            TextInput::new(cx, ctl::NAME_PLACEHOLDER)
                .max_chars(40)
                .label(ctl::NAME_LABEL)
        });
        let folder_input = cx.new(|cx| {
            TextInput::new(cx, ctl::FOLDER_PLACEHOLDER)
                .max_chars(4096)
                .label(ctl::FOLDER_LABEL)
        });
        for input in [&name_input, &folder_input] {
            cx.subscribe(input, |this, _, event, cx| match event {
                InputEvent::Submit => this.add_workspace(cx),
                InputEvent::Changed => {
                    if this.errors.remove(&Row::AddWorkspace).is_some() {
                        cx.notify();
                    }
                }
            })
            .detach();
        }
        let key_input = Self::new_key_input(KEY_LABEL_DEFAULT.into(), cx);
        let triggers = Triggers::new(cx);
        AgentsSettings {
            pcx,
            backend,
            loaded: None,
            load_error: None,
            generation: 0,
            op: None,
            op_generation: 0,
            pending_enabled: None,
            errors: HashMap::new(),
            editor: None,
            editor_generation: 0,
            key_error: None,
            key_input,
            opener: None,
            opener_focus: cx.focus_handle().tab_stop(true),
            pending_focus: None,
            provider_select,
            provider_values: Vec::new(),
            repo_select,
            repo_values: Vec::new(),
            name_input,
            folder_input,
            fixture_key: false,
            triggers,
        }
    }

    /// A fresh masked field (one per editor opened: no earlier text, undo
    /// or label can carry over).
    fn new_key_input(label: SharedString, cx: &mut Context<Self>) -> Entity<TextInput> {
        let input = cx.new(|cx| {
            TextInput::new(cx, ctl::KEY_PLACEHOLDER)
                .masked()
                .label(label)
        });
        cx.subscribe(&input, |this, _, event, cx| match event {
            InputEvent::Submit => this.save_key(cx),
            InputEvent::Changed => cx.notify(),
        })
        .detach();
        input
    }

    // ------------------------------------------------------------ loading ---

    /// The Settings column this tab is drawn in (to bring a section into
    /// view).
    pub fn with_page_scroll(mut self, scroll: gpui::ScrollHandle) -> Self {
        self.triggers.page_scroll = Some(scroll);
        self
    }

    /// The tab came on screen (or anything changed): read again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.backend = self.pcx.services.remote.agents(&self.pcx.services.config());
        self.load(cx);
    }

    /// The window came back to the front: read again only while some
    /// provider isn't connected (a key or sign-in may have been fixed
    /// elsewhere), or nothing has been read yet. No polling otherwise.
    pub fn window_activated(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .loaded
            .as_ref()
            .is_none_or(|l| ctl::reload_on_activation(&l.providers));
        if stale || self.load_error.is_some() {
            self.load(cx);
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let settings = backend.settings()?;
                    let providers = backend.providers()?;
                    let rules = backend.rules()?;
                    Ok::<_, AgentsError>((settings, providers, rules))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.loaded(generation, result, cx));
        })
        .detach();
    }

    fn loaded(
        &mut self,
        generation: u64,
        result: Result<(AgentSettings, Vec<Provider>, Vec<Rule>), AgentsError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok((settings, providers, rules)) => {
                self.loaded = Some(Loaded {
                    settings,
                    providers,
                    rules,
                });
                self.load_error = None;
            }
            // The last answer stays (rows visible, controls disabled).
            Err(error) => self.load_error = Some(error),
        }
        self.sync(cx);
        if self.fixture_key {
            self.open_fixture_key(cx);
        }
        self.triggers_loaded(cx);
        cx.notify();
    }

    /// Push the stored defaults into the two selects (also snapping a
    /// refused choice back).
    fn sync(&mut self, cx: &mut Context<Self>) {
        let disabled = !self.writable();
        let Some(loaded) = &self.loaded else {
            for select in [&self.provider_select, &self.repo_select] {
                select.update(cx, |s, cx| s.set_disabled(true, cx));
            }
            return;
        };
        let providers =
            ctl::default_provider_choices(&loaded.providers, &loaded.settings.default_provider);
        let repos = ctl::default_repo_choices(&loaded.settings);
        self.provider_values = providers.values.clone();
        self.repo_values = repos.values.clone();
        for (select, choices) in [
            (&self.provider_select, providers),
            (&self.repo_select, repos),
        ] {
            let empty = choices.labels.is_empty();
            select.update(cx, |s, cx| {
                s.set_options(
                    choices.labels.into_iter().map(SharedString::from).collect(),
                    choices.selected,
                    cx,
                );
                s.set_disabled(disabled || empty, cx);
            });
        }
    }

    /// Controls can write: the service answers and nothing else is being
    /// written.
    fn writable(&self) -> bool {
        self.load_error.is_none() && self.loaded.is_some() && self.op.is_none()
    }

    // ------------------------------------------------------------- writes ---

    /// Start the one write for `row`; `None` while another runs.
    fn begin(&mut self, row: Row, cx: &mut Context<Self>) -> Option<u64> {
        if self.op.is_some() {
            return None;
        }
        self.op_generation = self.op_generation.wrapping_add(1);
        self.errors.remove(&row);
        self.op = Some((self.op_generation, row));
        self.sync(cx);
        Some(self.op_generation)
    }

    /// The write `generation` finished; `true` when it was the running one.
    fn finish(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if self.op.as_ref().is_some_and(|(g, _)| *g == generation) {
            self.op = None;
            self.sync(cx);
            true
        } else {
            false
        }
    }

    fn busy_row(&self, row: &Row) -> bool {
        self.op.as_ref().is_some_and(|(_, r)| r == row)
    }

    /// `PUT /api/config` with `patch` for `row`. Success re-reads and asks
    /// the shell to re-probe the gate; a refusal shows the service's detail
    /// on the row and the control snaps back to the stored value.
    fn write_config(
        &mut self,
        row: Row,
        patch: serde_json::Value,
        then: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.begin(row.clone(), cx) else {
            return;
        };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.set_config(&patch) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                this.pending_enabled = None;
                this.triggers.pending_hot = None;
                match result {
                    Ok(()) => {
                        then(this, cx);
                        this.pcx.agents_changed(cx);
                        this.pcx.config_changed(cx);
                    }
                    Err(error) => {
                        this.errors.insert(row, ctl::write_error(&error));
                    }
                }
                this.load(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn set_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        if !self.writable() {
            return;
        }
        self.pending_enabled = Some(on);
        self.write_config(Row::Enabled, ctl::enabled_patch(on), |_, _| {}, cx);
    }

    fn choose_provider(&mut self, choice: usize, cx: &mut Context<Self>) {
        let (Some(value), Some(loaded)) = (self.provider_values.get(choice), &self.loaded) else {
            return;
        };
        if *value == loaded.settings.default_provider {
            return;
        }
        let patch = ctl::default_provider_patch(value);
        self.write_config(Row::DefaultProvider, patch, |_, _| {}, cx);
    }

    fn choose_repo(&mut self, choice: usize, cx: &mut Context<Self>) {
        let (Some(value), Some(loaded)) = (self.repo_values.get(choice), &self.loaded) else {
            return;
        };
        if *value == loaded.settings.default_repo {
            return;
        }
        let patch = ctl::default_repo_patch(value);
        self.write_config(Row::DefaultRepo, patch, |_, _| {}, cx);
    }

    pub(crate) fn add_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        if !self.writable() {
            return;
        }
        let name = self.name_input.read(cx).text().to_string();
        let folder = self.folder_input.read(cx).text().to_string();
        match ctl::validate_workspace(&name, &folder, &loaded.settings.repos) {
            Err(message) => {
                self.errors.insert(Row::AddWorkspace, message);
                cx.notify();
            }
            Ok((alias, path)) => {
                let patch = ctl::repos_add_patch(&loaded.settings, &alias, &path);
                self.write_config(
                    Row::AddWorkspace,
                    patch,
                    |this, cx| {
                        this.name_input.update(cx, |i, cx| i.clear(cx));
                        this.folder_input.update(cx, |i, cx| i.clear(cx));
                    },
                    cx,
                );
            }
        }
    }

    /// Remove… asks first (destructive; focus starts on Cancel).
    pub(crate) fn confirm_remove_workspace(&mut self, alias: String, cx: &mut Context<Self>) {
        let (title, body, verb) = ctl::remove_workspace_confirm(&alias);
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: title.into(),
                body: body.into(),
                verb: verb.into(),
                destructive: true,
                on_confirm: Rc::new(move |_, cx| {
                    let alias = alias.clone();
                    let _ = this.update(cx, |p, cx| p.remove_workspace(alias, cx));
                }),
            },
            cx,
        );
    }

    /// The whole new list in one `PUT`; the default goes with it when it
    /// was this one.
    fn remove_workspace(&mut self, alias: String, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let patch = ctl::repos_remove_patch(&loaded.settings, &alias);
        self.write_config(Row::Workspace(alias), patch, |_, _| {}, cx);
    }

    /// Choose…: a folder picker. Fills Folder and, while Name is empty,
    /// Name from the folder's name.
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(ctl::CHOOSE.trim_end_matches('…').into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let name = ctl::name_from_folder(&path);
                this.folder_input
                    .update(cx, |i, cx| i.set_text(path.display().to_string(), cx));
                if this.name_input.read(cx).text().trim().is_empty() {
                    this.name_input.update(cx, |i, cx| i.set_text(name, cx));
                }
                this.errors.remove(&Row::AddWorkspace);
                cx.notify();
            });
        })
        .detach();
    }

    // ---------------------------------------------------------- providers ---

    fn provider(&self, id: &str, profile: &str) -> Option<&Provider> {
        self.loaded
            .as_ref()?
            .providers
            .iter()
            .find(|p| p.id == id && p.profile == profile)
    }

    fn replace_provider(&mut self, provider: Provider) {
        if let Some(loaded) = &mut self.loaded {
            if let Some(slot) = loaded
                .providers
                .iter_mut()
                .find(|p| p.id == provider.id && p.profile == provider.profile)
            {
                *slot = provider;
            }
        }
    }

    fn name_of(&self, provider: &Provider) -> String {
        let all = self
            .loaded
            .as_ref()
            .map(|l| l.providers.as_slice())
            .unwrap_or_default();
        ctl::provider_title(provider, all)
    }

    /// Check: a deep check; its answer replaces the row.
    pub(crate) fn check(&mut self, id: String, profile: String, cx: &mut Context<Self>) {
        if self.load_error.is_some() {
            return;
        }
        let row = Row::Provider(id.clone(), profile.clone());
        let Some(generation) = self.begin(row.clone(), cx) else {
            return;
        };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.check_provider(&id, &profile) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                match result {
                    Ok(provider) => this.replace_provider(provider),
                    Err(error) => {
                        this.errors.insert(row, ctl::write_error(&error));
                    }
                }
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Add key… / Replace key…: the editor opens under the row (one at a
    /// time) with a fresh, empty masked field, focused.
    pub(crate) fn open_key(
        &mut self,
        target: KeyTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_key_editor(target, cx);
        self.key_input.read(cx).focus(window);
    }

    fn open_key_editor(&mut self, target: KeyTarget, cx: &mut Context<Self>) {
        let Some(provider) = self.provider(&target.id, &target.profile).cloned() else {
            return;
        };
        let Some(slot) = provider.secrets.iter().find(|s| s.name == target.name) else {
            return;
        };
        let label = ctl::key_accessible_label(&provider, slot, &self.name_of(&provider));
        // One editor at a time: a trigger editor closes.
        self.triggers.editor = None;
        // The old field (and anything typed into it) goes with its entity.
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.key_input = Self::new_key_input(label.into(), cx);
        self.editor_generation = self.editor_generation.wrapping_add(1);
        self.editor = Some((target.clone(), self.editor_generation));
        self.opener = Some(target);
        self.key_error = None;
        cx.notify();
    }

    fn open_fixture_key(&mut self, cx: &mut Context<Self>) {
        let target = self.loaded.as_ref().and_then(|l| {
            l.providers.iter().find_map(|p| {
                let slot = p.secrets.iter().find(|s| s.required && !s.set)?;
                (p.caps.kind == crate::controllers::agents::Kind::Cloud).then(|| KeyTarget {
                    id: p.id.clone(),
                    profile: p.profile.clone(),
                    name: slot.name.clone(),
                })
            })
        });
        if let Some(target) = target {
            self.fixture_key = false;
            self.open_key_editor(target, cx);
            self.pending_focus = Some(PendingFocus::KeyField);
        }
    }

    /// The `agents-settings-key` scene: open the editor once read.
    pub fn run_key_fixture(&mut self, cx: &mut Context<Self>) {
        self.fixture_key = true;
        if self.loaded.is_some() {
            self.open_fixture_key(cx);
        }
    }

    /// Close the editor; its field is emptied (and dropped on next open).
    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.key_input.update(cx, |i, cx| i.clear(cx));
        if self.editor.take().is_some() {
            self.pending_focus = Some(PendingFocus::Opener);
        }
        self.key_error = None;
        cx.notify();
    }

    /// Escape: close the open editor first; `false` lets it pass on.
    pub fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editor.is_some() {
            self.close_editor(cx);
            true
        } else if self.triggers.editor.is_some() {
            self.cancel_rule(cx);
            true
        } else {
            false
        }
    }

    /// Leaving the tab or the page: no half-entered key survives.
    pub fn clear_secret(&mut self, cx: &mut Context<Self>) {
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.editor = None;
        self.key_error = None;
        cx.notify();
    }

    /// Save: read the field, clear it at once, move the value into the
    /// worker. On success the row is replaced, the editor closes, "Key
    /// saved" is shown and a deep check runs.
    pub(crate) fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some((target, instance)) = self.editor.clone() else {
            return;
        };
        if self.key_input.read(cx).text().is_empty() || self.load_error.is_some() {
            return;
        }
        let Some(generation) = self.begin(target.row(), cx) else {
            return;
        };
        let value = SecretValue::new(self.key_input.read(cx).text().to_string());
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.key_error = None;
        let backend = self.backend.clone();
        let (id, profile, name) = (
            target.id.clone(),
            target.profile.clone(),
            target.name.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.set_secret(&id, &profile, &name, &value) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                let same = this.editor.as_ref().is_some_and(|(_, i)| *i == instance);
                match result {
                    Ok(provider) => {
                        this.replace_provider(provider);
                        // Only the instance this save came from, and only
                        // while nothing new was typed into it.
                        if same && this.key_input.read(cx).text().is_empty() {
                            this.close_editor(cx);
                        }
                        this.pcx.toast(ctl::KEY_SAVED, cx);
                        this.check(target.id.clone(), target.profile.clone(), cx);
                    }
                    Err(error) => {
                        if same {
                            this.key_error = Some(ctl::key_error(&error));
                        } else {
                            this.errors.insert(target.row(), ctl::key_error(&error));
                        }
                    }
                }
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Remove key… asks first (destructive; focus starts on Cancel).
    pub(crate) fn confirm_remove_key(&mut self, target: KeyTarget, cx: &mut Context<Self>) {
        let Some(provider) = self.provider(&target.id, &target.profile).cloned() else {
            return;
        };
        let required = provider
            .secrets
            .iter()
            .any(|s| s.name == target.name && s.required);
        let (title, body, verb) = ctl::remove_key_confirm(&self.name_of(&provider), required);
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: title.into(),
                body: body.into(),
                verb: verb.into(),
                destructive: true,
                on_confirm: Rc::new(move |_, cx| {
                    let target = target.clone();
                    let _ = this.update(cx, |p, cx| p.remove_key(target, cx));
                }),
            },
            cx,
        );
    }

    fn remove_key(&mut self, target: KeyTarget, cx: &mut Context<Self>) {
        if self.load_error.is_some() {
            return;
        }
        let row = target.row();
        let Some(generation) = self.begin(row.clone(), cx) else {
            return;
        };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    backend.set_secret(
                        &target.id,
                        &target.profile,
                        &target.name,
                        &SecretValue::removal(),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                match result {
                    Ok(provider) => this.replace_provider(provider),
                    Err(error) => {
                        this.errors.insert(row, ctl::key_error(&error));
                    }
                }
                this.sync(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------- test access ---

    #[cfg(test)]
    pub(crate) fn settings(&self) -> Option<&AgentSettings> {
        self.loaded.as_ref().map(|l| &l.settings)
    }

    #[cfg(test)]
    pub(crate) fn providers(&self) -> &[Provider] {
        self.loaded
            .as_ref()
            .map(|l| l.providers.as_slice())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> Option<&KeyTarget> {
        self.editor.as_ref().map(|(t, _)| t)
    }

    #[cfg(test)]
    pub(crate) fn key_error(&self) -> Option<&str> {
        self.key_error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn error(&self, row: &Row) -> Option<&str> {
        self.errors.get(row).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn key_text<'a>(&self, cx: &'a gpui::App) -> &'a str {
        self.key_input.read(cx).text()
    }

    #[cfg(test)]
    pub(crate) fn type_key(&mut self, text: &str, cx: &mut Context<Self>) {
        self.key_input.update(cx, |i, cx| i.set_text(text, cx));
    }

    #[cfg(test)]
    pub(crate) fn type_workspace(&mut self, name: &str, folder: &str, cx: &mut Context<Self>) {
        self.name_input.update(cx, |i, cx| i.set_text(name, cx));
        self.folder_input.update(cx, |i, cx| i.set_text(folder, cx));
    }

    #[cfg(test)]
    pub(crate) fn writing(&self) -> bool {
        self.op.is_some()
    }

    #[cfg(test)]
    pub(crate) fn service_down(&self) -> bool {
        self.load_error.is_some()
    }

    #[cfg(test)]
    pub(crate) fn choose_default_repo(&mut self, alias: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.repo_values.iter().position(|v| v == alias) {
            self.choose_repo(i, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn choose_default_provider(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.provider_values.iter().position(|v| v == id) {
            self.choose_provider(i, cx);
        }
    }

    // ------------------------------------------------------------ render ---

    fn switch_row(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let stored = self.loaded.as_ref().is_some_and(|l| l.settings.enabled);
        let on = self.pending_enabled.unwrap_or(stored);
        let this = cx.entity().downgrade();
        let control = Switch::new("agents-enabled", on, ctl::AGENTS_LABEL)
            .disabled(!self.writable())
            .on_toggle(move |on, _, cx| {
                let _ = this.update(cx, |p, cx| p.set_enabled(on, cx));
            });
        ui::setting_row(
            t,
            ctl::AGENTS_LABEL,
            ctl::AGENTS_DESCRIPTION,
            control.into_any_element(),
            self.note(&Row::Enabled),
        )
        .into_any_element()
    }

    fn note(&self, row: &Row) -> Option<(Tone, SharedString)> {
        self.errors
            .get(row)
            .map(|e| (Tone::Error, SharedString::from(e.clone())))
    }

    fn provider_row(
        &self,
        t: &Theme,
        index: usize,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = self.name_of(provider);
        let (word, tone) = ctl::readiness_word(provider.readiness);
        let row = Row::Provider(provider.id.clone(), provider.profile.clone());
        let busy = self.busy_row(&row);
        let writable = self.writable();
        let check = {
            let (id, profile) = (provider.id.clone(), provider.profile.clone());
            Button::new(("agents-check", index as u64), ctl::CHECK)
                .small()
                .busy(busy)
                .disabled(!writable && !busy)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.check(id.clone(), profile.clone(), cx)),
                )
        };
        let single = provider.secrets.len() == 1;
        let mut trailing = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::s4())
            .child(ui::status_dot(t, tone, word))
            .child(check);
        if single {
            trailing =
                trailing.children(self.key_buttons(index, 0, provider, &provider.secrets[0], cx));
        }
        let mut item = div()
            .debug_selector({
                let key = format!("agents-provider:{}:{}", provider.id, provider.profile);
                move || key.clone()
            })
            .flex()
            .flex_col()
            .gap(space::s4())
            .px(ui::row_inset())
            .py(space::s5())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s5())
                    .child(ui::monogram_tile(t, provider.monogram(), px(TILE)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s1())
                            .flex_1()
                            .min_w_0()
                            .child(ui::label(t, name.clone()))
                            .child(
                                ui::supporting(t, ctl::provider_support_line(provider))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis(),
                            )
                            .when_some(self.errors.get(&row).cloned(), |d, e| {
                                d.child(ui::error_line(t, e))
                            }),
                    )
                    .child(trailing),
            );
        // Several secrets: one line each, named in mono.
        if !single {
            for (i, slot) in provider.secrets.iter().enumerate() {
                item = item.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .pl(px(TILE) + space::s5())
                        .child(ui::mono(t, slot.name.clone()).flex_1().min_w_0())
                        .when(!slot.required, |d| {
                            d.child(ui::supporting(t, ctl::OPTIONAL))
                        })
                        .children(self.key_buttons(index, i, provider, slot, cx)),
                );
            }
        }
        if let Some((target, _)) = self
            .editor
            .as_ref()
            .filter(|(e, _)| e.id == provider.id && e.profile == provider.profile)
        {
            if let Some(slot) = provider.secrets.iter().find(|s| s.name == target.name) {
                item = item.child(self.key_editor(t, provider, slot, &name, busy, cx));
            }
        }
        item.into_any_element()
    }

    fn key_buttons(
        &self,
        index: usize,
        slot_index: usize,
        provider: &Provider,
        slot: &SecretSlot,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let target = KeyTarget {
            id: provider.id.clone(),
            profile: provider.profile.clone(),
            name: slot.name.clone(),
        };
        let disabled = !self.writable();
        let id = (index * crate::controllers::agents::MAX_SECRET_SLOTS + slot_index) as u64;
        let mut open = Button::new(
            ("agents-key-open", id),
            if slot.set {
                ctl::REPLACE_KEY
            } else {
                ctl::ADD_KEY
            },
        )
        .small()
        .disabled(disabled)
        .on_click(cx.listener({
            let target = target.clone();
            move |this, _, window, cx| this.open_key(target.clone(), window, cx)
        }));
        if self.opener.as_ref() == Some(&target) {
            open = open.track_focus(&self.opener_focus);
        }
        let mut out = vec![open.into_any_element()];
        if slot.set {
            out.push(
                Button::new(("agents-key-remove", id), ctl::REMOVE_KEY)
                    .small()
                    .ghost()
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_remove_key(target.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }
        out
    }

    fn key_editor(
        &self,
        t: &Theme,
        provider: &Provider,
        slot: &SecretSlot,
        name: &str,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let label = ctl::key_field_label(provider, slot);
        let label = if provider.secrets.len() == 1 {
            ui::label(t, label)
        } else {
            ui::mono(t, label)
        };
        let empty = self.key_input.read(cx).text().is_empty();
        div()
            .debug_selector(|| "agents-key-editor".into())
            .flex()
            .flex_col()
            .w_full()
            .gap(space::s3())
            .pl(px(TILE) + space::s5())
            .child(ui::secondary(t, ctl::disclosure(provider, name)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s3())
                    .child(label)
                    .when(!slot.required, |d| {
                        d.child(ui::supporting(t, ctl::OPTIONAL))
                    }),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap(space::s4())
                    .child(div().flex_auto().min_w_0().child(self.key_input.clone()))
                    .child(
                        Button::new("agents-key-save", ctl::SAVE)
                            .primary()
                            .small()
                            .busy(busy)
                            .disabled(empty || self.op.is_some() || self.load_error.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    )
                    .child(
                        Button::new("agents-key-cancel", ctl::CANCEL)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
                    ),
            )
            .when_some(self.key_error.clone(), |d, e| d.child(ui::error_line(t, e)))
            .child(ui::supporting(
                t,
                ctl::key_storage_line(self.backend.simulated()),
            ))
    }

    fn workspace_row(
        &self,
        t: &Theme,
        index: usize,
        repo: &ctl::RepoEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let row = Row::Workspace(repo.alias.clone());
        let alias = repo.alias.clone();
        let note = if let Some(e) = self.errors.get(&row) {
            Some(ui::error_line(t, e.clone()))
        } else if !repo.exists {
            Some(ui::supporting(t, ctl::FOLDER_NOT_FOUND).text_color(t.tone(Tone::Warning).0))
        } else {
            None
        };
        div()
            .debug_selector({
                let key = format!("agents-workspace:{}", repo.alias);
                move || key.clone()
            })
            .flex()
            .items_center()
            .justify_between()
            .gap(space::s8())
            .w_full()
            .min_h(crate::theme::size::row())
            .px(ui::row_inset())
            .py(space::s5())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s1())
                    .flex_1()
                    .min_w_0()
                    .child(ui::label(t, repo.alias.clone()))
                    .child(
                        ui::mono(t, ctl::path_line(&repo.path))
                            .text_color(t.text_secondary)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis(),
                    )
                    .children(note),
            )
            .child(
                Button::new(("agents-workspace-remove", index as u64), ctl::REMOVE)
                    .small()
                    .ghost()
                    .busy(self.busy_row(&row))
                    .disabled(!self.writable() && !self.busy_row(&row))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_remove_workspace(alias.clone(), cx)
                    })),
            )
            .into_any_element()
    }

    fn add_row(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let busy = self.busy_row(&Row::AddWorkspace);
        let field = |label: &'static str, input: Entity<TextInput>| {
            div()
                .flex()
                .flex_col()
                .gap(space::s1())
                .child(ui::supporting(t, label))
                .child(input)
        };
        div()
            .debug_selector(|| "agents-workspace-add".into())
            .flex()
            .flex_col()
            .gap(space::s3())
            .px(ui::row_inset())
            .py(space::s5())
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(space::s4())
                    .child(field(ctl::NAME_LABEL, self.name_input.clone()).w(px(180.)))
                    .child(
                        field(ctl::FOLDER_LABEL, self.folder_input.clone())
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        Button::new("agents-workspace-choose", ctl::CHOOSE)
                            .small()
                            .disabled(!self.writable())
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    )
                    .child(
                        Button::new("agents-workspace-add-button", ctl::ADD)
                            .primary()
                            .small()
                            .busy(busy)
                            .disabled(!self.writable() && !busy)
                            .on_click(cx.listener(|this, _, _, cx| this.add_workspace(cx))),
                    ),
            )
            .when_some(self.errors.get(&Row::AddWorkspace).cloned(), |d, e| {
                d.child(ui::error_line(t, e))
            })
            .into_any_element()
    }
}

const KEY_LABEL_DEFAULT: &str = ctl::KEY_FIELD_LABEL;

impl Render for AgentsSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        match self.pending_focus.take() {
            Some(PendingFocus::KeyField) => self.key_input.read(cx).focus(window),
            Some(PendingFocus::Opener) => window.focus(&self.opener_focus),
            Some(PendingFocus::SayField) => self.triggers.say.read(cx).focus(window),
            Some(PendingFocus::RuleOpener) => window.focus(&self.triggers.opener_focus),
            None => {}
        }
        if self.loaded.is_some() {
            crate::sandbox::scene_ready(crate::sandbox::Fixture::AgentsSettings);
            if self.editor.is_some() {
                crate::sandbox::scene_ready(crate::sandbox::Fixture::AgentsSettingsKey);
            }
            // The trigger scene scrolls the page to Voice triggers first,
            // and reports only once that scrolled frame is on screen.
            if self.trigger_scene_ready() && self.reveal_triggers(window, cx) {
                crate::sandbox::scene_ready(crate::sandbox::Fixture::AgentsSettingsTrigger);
            }
        }
        let mut body = div()
            .debug_selector(|| "agents-settings".into())
            .flex()
            .flex_col()
            .gap(space::s6())
            .w_full();
        let sandbox =
            self.backend.simulated() || self.loaded.as_ref().is_some_and(|l| l.settings.sandbox);
        if self.load_error.is_some() {
            body = body.child(ui::banner(
                &t,
                Tone::Error,
                ctl::SERVICE_DOWN,
                Some(
                    Button::new("agents-settings-retry", ctl::TRY_AGAIN)
                        .small()
                        .icon("refresh-cw")
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                        .into_any_element(),
                ),
            ));
        } else if sandbox {
            body = body.child(ui::banner(&t, Tone::Info, ctl::SANDBOX, None));
        }
        let Some(loaded) = &self.loaded else {
            if self.load_error.is_none() {
                body = body.child(
                    ui::group(&t).child(
                        div()
                            .px(ui::row_inset())
                            .py(space::s5())
                            .child(ui::secondary(&t, ctl::CHECKING)),
                    ),
                );
            }
            return body;
        };
        let providers = loaded.providers.clone();
        let repos = loaded.settings.repos.clone();
        body = body.child(ui::group_rows(
            &t,
            [self.switch_row(&t, cx)],
            ui::row_inset(),
        ));
        let provider_rows: Vec<gpui::AnyElement> = providers
            .iter()
            .enumerate()
            .map(|(i, p)| self.provider_row(&t, i, p, cx))
            .collect();
        body = body.child(ui::section(&t, ctl::PROVIDERS_HEADING, None, None).child(
            ui::group_rows(&t, provider_rows, ui::row_inset() + px(TILE) + space::s5()),
        ));
        let defaults = vec![
            ui::setting_row(
                &t,
                ctl::DEFAULT_PROVIDER_LABEL,
                ctl::DEFAULT_PROVIDER_DESCRIPTION,
                self.provider_select.clone().into_any_element(),
                self.note(&Row::DefaultProvider),
            )
            .into_any_element(),
            ui::setting_row(
                &t,
                ctl::DEFAULT_WORKSPACE_LABEL,
                ctl::DEFAULT_WORKSPACE_DESCRIPTION,
                self.repo_select.clone().into_any_element(),
                self.note(&Row::DefaultRepo),
            )
            .into_any_element(),
        ];
        body = body.child(
            ui::section(&t, ctl::DEFAULTS_HEADING, None, None).child(ui::group_rows(
                &t,
                defaults,
                ui::row_inset(),
            )),
        );
        let mut workspace_rows: Vec<gpui::AnyElement> = repos
            .iter()
            .enumerate()
            .map(|(i, r)| self.workspace_row(&t, i, r, cx))
            .collect();
        if workspace_rows.is_empty() {
            workspace_rows.push(
                div()
                    .px(ui::row_inset())
                    .py(space::s5())
                    .child(ui::secondary(&t, ctl::NO_WORKSPACES))
                    .into_any_element(),
            );
        }
        if repos.len() < ctl::MAX_WORKSPACES {
            workspace_rows.push(self.add_row(&t, cx));
        } else {
            workspace_rows.push(
                div()
                    .px(ui::row_inset())
                    .py(space::s5())
                    .child(ui::supporting(&t, ctl::TOO_MANY))
                    .into_any_element(),
            );
        }
        body = body.child(
            ui::section(
                &t,
                ctl::WORKSPACES_HEADING,
                Some(ctl::WORKSPACES_SUPPORT.into()),
                None,
            )
            .child(ui::group_rows(&t, workspace_rows, ui::row_inset())),
        );
        body.child(self.render_triggers(&t, cx))
    }
}
